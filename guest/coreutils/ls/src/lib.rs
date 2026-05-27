//! ls — list a directory's entries, one per line (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "ls",
    apis: [io, fs, text],
});

eo9_guest::main! {
    async fn main(path: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));

        let root = fs::default();
        let entries = fs::list_directory(&root, path).await.map_err(fs_err)?;
        for entry in &entries {
            text::write_out_line(entry).map_err(io_err)?;
        }
        Ok(ProgramSuccess::Listed(entries.len() as u32))
    }
}
