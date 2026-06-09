//! Component manuals: the `eo9-manual` custom-section reader, parser, and renderer
//! (docs/design/component-manuals.md).
//!
//! A component may carry a **self-described manual** as a wasm custom section named
//! `eo9-manual` — versioned, line-oriented UTF-8, embedded at compile time by the guest
//! SDK's `manual!` macro. The `man` builtin renders it; the incremental REPL's argument
//! grammars will consume the per-arg hints (strictly additively).
//!
//! Three parts, all `no_std + alloc`, no dependencies:
//!
//! * [`extract_manual`] — a two-level custom-section scan: the outer container's
//!   sections first, then (for a component) each depth-1 core module's sections; first
//!   hit wins. LEB128 framing only — this is a **container parser, never a validator**:
//!   any malformed length simply ends the scan, and broken bytes degrade to "no
//!   manual". Nested *components* (a saved composition's operands) are deliberately not
//!   descended into: a fused artifact has no top-level manual and falls back to
//!   `describe`, which is honest — a composition's behavior is the algebra's, not one
//!   part's prose.
//! * [`parse_manual`] — the schema-v1 parser, with the hard caps (16 KiB section, 64
//!   args, 16 examples, 120-byte lines). Unknown keys are skipped (forward
//!   compatibility); a second `eo9-manual` header before `end` is an error (the
//!   defense against lld concatenating two crates' sections).
//! * [`render_manual`] — display only. Control bytes are stripped (no escape
//!   injection), every line wraps to the 109-column page budget, and when the WIT
//!   argument signature is available each documented argument is checked against it:
//!   the manual is self-reported and unverified, so a disagreement is FLAGGED rather
//!   than either side trusted.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::backend::ArgSpec;

/// The custom-section name the guest SDK's `manual!` macro emits.
pub const SECTION_NAME: &str = "eo9-manual";

/// Hard cap on the section payload (anything larger is malformed, not truncated).
pub const MAX_SECTION_BYTES: usize = 16 * 1024;
/// Hard cap on documented arguments.
pub const MAX_ARGS: usize = 64;
/// Hard cap on examples.
pub const MAX_EXAMPLES: usize = 16;
/// Hard cap on one schema line, in bytes.
pub const MAX_LINE_BYTES: usize = 120;

/// The page column budget every rendered line stays within (the try-it page terminal —
/// the same budget as the builtin and API cards).
const COLUMN_BUDGET: usize = 109;

// ---------------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------------

/// Read one unsigned LEB128 u32 at `*pos`, advancing it. `None` on truncation or a
/// value that does not fit in 32 bits.
fn read_leb_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        if shift == 28 && byte & 0x70 != 0 {
            return None;
        }
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift > 28 {
            return None;
        }
    }
}

/// If `payload` is a custom-section body whose name is [`SECTION_NAME`], its data.
fn custom_section_data(payload: &[u8]) -> Option<&[u8]> {
    let mut pos = 0usize;
    let name_len = read_leb_u32(payload, &mut pos)? as usize;
    let name_end = pos.checked_add(name_len)?;
    if name_end > payload.len() {
        return None;
    }
    if &payload[pos..name_end] != SECTION_NAME.as_bytes() {
        return None;
    }
    Some(&payload[name_end..])
}

/// Walk one container's sections. Returns the first `eo9-manual` custom payload at
/// this level and, when the container is a component (preamble layer 1), the payload
/// of every core-module section (id 1), in order. Framing errors end the walk quietly.
fn scan_level(bytes: &[u8]) -> (Option<&[u8]>, Vec<&[u8]>) {
    let mut modules: Vec<&[u8]> = Vec::new();
    if bytes.len() < 8 || bytes[0..4] != *b"\0asm" {
        return (None, modules);
    }
    // Preamble bytes 4..8: version (2 bytes) + layer (2 bytes). Layer 0 = core module,
    // layer 1 = component. The version is deliberately not checked — framing only.
    let is_component = bytes[6] == 1;
    let mut pos = 8usize;
    while pos < bytes.len() {
        let id = bytes[pos];
        pos += 1;
        let Some(size) = read_leb_u32(bytes, &mut pos) else {
            break;
        };
        let Some(end) = pos.checked_add(size as usize) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        let payload = &bytes[pos..end];
        match id {
            0 => {
                if let Some(data) = custom_section_data(payload) {
                    return (Some(data), modules);
                }
            }
            1 if is_component => modules.push(payload),
            _ => {}
        }
        pos = end;
    }
    (None, modules)
}

/// Find the `eo9-manual` section in a component (or bare core module): the outer
/// container's custom sections first, then each depth-1 core module's; first hit wins.
/// `None` means "no manual" — including for malformed bytes (framing only, never a
/// validator) and for saved compositions, whose operands nest as components and are
/// not descended into.
pub fn extract_manual(bytes: &[u8]) -> Option<&[u8]> {
    let (own, modules) = scan_level(bytes);
    if own.is_some() {
        return own;
    }
    for module in modules {
        let (found, _) = scan_level(module);
        if found.is_some() {
            return found;
        }
    }
    None
}

// ---------------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------------

/// One documented argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualArg {
    pub name: String,
    /// Advisory type text (the WIT `ArgSpec.ty` stays the mechanical truth; for an
    /// `option<T>` parameter authors write `T` with `optional`).
    pub ty: String,
    pub required: bool,
    pub doc: Vec<String>,
    /// Literal value alternatives, comma-separated (`values: dhcp`). At most one of
    /// `values`/`kind` is present.
    pub values: Option<String>,
    /// A value vocabulary tag (v1 knows url, path, component-name, interface-name,
    /// port; unknown kinds are display text only).
    pub kind: Option<String>,
}

/// One example invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualExample {
    pub line: String,
    pub doc: Vec<String>,
}

/// A parsed manual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manual {
    pub name: String,
    pub synopsis: String,
    pub description: Vec<String>,
    pub args: Vec<ManualArg>,
    pub examples: Vec<ManualExample>,
    pub see_also: Option<String>,
}

/// Why a present `eo9-manual` section could not be used (the user sees "manual
/// malformed (<this>); showing describe").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualError {
    /// The section exceeds [`MAX_SECTION_BYTES`].
    TooLarge { bytes: usize },
    /// The payload is not UTF-8.
    NotUtf8,
    /// The first line is not `eo9-manual <major>`.
    BadHeader,
    /// The major version is not 1.
    UnsupportedVersion(u32),
    /// A line exceeds [`MAX_LINE_BYTES`] bytes (1-based line number).
    LineTooLong { line: usize },
    /// A second `eo9-manual` header before `end` (two concatenated sections).
    DuplicateHeader { line: usize },
    /// More than [`MAX_ARGS`] `arg` blocks.
    TooManyArgs,
    /// More than [`MAX_EXAMPLES`] `example` blocks.
    TooManyExamples,
    /// An `arg` header that is not `arg <name> <type-text> <required|optional>`.
    BadArgHeader { line: usize },
    /// An arg carries both `values:` and `kind:` (at most one is allowed).
    ValuesAndKind { arg: String },
    /// No `name:` line.
    MissingName,
    /// No `synopsis:` line.
    MissingSynopsis,
    /// The section ended without an `end` line (a truncated write).
    MissingEnd,
}

impl fmt::Display for ManualError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManualError::TooLarge { bytes } => {
                write!(f, "{bytes} bytes exceeds the {MAX_SECTION_BYTES}-byte cap")
            }
            ManualError::NotUtf8 => write!(f, "not UTF-8"),
            ManualError::BadHeader => write!(f, "missing the `eo9-manual <version>` header"),
            ManualError::UnsupportedVersion(major) => {
                write!(f, "schema version {major} (this reader knows version 1)")
            }
            ManualError::LineTooLong { line } => {
                write!(f, "line {line} exceeds {MAX_LINE_BYTES} bytes")
            }
            ManualError::DuplicateHeader { line } => {
                write!(
                    f,
                    "a second header at line {line} (two concatenated manuals)"
                )
            }
            ManualError::TooManyArgs => write!(f, "more than {MAX_ARGS} args"),
            ManualError::TooManyExamples => write!(f, "more than {MAX_EXAMPLES} examples"),
            ManualError::BadArgHeader { line } => write!(
                f,
                "line {line} is not `arg <name> <type> <required|optional>`"
            ),
            ManualError::ValuesAndKind { arg } => {
                write!(f, "arg `{arg}` carries both `values:` and `kind:`")
            }
            ManualError::MissingName => write!(f, "no `name:` line"),
            ManualError::MissingSynopsis => write!(f, "no `synopsis:` line"),
            ManualError::MissingEnd => write!(f, "no `end` line (truncated)"),
        }
    }
}

/// Which block indented lines currently attach to.
enum Block {
    None,
    Description,
    Arg,
    Example,
}

/// Parse a schema-v1 manual payload. Unknown top-level keys and unknown indented keys
/// are skipped (forward compatibility); structural problems and cap violations are
/// errors — the caller degrades to "manual malformed; showing describe".
pub fn parse_manual(payload: &[u8]) -> Result<Manual, ManualError> {
    if payload.len() > MAX_SECTION_BYTES {
        return Err(ManualError::TooLarge {
            bytes: payload.len(),
        });
    }
    let text = core::str::from_utf8(payload).map_err(|_| ManualError::NotUtf8)?;

    let mut name: Option<String> = None;
    let mut synopsis: Option<String> = None;
    let mut description: Vec<String> = Vec::new();
    let mut args: Vec<ManualArg> = Vec::new();
    let mut examples: Vec<ManualExample> = Vec::new();
    let mut see_also: Option<String> = None;
    let mut block = Block::None;
    let mut saw_header = false;
    let mut ended = false;

    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.len() > MAX_LINE_BYTES {
            return Err(ManualError::LineTooLong { line: number });
        }
        if !saw_header {
            // The first line must be the magic + major version.
            let mut tokens = line.split_whitespace();
            if tokens.next() != Some("eo9-manual") {
                return Err(ManualError::BadHeader);
            }
            let major: u32 = tokens
                .next()
                .and_then(|token| token.parse().ok())
                .ok_or(ManualError::BadHeader)?;
            if major != 1 {
                return Err(ManualError::UnsupportedVersion(major));
            }
            saw_header = true;
            continue;
        }

        if line.starts_with(' ') {
            // An indented line: attaches to the open block; unknown keys are skipped.
            let trimmed = line.trim_start();
            match block {
                Block::Description => {
                    description.push(String::from(line.strip_prefix("  ").unwrap_or(trimmed)))
                }
                Block::Arg => {
                    let arg = args.last_mut().expect("Block::Arg implies an open arg");
                    if let Some(doc) = trimmed.strip_prefix("doc: ") {
                        arg.doc.push(String::from(doc));
                    } else if let Some(values) = trimmed.strip_prefix("values: ") {
                        if arg.kind.is_some() {
                            return Err(ManualError::ValuesAndKind {
                                arg: arg.name.clone(),
                            });
                        }
                        arg.values = Some(String::from(values));
                    } else if let Some(kind) = trimmed.strip_prefix("kind: ") {
                        if arg.values.is_some() {
                            return Err(ManualError::ValuesAndKind {
                                arg: arg.name.clone(),
                            });
                        }
                        arg.kind = Some(String::from(kind));
                    }
                }
                Block::Example => {
                    let example = examples
                        .last_mut()
                        .expect("Block::Example implies an open example");
                    if let Some(doc) = trimmed.strip_prefix("doc: ") {
                        example.doc.push(String::from(doc));
                    }
                }
                Block::None => {}
            }
            continue;
        }

        // A top-level line closes whatever block was open.
        block = Block::None;
        if line == "end" {
            ended = true;
            break;
        }
        if line == "eo9-manual" || line.starts_with("eo9-manual ") {
            return Err(ManualError::DuplicateHeader { line: number });
        }
        if let Some(value) = line.strip_prefix("name: ") {
            name = Some(String::from(value));
        } else if let Some(value) = line.strip_prefix("synopsis: ") {
            synopsis = Some(String::from(value));
        } else if line == "description:" {
            block = Block::Description;
        } else if let Some(header) = line.strip_prefix("arg ") {
            if args.len() >= MAX_ARGS {
                return Err(ManualError::TooManyArgs);
            }
            let tokens: Vec<&str> = header.split_whitespace().collect();
            let bad = ManualError::BadArgHeader { line: number };
            if tokens.len() < 3 {
                return Err(bad);
            }
            let required = match tokens[tokens.len() - 1] {
                "required" => true,
                "optional" => false,
                _ => return Err(bad),
            };
            args.push(ManualArg {
                name: String::from(tokens[0]),
                ty: tokens[1..tokens.len() - 1].join(" "),
                required,
                doc: Vec::new(),
                values: None,
                kind: None,
            });
            block = Block::Arg;
        } else if let Some(value) = line.strip_prefix("example: ") {
            if examples.len() >= MAX_EXAMPLES {
                return Err(ManualError::TooManyExamples);
            }
            examples.push(ManualExample {
                line: String::from(value),
                doc: Vec::new(),
            });
            block = Block::Example;
        } else if let Some(value) = line.strip_prefix("see-also: ") {
            see_also = Some(String::from(value));
        }
        // Anything else: an unknown key — skipped for forward compatibility.
    }

    if !saw_header {
        return Err(ManualError::BadHeader);
    }
    if !ended {
        return Err(ManualError::MissingEnd);
    }
    Ok(Manual {
        name: name.ok_or(ManualError::MissingName)?,
        synopsis: synopsis.ok_or(ManualError::MissingSynopsis)?,
        description,
        args,
        examples,
        see_also,
    })
}

// ---------------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------------

/// Strip control bytes from self-reported text (no terminal-escape injection): tabs
/// become a space, every other control character disappears.
fn sanitize(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Wrap sanitized text to the page budget: the first line behind `prefix`, continuation
/// lines behind `indent`. Words longer than a whole line are hard-split.
fn wrap_into(lines: &mut Vec<String>, prefix: &str, indent: &str, text: &str) {
    let text = sanitize(text);
    let mut line = String::from(prefix);
    let mut line_started = false;
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let current = line.chars().count();
            let needed = word.chars().count() + if line_started { 1 } else { 0 };
            if current + needed <= COLUMN_BUDGET {
                if line_started {
                    line.push(' ');
                }
                line.push_str(word);
                line_started = true;
                break;
            }
            if !line_started {
                // The word alone overflows the line: hard-split it at the budget.
                let take = COLUMN_BUDGET.saturating_sub(current).max(1);
                let split = word
                    .char_indices()
                    .nth(take)
                    .map(|(at, _)| at)
                    .unwrap_or(word.len());
                line.push_str(&word[..split]);
                word = &word[split..];
            }
            lines.push(core::mem::take(&mut line));
            line = String::from(indent);
            line_started = false;
            if word.is_empty() {
                break;
            }
        }
    }
    if line.chars().count() > indent.chars().count() || lines.is_empty() {
        lines.push(line);
    }
}

/// The `(T, required|optional)` shape of a WIT `ArgSpec.ty`, for the mismatch check:
/// `option<T>` means optional-T, anything else required-itself.
fn wit_shape(ty: &str) -> (&str, bool) {
    match ty
        .strip_prefix("option<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        Some(inner) => (inner, false),
        None => (ty, true),
    }
}

/// Render a parsed manual for the page. The manual is self-reported and display-only;
/// when the component's WIT argument signature is available (`wit_args`), each
/// documented argument is checked against it and disagreements are flagged — the
/// program's declared signature is the mechanical truth, never the prose.
pub fn render_manual(manual: &Manual, wit_args: Option<&[ArgSpec]>) -> Vec<String> {
    let mut lines = Vec::new();
    wrap_into(
        &mut lines,
        "",
        "  ",
        &format!("{} — {}", manual.name, manual.synopsis),
    );
    for line in &manual.description {
        wrap_into(&mut lines, "", "  ", line);
    }
    if !manual.args.is_empty() {
        lines.push(String::from("args:"));
        for arg in &manual.args {
            let requiredness = if arg.required { "required" } else { "optional" };
            wrap_into(
                &mut lines,
                "  ",
                "      ",
                &format!("--{}: {} ({requiredness})", arg.name, arg.ty),
            );
            for doc in &arg.doc {
                wrap_into(&mut lines, "      ", "      ", doc);
            }
            if let Some(values) = &arg.values {
                wrap_into(
                    &mut lines,
                    "      ",
                    "        ",
                    &format!("values: {values}"),
                );
            }
            if let Some(kind) = &arg.kind {
                wrap_into(&mut lines, "      ", "        ", &format!("kind: {kind}"));
            }
            if let Some(specs) = wit_args {
                match specs.iter().find(|spec| spec.name == arg.name) {
                    None => wrap_into(
                        &mut lines,
                        "      ",
                        "      ",
                        &format!(
                            "(!) the program declares no `--{}` argument — trust `describe`",
                            arg.name
                        ),
                    ),
                    Some(spec) => {
                        let (ty, required) = wit_shape(&spec.ty);
                        if ty != arg.ty || required != arg.required {
                            wrap_into(
                                &mut lines,
                                "      ",
                                "      ",
                                &format!(
                                    "(!) the program declares `{}` — the manual disagrees; \
                                     trust `describe`",
                                    spec.ty
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    if !manual.examples.is_empty() {
        lines.push(String::from("examples:"));
        for example in &manual.examples {
            wrap_into(&mut lines, "  ", "      ", &example.line);
            for doc in &example.doc {
                wrap_into(&mut lines, "      ", "      ", doc);
            }
        }
    }
    if let Some(see_also) = &manual.see_also
        && !see_also.is_empty()
    {
        wrap_into(&mut lines, "", "  ", &format!("see-also: {see_also}"));
    }
    lines
}

/// Test-only byte fixtures: hand-encoded containers, shared with the session tests
/// (the `man` dispatch) so both exercise the same canonical shapes.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::SECTION_NAME;
    use alloc::vec;
    use alloc::vec::Vec;

    pub fn leb(mut value: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    pub fn section(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(leb(payload.len() as u32));
        out.extend_from_slice(payload);
        out
    }

    pub fn custom(name: &str, data: &[u8]) -> Vec<u8> {
        let mut payload = leb(name.len() as u32);
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(data);
        section(0, &payload)
    }

    /// A core module carrying the given sections.
    pub fn module(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\0asm".to_vec();
        out.extend_from_slice(&[1, 0, 0, 0]);
        for s in sections {
            out.extend_from_slice(s);
        }
        out
    }

    /// A component carrying the given sections.
    pub fn component(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\0asm".to_vec();
        out.extend_from_slice(&[0x0d, 0, 1, 0]);
        for s in sections {
            out.extend_from_slice(s);
        }
        out
    }

    /// The canonical shape the build pipeline produces: a component whose depth-1 core
    /// module carries the manual.
    pub fn component_with_manual(text: &str) -> Vec<u8> {
        let inner = module(&[custom("name", b"m"), custom(SECTION_NAME, text.as_bytes())]);
        component(&[custom("component-name", b"c"), section(1, &inner)])
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{component, component_with_manual, custom, module, section};
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    const MINIMAL: &str = "eo9-manual 1\nname: hello\nsynopsis: say hello\nend\n";

    // -- scanner ----------------------------------------------------------------------

    #[test]
    fn finds_the_manual_in_a_depth_1_core_module() {
        let bytes = component_with_manual(MINIMAL);
        assert_eq!(extract_manual(&bytes), Some(MINIMAL.as_bytes()));
    }

    #[test]
    fn finds_an_outer_component_level_manual_first() {
        // The design's (unused-for-now) fallback location: an OUTER custom section.
        // First hit wins: outer beats the core module's.
        let inner = module(&[custom(SECTION_NAME, b"inner")]);
        let bytes = component(&[custom(SECTION_NAME, b"outer"), section(1, &inner)]);
        assert_eq!(extract_manual(&bytes), Some(&b"outer"[..]));
    }

    #[test]
    fn finds_a_manual_in_a_bare_core_module() {
        let bytes = module(&[custom("other", b"x"), custom(SECTION_NAME, b"m")]);
        assert_eq!(extract_manual(&bytes), Some(&b"m"[..]));
    }

    #[test]
    fn does_not_descend_into_nested_components() {
        // A saved composition nests its operands as component sections (id 4); their
        // manuals are deliberately out of reach — the fused artifact answers with
        // describe, not one part's prose.
        let operand = component_with_manual(MINIMAL);
        let fused = component(&[section(4, &operand)]);
        assert_eq!(extract_manual(&fused), None);
    }

    #[test]
    fn module_sections_of_a_module_are_not_modules() {
        // Only a COMPONENT's id-1 sections are core modules (in a core module id 1 is
        // the type section); the scanner must not treat one as a nested container.
        let inner = module(&[custom(SECTION_NAME, b"m")]);
        let bytes = module(&[section(1, &inner)]);
        assert_eq!(extract_manual(&bytes), None);
    }

    #[test]
    fn absent_garbage_and_truncated_bytes_degrade_to_none() {
        assert_eq!(extract_manual(b""), None);
        assert_eq!(extract_manual(b"not wasm at all"), None);
        assert_eq!(extract_manual(&component(&[custom("other", b"x")])), None);
        // A section whose declared size runs past the end of the bytes.
        let mut truncated = component(&[]);
        truncated.extend_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert_eq!(extract_manual(&truncated), None);
        // A leb that never terminates.
        let mut runaway = component(&[]);
        runaway.extend_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(extract_manual(&runaway), None);
        // Cut a valid container mid-section.
        let whole = component_with_manual(MINIMAL);
        assert_eq!(extract_manual(&whole[..whole.len() - 10]), None);
    }

    // -- parser -----------------------------------------------------------------------

    /// The full canonical text the macro emits for the design doc's example.
    const FULL: &str = "eo9-manual 1\n\
        name: telnetd\n\
        synopsis: serve eosh sessions over telnet, one fused task per session\n\
        description:\n\
        \x20 Composes net.virtio $ net.l4.over-l2 $ net.text $ eosh, compiles it once,\n\
        \x20 and serves sessions sequentially.\n\
        arg port u16 optional\n\
        \x20 doc: TCP port to listen on (default 23)\n\
        arg nic string optional\n\
        \x20 doc: the NIC provider to compose at the bottom of the stack\n\
        \x20 kind: component-name\n\
        arg address string optional\n\
        \x20 doc: IPv4 acquisition mode\n\
        \x20 values: dhcp, static\n\
        example: telnetd --port 2323\n\
        \x20 doc: serve on a non-privileged port under QEMU user networking\n\
        see-also: net.l4.over-l2, net.text, eosh\n\
        end\n";

    #[test]
    fn parses_the_design_docs_example() {
        let manual = parse_manual(FULL.as_bytes()).expect("parses");
        assert_eq!(manual.name, "telnetd");
        assert_eq!(
            manual.synopsis,
            "serve eosh sessions over telnet, one fused task per session"
        );
        assert_eq!(manual.description.len(), 2);
        assert_eq!(manual.args.len(), 3);
        assert_eq!(manual.args[0].name, "port");
        assert_eq!(manual.args[0].ty, "u16");
        assert!(!manual.args[0].required);
        assert_eq!(
            manual.args[0].doc,
            vec!["TCP port to listen on (default 23)"]
        );
        assert_eq!(manual.args[1].kind.as_deref(), Some("component-name"));
        assert_eq!(manual.args[2].values.as_deref(), Some("dhcp, static"));
        assert_eq!(manual.examples.len(), 1);
        assert_eq!(manual.examples[0].line, "telnetd --port 2323");
        assert_eq!(
            manual.see_also.as_deref(),
            Some("net.l4.over-l2, net.text, eosh")
        );
    }

    #[test]
    fn multi_word_type_text_parses() {
        let text = "eo9-manual 1\nname: x\nsynopsis: y\n\
                    arg pair tuple<u16, bool> required\n  doc: a pair\nend\n";
        let manual = parse_manual(text.as_bytes()).expect("parses");
        assert_eq!(manual.args[0].ty, "tuple<u16, bool>");
        assert!(manual.args[0].required);
    }

    #[test]
    fn unknown_keys_are_skipped_for_forward_compatibility() {
        let text = "eo9-manual 1\nname: x\nsynopsis: y\n\
                    color: blue\n\
                    arg a u8 required\n  doc: d\n  flavor: spicy\n\
                    locale:\n  fr: bonjour\n\
                    end\n";
        let manual = parse_manual(text.as_bytes()).expect("parses");
        assert_eq!(manual.args.len(), 1);
        assert_eq!(manual.args[0].doc, vec!["d"]);
    }

    #[test]
    fn version_discipline() {
        assert_eq!(
            parse_manual(b"eo9-manual 2\nname: x\nsynopsis: y\nend\n"),
            Err(ManualError::UnsupportedVersion(2))
        );
        assert_eq!(parse_manual(b"banana\nend\n"), Err(ManualError::BadHeader));
        assert_eq!(parse_manual(b""), Err(ManualError::BadHeader));
        assert_eq!(
            parse_manual(b"eo9-manual one\nend\n"),
            Err(ManualError::BadHeader)
        );
    }

    #[test]
    fn structural_problems_are_errors() {
        // Truncated: no end.
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: x\nsynopsis: y\n"),
            Err(ManualError::MissingEnd)
        );
        // Missing required fields.
        assert_eq!(
            parse_manual(b"eo9-manual 1\nsynopsis: y\nend\n"),
            Err(ManualError::MissingName)
        );
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: x\nend\n"),
            Err(ManualError::MissingSynopsis)
        );
        // Two concatenated manuals (the lld concat defense).
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: x\neo9-manual 1\nname: y\nend\n"),
            Err(ManualError::DuplicateHeader { line: 3 })
        );
        // A bad arg header.
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: x\nsynopsis: y\narg port\nend\n"),
            Err(ManualError::BadArgHeader { line: 4 })
        );
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: x\nsynopsis: y\narg port u16 maybe\nend\n"),
            Err(ManualError::BadArgHeader { line: 4 })
        );
        // values: and kind: on one arg.
        assert_eq!(
            parse_manual(
                b"eo9-manual 1\nname: x\nsynopsis: y\n\
                  arg a u8 required\n  values: 1\n  kind: port\nend\n"
            ),
            Err(ManualError::ValuesAndKind {
                arg: "a".to_string()
            })
        );
    }

    #[test]
    fn caps_are_enforced() {
        // Oversize section.
        let mut big = String::from("eo9-manual 1\nname: x\nsynopsis: y\n");
        while big.len() <= MAX_SECTION_BYTES {
            big.push_str("filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
        }
        big.push_str("end\n");
        assert!(matches!(
            parse_manual(big.as_bytes()),
            Err(ManualError::TooLarge { .. })
        ));
        // An overlong line.
        let long = format!(
            "eo9-manual 1\nname: x\nsynopsis: {}\nend\n",
            "s".repeat(130)
        );
        assert_eq!(
            parse_manual(long.as_bytes()),
            Err(ManualError::LineTooLong { line: 3 })
        );
        // Too many args.
        let mut many = String::from("eo9-manual 1\nname: x\nsynopsis: y\n");
        for index in 0..(MAX_ARGS + 1) {
            many.push_str(&format!("arg a{index} u8 required\n  doc: d\n"));
        }
        many.push_str("end\n");
        assert_eq!(parse_manual(many.as_bytes()), Err(ManualError::TooManyArgs));
        // Too many examples.
        let mut many = String::from("eo9-manual 1\nname: x\nsynopsis: y\n");
        for index in 0..(MAX_EXAMPLES + 1) {
            many.push_str(&format!("example: x --n {index}\n"));
        }
        many.push_str("end\n");
        assert_eq!(
            parse_manual(many.as_bytes()),
            Err(ManualError::TooManyExamples)
        );
        // Exactly at the caps is fine.
        let mut at_cap = String::from("eo9-manual 1\nname: x\nsynopsis: y\n");
        for index in 0..MAX_ARGS {
            at_cap.push_str(&format!("arg a{index} u8 required\n"));
        }
        for index in 0..MAX_EXAMPLES {
            at_cap.push_str(&format!("example: x --n {index}\n"));
        }
        at_cap.push_str("end\n");
        assert!(parse_manual(at_cap.as_bytes()).is_ok());
    }

    #[test]
    fn not_utf8_is_an_error() {
        assert_eq!(
            parse_manual(b"eo9-manual 1\nname: \xff\xfe\nend\n"),
            Err(ManualError::NotUtf8)
        );
    }

    #[test]
    fn text_after_end_is_ignored() {
        // lld concatenation where the first manual is complete: the first wins.
        let text = format!("{MINIMAL}eo9-manual 1\nname: other\nsynopsis: z\nend\n");
        let manual = parse_manual(text.as_bytes()).expect("parses");
        assert_eq!(manual.name, "hello");
    }

    // -- renderer ---------------------------------------------------------------------

    fn spec(name: &str, ty: &str) -> ArgSpec {
        ArgSpec {
            name: name.to_string(),
            ty: ty.to_string(),
        }
    }

    #[test]
    fn renders_the_full_manual_within_budget() {
        let manual = parse_manual(FULL.as_bytes()).expect("parses");
        let specs = vec![
            spec("port", "option<u16>"),
            spec("nic", "option<string>"),
            spec("address", "option<string>"),
        ];
        let lines = render_manual(&manual, Some(&specs));
        let text = lines.join("\n");
        assert!(text.starts_with("telnetd — serve eosh sessions over telnet"));
        assert!(text.contains("args:"));
        assert!(text.contains("--port: u16 (optional)"));
        assert!(text.contains("TCP port to listen on (default 23)"));
        assert!(text.contains("values: dhcp, static"));
        assert!(text.contains("kind: component-name"));
        assert!(text.contains("examples:"));
        assert!(text.contains("telnetd --port 2323"));
        assert!(text.contains("see-also: net.l4.over-l2, net.text, eosh"));
        // The signature matches, so nothing is flagged.
        assert!(!text.contains("(!)"), "no flags expected: {text}");
        for line in &lines {
            assert!(
                line.chars().count() <= COLUMN_BUDGET,
                "line exceeds the {COLUMN_BUDGET}-column budget: {line:?}"
            );
        }
    }

    #[test]
    fn wit_disagreements_are_flagged_not_trusted() {
        let manual = parse_manual(FULL.as_bytes()).expect("parses");
        // The program declares port as required u32 and no `nic` at all.
        let specs = vec![spec("port", "u32"), spec("address", "option<string>")];
        let lines = render_manual(&manual, Some(&specs));
        let text = lines.join("\n");
        assert!(
            text.contains("(!) the program declares `u32`"),
            "type/optionality mismatch flagged: {text}"
        );
        assert!(
            text.contains("(!) the program declares no `--nic` argument"),
            "unknown argument flagged: {text}"
        );
        assert!(text.contains("trust `describe`"));
        // Without a signature nothing can be checked, and nothing is flagged.
        let unchecked = render_manual(&manual, None).join("\n");
        assert!(!unchecked.contains("(!)"));
    }

    #[test]
    fn control_bytes_are_stripped_and_long_lines_wrapped() {
        let text = format!(
            "eo9-manual 1\nname: evil\nsynopsis: \x1b[31mred\x07 alert\ttab\n\
             description:\n  {}\nend\n",
            "w ".repeat(58).trim_end()
        );
        let manual = parse_manual(text.as_bytes()).expect("parses");
        let lines = render_manual(&manual, None);
        let rendered = lines.join("\n");
        assert!(
            !rendered.contains('\x1b') && !rendered.contains('\x07'),
            "escape bytes stripped: {rendered:?}"
        );
        assert!(rendered.contains("[31mred alert tab"), "got: {rendered:?}");
        // The 116-char description line wrapped into two, both within budget.
        for line in &lines {
            assert!(
                line.chars().count() <= COLUMN_BUDGET,
                "line exceeds budget: {line:?}"
            );
        }
        assert!(
            lines.iter().filter(|l| l.contains("w w")).count() >= 2,
            "the long description wrapped: {lines:?}"
        );
    }

    #[test]
    fn unbreakable_words_are_hard_split() {
        let word = "x".repeat(250);
        let text = format!("eo9-manual 1\nname: n\nsynopsis: {word} done\nend\n");
        // 250 bytes exceeds the 120-byte line cap, so build the manual directly.
        let manual = Manual {
            name: String::from("n"),
            synopsis: format!("{word} done"),
            description: Vec::new(),
            args: Vec::new(),
            examples: Vec::new(),
            see_also: None,
        };
        let _ = text;
        let lines = render_manual(&manual, None);
        for line in &lines {
            assert!(line.chars().count() <= COLUMN_BUDGET, "line: {line:?}");
        }
        let kept = lines.join("").chars().filter(|c| *c == 'x').count();
        assert_eq!(kept, 250, "no characters lost across the hard split");
    }

    #[test]
    fn end_to_end_extract_parse_render() {
        let bytes = component_with_manual(FULL);
        let payload = extract_manual(&bytes).expect("found");
        let manual = parse_manual(payload).expect("parses");
        assert_eq!(manual.name, "telnetd");
    }
}
