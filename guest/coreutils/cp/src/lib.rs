//! cp — copy a file from src to dst (eo9:fs only).
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;

use eo9_guest::api::fs::fs;
use eo9_guest::buffer;

eo9_guest::bindings!({
    world: "cp",
    apis: [io, fs],
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

eo9_guest::main! {
    async fn main(src: String, dst: String) -> Result<ProgramSuccess, ProgramFailure> {
        if src.is_empty() || dst.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("src and dst must not be empty")));
        }
        let root = fs::default();

        let st = fs::stat(&root, src.clone()).await.map_err(|e| fs_fail(&src, e))?;
        let in_file = fs::open(&root, src.clone(), fs::OpenFlags::READ)
            .await
            .map_err(|e| fs_fail(&src, e))?;
        let dst_buf = buffer::with_capacity(st.size);
        let (dst_buf, read_result) = fs::read(&in_file, 0, dst_buf).await;
        let read = read_result.map_err(|e| fs_fail(&src, e))?;
        let bytes = buffer::prefix_to_vec(&dst_buf, read.bytes_read);

        let out_file = fs::open(
            &root,
            dst.clone(),
            fs::OpenFlags::WRITE | fs::OpenFlags::CREATE | fs::OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| fs_fail(&dst, e))?;
        let src_buf = buffer::from_bytes(&bytes);
        let (_src_buf, write_result) = fs::write(&out_file, 0, src_buf).await;
        let written = write_result.map_err(|e| fs_fail(&dst, e))?;
        Ok(ProgramSuccess::Copied(written.bytes_written))
    }
}
