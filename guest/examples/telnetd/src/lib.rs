//! telnetd — the shell-over-network supervisor.
//!
//! Targets the `eo9-examples:telnetd/telnetd` world (see `wit/world.wit`): an ordinary
//! program holding text + fs + exec that composes the per-session stack
//!
//! ```text
//! net.virtio $ net.l4.over-l2 $ net.text $ eosh
//! ```
//!
//! compiles it once, and serves sessions **sequentially** — spawn, wait, respawn — one
//! fused task per session (the plan/09 D44 handle-transfer finding: a live l4
//! connection cannot cross task stores, and one NIC is one task's claim). Session
//! death is never telnetd's death: every outcome, including a trap, is narrated to the
//! machine console and the next session is served. A remote `poweroff` is refused —
//! the outcome ends that session and is narrated, never honored.
//!
//! **SECURITY: sessions are cleartext, unauthenticated telnet (see net.text). Trusted
//! LAN / dev use only; SSH is explicitly deferred (owner ruling).**

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use eo9_guest::buffer;

// Six typed `main` parameters lower to more core-glue arguments than clippy's
// budget; the WIT signature is the real interface, so the lint is noise here.
#[allow(clippy::too_many_arguments)]
mod bindings {
    wit_bindgen::generate!({
        world: "telnetd",
        generate_all,
        with: {
            "eo9:io/buffers@0.1.0": eo9_guest::api::io::buffers,
            "eo9:text/types@0.1.0": eo9_guest::api::text::types,
            "eo9:text/text@0.1.0": eo9_guest::api::text::text,
            "eo9:fs/fs@0.1.0": eo9_guest::api::fs::fs,
        },
    });
}

use bindings::eo9::exec::{compile, component_algebra as algebra, task};
use bindings::{Guest, ProgramFailure, ProgramSuccess, export};
use eo9_guest::api::fs::fs;
use eo9_guest::api::text::text;

/// The documented default port (telnet).
const DEFAULT_PORT: u16 = 23;
/// Hard ceiling on sessions when `--sessions` is not given: telnetd must not respawn
/// forever unsupervised (the init console-restart cap, same number, same reason).
const MAX_SESSIONS: u32 = 1000;

/// The /bin names of the session stack, bottom to top (`--nic` overrides the NIC:
/// `net.rtl8125` on the board bench).
const DEFAULT_STACK_NIC: &str = "net.virtio";
const STACK_TCP: &str = "net.l4.over-l2";
const STACK_TEXT: &str = "net.text";
const STACK_SHELL: &str = "eosh";

/// Console narration, every line prefixed so the machine console stays attributable.
struct Out {
    text: text::TextImpl,
}

impl Out {
    fn new() -> Self {
        Out {
            text: text::default(),
        }
    }

    fn say(&self, line: &str) {
        let _ = text::write(&self.text, text::OutputStream::Out, "telnetd: ");
        let _ = text::write(&self.text, text::OutputStream::Out, line);
        let _ = text::write(&self.text, text::OutputStream::Out, "\n");
    }
}

/// Resolve a `/bin` program name to an open component value (the init/eosh interim
/// convention: open `/bin/<name>.wasm` for execution, read it, `load` it).
async fn resolve(handle: &fs::FsImpl, name: &str) -> Result<algebra::Component, String> {
    let path = format!("/bin/{name}.wasm");
    let exec_handle = fs::open_exec(handle, path.clone())
        .await
        .map_err(|err| format!("cannot resolve `{name}` ({path}): {err:?}"))?;

    let size = fs::exec_size(&exec_handle);
    let mut bytes: Vec<u8> = Vec::with_capacity(size as usize);
    while (bytes.len() as u64) < size {
        let offset = bytes.len() as u64;
        let chunk = buffer::with_capacity(size - offset);
        let (chunk, result) = fs::exec_read(&exec_handle, offset, chunk).await;
        let read = result.map_err(|err| format!("reading `{name}` failed: {err:?}"))?;
        if read.bytes_read == 0 {
            return Err(format!("reading `{name}` ended early (zero-length read)"));
        }
        bytes.extend_from_slice(&buffer::prefix_to_vec(&chunk, read.bytes_read));
    }

    algebra::load(&bytes).map_err(|err| format!("cannot load `{name}`: {err:?}"))
}

/// Render a session outcome in one line (the init convention).
fn outcome_text(outcome: &task::ProgramOutcome) -> String {
    match outcome {
        task::ProgramOutcome::Success(value) => {
            if value.value.is_empty() {
                String::from("success")
            } else {
                format!("success({})", value.value)
            }
        }
        task::ProgramOutcome::Failure(value) => {
            if value.value.is_empty() {
                String::from("failure")
            } else {
                format!("failure({})", value.value)
            }
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Trapped(reason)) => {
            format!("abnormal(trapped({reason}))")
        }
        task::ProgramOutcome::Abnormal(task::AbnormalExit::Killed) => {
            String::from("abnormal(killed)")
        }
    }
}

struct Telnetd;

impl Guest for Telnetd {
    async fn main(
        port: Option<u16>,
        sessions: Option<u32>,
        nic: Option<String>,
        address: Option<String>,
        prefix_length: Option<u8>,
        gateway: Option<String>,
        advertise_max: Option<u16>,
    ) -> Result<ProgramSuccess, ProgramFailure> {
        let out = Out::new();
        let port = port.unwrap_or(DEFAULT_PORT);
        if port == 0 {
            return Err(ProgramFailure::BadArguments(String::from(
                "--port must be 1..=65535",
            )));
        }
        let limit = match sessions {
            Some(0) => {
                return Err(ProgramFailure::BadArguments(String::from(
                    "--sessions must be at least 1 (omit it to serve until the cap)",
                )));
            }
            Some(n) => n.min(MAX_SESSIONS),
            None => MAX_SESSIONS,
        };

        // Static addressing: address and gateway travel together (the middleware's
        // configure binds all three fields at once; prefix-length alone is meaningless).
        if address.is_some() != gateway.is_some() {
            return Err(ProgramFailure::BadArguments(String::from(
                "--address and --gateway must be given together (prefix-length \
                 defaults to 24)",
            )));
        }
        if prefix_length.is_some() && address.is_none() {
            return Err(ProgramFailure::BadArguments(String::from(
                "--prefix-length needs --address and --gateway",
            )));
        }

        // ----- resolve, configure, compose, compile — once -----------------------------
        let fs_handle = fs::default();
        let nic_name = nic.as_deref().unwrap_or(DEFAULT_STACK_NIC);
        let nic = resolve(&fs_handle, nic_name)
            .await
            .map_err(ProgramFailure::Resolve)?;
        // Forward the speed cap to the NIC's own configure interface when given
        // (net.rtl8125's `rtl8125-config`). A NIC without such an interface (e.g.
        // net.virtio) makes this a clean configure error — option-C discipline:
        // refused typed at compose, never a trap.
        let nic = match advertise_max {
            Some(advertise_max) => algebra::configure(
                nic,
                &[algebra::NamedArg {
                    name: String::from("advertise-max"),
                    value: advertise_max.to_string(),
                }],
            )
            .map_err(|err| {
                ProgramFailure::Configure(format!("{nic_name} (advertise-max): {err:?}"))
            })?,
            None => nic,
        };
        let tcp = resolve(&fs_handle, STACK_TCP)
            .await
            .map_err(ProgramFailure::Resolve)?;
        let net_text = resolve(&fs_handle, STACK_TEXT)
            .await
            .map_err(ProgramFailure::Resolve)?;
        let shell = resolve(&fs_handle, STACK_SHELL)
            .await
            .map_err(ProgramFailure::Resolve)?;

        // Bake static IPv4 addressing into the middleware when given (the board
        // bench: `--address 10.20.3.70 --gateway 10.20.3.1`); without the arguments
        // the middleware keeps its documented QEMU user-net default.
        let tcp = match (&address, &gateway) {
            (Some(address), Some(gateway)) => algebra::configure(
                tcp,
                &[
                    algebra::NamedArg {
                        name: String::from("address"),
                        value: format!("{address:?}"),
                    },
                    algebra::NamedArg {
                        name: String::from("prefix-length"),
                        value: prefix_length.unwrap_or(24).to_string(),
                    },
                    algebra::NamedArg {
                        name: String::from("gateway"),
                        value: format!("{gateway:?}"),
                    },
                ],
            )
            .map_err(|err| ProgramFailure::Configure(format!("net.l4.over-l2: {err:?}")))?,
            _ => tcp,
        };

        // Bind the port at compose time (the config interface disappears with this, so
        // the session can neither observe nor re-run it).
        let net_text = algebra::configure(
            net_text,
            &[algebra::NamedArg {
                name: String::from("port"),
                value: port.to_string(),
            }],
        )
        .map_err(|err| ProgramFailure::Configure(format!("net.text: {err:?}")))?;

        // net.virtio $ net.l4.over-l2 $ net.text $ eosh — innermost first, exactly as
        // the shell line would evaluate it.
        let session = algebra::compose(net_text, shell)
            .map_err(|err| ProgramFailure::Compose(format!("net.text $ eosh: {err:?}")))?;
        let session = algebra::compose(tcp, session)
            .map_err(|err| ProgramFailure::Compose(format!("net.l4.over-l2 $ …: {err:?}")))?;
        let session = algebra::compose(nic, session)
            .map_err(|err| ProgramFailure::Compose(format!("{nic_name} $ …: {err:?}")))?;

        let opts = compile::CompileOpts {
            debug_info: false,
            safepoint_maps: false,
        };
        let image = compile::compile(session, opts)
            .map_err(|err| ProgramFailure::Compile(format!("{err:?}")))?;

        // ----- serve sessions, sequentially ---------------------------------------------
        out.say(&format!(
            "serving up to {limit} session(s) on port {port} — cleartext telnet, \
             unauthenticated; trusted networks only"
        ));
        let mut served: u32 = 0;
        while served < limit {
            let number = served + 1;
            out.say(&format!("session {number}: waiting for a connection"));
            let limits = task::SpawnLimits { max_memory: None };
            let session_task = task::spawn(&image, &[], Vec::new(), limits)
                .map_err(|err| ProgramFailure::Spawn(format!("session {number}: {err:?}")))?;
            let outcome = task::wait(&session_task).await;
            served += 1;
            out.say(&format!(
                "session {number} ended ({})",
                outcome_text(&outcome)
            ));

            // A remote poweroff is refused: halting the machine is a console intent,
            // not a network one (the session has already ended either way).
            if let task::ProgramOutcome::Success(value) = &outcome
                && value.value == "poweroff-requested"
            {
                out.say("refusing a remote poweroff request (console intent only)");
            }
        }
        out.say(&format!("served {served} session(s); exiting"));
        Ok(ProgramSuccess::Served(served))
    }
}

export!(Telnetd with_types_in bindings);
