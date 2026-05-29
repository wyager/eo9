//! The exec provider: host-side state behind the `eo9:exec/*` interfaces.
//!
//! Holding this provider is what makes a guest a *native executor* (SPEC "Execution
//! APIs"): it grants the component algebra, codegen (`compile`), and the task surface
//! (`spawn`/`resume`/`runnable`/`wait`/`kill`). It is **not** granted by default —
//! `Providers::none()` leaves it out, exactly like fs — and the embedder that grants it
//! also decides, through [`ChildPolicy`], what root providers every child spawned through
//! it receives.
//!
//! Children are ordinary [`Task`]s held in a [`ChildSet`] shared between the exec provider
//! (inside the parent's store) and the parent `Task` itself: killing or dropping the
//! parent drops its children. Children **execute inside the parent's `resume`** (one fuel
//! slice per runnable child per parent quantum, paid from the parent's donation), because
//! wasmtime forbids running one store's event loop from inside another's; the guest-facing
//! `wait`/`runnable` host functions only observe child state and wake the parent. See
//! plan/04-runtime.md § Decisions (D11).

use std::sync::{Arc, Mutex};

use wasmtime::Engine;

use crate::image::Image;
use crate::providers::Providers;
use crate::task::Task;

/// The children spawned by one exec-holding task, shared between the exec provider (inside
/// the task's store, used by the `eo9:exec/task` host functions) and the parent
/// [`Task`](crate::task::Task) itself (outside the store, whose `resume` drives them).
/// Dropping the parent drops the set and with it every child.
pub(crate) type ChildSet = Arc<Mutex<Table<Task>>>;

/// How many loaded `component` handles one task may hold at once.
pub const MAX_COMPONENTS: u32 = 32;
/// Ceiling on the total bytes of all live `component` handles of one task.
pub const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
/// How many compiled `image` handles one task may hold at once.
pub const MAX_IMAGES: u32 = 16;
/// How many live child tasks one task may hold at once.
pub const MAX_CHILDREN: u32 = 8;

/// The embedder-supplied policy for children spawned through the exec provider.
///
/// A child never inherits the parent's host-side authority implicitly: it gets exactly
/// what its own (already-composed) image carries, plus whatever root providers this
/// policy hands out. The default policy grants nothing.
pub struct ChildPolicy {
    providers: Box<dyn FnMut() -> Providers + Send>,
    /// Embedder-supplied spawn hints: when a spawn fails and the failure text mentions
    /// the import prefix on the left, the advice on the right is appended to the error
    /// (e.g. `eo9:disk/` → "relaunch with --disk <image>"). The raw reason is never
    /// replaced, only annotated.
    spawn_hints: Vec<(String, String)>,
}

impl ChildPolicy {
    /// Children get no root providers at all (their composition is all they have).
    pub fn no_providers() -> Self {
        Self {
            providers: Box::new(Providers::none),
            spawn_hints: Vec::new(),
        }
    }

    /// Children get the root providers produced by `factory` (called once per spawn).
    pub fn with_providers(factory: impl FnMut() -> Providers + Send + 'static) -> Self {
        Self {
            providers: Box::new(factory),
            spawn_hints: Vec::new(),
        }
    }

    /// Add a spawn hint: when a spawn failure mentions `import_prefix`, `advice` is
    /// appended to the reported error. Embedders use it to point at the flag or command
    /// that grants the missing capability in *their* vocabulary.
    pub fn with_spawn_hint(
        mut self,
        import_prefix: impl Into<String>,
        advice: impl Into<String>,
    ) -> Self {
        self.spawn_hints.push((import_prefix.into(), advice.into()));
        self
    }

    pub(crate) fn providers_for_child(&mut self) -> Providers {
        (self.providers)()
    }

    /// The first hint whose import prefix appears in `error_text`, if any.
    pub(crate) fn hint_for(&self, error_text: &str) -> Option<&str> {
        self.spawn_hints
            .iter()
            .find(|(prefix, _)| error_text.contains(prefix.as_str()))
            .map(|(_, advice)| advice.as_str())
    }
}

impl Default for ChildPolicy {
    fn default() -> Self {
        Self::no_providers()
    }
}

/// A small slot table keyed by the Component Model resource `rep`.
pub(crate) struct Table<T> {
    slots: Vec<Option<T>>,
    cap: u32,
    what: &'static str,
}

impl<T> Table<T> {
    fn new(cap: u32, what: &'static str) -> Self {
        Self {
            slots: Vec::new(),
            cap,
            what,
        }
    }

    pub(crate) fn insert(&mut self, value: T) -> wasmtime::Result<u32> {
        let live = self.slots.iter().filter(|slot| slot.is_some()).count() as u32;
        if live >= self.cap {
            return Err(wasmtime::Error::msg(format!(
                "too many live {} handles (cap {})",
                self.what, self.cap
            )));
        }
        if let Some(index) = self.slots.iter().position(Option::is_none) {
            self.slots[index] = Some(value);
            return Ok(index as u32);
        }
        self.slots.push(Some(value));
        Ok((self.slots.len() - 1) as u32)
    }

    pub(crate) fn get_mut(&mut self, rep: u32) -> wasmtime::Result<&mut T> {
        self.slots
            .get_mut(rep as usize)
            .and_then(Option::as_mut)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown {} handle {rep}", self.what)))
    }

    pub(crate) fn take(&mut self, rep: u32) -> wasmtime::Result<T> {
        self.slots
            .get_mut(rep as usize)
            .and_then(Option::take)
            .ok_or_else(|| wasmtime::Error::msg(format!("unknown {} handle {rep}", self.what)))
    }

    pub(crate) fn free(&mut self, rep: u32) -> Option<T> {
        self.slots.get_mut(rep as usize).and_then(Option::take)
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(Option::as_mut)
    }
}

/// Host-side state of the `eo9:exec/*` capability for one task.
pub struct ExecProvider {
    pub(crate) engine: Engine,
    pub(crate) policy: ChildPolicy,
    pub(crate) components: Table<eo9_component::Component>,
    pub(crate) images: Table<Image>,
    pub(crate) children: ChildSet,
    pub(crate) component_bytes: u64,
}

impl ExecProvider {
    /// Create the exec capability for one task. `engine` is the engine codegen runs
    /// against (normally the same pinned engine the parent was compiled with); `policy`
    /// decides what root providers children get.
    pub fn new(engine: &Engine, policy: ChildPolicy) -> Self {
        Self {
            engine: engine.clone(),
            policy,
            components: Table::new(MAX_COMPONENTS, "component"),
            images: Table::new(MAX_IMAGES, "image"),
            children: Arc::new(Mutex::new(Table::new(MAX_CHILDREN, "task"))),
            component_bytes: 0,
        }
    }

    /// The shared child set (cloned into the parent [`Task`](crate::task::Task) at spawn so
    /// its `resume` can drive the children).
    pub(crate) fn child_set(&self) -> ChildSet {
        self.children.clone()
    }

    /// Account and insert a freshly loaded component (the byte budget guards against a
    /// guest loading components in a loop to exhaust host memory).
    pub(crate) fn insert_component(
        &mut self,
        component: eo9_component::Component,
        size: u64,
    ) -> wasmtime::Result<u32> {
        if self.component_bytes + size > MAX_COMPONENT_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "component byte budget exceeded: {size} more bytes would pass the \
                 {MAX_COMPONENT_BYTES}-byte ceiling"
            )));
        }
        let rep = self.components.insert(component)?;
        self.component_bytes += size;
        Ok(rep)
    }

    /// Drop a component handle, releasing its byte budget.
    pub(crate) fn free_component(&mut self, rep: u32) {
        if let Some(component) = self.components.free(rep) {
            self.component_bytes = self
                .component_bytes
                .saturating_sub(component.save().len() as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_count_caps_reject_cleanly() {
        let mut table: Table<u32> = Table::new(2, "thing");
        table.insert(1).unwrap();
        table.insert(2).unwrap();
        let err = table.insert(3).unwrap_err();
        assert!(format!("{err}").contains("too many live thing handles"));
        // Freeing a slot makes room again.
        table.free(0);
        table.insert(4).unwrap();
    }

    #[test]
    fn component_byte_budget_rejects_an_over_limit_load() {
        let engine = crate::engine::new_engine(&crate::engine::EngineOptions::default()).unwrap();
        let mut exec = ExecProvider::new(&engine, ChildPolicy::no_providers());
        let tiny = eo9_component::Component::load(wat_bytes()).unwrap();
        // A component whose claimed size exceeds the budget is refused before insertion.
        let err = exec
            .insert_component(tiny, MAX_COMPONENT_BYTES + 1)
            .unwrap_err();
        assert!(format!("{err}").contains("component byte budget exceeded"));
        assert_eq!(exec.component_bytes, 0);
    }

    /// The smallest valid binary component encoding (an empty component), used as a stand-in
    /// for "some component bytes" in the byte-budget test.
    fn wat_bytes() -> Vec<u8> {
        // (component) binary header: magic + version/layer.
        vec![0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00]
    }
}
