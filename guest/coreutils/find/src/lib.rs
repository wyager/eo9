//! find — recursively list paths under a directory, optionally filtered by a substring
//! (an empty `name` matches everything). Walk is iterative (a worklist of directories)
//! to avoid async recursion. Capabilities: eo9:fs + eo9:text.
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use eo9_guest::api::fs::fs;
use eo9_guest::text;

eo9_guest::bindings!({
    world: "find",
    apis: [io, fs, text],
});

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

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
    async fn main(path: String, name: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
        }
        let matches = |candidate: &str| name.is_empty() || candidate.contains(name.as_str());

        let root = fs::default();
        let mut found = 0u32;

        // The start node itself: print if its basename matches, recurse if it is a dir.
        let start = fs::stat(&root, path.clone()).await.map_err(|e| fs_fail(&path, e))?;
        let start_name = path.rsplit('/').next().unwrap_or(path.as_str());
        if matches(start_name) {
            text::write_out_line(&path).map_err(io_fail)?;
            found += 1;
        }

        let mut stack: Vec<String> = Vec::new();
        if matches!(start.kind, fs::NodeKind::Directory) {
            stack.push(path);
        }
        while let Some(dir) = stack.pop() {
            let entries = fs::list_directory(&root, dir.clone())
                .await
                .map_err(|e| fs_fail(&dir, e))?;
            for entry in entries {
                let child = join(&dir, &entry);
                let st = fs::stat(&root, child.clone())
                    .await
                    .map_err(|e| fs_fail(&child, e))?;
                if matches(&entry) {
                    text::write_out_line(&child).map_err(io_fail)?;
                    found += 1;
                }
                if matches!(st.kind, fs::NodeKind::Directory) {
                    stack.push(child);
                }
            }
        }
        Ok(ProgramSuccess::Found(found))
    }
}
