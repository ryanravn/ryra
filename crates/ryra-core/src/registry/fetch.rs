use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::registry::service_def::ServiceDef;

/// How a registry source is accessed.
enum RegistrySource {
    /// A JSON URL (e.g., `https://registry.ryra.dev/index.json`).
    Json(String),
    /// A git repository URL.
    Git(String),
    /// A local directory path.
    Local(PathBuf),
}

/// Detect the registry source type from a URL/path string.
fn detect_source(url: &str) -> RegistrySource {
    let path = Path::new(url);
    if path.exists() && path.is_dir() {
        return RegistrySource::Local(path.to_path_buf());
    }

    if url.ends_with(".json") {
        return RegistrySource::Json(url.to_string());
    }

    RegistrySource::Git(url.to_string())
}

/// Ensure a registry is cached and return the path to it.
/// Supports JSON URLs, git repos, and local directories.
pub async fn ensure_repo(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    match detect_source(url) {
        RegistrySource::Local(path) => ensure_local_repo(&path, cache_dir),
        RegistrySource::Git(url) => ensure_git_repo(&url, cache_dir).await,
        RegistrySource::Json(url) => ensure_json_registry(&url, cache_dir).await,
    }
}

/// Convert a repo URL/path to a filesystem-safe cache directory name.
fn repo_cache_name(url: &str) -> String {
    url.replace("://", "-")
        .replace(['/', ':', '\\'], "-")
        .trim_matches('-')
        .to_string()
}

// ---------------------------------------------------------------------------
// JSON registry
// ---------------------------------------------------------------------------

/// JSON registry format: a map of service names to their definitions.
#[derive(serde::Deserialize)]
struct JsonRegistry {
    services: BTreeMap<String, ServiceDef>,
}

/// Fetch a JSON registry and write individual service.toml files to cache.
async fn ensure_json_registry(url: &str, cache_dir: &Path) -> Result<PathBuf> {
    let dest = cache_dir.join(repo_cache_name(url));

    let response = reqwest::get(url)
        .await
        .map_err(|e| Error::Registry(format!("failed to fetch registry from {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::Registry(format!(
            "registry at {url} returned {}",
            response.status()
        )));
    }

    let registry: JsonRegistry = response
        .json()
        .await
        .map_err(|e| Error::Registry(format!("failed to parse registry JSON from {url}: {e}")))?;

    // Write each service as {dest}/{name}/service.toml
    for (name, def) in &registry.services {
        let svc_dir = dest.join(name);
        std::fs::create_dir_all(&svc_dir).map_err(|source| Error::DirCreate {
            path: svc_dir.clone(),
            source,
        })?;
        let toml_content =
            toml::to_string_pretty(def).map_err(|e| Error::Registry(format!("failed to serialize {name}: {e}")))?;
        std::fs::write(svc_dir.join("service.toml"), toml_content).map_err(|source| Error::FileWrite {
            path: svc_dir.join("service.toml"),
            source,
        })?;
    }

    Ok(dest)
}

// ---------------------------------------------------------------------------
// Git registry
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Local directory
// ---------------------------------------------------------------------------

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
