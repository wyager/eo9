//! rm — remove a file or empty directory (eo9:fs only).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;

eo9_guest::bindings!({
    world: "rm",
    apis: [io, fs],
});

eo9_guest::main! {
    async fn main(path: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let root = fs::default();
        fs::remove(&root, path).await.map_err(fs_err)?;
        Ok(ProgramSuccess::Removed)
    }
}
