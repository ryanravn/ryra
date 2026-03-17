use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Ensure a repo is cached and return the path to it.
/// For git URLs: clone or pull. For local paths: symlink.
pub async fn ensure_repo(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    let source_path = Path::new(url);
    if source_path.exists() && source_path.is_dir() {
        ensure_local_repo(source_path, cache_dir)
    } else {
        ensure_git_repo(url, cache_dir).await
    }
}

/// Convert a repo URL/path to a filesystem-safe cache directory name.
fn repo_cache_name(url: &str) -> String {
    url.replace("://", "-")
        .replace(['/', ':', '\\'], "-")
        .trim_matches('-')
        .to_string()
}

/// Clone or update a git repo into the cache directory.
async fn ensure_git_repo(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    let dest = cache_dir.join(repo_cache_name(url));

    if dest.exists() {
        if dest.join(".git").exists() {
            pull_repo(&dest).await?;
        }
    } else {
        clone_repo(url, &dest).await?;
    }

    Ok(dest)
}

async fn clone_repo(url: &str, dest: &Path) -> Result<()> {
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

async fn pull_repo(dest: &Path) -> Result<()> {
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

/// Symlink a local directory as a repo.
fn ensure_local_repo(source: &Path, cache_dir: &Path) -> Result<PathBuf> {
    let canonical = source.canonicalize().map_err(|source_err| Error::FileRead {
        path: source.to_path_buf(),
        source: source_err,
    })?;

    let dest = cache_dir.join(repo_cache_name(&canonical.to_string_lossy()));

    // Already symlinked to the right place
    if dest.is_symlink() {
        if let Ok(target) = std::fs::read_link(&dest) {
            if target == canonical {
                return Ok(dest);
            }
        }
        std::fs::remove_file(&dest).map_err(|source| Error::FileWrite {
            path: dest.clone(),
            source,
        })?;
    } else if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|source| Error::FileWrite {
            path: dest.clone(),
            source,
        })?;
    }

    std::os::unix::fs::symlink(&canonical, &dest).map_err(|source| Error::FileWrite {
        path: dest.clone(),
        source,
    })?;
    Ok(dest)
}
