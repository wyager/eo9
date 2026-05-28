//! The blob's JavaScript import surface (module `"env"`), with safe wrappers.
//!
//! Plain imports return immediately. The ones marked **JSPI** are wired on the page as
//! [`WebAssembly.Suspending`] functions: the call looks synchronous from here, but the
//! browser parks the whole blob activation on the underlying promise (a timer, the
//! visitor's input, a `fetch`) and resumes it with the result — which is what lets an Eo9
//! guest's `await` span real wall-clock time and real input on this host. When the browser
//! has no JSPI the page installs fallbacks that report "unavailable" (see `vm.js`), and the
//! page disables the affected demos with a clear message.

use std::string::String;
use std::vec;
use std::vec::Vec;

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// One UTF-8 line of terminal output.
    fn host_write(ptr: *const u8, len: usize);
    /// `Date.now()` in milliseconds.
    fn host_now_ms() -> f64;
    /// Monotonic nanoseconds (`performance.now() * 1e6`).
    fn host_monotonic_ns() -> f64;
    /// Fill `len` bytes at `ptr` from `crypto.getRandomValues`.
    fn host_random_fill(ptr: *mut u8, len: usize);
    /// **JSPI** — resolve after `ms` milliseconds (`setTimeout`).
    fn host_sleep_ms(ms: f64);
    /// **JSPI** — one line from the page terminal input. Writes at most `cap` bytes of
    /// UTF-8 at `ptr`; returns the byte length, `-1` for end-of-input, `-2` if the browser
    /// cannot suspend (no JSPI).
    fn host_read_line(ptr: *mut u8, cap: usize) -> i32;
    /// **JSPI** — fetch `/vm/store/<name>.cwasm`; returns its byte length and caches the
    /// bytes on the JS side, `-1` if the fetch failed, `-2` if the browser cannot suspend.
    fn host_fetch_len(name_ptr: *const u8, name_len: usize) -> i32;
    /// Copy the most recently fetched artifact (see [`host_fetch_len`]) into `dest`.
    fn host_fetch_copy(dest_ptr: *mut u8, len: usize);
}

pub fn write_out(message: &str) {
    unsafe { host_write(message.as_ptr(), message.len()) }
}

pub fn now_ms() -> f64 {
    unsafe { host_now_ms() }
}

pub fn monotonic_ns() -> u64 {
    let ns = unsafe { host_monotonic_ns() };
    if ns.is_finite() && ns > 0.0 {
        ns as u64
    } else {
        0
    }
}

pub fn random_fill(buffer: &mut [u8]) {
    unsafe { host_random_fill(buffer.as_mut_ptr(), buffer.len()) }
}

pub fn sleep_ms(ms: f64) {
    unsafe { host_sleep_ms(ms) }
}

/// One line from the page terminal (`None` = end of input, including the no-JSPI case —
/// the page separately disables input demos and explains why when JSPI is missing).
pub fn read_line(cap: usize) -> Option<String> {
    let mut buffer = vec![0u8; cap];
    let written = unsafe { host_read_line(buffer.as_mut_ptr(), buffer.len()) };
    if written < 0 {
        return None;
    }
    buffer.truncate(written as usize);
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Fetch a pre-AOT'd pulley32 artifact from the page's HTTP store (`/vm/store/<name>.cwasm`).
pub fn fetch_artifact(name: &str) -> Result<Vec<u8>, String> {
    let len = unsafe { host_fetch_len(name.as_ptr(), name.len()) };
    match len {
        -2 => Err(String::from(
            "this browser cannot suspend WebAssembly (no JSPI), so the blob cannot fetch \
             programs from the store",
        )),
        -1 => Err(std::format!(
            "could not fetch `{name}` from the page's program store"
        )),
        len => {
            let mut bytes = vec![0u8; len as usize];
            unsafe { host_fetch_copy(bytes.as_mut_ptr(), bytes.len()) };
            Ok(bytes)
        }
    }
}
