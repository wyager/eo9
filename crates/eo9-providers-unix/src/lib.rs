//! Unix-backed root providers for the Eo9 OS APIs.
//!
//! This crate is the usermode equivalent of drivers: trusted host-side implementations of
//! the `eo9:text`, `eo9:time`, `eo9:entropy`, `eo9:fs`, and `eo9:disk` interfaces, backed
//! by the host operating system. It is deliberately runtime-agnostic — there are no
//! wasmtime types anywhere — so the Eo9 runtime (the `eo9-runtime` crate) can wrap these
//! providers behind whatever host-trait shapes its linker needs.
//!
//! # Shape of the crate
//!
//! Each provider module mirrors one WIT package in `wit/`:
//!
//! | module      | WIT package   | provider type     | host trait(s)                      |
//! |-------------|---------------|-------------------|------------------------------------|
//! | [`text`]    | `eo9:text`    | `TextProvider`    | `TextHost`                         |
//! | [`time`]    | `eo9:time`    | `TimeProvider`    | `TimeHost`                         |
//! | [`entropy`] | `eo9:entropy` | `EntropyProvider` | `EntropyHost`                      |
//! | [`fs`]      | `eo9:fs`      | `FsProvider`      | `FsHost`, `FileHost`, `ImmutableHost` |
//! | [`disk`]    | `eo9:disk`    | `DiskProvider`    | `DiskHost`                         |
//!
//! The provider struct corresponds to the WIT `*-impl` root-handle resource; the
//! `default()` constructor of each WIT interface is the runtime's business (it hands out
//! a resource-table handle pointing at the provider instance).
//!
//! # Completion model
//!
//! Every potentially-blocking operation completes *asynchronously*: the caller supplies a
//! [`Completer`](completion::Completer) — a boxed `FnOnce` — and the provider guarantees
//! it is invoked exactly once, from a provider-owned thread, when the operation finishes
//! (on both the success and the error path). The runtime's completer typically pushes the
//! value into the issuing task's completion queue and rings its doorbell; a test's
//! completer just sends on a channel.
//!
//! The MVP backend is a small blocking-thread pool ([`pool::BlockingPool`]) plus a
//! dedicated timer thread (sleeps) and a dedicated stdin reader thread (read-line). The
//! backend is an implementation detail of each provider: an io_uring-style submission
//! backend can replace the pool without changing any caller, because the caller-visible
//! contract is only "op in, completion out".
//!
//! # Owned buffers
//!
//! Disk and fs I/O use the owned-buffer round-trip from the spec: the caller moves an
//! [`OwnedBuffer`](buffer::OwnedBuffer) into the provider and receives it back in the
//! completion value, on success and on error alike, so the buffer is held uniquely for
//! the life of the operation and never dangles.
//!
//! # Kill / linearity behavior
//!
//! Per the spec's kill contract, a killed task never observes anything again, and
//! anything it transferred away (a buffer in flight) belongs to the transferee. These
//! providers never *abort* an in-flight host operation:
//!
//! * **text** — `write` is synchronous. An in-flight `read-line` runs until the blocking
//!   read returns; the consumed line is delivered to the (possibly dead) completer and a
//!   dead runtime simply drops it — the line is lost, not pushed back.
//! * **time** — a pending `sleep` always fires at (or after) its deadline, even if the
//!   provider has been dropped in the meantime; the completion is dropped if nobody is
//!   listening.
//! * **entropy** — synchronous; nothing is ever in flight.
//! * **fs / disk** — in-flight reads and writes run to completion on a provider thread
//!   (a write issued before a kill may still reach the backing file); the completion,
//!   including the returned buffer, is then handed to the completer, and dropping it
//!   frees the buffer. Nothing dangles, nothing leaks.
//!
//! Dropping a provider never cancels accepted work: the blocking pool drains its queue on
//! drop, and the timer / reader threads keep servicing what they already accepted.

pub mod buffer;
pub mod completion;
pub mod pool;

pub use buffer::OwnedBuffer;
pub use completion::{Completer, completer};
pub use pool::BlockingPool;
