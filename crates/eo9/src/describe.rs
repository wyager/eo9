//! `eo9 describe`: print a component's kind, imports, exports, and argument signature,
//! via the component algebra's `describe` (SPEC.md "Execution APIs").

use eo9_component::{Component, ComponentKind, compose};

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

/// `eo9 describe --wiring <ref>…`: render the composition/wiring tree. With one reference
/// it shows that component (a leaf); with several it composes them as a `$`-chain
/// (right-associative: the last is the consumer, the earlier are providers layered onto
/// it) and renders the resulting tree, making interposed attenuators visible — the audit
/// view plain `describe` cannot give (the composition is built here, in-process; a
/// component's bytes do not carry their construction history).
pub fn cmd_wiring(cfg: &Config, references: &[String]) -> Result<u8, String> {
    let mut components: Vec<Component> = Vec::with_capacity(references.len());
    for reference in references {
        let (bytes, origin) = source::read_component(cfg, reference)?;
        let component = Component::load(bytes)
            .map_err(|err| format!("{origin}: not a loadable component: {err}"))?
            .with_label(reference.clone());
        components.push(component);
    }

    // Fold right: the last reference is the consumer; each earlier one is composed onto
    // the accumulator as a provider (`A $ B $ C`).
    let mut acc = components
        .pop()
        .expect("the caller passes at least one reference");
    while let Some(provider) = components.pop() {
        acc = compose(&provider, &acc).map_err(|err| format!("composition failed: {err}"))?;
    }

    println!("wiring:");
    print!("{}", acc.wiring_tree());
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
