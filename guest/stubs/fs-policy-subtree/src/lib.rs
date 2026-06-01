//! `fs.policy-subtree` — the subtree path policy.
//!
//! Targets the `eo9:fs/subtree-policy` stub world: a pure policy component ("policies
//! are programs" — SPEC, Eo9 API design) exporting `eo9:fs/path-policy`, deciding by
//! path prefix: operations under the configured prefix are permitted (read-write or
//! read-only), everything outside it is denied.
//!
//!   fs.policy-subtree --prefix /docs --access read-write $ fs.filtered $ program
//!
//! * Unconfigured, the policy denies **everything** (never-trap rule, plan/09 D14).
//! * The component imports nothing — `describe` shows an empty capability surface — so
//!   the policy provably cannot do anything but compute its answer.
//! * Defense in depth: the policy normalizes incoming paths itself (`.`/`..` resolved)
//!   before the prefix comparison, even though `fs.filtered` already normalizes — so the
//!   subtree rule holds even under a middleware that forgot to. A path that escapes the
//!   root is always denied. Prefix matching is segment-aware (`/docs` does not admit
//!   `/docsx`).

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::provider::ProviderState;

wit_bindgen::generate!({
    world: "subtree-policy",
    path: "../../../wit/fs",
    generate_all,
});

use exports::eo9::fs::path_policy::{self, FsOperation, Verdict};
use exports::eo9::fs::subtree_config::{self, SubtreeAccess};

/// The configured rule: (normalized prefix, access granted inside it).
/// Unconfigured means "deny everything" (see the module docs).
static RULE: ProviderState<(String, SubtreeAccess)> = ProviderState::new();

/// Normalize an absolute path: resolve `.` and `..` segments and collapse repeated
/// separators. Returns `None` when the path escapes the root — such paths are always
/// denied.
fn normalize(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&segments.join("/"));
    Some(out)
}

/// Segment-aware prefix test: `path` is `prefix` itself or lies underneath it.
/// (`/docs` admits `/docs` and `/docs/x`, never `/docsx`.)
fn under(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The `fs.policy-subtree` policy.
struct Stub;

impl subtree_config::Guest for Stub {
    fn configure(prefix: String, access: SubtreeAccess) -> Result<(), String> {
        // The prefix itself must be a sane absolute path; normalize it once here so the
        // per-check comparison is normalized-against-normalized.
        let Some(normalized) = normalize(&prefix) else {
            return Err(String::from(
                "the subtree prefix escapes the root (too many `..` segments)",
            ));
        };
        RULE.set((normalized, access));
        Ok(())
    }
}

impl path_policy::Guest for Stub {
    fn check(path: String, op: FsOperation) -> Verdict {
        if !RULE.is_set() {
            // Unconfigured: deny everything.
            return Verdict::Deny;
        }
        // Defense in depth: normalize even though fs.filtered already did.
        let Some(path) = normalize(&path) else {
            return Verdict::Deny;
        };
        RULE.with(|(prefix, access)| {
            if !under(&path, prefix) {
                return Verdict::Deny;
            }
            match (*access, op) {
                (SubtreeAccess::ReadWrite, _) => Verdict::Allow,
                (SubtreeAccess::ReadOnly, FsOperation::Read) => Verdict::Allow,
                (SubtreeAccess::ReadOnly, FsOperation::Write) => Verdict::ReadOnly,
            }
        })
    }
}

export!(Stub);
