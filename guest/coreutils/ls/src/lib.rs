//! ls — list a directory's entries, one per line (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "ls",
    apis: [io, fs, text],
});

eo9_guest::main! {
    /// `ls [<path>…]` — list each directory's entries, one per line. With no paths the
    /// root `/` is listed; with several, each group is introduced by a `<path>:` header.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));

        let paths = if paths.is_empty() { vec![String::from("/")] } else { paths };
        let many = paths.len() > 1;
        let root = fs::default();
        let mut total: u32 = 0;
        for (index, path) in paths.into_iter().enumerate() {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            if many {
                if index > 0 {
                    text::write_out_line("").map_err(io_err)?;
                }
                text::write_out_line(&format!("{path}:")).map_err(io_err)?;
            }
            let entries = fs::list_directory(&root, path).await.map_err(fs_err)?;
            for entry in &entries {
                text::write_out_line(entry).map_err(io_err)?;
            }
            total += entries.len() as u32;
        }
        Ok(ProgramSuccess::Listed(total))
    }
}
