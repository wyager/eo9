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
    /// `ls [<path>…]` — list each directory's entries, one per line. With no paths the
    /// root `/` is listed; with several, each group is introduced by a `<path>:` header.
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
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
                    text::write_out_line("").map_err(io_fail)?;
                }
                text::write_out_line(&format!("{path}:")).map_err(io_fail)?;
            }
            let entries = fs::list_directory(&root, path.clone())
                .await
                .map_err(|e| fs_fail(&path, e))?;
            for entry in &entries {
                text::write_out_line(entry).map_err(io_fail)?;
            }
            total += entries.len() as u32;
        }
        Ok(ProgramSuccess::Listed(total))
    }
}
