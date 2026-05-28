//! wc — print "<lines> <words> <bytes>" for a file (eo9:fs + eo9:text).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;
use eo9_guest::{buffer, text};

eo9_guest::bindings!({
    world: "wc",
    apis: [io, fs, text],
});

eo9_guest::main! {
    /// `wc <path>…` — print "<lines> <words> <bytes>" per file (suffixed with the path
    /// when more than one is given), plus a total line for several files.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));

        let many = paths.len() > 1;
        let root = fs::default();
        let (mut tl, mut tw, mut tb): (u64, u64, u64) = (0, 0, 0);
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            let st = fs::stat(&root, path.clone()).await.map_err(fs_err)?;
            let file = fs::open(&root, path.clone(), fs::OpenFlags::READ).await.map_err(fs_err)?;
            let dst = buffer::with_capacity(st.size);
            let (dst, read_result) = fs::read(&file, 0, dst).await;
            let read = read_result.map_err(fs_err)?;
            let bytes = buffer::prefix_to_vec(&dst, read.bytes_read);
            let contents = String::from_utf8_lossy(&bytes);
            let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
            let words = contents.split_whitespace().count() as u64;
            let line = if many {
                format!("{lines} {words} {} {path}", read.bytes_read)
            } else {
                format!("{lines} {words} {}", read.bytes_read)
            };
            text::write_out_line(&line).map_err(io_err)?;
            tl += lines;
            tw += words;
            tb += read.bytes_read;
        }
        if many {
            text::write_out_line(&format!("{tl} {tw} {tb} total")).map_err(io_err)?;
        }
        Ok(ProgramSuccess::Counted)
    }
}
