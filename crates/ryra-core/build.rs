fn main() {
    // Recompile when registry files change — include_dir! embeds them at
    // compile time but cargo doesn't track them automatically.
    // Walk the directory tree so changes to individual files are detected,
    // not just directory-level mtime changes.
    let registry = std::path::Path::new("../../registry");
    if registry.is_dir() {
        walk_dir(registry);
    }
}

fn walk_dir(dir: &std::path::Path) {
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
