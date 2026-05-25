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

/// A component whose `main` calls `eo9:time/time.sleep` and parks on the returned future
/// (Component Model async built-ins; `main` is async-lifted because sync-lifted exports
/// cannot block on the pinned runtime). With a provider whose sleep never resolves, the
/// task stays blocked on the provider future — exactly the state the kill/linearity
/// contract is about. Returns `9` if the sleep ever completes.
///
/// This is raw WAT rather than a WIT-built fixture because it needs the async canonical
/// built-ins directly; `eo9_runtime::Image::compile` accepts WAT text.
pub fn sleeper_wat() -> &'static str {
    r#"
(component
  (import "eo9:time/types@0.1.0" (instance $time-types
    (export "time-impl" (type (sub resource)))))
  (alias export $time-types "time-impl" (type $time-impl))
  (import "eo9:time/time@0.1.0" (instance $time
    (export "default" (func (result (own $time-impl))))
    (export "sleep" (func (param "t" (borrow $time-impl)) (param "duration-ns" u64) (result (future))))))

  (core module $libc (memory (export "memory") 1))
  (core instance $libc (instantiate $libc))

  (alias export $time "default" (func $default))
  (alias export $time "sleep" (func $sleep))

  (type $ft (future))
  (core func $default-lowered (canon lower (func $default)))
  (core func $sleep-lowered (canon lower (func $sleep)))
  (core func $future-read (canon future.read $ft async (memory $libc "memory")))
  (core func $ws-new (canon waitable-set.new))
  (core func $ws-join (canon waitable.join))
  (core func $ws-wait (canon waitable-set.wait (memory $libc "memory")))
  (core func $future-drop (canon future.drop-readable $ft))
  (core func $ws-drop (canon waitable-set.drop))
  (core func $task-return (canon task.return (result u32)))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "host" "default" (func $default (result i32)))
    (import "host" "sleep" (func $sleep (param i32 i64) (result i32)))
    (import "host" "future-read" (func $future-read (param i32 i32) (result i32)))
    (import "host" "waitable-set-new" (func $ws-new (result i32)))
    (import "host" "waitable-join" (func $ws-join (param i32 i32)))
    (import "host" "waitable-set-wait" (func $ws-wait (param i32 i32) (result i32)))
    (import "host" "future-drop" (func $future-drop (param i32)))
    (import "host" "waitable-set-drop" (func $ws-drop (param i32)))
    (import "host" "task-return" (func $task-return (param i32)))

    (func (export "main")
      (local $h i32) (local $f i32) (local $ws i32) (local $status i32)
      (local.set $h (call $default))
      ;; ask for a one-hour sleep; the test provider never resolves it anyway
      (local.set $f (call $sleep (local.get $h) (i64.const 3600000000000)))
      (local.set $status (call $future-read (local.get $f) (i32.const 16)))
      (if (i32.eq (local.get $status) (i32.const -1))
        (then
          (local.set $ws (call $ws-new))
          (call $ws-join (local.get $f) (local.get $ws))
          (drop (call $ws-wait (local.get $ws) (i32.const 32)))
          (call $ws-join (local.get $f) (i32.const 0))
          (call $ws-drop (local.get $ws))))
      (call $future-drop (local.get $f))
      (call $task-return (i32.const 9))))

  (core instance $i (instantiate $m
    (with "libc" (instance $libc))
    (with "host" (instance
      (export "default" (func $default-lowered))
      (export "sleep" (func $sleep-lowered))
      (export "future-read" (func $future-read))
      (export "waitable-set-new" (func $ws-new))
      (export "waitable-join" (func $ws-join))
      (export "waitable-set-wait" (func $ws-wait))
      (export "future-drop" (func $future-drop))
      (export "waitable-set-drop" (func $ws-drop))
      (export "task-return" (func $task-return))))))

  (func (export "main") async (result u32) (canon lift (core func $i "main") async))
)
"#
}
