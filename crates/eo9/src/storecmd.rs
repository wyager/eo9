//! `eo9 store …`: the module-store subcommands — add, ls, gc, reseed.

use std::path::PathBuf;

use eo9_store::{CachePolicy, Name};

use crate::cli::{ArgStream, Config, EXIT_SUCCESS, consume_global_options, expect_end, vlog};
use crate::seed;

pub fn cmd_store(stream: &mut ArgStream, cfg: &mut Config) -> Result<u8, String> {
    consume_global_options(stream, cfg)?;
    let Some(action) = stream.next() else {
        return Err("`store` needs an action: add, ls, gc, or reseed".to_string());
    };
    match action.as_str() {
        "add" => store_add(stream, cfg),
        "ls" => store_ls(stream, cfg),
        "gc" => store_gc(stream, cfg),
        "reseed" => store_reseed(stream, cfg),
        other => Err(format!(
            "unknown store action `{other}`: expected add, ls, gc, or reseed"
        )),
    }
}

/// `eo9 store reseed`: re-bind the bundled program names to the components carried by
/// this binary. With a seed record (any store this version has seeded), names the user
/// re-bound — and every name added with `store add --name` — stay theirs; on a store
/// without one (seeded by an older eo9), every bundled name is refreshed. Objects are
/// never deleted, so nothing is lost either way.
fn store_reseed(stream: &mut ArgStream, cfg: &mut Config) -> Result<u8, String> {
    consume_global_options(stream, cfg)?;
    expect_end(stream, "store reseed")?;
    let store = cfg.open_store()?;
    let refreshed = seed::reseed(cfg, &store)?;
    if refreshed == 0 {
        println!("bundled programs already match this eo9 binary");
    } else {
        println!("refreshed {refreshed} bundled program(s) to this eo9 binary's components");
    }
    Ok(EXIT_SUCCESS)
}

/// `eo9 store add <path> [--name <dotted-name>]`: add a component file to the
/// content-addressed store and optionally bind a bare dotted name to it.
fn store_add(stream: &mut ArgStream, cfg: &mut Config) -> Result<u8, String> {
    let mut path: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    loop {
        consume_global_options(stream, cfg)?;
        match stream.next() {
            None => break,
            Some(token) if token == "--name" => {
                name = Some(
                    stream
                        .next()
                        .ok_or_else(|| "option `--name` needs a value".to_string())?,
                );
            }
            Some(token) if token.starts_with('-') => {
                return Err(format!("unknown option `{token}` for `store add`"));
            }
            Some(token) if path.is_none() => path = Some(PathBuf::from(token)),
            Some(token) => {
                return Err(format!(
                    "unexpected extra argument `{token}` for `store add`"
                ));
            }
        }
    }
    let path = path.ok_or_else(|| "`store add` needs a file path".to_string())?;

    let store = cfg.open_store()?;
    let hash = store.add_file(&path).map_err(|err| err.to_string())?;
    vlog!(cfg, "added {} as object {hash}", path.display());
    println!("{hash}");
    if let Some(name) = name {
        let name = Name::parse(&name).map_err(|err| err.to_string())?;
        store.bind(&name, hash).map_err(|err| err.to_string())?;
        println!("{name} -> {hash}");
    }
    Ok(EXIT_SUCCESS)
}

/// `eo9 store ls`: list name bindings, object count, and compile-cache entries.
fn store_ls(stream: &mut ArgStream, cfg: &mut Config) -> Result<u8, String> {
    consume_global_options(stream, cfg)?;
    expect_end(stream, "store ls")?;
    let store = cfg.open_store()?;

    let names = store.names().map_err(|err| err.to_string())?;
    println!("names ({}):", names.len());
    for (name, hash) in &names {
        println!("  {name} {hash}");
    }

    let objects = store.objects().map_err(|err| err.to_string())?;
    println!("objects: {}", objects.len());

    let entries = store.cache_entries().map_err(|err| err.to_string())?;
    let total: u64 = entries.iter().map(|entry| entry.metadata.image_size).sum();
    println!("compile cache ({} entries, {total} bytes):", entries.len());
    for entry in &entries {
        println!(
            "  {} {} bytes, used {} time(s)",
            entry.key, entry.metadata.image_size, entry.metadata.use_count
        );
    }
    Ok(EXIT_SUCCESS)
}

/// `eo9 store gc [--max-cache-bytes <n>]`: evict compile-cache entries down to a size
/// budget (default: the store's 4 GiB provisional budget) and sweep stale temp files.
fn store_gc(stream: &mut ArgStream, cfg: &mut Config) -> Result<u8, String> {
    let mut max_bytes = CachePolicy::DEFAULT_MAX_BYTES;
    loop {
        consume_global_options(stream, cfg)?;
        match stream.next() {
            None => break,
            Some(token) if token == "--max-cache-bytes" => {
                let value = stream
                    .next()
                    .ok_or_else(|| "option `--max-cache-bytes` needs a value".to_string())?;
                max_bytes = value.parse().map_err(|err| {
                    format!("invalid --max-cache-bytes value {value:?} (bytes expected): {err}")
                })?;
            }
            Some(token) => {
                return Err(format!("unexpected argument `{token}` for `store gc`"));
            }
        }
    }

    let store = cfg.open_store()?;
    let report = store
        .gc(&CachePolicy { max_bytes })
        .map_err(|err| err.to_string())?;
    println!(
        "gc: evicted {} of {} cache entries ({} of {} bytes), removed {} stale temp file(s)",
        report.evicted.len(),
        report.entries_before,
        report.bytes_evicted,
        report.bytes_before,
        report.stale_tmp_removed
    );
    Ok(EXIT_SUCCESS)
}
