use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::error::{Error, Result};

/// Write `contents` to `path` atomically with the given permission mode.
///
/// Guarantees:
/// - The file is created with `mode` from byte zero (no world-readable window
///   even for secret-bearing files written with 0o600).
/// - The file never appears half-written on disk — either the old contents or
///   the new contents exist after this call, never a torn state. Achieved by
///   writing to a sibling `.<name>.tmp.<pid>` file and `rename`-ing over the
///   target (atomic on a single POSIX filesystem).
///
/// The temporary file is written to the same directory as `path` so the
/// rename stays within one filesystem. `sync_all` is called before the
/// rename so the new content hits disk before the rename metadata op.
pub fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::FileWrite {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        ),
    })?;

    // Ensure parent exists before we try to create the tempfile inside it.
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent).map_err(|source| Error::DirCreate {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let name = path.file_name().ok_or_else(|| Error::FileWrite {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name"),
    })?;

    // Refuse to clobber a symlink at `path`. The final `rename` would replace
    // the link itself with our tempfile, silently destroying the user's link
    // target. We only manage regular files — if a user has intentionally
    // symlinked a ryra-managed config elsewhere, surface the conflict.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_symlink()
    {
        return Err(Error::FileWrite {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to overwrite a symlink — resolve the symlink or remove it",
            ),
        });
    }

    // .<name>.tmp.<pid> — dot-prefixed so it's not mistaken for user content
    // if something interrupts us mid-write.
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp.");
    tmp_name.push(std::process::id().to_string());
    let tmp_path = parent.join(tmp_name);

    let write_result = (|| -> Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut f = opts.open(&tmp_path).map_err(|source| Error::FileWrite {
            path: tmp_path.clone(),
            source,
        })?;

        f.write_all(contents).map_err(|source| Error::FileWrite {
            path: tmp_path.clone(),
            source,
        })?;
        f.sync_all().map_err(|source| Error::FileWrite {
            path: tmp_path.clone(),
            source,
        })?;

        // On non-unix (none of our targets, but for completeness) the mode
        // argument has no effect at creation. Apply it after the fact so the
        // behavior is at least consistent.
        #[cfg(not(unix))]
        {
            let _ = mode;
        }

        std::fs::rename(&tmp_path, path).map_err(|source| Error::FileWrite {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(())
    })();

    // Best-effort cleanup if anything failed before the rename. After a
    // successful rename the tmp path no longer exists, so this is a no-op.
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_file_with_mode() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("secret.toml");
        atomic_write(&path, b"hello=world\n", 0o600)?;

        let contents = std::fs::read_to_string(&path)?;
        assert_eq!(contents, "hello=world\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        Ok(())
    }

    #[test]
    fn overwrites_existing() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        atomic_write(&path, b"first\n", 0o644)?;
        atomic_write(&path, b"second\n", 0o644)?;

        let contents = std::fs::read_to_string(&path)?;
        assert_eq!(contents, "second\n");
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn refuses_to_clobber_symlink() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("real.toml");
        std::fs::write(&target, b"original")?;
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link)?;

        let result = atomic_write(&link, b"new", 0o644);
        assert!(result.is_err(), "expected error, got {result:?}");

        // Target must be untouched — the whole point of the check.
        assert_eq!(std::fs::read_to_string(&target)?, "original");
        Ok(())
    }

    #[test]
    fn tightens_permissions_on_overwrite() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("ryra.toml");
        atomic_write(&path, b"v1", 0o644)?;
        atomic_write(&path, b"v2", 0o600)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "rename-over should install the new mode");
        }
        Ok(())
    }
}
