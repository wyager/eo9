//! Rewriting of a component's outer import/export extern names.
//!
//! Two operations need this:
//!
//! * [`rename`](crate::rename) relabels slot names directly in the import/export
//!   sections;
//! * compose/extend/restrict strip and re-attach the `implements` name annotation.
//!
//! The `implements` annotation is how `wit-component` (0.250 family) records which
//! interface a *plain-named* slot (`import system-fs: eo9:fs/fs`) is an instance of; it
//! is what lets `describe` report the interface identity of named slots. `wac-graph`
//! 0.10 is built on the 0.247 wasm-tools family, which predates that encoding and
//! rejects such names, so the wiring step works on components with the annotations
//! stripped and the algebra re-attaches them to the composition's own imports/exports
//! afterwards. The annotation is purely descriptive -- wiring and validation never
//! depend on it -- so stripping it inside the (embedded) operands is harmless.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wasm_encoder::{
    Component as ComponentBuilder, ComponentExportSection, ComponentImportSection,
    ComponentSectionId, RawSection,
};
use wasmparser::BinaryReader;

/// Which side of the component an extern name appears on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Import,
    Export,
}

/// A replacement extern name produced by a rewrite callback.
pub(crate) struct ExternName {
    pub name: String,
    pub implements: Option<String>,
}

/// Re-encodes the component's outer import and export sections, applying `rewrite` to
/// every extern name (a `None` return keeps the name as it was); every other section is
/// copied byte-for-byte.
pub(crate) fn rewrite_extern_names(
    bytes: &[u8],
    mut rewrite: impl FnMut(Side, &wasmparser::ComponentExternName<'_>) -> Option<ExternName>,
) -> Result<Vec<u8>, String> {
    let parse_err = |err: wasmparser::BinaryReaderError| format!("failed to reparse: {err}");

    let mut builder = ComponentBuilder::new();
    // Skip the 8-byte preamble (magic, version, layer): the builder re-emits it.
    let mut reader = BinaryReader::new(&bytes[8..], 8);
    while !reader.eof() {
        let id = reader.read_u8().map_err(parse_err)?;
        let size = reader.read_var_u32().map_err(parse_err)?;
        let offset = reader.original_position();
        let contents = reader.read_bytes(size as usize).map_err(parse_err)?;

        if id == u8::from(ComponentSectionId::Import) {
            let mut section = ComponentImportSection::new();
            let section_reader =
                wasmparser::ComponentImportSectionReader::new(BinaryReader::new(contents, offset))
                    .map_err(parse_err)?;
            for import in section_reader {
                let import = import.map_err(parse_err)?;
                section.import(
                    encoded_name(&import.name, rewrite(Side::Import, &import.name)),
                    import.ty.into(),
                );
            }
            builder.section(&section);
        } else if id == u8::from(ComponentSectionId::Export) {
            let mut section = ComponentExportSection::new();
            let section_reader =
                wasmparser::ComponentExportSectionReader::new(BinaryReader::new(contents, offset))
                    .map_err(parse_err)?;
            for export in section_reader {
                let export = export.map_err(parse_err)?;
                section.export(
                    encoded_name(&export.name, rewrite(Side::Export, &export.name)),
                    export.kind.into(),
                    export.index,
                    export.ty.map(Into::into),
                );
            }
            builder.section(&section);
        } else {
            builder.section(&RawSection { id, data: contents });
        }
    }
    Ok(builder.finish())
}

fn encoded_name<'a>(
    original: &wasmparser::ComponentExternName<'a>,
    replacement: Option<ExternName>,
) -> wasm_encoder::ComponentExternName<'a> {
    match replacement {
        Some(name) => wasm_encoder::ComponentExternName {
            name: Cow::Owned(name.name),
            implements: name.implements.map(Cow::Owned),
        },
        None => (*original).into(),
    }
}

/// Drops every `implements` annotation from the component's outer extern names (so the
/// 0.247-family machinery inside `wac-graph` can parse it).
pub(crate) fn strip_implements(bytes: &[u8]) -> Result<Vec<u8>, String> {
    rewrite_extern_names(bytes, |_, name| {
        name.implements.map(|_| ExternName {
            name: name.name.to_string(),
            implements: None,
        })
    })
}

/// Attaches `implements` annotations to the component's outer extern names: any
/// plain-named import/export listed in `annotations` (extern name -> versioned interface
/// id) that does not already carry one gets the recorded interface identity.
pub(crate) fn attach_implements(
    bytes: &[u8],
    annotations: &BTreeMap<String, String>,
) -> Result<Vec<u8>, String> {
    if annotations.is_empty() {
        return Ok(bytes.to_vec());
    }
    rewrite_extern_names(bytes, |_, name| {
        if name.implements.is_some() {
            return None;
        }
        annotations.get(name.name).map(|interface| ExternName {
            name: name.name.to_string(),
            implements: Some(interface.clone()),
        })
    })
}
