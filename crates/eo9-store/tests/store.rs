//! Integration tests for the store: object store, name resolution, and the compile
//! cache, exercised against synthetic data in throwaway store roots.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use eo9_store::{
    CacheKeyParams, CachePolicy, Manifest, Name, ObjectHash, Profile, Store, StoreError,
};

/// A store rooted in a fresh temporary directory, removed on drop.
struct TempStore {
    root: PathBuf,
    store: Store,
}

impl TempStore {
    fn new() -> TempStore {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "eo9-store-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(unique);
        let store = Store::open(&root).expect("open temp store");
        TempStore { root, store }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = chmod_writable_recursive(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Objects are written read-only; make everything writable again so cleanup succeeds on
/// platforms where read-only files resist deletion.
fn chmod_writable_recursive(path: &std::path::Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            chmod_writable_recursive(&entry?.path())?;
        }
    }
    Ok(())
}

fn name(s: &str) -> Name {
    Name::parse(s).unwrap()
}

fn params(modules: &[&[u8]], deterministic: bool) -> CacheKeyParams {
    CacheKeyParams {
        module_hashes: modules.iter().map(|m| ObjectHash::of(m)).collect(),
        configure_constants: vec![("dir".to_owned(), "\"/sandbox\"".to_owned())],
        compile_opts: "{debug-info: false, safepoint-maps: false}".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        compiler_version: "wasmtime-45.0.0".to_owned(),
        compiler_deterministic: deterministic,
    }
}

// ---------------------------------------------------------------------------
// Store layout
// ---------------------------------------------------------------------------

#[test]
fn open_creates_the_layout_and_is_idempotent() {
    let temp = TempStore::new();
    for dir in ["objects", "manifests", "profiles", "cache"] {
        assert!(temp.root.join(dir).is_dir(), "{dir}/ should exist");
    }
    let marker = fs::read_to_string(temp.root.join("version")).unwrap();
    assert_eq!(marker, "eo9-store 1\n");

    // Reopening an existing store is fine; a future layout version is refused.
    Store::open(&temp.root).expect("reopen");
    fs::write(temp.root.join("version"), "eo9-store 99\n").unwrap();
    assert!(matches!(
        Store::open(&temp.root),
        Err(StoreError::Corrupt { .. })
    ));
}

// ---------------------------------------------------------------------------
// Object store
// ---------------------------------------------------------------------------

#[test]
fn add_is_content_addressed_idempotent_and_immutable() {
    let temp = TempStore::new();
    let bytes = b"synthetic component bytes".as_slice();

    let hash = temp.store.add(bytes).unwrap();
    assert_eq!(hash, ObjectHash::of(bytes), "the key is the content hash");
    assert!(temp.store.contains(&hash));
    assert_eq!(temp.store.read_object(&hash).unwrap(), bytes);

    // Adding the same bytes again yields the same hash and the same single object.
    assert_eq!(temp.store.add(bytes).unwrap(), hash);
    assert_eq!(temp.store.objects().unwrap(), vec![hash]);

    // The stored object is read-only.
    let metadata = fs::metadata(temp.store.object_path(&hash)).unwrap();
    assert!(metadata.permissions().readonly());

    // Distinct bytes get distinct objects.
    let other = temp.store.add(b"different bytes").unwrap();
    assert_ne!(other, hash);
    assert_eq!(temp.store.objects().unwrap().len(), 2);
}

#[test]
fn add_file_and_handles_round_trip() {
    let temp = TempStore::new();
    let source = temp.root.join("input.wasm");
    fs::write(&source, b"component from a file").unwrap();

    let hash = temp.store.add_file(&source).unwrap();
    let handle = temp.store.open_object(&hash).unwrap();
    assert_eq!(handle.hash(), &hash);
    assert_eq!(handle.path(), temp.store.object_path(&hash));
    assert_eq!(handle.size().unwrap(), 21);
    assert_eq!(handle.bytes().unwrap(), b"component from a file");
    handle.verify().unwrap();
}

#[test]
fn missing_and_corrupted_objects_are_reported() {
    let temp = TempStore::new();
    let absent = ObjectHash::of(b"never added");
    assert!(!temp.store.contains(&absent));
    assert!(matches!(
        temp.store.open_object(&absent),
        Err(StoreError::MissingObject { hash }) if hash == absent
    ));

    // Tamper with a stored object behind the store's back: verification catches it.
    let hash = temp.store.add(b"original").unwrap();
    let path = temp.store.object_path(&hash);
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    fs::write(&path, b"tampered").unwrap();
    assert!(matches!(
        temp.store.read_object(&hash),
        Err(StoreError::HashMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// Names, manifests, profiles
// ---------------------------------------------------------------------------

#[test]
fn bind_and_resolve_through_the_default_manifest() {
    let temp = TempStore::new();
    let browser = temp.store.add(b"browser component").unwrap();
    let memfs = temp.store.add(b"memfs stub").unwrap();

    temp.store.bind(&name("browser"), browser).unwrap();
    temp.store.bind(&name("fs.memfs"), memfs).unwrap();

    let resolved = temp.store.resolve(&name("browser")).unwrap();
    assert_eq!(resolved.hash, browser);
    assert_eq!(resolved.handle.bytes().unwrap(), b"browser component");

    let resolved = temp.store.resolve(&name("fs.memfs")).unwrap();
    assert_eq!(resolved.hash, memfs);

    let names = temp.store.names().unwrap();
    assert_eq!(names.len(), 2);
    assert_eq!(names[&name("browser")], browser);

    // Unknown names and unbound objects fail loudly.
    assert!(matches!(
        temp.store.resolve(&name("absent")),
        Err(StoreError::UnknownName { .. })
    ));
    assert!(matches!(
        temp.store
            .bind(&name("dangling"), ObjectHash::of(b"not added")),
        Err(StoreError::MissingObject { .. })
    ));
}

#[test]
fn rebinding_repoints_a_name_without_touching_objects() {
    let temp = TempStore::new();
    let v1 = temp.store.add(b"tool v1").unwrap();
    let v2 = temp.store.add(b"tool v2").unwrap();

    temp.store.bind(&name("virtualfs.create"), v1).unwrap();
    assert_eq!(
        temp.store.resolve(&name("virtualfs.create")).unwrap().hash,
        v1
    );

    temp.store.bind(&name("virtualfs.create"), v2).unwrap();
    assert_eq!(
        temp.store.resolve(&name("virtualfs.create")).unwrap().hash,
        v2
    );

    // Both versions still coexist in the object store; only the name moved.
    assert!(temp.store.contains(&v1));
    assert!(temp.store.contains(&v2));
}

#[test]
fn profiles_stack_manifests_with_later_entries_shadowing_earlier_ones() {
    let temp = TempStore::new();
    let base_browser = temp.store.add(b"base browser").unwrap();
    let base_time = temp.store.add(b"base time").unwrap();
    let override_browser = temp.store.add(b"patched browser").unwrap();

    temp.store
        .bind_in("base", &name("browser"), base_browser)
        .unwrap();
    temp.store
        .bind_in("base", &name("time.frozen"), base_time)
        .unwrap();
    temp.store
        .bind_in("overrides", &name("browser"), override_browser)
        .unwrap();

    let mut profile = Profile::new();
    profile.push("base").unwrap();
    profile.push("overrides").unwrap();
    temp.store.write_profile("dev", &profile).unwrap();

    // The later manifest shadows the earlier one; unshadowed names show through.
    assert_eq!(
        temp.store.resolve_in("dev", &name("browser")).unwrap().hash,
        override_browser
    );
    assert_eq!(
        temp.store
            .resolve_in("dev", &name("time.frozen"))
            .unwrap()
            .hash,
        base_time
    );

    let names = temp.store.names_in("dev").unwrap();
    assert_eq!(names[&name("browser")], override_browser);
    assert_eq!(names[&name("time.frozen")], base_time);

    // A profile that names a missing manifest is an error, not a silent skip.
    let mut broken = Profile::new();
    broken.push("missing").unwrap();
    temp.store.write_profile("broken", &broken).unwrap();
    assert!(matches!(
        temp.store.resolve_in("broken", &name("browser")),
        Err(StoreError::MissingManifest { .. })
    ));
}

#[test]
fn manifests_round_trip_through_their_on_disk_form() {
    let temp = TempStore::new();
    let hash = temp.store.add(b"object").unwrap();

    let mut manifest = Manifest::new();
    manifest.set(name("virtualfs"), hash);
    manifest.set(name("virtualfs.create"), hash);
    temp.store.write_manifest("tools", &manifest).unwrap();

    let read_back = temp.store.read_manifest("tools").unwrap().unwrap();
    assert_eq!(read_back, manifest);

    // The file itself is the documented text format.
    let text = fs::read_to_string(temp.root.join("manifests/tools.manifest")).unwrap();
    assert!(text.starts_with("eo9-manifest 1\n"));
    assert!(text.contains(&format!("virtualfs {hash}")));
    assert!(text.contains(&format!("virtualfs.create {hash}")));

    assert!(temp.store.read_manifest("nonexistent").unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Compile cache
// ---------------------------------------------------------------------------

#[test]
fn cache_miss_insert_then_hit() {
    let temp = TempStore::new();
    let params = params(&[b"env", b"app"], false);
    let key = params.key();

    assert!(temp.store.lookup_image(&key).unwrap().is_none());

    let inserted = temp
        .store
        .insert_image(&params, b"compiled image bytes")
        .unwrap();
    assert_eq!(inserted, key);

    let hit = temp.store.lookup_image(&key).unwrap().expect("cache hit");
    assert_eq!(hit.key, key);
    assert_eq!(hit.image, b"compiled image bytes");
    assert_eq!(hit.metadata.image_size, 20);
    assert_eq!(hit.metadata.use_count, 2, "insert counts as first use");
    assert_eq!(hit.metadata.target_triple, params.target_triple);
    assert_eq!(hit.metadata.compiler_version, params.compiler_version);
    assert_eq!(hit.metadata.module_hashes, params.module_hashes);
    assert!(!hit.metadata.compiler_deterministic);

    // Lookups keep bumping the use count.
    let hit = temp.store.lookup_image(&key).unwrap().unwrap();
    assert_eq!(hit.metadata.use_count, 3);

    // A different composition is a different key and a miss.
    let other = self::params(&[b"app"], false).key();
    assert_ne!(other, key);
    assert!(temp.store.lookup_image(&other).unwrap().is_none());

    // Re-inserting an existing key is a no-op, not an error.
    assert_eq!(
        temp.store
            .insert_image(&params, b"compiled image bytes")
            .unwrap(),
        key
    );
}

#[test]
fn second_launch_is_a_cache_hit_and_determinism_flag_separates_keys() {
    let temp = TempStore::new();

    // First "launch": resolve modules, derive the key, miss, compile (synthetically),
    // insert. Second "launch": same inputs derive the same key and hit.
    let env = temp.store.add(b"deterministic env").unwrap();
    let app = temp.store.add(b"app module").unwrap();
    let make_params = |deterministic| CacheKeyParams {
        module_hashes: vec![env, app],
        configure_constants: vec![],
        compile_opts: "{debug-info: false, safepoint-maps: false}".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        compiler_version: "wasmtime-45.0.0".to_owned(),
        compiler_deterministic: deterministic,
    };

    let first = make_params(false);
    assert!(temp.store.lookup_image(&first.key()).unwrap().is_none());
    temp.store.insert_image(&first, b"image-v1").unwrap();

    let second = make_params(false);
    let hit = temp
        .store
        .lookup_image(&second.key())
        .unwrap()
        .expect("second launch hits");
    assert_eq!(hit.image, b"image-v1");

    // Once the compiler is verified deterministic the flag flips, which keys entries
    // separately: nothing produced under the unverified compiler is silently reused.
    let verified = make_params(true);
    assert_ne!(verified.key(), first.key());
    assert!(temp.store.lookup_image(&verified.key()).unwrap().is_none());
}

#[test]
fn gc_enforces_the_budget_and_reports_what_it_did() {
    let temp = TempStore::new();
    let image = vec![0u8; 1000];

    // Three 1000-byte entries with distinct usage patterns.
    let rarely_used = params(&[b"rarely"], false);
    let often_used = params(&[b"often"], false);
    let fresh = params(&[b"fresh"], false);
    temp.store.insert_image(&rarely_used, &image).unwrap();
    temp.store.insert_image(&often_used, &image).unwrap();
    temp.store.insert_image(&fresh, &image).unwrap();
    for _ in 0..5 {
        temp.store.lookup_image(&often_used.key()).unwrap().unwrap();
    }
    temp.store.lookup_image(&fresh.key()).unwrap().unwrap();
    assert_eq!(temp.store.cache_size().unwrap(), 3000);

    // Inside the budget: gc removes nothing.
    let report = temp.store.gc(&CachePolicy { max_bytes: 3000 }).unwrap();
    assert_eq!(report.entries_before, 3);
    assert_eq!(report.bytes_before, 3000);
    assert!(report.evicted.is_empty());

    // Over the budget: the least-used entry goes first; the often-used entry survives.
    let report = temp.store.gc(&CachePolicy { max_bytes: 2000 }).unwrap();
    assert_eq!(report.evicted, vec![rarely_used.key()]);
    assert_eq!(report.bytes_evicted, 1000);
    assert!(
        temp.store
            .lookup_image(&rarely_used.key())
            .unwrap()
            .is_none()
    );
    assert!(
        temp.store
            .lookup_image(&often_used.key())
            .unwrap()
            .is_some()
    );
    assert_eq!(temp.store.cache_size().unwrap(), 2000);

    // Tighter still: only the most frequently used entry survives.
    let report = temp.store.gc(&CachePolicy { max_bytes: 1000 }).unwrap();
    assert_eq!(report.evicted.len(), 1);
    assert!(
        temp.store
            .lookup_image(&often_used.key())
            .unwrap()
            .is_some()
    );

    let entries = temp.store.cache_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, often_used.key());
}
