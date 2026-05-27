//! stat — print a node's kind and size (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "stat",
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
        let st = fs::stat(&root, path).await.map_err(fs_err)?;
        let kind = match st.kind {
            fs::NodeKind::File => "file",
            fs::NodeKind::Directory => "directory",
        };
        text::write_out_line(&format!("{kind} {} bytes", st.size)).map_err(io_err)?;
        Ok(ProgramSuccess::Described)
    }
}
