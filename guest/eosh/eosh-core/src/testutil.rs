//! Test support: a mock [`Backend`] that records every operation, and a tiny executor
//! for driving the (immediately-ready) futures the mock produces.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::backend::{
    ArgSpec, Backend, BackendError, ComponentInfo, ComponentKind, ExportSlot, InterfaceRef,
    NamedArg, Outcome, WaveValue,
};

/// Drive a future that never actually suspends (every await point in the mock backend
/// is immediately ready) to completion.
pub fn block_on_ready<F: Future>(future: F) -> F::Output {
    fn raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        RawWaker::new(
            core::ptr::null(),
            &RawWakerVTable::new(clone, no_op, no_op, no_op),
        )
    }
    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("mock-backed future unexpectedly suspended"),
    }
}

/// A `ComponentInfo` for a binary with the given `(name, ty)` argument signature.
pub fn binary(args: &[(&str, &str)]) -> ComponentInfo {
    ComponentInfo {
        kind: ComponentKind::Binary,
        imports: Vec::new(),
        exports: Vec::new(),
        args: args
            .iter()
            .map(|(name, ty)| ArgSpec {
                name: name.to_string(),
                ty: ty.to_string(),
            })
            .collect(),
    }
}

/// A `ComponentInfo` for a provider exporting the given interfaces (slot name defaults
/// to the interface name, as in the spec).
pub fn provider(exports: &[&str]) -> ComponentInfo {
    ComponentInfo {
        kind: ComponentKind::Provider,
        imports: Vec::new(),
        exports: exports
            .iter()
            .map(|interface| ExportSlot {
                name: interface.to_string(),
                interface: interface.to_string(),
                version: "0.1.0".to_string(),
            })
            .collect(),
        args: Vec::new(),
    }
}

/// A mock backend: components are small integers, every operation appends one line to
/// [`MockBackend::log`], and printed output is captured.
pub struct MockBackend {
    /// Name → info for programs `resolve` can find.
    programs: BTreeMap<String, ComponentInfo>,
    /// Component id → info.
    infos: BTreeMap<u32, ComponentInfo>,
    next_component: u32,
    next_image: u32,
    next_task: u32,
    /// The outcome `wait` reports (settable by tests).
    pub outcome: Outcome,
    /// Every backend operation, one line each.
    pub log: Vec<String>,
    /// Lines printed to standard output.
    pub out: Vec<String>,
    /// Lines printed to standard error.
    pub err: Vec<String>,
}

impl MockBackend {
    pub fn new() -> Self {
        MockBackend {
            programs: BTreeMap::new(),
            infos: BTreeMap::new(),
            next_component: 0,
            next_image: 0,
            next_task: 0,
            outcome: Outcome::Success(WaveValue {
                ty: "program-success".to_string(),
                value: "done".to_string(),
            }),
            log: Vec::new(),
            out: Vec::new(),
            err: Vec::new(),
        }
    }

    /// Register a resolvable program.
    pub fn program(&mut self, name: &str, info: ComponentInfo) {
        self.programs.insert(name.to_string(), info);
    }

    /// Register a resolvable program with an explicit argument signature.
    pub fn program_with_args(
        &mut self,
        name: &str,
        mut info: ComponentInfo,
        args: &[(&str, &str)],
    ) {
        info.args = args
            .iter()
            .map(|(name, ty)| ArgSpec {
                name: name.to_string(),
                ty: ty.to_string(),
            })
            .collect();
        self.programs.insert(name.to_string(), info);
    }

    /// Insert a component value directly (e.g. for pre-made `let` bindings).
    pub fn insert(&mut self, info: ComponentInfo) -> u32 {
        self.next_component += 1;
        let id = self.next_component;
        self.infos.insert(id, info);
        id
    }

    fn fresh(&mut self, info: ComponentInfo) -> u32 {
        self.insert(info)
    }

    fn info(&self, id: u32) -> ComponentInfo {
        self.infos
            .get(&id)
            .cloned()
            .expect("mock component id should exist")
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for MockBackend {
    type Component = u32;
    type Image = u32;
    type Task = u32;

    async fn resolve(&mut self, name: &str) -> Result<u32, BackendError> {
        match self.programs.get(name).cloned() {
            Some(info) => {
                let id = self.fresh(info);
                self.log.push(format!("resolve({name}) -> c{id}"));
                Ok(id)
            }
            None => Err(BackendError::new(format!(
                "cannot resolve `{name}`: no such module"
            ))),
        }
    }

    fn load(&mut self, bytes: &[u8]) -> Result<u32, BackendError> {
        let id = self.fresh(binary(&[]));
        self.log
            .push(format!("load({} bytes) -> c{id}", bytes.len()));
        Ok(id)
    }

    fn duplicate(&mut self, component: &u32) -> Result<u32, BackendError> {
        let info = self.info(*component);
        let id = self.fresh(info);
        self.log.push(format!("duplicate(c{component}) -> c{id}"));
        Ok(id)
    }

    fn describe(&mut self, component: &u32) -> ComponentInfo {
        self.log.push(format!("describe(c{component})"));
        self.info(*component)
    }

    fn compose(&mut self, provider: u32, consumer: u32) -> Result<u32, BackendError> {
        // Kind preservation: the result is whatever the consumer is, with its exports
        // and argument signature (SPEC: Algebraic properties).
        let info = self.info(consumer);
        let id = self.fresh(info);
        self.log
            .push(format!("compose(c{provider}, c{consumer}) -> c{id}"));
        Ok(id)
    }

    fn extend(&mut self, base: u32, layer: u32) -> Result<u32, BackendError> {
        let base_info = self.info(base);
        let layer_info = self.info(layer);
        // Right-biased union of exports.
        let mut exports = layer_info.exports.clone();
        for export in base_info.exports {
            if !exports.iter().any(|e| e.name == export.name) {
                exports.push(export);
            }
        }
        let id = self.fresh(ComponentInfo {
            kind: ComponentKind::Provider,
            imports: Vec::new(),
            exports,
            args: Vec::new(),
        });
        self.log.push(format!("extend(c{base}, c{layer}) -> c{id}"));
        Ok(id)
    }

    fn restrict(&mut self, component: u32, allow: &[InterfaceRef]) -> Result<u32, BackendError> {
        let info = self.info(component);
        let id = self.fresh(info);
        let rendered: Vec<String> = allow
            .iter()
            .map(|r| match &r.version {
                Some(version) => format!("{}@{version}", r.interface),
                None => r.interface.clone(),
            })
            .collect();
        self.log.push(format!(
            "restrict(c{component}, [{}]) -> c{id}",
            rendered.join(", ")
        ));
        Ok(id)
    }

    fn configure(&mut self, provider: u32, args: &[NamedArg]) -> Result<u32, BackendError> {
        // A configured provider keeps its exports but no longer exposes a config
        // signature (the config interface is sealed by configuration).
        let mut info = self.info(provider);
        info.args = Vec::new();
        let id = self.fresh(info);
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| format!("{}={}", arg.name, arg.value))
            .collect();
        self.log.push(format!(
            "configure(c{provider}, [{}]) -> c{id}",
            rendered.join(", ")
        ));
        Ok(id)
    }

    fn rename(&mut self, component: u32, from: &str, to: &str) -> Result<u32, BackendError> {
        let mut info = self.info(component);
        for export in &mut info.exports {
            if export.name == from {
                export.name = to.to_string();
            }
        }
        for import in &mut info.imports {
            if import.slot == from {
                import.slot = to.to_string();
            }
        }
        let id = self.fresh(info);
        self.log
            .push(format!("rename(c{component}, {from} -> {to}) -> c{id}"));
        Ok(id)
    }

    fn compile(&mut self, component: u32) -> Result<u32, BackendError> {
        self.next_image += 1;
        let id = self.next_image;
        self.log.push(format!("compile(c{component}) -> i{id}"));
        Ok(id)
    }

    fn spawn(&mut self, image: &u32, args: &[NamedArg]) -> Result<u32, BackendError> {
        self.next_task += 1;
        let id = self.next_task;
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| format!("{}={}", arg.name, arg.value))
            .collect();
        self.log.push(format!(
            "spawn(i{image}, [{}]) -> t{id}",
            rendered.join(", ")
        ));
        Ok(id)
    }

    async fn wait(&mut self, task: u32) -> Outcome {
        self.log.push(format!("wait(t{task})"));
        self.outcome.clone()
    }

    fn print(&mut self, text: &str) {
        self.out.push(text.to_string());
    }

    fn print_error(&mut self, text: &str) {
        self.err.push(text.to_string());
    }
}
