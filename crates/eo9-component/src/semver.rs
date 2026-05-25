//! The semver rule used for link-time satisfaction (SPEC.md "WASM runtime"):
//! a provider of `eo9:disk@1.2.0` satisfies an import of `eo9:disk@1.0.0` -- same major,
//! equal-or-newer minor/patch; different majors never unify.
//!
//! For pre-1.0 versions the spec is silent, so this module follows the interpretation
//! used across the Component Model tooling (wasm-tools, wac, cargo): `0.minor` is the
//! compatibility track (`0.1.2` satisfies `0.1.0`, but `0.2.0` does not satisfy `0.1.0`),
//! and `0.0.x` is only satisfied by exactly `0.0.x`. Recorded in
//! plan/03-component-algebra.md Decisions.

/// A parsed `major.minor.patch` version.
///
/// Deliberately minimal: pre-release/build suffixes are carried verbatim and make a
/// version satisfiable only by an identical version string (a pre-release makes no
/// compatibility promises). This avoids pulling a full semver crate into the
/// foundation-dependency set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major version number.
    pub major: u64,
    /// Minor version number.
    pub minor: u64,
    /// Patch version number.
    pub patch: u64,
    /// Anything after the patch number (`-pre`, `+build`, ...), including the sigil.
    pub suffix: String,
}

impl Version {
    /// Parses a `major.minor.patch[-pre][+build]` string. Returns `None` if the three
    /// leading numeric components are not present.
    pub fn parse(s: &str) -> Option<Self> {
        let (numeric, suffix) = match s.find(['-', '+']) {
            Some(idx) => (&s[..idx], &s[idx..]),
            None => (s, ""),
        };
        let mut parts = numeric.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            suffix: suffix.to_string(),
        })
    }
}

/// Returns whether a provider built against `provided` satisfies an import pinned to
/// `required`, per the semver rule above.
///
/// Either string being empty means "unversioned"; an unversioned export satisfies any
/// requirement (structural type checking still applies downstream), while an
/// unversioned requirement is satisfied by anything.
pub fn satisfies(provided: &str, required: &str) -> bool {
    if required.is_empty() || provided == required {
        return true;
    }
    if provided.is_empty() {
        // An unversioned export cannot demonstrate compatibility with a pinned import.
        return false;
    }
    let (Some(p), Some(r)) = (Version::parse(provided), Version::parse(required)) else {
        // Unparseable versions only satisfy by exact equality (handled above).
        return false;
    };
    if !p.suffix.is_empty() || !r.suffix.is_empty() {
        // Pre-release/build-tagged versions make no compatibility promises.
        return false;
    }
    if p.major != r.major {
        return false;
    }
    if p.major > 0 {
        (p.minor, p.patch) >= (r.minor, r.patch)
    } else if r.minor > 0 {
        p.minor == r.minor && p.patch >= r.patch
    } else {
        // 0.0.x: only exact equality (already handled above).
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_versions() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(
            (v.major, v.minor, v.patch, v.suffix.as_str()),
            (1, 2, 3, "")
        );
        let v = Version::parse("0.1.0-rc.1").unwrap();
        assert_eq!(v.suffix, "-rc.1");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("one.two.three").is_none());
    }

    #[test]
    fn spec_example_same_major_newer_minor() {
        // The spec's own example: 1.2.0 satisfies an import of 1.0.0.
        assert!(satisfies("1.2.0", "1.0.0"));
        assert!(satisfies("1.0.0", "1.0.0"));
        assert!(satisfies("1.0.1", "1.0.0"));
        // ... but not the other way around, and never across majors.
        assert!(!satisfies("1.0.0", "1.2.0"));
        assert!(!satisfies("2.0.0", "1.0.0"));
        assert!(!satisfies("1.9.9", "2.0.0"));
    }

    #[test]
    fn pre_one_uses_minor_as_the_track() {
        assert!(satisfies("0.1.0", "0.1.0"));
        assert!(satisfies("0.1.2", "0.1.0"));
        assert!(!satisfies("0.1.0", "0.1.2"));
        assert!(!satisfies("0.2.0", "0.1.0"));
        assert!(!satisfies("0.1.0", "0.2.0"));
        // 0.0.x is satisfied only by itself.
        assert!(satisfies("0.0.3", "0.0.3"));
        assert!(!satisfies("0.0.4", "0.0.3"));
    }

    #[test]
    fn unversioned_and_odd_cases() {
        assert!(satisfies("", ""));
        assert!(satisfies("1.0.0", ""));
        assert!(!satisfies("", "1.0.0"));
        // Pre-releases only match exactly.
        assert!(satisfies("1.0.0-rc.1", "1.0.0-rc.1"));
        assert!(!satisfies("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(!satisfies("1.0.0", "1.0.0-rc.1"));
    }
}
