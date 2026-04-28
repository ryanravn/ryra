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

/// Wire-format struct used only on deserialization. Captures both the
/// historical `(url, tailscale_enabled)` pair and the new `exposure`
/// field so existing `preferences.toml` files keep loading after the typed
/// migration. New writes always emit `exposure` (see [`InstalledService`]'s
/// `Serialize`-derived form, which has no legacy fields), so old
/// shapes phase out the next time the config is saved.
#[derive(Deserialize)]
struct InstalledServiceCompat {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
    #[serde(default)]
    pub auth_kind: Option<AuthKind>,
    /// New, typed field. Present on configs written by current ryra.
    #[serde(default)]
    pub exposure: Option<crate::Exposure>,
    /// Legacy fields. Used only when `exposure` is absent (loading a
    /// pre-migration config); ignored otherwise.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub tailscale_enabled: bool,
    #[serde(default)]
    pub installed: bool,
}

impl From<InstalledServiceCompat> for InstalledService {
    fn from(c: InstalledServiceCompat) -> Self {
        let exposure = c.exposure.unwrap_or_else(|| match (c.url, c.tailscale_enabled) {
            (None, _) => crate::Exposure::Loopback,
            (Some(u), true) => crate::Exposure::Tailscale { url: u },
            (Some(u), false) => crate::Exposure::from_url(&u),
        });
        InstalledService {
            name: c.name,
            version: c.version,
            repo: c.repo,
            ports: c.ports,
            auth_kind: c.auth_kind,
            exposure,
            installed: c.installed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "InstalledServiceCompat")]
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
    /// How this service is reachable. Replaces the old `url` +
    /// `tailscale_enabled` pair so consumers pattern-match a single
    /// typed value instead of reconstructing the variant from string
    /// inspection. Old configs with `url`/`tailscale_enabled` are
    /// auto-migrated by [`InstalledServiceCompat`].
    pub exposure: crate::Exposure,
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

    #[test]
    fn installed_service_serializes_loopback_exposure() {
        let svc = InstalledService {
            name: "uptime-kuma".into(),
            version: "0.1.0".into(),
            repo: "bundled".into(),
            ports: BTreeMap::new(),
            auth_kind: None,
            exposure: crate::Exposure::Loopback,
            installed: true,
        };
        let s = toml::to_string(&svc).unwrap();
        assert!(s.contains(r#"kind = "loopback""#));
        // No legacy fields leaked into the new serialized form.
        assert!(!s.contains("tailscale_enabled"));
        assert!(!s.contains("url ="));
    }

    #[test]
    fn installed_service_serializes_tailscale_exposure() {
        let svc = InstalledService {
            name: "seafile".into(),
            version: "0.1.0".into(),
            repo: "bundled".into(),
            ports: BTreeMap::new(),
            auth_kind: None,
            exposure: crate::Exposure::Tailscale {
                url: "https://seafile.foo.ts.net".into(),
            },
            installed: true,
        };
        let s = toml::to_string(&svc).unwrap();
        assert!(s.contains(r#"kind = "tailscale""#));
        assert!(s.contains("seafile.foo.ts.net"));
    }

    #[test]
    fn installed_service_loads_legacy_url_and_tailscale_fields() {
        // Pre-migration shape: a `[[services]]` table with `url` and
        // `tailscale_enabled` fields and no `exposure`. The compat
        // deserializer should reconstruct the right variant.
        let toml = r#"
            name = "seafile"
            version = "0.1.0"
            repo = "bundled"
            url = "https://seafile.foo.ts.net"
            tailscale_enabled = true
            installed = true
        "#;
        let svc: InstalledService = toml::from_str(toml).unwrap();
        match svc.exposure {
            crate::Exposure::Tailscale { url } => assert_eq!(url, "https://seafile.foo.ts.net"),
            other => panic!("expected Tailscale, got {other:?}"),
        }
    }

    #[test]
    fn installed_service_loads_legacy_internal_url_no_tailscale() {
        let toml = r#"
            name = "vaultwarden"
            version = "0.1.0"
            url = "https://vault.internal:8443"
            installed = true
        "#;
        let svc: InstalledService = toml::from_str(toml).unwrap();
        match svc.exposure {
            crate::Exposure::Internal { url } => assert_eq!(url, "https://vault.internal:8443"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}
