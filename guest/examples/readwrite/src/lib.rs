//! readwrite — the filesystem / async example.
//!
//! Targets the `eo9-examples:readwrite/readwrite` world (see `wit/world.wit`): opens a
//! file, writes its argument through the owned-buffer round-trip, reads it back, and
//! compares. Every fs operation returns a Component Model future; the synchronous
//! `main` drives them with [`eo9_guest::block_on`].

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::buffer;

eo9_guest::bindings!({
    world: "readwrite",
    apis: [io, fs],
});

eo9_guest::main! {
    fn main(path: String, contents: String) -> Result<ProgramSuccess, ProgramFailure> {
        if path.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from(
                "path must not be empty",
            )));
        }

        let fs_failure = |err: fs::FsError| ProgramFailure::Fs(format!("{err:?}"));

        eo9_guest::block_on(async move {
            let root = fs::default();
            let file = fs::open(
                &root,
                &path,
                fs::OpenFlags::READ
                    | fs::OpenFlags::WRITE
                    | fs::OpenFlags::CREATE
                    | fs::OpenFlags::TRUNCATE,
            )
            .await
            .map_err(fs_failure)?;

            // Owned-buffer round-trip, write side: the buffer is transferred to the
            // backend and comes back with the operation's result.
            let src = buffer::from_bytes(contents.as_bytes());
            let (_src, write_result) = fs::write(&file, 0, src).await;
            let written = write_result.map_err(fs_failure)?;

            // Read side: hand over an empty buffer of the right size, get it back full.
            let dst = buffer::with_capacity(contents.len() as u64);
            let (dst, read_result) = fs::read(&file, 0, dst).await;
            let read = read_result.map_err(fs_failure)?;

            if buffer::prefix_to_vec(&dst, read.bytes_read) != contents.as_bytes() {
                return Err(ProgramFailure::Mismatch);
            }
            Ok(ProgramSuccess::RoundTripped(written.bytes_written))
        })
    }
}
