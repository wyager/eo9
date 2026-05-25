//! Bare dotted module names.
//!
//! Names follow the packaging convention from SPEC.md ("Packaging and submodules"): the
//! bare package name (`virtualfs`, `browser`) addresses the package's default world, and
//! a dotted suffix (`virtualfs.create`, `fs.memfs`) addresses a sibling world of the same
//! package. The store treats the whole dotted name as a flat key — hierarchy is a naming
//! convention, not containment — but [`Name::package`] and [`Name::world`] expose the two
//! parts for consumers (the shell, the usermode binary) that want them.

use std::fmt;
use std::str::FromStr;

use crate::StoreError;

/// A validated bare dotted module name, e.g. `browser`, `virtualfs.create`, `fs.memfs`.
///
/// Each dot-separated segment is a kebab-case identifier: it starts with a lowercase
/// ASCII letter, continues with lowercase letters, digits, and `-`, and does not end
/// with `-`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name {
    raw: String,
}

impl Name {
    /// Parse and validate a bare dotted name.
    pub fn parse(input: &str) -> Result<Name, StoreError> {
        let invalid = |reason: &str| StoreError::InvalidName {
            input: input.to_owned(),
            reason: reason.to_owned(),
        };
        if input.is_empty() {
            return Err(invalid("names must be non-empty"));
        }
        for segment in input.split('.') {
            validate_segment(segment).map_err(|reason| invalid(&reason))?;
        }
        Ok(Name {
            raw: input.to_owned(),
        })
    }

    /// The full dotted name as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The package part: everything before the first dot (`virtualfs` in
    /// `virtualfs.create`), or the whole name if there is no dot.
    pub fn package(&self) -> &str {
        match self.raw.split_once('.') {
            Some((package, _)) => package,
            None => &self.raw,
        }
    }

    /// The world part: everything after the first dot (`create` in `virtualfs.create`),
    /// or `None` for a bare package name, which addresses the package's default world.
    pub fn world(&self) -> Option<&str> {
        self.raw.split_once('.').map(|(_, world)| world)
    }
}

fn validate_segment(segment: &str) -> Result<(), String> {
    let Some(first) = segment.chars().next() else {
        return Err("name segments must be non-empty".to_owned());
    };
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "segment {segment:?} must start with a lowercase ASCII letter"
        ));
    }
    if let Some(bad) = segment
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(format!(
            "segment {segment:?} contains {bad:?}; only lowercase letters, digits, and '-' are allowed"
        ));
    }
    if segment.ends_with('-') {
        return Err(format!("segment {segment:?} must not end with '-'"));
    }
    Ok(())
}

/// Validate a manifest or profile file stem (the part before `.manifest` / `.profile`).
/// Same character rules as a single name segment — in particular no dots and no path
/// separators, so stems can never escape their directory.
pub(crate) fn validate_file_stem(stem: &str) -> Result<(), StoreError> {
    validate_segment(stem).map_err(|reason| StoreError::InvalidFileStem {
        input: stem.to_owned(),
        reason,
    })
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({})", self.raw)
    }
}

impl FromStr for Name {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Name, StoreError> {
        Name::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_and_dotted_names_parse() {
        for ok in [
            "browser",
            "virtualfs.create",
            "fs.memfs",
            "time-frozen",
            "a1.b2-c3",
        ] {
            let name = Name::parse(ok).unwrap();
            assert_eq!(name.as_str(), ok);
        }
    }

    #[test]
    fn package_and_world_split_on_the_first_dot() {
        let bare = Name::parse("browser").unwrap();
        assert_eq!(bare.package(), "browser");
        assert_eq!(bare.world(), None);

        let dotted = Name::parse("virtualfs.create").unwrap();
        assert_eq!(dotted.package(), "virtualfs");
        assert_eq!(dotted.world(), Some("create"));
    }

    #[test]
    fn invalid_names_are_rejected() {
        for bad in [
            "",
            ".",
            "a.",
            ".a",
            "Fs",
            "fs..memfs",
            "fs.mem fs",
            "-fs",
            "fs-",
            "fs/x",
            "虚拟",
        ] {
            assert!(Name::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn file_stems_reject_dots() {
        assert!(validate_file_stem("default").is_ok());
        assert!(validate_file_stem("my-profile2").is_ok());
        assert!(validate_file_stem("a.b").is_err());
        assert!(validate_file_stem("..").is_err());
        assert!(validate_file_stem("a/b").is_err());
    }
}
