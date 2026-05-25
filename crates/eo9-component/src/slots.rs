//! Slot-name helpers.
//!
//! A module's ports are *slots*: `(name, interface type)` pairs whose name defaults to
//! the interface name (SPEC.md "Capability slots, `rename`, and `with`"). In the
//! component binary a slot appears as an extern (import/export) name that is either a
//! plain kebab name (`system-fs`) or a fully-qualified, usually versioned interface name
//! (`eo9:fs/fs@0.1.0`). Slot names in the algebra are versionless: the version is carried
//! separately and matched by the semver rule.

use crate::semver;

/// Splits a component extern name into its versionless slot name and the version text
/// (empty if the name carries no version).
pub(crate) fn split_extern_name(name: &str) -> (&str, &str) {
    match name.split_once('@') {
        Some((slot, version)) => (slot, version),
        None => (name, ""),
    }
}

/// The versionless slot name of an extern name.
pub(crate) fn slot_name(extern_name: &str) -> &str {
    split_extern_name(extern_name).0
}

/// Whether an extern name is an interface-style name (`ns:pkg/iface[@version]`) rather
/// than a plain kebab slot name.
pub(crate) fn is_interface_style(name: &str) -> bool {
    name.contains(':')
}

/// Whether an export with extern name `export_name` satisfies an import with extern name
/// `import_name` under `$`/`&` matching: equal slot names, and the export's version
/// satisfies the import's version per the semver rule.
pub(crate) fn export_matches_import(export_name: &str, import_name: &str) -> bool {
    let (export_slot, export_version) = split_extern_name(export_name);
    let (import_slot, import_version) = split_extern_name(import_name);
    export_slot == import_slot && semver::satisfies(export_version, import_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_names() {
        assert_eq!(split_extern_name("eo9:fs/fs@0.1.0"), ("eo9:fs/fs", "0.1.0"));
        assert_eq!(split_extern_name("eo9:fs/fs"), ("eo9:fs/fs", ""));
        assert_eq!(split_extern_name("system-fs"), ("system-fs", ""));
        assert!(is_interface_style("eo9:fs/fs@0.1.0"));
        assert!(!is_interface_style("system-fs"));
    }

    #[test]
    fn matches_by_slot_name_and_semver() {
        assert!(export_matches_import("eo9:fs/fs@0.1.0", "eo9:fs/fs@0.1.0"));
        assert!(export_matches_import("eo9:fs/fs@0.1.2", "eo9:fs/fs@0.1.0"));
        assert!(!export_matches_import("eo9:fs/fs@0.1.0", "eo9:fs/fs@0.1.2"));
        assert!(!export_matches_import("eo9:fs/fs@0.2.0", "eo9:fs/fs@0.1.0"));
        // Slot names, not interface types, drive `$`/`&` matching.
        assert!(!export_matches_import("eo9:fs/fs@0.1.0", "system-fs"));
        assert!(export_matches_import("system-fs", "system-fs"));
    }
}
