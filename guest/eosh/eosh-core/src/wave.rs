//! Type-directed argument encoding: shell tokens to WAVE value text.
//!
//! The canonical value encoding for arguments and outcomes is WAVE (SPEC.md,
//! "Arguments and outcomes"); `spawn` takes `main`'s arguments as WAVE text and the
//! host checks them against `main`'s signature. The shell's job is therefore not to
//! *validate* values but to get honest WAVE text out of what the user typed, directed
//! by the declared parameter type (`arg-spec.ty`):
//!
//! * `string` parameters: the token's text is the string's contents — the shell quotes
//!   and escapes it (`--name world` → `"world"`), so users never write WAVE string
//!   syntax by hand;
//! * `option<T>` parameters: the bare word `none` is `none`; anything else (and any
//!   quoted token) encodes as `some(…)` of the inner type; an omitted optional
//!   parameter is filled with `none` by the evaluator;
//! * everything else (`bool`, integers, floats, `char`, enums, records, …): the token
//!   is passed through verbatim as WAVE text, so `--verbose true`, `--retries 3`, and
//!   `--mode fast` all mean what they say, and structured values can be quoted when
//!   they need spaces or structural characters.
//!
//! Whether a parameter takes a *program expression* instead of text at all is the
//! other half of type direction ([`is_component_type`]), used by the evaluator.

use alloc::string::String;

use crate::ast::ArgValue;

/// Is this WIT type text a `component`-typed parameter (a program expression slot)?
///
/// Component-typed parameters are declared with the component algebra's `component`
/// resource, so the type text is `component`, `borrow<component>`, or a package-
/// qualified spelling of the same.
pub fn is_component_type(ty: &str) -> bool {
    let ty = ty.trim();
    let inner = ty
        .strip_prefix("borrow<")
        .or_else(|| ty.strip_prefix("own<"))
        .and_then(|rest| rest.strip_suffix(">"))
        .unwrap_or(ty);
    inner == "component" || inner.ends_with("/component") || inner.ends_with(".component")
}

/// Encode the textual token `text` as WAVE for a parameter of type `ty`.
///
/// `bare` is true when the token was an unquoted word (so `none` can mean the absent
/// option), false when it was a quoted string (always literal contents).
pub fn encode_text(ty: &str, text: &str, bare: bool) -> String {
    let ty = ty.trim();
    if let Some(inner) = option_inner(ty) {
        if bare && text == "none" {
            return String::from("none");
        }
        let mut out = String::from("some(");
        out.push_str(&encode_text(inner, text, bare));
        out.push(')');
        return out;
    }
    if ty == "string" {
        return quote_string(text);
    }
    String::from(text)
}

/// Encode a parsed argument value for a data-typed (non-component) parameter.
pub fn encode_arg_value(ty: &str, value: &ArgValue) -> Option<String> {
    match value {
        ArgValue::Word(text) => Some(encode_text(ty, text, true)),
        ArgValue::Quoted(text) => Some(encode_text(ty, text, false)),
        ArgValue::Expr(_) => None,
    }
}

/// The WAVE text for an omitted `option<…>` parameter.
pub fn none_value() -> String {
    String::from("none")
}

/// Is `ty` an `option<…>` type? Returns the inner type text if so.
pub fn option_inner(ty: &str) -> Option<&str> {
    ty.trim()
        .strip_prefix("option<")
        .and_then(|rest| rest.strip_suffix(">"))
        .map(str::trim)
}

/// Quote `text` as a WAVE string literal.
pub fn quote_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::string::ToString;

    use crate::ast::Expr;

    #[test]
    fn component_types_are_recognised() {
        assert!(is_component_type("component"));
        assert!(is_component_type("borrow<component>"));
        assert!(is_component_type("own<component>"));
        assert!(is_component_type("eo9:exec/component-algebra.component"));
        assert!(!is_component_type("string"));
        assert!(!is_component_type("list<u8>"));
        assert!(!is_component_type("component-info"));
    }

    #[test]
    fn strings_are_quoted_and_escaped() {
        assert_eq!(encode_text("string", "world", true), "\"world\"");
        assert_eq!(
            encode_text("string", "he said \"hi\"\\", false),
            "\"he said \\\"hi\\\"\\\\\""
        );
        assert_eq!(
            encode_text("string", "line\nbreak", false),
            "\"line\\nbreak\""
        );
    }

    #[test]
    fn scalars_pass_through_verbatim() {
        assert_eq!(encode_text("bool", "true", true), "true");
        assert_eq!(encode_text("u32", "64", true), "64");
        assert_eq!(encode_text("s64", "-5", true), "-5");
        assert_eq!(encode_text("f64", "2.5", true), "2.5");
        // Enum/variant/record values are the user's own WAVE text.
        assert_eq!(encode_text("mode", "fast", true), "fast");
        assert_eq!(encode_text("point", "{x: 1, y: 2}", false), "{x: 1, y: 2}");
    }

    #[test]
    fn options_wrap_in_some_and_bare_none_is_none() {
        assert_eq!(
            encode_text("option<string>", "world", true),
            "some(\"world\")"
        );
        assert_eq!(encode_text("option<u32>", "3", true), "some(3)");
        assert_eq!(encode_text("option<string>", "none", true), "none");
        // A quoted "none" is the three-letter string, not the absent option.
        assert_eq!(
            encode_text("option<string>", "none", false),
            "some(\"none\")"
        );
        assert_eq!(none_value(), "none");
    }

    #[test]
    fn arg_values_encode_by_surface_form() {
        assert_eq!(
            encode_arg_value("string", &ArgValue::Word("world".to_string())),
            Some("\"world\"".to_string())
        );
        assert_eq!(
            encode_arg_value("string", &ArgValue::Quoted("two words".to_string())),
            Some("\"two words\"".to_string())
        );
        assert_eq!(
            encode_arg_value("string", &ArgValue::Expr(Box::new(Expr::name("x")))),
            None
        );
    }
}
