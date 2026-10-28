// A fingerprint of source trees, for a runtime library's ABI symbol: shared,
// by `include!`, between the runtime's `build.rs` and `meadow-llvm`'s, which
// must answer alike for the same trees -- the code generator names the symbol
// the runtime's library exports.
//
// FNV-1a over every `.rs` file's path, relative to its root, and contents, in
// sorted order: stable across toolchains, unlike `DefaultHasher`.

/// The fingerprint of the `.rs` files under `roots`, as sixteen hex digits,
/// telling `cargo` to build again when any of them changes.
fn fingerprint(roots: &[std::path::PathBuf]) -> String {
    use std::hash::Hasher;
    // Canonical, so that two build scripts reaching the same trees by
    // different `..` paths sort the files the same way.
    let roots: Vec<std::path::PathBuf> = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()))
        .collect();
    let mut files = Vec::new();
    for root in &roots {
        println!("cargo:rerun-if-changed={}", root.display());
        walk(root, &mut files);
    }
    files.sort();
    let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
    for file in &files {
        let rel = roots
            .iter()
            .find_map(|r| file.strip_prefix(r).ok())
            .unwrap_or(file);
        // Separators as `/`, so a checkout on Windows answers as one on Unix.
        hash.write(rel.to_string_lossy().replace('\\', "/").as_bytes());
        hash.write(&std::fs::read(file).unwrap_or_default());
    }
    format!("{:016x}", hash.finish())
}

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

struct Fnv(u64);

impl std::hash::Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
