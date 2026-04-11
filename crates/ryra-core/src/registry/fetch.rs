use std::path::Path;

use crate::error::{Error, Result};

/// Clone a git repo to `dest`, or pull if it already exists.
pub async fn clone_or_pull(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        if dest.join(".git").exists() {
            pull(dest).await?;
        }
    } else {
        clone(url, dest).await?;
    }
    Ok(())
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
