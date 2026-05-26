//! Executable fixture components, built in-process from WIT text plus hand-written core
//! modules.
//!
//! The integration suites need components that actually *run* (so capability decisions are
//! observable in program behaviour), but they must not depend on the guest workspace or on
//! area 09's stub providers, which are developed in parallel and are not built when
//! `cargo test --workspace` runs. Each fixture is therefore assembled here, from:
//!
//! 1. a WIT world (the capability surface — the part the algebra operates on), and
//! 2. a small hand-written core module (the behaviour), using the same legacy canonical-ABI
//!    names a `wit-bindgen` build would use,
//!
//! joined by `wit-component`'s metadata embedding and `ComponentEncoder` — the same
//! pipeline real guest components go through, minus the Rust. The result is a validated
//! [`Component`] the algebra and the runtime both accept.
//!
//! Two fixture vocabularies are provided:
//!
//! * `eo9-tests:cap` — a self-contained, resource-free capability vocabulary (`answer`,
//!   `answer-optional`, `store`) for the capability suite: sealing, `only`, deny-style
//!   providers, named slots, optional absence.
//! * fixtures against the real `eo9:text` / `eo9:entropy` / `eo9:time` packages from
//!   `wit/`, for tests that involve the runtime's root providers (the ambient context)
//!   and the determinism suite.
//!
//! The kill/linearity fixture ([`sleeper_wat`]) is a raw component WAT (not WIT-built):
//! it needs the Component Model async built-ins to park on a host future, and
//! `eo9_runtime::Image::compile` accepts WAT directly.

use std::path::PathBuf;

use eo9_component::Component;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

// -----------------------------------------------------------------------------------------
// The generic builder
// -----------------------------------------------------------------------------------------

/// The repository's `wit/` directory (the machine-readable interface contract).
pub fn repo_wit_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("wit")
}

/// Builds an executable fixture component: parse `wit` (plus the named `wit/<dir>`
/// packages from the repository), select `world`, and wrap the hand-written `core_wat`
/// module into a component implementing that world.
///
/// The core module must use the legacy canonical-ABI names (`<interface>#<func>` exports,
/// `<interface>`/`<func>` imports, `memory`, `cabi_realloc`); `ComponentEncoder` validates
/// the module against the world, so a mismatch fails loudly here rather than at run time.
pub fn build_component(wit: &str, wit_deps: &[&str], world: &str, core_wat: &str) -> Component {
    let mut resolve = Resolve::default();
    for dir in wit_deps {
        let path = repo_wit_dir().join(dir);
        resolve
            .push_dir(&path)
            .unwrap_or_else(|err| panic!("failed to load wit/{dir}: {err:#}"));
    }
    let package = resolve
        .push_source("fixture.wit", wit)
        .unwrap_or_else(|err| panic!("failed to parse fixture WIT: {err:#}"));
    let world = resolve
        .select_world(&[package], Some(world))
        .unwrap_or_else(|err| panic!("failed to select fixture world: {err:#}"));

    let mut module =
        wat::parse_str(core_wat).unwrap_or_else(|err| panic!("invalid fixture core WAT: {err:#}"));
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
        .unwrap_or_else(|err| panic!("failed to embed component metadata: {err:#}"));
    let bytes = ComponentEncoder::default()
        .validate(true)
        .module(&module)
        .unwrap_or_else(|err| panic!("fixture core module does not implement its world: {err:#}"))
        .encode()
        .unwrap_or_else(|err| panic!("failed to encode fixture component: {err:#}"));
    Component::load(bytes).expect("fixture component should load")
}

// -----------------------------------------------------------------------------------------
// The self-contained capability vocabulary: eo9-tests:cap
// -----------------------------------------------------------------------------------------

/// The `eo9-tests:cap` fixture package: a minimal, resource-free capability vocabulary.
const CAP_WIT: &str = r#"
package eo9-tests:cap@0.1.0;

/// A minimal capability: something that answers a question.
interface answer {
    get: func() -> u32;
}

/// The mechanically derived optional flavor of `answer` (SPEC: The capability algebra).
interface answer-optional {
    default: func() -> option<u32>;
}

/// A capability whose operations fail in their own error vocabulary.
interface store {
    variant fetch-error {
        denied,
        io(string),
    }
    fetch: func(key: string) -> result<u32, fetch-error>;
}

/// A binary that requires `answer` and reports what it was told.
world answer-consumer {
    import answer;
    export main: func() -> result<u32, string>;
}

/// A binary asking for two instances of `answer` under distinct slot names.
world two-answers {
    import left: answer;
    import right: answer;
    export main: func() -> result<u32, string>;
}

/// A binary that can use `answer` but does not require it.
world optional-consumer {
    import answer-optional;
    export main: func() -> result<u32, string>;
}

/// A binary that requires `store` and reports failures in its own vocabulary.
world storage-consumer {
    import store;
    variant grief {
        storage-denied,
        storage-unavailable(string),
    }
    export main: func() -> result<u32, grief>;
}

/// A provider of `answer` (the answer itself lives in the core module).
world answer-provider {
    export answer;
}

/// A provider of the optional flavor of `answer`.
world optional-provider {
    export answer-optional;
}

/// A provider of `store`.
world store-provider {
    export store;
}
"#;

/// A binary requiring `answer`; `main` returns `ok(get())`.
pub fn answer_consumer() -> Component {
    const CORE: &str = r#"
(module
  (import "eo9-tests:cap/answer@0.1.0" "get" (func $get (result i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "main") (result i32)
    ;; result<u32, string>: ok(get()) at 32
    (i32.store8 (i32.const 32) (i32.const 0))
    (i32.store (i32.const 36) (call $get))
    (i32.const 32)))
"#;
    build_component(CAP_WIT, &[], "answer-consumer", CORE)
}

/// A binary with two named `answer` slots; `main` returns `ok(left * 100 + right)`.
pub fn two_answers_consumer() -> Component {
    const CORE: &str = r#"
(module
  (import "left" "get" (func $left-get (result i32)))
  (import "right" "get" (func $right-get (result i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "main") (result i32)
    ;; result<u32, string>: ok(left * 100 + right) at 32
    (i32.store8 (i32.const 32) (i32.const 0))
    (i32.store (i32.const 36)
      (i32.add (i32.mul (call $left-get) (i32.const 100)) (call $right-get)))
    (i32.const 32)))
"#;
    build_component(CAP_WIT, &[], "two-answers", CORE)
}

/// The value [`optional_consumer`] reports when the optional `answer` capability is absent.
pub const OPTIONAL_ABSENT_SENTINEL: u32 = 7777;

/// A binary importing `answer-optional`; `main` returns `ok(value)` when the capability is
/// present and `ok(`[`OPTIONAL_ABSENT_SENTINEL`]`)` when it observes absence.
pub fn optional_consumer() -> Component {
    const CORE: &str = r#"
(module
  (import "eo9-tests:cap/answer-optional@0.1.0" "default" (func $default (param i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "main") (result i32)
    ;; option<u32> is written to 16 by the import
    (call $default (i32.const 16))
    (i32.store8 (i32.const 32) (i32.const 0))
    (if (i32.load8_u (i32.const 16))
      (then (i32.store (i32.const 36) (i32.load (i32.const 20))))
      (else (i32.store (i32.const 36) (i32.const 7777))))
    (i32.const 32)))
"#;
    build_component(CAP_WIT, &[], "optional-consumer", CORE)
}

/// A binary requiring `store`; `main` maps the API's errors into its own failure variant
/// (`storage-denied` / `storage-unavailable`), so a denial is reported in the program's own
/// vocabulary rather than as a trap.
pub fn storage_consumer() -> Component {
    const CORE: &str = r#"
(module
  (import "eo9-tests:cap/store@0.1.0" "fetch" (func $fetch (param i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "task-state")
  (data (i32.const 80) "unexpected backend error")
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 4096))
  (func (export "main") (result i32)
    ;; fetch("task-state") -> result<u32, fetch-error>, written to 16
    (call $fetch (i32.const 8) (i32.const 10) (i32.const 16))
    (if (i32.load8_u (i32.const 16))
      (then
        ;; err: map the API's error into the program's own failure vocabulary
        (i32.store8 (i32.const 48) (i32.const 1))
        (if (i32.load8_u (i32.const 20))
          (then
            ;; io(_) -> storage-unavailable("unexpected backend error")
            (i32.store8 (i32.const 52) (i32.const 1))
            (i32.store (i32.const 56) (i32.const 80))
            (i32.store (i32.const 60) (i32.const 24)))
          (else
            ;; denied -> storage-denied
            (i32.store8 (i32.const 52) (i32.const 0)))))
      (else
        ;; ok(v) -> ok(v)
        (i32.store8 (i32.const 48) (i32.const 0))
        (i32.store (i32.const 52) (i32.load (i32.const 20)))))
    (i32.const 48)))
"#;
    build_component(CAP_WIT, &[], "storage-consumer", CORE)
}

/// A provider of `answer` whose `get()` always returns `value`.
pub fn answer_provider(value: u32) -> Component {
    let core = format!(
        r#"
(module
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "eo9-tests:cap/answer@0.1.0#get") (result i32) (i32.const {value})))
"#
    );
    build_component(CAP_WIT, &[], "answer-provider", &core)
}

/// A provider of `answer-optional` whose `default()` answers `some(value)`.
pub fn optional_provider_present(value: u32) -> Component {
    let core = format!(
        r#"
(module
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "eo9-tests:cap/answer-optional@0.1.0#default") (result i32)
    (i32.store8 (i32.const 16) (i32.const 1))
    (i32.store (i32.const 20) (i32.const {value}))
    (i32.const 16)))
"#
    );
    build_component(CAP_WIT, &[], "optional-provider", &core)
}

/// A provider of `answer-optional` whose `default()` answers `none` — the shape of an
/// `X.none` absence stub.
pub fn optional_provider_absent() -> Component {
    const CORE: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "eo9-tests:cap/answer-optional@0.1.0#default") (result i32)
    ;; 64 points at zeroed memory: discriminant 0 = none
    (i32.const 64)))
"#;
    build_component(CAP_WIT, &[], "optional-provider", CORE)
}

/// A bump allocator body shared by the provider fixtures that receive strings.
const BUMP_REALLOC: &str = r#"
  (global $heap (mut i32) (i32.const 4096))
  (func (export "cabi_realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
    (local $ptr i32)
    (local.set $ptr
      (i32.and
        (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
        (i32.sub (i32.const 0) (local.get $align))))
    (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
    (local.get $ptr))
"#;

/// A deny-style provider of `store`: every `fetch` fails with the API's own `denied` case.
pub fn store_deny_provider() -> Component {
    let core = format!(
        r#"
(module
  (memory (export "memory") 1)
{BUMP_REALLOC}
  (func (export "eo9-tests:cap/store@0.1.0#fetch") (param i32 i32) (result i32)
    ;; err(denied), in the store API's own error vocabulary
    (i32.store8 (i32.const 32) (i32.const 1))
    (i32.store8 (i32.const 36) (i32.const 0))
    (i32.const 32)))
"#
    );
    build_component(CAP_WIT, &[], "store-provider", &core)
}

/// A working provider of `store`: every `fetch` succeeds with `value`.
pub fn store_ok_provider(value: u32) -> Component {
    let core = format!(
        r#"
(module
  (memory (export "memory") 1)
{BUMP_REALLOC}
  (func (export "eo9-tests:cap/store@0.1.0#fetch") (param i32 i32) (result i32)
    (i32.store8 (i32.const 32) (i32.const 0))
    (i32.store (i32.const 36) (i32.const {value}))
    (i32.const 32)))
"#
    );
    build_component(CAP_WIT, &[], "store-provider", &core)
}

// -----------------------------------------------------------------------------------------
// Fixtures against the real eo9:* packages from wit/
// -----------------------------------------------------------------------------------------

/// Fixture worlds against the real `eo9:text` package.
const TEXTCAP_WIT: &str = r#"
package eo9-tests:textcap@0.1.0;

/// A binary that writes one line through the real `eo9:text` capability.
world text-writer {
    import eo9:text/text@0.1.0;
    export main: func() -> result<u32, string>;
}

/// A provider of the real `eo9:text` interface that accepts and discards output
/// (the shape of `text.null`).
world text-sink {
    export eo9:text/types@0.1.0;
    export eo9:text/text@0.1.0;
}
"#;

/// The text [`text_writer`] writes to stdout.
pub const TEXT_WRITER_OUTPUT: &str = "sealed-write";

/// A binary importing the real `eo9:text/text`; it writes [`TEXT_WRITER_OUTPUT`] to stdout
/// and returns `ok(42)` (or `err("write failed")` if the provider refused the write).
pub fn text_writer() -> Component {
    const CORE: &str = r#"
(module
  (import "eo9:text/text@0.1.0" "default" (func $default (result i32)))
  (import "eo9:text/text@0.1.0" "write" (func $write (param i32 i32 i32 i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "sealed-write")
  (data (i32.const 32) "write failed")
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 4096))
  (func (export "main") (result i32)
    ;; write "sealed-write" to stdout; the write result lands at 64
    (call $write (call $default) (i32.const 0) (i32.const 8) (i32.const 12) (i32.const 64))
    (if (result i32) (i32.load8_u (i32.const 64))
      (then
        ;; err(_) from the provider -> err("write failed")
        (i32.store8 (i32.const 96) (i32.const 1))
        (i32.store (i32.const 100) (i32.const 32))
        (i32.store (i32.const 104) (i32.const 12))
        (i32.const 96))
      (else
        ;; ok -> ok(42)
        (i32.store8 (i32.const 96) (i32.const 0))
        (i32.store (i32.const 100) (i32.const 42))
        (i32.const 96)))))
"#;
    build_component(TEXTCAP_WIT, &["text"], "text-writer", CORE)
}

/// A provider of the real `eo9:text/text` interface that accepts and discards all output.
pub fn text_sink_provider() -> Component {
    let core = format!(
        r#"
(module
  (import "[export]eo9:text/types@0.1.0" "[resource-new]text-impl" (func $new (param i32) (result i32)))
  (memory (export "memory") 1)
{BUMP_REALLOC}
  (func (export "eo9:text/types@0.1.0#[dtor]text-impl") (param i32))
  (func (export "eo9:text/text@0.1.0#default") (result i32)
    (call $new (i32.const 0)))
  (func (export "eo9:text/text@0.1.0#write") (param i32 i32 i32 i32) (result i32)
    ;; accept and discard; 64 points at zeroed memory = ok
    (i32.const 64))
  (func (export "eo9:text/text@0.1.0#read-line") (param i32) (result i32)
    ;; not exercised by the fixtures
    unreachable))
"#
    );
    build_component(TEXTCAP_WIT, &["text"], "text-sink", &core)
}

/// The determinism-suite guest world: text + entropy + time folded into the outcome.
const DET_WIT: &str = r#"
package eo9-tests:det@0.1.0;

/// A binary that folds text, entropy, and time observations into its outcome, for the
/// determinism suite: with deterministic providers, repeated runs must be byte-identical.
world det {
    import eo9:text/text@0.1.0;
    import eo9:entropy/entropy@0.1.0;
    import eo9:time/time@0.1.0;
    export main: func(tag: string) -> result<u64, string>;
}
"#;

/// A binary importing the real text, entropy, and time interfaces. `main(tag)` echoes
/// `tag` plus one entropy-derived character to stdout and returns
/// `ok((x ^ y) + now.seconds + monotonic-now)` over two entropy samples — every
/// provider observation is folded into the rendered outcome.
pub fn det_guest() -> Component {
    let core = format!(
        r#"
(module
  (import "eo9:text/text@0.1.0" "default" (func $text-default (result i32)))
  (import "eo9:text/text@0.1.0" "write" (func $write (param i32 i32 i32 i32 i32)))
  (import "eo9:entropy/entropy@0.1.0" "default" (func $entropy-default (result i32)))
  (import "eo9:entropy/entropy@0.1.0" "get-u64" (func $get-u64 (param i32) (result i64)))
  (import "eo9:time/time@0.1.0" "default" (func $time-default (result i32)))
  (import "eo9:time/time@0.1.0" "now" (func $now (param i32 i32)))
  (import "eo9:time/time@0.1.0" "monotonic-now" (func $monotonic-now (param i32) (result i64)))
  (memory (export "memory") 1)
{BUMP_REALLOC}
  (func (export "main") (param $tag-ptr i32) (param $tag-len i32) (result i32)
    (local $text i32) (local $entropy i32) (local $time i32)
    (local $x i64) (local $y i64) (local $value i64)
    (local.set $text (call $text-default))
    (local.set $entropy (call $entropy-default))
    (local.set $time (call $time-default))
    ;; echo the tag to stdout (the write result goes to scratch at 128 and is ignored)
    (call $write (local.get $text) (i32.const 0) (local.get $tag-ptr) (local.get $tag-len) (i32.const 128))
    ;; two entropy samples
    (local.set $x (call $get-u64 (local.get $entropy)))
    (local.set $y (call $get-u64 (local.get $entropy)))
    ;; one entropy-derived character ('a' + (x & 15)) to stdout
    (i32.store8 (i32.const 8)
      (i32.add (i32.const 97) (i32.and (i32.wrap_i64 (local.get $x)) (i32.const 15))))
    (call $write (local.get $text) (i32.const 0) (i32.const 8) (i32.const 1) (i32.const 128))
    ;; fold wall-clock seconds and the monotonic reading into the outcome
    (call $now (local.get $time) (i32.const 160))
    (local.set $value
      (i64.add
        (i64.add (i64.xor (local.get $x) (local.get $y)) (i64.load (i32.const 160)))
        (call $monotonic-now (local.get $time))))
    ;; ok(value) : result<u64, string> at 192
    (i32.store8 (i32.const 192) (i32.const 0))
    (i64.store (i32.const 200) (local.get $value))
    (i32.const 192)))
"#
    );
    build_component(DET_WIT, &["text", "entropy", "time"], "det", &core)
}

// -----------------------------------------------------------------------------------------
// The kill/linearity fixture (raw component WAT)
// -----------------------------------------------------------------------------------------

/// A component whose `main` calls the async `eo9:time/time.sleep` operation and parks on
/// it (`main` is async-lifted because sync-lifted exports cannot block on the pinned
/// runtime; the sync-lowered call to the async operation suspends the task until the host
/// completes it). With a provider whose sleep never resolves, the task stays blocked on
/// the provider operation — exactly the state the kill/linearity contract is about.
/// Returns `9` if the sleep ever completes.
///
/// This is raw WAT rather than a WIT-built fixture because it needs the async lift
/// directly; `eo9_runtime::Image::compile` accepts WAT text.
pub fn sleeper_wat() -> &'static str {
    r#"
(component
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $ti (eq $time-impl)))
    (export "default" (func (result (own $ti))))
    (export "sleep" (func async (param "t" (borrow $ti)) (param "duration-ns" u64)))))

  (alias export $time "default" (func $default))
  (alias export $time "sleep" (func $sleep))

  (core func $default-lowered (canon lower (func $default)))
  (core func $sleep-lowered (canon lower (func $sleep)))
  (core func $task-return (canon task.return (result u32)))

  (core module $m
    (import "host" "default" (func $default (result i32)))
    (import "host" "sleep" (func $sleep (param i32 i64)))
    (import "host" "task-return" (func $task-return (param i32)))

    (func (export "main")
      ;; ask for a one-hour sleep; the test provider never resolves it anyway
      (call $sleep (call $default) (i64.const 3600000000000))
      (call $task-return (i32.const 9))))

  (core instance $i (instantiate $m
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "sleep" (func $sleep-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u32) (canon lift (core func $i "main") async))
)
"#
}

// -----------------------------------------------------------------------------------------
// The invoker-configured environment fixture (raw component WAT)
// -----------------------------------------------------------------------------------------

/// The fixed line [`invoker_env_guest`] writes to stdout (followed by one entropy-derived
/// character).
pub const INVOKER_ENV_OUTPUT_LINE: &str = "invoker-env-ok";

/// The guest of the invoker-configured environment suite: a binary that imports the real
/// `eo9:time`, `eo9:entropy`, and `eo9:text` interfaces and **none of the stub config
/// interfaces** -- configuration is entirely the invoker's business (the algebra's
/// `configure` operation), and the program only observes the result.
///
/// Its async `main`:
/// 1. reads the wall clock and the monotonic clock,
/// 2. sleeps through the (frozen) clock -- the call completing at all is what proves the
///    configured provider forwards its async API,
/// 3. draws two samples from the seeded entropy,
/// 4. writes [`INVOKER_ENV_OUTPUT_LINE`] plus one entropy-derived character through
///    `eo9:text` (left residual, so the ambient text provider captures it), and
/// 5. returns `(x ^ y) + now.seconds + monotonic-now` -- every provider observation is
///    folded into the rendered outcome.
pub fn invoker_env_guest() -> Component {
    let bytes = wat::parse_str(invoker_env_wat())
        .expect("invoker-env fixture must be valid component WAT")
        .to_vec();
    Component::load(bytes).expect("invoker-env fixture should load")
}

fn invoker_env_wat() -> &'static str {
    r#"
(component
  ;; ----- time: the API only (no frozen-config import) ---------------------------------------
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $ti (eq $time-impl)))
    (type $datetime-def (record (field "seconds" s64) (field "nanoseconds" u32)))
    (export "datetime" (type $datetime (eq $datetime-def)))
    (type $instant-def (record (field "nanoseconds" u64)))
    (export "instant" (type $instant (eq $instant-def)))
    (export "default" (func (result (own $ti))))
    (export "now" (func (param "t" (borrow $ti)) (result $datetime)))
    (export "monotonic-now" (func (param "t" (borrow $ti)) (result $instant)))
    (export "sleep" (func async (param "t" (borrow $ti)) (param "duration-ns" u64)))))

  ;; ----- entropy: the API only (no seeded-config import) ------------------------------------
  (import "eo9:entropy/types@0.1.0" (instance $entropy-types
    (export "entropy-impl" (type (sub resource)))))
  (alias export $entropy-types "entropy-impl" (type $entropy-impl))
  (import "eo9:entropy/entropy@0.1.0" (instance $entropy
    (export "entropy-impl" (type $ei (eq $entropy-impl)))
    (export "default" (func (result (own $ei))))
    (export "get-u64" (func (param "e" (borrow $ei)) (result u64)))))

  ;; ----- text: left residual so the ambient text provider captures the output --------------
  (import "eo9:text/types@0.1.0" (instance $text-types
    (export "text-impl" (type (sub resource)))))
  (alias export $text-types "text-impl" (type $text-impl))
  (import "eo9:text/text@0.1.0" (instance $text
    (export "text-impl" (type $txi (eq $text-impl)))
    (type $output-stream-def (enum "out" "err"))
    (export "output-stream" (type $output-stream (eq $output-stream-def)))
    (type $text-error-def (variant (case "closed") (case "io" string)))
    (export "text-error" (type $text-error (eq $text-error-def)))
    (export "default" (func (result (own $txi))))
    (export "write" (func
      (param "t" (borrow $txi))
      (param "to" $output-stream)
      (param "text" string)
      (result (result (error $text-error)))))))

  ;; ----- libc: memory, bump realloc, string constants --------------------------------------
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (data (i32.const 16) "invoker-env-ok")
    (func (export "realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  ;; ----- lowered imports --------------------------------------------------------------------
  (alias export $time "default" (func $time-default))
  (alias export $time "now" (func $now))
  (alias export $time "monotonic-now" (func $monotonic-now))
  (alias export $time "sleep" (func $sleep))
  (alias export $entropy "default" (func $entropy-default))
  (alias export $entropy "get-u64" (func $get-u64))
  (alias export $text "default" (func $text-default))
  (alias export $text "write" (func $text-write))

  (core func $time-default-lowered (canon lower (func $time-default)))
  (core func $now-lowered (canon lower (func $now) (memory $libc "memory")))
  (core func $monotonic-now-lowered (canon lower (func $monotonic-now)))
  (core func $sleep-lowered (canon lower (func $sleep)))
  (core func $entropy-default-lowered (canon lower (func $entropy-default)))
  (core func $get-u64-lowered (canon lower (func $get-u64)))
  (core func $text-default-lowered (canon lower (func $text-default)))
  (core func $text-write-lowered (canon lower (func $text-write)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $task-return (canon task.return (result u64)))

  ;; ----- the program --------------------------------------------------------------------------
  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "time-default" (func $time-default (result i32)))
    ;; now(handle, retptr): datetime { seconds: s64 @0, nanoseconds: u32 @8 }
    (import "host" "now" (func $now (param i32 i32)))
    (import "host" "monotonic-now" (func $monotonic-now (param i32) (result i64)))
    ;; sleep(handle, duration-ns)
    (import "host" "sleep" (func $sleep (param i32 i64)))
    (import "host" "entropy-default" (func $entropy-default (result i32)))
    (import "host" "get-u64" (func $get-u64 (param i32) (result i64)))
    (import "host" "text-default" (func $text-default (result i32)))
    ;; write(handle, stream, text-ptr, text-len, retptr)
    (import "host" "write" (func $write (param i32 i32 i32 i32 i32)))
    (import "host" "task-return" (func $task-return (param i64)))

    (func (export "main")
      (local $t i32) (local $e i32) (local $txt i32)
      (local $x i64) (local $y i64)

      ;; 1. observe the clock the invoker configured
      (local.set $t (call $time-default))
      (call $now (local.get $t) (i32.const 256))

      ;; 2. sleep through it -- on a frozen clock the wait is over immediately, and the
      ;;    call completing at all proves the configured provider forwards its async API
      (call $sleep (local.get $t) (i64.const 1000000))

      ;; 3. two samples from the seeded entropy
      (local.set $e (call $entropy-default))
      (local.set $x (call $get-u64 (local.get $e)))
      (local.set $y (call $get-u64 (local.get $e)))

      ;; 4. observable output: the fixed line plus one entropy-derived character
      (local.set $txt (call $text-default))
      (call $write (local.get $txt) (i32.const 0) (i32.const 16) (i32.const 14) (i32.const 320))
      (i32.store8 (i32.const 48)
        (i32.add (i32.const 97) (i32.and (i32.wrap_i64 (local.get $x)) (i32.const 15))))
      (call $write (local.get $txt) (i32.const 0) (i32.const 48) (i32.const 1) (i32.const 320))

      ;; 5. the outcome folds every observation
      (call $task-return
        (i64.add
          (i64.add (i64.xor (local.get $x) (local.get $y)) (i64.load (i32.const 256)))
          (call $monotonic-now (local.get $t))))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "time-default" (func $time-default-lowered))
      (export "now" (func $now-lowered))
      (export "monotonic-now" (func $monotonic-now-lowered))
      (export "sleep" (func $sleep-lowered))
      (export "entropy-default" (func $entropy-default-lowered))
      (export "get-u64" (func $get-u64-lowered))
      (export "text-default" (func $text-default-lowered))
      (export "write" (func $text-write-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u64) (canon lift (core func $i "main") async))
)
"#
}

// -----------------------------------------------------------------------------------------
// The deterministic-environment fixture (raw component WAT, milestone 2 / gate I2)
// -----------------------------------------------------------------------------------------

/// The wall-clock seconds [`det_env_guest`] binds through `eo9:time/frozen-config`.
pub const DET_ENV_FROZEN_SECONDS: i64 = 1111;
/// The monotonic reading [`det_env_guest`] binds through `eo9:time/frozen-config`.
pub const DET_ENV_FROZEN_MONOTONIC_NS: u64 = 2222;
/// The seed [`det_env_guest`] binds through `eo9:entropy/seeded-config`.
pub const DET_ENV_SEED: u64 = 7777;
/// The fixed line [`det_env_guest`] writes to stdout (followed by one entropy-derived
/// character).
pub const DET_ENV_OUTPUT_LINE: &str = "det-env-ok";

/// The guest of the deterministic-environment suite: a binary that imports the real
/// `eo9:time`, `eo9:entropy`, `eo9:fs`, and `eo9:text` interfaces *plus the stub
/// config interfaces* (`frozen-config`, `seeded-config`, `memfs-config`).
///
/// Its async `main`:
/// 1. binds the three stub configurations through their config interfaces (the constants
///    above — today the config interfaces are ordinary capability imports, because the
///    algebra has no compose-time `configure` binding yet; see plan/13-tests.md Decisions),
/// 2. checks that the frozen clock serves exactly the configured instant,
/// 3. draws two samples from the seeded entropy,
/// 4. creates and stats a directory in the memfs (and checks a missing path reports
///    `not-found`),
/// 5. writes [`DET_ENV_OUTPUT_LINE`] plus one entropy-derived character through `eo9:text`
///    (left residual, so the ambient text provider captures it), and
/// 6. returns `x ^ y` over the two entropy samples.
///
/// Any failed internal check returns a small sentinel (1..=12) instead, so a broken stub
/// shows up as a distinct outcome value rather than a trap.
pub fn det_env_guest() -> Component {
    let bytes = wat::parse_str(det_env_wat())
        .expect("det-env fixture must be valid component WAT")
        .to_vec();
    Component::load(bytes).expect("det-env fixture should load")
}

fn det_env_wat() -> String {
    format!(
        r#"
(component
  ;; ----- time: the API, and time.frozen's config interface --------------------------------
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "time-impl" (type $ti (eq $time-impl)))
    (type $datetime-def (record (field "seconds" s64) (field "nanoseconds" u32)))
    (export "datetime" (type $datetime (eq $datetime-def)))
    (type $instant-def (record (field "nanoseconds" u64)))
    (export "instant" (type $instant (eq $instant-def)))
    (export "default" (func (result (own $ti))))
    (export "now" (func (param "t" (borrow $ti)) (result $datetime)))
    (export "monotonic-now" (func (param "t" (borrow $ti)) (result $instant)))))
  (import "eo9:time/frozen-config@0.1.0" (instance $frozen-config
    (export "time-impl" (type $tfc (eq $time-impl)))
    (export "configure" (func async (param "now-seconds" s64) (param "monotonic-ns" u64)
      (result (result (own $tfc) (error string)))))))

  ;; ----- entropy: the API, and entropy.seeded's config interface --------------------------
  (import "eo9:entropy/types@0.1.0" (instance $entropy-types
    (export "entropy-impl" (type (sub resource)))))
  (alias export $entropy-types "entropy-impl" (type $entropy-impl))
  (import "eo9:entropy/entropy@0.1.0" (instance $entropy
    (export "entropy-impl" (type $ei (eq $entropy-impl)))
    (export "default" (func (result (own $ei))))
    (export "get-u64" (func (param "e" (borrow $ei)) (result u64)))))
  (import "eo9:entropy/seeded-config@0.1.0" (instance $seeded-config
    (export "entropy-impl" (type $eic (eq $entropy-impl)))
    (export "configure" (func async (param "seed" u64)
      (result (result (own $eic) (error string)))))))

  ;; ----- fs: the API (narrowed), and fs.memfs's config interface ---------------------------
  (import "eo9:fs/types@0.1.0" (instance $fs-types
    (export "fs-impl" (type (sub resource)))))
  (alias export $fs-types "fs-impl" (type $fs-impl))
  (import "eo9:fs/fs@0.1.0" (instance $fs
    (export "fs-impl" (type $fsi (eq $fs-impl)))
    (type $fs-error-def (variant
      (case "not-found") (case "already-exists") (case "not-a-directory")
      (case "is-a-directory") (case "denied") (case "read-only") (case "no-space")
      (case "not-immutable") (case "io" string)))
    (export "fs-error" (type $fs-error (eq $fs-error-def)))
    (type $node-kind-def (enum "file" "directory"))
    (export "node-kind" (type $node-kind (eq $node-kind-def)))
    (type $node-stat-def (record (field "kind" $node-kind) (field "size" u64)))
    (export "node-stat" (type $node-stat (eq $node-stat-def)))
    (export "default" (func (result (own $fsi))))
    (export "create-directory" (func async (param "fs" (borrow $fsi)) (param "path" string)
      (result (result (error $fs-error)))))
    (export "stat" (func async (param "fs" (borrow $fsi)) (param "path" string)
      (result (result $node-stat (error $fs-error)))))))
  (import "eo9:fs/memfs-config@0.1.0" (instance $memfs-config
    (export "fs-impl" (type $fsic (eq $fs-impl)))
    (export "configure" (func async
      (result (result (own $fsic) (error string)))))))

  ;; ----- text: left residual so the ambient text provider captures the output --------------
  (import "eo9:text/types@0.1.0" (instance $text-types
    (export "text-impl" (type (sub resource)))))
  (alias export $text-types "text-impl" (type $text-impl))
  (import "eo9:text/text@0.1.0" (instance $text
    (export "text-impl" (type $txi (eq $text-impl)))
    (type $output-stream-def (enum "out" "err"))
    (export "output-stream" (type $output-stream (eq $output-stream-def)))
    (type $text-error-def (variant (case "closed") (case "io" string)))
    (export "text-error" (type $text-error (eq $text-error-def)))
    (export "default" (func (result (own $txi))))
    (export "write" (func
      (param "t" (borrow $txi))
      (param "to" $output-stream)
      (param "text" string)
      (result (result (error $text-error)))))))

  ;; ----- libc: memory, bump realloc, string constants --------------------------------------
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 4096))
    (data (i32.const 16) "scratch")
    (data (i32.const 32) "missing")
    (data (i32.const 48) "det-env-ok")
    (func (export "realloc") (param $old i32) (param $old-size i32) (param $align i32) (param $new-size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new-size)))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))

  ;; ----- lowered imports --------------------------------------------------------------------
  (alias export $frozen-config "configure" (func $configure-frozen))
  (alias export $seeded-config "configure" (func $configure-seeded))
  (alias export $memfs-config "configure" (func $configure-memfs))
  (alias export $time "default" (func $time-default))
  (alias export $time "now" (func $now))
  (alias export $time "monotonic-now" (func $monotonic-now))
  (alias export $entropy "default" (func $entropy-default))
  (alias export $entropy "get-u64" (func $get-u64))
  (alias export $fs "default" (func $fs-default))
  (alias export $fs "create-directory" (func $create-dir))
  (alias export $fs "stat" (func $stat))
  (alias export $text "default" (func $text-default))
  (alias export $text "write" (func $text-write))

  (core func $configure-frozen-lowered (canon lower (func $configure-frozen)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $configure-seeded-lowered (canon lower (func $configure-seeded)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $configure-memfs-lowered (canon lower (func $configure-memfs)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $time-default-lowered (canon lower (func $time-default)))
  (core func $now-lowered (canon lower (func $now) (memory $libc "memory")))
  (core func $monotonic-now-lowered (canon lower (func $monotonic-now)))
  (core func $entropy-default-lowered (canon lower (func $entropy-default)))
  (core func $get-u64-lowered (canon lower (func $get-u64)))
  (core func $fs-default-lowered (canon lower (func $fs-default)))
  (core func $create-dir-lowered (canon lower (func $create-dir)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $stat-lowered (canon lower (func $stat)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $text-default-lowered (canon lower (func $text-default)))
  (core func $text-write-lowered (canon lower (func $text-write)
    (memory $libc "memory") (realloc (func $libc "realloc"))))
  (core func $task-return (canon task.return (result u64)))

  ;; ----- the program --------------------------------------------------------------------------
  (core module $m
    (import "libc" "memory" (memory 1))
    ;; configure(now-seconds, monotonic-ns, retptr) / (seed, retptr) / (retptr)
    (import "host" "configure-frozen" (func $configure-frozen (param i64 i64 i32)))
    (import "host" "configure-seeded" (func $configure-seeded (param i64 i32)))
    (import "host" "configure-memfs" (func $configure-memfs (param i32)))
    (import "host" "time-default" (func $time-default (result i32)))
    ;; now(handle, retptr): datetime {{ seconds: s64 @0, nanoseconds: u32 @8 }}
    (import "host" "now" (func $now (param i32 i32)))
    (import "host" "monotonic-now" (func $monotonic-now (param i32) (result i64)))
    (import "host" "entropy-default" (func $entropy-default (result i32)))
    (import "host" "get-u64" (func $get-u64 (param i32) (result i64)))
    (import "host" "fs-default" (func $fs-default (result i32)))
    ;; create-directory(handle, path-ptr, path-len, retptr) / stat(handle, path-ptr, path-len, retptr)
    (import "host" "create-directory" (func $create-dir (param i32 i32 i32 i32)))
    (import "host" "stat" (func $stat (param i32 i32 i32 i32)))
    (import "host" "text-default" (func $text-default (result i32)))
    ;; write(handle, stream, text-ptr, text-len, retptr)
    (import "host" "write" (func $write (param i32 i32 i32 i32 i32)))
    (import "host" "task-return" (func $task-return (param i64)))

    (func (export "main")
      (local $t i32) (local $e i32) (local $f i32) (local $txt i32)
      (local $x i64) (local $y i64)

      ;; 1. bind the three stub configurations through their config interfaces
      (call $configure-frozen (i64.const {frozen_seconds}) (i64.const {frozen_monotonic}) (i32.const 256))
      (if (i32.load8_u (i32.const 256))
        (then (call $task-return (i64.const 1)) (return)))
      (call $configure-seeded (i64.const {seed}) (i32.const 288))
      (if (i32.load8_u (i32.const 288))
        (then (call $task-return (i64.const 2)) (return)))
      (call $configure-memfs (i32.const 320))
      (if (i32.load8_u (i32.const 320))
        (then (call $task-return (i64.const 3)) (return)))

      ;; 2. the frozen clock serves exactly the configured instant
      (local.set $t (call $time-default))
      (call $now (local.get $t) (i32.const 352))
      (if (i64.ne (i64.load (i32.const 352)) (i64.const {frozen_seconds}))
        (then (call $task-return (i64.const 4)) (return)))
      (if (i64.ne (call $monotonic-now (local.get $t)) (i64.const {frozen_monotonic}))
        (then (call $task-return (i64.const 5)) (return)))

      ;; 3. two samples from the seeded entropy
      (local.set $e (call $entropy-default))
      (local.set $x (call $get-u64 (local.get $e)))
      (local.set $y (call $get-u64 (local.get $e)))
      (if (i64.eq (local.get $x) (local.get $y))
        (then (call $task-return (i64.const 6)) (return)))

      ;; 4. the memfs is real: create "scratch", stat it, and miss "missing"
      (local.set $f (call $fs-default))
      (call $create-dir (local.get $f) (i32.const 16) (i32.const 7) (i32.const 384))
      (if (i32.load8_u (i32.const 384))
        (then (call $task-return (i64.const 7)) (return)))
      ;; stat result: discriminant @0; payload @8 (node-stat kind @8, size @16 / fs-error disc @8)
      (call $stat (local.get $f) (i32.const 16) (i32.const 7) (i32.const 416))
      (if (i32.load8_u (i32.const 416))
        (then (call $task-return (i64.const 8)) (return)))
      (if (i32.ne (i32.load8_u (i32.const 424)) (i32.const 1))
        (then (call $task-return (i64.const 9)) (return)))
      (if (i64.ne (i64.load (i32.const 432)) (i64.const 0))
        (then (call $task-return (i64.const 10)) (return)))
      (call $stat (local.get $f) (i32.const 32) (i32.const 7) (i32.const 448))
      (if (i32.eqz (i32.load8_u (i32.const 448)))
        (then (call $task-return (i64.const 11)) (return)))
      (if (i32.load8_u (i32.const 456))
        (then (call $task-return (i64.const 12)) (return)))

      ;; 5. observable output: the fixed line plus one entropy-derived character
      (local.set $txt (call $text-default))
      (call $write (local.get $txt) (i32.const 0) (i32.const 48) (i32.const 10) (i32.const 480))
      (i32.store8 (i32.const 64)
        (i32.add (i32.const 97) (i32.and (i32.wrap_i64 (local.get $x)) (i32.const 15))))
      (call $write (local.get $txt) (i32.const 0) (i32.const 64) (i32.const 1) (i32.const 480))

      ;; 6. the outcome folds the two entropy samples
      (call $task-return (i64.xor (local.get $x) (local.get $y)))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "configure-frozen" (func $configure-frozen-lowered))
      (export "configure-seeded" (func $configure-seeded-lowered))
      (export "configure-memfs" (func $configure-memfs-lowered))
      (export "time-default" (func $time-default-lowered))
      (export "now" (func $now-lowered))
      (export "monotonic-now" (func $monotonic-now-lowered))
      (export "entropy-default" (func $entropy-default-lowered))
      (export "get-u64" (func $get-u64-lowered))
      (export "fs-default" (func $fs-default-lowered))
      (export "create-directory" (func $create-dir-lowered))
      (export "stat" (func $stat-lowered))
      (export "text-default" (func $text-default-lowered))
      (export "write" (func $text-write-lowered))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u64) (canon lift (core func $i "main") async))
)
"#,
        frozen_seconds = DET_ENV_FROZEN_SECONDS,
        frozen_monotonic = DET_ENV_FROZEN_MONOTONIC_NS,
        seed = DET_ENV_SEED,
    )
}

// -----------------------------------------------------------------------------------------
// Runtime-rule fixtures (milestone 2): io-buffer caps and the optional-import loader rule
// -----------------------------------------------------------------------------------------

/// A binary that constructs `count` io buffers of `len` bytes each and returns `count`.
/// Used to exercise the runtime's per-buffer and per-task buffer caps: an over-cap request
/// must fail with a clean in-band error (a trap naming the cap), never by growing host
/// memory. Raw WAT, compiled directly by `Image::compile`.
pub fn buffer_hog_wat() -> &'static str {
    r#"
(component
  (import "eo9:io/buffers@0.1.0" (instance $buffers
    (export "buffer" (type $buffer (sub resource)))
    (export "[constructor]buffer" (func (param "len" u64) (result (own $buffer))))))
  (alias export $buffers "[constructor]buffer" (func $new))
  (core func $new-lowered (canon lower (func $new)))
  (core module $m
    (import "host" "new" (func $new (param i64) (result i32)))
    (func (export "main") (param $len i64) (param $count i32) (result i32)
      (local $i i32)
      (block $done
        (loop $again
          (br_if $done (i32.ge_u (local.get $i) (local.get $count)))
          (drop (call $new (local.get $len)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $again)))
      (local.get $count)))
  (core instance $i (instantiate $m
    (with "host" (instance (export "new" (func $new-lowered))))))
  (func (export "main") (param "len" u64) (param "count" u32) (result u32)
    (canon lift (core func $i "main")))
)
"#
}

/// A binary importing only the real `eo9:entropy/entropy-optional` flavor; `main` returns
/// `ok(1)` when the capability is present and `ok(0)` when it observes absence. Used to
/// exercise the runtime's loader rule for optional imports (auto-sealing at spawn).
pub fn optional_entropy_probe() -> Component {
    const WIT: &str = r#"
package eo9-tests:optional@0.1.0;

/// A binary that merely *can use* entropy: presence or absence is observed through the
/// `-optional` import's own type.
world entropy-probe {
    import eo9:entropy/entropy-optional@0.1.0;
    export main: func() -> result<u32, string>;
}
"#;
    const CORE: &str = r#"
(module
  (import "eo9:entropy/entropy-optional@0.1.0" "default" (func $default (param i32)))
  (memory (export "memory") 1)
  (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024))
  (func (export "main") (result i32)
    ;; option<entropy-impl> is written to 16 by the import; its discriminant is the answer
    (call $default (i32.const 16))
    (i32.store8 (i32.const 32) (i32.const 0))
    (i32.store (i32.const 36) (i32.load8_u (i32.const 16)))
    (i32.const 32)))
"#;
    build_component(WIT, &["entropy"], "entropy-probe", CORE)
}
