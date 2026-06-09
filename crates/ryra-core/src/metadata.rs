//! Per-install record. Written at `ryra add` time, read by every command
//! that needs to know how a service was set up. Mirrors the data that used
//! to live in `# Service-*` quadlet header comments.

use crate::capability::Capability;
use crate::error::{Error, Result};
use crate::paths::metadata_path;
use crate::registry::service_def::{AuthKind, Runtime};

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
    /// True if `--backup` was passed at `ryra add` time. Drives
    /// whether `ryra backup run` picks this install up.
    ///
    /// Default `false` so an existing install (written by a ryra
    /// version that pre-dates the backup feature) reads back as
    /// not-enabled rather than as malformed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub backup_enabled: bool,
    /// Whether the user opted in to global-SMTP wiring for this install
    /// (the `--smtp` flag at install time, or "yes" at the interactive
    /// SMTP prompt). Stored as *user intent*, NOT as "SMTP is currently
    /// being rendered" — the latter is gated additionally on
    /// `config.smtp.is_some()` inside the planner. Decoupling lets
    /// `ryra configure` remember the choice across re-renders even when
    /// global SMTP isn't configured yet.
    ///
    /// Default `true` so installs that pre-date this field read back
    /// as opt-in (matches the historical CLI shape: `ryra add` passed
    /// `enable_smtp = true` unconditionally and let the planner gate).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub smtp_enabled: bool,
    /// `[[env_group]]` bundles that were enabled at install time.
    /// Persisted so `ryra configure --disable <group>` and re-renders
    /// know which group members belong in the rendered `.env`. Default
    /// empty for legacy installs (groups are an opt-in feature; an
    /// empty list reads back as "no groups were toggled").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_groups: Vec<String>,
    /// How this service runs: a podman container (default) or a native binary
    /// under systemd --user. Recorded at install time so post-install commands
    /// (remove, list, status, backup) stay runtime-aware from the install
    /// record alone, never depending on the registry (which may drift or be
    /// gone). Absent in legacy installs reads back as `Podman`.
    #[serde(default, skip_serializing_if = "Runtime::is_podman")]
    pub runtime: Runtime,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_true(b: &bool) -> bool {
    *b
}

fn default_true() -> bool {
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_enabled_defaults_false_on_legacy_metadata() {
        // Pre-feature metadata files have no `backup_enabled` key.
        let toml_src = r#"
registry = "default"
"#;
        let meta: Metadata = toml::from_str(toml_src).expect("parse");
        assert!(!meta.backup_enabled);
    }

    #[test]
    fn backup_enabled_round_trips() {
        let meta = Metadata {
            registry: "default".into(),
            url: None,
            auth: None,
            provides: vec![],
            backup_enabled: true,
            smtp_enabled: true,
            enabled_groups: vec![],
        };
        let text = toml::to_string(&meta).expect("serialize");
        assert!(
            text.contains("backup_enabled = true"),
            "serialized form: {text}"
        );
        let parsed: Metadata = toml::from_str(&text).expect("parse");
        assert!(parsed.backup_enabled);
    }

    #[test]
    fn backup_enabled_false_is_omitted_from_serialization() {
        // Reduce visual noise for the common case (every existing
        // service today): when off, the field shouldn't appear at all.
        let meta = Metadata {
            registry: "default".into(),
            url: None,
            auth: None,
            provides: vec![],
            backup_enabled: false,
            smtp_enabled: true,
            enabled_groups: vec![],
        };
        let text = toml::to_string(&meta).expect("serialize");
        assert!(!text.contains("backup_enabled"), "got: {text}");
    }
}
