//! The session resolve cache: skip re-reading `/bin` and re-running the algebra for
//! compositions the session has already built.
//!
//! Two layers, both session-scoped and bounded:
//!
//! * a **bytes cache** (name → component bytes): a repeated program name loads from the
//!   cached bytes instead of re-reading `/bin/<name>.wasm` through the filesystem;
//! * an **image cache** (canonical run key → compiled image + bound arguments): a
//!   repeated command line skips resolution, the algebra, and `compile` entirely and
//!   goes straight to `spawn`.
//!
//! The image key is the **structural identity** of the run (the owner's granularity
//! ruling, mirroring the kernel's fusion-graph hash): the canonicalized expression tree
//! with `let` bindings substituted by the sub-key frozen at bind time and `/bin` leaves
//! tagged with generation counters. Spelling, whitespace, parenthesization, and binding
//! *names* do not appear in the key — `let e = X` then `e $ hello` and the inline
//! `X $ hello` produce the same key. Atoms are length-prefixed (netstring style), so no
//! input text can fake the tree structure.
//!
//! Invalidation is structural and eager-but-conservative (a stale resolve serving old
//! bytes would be a correctness bug, so when in doubt the caches re-resolve):
//!
//! | event | effect |
//! |---|---|
//! | `save <name>` succeeds | `<name>`'s generation bumps (keys containing that `/bin` leaf miss); its bytes entry drops |
//! | a run whose program imports `eo9:fs` completes | the global generation bumps (every `/bin` leaf key misses) and the bytes cache clears — the program could have rewritten `/bin` and the filesystem cannot tell us |
//! | `let` rebinds a name | the binding's frozen sub-key is replaced; entries built from the old value become unreachable; unrelated entries are untouched |
//! | `detach` of a child that imports `eo9:fs` | both caches are disabled for the rest of the session (the service is a concurrent writer we cannot observe) |
//!
//! `let` bindings hold values captured at bind time, so their sub-keys deliberately
//! freeze the generations seen then: a later `save` over a leaf they were built from
//! changes the *inline* spelling's key but not the binding's — exactly the algebra's
//! semantics.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ast::{Arg, ArgValue, Expr};
use crate::backend::{ComponentInfo, NamedArg};
use crate::manual::Manual;

/// Bytes-cache bounds: entries and total byte budget (components run tens to a few
/// hundred KiB; the budget keeps the browser blob comfortable).
const BYTES_MAX_ENTRIES: usize = 16;
const BYTES_MAX_TOTAL: usize = 4 * 1024 * 1024;

/// Image-cache bound. Deliberately small: a cached entry retains a live image handle
/// in the embedder's exec provider, and that table is a bounded shared resource
/// (usermode `MAX_IMAGES` is 16, shared with detached services' respawn churn).
/// 4 keeps the repeated-prompt-line win while leaving the table mostly free.
const IMAGES_MAX_ENTRIES: usize = 4;

/// Argument-memo bound (entries are a `ComponentInfo` plus a parsed manual — a few
/// KiB each at most; 32 comfortably covers a session's working set of program names).
const ARGS_MAX_ENTRIES: usize = 32;

/// One memoized argument-completion entry (the repl M3 lazy memo,
/// docs/design/component-manuals.md §4): what `describe` reported and, when the
/// component carries one, its parsed manual — both derived from the same bytes the
/// [`BytesCache`] holds, so the entry lives and dies by the same structural rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgMemoEntry {
    pub info: ComponentInfo,
    pub manual: Option<Manual>,
}

/// The bytes half: name → component bytes, LRU, eagerly invalidated.
pub struct BytesCache {
    /// Most-recently-used last.
    entries: Vec<(String, Vec<u8>)>,
    total: usize,
    disabled: bool,
}

impl BytesCache {
    fn new() -> Self {
        BytesCache {
            entries: Vec::new(),
            total: 0,
            disabled: false,
        }
    }

    /// The cached bytes for `name`, refreshing its LRU position.
    pub fn get(&mut self, name: &str) -> Option<&[u8]> {
        if self.disabled {
            return None;
        }
        let index = self.entries.iter().position(|(n, _)| n == name)?;
        let entry = self.entries.remove(index);
        self.entries.push(entry);
        self.entries.last().map(|(_, bytes)| bytes.as_slice())
    }

    /// Remember `name`'s bytes (evicting least-recently-used entries past the bounds).
    pub fn insert(&mut self, name: &str, bytes: Vec<u8>) {
        if self.disabled || bytes.len() > BYTES_MAX_TOTAL {
            return;
        }
        self.remove(name);
        self.total += bytes.len();
        self.entries.push((String::from(name), bytes));
        while self.entries.len() > BYTES_MAX_ENTRIES || self.total > BYTES_MAX_TOTAL {
            let (_, evicted) = self.entries.remove(0);
            self.total -= evicted.len();
        }
    }

    fn remove(&mut self, name: &str) {
        if let Some(index) = self.entries.iter().position(|(n, _)| n == name) {
            let (_, bytes) = self.entries.remove(index);
            self.total -= bytes.len();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
    }

    fn disable(&mut self) {
        self.clear();
        self.disabled = true;
    }
}

/// One cached image: the canonical key, the compiled image, the bound (completed)
/// `main` arguments, and whether the program imports `eo9:fs` (so a hit knows to bump
/// the store generation after running, same as the miss path).
struct ImageEntry<I> {
    key: String,
    image: I,
    args: Vec<NamedArg>,
    imports_fs: bool,
}

/// The whole session cache. `I` is the backend's image type.
pub struct SessionCache<I> {
    pub bytes: BytesCache,
    images: Vec<ImageEntry<I>>,
    /// The argument-completion memo (name → describe + manual), LRU like the bytes
    /// half and invalidated by exactly the same structural events.
    args: Vec<(String, ArgMemoEntry)>,
    /// Bumped when any program that *could* have written `/bin` ran.
    global_gen: u64,
    /// Per-name bumps from this session's own `save`s.
    name_gens: BTreeMap<String, u64>,
    /// `let` name → the sub-key frozen at bind time.
    binding_keys: BTreeMap<String, String>,
    /// Fallback identity counter for bindings whose expression we could not canonicalize.
    next_unique: u64,
    disabled: bool,
}

impl<I> SessionCache<I> {
    pub fn new() -> Self {
        SessionCache {
            bytes: BytesCache::new(),
            images: Vec::new(),
            args: Vec::new(),
            global_gen: 0,
            name_gens: BTreeMap::new(),
            binding_keys: BTreeMap::new(),
            next_unique: 0,
            disabled: false,
        }
    }

    /// The canonical key for running `expr` in this session, or `None` when caching is
    /// off. Pure (no backend calls): names that turn out unresolvable simply produce
    /// keys that never hit.
    pub fn run_key(&self, expr: &Expr, has_environment: bool) -> Option<String> {
        if self.disabled {
            return None;
        }
        let env = if has_environment { "env" } else { "noenv" };
        Some(format!("{}|{env}", self.canon(expr)))
    }

    /// The frozen sub-key for a new `let` binding of `expr`.
    pub fn record_binding(&mut self, name: &str, expr: &Expr) {
        let key = if self.disabled {
            self.next_unique += 1;
            format!("U({})", self.next_unique)
        } else {
            self.canon(expr)
        };
        self.binding_keys.insert(String::from(name), key);
    }

    /// The cached image + arguments for `key`, refreshing its LRU position. The bool is
    /// the entry's `imports_fs` flag.
    pub fn image_get(&mut self, key: &str) -> Option<(&I, &[NamedArg], bool)> {
        if self.disabled {
            return None;
        }
        let index = self.images.iter().position(|entry| entry.key == key)?;
        let entry = self.images.remove(index);
        self.images.push(entry);
        self.images
            .last()
            .map(|entry| (&entry.image, entry.args.as_slice(), entry.imports_fs))
    }

    /// Remember a successfully spawned run.
    pub fn image_insert(&mut self, key: String, image: I, args: Vec<NamedArg>, imports_fs: bool) {
        if self.disabled {
            return;
        }
        if let Some(index) = self.images.iter().position(|entry| entry.key == key) {
            self.images.remove(index);
        }
        self.images.push(ImageEntry {
            key,
            image,
            args,
            imports_fs,
        });
        while self.images.len() > IMAGES_MAX_ENTRIES {
            self.images.remove(0);
        }
    }

    /// The memoized argument-completion entry for `name`, refreshing its LRU position.
    pub fn args_get(&mut self, name: &str) -> Option<&ArgMemoEntry> {
        if self.disabled {
            return None;
        }
        let index = self.args.iter().position(|(n, _)| n == name)?;
        let entry = self.args.remove(index);
        self.args.push(entry);
        self.args.last().map(|(_, entry)| entry)
    }

    /// Memoize one argument-completion entry (evicting least-recently-used past the
    /// bound).
    pub fn args_insert(&mut self, name: &str, entry: ArgMemoEntry) {
        if self.disabled {
            return;
        }
        if let Some(index) = self.args.iter().position(|(n, _)| n == name) {
            self.args.remove(index);
        }
        self.args.push((String::from(name), entry));
        while self.args.len() > ARGS_MAX_ENTRIES {
            self.args.remove(0);
        }
    }

    /// This session wrote `/bin/<name>.wasm` (`save`): that leaf's keys change; its
    /// bytes entry drops — and so does its argument memo (same bytes, same rules).
    /// Unrelated entries are untouched (structural invalidation).
    pub fn note_bin_write(&mut self, name: &str) {
        *self.name_gens.entry(String::from(name)).or_insert(0) += 1;
        self.bytes.remove(name);
        if let Some(index) = self.args.iter().position(|(n, _)| n == name) {
            self.args.remove(index);
        }
    }

    /// A program that imports `eo9:fs` finished running: it could have rewritten any
    /// `/bin` entry, and the filesystem gives us no content identity to check — so
    /// every `/bin` leaf's keys change, the bytes cache clears, and the argument memo
    /// clears with it.
    pub fn note_fs_run(&mut self) {
        self.global_gen += 1;
        self.bytes.clear();
        self.args.clear();
    }

    /// A detached service that imports `eo9:fs` is now a *concurrent* potential writer:
    /// no point-in-time invalidation is sound, so caching is off for the session.
    pub fn disable(&mut self) {
        self.disabled = true;
        self.images.clear();
        self.args.clear();
        self.bytes.disable();
    }

    /// Canonicalize an expression: structure tags + netstring atoms, bindings
    /// substituted by their frozen sub-keys, `/bin` leaves generation-tagged.
    fn canon(&self, expr: &Expr) -> String {
        match expr {
            Expr::Name(name) => match self.binding_keys.get(name) {
                Some(frozen) => frozen.clone(),
                None => {
                    let name_gen = self.name_gens.get(name).copied().unwrap_or(0);
                    format!("B({}@{}.{})", net(name), self.global_gen, name_gen)
                }
            },
            Expr::App { callee, args } => {
                let rendered: Vec<String> = args.iter().map(|arg| self.canon_arg(arg)).collect();
                format!("A({};{})", self.canon(callee), rendered.join(","))
            }
            Expr::Compose { provider, consumer } => {
                format!("C({},{})", self.canon(provider), self.canon(consumer))
            }
            Expr::Extend { base, layer } => {
                format!("X({},{})", self.canon(base), self.canon(layer))
            }
            Expr::Only { allow, body } => {
                let entries: Vec<String> = allow.iter().map(|entry| net(entry)).collect();
                format!("O([{}],{})", entries.join(","), self.canon(body))
            }
            Expr::Rename { from, to, body } => {
                format!("R({},{},{})", net(from), net(to), self.canon(body))
            }
            Expr::With { bindings, body } => {
                let rendered: Vec<String> = bindings
                    .iter()
                    .map(|binding| {
                        format!("{}={}", net(&binding.slot), self.canon(&binding.provider))
                    })
                    .collect();
                format!("W([{}],{})", rendered.join(","), self.canon(body))
            }
        }
    }

    fn canon_arg(&self, arg: &Arg) -> String {
        match arg {
            Arg::Flag { name, value } => format!("F({},{})", net(name), self.canon_value(value)),
            Arg::Positional(value) => format!("P({})", self.canon_value(value)),
        }
    }

    fn canon_value(&self, value: &ArgValue) -> String {
        match value {
            ArgValue::Word(text) => format!("w{}", net(text)),
            ArgValue::Quoted(text) => format!("q{}", net(text)),
            ArgValue::Expr(expr) => format!("e({})", self.canon(expr)),
        }
    }
}

impl<I> Default for SessionCache<I> {
    fn default() -> Self {
        Self::new()
    }
}

/// Netstring-style atom: `<byte-len>:<text>`. Length-prefixing makes the canonical form
/// unambiguous no matter what characters user text contains.
fn net(text: &str) -> String {
    format!("{}:{}", text.len(), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_expr;

    fn cache() -> SessionCache<u32> {
        SessionCache::new()
    }

    fn key(cache: &SessionCache<u32>, src: &str) -> String {
        cache
            .run_key(&parse_expr(src).expect("parse"), false)
            .expect("enabled")
    }

    #[test]
    fn spelling_and_parens_do_not_change_the_key() {
        let cache = cache();
        assert_eq!(key(&cache, "a $ b $ c"), key(&cache, "a $ (b $ c)"));
        assert_eq!(key(&cache, "a   $  b"), key(&cache, "a $ b"));
    }

    #[test]
    fn binding_substitution_matches_the_inline_spelling() {
        let mut cache = cache();
        cache.record_binding(
            "e",
            &parse_expr("time.frozen --now-seconds 0").expect("parse"),
        );
        assert_eq!(
            key(&cache, "e $ hello"),
            key(&cache, "time.frozen --now-seconds 0 $ hello"),
        );
    }

    #[test]
    fn args_and_structure_are_unambiguous() {
        let cache = cache();
        // The classic injection shape: an argument value that *looks* like more tree.
        assert_ne!(key(&cache, "a --x \"b $ c\""), key(&cache, "a --x b $ c"));
        assert_ne!(key(&cache, "a $ b"), key(&cache, "a $ b $ b"));
    }

    #[test]
    fn save_bumps_exactly_that_name() {
        let mut cache = cache();
        let before_x = key(&cache, "x $ run");
        let before_y = key(&cache, "y $ run");
        cache.note_bin_write("x");
        assert_ne!(before_x, key(&cache, "x $ run"));
        assert_eq!(before_y, key(&cache, "y $ run"));
    }

    #[test]
    fn fs_runs_bump_every_bin_leaf_but_not_frozen_bindings() {
        let mut cache = cache();
        cache.record_binding("e", &parse_expr("time.frozen").expect("parse"));
        let bound = key(&cache, "e");
        let inline = key(&cache, "time.frozen");
        assert_eq!(bound, inline);
        cache.note_fs_run();
        // The inline spelling re-reads /bin, so its key moved; the binding captured its
        // value at bind time, so its key is frozen.
        assert_eq!(bound, key(&cache, "e"));
        assert_ne!(inline, key(&cache, "time.frozen"));
        // And in a larger expression, only the /bin leaves move: the two spellings
        // stay distinct from each other by exactly the frozen subtree.
        assert_ne!(key(&cache, "e $ hello"), key(&cache, "time.frozen $ hello"));
    }

    #[test]
    fn rebinding_replaces_the_frozen_key() {
        let mut cache = cache();
        cache.record_binding("e", &parse_expr("entropy.seeded").expect("parse"));
        let first = key(&cache, "e $ rng");
        cache.record_binding("e", &parse_expr("entropy.system").expect("parse"));
        assert_ne!(first, key(&cache, "e $ rng"));
    }

    #[test]
    fn image_lru_is_bounded() {
        let mut cache = cache();
        for index in 0..(IMAGES_MAX_ENTRIES + 3) {
            cache.image_insert(format!("k{index}"), index as u32, Vec::new(), false);
        }
        assert!(cache.image_get("k0").is_none());
        assert!(cache.image_get("k3").is_some());
    }

    #[test]
    fn bytes_cache_is_bounded_and_lru() {
        let mut cache = BytesCache::new();
        for index in 0..(BYTES_MAX_ENTRIES + 2) {
            cache.insert(&format!("n{index}"), alloc::vec![0u8; 8]);
        }
        assert!(cache.get("n0").is_none());
        assert!(cache.get("n2").is_some());
    }

    fn memo_entry() -> ArgMemoEntry {
        ArgMemoEntry {
            info: ComponentInfo {
                kind: crate::backend::ComponentKind::Binary,
                imports: Vec::new(),
                exports: Vec::new(),
                args: Vec::new(),
            },
            manual: None,
        }
    }

    #[test]
    fn args_memo_is_bounded_and_follows_the_structural_rules() {
        let mut cache = cache();
        cache.args_insert("telnetd", memo_entry());
        cache.args_insert("hello", memo_entry());
        assert!(cache.args_get("telnetd").is_some());
        // `save telnetd` drops exactly that entry.
        cache.note_bin_write("telnetd");
        assert!(cache.args_get("telnetd").is_none());
        assert!(cache.args_get("hello").is_some());
        // An fs-importing run clears the memo wholesale.
        cache.note_fs_run();
        assert!(cache.args_get("hello").is_none());
        // Bounded, LRU.
        for index in 0..40 {
            cache.args_insert(&format!("p{index}"), memo_entry());
        }
        assert!(cache.args_get("p0").is_none());
        assert!(cache.args_get("p39").is_some());
        // Disable turns it off entirely.
        cache.disable();
        assert!(cache.args_get("p39").is_none());
        cache.args_insert("late", memo_entry());
        assert!(cache.args_get("late").is_none());
    }

    #[test]
    fn disable_turns_everything_off() {
        let mut cache = cache();
        cache.image_insert(String::from("k"), 1, Vec::new(), false);
        cache.bytes.insert("n", alloc::vec![1, 2, 3]);
        cache.disable();
        assert!(cache.image_get("k").is_none());
        assert!(cache.bytes.get("n").is_none());
        assert!(
            cache
                .run_key(&parse_expr("a").expect("parse"), false)
                .is_none()
        );
        cache.image_insert(String::from("k2"), 2, Vec::new(), false);
        assert!(cache.image_get("k2").is_none());
    }
}
