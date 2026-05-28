//! Composition provenance: the wiring tree of how a `Component` was built.
//!
//! Every algebra operation records what it did onto its result's [`Wiring`], so a composed
//! value carries the full structure of its construction -- which provider was layered onto
//! which consumer, what each layer satisfies, what `only` sealed, what was renamed or
//! configured. This is what makes interposed attenuators visible: `describe` of a plain
//! component shows only its residual surface, so `fs.readonly $ cat` looks identical to
//! `cat`; the wiring tree shows the `fs.readonly` layer sitting between the grant and `cat`.
//!
//! Provenance is in-memory metadata only. It does NOT live in the component bytes, never
//! changes `save()`/`executable_bytes()`/the content hash, and is not part of `Component`
//! equality -- a component loaded from the store (whose composition happened in some other
//! process) is a [`Wiring::Leaf`], because the structure was never in its bytes. The full
//! tree appears for compositions built in this process (e.g. `eo9 describe --wiring`).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ComponentKind;
use crate::describe::Meta;

/// How a [`Component`](crate::Component) was constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wiring {
    /// A component loaded from bytes, with no recorded composition history.
    Leaf {
        /// A human label (e.g. the store name the CLI resolved), if known.
        label: Option<String>,
        /// Binary or provider.
        kind: ComponentKind,
        /// The interfaces it exports.
        exports: Vec<String>,
        /// The interfaces it still requires (required, authority-carrying imports).
        imports: Vec<String>,
    },
    /// `provider $ consumer` -- the provider's matching exports satisfy the consumer.
    Compose {
        /// The layered-on provider's wiring.
        provider: Box<Wiring>,
        /// The consumer's wiring.
        consumer: Box<Wiring>,
        /// The interfaces the provider sealed for the consumer.
        satisfied: Vec<String>,
    },
    /// `base & layer` -- environment extension with right-biased shadowing.
    Extend {
        /// The base environment's wiring.
        base: Box<Wiring>,
        /// The extending layer's wiring.
        layer: Box<Wiring>,
        /// The base export slots the layer shadowed.
        shadowed: Vec<String>,
    },
    /// `only <allow> $ body` -- restriction to an allow-list.
    Restrict {
        /// The allow-list entries.
        allow: Vec<String>,
        /// The optional interfaces sealed as absent (outside the allow-list).
        sealed_absent: Vec<String>,
        /// The restricted component's wiring.
        body: Box<Wiring>,
    },
    /// `rename from to $ body` -- a slot relabel.
    Rename {
        /// The original slot name.
        from: String,
        /// The new slot name.
        to: String,
        /// The renamed component's wiring.
        body: Box<Wiring>,
    },
    /// `configure(provider, args)` -- compose-time configuration bound and sealed.
    Configure {
        /// The bound `name=value` arguments.
        args: Vec<String>,
        /// The configured provider's wiring.
        body: Box<Wiring>,
    },
}

impl Wiring {
    /// The leaf wiring of a freshly loaded component, from its metadata.
    pub(crate) fn leaf(meta: &Meta) -> Self {
        let exports = meta
            .exports
            .iter()
            .filter(|e| !e.interface.is_empty())
            .map(|e| e.interface.clone())
            .collect();
        let imports = meta
            .imports
            .iter()
            .filter(|i| i.required && !i.authority_free && !i.interface.is_empty())
            .map(|i| i.interface.clone())
            .collect();
        Wiring::Leaf {
            label: None,
            kind: meta.kind,
            exports,
            imports,
        }
    }

    /// Attach a human label to a leaf (no-op on a composed node).
    pub(crate) fn set_label(&mut self, name: impl Into<String>) {
        if let Wiring::Leaf { label, .. } = self {
            *label = Some(name.into());
        }
    }

    /// Render the wiring as an indented tree (one node per line, children indented).
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0, "");
        out
    }

    fn write(&self, out: &mut String, depth: usize, role: &str) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        if !role.is_empty() {
            out.push_str(role);
            out.push_str(": ");
        }
        out.push_str(&self.summary());
        out.push('\n');
        match self {
            Wiring::Leaf { .. } => {}
            Wiring::Compose {
                provider, consumer, ..
            } => {
                provider.write(out, depth + 1, "provider");
                consumer.write(out, depth + 1, "consumer");
            }
            Wiring::Extend { base, layer, .. } => {
                base.write(out, depth + 1, "base");
                layer.write(out, depth + 1, "layer");
            }
            Wiring::Restrict { body, .. }
            | Wiring::Rename { body, .. }
            | Wiring::Configure { body, .. } => {
                body.write(out, depth + 1, "of");
            }
        }
    }

    /// The single-line description of this node.
    fn summary(&self) -> String {
        match self {
            Wiring::Leaf {
                label,
                kind,
                exports,
                imports,
            } => {
                let kind = match kind {
                    ComponentKind::Binary => "binary",
                    ComponentKind::Provider => "provider",
                };
                let mut s = match label {
                    Some(name) => format!("{name} [{kind}]"),
                    None => format!("[{kind}]"),
                };
                if !exports.is_empty() {
                    s.push_str(&format!("  exports: {}", exports.join(", ")));
                }
                if !imports.is_empty() {
                    s.push_str(&format!("  imports: {}", imports.join(", ")));
                }
                if exports.is_empty() && imports.is_empty() {
                    s.push_str("  (no capabilities)");
                }
                s
            }
            Wiring::Compose { satisfied, .. } => {
                if satisfied.is_empty() {
                    "$ compose (provider satisfies nothing — dead layer)".to_string()
                } else {
                    format!("$ compose (provider satisfies: {})", satisfied.join(", "))
                }
            }
            Wiring::Extend { shadowed, .. } => {
                if shadowed.is_empty() {
                    "& extend".to_string()
                } else {
                    format!("& extend (layer shadows: {})", shadowed.join(", "))
                }
            }
            Wiring::Restrict {
                allow,
                sealed_absent,
                ..
            } => {
                let mut s = format!("only [{}]", allow.join(", "));
                if !sealed_absent.is_empty() {
                    s.push_str(&format!(" (sealed absent: {})", sealed_absent.join(", ")));
                }
                s
            }
            Wiring::Rename { from, to, .. } => format!("rename {from} -> {to}"),
            Wiring::Configure { args, .. } => {
                if args.is_empty() {
                    "configure()".to_string()
                } else {
                    format!("configure({})", args.join(", "))
                }
            }
        }
    }
}
