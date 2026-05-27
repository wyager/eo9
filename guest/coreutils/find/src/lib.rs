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

eo9_guest::main! {
    async fn main(path: String, name: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("path must not be empty")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let io_err = |e: text::TextError| ProgramFailure::Io(format!("{e:?}"));
        let matches = |candidate: &str| name.is_empty() || candidate.contains(name.as_str());

        let root = fs::default();
        let mut found = 0u32;

        // The start node itself: print if its basename matches, recurse if it is a dir.
        let start = fs::stat(&root, path.clone()).await.map_err(fs_err)?;
        let start_name = path.rsplit('/').next().unwrap_or(path.as_str());
        if matches(start_name) {
            text::write_out_line(&path).map_err(io_err)?;
            found += 1;
        }

        let mut stack: Vec<String> = Vec::new();
        if matches!(start.kind, fs::NodeKind::Directory) {
            stack.push(path);
        }
        while let Some(dir) = stack.pop() {
            let entries = fs::list_directory(&root, dir.clone()).await.map_err(fs_err)?;
            for entry in entries {
                let child = join(&dir, &entry);
                let st = fs::stat(&root, child.clone()).await.map_err(fs_err)?;
                if matches(&entry) {
                    text::write_out_line(&child).map_err(io_err)?;
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
