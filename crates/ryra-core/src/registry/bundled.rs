use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

use crate::error::{Error, Result};

static BUNDLED_REGISTRY: Dir<'static> =
    include_dir!("$CARGO_MANIFEST_DIR/../../registry");

const BUNDLED_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Content hash of the registry at build time — changes when any file is modified,
/// even if the crate version stays the same.
const BUNDLED_HASH: &str = env!("RYRA_REGISTRY_HASH");

/// Ensures the bundled registry is extracted to `<cache_dir>/bundled/`.
///
/// Compares the registry hash against the `VERSION` file in the cache dir.
/// If the hash is missing or mismatched, the old directory is removed and
/// the embedded registry is re-extracted. Returns the path to the extracted
/// registry directory.
pub fn ensure_bundled(cache_dir: &Path) -> Result<PathBuf> {
    let dest = cache_dir.join("bundled");
    let version_file = dest.join("VERSION");

    // Use "version-hash" format so both release upgrades and dev edits trigger re-extraction.
    let expected = format!("{BUNDLED_VERSION}-{BUNDLED_HASH}");
    let needs_extract = if version_file.exists() {
        let cached = std::fs::read_to_string(&version_file).map_err(|source| {
            Error::FileRead {
                path: version_file.clone(),
                source,
            }
        })?;
        cached.trim() != expected
    } else {
        true
    };

    if needs_extract {
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|source| Error::FileWrite {
                path: dest.clone(),
                source,
            })?;
        }

        std::fs::create_dir_all(&dest).map_err(|source| Error::DirCreate {
            path: dest.clone(),
            source,
        })?;

        extract_dir(&BUNDLED_REGISTRY, &dest)?;

        std::fs::write(&version_file, &expected).map_err(|source| Error::FileWrite {
            path: version_file,
            source,
        })?;
    }

    Ok(dest)
}

fn extract_dir(dir: &Dir, dest: &Path) -> Result<()> {
    for file in dir.files() {
        let target = dest.join(file.path());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::DirCreate {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&target, file.contents()).map_err(|source| Error::FileWrite {
            path: target,
            source,
        })?;
    }

    for subdir in dir.dirs() {
        extract_dir(subdir, dest)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extracts_bundled_registry() {
        let tmp = TempDir::new().expect("create temp dir");
        let registry_dir = ensure_bundled(tmp.path()).expect("ensure_bundled succeeds");

        assert!(registry_dir.exists(), "registry dir should exist");

        // At least one service.toml should be present somewhere under the dir
        let found = walkdir_has_service_toml(&registry_dir);
        assert!(found, "at least one service.toml should exist in extracted registry");
    }

    #[test]
    fn skips_extraction_when_version_matches() {
        let tmp = TempDir::new().expect("create temp dir");

        // First extraction
        let registry_dir = ensure_bundled(tmp.path()).expect("first ensure_bundled");

        // Record modification time of VERSION file
        let version_file = registry_dir.join("VERSION");
        let mtime_before = std::fs::metadata(&version_file)
            .expect("VERSION file exists")
            .modified()
            .expect("mtime available");

        // Small sleep to ensure mtime would differ if re-written
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second call — should be a no-op
        let registry_dir2 = ensure_bundled(tmp.path()).expect("second ensure_bundled");
        assert_eq!(registry_dir, registry_dir2, "same path returned");

        let mtime_after = std::fs::metadata(&version_file)
            .expect("VERSION file still exists")
            .modified()
            .expect("mtime available");

        assert_eq!(mtime_before, mtime_after, "VERSION file should not have been re-written");
    }

    #[test]
    fn re_extracts_on_version_mismatch() {
        let tmp = TempDir::new().expect("create temp dir");

        // First extraction
        let registry_dir = ensure_bundled(tmp.path()).expect("first ensure_bundled");

        // Tamper with the VERSION file
        let version_file = registry_dir.join("VERSION");
        std::fs::write(&version_file, "0.0.0-fake").expect("write fake version");

        // Small sleep so mtime would differ
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Second call — should re-extract because version mismatches
        ensure_bundled(tmp.path()).expect("second ensure_bundled");

        let new_version = std::fs::read_to_string(&version_file).expect("read VERSION");
        let expected = format!("{BUNDLED_VERSION}-{BUNDLED_HASH}");
        assert_eq!(
            new_version.trim(),
            expected,
            "VERSION should be updated to current version-hash after re-extraction"
        );
    }

    fn walkdir_has_service_toml(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walkdir_has_service_toml(&path) {
                    return true;
                }
            } else if path.file_name().and_then(|n| n.to_str()) == Some("service.toml") {
                return true;
            }
        }
        false
    }
}
