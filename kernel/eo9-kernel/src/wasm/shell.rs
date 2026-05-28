//! Boot-to-eosh: run the unmodified eosh component as the kernel's boot program.
//!
//! eosh is an ordinary Eo9 program (SPEC "Shell"): it has no private powers, it just
//! happens to be granted the text, fs, and exec capabilities. The kernel is its embedder
//! here, exactly as `eo9 shell` is in usermode:
//!
//! * **text** — the PL011 console (the same kernel root provider every program gets);
//! * **fs** — a read-only view of the baked-in store image: `/bin/<name>.wasm` per store
//!   entry plus the `/session` manifest (src/wasm/shellfs.rs);
//! * **exec** — component-algebra/compile/task backed by the store image and the child
//!   drive loop (src/wasm/shellexec.rs); children inherit the full session environment
//!   (text/time/entropy, the read-only store fs, io buffers, and exec) every generation —
//!   the same inherit-everything default as usermode, restricted with `only` — so a nested
//!   `eosh` is a full peer.
//!
//! The session manifest (`eo9-session 1` format, plan/10 D9) is generated here so eosh's
//! `env` builtin can show the capability picture of this machine.
//!
//! The drive loop polls eosh's `main` and, between polls, every running child — the
//! bare-metal counterpart of usermode children executing inside their parent's resume
//! (wasmtime forbids re-entering the event loop from a host function). There is no
//! watchdog on the session: it is user-paced and ends at `exit` or end of input.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

use super::providers::{self, KernelState};
use super::shellexec::{self, ShellExec};
use super::shellfs::{self, BufferTable, ShellFs};
use super::store::StoreEntry;
use super::wave;

/// The shell session's state, hung off the eosh store's [`KernelState`]: the fs view, the
/// I/O buffer table, the exec tables, and the engine children are instantiated against.
pub struct ShellState {
    pub fs: ShellFs,
    pub buffers: BufferTable,
    pub exec: ShellExec,
    pub engine: Engine,
}

/// Boot the interactive shell: instantiate eosh from the store and drive it (and its
/// children) until the session ends.
pub fn boot_to_eosh(entries: &'static [StoreEntry]) {
    crate::kprintln!(
        "eosh: starting the Eo9 shell from the baked-in store ({} programs under /bin)",
        entries.len()
    );
    match run_eosh(entries) {
        Ok(outcome) => crate::kprintln!("eosh: session ended, outcome = {outcome}"),
        Err(error) => crate::kprintln!("eosh: FAILED: {error:?}"),
    }
}

/// The session manifest eosh's `env` builtin reads from `/session` (the `eo9-session 1`
/// format from plan/10 D9 / plan/11 D12 — keep in sync with eosh-core's `envinfo`).
/// Children read the same manifest through their own fs view (they inherit the full
/// session environment, so the picture it paints is theirs too).
pub(super) fn session_manifest(entries: &'static [StoreEntry]) -> String {
    let names: alloc::vec::Vec<&str> = entries.iter().map(|entry| entry.name).collect();
    let mut lines = vec![
        String::from("eo9-session 1"),
        String::from("shell text PL011 serial console"),
        String::from("shell fs the baked-in read-only store image (program names under /bin)"),
        String::from("shell exec spawn programs as children"),
        String::from("child text PL011 serial console (shared with the shell)"),
        String::from("child time generic timer + PL031 RTC"),
        String::from("child entropy counter-seeded splitmix64 (a stub, not a CSPRNG)"),
        String::from(
            "child fs the same read-only store image view (programs under /bin, /session)",
        ),
        String::from(
            "child exec spawn programs as children (the full session environment is inherited, every generation)",
        ),
        String::from("note programs get no writable filesystem on bare metal yet"),
        String::from("note restrict a command with `only` to strip capabilities before it runs"),
        if cfg!(feature = "wasm-codegen") {
            String::from(
                "note the store is read-only and baked into the kernel image; compositions \
                 (`$`, `&`, `only`, configure) are fused and compiled on-target",
            )
        } else {
            String::from(
                "note the store is read-only and baked into the kernel image; this kernel \
                 was built without `wasm-codegen`, so composition and on-target codegen are \
                 not available",
            )
        },
        format!("note programs available under /bin: {}", names.join(", ")),
    ];
    let mut manifest = String::new();
    for line in lines.drain(..) {
        manifest.push_str(&line);
        manifest.push('\n');
    }
    manifest
}

/// No-op (but real, cloneable) waker: wasmtime records wake-ups against it; the drive loop
/// polls every iteration regardless.
struct LoopWaker;

impl alloc::task::Wake for LoopWaker {
    fn wake(self: Arc<Self>) {}
    fn wake_by_ref(self: &Arc<Self>) {}
}

fn run_eosh(entries: &'static [StoreEntry]) -> Result<String, wasmtime::Error> {
    let eosh = entries
        .iter()
        .find(|entry| entry.name == "eosh")
        .ok_or_else(|| wasmtime::Error::msg("the baked-in store has no `eosh` entry"))?;

    // Children of a previous session (there are none today, but be safe) cannot alias the
    // new session's task handles.
    shellexec::reset_children();

    let engine = super::new_engine()?;

    // SAFETY: the artifact comes from the store image produced by `cargo xtask
    // build-kernel` with the same wasmtime version and engine configuration, embedded
    // read-only in the kernel image.
    let component = unsafe { Component::deserialize(&engine, eosh.artifact)? };

    let mut linker: Linker<KernelState> = Linker::new(&engine);
    providers::add_providers(&mut linker)?;
    shellfs::add_buffers(&mut linker)?;
    shellfs::add_fs(&mut linker)?;
    shellexec::add_exec(&mut linker)?;

    let manifest = session_manifest(entries);
    let mut state = KernelState::new();
    state.shell = Some(alloc::boxed::Box::new(ShellState {
        fs: ShellFs::new(entries, manifest),
        buffers: BufferTable::default(),
        exec: ShellExec::default(),
        engine: engine.clone(),
    }));
    let mut store = Store::new(&engine, state);
    // The engine meters fuel (see `new_engine`); the shell itself runs from an
    // effectively-unlimited pool — its own guest code is the parser/evaluator, and the
    // heavy work (children, on-target compilation) happens elsewhere. Children get their
    // own sliced pools in `shellexec::spawn_child`.
    store.set_fuel(u64::MAX)?;

    let instance = super::block_on(
        "eosh instantiation",
        linker.instantiate_async(&mut store, &component),
    )??;

    let main = instance
        .get_func(&mut store, "main")
        .ok_or_else(|| wasmtime::Error::msg("eosh does not export `main`"))?;

    // eosh's single argument is `command: option<string>`: absent means the interactive
    // read–eval loop on the serial console.
    let params = [Val::Option(None)];
    let mut results = vec![Val::Bool(false)];

    // The session drive loop: poll eosh, and between polls give every running child a
    // poll of its own. No watchdog — the session is paced by the user at the console.
    // (The block scopes the call future so its borrow of `results` ends before the
    // outcome is read back.)
    {
        let call = main.call_async(&mut store, &params, &mut results);
        let mut call = pin!(call);
        let waker = Waker::from(Arc::new(LoopWaker));
        let mut cx = Context::from_waker(&waker);
        loop {
            match call.as_mut().poll(&mut cx) {
                Poll::Ready(Ok(())) => break,
                Poll::Ready(Err(err)) => return Err(err),
                Poll::Pending => {
                    shellexec::drive_children();
                    // Idle the core between polls instead of spinning at 100%: give every
                    // running child a turn (above), then sleep in `wfi` until the generic
                    // timer fires (src/wasm/mod.rs arms it + the GIC/IRQ wakes us). Heavy
                    // guest compute runs inside a single poll, so this only bounds the
                    // latency at an await point (a child finishing, serial input arriving),
                    // not throughput. `wake_idle` re-drives eosh's parked `read-line` future.
                    super::idle_wait();
                }
            }
        }
    }

    Ok(results.first().map(wave::render).unwrap_or_default())
}
