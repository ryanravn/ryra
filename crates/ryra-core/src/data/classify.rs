use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::manifest;

/// Classify top-level children of a service home dir into `(data, ephemeral)`.
///
/// Source of truth is `service.manifest` — the per-install render manifest
/// written by core, listing every file `Step::WriteFile` emitted. A
/// top-level child is **ephemeral** if it's the manifest itself, the `.env`
/// file (deliberately excluded from the manifest because it carries
/// rotated secrets), or if any manifest entry's path equals or lives
/// inside it. Everything else is **data**: bind-mount dirs the registry
/// declared (`db-data/`, `storage-data/`), user-dropped files, and runtime
/// artifacts a container wrote.
///
/// When the manifest is absent — a pre-manifest install, or an orphan
/// left after `--preserve` (where the manifest itself was wiped along
/// with the rest of the ephemerals) — the home dir contains only data by
/// construction, so every top-level child is reported as data. That's
/// the safe default: better to over-preserve than to wipe something we
/// don't recognise.
///
/// Both vecs are sorted by path for deterministic output. All entries
/// are absolute paths rooted at `home_dir`.
pub fn classify_home_dir(home_dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut data = Vec::new();
    let mut ephemeral = Vec::new();
    if !home_dir.exists() {
        return Ok((data, ephemeral));
    }

    let manifest_path = home_dir.join(manifest::MANIFEST_FILENAME);
    let manifest_entries: Vec<PathBuf> = if manifest_path.exists() {
        let content = std::fs::read_to_string(&manifest_path).map_err(|source| Error::FileRead {
            path: manifest_path.clone(),
            source,
        })?;
        let (entries, _envs) = manifest::parse(&content)?;
        entries.into_iter().map(|e| e.path).collect()
    } else {
        Vec::new()
    };
    let have_manifest = manifest_path.exists();

    let entries = std::fs::read_dir(home_dir).map_err(|source| Error::FileRead {
        path: home_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: home_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();

        if !have_manifest {
            // Orphan or pre-manifest install: nothing in the dir is
            // recognisably ryra-generated, so preserve all of it.
            data.push(path);
            continue;
        }

        // The manifest itself and `.env` are excluded from the manifest's
        // own listing by design (chicken-and-egg for the manifest;
        // rotated secrets for `.env`). Both are ryra-managed and
        // regenerable, so they're ephemeral.
        if path == manifest_path || path.file_name().and_then(|n| n.to_str()) == Some(".env") {
            ephemeral.push(path);
            continue;
        }

        // A top-level child is ephemeral if it equals a manifest entry
        // (file case) or contains one (directory case, e.g. `configs/`
        // holding `configs/scripts/foo.sh`).
        let is_ephemeral = manifest_entries
            .iter()
            .any(|m| m == &path || m.starts_with(&path));
        if is_ephemeral {
            ephemeral.push(path);
        } else {
            data.push(path);
        }
    }
    data.sort();
    ephemeral.sort();
    Ok((data, ephemeral))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestEntry, format};
    use std::fs;

    fn write_manifest(home: &Path, paths: &[&Path]) {
        let entries: Vec<ManifestEntry> = paths
            .iter()
            .map(|p| ManifestEntry {
                path: p.to_path_buf(),
                sha256: "0".repeat(64),
            })
            .collect();
        fs::write(
            home.join(manifest::MANIFEST_FILENAME),
            format(&entries, &[]),
        )
        .unwrap();
    }

    #[test]
    fn classifies_manifest_listed_files_as_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Files ryra generated: .container, .network, metadata.toml, configs/.
        fs::write(home.join("svc.container"), "[Container]").unwrap();
        fs::write(home.join("svc.network"), "[Network]").unwrap();
        fs::write(home.join("metadata.toml"), "").unwrap();
        fs::create_dir(home.join("configs")).unwrap();
        fs::write(home.join("configs").join("nginx.conf"), "").unwrap();
        // .env is not in the manifest by design but is still ephemeral.
        fs::write(home.join(".env"), "FOO=bar").unwrap();

        // User data: bind-mount dirs the registry declared.
        fs::create_dir(home.join("db-data")).unwrap();
        fs::create_dir(home.join("storage-data")).unwrap();

        write_manifest(
            home,
            &[
                &home.join("svc.container"),
                &home.join("svc.network"),
                &home.join("metadata.toml"),
                &home.join("configs").join("nginx.conf"),
            ],
        );

        let (data, eph) = classify_home_dir(home).unwrap();
        let eph_names: Vec<String> = eph
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let data_names: Vec<String> = data
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            eph_names,
            vec![
                ".env".to_string(),
                "configs".to_string(),
                "metadata.toml".to_string(),
                "service.manifest".to_string(),
                "svc.container".to_string(),
                "svc.network".to_string(),
            ]
        );
        assert_eq!(
            data_names,
            vec!["db-data".to_string(), "storage-data".to_string()]
        );
    }

    #[test]
    fn user_dropped_files_are_preserved_as_data() {
        // A file the user manually placed in the home dir isn't in the
        // manifest, so the classifier treats it as data.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::write(home.join("svc.container"), "").unwrap();
        fs::write(home.join("my-notes.txt"), "remember to back this up").unwrap();
        write_manifest(home, &[&home.join("svc.container")]);

        let (data, eph) = classify_home_dir(home).unwrap();
        let data_names: Vec<String> = data
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let eph_names: Vec<String> = eph
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(data_names, vec!["my-notes.txt".to_string()]);
        assert_eq!(
            eph_names,
            vec!["service.manifest".to_string(), "svc.container".to_string()]
        );
    }

    #[test]
    fn missing_manifest_classifies_everything_as_data() {
        // Pre-manifest install or post-preserve orphan: no manifest, so
        // everything in the home dir is treated as data (safe default).
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        fs::create_dir(home.join("db-data")).unwrap();
        fs::write(home.join("leftover.txt"), "").unwrap();

        let (data, eph) = classify_home_dir(home).unwrap();
        assert!(eph.is_empty());
        let data_names: Vec<String> = data
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            data_names,
            vec!["db-data".to_string(), "leftover.txt".to_string()]
        );
    }

    #[test]
    fn missing_home_dir_returns_empty() {
        let (data, eph) = classify_home_dir(Path::new("/nonexistent-xyz-123")).unwrap();
        assert!(data.is_empty());
        assert!(eph.is_empty());
    }
}
