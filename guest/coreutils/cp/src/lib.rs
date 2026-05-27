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

eo9_guest::main! {
    async fn main(src: String, dst: String) -> Result<ProgramSuccess, ProgramFailure> {
        if src.is_empty() || dst.is_empty() {
            return Err(ProgramFailure::BadArguments(String::from("src and dst must not be empty")));
        }
        let fs_err = |e: fs::FsError| ProgramFailure::Fs(format!("{e:?}"));
        let root = fs::default();

        let st = fs::stat(&root, src.clone()).await.map_err(fs_err)?;
        let in_file = fs::open(&root, src, fs::OpenFlags::READ).await.map_err(fs_err)?;
        let dst_buf = buffer::with_capacity(st.size);
        let (dst_buf, read_result) = fs::read(&in_file, 0, dst_buf).await;
        let read = read_result.map_err(fs_err)?;
        let bytes = buffer::prefix_to_vec(&dst_buf, read.bytes_read);

        let out_file = fs::open(
            &root,
            dst,
            fs::OpenFlags::WRITE | fs::OpenFlags::CREATE | fs::OpenFlags::TRUNCATE,
        )
        .await
        .map_err(fs_err)?;
        let src_buf = buffer::from_bytes(&bytes);
        let (_src_buf, write_result) = fs::write(&out_file, 0, src_buf).await;
        let written = write_result.map_err(fs_err)?;
        Ok(ProgramSuccess::Copied(written.bytes_written))
    }
}
