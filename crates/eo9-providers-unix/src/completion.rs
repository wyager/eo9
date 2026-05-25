//! How potentially-blocking operations hand their results back to the caller.
//!
//! Every provider operation that may block takes a [`Completer`] supplied by the caller
//! and returns immediately; the provider invokes the completer exactly once, from a
//! provider-owned thread, when the operation finishes — on the success and the error
//! path alike. The Eo9 runtime's completer pushes the value into the issuing task's
//! completion queue and rings its doorbell; a test's completer sends on a channel.
//!
//! The completer is the *whole* caller-visible asynchrony contract: which thread (or
//! io_uring ring) actually performs the blocking work is a private implementation detail
//! of each provider.

/// A one-shot completion callback: invoked exactly once with the operation's result.
///
/// The callback must be `Send` because it is invoked from a provider-owned thread, and
/// `'static` because the operation may outlive the caller's stack frame (that is the
/// point of asynchronous completion).
pub type Completer<T> = Box<dyn FnOnce(T) + Send + 'static>;

/// Builds a [`Completer`] from a closure without spelling out the `Box`.
pub fn completer<T, F>(f: F) -> Completer<T>
where
    F: FnOnce(T) + Send + 'static,
{
    Box::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn completer_delivers_its_value() {
        let (tx, rx) = mpsc::channel();
        let done: Completer<u32> = completer(move |value| tx.send(value).unwrap());
        done(17);
        assert_eq!(rx.recv().unwrap(), 17);
    }
}
