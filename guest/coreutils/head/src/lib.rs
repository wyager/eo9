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
        text::TextError::Unsupported => String::from("io: unsupported operation"),
        text::TextError::Io(m) => format!("io: {m}"),
    })
}

eo9_guest::main! {
    /// `head --lines <n> <path>…` — print the first `lines` lines of each file (with a
    /// `==> path <==` header when more than one is given).
    async fn main(lines: u64, paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let many = paths.len() > 1;
        let root = fs::default();
        let mut printed = 0u32;
        for (index, path) in paths.into_iter().enumerate() {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            if many {
                if index > 0 {
                    text::write_out_line("").map_err(io_fail)?;
                }
                text::write_out_line(&format!("==> {path} <==")).map_err(io_fail)?;
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
            for line in contents.lines().take(lines as usize) {
                text::write_out_line(line).map_err(io_fail)?;
                printed += 1;
            }
        }
        Ok(ProgramSuccess::Printed(printed))
    }
}
