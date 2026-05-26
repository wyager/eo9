//! Adapters from the unix root providers onto the runtime's provider traits, plus the
//! blocking helper the built-in drive loop uses.
//!
//! The two library crates deliberately do not know about each other: `eo9-providers-unix`
//! is runtime-agnostic (plain structs, completion callbacks, no wasmtime types), and
//! `eo9-runtime`'s provider traits use plain futures polled from the task's event loop
//! (the waker is the task's doorbell). The glue lives here, in the embedder
//! (plan/11-usermode.md): each adapter wraps a unix provider and bridges its
//! callback-style completions into the runtime's [`BoxOp`] futures with a one-shot cell.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use eo9_providers_unix::completer;
use eo9_providers_unix::entropy::{EntropyHost, EntropyProvider as UnixEntropy};
use eo9_providers_unix::text::{
    OutputStream as UnixOutputStream, ReadLineCompletion, TextError as UnixTextError, TextHost,
    TextProvider as UnixText,
};
use eo9_providers_unix::time::{TimeHost, TimeProvider as UnixTime};
use eo9_runtime::providers::BoxOp;
use eo9_runtime::{
    Datetime, EntropyProvider, OutputStream, Providers, Task, TextError, TextProvider, TimeProvider,
};

// ---------------------------------------------------------------------------------------
// Completion-callback -> future bridge
// ---------------------------------------------------------------------------------------

struct OneshotState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

/// The future half of a one-shot bridge: resolves once the paired completer has run.
struct Oneshot<T> {
    state: Arc<Mutex<OneshotState<T>>>,
}

impl<T: Send + 'static> Future for Oneshot<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.state.lock().unwrap();
        match state.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

/// A one-shot operation: the [`BoxOp`] future the runtime polls, and the completion
/// closure handed to the provider. The unix providers guarantee exactly-once completion
/// (on the success and error path alike), so the future can never be left dangling.
fn oneshot<T: Send + 'static>() -> (BoxOp<T>, impl FnOnce(T) + Send + 'static) {
    let state = Arc::new(Mutex::new(OneshotState {
        value: None,
        waker: None,
    }));
    let completion_state = Arc::clone(&state);
    let complete = move |value: T| {
        let waker = {
            let mut state = completion_state.lock().unwrap();
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    };
    (Box::pin(Oneshot { state }), complete)
}

// ---------------------------------------------------------------------------------------
// Provider adapters
// ---------------------------------------------------------------------------------------

/// `eo9:text/text` backed by the process's standard streams.
struct StdioText {
    inner: UnixText,
}

impl TextProvider for StdioText {
    fn write(&mut self, to: OutputStream, text: &str) -> Result<(), TextError> {
        let stream = match to {
            OutputStream::Out => UnixOutputStream::Out,
            OutputStream::Err => UnixOutputStream::Err,
        };
        self.inner.write(stream, text).map_err(text_error)
    }

    fn read_line(&mut self) -> BoxOp<Result<Option<String>, TextError>> {
        let (op, complete) = oneshot();
        self.inner
            .read_line(completer(move |result: ReadLineCompletion| {
                complete(result.map_err(text_error));
            }));
        op
    }
}

fn text_error(err: UnixTextError) -> TextError {
    match err {
        UnixTextError::Closed => TextError::Closed,
        UnixTextError::Io(message) => TextError::Io(message),
    }
}

/// `eo9:time/time` backed by the host's real clocks.
struct HostTime {
    inner: UnixTime,
}

impl TimeProvider for HostTime {
    fn now(&mut self) -> Datetime {
        let now = self.inner.now();
        Datetime {
            seconds: now.seconds,
            nanoseconds: now.nanoseconds,
        }
    }

    fn monotonic_now(&mut self) -> u64 {
        self.inner.monotonic_now().nanoseconds
    }

    fn resolution(&mut self) -> u64 {
        self.inner.resolution()
    }

    fn sleep(&mut self, duration_ns: u64) -> BoxOp<()> {
        let (op, complete) = oneshot();
        self.inner
            .sleep(duration_ns, completer(move |()| complete(())));
        op
    }
}

/// `eo9:entropy/entropy` backed by the host OS RNG.
struct HostEntropy {
    inner: UnixEntropy,
}

impl EntropyProvider for HostEntropy {
    fn get_bytes(&mut self, len: u64) -> Vec<u8> {
        self.inner.get_bytes(len)
    }

    fn get_u64(&mut self) -> u64 {
        self.inner.get_u64()
    }
}

/// The root providers of a usermode run: text on the process's standard streams, the
/// host's real clocks, and the OS RNG.
///
/// Handing all three to `spawn` never widens a program's capability set: the runtime only
/// links the interfaces the component actually imports (the loader rule), and an import
/// with no provider — today, anything beyond text/time/entropy — is a spawn error.
pub fn root_providers() -> Providers {
    Providers {
        text: Some(Box::new(StdioText {
            inner: UnixText::stdio(),
        })),
        time: Some(Box::new(HostTime {
            inner: UnixTime::new(),
        })),
        entropy: Some(Box::new(HostEntropy {
            inner: UnixEntropy::new(),
        })),
        // No root fs provider yet: the unix-backed filesystem provider is area 08/11
        // follow-up work; programs importing eo9:fs keep failing at spawn until then.
        fs: None,
    }
}

// ---------------------------------------------------------------------------------------
// Blocking until a task is runnable again
// ---------------------------------------------------------------------------------------

/// Wakes the driving thread when the task's doorbell rings.
struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Block the calling thread until `task` can make progress again — that is, until a
/// provider completion rings its doorbell. Used by the built-in drive loop whenever
/// `resume` reports the task blocked on I/O.
pub fn wait_until_runnable(task: &Task) {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let runnable = task.runnable();
    let mut runnable = std::pin::pin!(runnable);
    while runnable.as_mut().poll(&mut context).is_pending() {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oneshot_resolves_after_completion_and_wakes_the_waker() {
        let (mut op, complete) = oneshot::<u32>();
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));

        struct Flag(Arc<std::sync::atomic::AtomicBool>);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let waker = Waker::from(Arc::new(Flag(Arc::clone(&woken))));
        let mut context = Context::from_waker(&waker);
        assert!(op.as_mut().poll(&mut context).is_pending());

        complete(17);
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(op.as_mut().poll(&mut context), Poll::Ready(17));
    }

    #[test]
    fn oneshot_completed_before_first_poll_is_immediately_ready() {
        let (mut op, complete) = oneshot::<&'static str>();
        complete("done");
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        assert_eq!(op.as_mut().poll(&mut context), Poll::Ready("done"));
    }

    #[test]
    fn time_adapter_reports_monotonic_progress() {
        let mut time = HostTime {
            inner: UnixTime::new(),
        };
        let first = time.monotonic_now();
        let second = time.monotonic_now();
        assert!(second >= first);
        assert!(time.resolution() >= 1);
    }

    #[test]
    fn entropy_adapter_returns_requested_lengths() {
        let mut entropy = HostEntropy {
            inner: UnixEntropy::new(),
        };
        assert_eq!(entropy.get_bytes(16).len(), 16);
        let _ = entropy.get_u64();
    }
}
