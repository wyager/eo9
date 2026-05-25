//! Rendering outcomes and component descriptions as text.
//!
//! Outcomes arrive as the executor-side three-way `program-outcome` (SPEC.md,
//! "Arguments and outcomes"): `success` and `failure` carry the program's own variants
//! as WAVE text, `abnormal` covers runs that never returned (trapped or killed). The
//! shell prints the WAVE value itself — the program's own vocabulary — behind a short
//! prefix saying which arm it was.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::backend::{AbnormalExit, ComponentInfo, ComponentKind, Outcome};

/// Render a program outcome as one line.
pub fn render_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Success(value) => format!("ok: {}", value.value),
        Outcome::Failure(value) => format!("error: {}", value.value),
        Outcome::Abnormal(AbnormalExit::Trapped(reason)) => format!("abnormal: trapped: {reason}"),
        Outcome::Abnormal(AbnormalExit::Killed) => String::from("abnormal: killed"),
    }
}

/// Render a component's kind, argument signature, imports, and exports (the `describe`
/// builtin).
pub fn render_info(info: &ComponentInfo) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "kind: {}",
        match info.kind {
            ComponentKind::Binary => "binary",
            ComponentKind::Provider => "provider",
        }
    ));

    if info.args.is_empty() {
        lines.push(String::from("args: (none)"));
    } else {
        lines.push(String::from("args:"));
        for arg in &info.args {
            lines.push(format!("  --{}: {}", arg.name, arg.ty));
        }
    }

    lines.extend(render_imports(info));

    if info.exports.is_empty() {
        lines.push(String::from("exports: (none)"));
    } else {
        lines.push(String::from("exports:"));
        for export in &info.exports {
            lines.push(format!(
                "  {} ({}@{})",
                export.name, export.interface, export.version
            ));
        }
    }

    lines
}

/// Render just the residual imports (the `imports` builtin).
pub fn render_imports(info: &ComponentInfo) -> Vec<String> {
    let mut lines = Vec::new();
    if info.imports.is_empty() {
        lines.push(String::from("imports: (none)"));
    } else {
        lines.push(String::from("imports:"));
        for import in &info.imports {
            lines.push(format!(
                "  {} {} ({}@{})",
                if import.required {
                    "required"
                } else {
                    "optional"
                },
                import.slot,
                import.interface,
                import.version
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    use crate::backend::{ArgSpec, ExportSlot, ImportNeed, WaveValue};

    #[test]
    fn outcomes_render_their_three_arms() {
        assert_eq!(
            render_outcome(&Outcome::Success(WaveValue {
                ty: "program-success".to_string(),
                value: "greeted".to_string(),
            })),
            "ok: greeted"
        );
        assert_eq!(
            render_outcome(&Outcome::Failure(WaveValue {
                ty: "program-failure".to_string(),
                value: "bad-arguments(\"rounds must be at least 1\")".to_string(),
            })),
            "error: bad-arguments(\"rounds must be at least 1\")"
        );
        assert_eq!(
            render_outcome(&Outcome::Abnormal(AbnormalExit::Trapped(
                "unreachable executed".to_string()
            ))),
            "abnormal: trapped: unreachable executed"
        );
        assert_eq!(
            render_outcome(&Outcome::Abnormal(AbnormalExit::Killed)),
            "abnormal: killed"
        );
    }

    #[test]
    fn component_info_renders_kind_args_imports_exports() {
        let info = ComponentInfo {
            kind: ComponentKind::Binary,
            imports: vec![
                ImportNeed {
                    slot: "eo9:net/net".to_string(),
                    interface: "eo9:net/net".to_string(),
                    version: "0.1.0".to_string(),
                    required: true,
                },
                ImportNeed {
                    slot: "scratch-fs".to_string(),
                    interface: "eo9:fs/fs".to_string(),
                    version: "0.1.0".to_string(),
                    required: false,
                },
            ],
            exports: vec![],
            args: vec![ArgSpec {
                name: "url".to_string(),
                ty: "string".to_string(),
            }],
        };
        assert_eq!(
            render_info(&info),
            vec![
                "kind: binary",
                "args:",
                "  --url: string",
                "imports:",
                "  required eo9:net/net (eo9:net/net@0.1.0)",
                "  optional scratch-fs (eo9:fs/fs@0.1.0)",
                "exports: (none)",
            ]
        );

        let provider = ComponentInfo {
            kind: ComponentKind::Provider,
            imports: vec![],
            exports: vec![ExportSlot {
                name: "eo9:fs/fs".to_string(),
                interface: "eo9:fs/fs".to_string(),
                version: "0.1.0".to_string(),
            }],
            args: vec![],
        };
        assert_eq!(
            render_info(&provider),
            vec![
                "kind: provider",
                "args: (none)",
                "imports: (none)",
                "exports:",
                "  eo9:fs/fs (eo9:fs/fs@0.1.0)",
            ]
        );
    }
}
