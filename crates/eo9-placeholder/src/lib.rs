//! Placeholder crate so the host workspace has a member to build, lint, and test.
//!
//! Real host-side crates (`eo9-component`, `eo9-runtime`, `eo9-sched`, `eo9-store`,
//! `eo9-providers-unix`, `eo9`) replace this as their areas land.

/// A trivial function so the workspace skeleton has something to test.
pub fn answer() -> u32 {
    9
}

#[cfg(test)]
mod tests {
    #[test]
    fn answer_is_nine() {
        assert_eq!(super::answer(), 9);
    }
}
