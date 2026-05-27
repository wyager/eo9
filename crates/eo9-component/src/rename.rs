//! `rename` -- slot relabeling (SPEC.md "Capability slots, `rename`, and `with`").
//!
//! `rename(c, from, to)` relabels slot `from` to `to` on imports and exports alike. It is
//! implemented by re-encoding the component's outer import/export sections with the new
//! extern names (every other section is copied verbatim), so the component's contents and
//! wiring are untouched -- pure relabeling, exactly as the spec describes.
//!
//! When a default slot (named after its interface) is given a plain slot name, the new
//! extern name carries an `implements` annotation recording the interface it is an
//! instance of; this is the same encoding `wit-component` uses for named slots such as
//! `import system-fs: eo9:fs/fs`, and it is what keeps the interface identity visible to
//! `describe` after the rename.

use alloc::string::{String, ToString};

use crate::Component;
use crate::describe::{ExportMeta, ImportMeta};
use crate::error::RenameError;
use crate::externs::{self, ExternName, Side};
use crate::slots;

/// One planned extern-name rewrite.
struct Rewrite {
    /// The extern name as it currently appears in the binary.
    old_extern: String,
    /// The extern name to write instead.
    new_extern: String,
    /// The `implements` annotation to attach to the new name (plain-named slots only).
    implements: Option<String>,
}

/// Relabels slot `from` to `to` on the imports and exports of `c`.
///
/// Slot names are the versionless names reported by `describe`. The new name may be a
/// plain kebab name (`scratch-fs`) or the slot's own interface name (restoring the
/// default slot); naming a *different* interface is rejected, since a default slot's
/// name and its interface must agree.
pub fn rename(c: &Component, from: &str, to: &str) -> Result<Component, RenameError> {
    let meta = c.meta();
    let import = meta
        .imports
        .iter()
        .find(|i| i.slot == from || i.extern_name == from);
    let export = meta
        .exports
        .iter()
        .find(|e| e.slot == from || e.extern_name == from);
    if import.is_none() && export.is_none() {
        return Err(RenameError::NoSuchSlot(from.to_string()));
    }

    let import_rewrite = import
        .map(|i| plan_import_rewrite(i, to, &meta.imports))
        .transpose()?;
    let export_rewrite = export
        .map(|e| plan_export_rewrite(e, to, &meta.exports))
        .transpose()?;

    let is_noop = |rewrite: &Option<Rewrite>| {
        rewrite
            .as_ref()
            .is_none_or(|r| r.old_extern == r.new_extern)
    };
    if is_noop(&import_rewrite) && is_noop(&export_rewrite) {
        return Ok(c.clone());
    }

    let bytes = externs::rewrite_extern_names(c.bytes(), |side, name| {
        let rewrite = match side {
            Side::Import => import_rewrite.as_ref(),
            Side::Export => export_rewrite.as_ref(),
        };
        rewrite
            .filter(|r| r.old_extern == name.name)
            .map(|r| ExternName {
                name: r.new_extern.clone(),
                implements: r.implements.clone(),
            })
    })
    .map_err(RenameError::Internal)?;
    Component::load(bytes).map_err(|err| RenameError::Internal(format!("rename produced {err}")))
}

fn plan_import_rewrite(
    import: &ImportMeta,
    to: &str,
    all: &[ImportMeta],
) -> Result<Rewrite, RenameError> {
    let rewrite = plan_rewrite(&import.extern_name, to, &import.interface, &import.version)?;
    let collides = all.iter().any(|other| {
        other.extern_name != import.extern_name
            && (other.slot == to || other.extern_name == rewrite.new_extern)
    });
    if collides {
        return Err(RenameError::SlotCollision(format!(
            "an import slot named `{to}` already exists"
        )));
    }
    Ok(rewrite)
}

fn plan_export_rewrite(
    export: &ExportMeta,
    to: &str,
    all: &[ExportMeta],
) -> Result<Rewrite, RenameError> {
    let rewrite = plan_rewrite(&export.extern_name, to, &export.interface, &export.version)?;
    let collides = all.iter().any(|other| {
        other.extern_name != export.extern_name
            && (other.slot == to || other.extern_name == rewrite.new_extern)
    });
    if collides {
        return Err(RenameError::SlotCollision(format!(
            "an export slot named `{to}` already exists"
        )));
    }
    Ok(rewrite)
}

/// Works out the new extern name (and its `implements` annotation) for renaming a slot
/// with the given interface identity to `to`.
fn plan_rewrite(
    old_extern: &str,
    to: &str,
    interface: &str,
    version: &str,
) -> Result<Rewrite, RenameError> {
    let versioned_interface = || {
        if version.is_empty() {
            interface.to_string()
        } else {
            format!("{interface}@{version}")
        }
    };

    if !slots::is_interface_style(to) {
        return Ok(Rewrite {
            old_extern: old_extern.to_string(),
            new_extern: to.to_string(),
            implements: (!interface.is_empty()).then(versioned_interface),
        });
    }

    // An interface-style target means "make this the default slot of that interface";
    // the name must be the slot's own interface (a default slot's name and interface
    // always agree), optionally pinned to its exact version.
    let (to_slot, to_version) = slots::split_extern_name(to);
    if to_slot != interface {
        return Err(RenameError::SlotCollision(format!(
            "`{to}` names interface `{to_slot}`, but the slot's interface is `{interface}`; \
             a default slot's name and interface must agree"
        )));
    }
    if !to_version.is_empty() && to_version != version {
        return Err(RenameError::SlotCollision(format!(
            "`{to}` pins version `{to_version}`, but the slot's interface version is `{version}`"
        )));
    }
    Ok(Rewrite {
        old_extern: old_extern.to_string(),
        new_extern: versioned_interface(),
        implements: None,
    })
}
