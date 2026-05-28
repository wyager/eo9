//! Pre-compress the eo9.org static assets.
//!
//! Walks the site tree and, for every compressible file, writes a brotli sibling
//! (`<file>.br`) and a gzip sibling (`<file>.gz`) next to it — but only when compression
//! actually pays for itself. The server (`www/src`) serves these siblings by
//! `Accept-Encoding` negotiation and falls back to the original whenever a sibling is
//! missing or older than the original, so a stale or absent variant is never wrong, only
//! slower. Run via `cargo xtask precompress-site`; the outputs are committed alongside the
//! other built site assets (the blob, the jco bundles, the store images).
//!
//! Usage: `eo9-precompress --site <dir> [--quiet]`

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File extensions worth compressing. Already-compressed formats (png, jpeg, woff2, …) are
/// deliberately absent; wasm and the Pulley `.cwasm` images compress extremely well.
const COMPRESSIBLE: &[&str] = &[
    "html", "htm", "css", "js", "mjs", "json", "svg", "xml", "txt", "md", "wasm", "cwasm",
];

/// Files smaller than this are not worth a second request representation.
const MIN_SIZE_BYTES: u64 = 1024;

/// A variant must save at least this fraction of the original size to be emitted.
const MIN_SAVING: f64 = 0.05;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eo9-precompress: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let (site, quiet) = parse_args()?;
    if !site.is_dir() {
        return Err(format!(
            "site directory `{}` does not exist or is not a directory",
            site.display()
        ));
    }

    let mut files = Vec::new();
    collect_files(&site, &mut files)?;
    files.sort();

    let mut emitted = 0usize;
    let mut original_total = 0u64;
    let mut brotli_total = 0u64;
    let mut gzip_total = 0u64;

    for file in &files {
        let Some(report) = process_file(file)? else {
            continue;
        };
        emitted += 1;
        original_total += report.original;
        brotli_total += report.brotli.unwrap_or(report.original);
        gzip_total += report.gzip.unwrap_or(report.original);
        if !quiet {
            println!(
                "precompress: {} {} -> br {} / gz {}",
                relative(file, &site),
                report.original,
                report
                    .brotli
                    .map_or_else(|| "(kept original)".to_owned(), |n| n.to_string()),
                report
                    .gzip
                    .map_or_else(|| "(kept original)".to_owned(), |n| n.to_string()),
            );
        }
    }

    println!(
        "precompress: {} compressible file(s), {} KiB original, {} KiB brotli, {} KiB gzip",
        emitted,
        original_total / 1024,
        brotli_total / 1024,
        gzip_total / 1024
    );
    Ok(())
}

fn parse_args() -> Result<(PathBuf, bool), String> {
    let mut site = None;
    let mut quiet = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--site" => {
                site = Some(PathBuf::from(args.next().ok_or("--site requires a value")?));
            }
            "--quiet" => quiet = true,
            other => return Err(format!("unrecognized argument `{other}` (expected --site)")),
        }
    }
    Ok((site.ok_or("usage: eo9-precompress --site <dir>")?, quiet))
}

/// Recursively collect regular files under `dir`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

struct Report {
    original: u64,
    brotli: Option<u64>,
    gzip: Option<u64>,
}

/// Compress one file if it qualifies. Returns `None` for files that are not compressible
/// (wrong extension, too small, or themselves a `.br`/`.gz` variant). Stale variants of
/// files that no longer qualify are removed so they can never shadow a changed original.
fn process_file(file: &Path) -> Result<Option<Report>, String> {
    let extension = file
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if extension == "br" || extension == "gz" {
        return Ok(None);
    }
    let qualifies = COMPRESSIBLE.contains(&extension.as_str());
    let size = fs::metadata(file)
        .map_err(|err| format!("failed to stat {}: {err}", file.display()))?
        .len();
    if !qualifies || size < MIN_SIZE_BYTES {
        // Never leave variants behind for a file we no longer compress.
        remove_variant(file, "br")?;
        remove_variant(file, "gz")?;
        return Ok(None);
    }

    let bytes =
        fs::read(file).map_err(|err| format!("failed to read {}: {err}", file.display()))?;
    let brotli = write_if_worthwhile(file, "br", &bytes, brotli_compress(&bytes)?)?;
    let gzip = write_if_worthwhile(file, "gz", &bytes, gzip_compress(&bytes)?)?;
    Ok(Some(Report {
        original: size,
        brotli,
        gzip,
    }))
}

/// Write `<file>.<ext>` if the compressed bytes save at least [`MIN_SAVING`]; otherwise make
/// sure no stale variant lingers. Returns the variant size when written.
fn write_if_worthwhile(
    file: &Path,
    ext: &str,
    original: &[u8],
    compressed: Vec<u8>,
) -> Result<Option<u64>, String> {
    let saved_enough = (compressed.len() as f64) <= (original.len() as f64) * (1.0 - MIN_SAVING);
    let variant = variant_path(file, ext);
    if saved_enough {
        fs::write(&variant, &compressed)
            .map_err(|err| format!("failed to write {}: {err}", variant.display()))?;
        Ok(Some(compressed.len() as u64))
    } else {
        remove_variant(file, ext)?;
        Ok(None)
    }
}

fn variant_path(file: &Path, ext: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

fn remove_variant(file: &Path, ext: &str) -> Result<(), String> {
    let variant = variant_path(file, ext);
    if variant.exists() {
        fs::remove_file(&variant)
            .map_err(|err| format!("failed to remove {}: {err}", variant.display()))?;
    }
    Ok(())
}

/// Brotli at maximum quality with the largest window: this runs at build time, so spend the
/// CPU once and save it on every transfer.
fn brotli_compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 24,
        ..Default::default()
    };
    brotli::enc::BrotliCompress(&mut std::io::Cursor::new(bytes), &mut out, &params)
        .map_err(|err| format!("brotli compression failed: {err}"))?;
    Ok(out)
}

fn gzip_compress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(bytes)
        .map_err(|err| format!("gzip compression failed: {err}"))?;
    encoder
        .finish()
        .map_err(|err| format!("gzip compression failed: {err}"))
}

fn relative<'a>(file: &'a Path, root: &Path) -> std::borrow::Cow<'a, str> {
    file.strip_prefix(root).unwrap_or(file).to_string_lossy()
}
