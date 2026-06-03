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
    /// `stat <path>…` — print each node's kind and size (prefixed with the path when
    /// more than one is given).
    async fn main(paths: Vec<String>) -> Result<ProgramSuccess, ProgramFailure> {
        if paths.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("at least one path is required")));
        }
        let many = paths.len() > 1;
        let root = fs::default();
        for path in paths {
            if path.is_empty() {
                return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
            }
            let st = fs::stat(&root, path.clone()).await.map_err(|e| fs_fail(&path, e))?;
            let kind = match st.kind {
                fs::NodeKind::File => "file",
                fs::NodeKind::Directory => "directory",
            };
            let line = if many {
                format!("{path}: {kind} {} bytes", st.size)
            } else {
                format!("{kind} {} bytes", st.size)
            };
            text::write_out_line(&line).map_err(io_fail)?;
        }
        Ok(ProgramSuccess::Described)
    }
}
