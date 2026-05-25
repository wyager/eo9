//! Property-style test: a deterministic pseudo-random operation sequence is applied both to
//! the filesystem and to a trivial in-memory model; they must agree at every step, after a
//! full `verify()`, and again after a remount.

use std::collections::{BTreeMap, BTreeSet};

use eofs_core::{Eofs, FormatOptions, MemDevice, NodeKind};

const DEV_SIZE: u64 = 16 * 1024 * 1024;
const STEPS: usize = 400;

/// xorshift64* — deterministic, seedable, no external crates.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

#[derive(Default)]
struct Model {
    files: BTreeMap<String, Vec<u8>>,
    dirs: BTreeSet<String>,
}

impl Model {
    fn parent_exists(&self, path: &str) -> bool {
        match path.rfind('/') {
            Some(0) | None => true,
            Some(at) => self.dirs.contains(&path[..at]),
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    fn dir_is_empty(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        !self.files.keys().any(|p| p.starts_with(&prefix))
            && !self.dirs.iter().any(|p| p.starts_with(&prefix))
    }
}

/// The small namespace the random walk draws paths from.
fn candidate_paths() -> Vec<String> {
    let mut paths = Vec::new();
    for top in ["alpha", "beta", "gamma"] {
        paths.push(format!("/{top}"));
        for sub in ["x", "y"] {
            paths.push(format!("/{top}/{sub}"));
            for leaf in ["one", "two"] {
                paths.push(format!("/{top}/{sub}/{leaf}"));
            }
        }
    }
    paths
}

fn check_agreement(fs: &Eofs<MemDevice>, model: &Model) {
    fn walk(
        fs: &Eofs<MemDevice>,
        dir: &str,
        files: &mut BTreeMap<String, Vec<u8>>,
        dirs: &mut BTreeSet<String>,
    ) {
        for name in fs.list(dir).unwrap() {
            let path = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            let stat = fs.stat(&path).unwrap();
            match stat.kind {
                NodeKind::Directory => {
                    dirs.insert(path.clone());
                    walk(fs, &path, files, dirs);
                }
                NodeKind::File => {
                    let mut buf = vec![0u8; stat.size as usize];
                    assert_eq!(fs.read(&path, 0, &mut buf).unwrap(), buf.len());
                    files.insert(path, buf);
                }
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    walk(fs, "/", &mut files, &mut dirs);
    assert_eq!(files, model.files);
    assert_eq!(dirs, model.dirs);
}

#[test]
fn random_operations_match_a_simple_model() {
    let mut rng = Rng(0xe0f5_0001_dead_beef);
    let mut fs = Eofs::format(MemDevice::new(DEV_SIZE), &FormatOptions::default()).unwrap();
    let mut model = Model::default();
    let paths = candidate_paths();

    for step in 0..STEPS {
        let path = &paths[rng.below(paths.len() as u64) as usize];
        match rng.below(10) {
            // mkdir
            0 | 1 => {
                if model.parent_exists(path) && !model.exists(path) {
                    fs.mkdir(path).unwrap();
                    model.dirs.insert(path.clone());
                }
            }
            // create
            2 | 3 => {
                if model.parent_exists(path) && !model.exists(path) {
                    fs.create_file(path).unwrap();
                    model.files.insert(path.clone(), Vec::new());
                }
            }
            // write at a random offset
            4..=6 => {
                if model.files.contains_key(path) {
                    let offset = rng.below(12_000);
                    let len = rng.below(9_000) as usize + 1;
                    let data = rng.bytes(len);
                    fs.write(path, offset, &data).unwrap();
                    let content = model.files.get_mut(path).unwrap();
                    let end = offset as usize + len;
                    if content.len() < end {
                        content.resize(end, 0);
                    }
                    content[offset as usize..end].copy_from_slice(&data);
                }
            }
            // remove
            7 => {
                if model.files.contains_key(path) {
                    fs.remove(path).unwrap();
                    model.files.remove(path);
                } else if model.dirs.contains(path) && model.dir_is_empty(path) {
                    fs.remove(path).unwrap();
                    model.dirs.remove(path);
                }
            }
            // commit
            8 => {
                fs.commit().unwrap();
            }
            // gc (keeps the image from outgrowing the small test device)
            _ => {
                fs.commit().unwrap();
                fs.gc().unwrap();
            }
        }
        if step % 50 == 49 {
            check_agreement(&fs, &model);
        }
    }

    fs.commit().unwrap();
    check_agreement(&fs, &model);
    fs.verify().unwrap();

    // Everything also holds after a remount of the raw image.
    let fs = Eofs::mount(fs.unmount()).unwrap();
    check_agreement(&fs, &model);
    fs.verify().unwrap();
}
