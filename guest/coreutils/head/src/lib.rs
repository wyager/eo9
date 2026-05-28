//! head — print the first `lines` lines of a file (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "head",
    apis: [io, fs, text],
});

eo9_guest::main! {
    /// `head --lines <n> <path>…` — print the first `lines` lines of each file (with a
    /// `==> path <==` header when more than one is given).
    async fn main(lines: u64, paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));

        let many = paths.len() > 1;
        let root = fs::default();
        let mut printed = 0u32;
        for (index, path) in paths.into_iter().enumerate() {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            if many {
                if index > 0 {
                    text::write_out_line("").map_err(io_err)?;
                }
                text::write_out_line(&format!("==> {path} <==")).map_err(io_err)?;
            }
            let st = fs::stat(&root, path.clone()).await.map_err(fs_err)?;
            let file = fs::open(&root, path, fs::OpenFlags::READ).await.map_err(fs_err)?;
            let dst = buffer::with_capacity(st.size);
            let (dst, read_result) = fs::read(&file, 0, dst).await;
            let read = read_result.map_err(fs_err)?;
            let bytes = buffer::prefix_to_vec(&dst, read.bytes_read);
            let contents = String::from_utf8_lossy(&bytes);
            for line in contents.lines().take(lines as usize) {
                text::write_out_line(line).map_err(io_err)?;
                printed += 1;
            }
        }
        Ok(ProgramSuccess::Printed(printed))
    }
}
