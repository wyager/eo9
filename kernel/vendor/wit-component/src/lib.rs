//! The WebAssembly component tooling.

#![no_std]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

#[macro_use]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

/// no_std prelude: the `alloc` items the std prelude would otherwise provide. Each module
/// does `use crate::prelude::*;` (the `vec!`/`format!` macros come from `#[macro_use]
/// extern crate alloc` above).
pub(crate) mod prelude {
    pub(crate) use alloc::borrow::ToOwned;
    pub(crate) use alloc::boxed::Box;
    pub(crate) use alloc::string::{String, ToString};
    pub(crate) use alloc::vec::Vec;
}

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Display;
use core::str::FromStr;

use anyhow::{Result, bail};
use wasm_encoder::{CanonicalOption, Encode, Section};

/// Insertion-ordered map/set aliases with a no_std-capable default hasher (mirrors the
/// vendored wac crates). indexmap's own `IndexMap::new`/`IndexSet::new` require the
/// std-only `RandomState`, so the source uses `::default()` and these aliases.
#[cfg(feature = "std")]
pub(crate) type IndexMap<K, V> = indexmap::IndexMap<K, V, std::hash::RandomState>;
#[cfg(not(feature = "std"))]
pub(crate) type IndexMap<K, V> = indexmap::IndexMap<K, V, hashbrown::DefaultHashBuilder>;
#[cfg(feature = "std")]
pub(crate) type IndexSet<T> = indexmap::IndexSet<T, std::hash::RandomState>;
#[cfg(not(feature = "std"))]
pub(crate) type IndexSet<T> = indexmap::IndexSet<T, hashbrown::DefaultHashBuilder>;
use wit_parser::{Resolve, WorldId};

mod encoding;
mod gc;
mod linking;
mod printing;
mod targets;
mod validation;

pub use encoding::{ComponentEncoder, LibraryInfo, encode};
pub use linking::Linker;
pub use printing::*;
pub use targets::*;
pub use validation::AdapterModuleDidNotExport;
pub use wit_parser::decoding::{DecodedWasm, decode};
// `decode_reader` is the streaming `std::io::Read` entry point, gated to `std` in wit-parser.
#[cfg(feature = "std")]
pub use wit_parser::decoding::decode_reader;

pub mod metadata;

#[cfg(feature = "dummy-module")]
pub use dummy::dummy_module;
#[cfg(feature = "dummy-module")]
mod dummy;

#[cfg(feature = "semver-check")]
mod semver_check;
#[cfg(feature = "semver-check")]
pub use semver_check::*;

/// Supported string encoding formats.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum StringEncoding {
    /// Strings are encoded with UTF-8.
    #[default]
    UTF8,
    /// Strings are encoded with UTF-16.
    UTF16,
    /// Strings are encoded with compact UTF-16 (i.e. Latin1+UTF-16).
    CompactUTF16,
}

impl Display for StringEncoding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StringEncoding::UTF8 => write!(f, "utf8"),
            StringEncoding::UTF16 => write!(f, "utf16"),
            StringEncoding::CompactUTF16 => write!(f, "compact-utf16"),
        }
    }
}

impl FromStr for StringEncoding {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "utf8" => Ok(StringEncoding::UTF8),
            "utf16" => Ok(StringEncoding::UTF16),
            "compact-utf16" => Ok(StringEncoding::CompactUTF16),
            _ => bail!("unknown string encoding `{s}`"),
        }
    }
}

impl From<StringEncoding> for wasm_encoder::CanonicalOption {
    fn from(e: StringEncoding) -> wasm_encoder::CanonicalOption {
        match e {
            StringEncoding::UTF8 => CanonicalOption::UTF8,
            StringEncoding::UTF16 => CanonicalOption::UTF16,
            StringEncoding::CompactUTF16 => CanonicalOption::CompactUTF16,
        }
    }
}

/// A producer section to be added to all modules and components synthesized by
/// this crate
#[cfg(feature = "metadata")]
pub(crate) fn base_producers() -> wasm_metadata::Producers {
    let mut producer = wasm_metadata::Producers::empty();
    producer.add("processed-by", "wit-component", env!("CARGO_PKG_VERSION"));
    producer
}

/// `wasm_metadata::Producers` when the `metadata` feature is on; otherwise a no-op
/// placeholder with the same API surface so the producers-carrying types and signatures
/// still compile. With `metadata` off the (informational) producers custom sections are
/// simply not emitted — see the `#[cfg(feature = "metadata")]` guards at the emission
/// sites — so the encoded component stays valid.
#[cfg(feature = "metadata")]
pub(crate) use wasm_metadata::Producers;
#[cfg(not(feature = "metadata"))]
pub(crate) use self::no_metadata::Producers;

#[cfg(not(feature = "metadata"))]
mod no_metadata {
    /// No-op stand-in for `wasm_metadata::Producers` (see [`crate::Producers`]).
    #[derive(Default, Clone)]
    pub struct Producers;

    impl Producers {
        pub fn empty() -> Self {
            Producers
        }
        pub fn add(&mut self, _field: &str, _name: &str, _version: &str) {}
        pub fn merge(&mut self, _other: &Producers) {}
        pub fn from_bytes(_bytes: &[u8], _offset: usize) -> anyhow::Result<Self> {
            Ok(Producers)
        }
        pub fn from_wasm(_bytes: &[u8]) -> anyhow::Result<Option<Self>> {
            Ok(None)
        }
    }

    // `raw_custom_section` is intentionally NOT provided: every producers-section emission
    // site is `#[cfg(feature = "metadata")]`-gated, so a missing guard is a compile error
    // here rather than a silently-invalid empty section in the output.
}

/// Embed component metadata in a buffer of bytes that contains a Wasm module
pub fn embed_component_metadata(
    bytes: &mut Vec<u8>,
    wit_resolver: &Resolve,
    world: WorldId,
    encoding: StringEncoding,
) -> Result<()> {
    let encoded = metadata::encode(&wit_resolver, world, encoding, None)?;

    let section = wasm_encoder::CustomSection {
        name: "component-type".into(),
        data: Cow::Borrowed(&encoded),
    };
    bytes.push(section.id());
    section.encode(bytes);

    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use wasmparser::Payload;
    use wit_parser::Resolve;

    use super::{StringEncoding, embed_component_metadata};

    const MODULE_WAT: &str = r#"
(module
  (type (;0;) (func))
  (func (;0;) (type 0)
    nop
  )
)
"#;

    const COMPONENT_WIT: &str = r#"
package test:foo;
world test-world {}
"#;

    #[test]
    fn component_metadata_embedding_works() -> Result<()> {
        let mut bytes = wat::parse_str(MODULE_WAT)?;

        // Get original len & custom section count
        let original_len = bytes.len();
        let payloads = wasmparser::Parser::new(0).parse_all(&bytes);
        let original_custom_section_count = payloads.fold(0, |acc, payload| {
            if let Ok(Payload::CustomSection { .. }) = payload {
                acc + 1
            } else {
                acc
            }
        });

        // Parse pre-canned WIT to build resolver
        let mut resolver = Resolve::default();
        let pkg = resolver.push_str("in-code.wit", COMPONENT_WIT)?;
        let world = resolver.select_world(&[pkg], Some("test-world"))?;

        // Embed component metadata
        embed_component_metadata(&mut bytes, &resolver, world, StringEncoding::UTF8)?;

        // Re-retrieve custom section count, and search for the component-type custom section along the way
        let mut found_component_section = false;
        let new_custom_section_count =
            wasmparser::Parser::new(0)
                .parse_all(&bytes)
                .fold(0, |acc, payload| {
                    if let Ok(Payload::CustomSection(reader)) = payload {
                        if reader.name() == "component-type" {
                            found_component_section = true;
                        }
                        acc + 1
                    } else {
                        acc
                    }
                });

        assert!(original_len < bytes.len());
        assert_eq!(original_custom_section_count + 1, new_custom_section_count);
        assert!(found_component_section);

        Ok(())
    }
}
