//! head — print the first `lines` lines of a file (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "head",
    apis: [io, fs, text],
});

eo9_guest::main! {
    async fn main(path: String, lines: u64) -> Result<ProgramSuccess, ProgramFailure> {
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
        let mut printed = 0u32;
        for line in contents.lines().take(lines as usize) {
            text::write_out_line(line).map_err(io_err)?;
            printed += 1;
        }
        Ok(ProgramSuccess::Printed(printed))
    }
}
