//! stat — print a node's kind and size (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "stat",
    apis: [io, fs, text],
});

eo9_guest::main! {
    /// `stat <path>…` — print each node's kind and size (prefixed with the path when
    /// more than one is given).
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));

        let many = paths.len() > 1;
        let root = fs::default();
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            let st = fs::stat(&root, path.clone()).await.map_err(fs_err)?;
            let kind = match st.kind {
                fs::NodeKind::File => "file",
                fs::NodeKind::Directory => "directory",
            };
            let line = if many {
                format!("{path}: {kind} {} bytes", st.size)
            } else {
                format!("{kind} {} bytes", st.size)
            };
            text::write_out_line(&line).map_err(io_err)?;
        }
        Ok(ProgramSuccess::Described)
    }
}
