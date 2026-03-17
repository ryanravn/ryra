use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Clone or update a git registry into the cache directory.
pub async fn fetch_registry(url: &str, cache_dir: &Path, name: &str) -> Result<PathBuf> {
    let dest = cache_dir.join(name);

    if dest.exists() {
        // Already cached — pull if it's a git repo, otherwise leave as-is
        if dest.join(".git").exists() {
            pull_registry(&dest).await?;
        }
    } else {
        clone_registry(url, &dest).await?;
    }

    Ok(dest)
}

async fn clone_registry(url: &str, dest: &Path) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["clone", "--depth=1", url])
        .arg(dest)
        .output()
        .await
        .map_err(|e| Error::Git(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!("git clone failed: {stderr}")));
    }

    Ok(())
}

async fn pull_registry(dest: &Path) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(dest)
        .output()
        .await
        .map_err(|e| Error::Git(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Git(format!("git pull failed: {stderr}")));
    }

    Ok(())
}

/// Symlink a local directory as a registry — changes are reflected immediately.
pub fn add_local_registry(source: &Path, cache_dir: &Path, name: &str) -> Result<PathBuf> {
    let dest = cache_dir.join(name);
    if dest.exists() || dest.is_symlink() {
        if dest.is_symlink() {
            std::fs::remove_file(&dest)
        } else {
            std::fs::remove_dir_all(&dest)
        }
        .map_err(|source| Error::FileWrite {
            path: dest.clone(),
            source,
        })?;
    }
    let canonical = source.canonicalize().map_err(|source_err| Error::FileRead {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    std::os::unix::fs::symlink(&canonical, &dest).map_err(|source| Error::FileWrite {
        path: dest.clone(),
        source,
    })?;
    Ok(dest)
}

