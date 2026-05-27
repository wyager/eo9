//! cat — print a file's contents to stdout (eo9:fs read + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "cat",
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
        let st = fs::stat(&root, path.clone()).await.map_err(fs_err)?;
        let file = fs::open(&root, path, fs::OpenFlags::READ).await.map_err(fs_err)?;
        let dst = buffer::with_capacity(st.size);
        let (dst, read_result) = fs::read(&file, 0, dst).await;
        let read = read_result.map_err(fs_err)?;
        let bytes = buffer::prefix_to_vec(&dst, read.bytes_read);
        let contents = String::from_utf8_lossy(&bytes);
        text::write_out(&contents).map_err(io_err)?;
        Ok(ProgramSuccess::Printed(read.bytes_read))
    }
}
