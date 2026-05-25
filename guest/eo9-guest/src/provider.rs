//! Provider-authoring support: helpers for guest crates that *export* eo9 APIs.
//!
//! A provider (see SPEC.md, "Composition and the `$` operator") exports one or more
//! `eo9:*` API interfaces plus a per-world config interface whose `configure` entry
//! binds the provider's configuration and returns its root capability handle. Provider
//! crates generate their own bindings with a plain `wit_bindgen::generate!` against
//! their stub world (the worlds live in the repo-level `wit/<api>` packages) and
//! implement the generated `Guest` traits; see `guest/stubs/*` for complete providers.
//!
//! What this module adds is the one piece every provider implementation needs:
//! [`ProviderState`], a `static`-friendly cell for the provider's shared state — bound
//! by `configure`, read by every operation and by `default()`. Exported resource types
//! are just tokens referring to that shared state: `default()` has to hand out the same
//! capability that `configure` returned, so the state cannot live inside any single
//! resource instance.
//!
//! Note (toolchain constraint, see plan/09-providers-stubs.md Decisions): a wasm guest
//! provider currently cannot implement an exported operation that returns a `future<T>`
//! it would have to complete itself — only `async func` exports may be async-lifted, so
//! a plain `func(...) -> future<T>` export has no live task left to deliver the value
//! after it returns. The shipped stubs therefore cover the interfaces whose operations
//! are synchronous; the rest are deferred pending that escalation.

use core::cell::RefCell;

/// A `static`-friendly cell holding a provider's shared state.
///
/// A provider's exported resources are just tokens: the state they operate on is shared
/// between the handle `configure` returns and the handle `default()` hands out, so it
/// lives in a `static` rather than inside any one resource instance. `ProviderState` is
/// that static: `configure` calls [`set`](Self::set), operations call
/// [`with`](Self::with).
///
/// Eo9 guest code is single-threaded by construction — shared-memory threading is a
/// capability (`eo9:threads`) that does not exist yet and is never granted to
/// deterministic environments (see SPEC.md, "Execution APIs") — so the `Sync`
/// implementation below is sound; re-entrant access is still caught at runtime by the
/// inner `RefCell`.
pub struct ProviderState<T> {
    inner: RefCell<Option<T>>,
}

// SAFETY: guest components run single-threaded (see the type docs); the cell is never
// actually shared across threads, `Sync` is only needed to put it in a `static`.
unsafe impl<T> Sync for ProviderState<T> {}

impl<T> ProviderState<T> {
    /// An empty, unconfigured state.
    pub const fn new() -> Self {
        Self {
            inner: RefCell::new(None),
        }
    }

    /// Bind the state. Called by `configure`; replaces any previous value.
    pub fn set(&self, value: T) {
        *self.inner.borrow_mut() = Some(value);
    }

    /// Whether the state has been bound.
    pub fn is_set(&self) -> bool {
        self.inner.borrow().is_some()
    }

    /// Run `f` with mutable access to the state.
    ///
    /// Panics (and therefore traps) if the provider has not been configured, or on
    /// re-entrant access — both are contract violations by the embedding, not
    /// recoverable conditions of the provider.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut guard = self.inner.borrow_mut();
        let state = guard
            .as_mut()
            .expect("provider used before `configure` bound its state");
        f(state)
    }
}

impl<T> Default for ProviderState<T> {
    fn default() -> Self {
        Self::new()
    }
}
