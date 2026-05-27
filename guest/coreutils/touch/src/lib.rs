//! touch — create an empty file if it does not exist (eo9:fs only).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;

eo9_guest::bindings!({
    world: "touch",
    apis: [io, fs],
});

eo9_guest::main! {
    async fn main(path: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let root = fs::default();
        // CREATE without TRUNCATE: makes the file if absent, leaves an existing one intact.
        let _file = fs::open(&root, path, fs::OpenFlags::CREATE | fs::OpenFlags::WRITE)
            .await
            .map_err(fs_err)?;
        Ok(ProgramSuccess::Touched)
    }
}
