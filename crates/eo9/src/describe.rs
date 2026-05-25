//! `eo9 describe`: print a component's kind, imports, exports, and argument signature,
//! via the component algebra's `describe` (SPEC.md "Execution APIs").

use eo9_component::{Component, ComponentKind};

use crate::cli::{Config, EXIT_SUCCESS};
use crate::source;

pub fn cmd_describe(cfg: &Config, reference: &str) -> Result<u8, String> {
    let (bytes, origin) = source::read_component(cfg, reference)?;
    let info = Component::load(bytes)
        .map_err(|err| format!("{origin}: not a loadable component: {err}"))?
        .describe();

    println!("component: {origin}");
    let kind = match info.kind {
        ComponentKind::Binary => "binary",
        ComponentKind::Provider => "provider",
    };
    println!("kind: {kind}");

    println!("imports:");
    if info.imports.is_empty() {
        println!("  (none)");
    }
    for need in &info.imports {
        let requirement = if need.required {
            "required"
        } else {
            "optional"
        };
        let interface = interface_ref(&need.interface, &need.version);
        if need.slot == need.interface {
            println!("  {interface} ({requirement})");
        } else {
            println!("  {}: {interface} ({requirement})", need.slot);
        }
    }

    println!("exports:");
    if info.exports.is_empty() {
        println!("  (none)");
    }
    for slot in &info.exports {
        let interface = interface_ref(&slot.interface, &slot.version);
        if slot.name == slot.interface {
            println!("  {interface}");
        } else {
            println!("  {}: {interface}", slot.name);
        }
    }

    let entry = match info.kind {
        ComponentKind::Binary => "main",
        ComponentKind::Provider => "configure",
    };
    println!("{entry} arguments:");
    if info.args.is_empty() {
        println!("  (none)");
    }
    for arg in &info.args {
        println!("  --{} <{}>", arg.name, arg.ty);
    }

    Ok(EXIT_SUCCESS)
}

fn interface_ref(interface: &str, version: &str) -> String {
    if version.is_empty() {
        interface.to_string()
    } else {
        format!("{interface}@{version}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_refs_only_carry_a_version_when_there_is_one() {
        assert_eq!(
            interface_ref("eo9:text/text", "0.1.0"),
            "eo9:text/text@0.1.0"
        );
        assert_eq!(interface_ref("eo9:text/text", ""), "eo9:text/text");
    }
}
