//! Filesystem paths ryra reads and writes.
//!
//! The directory name is `services/` (not `ryra/`) because the deployments
//! are the user's — ryra is just the scaffolding tool that puts them there.
//! Wiping `~/.local/share/services/`, `~/.config/services/`, and the
//! ryra-managed quadlets in `~/.config/containers/systemd/` removes ryra's
//! footprint completely.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Sentinel value for `InstalledService.repo` meaning "shipped with ryra"
/// rather than a user-added custom registry.
pub const REGISTRY_BUNDLED: &str = "bundled";

/// Resolve the user's home directory, falling back to $HOME.
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .ok_or(Error::HomeDirNotFound)
}

/// Root directory holding every installed service's home dir:
/// `~/.local/share/services/`.
pub fn service_data_root() -> Result<PathBuf> {
    let base = match dirs::data_dir() {
        Some(d) => d,
        None => home_dir()?.join(".local").join("share"),
    };
    Ok(base.join("services"))
}

/// Data directory for a service: `~/.local/share/services/<name>`
pub fn service_home(service_name: &str) -> Result<PathBuf> {
    Ok(service_data_root()?.join(service_name))
}

/// Per-install metadata file: `~/.local/share/services/<name>/metadata.toml`.
/// Stores the install-time decisions (registry, exposure, url, auth) so
/// later commands can reconstruct the install without scraping comments.
pub fn metadata_path(service_name: &str) -> Result<PathBuf> {
    Ok(service_home(service_name)?.join("metadata.toml"))
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> Result<PathBuf> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("containers").join("systemd"))
}
