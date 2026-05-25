//! Placeholder crate so the kernel workspace has a member to build, lint, and test.
//!
//! no_std everywhere except under `cargo test`, where the host test harness needs std.
//! xtask builds this workspace for a bare-metal target (aarch64-unknown-none) to keep it
//! honest about no_std, and runs its unit tests on the host triple.
#![cfg_attr(not(test), no_std)]

/// A trivial constant accessor so the workspace skeleton has something to test.
pub fn boot_magic() -> u64 {
    0xE09
}

#[cfg(test)]
mod tests {
    #[test]
    fn boot_magic_is_stable() {
        assert_eq!(super::boot_magic(), 0xE09);
    }
}
