//! Raw `wit-bindgen` bindings for the `eo9:*` API packages, re-exported one module per
//! API.
//!
//! Each API package contributes its types-only `types` interface (the root-handle
//! resource) plus its main interface; `io` contributes the shared `buffers` interface.
//! Program crates reuse these modules — rather than generating their own copies — via
//! the [`crate::bindings!`] macro, which maps the interfaces of the program's world onto
//! them with `wit-bindgen`'s `with` option. That keeps every guest crate speaking the
//! same Rust types, so the helpers in this crate work unchanged in any program.

pub mod io {
    pub use crate::bindings::eo9::io::buffers;
}

pub mod text {
    pub use crate::bindings::eo9::text::{text, types};
}

pub mod time {
    pub use crate::bindings::eo9::time::{time, types};
}

pub mod entropy {
    pub use crate::bindings::eo9::entropy::{entropy, types};
}

pub mod perf {
    pub use crate::bindings::eo9::perf::{perf, types};
}

pub mod disk {
    pub use crate::bindings::eo9::disk::{disk, types};
}

pub mod fs {
    pub use crate::bindings::eo9::fs::fs;
}

pub mod net {
    pub use crate::bindings::eo9::net::{net, types};
}
