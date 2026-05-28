//! WAVE argument and outcome handling.
//!
//! The canonical value encoding for invocations and outcomes is WAVE (SPEC "Arguments and
//! outcomes"): `spawn` takes `main`'s named arguments as WAVE text and type-checks them
//! against the signature extracted from the component; the program's
//! `result<program-success, program-failure>` is rendered back as WAVE text plus WIT type
//! text so the outcome can outlive the component.
//!
//! The WAVE implementation used is the one re-exported by wasmtime's `wave` feature, which
//! implements the WAVE traits directly for `wasmtime::component::{Val, Type}` (see
//! plan/04-runtime.md § Decisions for the version note).

use wasmtime::component::types::ComponentFunc;
use wasmtime::component::wasm_wave::wasm::DisplayType;
use wasmtime::component::{Type, Val, wasm_wave};

use crate::outcome::{Outcome, WaveValue};

/// One named `main` argument, WAVE-encoded (`eo9:exec/task.named-arg`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedArg {
    pub name: String,
    pub value: String,
}

impl NamedArg {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// Parse WAVE-encoded named arguments against `main`'s signature, producing the positional
/// `Val` list `main` is called with.
///
/// Every declared parameter must be supplied exactly once and every supplied argument must
/// name a declared parameter; anything else is a `bad-arguments` spawn error. The one
/// exception is a **final `list<string>` parameter**, which is the variadic tail by
/// convention (`cat a.txt b.txt`): when nothing supplies it, it defaults to the empty list
/// instead of being a missing-argument error.
pub(crate) fn parse_args(signature: &ComponentFunc, args: &[NamedArg]) -> Result<Vec<Val>, String> {
    let params: Vec<(String, Type)> = signature
        .params()
        .map(|(name, ty)| (name.to_string(), ty))
        .collect();

    for arg in args {
        if !params.iter().any(|(name, _)| *name == arg.name) {
            return Err(format!("unknown argument `{}`", arg.name));
        }
    }

    let mut vals = Vec::with_capacity(params.len());
    for (index, (name, ty)) in params.iter().enumerate() {
        let matching: Vec<&NamedArg> = args.iter().filter(|arg| arg.name == *name).collect();
        let arg = match matching.as_slice() {
            [] => {
                if index == params.len() - 1 && is_string_list(ty) {
                    vals.push(Val::List(Vec::new()));
                    continue;
                }
                return Err(format!("missing argument `{name}`"));
            }
            [arg] => *arg,
            _ => return Err(format!("argument `{name}` supplied more than once")),
        };
        let val: Val = wasm_wave::from_str(ty, &arg.value).map_err(|err| {
            format!(
                "argument `{name}` is not a valid `{}`: {err}",
                DisplayType(ty)
            )
        })?;
        vals.push(val);
    }
    Ok(vals)
}

/// Is this a `list<string>` type — the shape the variadic-tail convention applies to?
fn is_string_list(ty: &Type) -> bool {
    matches!(ty, Type::List(list) if matches!(list.ty(), Type::String))
}

/// Render a completed `main` return value as a [`Outcome`].
///
/// `main` returns `result<program-success, program-failure>`; the payload of whichever arm
/// was taken is rendered as WAVE text together with its WIT type text. A payload-less arm
/// renders as an empty value with empty type text. A non-`result` return value (not
/// expected from a well-formed Eo9 binary, but handled for robustness) is rendered as a
/// success value.
pub(crate) fn render_outcome(result_ty: &Type, val: &Val) -> Outcome {
    match (result_ty, val) {
        (Type::Result(result_ty), Val::Result(result_val)) => match result_val {
            Ok(payload) => Outcome::Success(render_payload(result_ty.ok(), payload.as_deref())),
            Err(payload) => Outcome::Failure(render_payload(result_ty.err(), payload.as_deref())),
        },
        _ => Outcome::Success(render_value(result_ty, val)),
    }
}

fn render_payload(ty: Option<Type>, val: Option<&Val>) -> WaveValue {
    match (ty, val) {
        (Some(ty), Some(val)) => render_value(&ty, val),
        _ => WaveValue {
            ty: String::new(),
            value: String::new(),
        },
    }
}

fn render_value(ty: &Type, val: &Val) -> WaveValue {
    let value =
        wasm_wave::to_string(val).unwrap_or_else(|err| format!("<unrenderable value: {err}>"));
    WaveValue {
        ty: DisplayType(ty).to_string(),
        value,
    }
}
