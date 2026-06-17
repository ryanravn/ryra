use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Clone a git repo to `dest`, or pull if it already exists.
pub async fn clone_or_pull(url: &str, dest: &Path) -> Result<()> {
    // Serialize fetches on this registry across processes. ryra-api spawns one
    // `ryra rpc` process per request, so several concurrent installs each run
    // `git pull` on the same clone at once; concurrent pulls clobber FETCH_HEAD
    // and one fails with "cannot fast-forward to multiple branches". An advisory
    // lock on a sibling file makes clone/pull mutually exclusive. Blocking to
    // take it is fine: `ryra rpc` (and the CLI) is a one-shot single-request
    // process, so there's no other task on its runtime to starve while it waits.
    let _lock = fetch_lock(dest)?;
    if dest.exists() {
        if dest.join(".git").exists() {
            pull(dest).await?;
        }
    } else {
        clone(url, dest).await?;
    }
    Ok(())
}

/// Acquire the advisory fetch lock for `dest`, held until the returned file is
/// dropped. The lock file is a sibling (`.<name>.fetch.lock`) so it works even
/// before the clone exists.
fn fetch_lock(dest: &Path) -> Result<std::fs::File> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Err(Error::Git(format!(
            "invalid registry path: {}",
            dest.display()
        )));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::Git(format!("create registry cache dir: {e}")))?;
    let lock_path: PathBuf = parent.join(format!(".{}.fetch.lock", name.to_string_lossy()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| Error::Git(format!("open registry fetch lock: {e}")))?;
    file.lock()
        .map_err(|e| Error::Git(format!("acquire registry fetch lock: {e}")))?;
    Ok(file)
}

async fn clone(url: &str, dest: &Path) -> Result<()> {
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

async fn pull(dest: &Path) -> Result<()> {
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
