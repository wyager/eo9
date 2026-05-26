//! A small, kernel-side WAVE codec for arguments and outcomes.
//!
//! The canonical value encoding for invocations and outcomes is WAVE (SPEC "Arguments and
//! outcomes"). The usermode runtime uses the full `wasm-wave` implementation re-exported
//! by wasmtime's `wave` feature, which needs `std`; on the kernel the surface actually
//! exercised today is much smaller — eosh encodes scalar flags (quoted strings, bools,
//! integers, floats, chars, `none`/`some(…)` options, bare enum cases) and the example
//! programs return scalar-or-variant payloads — so this module hand-rolls exactly that:
//!
//! * [`parse`]: WAVE text → `Val`, directed by the parameter's [`Type`] (used by `spawn`
//!   to bind `main`'s arguments). Types outside the supported set are rejected with a
//!   clear message rather than guessed at.
//! * [`render`]: `Val` → WAVE text, structural and total (used to render outcomes).
//! * [`type_text`]: a WIT-ish rendering of a [`Type`] for `wave-value.ty` (informational).

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use wasmtime::component::{Type, Val};

/// Parse one WAVE-encoded value of type `ty`.
pub fn parse(ty: &Type, text: &str) -> Result<Val, String> {
    let text = text.trim();
    Ok(match ty {
        Type::Bool => match text {
            "true" => Val::Bool(true),
            "false" => Val::Bool(false),
            _ => return Err("expected `true` or `false`".to_owned()),
        },
        Type::U8 => Val::U8(int(text)?),
        Type::U16 => Val::U16(int(text)?),
        Type::U32 => Val::U32(int(text)?),
        Type::U64 => Val::U64(int(text)?),
        Type::S8 => Val::S8(int(text)?),
        Type::S16 => Val::S16(int(text)?),
        Type::S32 => Val::S32(int(text)?),
        Type::S64 => Val::S64(int(text)?),
        Type::Float32 => Val::Float32(int(text)?),
        Type::Float64 => Val::Float64(int(text)?),
        Type::Char => parse_char(text)?,
        Type::String => Val::String(parse_string(text)?),
        Type::Enum(cases) => {
            if cases.names().any(|case| case == text) {
                Val::Enum(text.to_owned())
            } else {
                return Err(format!("`{text}` is not a case of the expected enum"));
            }
        }
        Type::Option(option) => {
            if text == "none" {
                Val::Option(None)
            } else if let Some(inner) = text.strip_prefix("some(").and_then(|t| t.strip_suffix(')'))
            {
                Val::Option(Some(Box::new(parse(&option.ty(), inner)?)))
            } else {
                // WAVE allows the inner value to stand for `some(value)` everywhere except
                // the bare word `none`; eosh relies on the explicit forms only, but accept
                // the shorthand for hand-typed kernel command lines.
                Val::Option(Some(Box::new(parse(&option.ty(), text)?)))
            }
        }
        other => {
            return Err(format!(
                "the kernel only parses scalar, enum, and option arguments so far \
                 (got a {} parameter)",
                type_text(other)
            ));
        }
    })
}

fn int<T: core::str::FromStr>(text: &str) -> Result<T, String>
where
    T::Err: core::fmt::Display,
{
    text.parse::<T>().map_err(|err| err.to_string())
}

fn parse_char(text: &str) -> Result<Val, String> {
    let inner = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .unwrap_or(text);
    let unescaped = unescape(inner)?;
    let mut chars = unescaped.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Ok(Val::Char(ch)),
        _ => Err("expected exactly one character".to_owned()),
    }
}

fn parse_string(text: &str) -> Result<String, String> {
    match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(inner) => unescape(inner),
        None => Err("expected a double-quoted WAVE string".to_owned()),
    }
}

fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\'') => out.push('\''),
            Some(other) => return Err(format!("unsupported escape `\\{other}`")),
            None => return Err("dangling `\\` at the end of the value".to_owned()),
        }
    }
    Ok(out)
}

/// Render a value as WAVE text (structural, total — anything renders to something).
pub fn render(val: &Val) -> String {
    match val {
        Val::Bool(v) => v.to_string(),
        Val::U8(v) => v.to_string(),
        Val::U16(v) => v.to_string(),
        Val::U32(v) => v.to_string(),
        Val::U64(v) => v.to_string(),
        Val::S8(v) => v.to_string(),
        Val::S16(v) => v.to_string(),
        Val::S32(v) => v.to_string(),
        Val::S64(v) => v.to_string(),
        Val::Float32(v) => v.to_string(),
        Val::Float64(v) => v.to_string(),
        Val::Char(v) => format!("'{}'", escape_char(*v)),
        Val::String(v) => quote(v),
        Val::Enum(case) => case.clone(),
        Val::Variant(case, payload) => match payload {
            None => case.clone(),
            Some(inner) => format!("{case}({})", render(inner)),
        },
        Val::Option(None) => "none".to_owned(),
        Val::Option(Some(inner)) => format!("some({})", render(inner)),
        Val::Result(Ok(payload)) => match payload {
            None => "ok".to_owned(),
            Some(inner) => format!("ok({})", render(inner)),
        },
        Val::Result(Err(payload)) => match payload {
            None => "err".to_owned(),
            Some(inner) => format!("err({})", render(inner)),
        },
        Val::List(items) => {
            let rendered: Vec<String> = items.iter().map(render).collect();
            format!("[{}]", rendered.join(", "))
        }
        Val::Tuple(items) => {
            let rendered: Vec<String> = items.iter().map(render).collect();
            format!("({})", rendered.join(", "))
        }
        Val::Record(fields) => {
            let rendered: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", render(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
        Val::Flags(names) => format!("{{{}}}", names.join(", ")),
        other => format!("{other:?}"),
    }
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn escape_char(ch: char) -> String {
    match ch {
        '\'' => "\\'".to_owned(),
        '\\' => "\\\\".to_owned(),
        '\n' => "\\n".to_owned(),
        '\t' => "\\t".to_owned(),
        '\r' => "\\r".to_owned(),
        other => other.to_string(),
    }
}

/// A WIT-ish rendering of a type, for the informational `wave-value.ty` field.
pub fn type_text(ty: &Type) -> String {
    match ty {
        Type::Bool => "bool".to_owned(),
        Type::U8 => "u8".to_owned(),
        Type::U16 => "u16".to_owned(),
        Type::U32 => "u32".to_owned(),
        Type::U64 => "u64".to_owned(),
        Type::S8 => "s8".to_owned(),
        Type::S16 => "s16".to_owned(),
        Type::S32 => "s32".to_owned(),
        Type::S64 => "s64".to_owned(),
        Type::Float32 => "f32".to_owned(),
        Type::Float64 => "f64".to_owned(),
        Type::Char => "char".to_owned(),
        Type::String => "string".to_owned(),
        Type::List(list) => format!("list<{}>", type_text(&list.ty())),
        Type::Option(option) => format!("option<{}>", type_text(&option.ty())),
        Type::Result(result) => {
            let ok = result.ok().map(|ty| type_text(&ty));
            let err = result.err().map(|ty| type_text(&ty));
            match (ok, err) {
                (Some(ok), Some(err)) => format!("result<{ok}, {err}>"),
                (Some(ok), None) => format!("result<{ok}>"),
                (None, Some(err)) => format!("result<_, {err}>"),
                (None, None) => "result".to_owned(),
            }
        }
        Type::Tuple(tuple) => {
            let parts: Vec<String> = tuple.types().map(|ty| type_text(&ty)).collect();
            format!("tuple<{}>", parts.join(", "))
        }
        Type::Enum(cases) => {
            let names: Vec<&str> = cases.names().collect();
            format!("enum {{ {} }}", names.join(", "))
        }
        Type::Variant(variant) => {
            let cases: Vec<String> = variant
                .cases()
                .map(|case| match case.ty {
                    Some(ty) => format!("{}({})", case.name, type_text(&ty)),
                    None => case.name.to_owned(),
                })
                .collect();
            format!("variant {{ {} }}", cases.join(", "))
        }
        Type::Record(record) => {
            let fields: Vec<String> = record
                .fields()
                .map(|field| format!("{}: {}", field.name, type_text(&field.ty)))
                .collect();
            format!("record {{ {} }}", fields.join(", "))
        }
        Type::Flags(flags) => {
            let names: Vec<&str> = flags.names().collect();
            format!("flags {{ {} }}", names.join(", "))
        }
        other => format!("{other:?}"),
    }
}
