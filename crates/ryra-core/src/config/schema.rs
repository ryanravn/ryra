use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::registry::service_def::AuthKind;

/// Top-level ryra.toml configuration.
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
    #[serde(default)]
    pub services: Vec<InstalledService>,
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

/// Auth credential + cached tailnet metadata. Stored in ryra.toml under
/// `[tailscale]` so the user pastes their auth key once and every
/// subsequent `--tailscale` install reuses it. Same file mode (0600) as
/// SMTP/auth credentials, no separate secret file to manage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailscaleConfig {
    /// Auth credential — accepts both `tskey-auth-…` (pre-auth key) and
    /// `tskey-client-…` (OAuth client secret). The Tailscale container
    /// detects the format from the prefix; ryra doesn't care which one.
    pub auth_key: String,
    /// Cached tailnet suffix (e.g. `cobbler-tuna.ts.net`). Resolved
    /// lazily from `tailscale status --json` and remembered so we don't
    /// re-shell out on every install. Empty `Option` means "resolve on
    /// next install".
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledService {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub repo: String,
    /// All allocated host ports by name (e.g., "http" → 8080, "tcp" → 5432).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// The auth kind the user chose when installing this service, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_kind: Option<AuthKind>,
    /// Public URL for this service (browser-visible, e.g., https://docs.example.com).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// True when this service was installed with `--tailscale` (or "Tailscale"
    /// picked in the exposure prompt). Means the service has a sibling
    /// `ts-<name>` sidecar quadlet running tailscaled in a shared netns,
    /// joined to the user's tailnet as its own device. `ryra remove` uses
    /// this to know it has to tear down the sidecar and its state volume.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tailscale_enabled: bool,
    /// Whether the service was fully installed (all steps completed).
    /// Services with `installed: false` are partially installed and can be
    /// retried with `ryra add` or cleaned up with `ryra remove`.
    #[serde(default)]
    pub installed: bool,
}

impl Config {
    /// Validate structural invariants after deserialization.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for svc in &self.services {
            if !seen.insert(&svc.name) {
                return Err(format!("duplicate service '{}' in config", svc.name));
            }
        }
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
                auth_key: "tskey-auth-XXXX".into(),
                tailnet: Some("cobbler-tuna.ts.net".into()),
            }),
            ..Config::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("[tailscale]"));
        assert!(serialized.contains("auth_key = \"tskey-auth-XXXX\""));
        assert!(serialized.contains("tailnet = \"cobbler-tuna.ts.net\""));
        let parsed: Config = toml::from_str(&serialized).unwrap();
        let ts = parsed.tailscale.expect("[tailscale] should round-trip");
        assert_eq!(ts.auth_key, "tskey-auth-XXXX");
        assert_eq!(ts.tailnet.as_deref(), Some("cobbler-tuna.ts.net"));
    }

    #[test]
    fn tailscale_config_tailnet_optional() {
        // Cached tailnet should be skipped on serialize when None — the
        // first install resolves it lazily and writes it back; serialize
        // shouldn't emit `tailnet = ""` for fresh configs.
        let cfg = Config {
            tailscale: Some(TailscaleConfig {
                auth_key: "tskey-client-YYY".into(),
                tailnet: None,
            }),
            ..Config::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("tailnet"));
    }

    #[test]
    fn installed_service_skips_tailscale_when_false() {
        // Default-false: don't write `tailscale_enabled = false` on every
        // service that doesn't use it — keeps ryra.toml tidy.
        let svc = InstalledService {
            name: "uptime-kuma".into(),
            version: "0.1.0".into(),
            repo: "bundled".into(),
            ports: BTreeMap::new(),
            auth_kind: None,
            url: None,
            tailscale_enabled: false,
            installed: true,
        };
        let s = toml::to_string(&svc).unwrap();
        assert!(!s.contains("tailscale_enabled"));
    }

    #[test]
    fn installed_service_writes_tailscale_when_true() {
        let svc = InstalledService {
            name: "seafile".into(),
            version: "0.1.0".into(),
            repo: "bundled".into(),
            ports: BTreeMap::new(),
            auth_kind: None,
            url: None,
            tailscale_enabled: true,
            installed: true,
        };
        let s = toml::to_string(&svc).unwrap();
        assert!(s.contains("tailscale_enabled = true"));
    }
}
