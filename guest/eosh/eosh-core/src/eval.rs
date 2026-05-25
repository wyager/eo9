//! The evaluator: [`Expr`]s to component values (plus bound `main` arguments), via the
//! [`Backend`].
//!
//! Evaluation is exactly the spec's reading of the operators: names resolve to open
//! components (a `let` binding shadows resolution), `$` is `compose`, `&` is `extend`,
//! `only` is `restrict`, `rename` is `rename`, and `with p as n` is "rename `p`'s
//! single export slot to `n`, then compose". Argument application is type-directed by
//! the callee's `describe`d signature: data-typed parameters take WAVE text
//! ([`crate::wave`]), component-typed parameters take program expressions. Naming or
//! composing a program never runs it — running is the session's top-level rule
//! ([`crate::session`]), not the evaluator's.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::future::Future;
use core::pin::Pin;

use crate::ast::{Arg, ArgValue, Expr, WithBinding};
use crate::backend::{ArgSpec, Backend, BackendError, ComponentKind, InterfaceRef, NamedArg};
use crate::wave;

/// The result of evaluating an expression: an open component value, plus any `main`
/// arguments that were applied to the (binary) consumer along the way. Arguments are
/// invocation data bound at run time, so they ride alongside the component until the
/// top level spawns it; they never affect composition itself.
#[derive(Debug)]
pub struct EvalOutput<C> {
    pub component: C,
    pub args: Vec<NamedArg>,
}

/// An evaluation error, rendered for the user by its `Display` impl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// The backend reported an error (resolution, algebra, compile, spawn, …).
    Backend(BackendError),
    /// A flag that the callee's signature does not declare.
    UnknownFlag { name: String },
    /// The same parameter was given twice.
    DuplicateArgument { name: String },
    /// More positional arguments than unfilled parameters.
    TooManyPositional,
    /// A required parameter was not provided (checked at the top level).
    MissingArgument { name: String, ty: String },
    /// A data-typed parameter was given a parenthesized program expression.
    ExpressionForDataParameter { name: String, ty: String },
    /// A component-typed parameter: understood, but not yet supported end to end.
    ComponentArgumentUnsupported { name: String },
    /// Arguments were applied to a provider (configure-at-compose-time is not yet
    /// supported by the component algebra).
    ProviderArguments,
    /// Arguments are not allowed in this position (e.g. on an `&` operand).
    ArgumentsNotAllowed { context: &'static str },
    /// An `only` entry that is a bare world name (policy worlds are not resolvable yet).
    NamedWorldUnsupported { name: String },
    /// A `with` provider that does not export exactly one interface.
    WithProviderExports { slot: String, exports: usize },
    /// The top level was given a provider; providers are composed, not run.
    TopLevelProvider,
    /// A name is neither a `let` binding nor resolvable — carried as a backend error
    /// in practice; kept for completeness of rendering.
    UnknownName { name: String },
}

impl From<BackendError> for EvalError {
    fn from(err: BackendError) -> Self {
        EvalError::Backend(err)
    }
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::Backend(err) => write!(f, "{err}"),
            EvalError::UnknownFlag { name } => {
                write!(
                    f,
                    "unknown flag `--{name}`: the program declares no such parameter"
                )
            }
            EvalError::DuplicateArgument { name } => {
                write!(f, "parameter `{name}` was given more than once")
            }
            EvalError::TooManyPositional => {
                write!(
                    f,
                    "more positional arguments than the program has parameters left to fill"
                )
            }
            EvalError::MissingArgument { name, ty } => {
                write!(f, "missing argument `--{name}` (a {ty})")
            }
            EvalError::ExpressionForDataParameter { name, ty } => write!(
                f,
                "parameter `{name}` is {ty}-typed and takes literal text, not a program expression"
            ),
            EvalError::ComponentArgumentUnsupported { name } => write!(
                f,
                "parameter `{name}` is component-typed; passing programs as arguments is not \
                 supported yet (the task API takes only WAVE-encoded data arguments)"
            ),
            EvalError::ProviderArguments => write!(
                f,
                "configure arguments on providers are not supported yet: the component algebra \
                 has no compose-time configure binding"
            ),
            EvalError::ArgumentsNotAllowed { context } => {
                write!(f, "arguments cannot be applied to {context}")
            }
            EvalError::NamedWorldUnsupported { name } => write!(
                f,
                "`only {name}`: named policy worlds are not supported yet; list the interfaces \
                 (e.g. `only eo9:fs,eo9:time`)"
            ),
            EvalError::WithProviderExports { slot, exports } => write!(
                f,
                "`with … as {slot}`: the provider must export exactly one interface (it exports \
                 {exports}); use `rename` explicitly instead"
            ),
            EvalError::TopLevelProvider => write!(
                f,
                "this is a provider, not a binary: providers are composed (`$`, `&`) or bound \
                 (`let`), never run directly"
            ),
            EvalError::UnknownName { name } => write!(f, "unknown name `{name}`"),
        }
    }
}

/// The boxed future produced by recursive [`Evaluator::eval`] calls (recursion in an
/// `async fn` needs one level of boxing).
type EvalFuture<'s, C> = Pin<Box<dyn Future<Output = Result<EvalOutput<C>, EvalError>> + 's>>;

/// The evaluator: a backend plus the session's `let` bindings.
pub struct Evaluator<'a, B: Backend> {
    pub backend: &'a mut B,
    pub bindings: &'a BTreeMap<String, B::Component>,
}

impl<'a, B: Backend> Evaluator<'a, B> {
    pub fn new(backend: &'a mut B, bindings: &'a BTreeMap<String, B::Component>) -> Self {
        Evaluator { backend, bindings }
    }

    /// Evaluate `expr` to a component value (plus applied `main` arguments).
    pub async fn eval(&mut self, expr: &Expr) -> Result<EvalOutput<B::Component>, EvalError> {
        match expr {
            Expr::Name(name) => {
                let component = self.lookup(name).await?;
                Ok(EvalOutput {
                    component,
                    args: Vec::new(),
                })
            }
            Expr::App { callee, args } => self.eval_app(callee, args).await,
            Expr::Compose { provider, consumer } => {
                let provider = self
                    .eval_plain(provider, "the provider side of `$`")
                    .await?;
                let consumer = self.eval_boxed(consumer).await?;
                let component = self.backend.compose(provider, consumer.component)?;
                Ok(EvalOutput {
                    component,
                    args: consumer.args,
                })
            }
            Expr::Extend { base, layer } => {
                let base = self.eval_plain(base, "an `&` operand").await?;
                let layer = self.eval_plain(layer, "an `&` operand").await?;
                let component = self.backend.extend(base, layer)?;
                Ok(EvalOutput {
                    component,
                    args: Vec::new(),
                })
            }
            Expr::Only { allow, body } => {
                let refs = allow
                    .iter()
                    .map(|entry| parse_allow_entry(entry))
                    .collect::<Result<Vec<_>, _>>()?;
                let body = self.eval_boxed(body).await?;
                let component = self.backend.restrict(body.component, &refs)?;
                Ok(EvalOutput {
                    component,
                    args: body.args,
                })
            }
            Expr::Rename { from, to, body } => {
                let body = self.eval_boxed(body).await?;
                let component = self.backend.rename(body.component, from, to)?;
                Ok(EvalOutput {
                    component,
                    args: body.args,
                })
            }
            Expr::With { bindings, body } => {
                let body = self.eval_boxed(body).await?;
                let mut component = body.component;
                // Bindings compose right-to-left so the first written binding ends up
                // outermost — the same order as writing the renamed providers out as a
                // `$` chain in the order given.
                for binding in bindings.iter().rev() {
                    let provider = self.eval_with_provider(binding).await?;
                    component = self.backend.compose(provider, component)?;
                }
                Ok(EvalOutput {
                    component,
                    args: body.args,
                })
            }
        }
    }

    /// Evaluate and require that no `main` arguments were applied (provider positions,
    /// `&` operands, `let` values, argument expressions).
    pub async fn eval_plain(
        &mut self,
        expr: &Expr,
        context: &'static str,
    ) -> Result<B::Component, EvalError> {
        let out = self.eval_boxed(expr).await?;
        if out.args.is_empty() {
            Ok(out.component)
        } else {
            Err(EvalError::ArgumentsNotAllowed { context })
        }
    }

    /// Boxed recursion shim: `eval` is async and recursive, so recursive calls go
    /// through a boxed future.
    fn eval_boxed<'s>(&'s mut self, expr: &'s Expr) -> EvalFuture<'s, B::Component> {
        Box::pin(self.eval(expr))
    }

    /// A name: a `let` binding (duplicated, so bindings are reusable values) or a
    /// program name resolved by the backend.
    async fn lookup(&mut self, name: &str) -> Result<B::Component, EvalError> {
        if let Some(bound) = self.bindings.get(name) {
            return Ok(self.backend.duplicate(bound)?);
        }
        Ok(self.backend.resolve(name).await?)
    }

    /// Argument application, type-directed by the callee's argument signature.
    async fn eval_app(
        &mut self,
        callee: &Expr,
        args: &[Arg],
    ) -> Result<EvalOutput<B::Component>, EvalError> {
        let callee_out = self.eval_boxed(callee).await?;
        let info = self.backend.describe(&callee_out.component);

        if info.kind == ComponentKind::Provider && !args.is_empty() {
            return Err(EvalError::ProviderArguments);
        }

        let mut named = callee_out.args;
        for arg in args {
            match arg {
                Arg::Flag { name, value } => {
                    let spec = info
                        .args
                        .iter()
                        .find(|spec| spec.name == *name)
                        .ok_or_else(|| EvalError::UnknownFlag { name: name.clone() })?;
                    push_arg(&mut named, spec, value)?;
                }
                Arg::Positional(value) => {
                    let spec = info
                        .args
                        .iter()
                        .find(|spec| !named.iter().any(|arg| arg.name == spec.name))
                        .ok_or(EvalError::TooManyPositional)?;
                    push_arg(&mut named, spec, value)?;
                }
            }
        }

        Ok(EvalOutput {
            component: callee_out.component,
            args: named,
        })
    }

    /// One `with` binding: the provider must export exactly one interface; its export
    /// slot is renamed to the target slot name (a no-op if already so named).
    async fn eval_with_provider(
        &mut self,
        binding: &WithBinding,
    ) -> Result<B::Component, EvalError> {
        let provider = self
            .eval_plain(&binding.provider, "a `with` provider")
            .await?;
        let info = self.backend.describe(&provider);
        let [export] = info.exports.as_slice() else {
            return Err(EvalError::WithProviderExports {
                slot: binding.slot.clone(),
                exports: info.exports.len(),
            });
        };
        if export.name == binding.slot {
            return Ok(provider);
        }
        let renamed = self.backend.rename(provider, &export.name, &binding.slot)?;
        Ok(renamed)
    }
}

/// Fill one parameter, encoding the value per the parameter's declared type.
fn push_arg(named: &mut Vec<NamedArg>, spec: &ArgSpec, value: &ArgValue) -> Result<(), EvalError> {
    if named.iter().any(|arg| arg.name == spec.name) {
        return Err(EvalError::DuplicateArgument {
            name: spec.name.clone(),
        });
    }
    if wave::is_component_type(&spec.ty) {
        // Type-directed: this parameter takes a program expression, not text. The
        // grammar and classification are in place; actually passing a component
        // through `spawn`'s WAVE-text arguments is not representable yet.
        return Err(EvalError::ComponentArgumentUnsupported {
            name: spec.name.clone(),
        });
    }
    let Some(encoded) = wave::encode_arg_value(&spec.ty, value) else {
        return Err(EvalError::ExpressionForDataParameter {
            name: spec.name.clone(),
            ty: spec.ty.clone(),
        });
    };
    named.push(NamedArg {
        name: spec.name.clone(),
        value: encoded,
    });
    Ok(())
}

/// Check an argument list for completeness against the program's signature: missing
/// optional parameters are filled with `none`, missing required ones are an error.
/// Used by the top-level run rule, where the final signature is known.
pub fn complete_args(args: &mut Vec<NamedArg>, specs: &[ArgSpec]) -> Result<(), EvalError> {
    for spec in specs {
        if args.iter().any(|arg| arg.name == spec.name) {
            continue;
        }
        if wave::option_inner(&spec.ty).is_some() {
            args.push(NamedArg {
                name: spec.name.clone(),
                value: wave::none_value(),
            });
        } else {
            return Err(EvalError::MissingArgument {
                name: spec.name.clone(),
                ty: spec.ty.clone(),
            });
        }
    }
    Ok(())
}

/// Parse one `only` allow-list entry into an interface reference.
///
/// Entries are interface or package references (`eo9:fs/fs`, `eo9:time`), optionally
/// versioned (`eo9:fs/fs@0.1.0`). A bare dotted world name (`sandbox.no-net`) would
/// need store-backed resolution of policy worlds, which is not available yet.
fn parse_allow_entry(entry: &str) -> Result<InterfaceRef, EvalError> {
    if !entry.contains(':') {
        return Err(EvalError::NamedWorldUnsupported {
            name: String::from(entry),
        });
    }
    let (interface, version) = match entry.split_once('@') {
        Some((interface, version)) => (interface, Some(String::from(version))),
        None => (entry, None),
    };
    Ok(InterfaceRef {
        interface: String::from(interface),
        version,
    })
}

/// Render a backend error with the operation it came from, for consistent messages.
pub fn operation_failed(operation: &str, err: &BackendError) -> String {
    format!("{operation} failed: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    use crate::parse::parse_expr;
    use crate::testutil::{MockBackend, binary, block_on_ready, provider};

    /// Evaluate `src` against `backend` with no `let` bindings.
    fn eval(backend: &mut MockBackend, src: &str) -> Result<EvalOutput<u32>, EvalError> {
        let expr = parse_expr(src).expect("parses");
        let bindings = BTreeMap::new();
        let mut evaluator = Evaluator::new(backend, &bindings);
        block_on_ready(evaluator.eval(&expr))
    }

    #[test]
    fn names_resolve_through_the_backend() {
        let mut backend = MockBackend::new();
        backend.program("browser", binary(&[("url", "string")]));
        let out = eval(&mut backend, "browser").expect("evaluates");
        assert!(out.args.is_empty());
        assert_eq!(backend.log, vec!["resolve(browser) -> c1"]);
        assert_eq!(out.component, 1);
    }

    #[test]
    fn compose_is_right_associative_in_evaluation_order() {
        let mut backend = MockBackend::new();
        backend.program("virtualfs", provider(&["eo9:fs/fs"]));
        backend.program("virtualnet", provider(&["eo9:net/net"]));
        backend.program("browser", binary(&[]));
        eval(&mut backend, "virtualfs $ virtualnet $ browser").expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(virtualfs) -> c1",
                "resolve(virtualnet) -> c2",
                "resolve(browser) -> c3",
                "compose(c2, c3) -> c4",
                "compose(c1, c4) -> c5",
            ]
        );
    }

    #[test]
    fn action_law_shape_extend_then_compose() {
        // (x & y) $ c evaluates as one extend followed by one compose.
        let mut backend = MockBackend::new();
        backend.program("time.frozen", provider(&["eo9:time/time"]));
        backend.program("virtualnet", provider(&["eo9:net/net"]));
        backend.program("app", binary(&[]));
        eval(&mut backend, "time.frozen & virtualnet $ app").expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(time.frozen) -> c1",
                "resolve(virtualnet) -> c2",
                "extend(c1, c2) -> c3",
                "resolve(app) -> c4",
                "compose(c3, c4) -> c5",
            ]
        );
    }

    #[test]
    fn only_restricts_with_parsed_interface_refs() {
        let mut backend = MockBackend::new();
        backend.program("cruncher", binary(&[("input", "string")]));
        eval(
            &mut backend,
            "only eo9:time,eo9:fs/fs@0.1.0 $ cruncher --input data.bin",
        )
        .expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(cruncher) -> c1",
                "describe(c1)",
                "restrict(c1, [eo9:time, eo9:fs/fs@0.1.0]) -> c2",
            ]
        );
    }

    #[test]
    fn only_with_a_named_world_is_not_supported_yet() {
        let mut backend = MockBackend::new();
        backend.program("app", binary(&[]));
        let err = eval(&mut backend, "only sandbox.no-net $ app").unwrap_err();
        assert_eq!(
            err,
            EvalError::NamedWorldUnsupported {
                name: "sandbox.no-net".to_string()
            }
        );
    }

    #[test]
    fn rename_relabels_on_the_body() {
        let mut backend = MockBackend::new();
        backend.program("tool", binary(&[]));
        eval(&mut backend, "rename eo9:fs/fs scratch-fs $ tool").expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(tool) -> c1",
                "rename(c1, eo9:fs/fs -> scratch-fs) -> c2",
            ]
        );
    }

    #[test]
    fn with_renames_the_provider_export_and_composes() {
        let mut backend = MockBackend::new();
        backend.program("realfs", provider(&["eo9:fs/fs"]));
        backend.program("memfs", provider(&["eo9:fs/fs"]));
        backend.program("backup-tool", binary(&[]));
        eval(
            &mut backend,
            "with realfs as system-fs, memfs as scratch-fs $ backup-tool",
        )
        .expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(backup-tool) -> c1",
                // Rightmost binding first (innermost), so the written order is the $ order.
                "resolve(memfs) -> c2",
                "describe(c2)",
                "rename(c2, eo9:fs/fs -> scratch-fs) -> c3",
                "compose(c3, c1) -> c4",
                "resolve(realfs) -> c5",
                "describe(c5)",
                "rename(c5, eo9:fs/fs -> system-fs) -> c6",
                "compose(c6, c4) -> c7",
            ]
        );
    }

    #[test]
    fn with_skips_the_rename_when_the_slot_already_matches() {
        let mut backend = MockBackend::new();
        backend.program("memfs", provider(&["eo9:fs/fs"]));
        backend.program("tool", binary(&[]));
        eval(&mut backend, "with memfs as eo9:fs/fs $ tool").expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "resolve(tool) -> c1",
                "resolve(memfs) -> c2",
                "describe(c2)",
                "compose(c2, c1) -> c3",
            ]
        );
    }

    #[test]
    fn with_provider_must_export_exactly_one_interface() {
        let mut backend = MockBackend::new();
        backend.program("posix-base", provider(&["eo9:fs/fs", "eo9:time/time"]));
        backend.program("tool", binary(&[]));
        let err = eval(&mut backend, "with posix-base as system-fs $ tool").unwrap_err();
        assert_eq!(
            err,
            EvalError::WithProviderExports {
                slot: "system-fs".to_string(),
                exports: 2
            }
        );
    }

    #[test]
    fn flags_encode_per_the_declared_types() {
        let mut backend = MockBackend::new();
        backend.program(
            "browser",
            binary(&[
                ("url", "string"),
                ("verbose", "bool"),
                ("max-connections", "u32"),
            ]),
        );
        let out = eval(
            &mut backend,
            "browser --url https://example.com --verbose true --max-connections 64",
        )
        .expect("evaluates");
        assert_eq!(
            out.args,
            vec![
                NamedArg {
                    name: "url".to_string(),
                    value: "\"https://example.com\"".to_string()
                },
                NamedArg {
                    name: "verbose".to_string(),
                    value: "true".to_string()
                },
                NamedArg {
                    name: "max-connections".to_string(),
                    value: "64".to_string()
                },
            ]
        );
    }

    #[test]
    fn positional_arguments_fill_parameters_in_declared_order() {
        let mut backend = MockBackend::new();
        backend.program("greet", binary(&[("name", "string"), ("excited", "bool")]));
        let out = eval(&mut backend, "greet world true").expect("evaluates");
        assert_eq!(
            out.args,
            vec![
                NamedArg {
                    name: "name".to_string(),
                    value: "\"world\"".to_string()
                },
                NamedArg {
                    name: "excited".to_string(),
                    value: "true".to_string()
                },
            ]
        );
        // Mixing named and positional: the named one is taken, positionals fill the rest.
        let out = eval(&mut backend, "greet --excited false shy").expect("evaluates");
        assert_eq!(
            out.args,
            vec![
                NamedArg {
                    name: "excited".to_string(),
                    value: "false".to_string()
                },
                NamedArg {
                    name: "name".to_string(),
                    value: "\"shy\"".to_string()
                },
            ]
        );
    }

    #[test]
    fn argument_errors_are_specific() {
        let mut backend = MockBackend::new();
        backend.program("greet", binary(&[("name", "string")]));
        assert_eq!(
            eval(&mut backend, "greet --nmae world").unwrap_err(),
            EvalError::UnknownFlag {
                name: "nmae".to_string()
            }
        );
        assert_eq!(
            eval(&mut backend, "greet --name a --name b").unwrap_err(),
            EvalError::DuplicateArgument {
                name: "name".to_string()
            }
        );
        assert_eq!(
            eval(&mut backend, "greet a b").unwrap_err(),
            EvalError::TooManyPositional
        );
        assert_eq!(
            eval(&mut backend, "greet --name (net.none $ x)").unwrap_err(),
            EvalError::ExpressionForDataParameter {
                name: "name".to_string(),
                ty: "string".to_string()
            }
        );
    }

    #[test]
    fn component_typed_parameters_are_recognised_but_unsupported() {
        let mut backend = MockBackend::new();
        backend.program("interpret", binary(&[("program", "borrow<component>")]));
        backend.program("cruncher", binary(&[]));
        let err = eval(&mut backend, "interpret (only eo9:time $ cruncher)").unwrap_err();
        assert_eq!(
            err,
            EvalError::ComponentArgumentUnsupported {
                name: "program".to_string()
            }
        );
        // A bare word in that position is also a program expression, not text.
        let err = eval(&mut backend, "interpret cruncher").unwrap_err();
        assert_eq!(
            err,
            EvalError::ComponentArgumentUnsupported {
                name: "program".to_string()
            }
        );
    }

    #[test]
    fn provider_arguments_are_rejected() {
        let mut backend = MockBackend::new();
        backend.program_with_args("virtualfs", provider(&["eo9:fs/fs"]), &[("dir", "string")]);
        backend.program("browser", binary(&[]));
        let err = eval(&mut backend, "virtualfs --dir /tmp/sandbox $ browser").unwrap_err();
        assert_eq!(err, EvalError::ProviderArguments);
    }

    #[test]
    fn arguments_on_amp_operands_are_rejected() {
        let mut backend = MockBackend::new();
        backend.program("hello", binary(&[("name", "string")]));
        backend.program("memfs", provider(&["eo9:fs/fs"]));
        let err = eval(&mut backend, "hello --name x & memfs").unwrap_err();
        assert_eq!(
            err,
            EvalError::ArgumentsNotAllowed {
                context: "an `&` operand"
            }
        );
    }

    #[test]
    fn arguments_survive_composition_and_gates() {
        let mut backend = MockBackend::new();
        backend.program("net.deny", provider(&["eo9:net/net"]));
        backend.program("fetcher", binary(&[("url", "string")]));
        let out = eval(
            &mut backend,
            "only eo9:net $ net.deny $ fetcher --url https://example.com",
        )
        .expect("evaluates");
        assert_eq!(
            out.args,
            vec![NamedArg {
                name: "url".to_string(),
                value: "\"https://example.com\"".to_string()
            }]
        );
    }

    #[test]
    fn unknown_names_report_the_backend_error() {
        let mut backend = MockBackend::new();
        let err = eval(&mut backend, "no-such-program").unwrap_err();
        assert_eq!(
            err,
            EvalError::Backend(BackendError::new(
                "cannot resolve `no-such-program`: no such module"
            ))
        );
    }

    #[test]
    fn let_bindings_are_duplicated_not_consumed() {
        let mut backend = MockBackend::new();
        backend.program("app", binary(&[]));
        let det_env = backend.insert(provider(&["eo9:time/time", "eo9:net/net"]));
        let mut bindings = BTreeMap::new();
        bindings.insert("det-env".to_string(), det_env);

        let expr = parse_expr("det-env $ app").expect("parses");
        let mut evaluator = Evaluator::new(&mut backend, &bindings);
        block_on_ready(evaluator.eval(&expr)).expect("evaluates");
        assert_eq!(
            backend.log,
            vec![
                "duplicate(c1) -> c2",
                "resolve(app) -> c3",
                "compose(c2, c3) -> c4",
            ]
        );
    }

    #[test]
    fn complete_args_fills_optionals_and_reports_missing_required() {
        let specs = vec![
            ArgSpec {
                name: "url".to_string(),
                ty: "string".to_string(),
            },
            ArgSpec {
                name: "proxy".to_string(),
                ty: "option<string>".to_string(),
            },
        ];
        let mut args = vec![NamedArg {
            name: "url".to_string(),
            value: "\"https://example.com\"".to_string(),
        }];
        complete_args(&mut args, &specs).expect("completes");
        assert_eq!(
            args,
            vec![
                NamedArg {
                    name: "url".to_string(),
                    value: "\"https://example.com\"".to_string()
                },
                NamedArg {
                    name: "proxy".to_string(),
                    value: "none".to_string()
                },
            ]
        );

        let mut missing = Vec::new();
        assert_eq!(
            complete_args(&mut missing, &specs).unwrap_err(),
            EvalError::MissingArgument {
                name: "url".to_string(),
                ty: "string".to_string()
            }
        );
    }
}
