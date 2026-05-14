use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    // Recompile when registry files change: include_dir! embeds them at
    // compile time but cargo doesn't track them automatically.
    let registry = Path::new("registry");
    if registry.is_dir() {
        walk_dir(registry);
    }

    // Generate a hash of all registry file contents so ensure_bundled()
    // can detect when the embedded registry has changed, even if the
    // crate version hasn't been bumped (common during development).
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if registry.is_dir() {
        collect_files(registry, registry, &mut files);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, content) in &files {
        std::hash::Hash::hash(path, &mut hasher);
        std::hash::Hash::hash(content, &mut hasher);
    }
    let hash = std::hash::Hasher::finish(&hasher);
    println!("cargo::rustc-env=REGISTRY_HASH={hash:016x}");
}

fn walk_dir(dir: &Path) {
    println!("cargo::rerun-if-changed={}", dir.display());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_dir(&path);
            } else {
                println!("cargo::rerun-if-changed={}", path.display());
            }
        }
    }
}

fn collect_files(dir: &Path, base: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, base, out);
            } else if let Ok(content) = std::fs::read(&path) {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                out.insert(rel.to_string_lossy().to_string(), content);
            }
        }
    }
}
