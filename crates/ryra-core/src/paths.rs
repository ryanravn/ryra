//! Filesystem paths ryra reads and writes.
//!
//! The directory name is `services/` (not `ryra/`) because the deployments
//! are the user's — ryra is just the scaffolding tool that puts them there.
//! Wiping `~/.local/share/services/`, `~/.config/services/`, and the
//! ryra-managed quadlets in `~/.config/containers/systemd/` removes ryra's
//! footprint completely.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Sentinel value for `InstalledService.repo` meaning "came from the
/// default registry" (the project-managed git repo at
/// [`DEFAULT_REGISTRY_URL`]) rather than a user-added custom registry.
pub const REGISTRY_DEFAULT: &str = "default";

/// Git URL of the default service registry. Cloned on first
/// `ryra add`/`ryra search` into `<cache>/default/` and updated by
/// `ryra registry update`.
///
/// Tests and dev workflows can short-circuit the clone by setting
/// [`REGISTRY_DIR_ENV`] to a local directory; the resolver uses that
/// path verbatim instead.
pub const DEFAULT_REGISTRY_URL: &str = "https://github.com/ryanravn/ryra-registry.git";

/// Env var that, when set to an existing directory, replaces the git
/// fetch entirely — ryra uses that directory as the default registry
/// verbatim (no clone, no pull). The E2E test harness sets this to
/// `/opt/ryra-test-registry` inside the VM; dev workflows can point it
/// at a local checkout to iterate without committing/pushing.
pub const REGISTRY_DIR_ENV: &str = "RYRA_REGISTRY_DIR";

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
