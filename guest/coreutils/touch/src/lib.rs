//! touch — create an empty file if it does not exist (eo9:fs only).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;

eo9_guest::bindings!({
    world: "touch",
    apis: [io, fs],
});

eo9_guest::main! {
    /// `touch <path>…` — create each file that does not exist yet, in order.
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
            // CREATE without TRUNCATE: makes the file if absent, leaves an existing one intact.
            let _file = fs::open(&root, path, fs::OpenFlags::CREATE | fs::OpenFlags::WRITE)
                .await
                .map_err(fs_err)?;
        }
        Ok(ProgramSuccess::Touched)
    }
}
