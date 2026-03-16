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

/// Copy a local directory as a registry (for development/testing).
pub fn add_local_registry(source: &Path, cache_dir: &Path, name: &str) -> Result<PathBuf> {
    let dest = cache_dir.join(name);
    if dest.exists() {
        // Remove old cached copy
        std::fs::remove_dir_all(&dest).map_err(|source| Error::FileWrite {
            path: dest.clone(),
            source,
        })?;
    }
    copy_dir_recursive(source, &dest)?;
    Ok(dest)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|source| Error::DirCreate {
        path: dst.to_path_buf(),
        source,
    })?;

    let entries = std::fs::read_dir(src).map_err(|source| Error::FileRead {
        path: src.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: src.to_path_buf(),
            source,
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|source| Error::FileWrite {
                path: dst_path,
                source,
            })?;
        }
    }
    Ok(())
}
