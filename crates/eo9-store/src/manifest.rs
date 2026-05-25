//! Name resolution: manifests and profiles.
//!
//! # On-disk formats (version 1)
//!
//! Both formats are line-oriented UTF-8 text. Blank lines and lines starting with `#` are
//! ignored; the first significant line is a version header. Files are written atomically
//! and in sorted order, so they diff cleanly and rewriting an unchanged map is a no-op.
//!
//! **Manifest** — `manifests/<stem>.manifest`, a flat map from bare dotted names to
//! object hashes:
//!
//! ```text
//! eo9-manifest 1
//! browser           5b3c…64-hex…9a
//! fs.memfs          77aa…64-hex…01
//! virtualfs.create  0f2e…64-hex…c4
//! ```
//!
//! Each entry line is `<name> <blake3-hex>` separated by whitespace; a name may appear at
//! most once.
//!
//! **Profile** — `profiles/<stem>.profile`, an ordered stack of manifest stems:
//!
//! ```text
//! eo9-profile 1
//! base
//! overrides
//! ```
//!
//! Manifests are listed base-first; **later manifests shadow earlier ones**, the same
//! override direction as the `&` operator ("rightward wins"). Resolving a name walks the
//! stack from the top (last line) down and takes the first binding found.
//!
//! If no profile file named `<p>.profile` exists, the profile `<p>` is implicitly the
//! single-manifest stack `[<p>]`, and if that manifest file is also missing it is simply
//! empty. A fresh store therefore works out of the box: `bind` writes into
//! `manifests/default.manifest` and `resolve` reads it back, no profile file required.
//! An *explicit* profile that names a missing manifest is an error — a dangling reference
//! is never silently skipped.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::name::validate_file_stem;
use crate::object::ObjectHandle;
use crate::{Name, ObjectHash, Store, StoreError, fsutil};

/// The manifest `bind` and `resolve` use when none is named: `manifests/default.manifest`.
pub const DEFAULT_MANIFEST: &str = "default";

/// The profile `resolve` uses when none is named.
pub const DEFAULT_PROFILE: &str = "default";

const MANIFEST_HEADER: &str = "eo9-manifest 1";
const PROFILE_HEADER: &str = "eo9-profile 1";

/// A flat map from bare dotted names to object hashes (the parsed form of a
/// `.manifest` file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    entries: BTreeMap<Name, ObjectHash>,
}

impl Manifest {
    /// An empty manifest.
    pub fn new() -> Manifest {
        Manifest::default()
    }

    /// Look up a name.
    pub fn get(&self, name: &Name) -> Option<&ObjectHash> {
        self.entries.get(name)
    }

    /// Bind `name` to `hash`, replacing any existing binding.
    pub fn set(&mut self, name: Name, hash: ObjectHash) {
        self.entries.insert(name, hash);
    }

    /// Remove a binding; returns the hash it pointed at, if any.
    pub fn remove(&mut self, name: &Name) -> Option<ObjectHash> {
        self.entries.remove(name)
    }

    /// Iterate over the bindings in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&Name, &ObjectHash)> {
        self.entries.iter()
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the manifest has no bindings.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parse the on-disk text format. `path` is used only for error reporting.
    pub fn parse(text: &str, path: &Path) -> Result<Manifest, StoreError> {
        let mut lines = significant_lines(text);
        expect_header(&mut lines, MANIFEST_HEADER, path)?;
        let mut entries = BTreeMap::new();
        for (line_number, line) in lines {
            let corrupt = |reason: String| StoreError::Corrupt {
                path: path.to_owned(),
                line: line_number,
                reason,
            };
            let mut fields = line.split_whitespace();
            let (Some(name), Some(hash), None) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(corrupt(format!("expected `<name> <hash>`, found {line:?}")));
            };
            let name = Name::parse(name).map_err(|err| corrupt(err.to_string()))?;
            let hash = ObjectHash::from_hex(hash).map_err(|err| corrupt(err.to_string()))?;
            if entries.insert(name.clone(), hash).is_some() {
                return Err(corrupt(format!("duplicate binding for {name}")));
            }
        }
        Ok(Manifest { entries })
    }

    /// Render the on-disk text format (sorted by name).
    pub fn to_text(&self) -> String {
        let mut out = format!("{MANIFEST_HEADER}\n");
        for (name, hash) in &self.entries {
            let _ = writeln!(out, "{name} {hash}");
        }
        out
    }
}

/// An ordered stack of manifest stems, base first (the parsed form of a `.profile` file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    manifests: Vec<String>,
}

impl Profile {
    /// An empty profile.
    pub fn new() -> Profile {
        Profile::default()
    }

    /// The manifest stems in stack order, base first.
    pub fn manifests(&self) -> &[String] {
        &self.manifests
    }

    /// Push a manifest onto the top of the stack (it shadows everything below it).
    pub fn push(&mut self, manifest: &str) -> Result<(), StoreError> {
        validate_file_stem(manifest)?;
        self.manifests.push(manifest.to_owned());
        Ok(())
    }

    /// Parse the on-disk text format. `path` is used only for error reporting.
    pub fn parse(text: &str, path: &Path) -> Result<Profile, StoreError> {
        let mut lines = significant_lines(text);
        expect_header(&mut lines, PROFILE_HEADER, path)?;
        let mut manifests = Vec::new();
        for (line_number, line) in lines {
            let stem = line.trim();
            validate_file_stem(stem).map_err(|err| StoreError::Corrupt {
                path: path.to_owned(),
                line: line_number,
                reason: err.to_string(),
            })?;
            manifests.push(stem.to_owned());
        }
        Ok(Profile { manifests })
    }

    /// Render the on-disk text format.
    pub fn to_text(&self) -> String {
        let mut out = format!("{PROFILE_HEADER}\n");
        for manifest in &self.manifests {
            let _ = writeln!(out, "{manifest}");
        }
        out
    }
}

/// The result of resolving a name: the content hash it is bound to and an immutable
/// handle to the object, ready to hand to `load`/`compile`.
#[derive(Debug)]
pub struct Resolved {
    /// The name that was resolved.
    pub name: Name,
    /// The content hash the name is bound to.
    pub hash: ObjectHash,
    /// An immutable handle to the object (hash, store path, open read-only file).
    pub handle: ObjectHandle,
}

impl Store {
    /// Bind `name` to `hash` in the default manifest. The object must already be in the
    /// store — manifests never point at objects that do not exist.
    pub fn bind(&self, name: &Name, hash: ObjectHash) -> Result<(), StoreError> {
        self.bind_in(DEFAULT_MANIFEST, name, hash)
    }

    /// Bind `name` to `hash` in the named manifest, creating the manifest if needed.
    pub fn bind_in(&self, manifest: &str, name: &Name, hash: ObjectHash) -> Result<(), StoreError> {
        if !self.contains(&hash) {
            return Err(StoreError::MissingObject { hash });
        }
        let mut map = self.read_manifest(manifest)?.unwrap_or_default();
        map.set(name.clone(), hash);
        self.write_manifest(manifest, &map)
    }

    /// Resolve a name in the default profile.
    pub fn resolve(&self, name: &Name) -> Result<Resolved, StoreError> {
        self.resolve_in(DEFAULT_PROFILE, name)
    }

    /// Resolve a name in the named profile: the hash it is bound to plus an immutable
    /// handle to the object.
    pub fn resolve_in(&self, profile: &str, name: &Name) -> Result<Resolved, StoreError> {
        let Some(hash) = self.lookup_name_in(profile, name)? else {
            return Err(StoreError::UnknownName {
                name: name.clone(),
                profile: profile.to_owned(),
            });
        };
        let handle = self.open_object(&hash)?;
        Ok(Resolved {
            name: name.clone(),
            hash,
            handle,
        })
    }

    /// Look up the hash a name is bound to in the named profile, without opening the
    /// object. Returns `None` if the name is not bound.
    pub fn lookup_name_in(
        &self,
        profile: &str,
        name: &Name,
    ) -> Result<Option<ObjectHash>, StoreError> {
        // Later manifests shadow earlier ones, so walk the stack from the top down.
        for (_, map) in self.profile_stack(profile)?.into_iter().rev() {
            if let Some(hash) = map.get(name) {
                return Ok(Some(*hash));
            }
        }
        Ok(None)
    }

    /// The effective name map of the default profile, after shadowing.
    pub fn names(&self) -> Result<BTreeMap<Name, ObjectHash>, StoreError> {
        self.names_in(DEFAULT_PROFILE)
    }

    /// The effective name map of the named profile, after shadowing.
    pub fn names_in(&self, profile: &str) -> Result<BTreeMap<Name, ObjectHash>, StoreError> {
        let mut effective = BTreeMap::new();
        // Base first; later manifests overwrite, which is exactly the shadowing rule.
        for (_, map) in self.profile_stack(profile)? {
            for (name, hash) in map.iter() {
                effective.insert(name.clone(), *hash);
            }
        }
        Ok(effective)
    }

    /// Read a manifest by stem; `Ok(None)` if the file does not exist.
    pub fn read_manifest(&self, stem: &str) -> Result<Option<Manifest>, StoreError> {
        validate_file_stem(stem)?;
        let path = self.manifest_path(stem);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(Manifest::parse(&text, &path)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    /// Write a manifest by stem (atomically).
    pub fn write_manifest(&self, stem: &str, manifest: &Manifest) -> Result<(), StoreError> {
        validate_file_stem(stem)?;
        fsutil::write_atomic(
            &self.manifest_path(stem),
            manifest.to_text().as_bytes(),
            false,
        )
    }

    /// Read a profile by stem; `Ok(None)` if the file does not exist.
    pub fn read_profile(&self, stem: &str) -> Result<Option<Profile>, StoreError> {
        validate_file_stem(stem)?;
        let path = self.profile_path(stem);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(Profile::parse(&text, &path)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    /// Write a profile by stem (atomically).
    pub fn write_profile(&self, stem: &str, profile: &Profile) -> Result<(), StoreError> {
        validate_file_stem(stem)?;
        fsutil::write_atomic(
            &self.profile_path(stem),
            profile.to_text().as_bytes(),
            false,
        )
    }

    fn manifest_path(&self, stem: &str) -> PathBuf {
        self.manifests_dir().join(format!("{stem}.manifest"))
    }

    fn profile_path(&self, stem: &str) -> PathBuf {
        self.profiles_dir().join(format!("{stem}.profile"))
    }

    /// Load the manifests of a profile in stack order (base first).
    ///
    /// An explicit profile file that names a missing manifest is an error; the implicit
    /// single-manifest profile tolerates its manifest being absent (an empty store).
    fn profile_stack(&self, profile: &str) -> Result<Vec<(String, Manifest)>, StoreError> {
        validate_file_stem(profile)?;
        match self.read_profile(profile)? {
            Some(stack) => {
                let mut manifests = Vec::with_capacity(stack.manifests().len());
                for stem in stack.manifests() {
                    let Some(map) = self.read_manifest(stem)? else {
                        return Err(StoreError::MissingManifest {
                            profile: profile.to_owned(),
                            manifest: stem.clone(),
                        });
                    };
                    manifests.push((stem.clone(), map));
                }
                Ok(manifests)
            }
            None => {
                let map = self.read_manifest(profile)?.unwrap_or_default();
                Ok(vec![(profile.to_owned(), map)])
            }
        }
    }
}

/// Iterate over non-blank, non-comment lines with their 1-based line numbers.
fn significant_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
}

/// Consume and check the version header line.
fn expect_header<'a>(
    lines: &mut impl Iterator<Item = (usize, &'a str)>,
    expected: &str,
    path: &Path,
) -> Result<(), StoreError> {
    match lines.next() {
        Some((_, line)) if line == expected => Ok(()),
        Some((line_number, line)) => Err(StoreError::Corrupt {
            path: path.to_owned(),
            line: line_number,
            reason: format!("expected header {expected:?}, found {line:?}"),
        }),
        None => Err(StoreError::Corrupt {
            path: path.to_owned(),
            line: 1,
            reason: format!("empty file; expected header {expected:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    #[test]
    fn manifest_text_round_trips() {
        let mut manifest = Manifest::new();
        manifest.set(name("browser"), ObjectHash::of(b"browser"));
        manifest.set(name("virtualfs.create"), ObjectHash::of(b"create"));
        manifest.set(name("fs.memfs"), ObjectHash::of(b"memfs"));
        let text = manifest.to_text();
        let parsed = Manifest::parse(&text, Path::new("test.manifest")).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn manifest_ignores_comments_and_blank_lines() {
        let hash = ObjectHash::of(b"x");
        let text = format!("eo9-manifest 1\n\n# a comment\n  browser   {hash}  \n");
        let parsed = Manifest::parse(&text, Path::new("test.manifest")).unwrap();
        assert_eq!(parsed.get(&name("browser")), Some(&hash));
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn manifest_rejects_bad_input() {
        let hash = ObjectHash::of(b"x");
        let cases = [
            format!("browser {hash}\n"),                       // missing header
            "eo9-manifest 2\n".to_owned(),                     // wrong version
            "eo9-manifest 1\nbrowser\n".to_owned(),            // missing hash
            format!("eo9-manifest 1\nbrowser {hash} extra\n"), // trailing field
            "eo9-manifest 1\nbrowser abc\n".to_owned(),        // bad hash
            format!("eo9-manifest 1\nBad.Name {hash}\n"),      // bad name
            format!("eo9-manifest 1\nbrowser {hash}\nbrowser {hash}\n"), // duplicate
        ];
        for text in cases {
            assert!(
                Manifest::parse(&text, Path::new("test.manifest")).is_err(),
                "should reject {text:?}"
            );
        }
    }

    #[test]
    fn profile_text_round_trips() {
        let mut profile = Profile::new();
        profile.push("base").unwrap();
        profile.push("overrides").unwrap();
        let text = profile.to_text();
        let parsed = Profile::parse(&text, Path::new("test.profile")).unwrap();
        assert_eq!(parsed, profile);
        assert_eq!(parsed.manifests(), ["base", "overrides"]);
    }

    #[test]
    fn profile_rejects_bad_stems() {
        assert!(Profile::parse("eo9-profile 1\n../escape\n", Path::new("p")).is_err());
        let mut profile = Profile::new();
        assert!(profile.push("has.dot").is_err());
    }
}
