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

eo9_guest::main! {
    /// `rm <path>…` — remove each file or empty directory, in order.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let root = fs::default();
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            fs::remove(&root, path).await.map_err(fs_err)?;
        }
        Ok(ProgramSuccess::Removed)
    }
}
