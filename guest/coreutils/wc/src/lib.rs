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

/// Human text for an fs failure at `path` — the typed vocabulary, never Rust debug
/// formatting (the R2-18 enum-leak class).
fn fs_fail(path: &str, e: fs::FsError) -> ProgramFailure {
    let what = match e {
        fs::FsError::NotFound => String::from("not found"),
        fs::FsError::AlreadyExists => String::from("already exists"),
        fs::FsError::NotADirectory => String::from("not a directory"),
        fs::FsError::IsADirectory => String::from("is a directory"),
        fs::FsError::Denied => String::from("denied"),
        fs::FsError::ReadOnly => String::from("read-only"),
        fs::FsError::NoSpace => String::from("no space"),
        fs::FsError::NotImmutable => String::from("not immutable"),
        fs::FsError::Io(m) => format!("io: {m}"),
    };
    ProgramFailure::Fs(format!("{path}: {what}"))
}

/// Human text for an output failure, same rule.
fn io_fail(e: text::TextError) -> ProgramFailure {
    ProgramFailure::Io(match e {
        text::TextError::Closed => String::from("output closed"),
        text::TextError::Io(m) => format!("io: {m}"),
    })
}

eo9_guest::main! {
    /// `wc <path>…` — print "<lines> <words> <bytes>" per file (suffixed with the path
    /// when more than one is given), plus a total line for several files.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let many = paths.len() > 1;
        let root = fs::default();
        let (mut tl, mut tw, mut tb): (u64, u64, u64) = (0, 0, 0);
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            let st = fs::stat(&root, path.clone()).await.map_err(|e| fs_fail(&path, e))?;
            let file = fs::open(&root, path.clone(), fs::OpenFlags::READ)
                .await
                .map_err(|e| fs_fail(&path, e))?;
            let dst = buffer::with_capacity(st.size);
            let (dst, read_result) = fs::read(&file, 0, dst).await;
            let read = read_result.map_err(|e| fs_fail(&path, e))?;
            let bytes = buffer::prefix_to_vec(&dst, read.bytes_read);
            let contents = String::from_utf8_lossy(&bytes);
            let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64;
            let words = contents.split_whitespace().count() as u64;
            let line = if many {
                format!("{lines} {words} {} {path}", read.bytes_read)
            } else {
                format!("{lines} {words} {}", read.bytes_read)
            };
            text::write_out_line(&line).map_err(io_fail)?;
            tl += lines;
            tw += words;
            tb += read.bytes_read;
        }
        if many {
            text::write_out_line(&format!("{tl} {tw} {tb} total")).map_err(io_fail)?;
        }
        Ok(ProgramSuccess::Counted)
    }
}
