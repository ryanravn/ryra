//! Per-install record. Written at `ryra add` time, read by every command
//! that needs to know how a service was set up. Mirrors the data that used
//! to live in `# Service-*` quadlet header comments.

use crate::capability::Capability;
use crate::error::{Error, Result};
use crate::paths::metadata_path;
use crate::registry::service_def::AuthKind;

/// Per-install record persisted to `~/.local/share/services/<name>/metadata.toml`.
///
/// Exposure isn't stored — it's derived from `url` at read time
/// (absent = Loopback, `.internal` = Internal, `.ts.net` = Tailscale,
/// otherwise Public). One source of truth for "where does this
/// service live."
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Metadata {
    pub registry: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Auth kind: `oidc` if `--auth` was used, otherwise absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthKind>,
    /// Capabilities the service provides — snapshotted from
    /// `service.toml` at install time so [`crate::list_installed`] can
    /// answer "is there an installed reverse proxy / OIDC provider /
    /// SMTP relay / metrics scraper?" without re-reading the registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<Capability>,
}

/// Load metadata.toml for an installed service. Returns `None` if the
/// file doesn't exist (service not installed via this ryra version, or
/// uninstalled), `Err` if it exists but can't be parsed.
pub fn load_metadata(service_name: &str) -> Result<Option<Metadata>> {
    let path = metadata_path(service_name)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path).map_err(|source| Error::FileRead {
        path: path.clone(),
        source,
    })?;
    let meta: Metadata = toml::from_str(&content).map_err(|source| Error::TomlParse {
        path: path.clone(),
        source,
    })?;
    Ok(Some(meta))
}
