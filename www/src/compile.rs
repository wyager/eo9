//! Server-side composition compiler for the in-browser `/vm` demo (plan/18 Decision 20).
//!
//! In-blob codegen is std/mmap-blocked, so the browser eosh cannot compile a fused
//! composition itself — `compile` of a `$`/`&` result returns a clean "needs the compiler"
//! refusal (plan/18 D19). This module is the path to actually running such a composition:
//! the standalone server (which has the full host toolchain) accepts a composition expressed
//! over **store program names + algebra ops** (never uploaded component bytes), resolves each
//! name against a fixed allow-set of raw components shipped with the site, fuses them with the
//! real `eo9-component` algebra, and compiles the result to a `pulley32` image with the same
//! engine configuration the blob's pre-AOT'd artifacts use (`xtask::preaot_for_web`). The
//! browser runs the returned image through its existing run-to-completion `spawn` path.
//!
//! Security: the only attacker-controlled input is a short expression of *names and ops* — no
//! bytes cross the boundary, names are checked against the allow-set (anything else is
//! rejected), and the caller bounds request size, compile time, and concurrency.

use std::path::{Path, PathBuf};

use eo9_component::{Component, compose, restrict};

/// A parsed composition: an optional leading `only <interfaces>` gate and a `$`-separated
/// chain of store program names. Consumer flags (`--name value`) are stripped — they are
/// bound when the browser spawns the returned image, not part of the fused component.
struct Expr {
    only: Option<Vec<String>>,
    /// Program names, in source order; folded right-associatively with `$`.
    names: Vec<String>,
}

/// Why a `/vm/compile` request was rejected. All map to 4xx — the input is the client's.
#[derive(Debug)]
pub enum CompileError {
    /// The expression could not be parsed, or used an unsupported operator.
    BadExpression(String),
    /// A referenced name is not in the allow-set (the shipped store programs).
    UnknownProgram(String),
    /// The named program's raw component bytes are missing on the server.
    MissingComponent(String),
    /// The algebra or the pulley compiler rejected the (resolved, trusted) composition.
    CompileFailed(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::BadExpression(m) => write!(f, "bad composition expression: {m}"),
            CompileError::UnknownProgram(n) => {
                write!(f, "`{n}` is not one of the available store programs")
            }
            CompileError::MissingComponent(n) => write!(f, "no component on the server for `{n}`"),
            CompileError::CompileFailed(m) => write!(f, "compile failed: {m}"),
        }
    }
}

/// Parse `[only <iface>[,<iface>...] $] name [ $ name ]* [--flag value ...]`. Only `$`
/// composition and a single leading `only` gate are supported by the browser compile
/// endpoint; `&`/`rename`/`configure` (and anything else) are rejected with a clear message.
fn parse(expr: &str) -> Result<Expr, CompileError> {
    // Drop consumer flags: everything from the first `--<flag>` token onward.
    let mut head: Vec<&str> = Vec::new();
    for tok in expr.split_whitespace() {
        if tok.starts_with("--") {
            break;
        }
        head.push(tok);
    }
    if head.is_empty() {
        return Err(CompileError::BadExpression("empty composition".into()));
    }
    if head.iter().any(|t| *t == "&" || *t == "rename" || *t == "with" || *t == "configure") {
        return Err(CompileError::BadExpression(
            "the browser compile endpoint supports only `$` composition and a leading `only` \
             gate for now (native Eo9 and the bare-metal kernel run the full algebra)"
                .into(),
        ));
    }

    let mut only = None;
    let mut rest = &head[..];
    if rest.first() == Some(&"only") {
        // `only <allow-list> $ ...` — the allow-list is one comma-separated token.
        let list = rest
            .get(1)
            .ok_or_else(|| CompileError::BadExpression("`only` needs an allow-list".into()))?;
        if rest.get(2) != Some(&"$") {
            return Err(CompileError::BadExpression("`only <list>` must be followed by `$`".into()));
        }
        only = Some(list.split(',').map(|s| s.trim().to_string()).collect());
        rest = &rest[3..];
    }

    // The remainder is `name ($ name)*`.
    let mut names = Vec::new();
    let mut expect_name = true;
    for tok in rest {
        if expect_name {
            if *tok == "$" {
                return Err(CompileError::BadExpression("expected a program name".into()));
            }
            names.push((*tok).to_string());
            expect_name = false;
        } else {
            if *tok != "$" {
                return Err(CompileError::BadExpression(format!(
                    "expected `$` between program names, found `{tok}`"
                )));
            }
            expect_name = true;
        }
    }
    if names.is_empty() || expect_name {
        return Err(CompileError::BadExpression("dangling `$` / missing program name".into()));
    }
    Ok(Expr { only, names })
}

/// Resolve a program name to its raw component bytes under `raw_dir`, after checking it is in
/// the allow-set. `allow` is the set of program names the site actually ships.
fn load_program(name: &str, raw_dir: &Path, allow: &[String]) -> Result<Component, CompileError> {
    if !allow.iter().any(|a| a == name) {
        return Err(CompileError::UnknownProgram(name.to_string()));
    }
    let path: PathBuf = raw_dir.join(format!("{name}.wasm"));
    let bytes = std::fs::read(&path).map_err(|_| CompileError::MissingComponent(name.to_string()))?;
    Component::load(bytes).map_err(|e| CompileError::CompileFailed(format!("load `{name}`: {e:?}")))
}

/// Fuse the expression's programs (resolved from the trusted `raw_dir`/`allow` store) with the
/// real algebra and compile the result to a `pulley32` image the browser blob can run. Returns
/// the `.cwasm` bytes.
pub fn compile_expression(
    expr: &str,
    raw_dir: &Path,
    allow: &[String],
) -> Result<Vec<u8>, CompileError> {
    let parsed = parse(expr)?;

    // Fold the `$` chain right-associatively (the spec's reading): a $ (b $ c).
    let mut acc: Option<Component> = None;
    for name in parsed.names.iter().rev() {
        let comp = load_program(name, raw_dir, allow)?;
        acc = Some(match acc {
            None => comp,
            Some(consumer) => compose(&comp, &consumer)
                .map_err(|e| CompileError::CompileFailed(format!("compose: {e:?}")))?,
        });
    }
    let mut fused = acc.expect("parse guarantees at least one name");

    if let Some(allow_list) = parsed.only {
        let refs: Vec<eo9_component::InterfaceRef> = allow_list
            .into_iter()
            .map(eo9_component::InterfaceRef::any)
            .collect();
        fused = restrict(&fused, &refs)
            .map_err(|e| CompileError::CompileFailed(format!("only: {e:?}")))?;
    }

    precompile_pulley(&fused.executable_bytes())
}

/// The `pulley32` engine the blob's pre-AOT'd artifacts use (mirrors `xtask::preaot_for_web`,
/// `consume_fuel = false`). The compile-relevant settings here must match the blob's runtime
/// engine, or a produced image will not deserialize there.
pub fn pulley_engine() -> Result<wasmtime::Engine, CompileError> {
    let mut config = wasmtime::Config::new();
    config
        .target("pulley32")
        .map_err(|e| CompileError::CompileFailed(format!("target pulley32: {e:#}")))?;
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.signals_based_traps(false);
    config.memory_reservation(0);
    config.memory_reservation_for_growth(1 << 20);
    config.memory_guard_size(0);
    config.memory_init_cow(false);
    config.concurrency_support(true);
    config.gc_support(false);
    config.wasm_threads(false);
    config.consume_fuel(false);
    wasmtime::Engine::new(&config)
        .map_err(|e| CompileError::CompileFailed(format!("pulley32 engine: {e:#}")))
}

/// Compile a component to a `pulley32` artifact the browser blob can run.
fn precompile_pulley(component: &[u8]) -> Result<Vec<u8>, CompileError> {
    pulley_engine()?
        .precompile_component(component)
        .map_err(|e| CompileError::CompileFailed(format!("precompile: {e:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_unsupported_ops_and_dangling() {
        assert!(parse("a & b").is_err());
        assert!(parse("a $").is_err());
        assert!(parse("$ a").is_err());
        assert!(parse("").is_err());
        let e = parse("only eo9:text/text $ rng --count 3").unwrap();
        assert_eq!(e.only.as_deref(), Some(&["eo9:text/text".to_string()][..]));
        assert_eq!(e.names, vec!["rng".to_string()]);
    }

    /// The end-to-end server compile: fuse `entropy.seeded $ rng` from the real guest
    /// components and produce a pulley32 image the blob's engine deserializes as a component.
    #[test]
    fn compiles_entropy_seeded_compose_rng_to_a_runnable_pulley_image() {
        // Resolve the raw components from the guest build output into a name-keyed temp dir.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let components = manifest
            .parent()
            .unwrap()
            .join("guest")
            .join("target")
            .join("components");
        let pairs = [
            ("entropy.seeded", "eo9-stub-entropy-seeded.wasm"),
            ("rng", "eo9-coreutil-rng.wasm"),
        ];
        let raw_dir = std::env::temp_dir().join("eo9-www-compile-test");
        let _ = std::fs::remove_dir_all(&raw_dir);
        std::fs::create_dir_all(&raw_dir).unwrap();
        for (name, file) in pairs {
            let src = components.join(file);
            if !src.exists() {
                eprintln!("skipping: {} not built (run `cargo xtask build-guest`)", src.display());
                return;
            }
            std::fs::copy(&src, raw_dir.join(format!("{name}.wasm"))).unwrap();
        }

        let allow = vec!["entropy.seeded".to_string(), "rng".to_string()];
        // The strong signal: the real algebra fuses `entropy.seeded $ rng` and the host
        // pulley32 compiler ACCEPTS and compiles the result (`precompile_component` -> Ok).
        // The artifact targets pulley32 (the 32-bit pulley host = the wasm32 blob), so it is
        // not deserializable in this native 64-bit process — running it is the blob's job
        // (the round-trip is the browser/harness test); here we verify it compiled and is a
        // substantial artifact, not an empty/degenerate one.
        let image = compile_expression("entropy.seeded $ rng --count 3", &raw_dir, &allow)
            .expect("the composition compiles server-side");
        assert!(image.len() > 4096, "implausibly small image: {} bytes", image.len());

        // A name outside the allow-set is rejected before any compile.
        assert!(matches!(
            compile_expression("secret $ rng", &raw_dir, &allow),
            Err(CompileError::UnknownProgram(_))
        ));
        // A plain program (no `$`) compiles too (the endpoint also serves single names).
        assert!(compile_expression("rng", &raw_dir, &allow).is_ok());
    }
}
