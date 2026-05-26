//! The `env` builtin's capability picture: the session manifest and its rendering.
//!
//! The shell is an ordinary Eo9 program — it has no private way to ask the runtime which
//! root capabilities its session holds or what children spawned from it will receive.
//! The embedder that builds the session (usermode `eo9 shell`, later the kernel's
//! boot-to-shell) *does* know, and it leaves a small plain-text manifest where the shell
//! can read it with a capability it already holds: the session filesystem, at
//! [`SESSION_MANIFEST_PATH`]. `env` renders that manifest; `env <expr>` combines it with
//! `describe` to show how this session would treat a program's imports.
//!
//! The manifest is informational only. A missing or malformed manifest means the
//! information is unavailable — it never changes what the shell or its children can
//! actually do (the runtime's linking rules are the authority).
//!
//! Format (one record per line, first line is a magic/version marker):
//!
//! ```text
//! eo9-session 1
//! shell <capability> <description…>
//! child <capability> <description…>
//! note <free-form text…>
//! ```

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::backend::ComponentInfo;

/// Where the embedder leaves the session manifest on the shell's granted filesystem.
/// (The usermode embedder writes `<session-dir>/session`; keep the two in sync.)
pub const SESSION_MANIFEST_PATH: &str = "/session";

/// One granted capability: its short name (`text`, `fs`, …) and a human description of
/// what backs it in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityLine {
    pub capability: String,
    pub description: String,
}

/// The parsed session manifest: what the shell holds, what its children receive, and
/// any free-form notes the embedder wants shown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionManifest {
    pub shell: Vec<CapabilityLine>,
    pub child: Vec<CapabilityLine>,
    pub notes: Vec<String>,
}

impl SessionManifest {
    /// Parse the manifest text. `None` if the magic line is missing or no record parses
    /// (an unknown record kind is skipped, not fatal, so the format can grow).
    pub fn parse(text: &str) -> Option<SessionManifest> {
        let mut lines = text.lines();
        let magic = lines.next()?.trim();
        if magic != "eo9-session 1" {
            return None;
        }

        let mut manifest = SessionManifest::default();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((kind, rest)) = line.split_once(' ') else {
                continue;
            };
            match kind {
                "shell" | "child" => {
                    let (capability, description) = match rest.split_once(' ') {
                        Some((capability, description)) => {
                            (capability.to_string(), description.trim().to_string())
                        }
                        None => (rest.to_string(), String::new()),
                    };
                    let entry = CapabilityLine {
                        capability,
                        description,
                    };
                    if kind == "shell" {
                        manifest.shell.push(entry);
                    } else {
                        manifest.child.push(entry);
                    }
                }
                "note" => manifest.notes.push(rest.trim().to_string()),
                // Unknown record kinds are ignored so older shells keep working when the
                // embedder learns to say more.
                _ => {}
            }
        }
        Some(manifest)
    }

    /// Does this session hand the named capability to children spawned from the shell?
    pub fn child_has(&self, capability: &str) -> bool {
        self.child.iter().any(|line| line.capability == capability)
    }
}

/// The short capability name behind an interface reference: `eo9:fs/fs@0.1.0` → `fs`.
/// `None` for interfaces outside the `eo9:` namespace.
pub fn capability_of(interface: &str) -> Option<&str> {
    let rest = interface.strip_prefix("eo9:")?;
    let package = rest.split('/').next()?;
    if package.is_empty() {
        None
    } else {
        Some(package)
    }
}

/// Is this interface one that every program gets regardless of grants? Types-only
/// interfaces (`eo9:*/types`) and the owned-buffer plumbing (`eo9:io/*`) carry no
/// authority, so the runtime links them for any program that asks.
fn always_available(interface: &str) -> bool {
    interface.starts_with("eo9:io/") || interface.ends_with("/types")
}

/// Render the plain `env` view: what the shell holds and what children receive.
pub fn render_session(manifest: &SessionManifest) -> Vec<String> {
    let mut lines = Vec::new();

    lines.push(String::from("capabilities granted to this shell:"));
    if manifest.shell.is_empty() {
        lines.push(String::from("  (none)"));
    }
    for entry in &manifest.shell {
        lines.push(format!("  {:<8} {}", entry.capability, entry.description));
    }

    lines.push(String::from("programs started from this shell receive:"));
    if manifest.child.is_empty() {
        lines.push(String::from("  (none)"));
    }
    for entry in &manifest.child {
        lines.push(format!("  {:<8} {}", entry.capability, entry.description));
    }

    for note in &manifest.notes {
        lines.push(format!("  note: {note}"));
    }

    lines
}

/// Render the `env <expr>` view: the expression's imports, each marked with how this
/// session would treat it when the expression is run (satisfied by a session grant,
/// always available, optional-and-absent, or missing and therefore refused at spawn).
///
/// When `manifest` is `None` the session grants are unknown, so imports are listed with
/// that caveat instead of a verdict.
pub fn render_capability_view(
    info: &ComponentInfo,
    manifest: Option<&SessionManifest>,
) -> Vec<String> {
    let mut lines = Vec::new();

    if info.imports.is_empty() {
        lines.push(String::from(
            "imports: (none) — runs without any capabilities",
        ));
        return lines;
    }

    lines.push(String::from("imports, as this session treats them:"));
    for import in &info.imports {
        let requirement = if import.required {
            "required"
        } else {
            "optional"
        };
        let reference = format!("{}@{}", import.interface, import.version);
        let status = import_status(&import.interface, import.required, manifest);
        lines.push(format!("  {requirement} {reference} — {status}"));
    }

    if manifest.is_none() {
        lines.push(String::from(
            "  (session grant information unavailable; showing imports only)",
        ));
    }

    lines
}

/// One import's verdict under the session's grants.
fn import_status(interface: &str, required: bool, manifest: Option<&SessionManifest>) -> String {
    if always_available(interface) {
        return String::from("always available (carries no authority)");
    }

    let capability = capability_of(interface);
    let Some(manifest) = manifest else {
        return match capability {
            Some(capability) if required => format!("needs the `{capability}` capability"),
            Some(capability) => format!("uses the `{capability}` capability if present"),
            None => String::from("outside the eo9 capability namespace"),
        };
    };

    match capability {
        Some(capability) if manifest.child_has(capability) => {
            format!("satisfied by the session ({capability})")
        }
        Some(capability) if required => format!(
            "missing — would be refused at spawn; compose a provider (e.g. `{capability}.none $ …`) \
             or grant `{capability}` to the session"
        ),
        Some(_) => String::from("absent — the program will observe absence"),
        None if required => String::from(
            "missing — no session capability provides this interface; compose a provider",
        ),
        None => String::from("absent — the program will observe absence"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    use crate::backend::{ComponentKind, ImportNeed};

    fn manifest_text() -> &'static str {
        "eo9-session 1\n\
         shell text terminal standard streams\n\
         shell exec spawn programs as children\n\
         child text terminal standard streams\n\
         child time host clocks\n\
         child entropy host OS RNG\n\
         note children never receive the exec capability\n"
    }

    fn import(interface: &str, required: bool) -> ImportNeed {
        ImportNeed {
            slot: interface.to_string(),
            interface: interface.to_string(),
            version: "0.1.0".to_string(),
            required,
        }
    }

    #[test]
    fn manifest_round_trips_shell_child_and_notes() {
        let manifest = SessionManifest::parse(manifest_text()).expect("parses");
        assert_eq!(manifest.shell.len(), 2);
        assert_eq!(manifest.shell[0].capability, "text");
        assert_eq!(manifest.shell[0].description, "terminal standard streams");
        assert_eq!(manifest.child.len(), 3);
        assert!(manifest.child_has("time"));
        assert!(!manifest.child_has("fs"));
        assert_eq!(
            manifest.notes,
            vec!["children never receive the exec capability".to_string()]
        );
    }

    #[test]
    fn manifest_requires_the_magic_line_but_skips_unknown_records() {
        assert_eq!(SessionManifest::parse("not a manifest"), None);
        assert_eq!(SessionManifest::parse(""), None);
        let grown = "eo9-session 1\nshell text stdio\nfuture-record something\n";
        let manifest = SessionManifest::parse(grown).expect("parses");
        assert_eq!(manifest.shell.len(), 1);
        assert!(manifest.child.is_empty());
    }

    #[test]
    fn capability_names_come_from_the_interface_package() {
        assert_eq!(capability_of("eo9:fs/fs"), Some("fs"));
        assert_eq!(capability_of("eo9:exec/component-algebra"), Some("exec"));
        assert_eq!(capability_of("wasi:http/outgoing-handler"), None);
    }

    #[test]
    fn session_rendering_lists_both_grant_sets_and_notes() {
        let manifest = SessionManifest::parse(manifest_text()).expect("parses");
        let lines = render_session(&manifest);
        assert_eq!(lines[0], "capabilities granted to this shell:");
        assert!(lines.iter().any(|l| l.contains("exec")));
        assert!(
            lines
                .iter()
                .any(|l| l == "programs started from this shell receive:")
        );
        assert!(lines.iter().any(|l| l.contains("note: children never")));
    }

    #[test]
    fn capability_view_classifies_each_import() {
        let manifest = SessionManifest::parse(manifest_text()).expect("parses");
        let info = ComponentInfo {
            kind: ComponentKind::Binary,
            imports: vec![
                import("eo9:text/text", true),
                import("eo9:fs/fs", true),
                import("eo9:net/net", false),
                import("eo9:time/types", true),
                import("eo9:io/buffers", true),
            ],
            exports: Vec::new(),
            args: Vec::new(),
        };
        let lines = render_capability_view(&info, Some(&manifest));
        let line_for = |needle: &str| {
            lines
                .iter()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no line mentions {needle}: {lines:?}"))
                .clone()
        };
        assert!(line_for("eo9:text/text").contains("satisfied by the session (text)"));
        assert!(line_for("eo9:fs/fs").contains("missing — would be refused at spawn"));
        assert!(line_for("eo9:fs/fs").contains("fs.none $"));
        assert!(line_for("eo9:net/net").contains("absent — the program will observe absence"));
        assert!(line_for("eo9:time/types").contains("always available"));
        assert!(line_for("eo9:io/buffers").contains("always available"));
    }

    #[test]
    fn capability_view_without_a_manifest_still_lists_imports() {
        let info = ComponentInfo {
            kind: ComponentKind::Binary,
            imports: vec![import("eo9:fs/fs", true), import("eo9:net/net", false)],
            exports: Vec::new(),
            args: Vec::new(),
        };
        let lines = render_capability_view(&info, None);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("needs the `fs` capability"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("uses the `net` capability if present"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("session grant information unavailable"))
        );
    }

    #[test]
    fn a_program_with_no_imports_is_called_out() {
        let info = ComponentInfo {
            kind: ComponentKind::Binary,
            imports: Vec::new(),
            exports: Vec::new(),
            args: Vec::new(),
        };
        let manifest = SessionManifest::parse(manifest_text()).expect("parses");
        assert_eq!(
            render_capability_view(&info, Some(&manifest)),
            vec!["imports: (none) — runs without any capabilities".to_string()]
        );
    }
}
