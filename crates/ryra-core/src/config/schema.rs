use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::registry::service_def::AuthKind;

/// Top-level preferences.toml configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Ryra version that last wrote this config. Written on every save,
    /// checked on load to reject configs from newer versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Legacy — reads old configs with [host], never written back.
    #[serde(default, skip_serializing)]
    pub host: HostConfig,
    /// Admin email used as the default for services that need an admin account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_email: Option<String>,
    pub smtp: Option<SmtpCredentials>,
    pub auth: Option<AuthCredentials>,
    /// Tailscale auth credential + cached tailnet metadata. Set on first
    /// `--tailscale` install; reused for every subsequent service so the
    /// user only ever pastes their key once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<TailscaleConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<RegistryEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub domain: Option<String>,
}

// --- SMTP ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    #[serde(default)]
    pub security: SmtpSecurity,
}

/// SMTP transport security mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    #[default]
    Starttls,
    ForceTls,
    Off,
}

impl SmtpSecurity {
    pub fn as_str(&self) -> &'static str {
        match self {
            SmtpSecurity::Starttls => "starttls",
            SmtpSecurity::ForceTls => "force_tls",
            SmtpSecurity::Off => "off",
        }
    }
}

// --- Auth ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AuthCredentials {
    /// Managed Authelia instance installed via ryra.
    Authelia { url: String, port: u16 },
    /// External OIDC provider managed by the user.
    External { url: String },
}

impl AuthCredentials {
    pub fn url(&self) -> &str {
        match self {
            AuthCredentials::Authelia { url, .. } => url,
            AuthCredentials::External { url } => url,
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            AuthCredentials::Authelia { .. } => "authelia",
            AuthCredentials::External { .. } => "external",
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            AuthCredentials::Authelia { port, .. } => Some(*port),
            AuthCredentials::External { .. } => None,
        }
    }
}

// --- Caddy local domain ---

/// Hardcoded Caddy domain. Caddy in ryra exists for local HTTPS during
/// development and OIDC testing — services are reachable at
/// `<service>.internal:<caddy_https_port>` from the host. There's no
/// global "TLS provider" config; the URL on each `InstalledService`
/// is the source of truth for how that service is reached, and ryra
/// inspects URL hostnames (`*.internal` → Caddy local) when behavior
/// has to dispatch on it (auth bridge, /etc/hosts writes).
pub const CADDY_LOCAL_DOMAIN: &str = "internal";

// --- Tailscale ---

/// Tag ryra applies to the host advertising services. Required by
/// Tailscale Services (service hosts must be tagged), declared in the
/// tailnet ACL by `ensure_setup`. Single per-tailnet tag — every ryra
/// host shares it.
pub const HOST_TAG: &str = "tag:ryra-host";

/// Tag ryra applies to defined services. Used by autoApprovers in the
/// ACL so every ryra-defined service auto-approves its host without
/// manual admin clicks.
pub const SERVICE_TAG: &str = "tag:ryra-service";

/// Admin API token + cached tailnet metadata for Tailscale Services.
/// Stored in preferences.toml under `[tailscale]` so the user pastes the
/// admin token once and every subsequent `--tailscale` install reuses
/// it for service definition + ACL setup. Same file mode (0600) as
/// SMTP/auth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailscaleConfig {
    /// Admin API token (`tskey-api-…`). Used to manage Tailscale
    /// Services: define services, update ACL with auto-approval, tag
    /// the host. Stored locally because every `--tailscale` install
    /// (and every `--tailscale` removal) calls the API.
    pub admin_api_key: String,
    /// Cached tailnet suffix (e.g. `cobbler-tuna.ts.net`). Resolved
    /// lazily from `tailscale status --json` and remembered so we don't
    /// re-shell out on every install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
}

// --- Registry entry ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub url: String,
}

// --- Installed service record ---

/// In-memory view of a single installed service. Reconstructed by
/// `ryra_core::list_installed()` from the quadlet directory's
/// `# Service-*` headers + the per-service `.env` file. No longer
/// persisted to `preferences.toml` — the on-disk artifacts are the
/// source of truth.
#[derive(Debug, Clone)]
pub struct InstalledService {
    pub name: String,
    pub version: String,
    pub repo: String,
    /// All allocated host ports by name (e.g., "http" → 8080, "tcp" → 5432).
    pub ports: BTreeMap<String, u16>,
    /// The auth kind the user chose when installing this service, if any.
    pub auth_kind: Option<AuthKind>,
    /// How this service is reachable.
    pub exposure: crate::Exposure,
    /// Whether the service was fully installed. Always `true` when
    /// reconstructed from the quadlet scan (a marker'd `.container`
    /// only exists for completed installs).
    pub installed: bool,
}

impl Config {
    /// Validate structural invariants after deserialization.
    pub fn validate(&self) -> Result<(), String> {
        // Future invariants land here. Per-service uniqueness is no
        // longer a Config concern: the source of truth for installed
        // services is the quadlet directory, where each service has a
        // single `.container` by definition.
        let _ = self;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_config_round_trip() {
        let cfg = Config {
            tailscale: Some(TailscaleConfig {
                admin_api_key: "tskey-api-XXXX".into(),
                tailnet: Some("cobbler-tuna.ts.net".into()),
            }),
            ..Config::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("[tailscale]"));
        assert!(serialized.contains("admin_api_key = \"tskey-api-XXXX\""));
        assert!(serialized.contains("tailnet = \"cobbler-tuna.ts.net\""));
        let parsed: Config = toml::from_str(&serialized).unwrap();
        let ts = parsed.tailscale.expect("[tailscale] should round-trip");
        assert_eq!(ts.admin_api_key, "tskey-api-XXXX");
        assert_eq!(ts.tailnet.as_deref(), Some("cobbler-tuna.ts.net"));
    }

    #[test]
    fn tailscale_config_tailnet_optional() {
        // Cached tailnet should be skipped on serialize when None — the
        // first install resolves it lazily and writes it back; serialize
        // shouldn't emit `tailnet = ""` for fresh configs.
        let cfg = Config {
            tailscale: Some(TailscaleConfig {
                admin_api_key: "tskey-api-YYY".into(),
                tailnet: None,
            }),
            ..Config::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("tailnet"));
    }

}
