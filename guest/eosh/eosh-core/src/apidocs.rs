//! `describe` cards for the OS APIs themselves: packages (`describe eo9:pci`) and
//! interfaces (`describe eo9:pci/pci`).
//!
//! The card content is extracted at build time from the `wit/` doc comments (see
//! `build.rs`) — the WIT docs are the single source, so the cards cannot drift from the
//! APIs. The session adds a live section on top ("in this store: …") by scanning `/bin`
//! through the backend, so the static knowledge ("what is eo9:pci") and the local
//! situation ("who here exports it") arrive together — the same explain-itself thread
//! as the builtin cards (`builtins.rs`), in the same voice.
//!
//! Coverage discipline mirrors the builtin cards: `every_wit_package_and_interface_has_a_card`
//! independently re-scans `wit/` and asserts every package and interface renders within
//! the page column budget, so a new API cannot ship undescribed.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One package card: `describe eo9:pci`.
pub struct ApiPackageDoc {
    /// `eo9:pci`.
    pub name: &'static str,
    /// `0.1.0`.
    pub version: &'static str,
    /// The first paragraph of the package's WIT doc comment, pre-wrapped.
    pub summary: &'static [&'static str],
    /// `(full interface name, one-line summary)` in declaration order.
    pub interfaces: &'static [(&'static str, &'static str)],
    /// The package's world names (the provider shapes built from it).
    pub worlds: &'static [&'static str],
}

/// One interface card: `describe eo9:pci/pci`.
pub struct ApiInterfaceDoc {
    /// `eo9:pci`.
    pub package: &'static str,
    /// `pci` (the bare interface name).
    pub name: &'static str,
    /// The package version.
    pub version: &'static str,
    /// The first paragraph of the interface's WIT doc comment, pre-wrapped.
    pub summary: &'static [&'static str],
    /// `(function name, one-line summary)` in declaration order.
    pub functions: &'static [(&'static str, &'static str)],
}

include!(concat!(env!("OUT_DIR"), "/api_docs.rs"));

/// Kind line for package cards.
const PACKAGE_KIND: &str = "OS API package (a capability family; programs import its interfaces)";
/// Kind line for interface cards.
const INTERFACE_KIND: &str =
    "OS API interface (a typed capability surface; an import names one of these)";

/// What an API-shaped describe word resolved to.
pub enum ApiDoc {
    Package(&'static ApiPackageDoc),
    Interface(&'static ApiInterfaceDoc),
}

/// Look up an API name: a package (`eo9:pci`) or an interface (`eo9:pci/pci`), with an
/// optional `@version` suffix tolerated on either (`eo9:fs/fs@0.1.0` — the spelling
/// import lists use).
pub fn api_doc(word: &str) -> Option<ApiDoc> {
    let bare = word.split('@').next().unwrap_or(word);
    if let Some((package, interface)) = bare.split_once('/') {
        API_INTERFACES
            .iter()
            .find(|doc| doc.package == package && doc.name == interface)
            .map(ApiDoc::Interface)
    } else {
        API_PACKAGES
            .iter()
            .find(|doc| doc.name == bare)
            .map(ApiDoc::Package)
    }
}

/// Every known package name, for the not-found message.
pub fn package_names() -> Vec<&'static str> {
    API_PACKAGES.iter().map(|p| p.name).collect()
}

/// Render a package card (the static part; the session appends the live store section).
pub fn render_package(doc: &ApiPackageDoc) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("kind: {PACKAGE_KIND}"));
    lines.push(format!("package: {}@{}", doc.name, doc.version));
    for line in doc.summary {
        lines.push(String::from(*line));
    }
    lines.push(String::from("interfaces:"));
    for (name, summary) in doc.interfaces {
        if summary.is_empty() {
            lines.push(format!("  {name}"));
        } else {
            lines.push(format!("  {name} — {summary}"));
        }
    }
    if !doc.worlds.is_empty() {
        let mut line = String::from("worlds (the provider shapes this package defines): ");
        for (index, world) in doc.worlds.iter().enumerate() {
            let piece = if index == 0 {
                String::from(*world)
            } else {
                format!(", {world}")
            };
            if line.chars().count() + piece.chars().count() > 109 {
                line.push(',');
                lines.push(line);
                line = format!("  {world}");
            } else {
                line.push_str(&piece);
            }
        }
        lines.push(line);
    }
    if let Some((first, _)) = doc.interfaces.iter().find(|(n, _)| !n.ends_with("/types")) {
        lines.push(format!("related: describe {first}, only, imports"));
    } else {
        lines.push(String::from("related: only, imports, env"));
    }
    lines
}

/// Render an interface card (the static part; the session appends the live section).
pub fn render_interface(doc: &ApiInterfaceDoc) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("kind: {INTERFACE_KIND}"));
    lines.push(format!(
        "interface: {}/{}@{} (package {})",
        doc.package, doc.name, doc.version, doc.package
    ));
    for line in doc.summary {
        lines.push(String::from(*line));
    }
    if !doc.functions.is_empty() {
        lines.push(String::from("functions:"));
        for (name, summary) in doc.functions {
            if summary.is_empty() {
                lines.push(format!("  {name}"));
            } else {
                lines.push(format!("  {name} — {summary}"));
            }
        }
    }
    lines.push(format!("related: describe {}, imports, env", doc.package));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    /// The page terminal's column budget (mirrors `builtins.rs`).
    const COLUMN_BUDGET: usize = 109;

    /// Independently re-scan `wit/` (simple line grep, deliberately not the build
    /// script's parser) and assert every package and interface has a card that renders
    /// within budget — a new API cannot ship undescribed.
    #[test]
    fn every_wit_package_and_interface_has_a_card() {
        let wit_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../wit");
        let mut packages_seen = 0usize;
        let mut interfaces_seen = 0usize;
        for dir in std::fs::read_dir(&wit_root).expect("read wit/") {
            let dir = dir.expect("entry").path();
            if !dir.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&dir).expect("read package dir") {
                let file = file.expect("entry").path();
                if file.extension().is_none_or(|x| x != "wit") {
                    continue;
                }
                let text = std::fs::read_to_string(&file).expect("read wit");
                let mut package = String::new();
                for line in text.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("package ") {
                        package = rest
                            .trim_end_matches(';')
                            .split('@')
                            .next()
                            .unwrap_or_default()
                            .to_string();
                        packages_seen += 1;
                        let doc = api_doc(&package);
                        assert!(
                            matches!(doc, Some(ApiDoc::Package(_))),
                            "wit package `{package}` has no describe card"
                        );
                        if let Some(ApiDoc::Package(doc)) = doc {
                            assert_card(&render_package(doc), &package);
                            assert!(
                                !doc.summary.is_empty(),
                                "package `{package}` needs a /// doc paragraph for its card"
                            );
                        }
                    } else if line.starts_with("interface ") {
                        let name = trimmed
                            .trim_start_matches("interface ")
                            .split_whitespace()
                            .next()
                            .unwrap_or_default();
                        interfaces_seen += 1;
                        let full = std::format!("{package}/{name}");
                        let doc = api_doc(&full);
                        assert!(
                            matches!(doc, Some(ApiDoc::Interface(_))),
                            "wit interface `{full}` has no describe card"
                        );
                        if let Some(ApiDoc::Interface(doc)) = doc {
                            assert_card(&render_interface(doc), &full);
                        }
                    }
                }
            }
        }
        assert!(packages_seen >= 14, "the wit/ scan found too few packages");
        assert!(
            interfaces_seen > packages_seen,
            "the wit/ scan found too few interfaces"
        );
    }

    fn assert_card(lines: &[String], what: &str) {
        assert!(
            lines.first().is_some_and(|l| l.starts_with("kind: ")),
            "`{what}`'s card must open with its kind"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("related: ")),
            "`{what}`'s card must point somewhere next"
        );
        for line in lines {
            assert!(
                line.chars().count() <= COLUMN_BUDGET,
                "`{what}`'s card line exceeds the {COLUMN_BUDGET}-column budget: {line:?}"
            );
        }
    }

    #[test]
    fn lookup_handles_versions_and_unknowns() {
        assert!(matches!(api_doc("eo9:fs"), Some(ApiDoc::Package(_))));
        assert!(matches!(api_doc("eo9:fs/fs"), Some(ApiDoc::Interface(_))));
        assert!(matches!(
            api_doc("eo9:fs/fs@0.1.0"),
            Some(ApiDoc::Interface(_))
        ));
        assert!(api_doc("eo9:nope").is_none());
        assert!(api_doc("eo9:fs/nope").is_none());
        assert!(api_doc("wasi:io/streams").is_none());
    }
}
