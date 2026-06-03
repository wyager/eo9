//! rm — remove a file or empty directory (eo9:fs only).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;

eo9_guest::bindings!({
    world: "rm",
    apis: [io, fs],
});

/// Human text for an fs failure at `path` — the typed vocabulary, never Rust debug
/// formatting (the R2-18 enum-leak class).
fn fs_fail(path: &str, e: fs::FsError) -> ProgramFailure {
    let what = match e {
        fs::FsError::NotFound => String::from("not found"),
        fs::FsError::AlreadyExists => String::from("already exists"),
        fs::FsError::NotADirectory => String::from("not a directory"),
        fs::FsError::IsADirectory => String::from("is a directory"),
        fs::FsError::Denied => String::from("denied"),
        fs::FsError::ReadOnly => String::from("read-only"),
        fs::FsError::NoSpace => String::from("no space"),
        fs::FsError::NotImmutable => String::from("not immutable"),
        fs::FsError::Io(m) => format!("io: {m}"),
    };
    ProgramFailure::Fs(format!("{path}: {what}"))
}

eo9_guest::main! {
    /// `rm <path>…` — remove each file or empty directory, in order.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let root = fs::default();
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            fs::remove(&root, path.clone())
                .await
                .map_err(|e| fs_fail(&path, e))?;
        }
        Ok(ProgramSuccess::Removed)
    }
}
