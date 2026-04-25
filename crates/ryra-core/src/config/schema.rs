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
    /// `tailscale serve --https=<port>` allocation for this service when
    /// `--tailscale` is used. Drawn from `TAILSCALE_HTTPS_PORTS` because
    /// `tailscale serve` only binds those three ports for HTTPS. Persisted
    /// so subsequent `ryra add --tailscale` calls don't re-allocate the
    /// same port, and `ryra remove` knows what to tear down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale_port: Option<u16>,
    /// Whether the service was fully installed (all steps completed).
    /// Services with `installed: false` are partially installed and can be
    /// retried with `ryra add` or cleaned up with `ryra remove`.
    #[serde(default)]
    pub installed: bool,
}

/// HTTPS ports `tailscale serve` is allowed to bind. Tailscale enforces
/// this at the daemon level; ryra allocates from the pool when services
/// are added with `--tailscale` and refuses the 4th request with a clear
/// error pointing at the constraint.
pub const TAILSCALE_HTTPS_PORTS: [u16; 3] = [443, 8443, 10000];

/// Failure modes for `allocate_tailscale_port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailscalePortError {
    /// All three of {443, 8443, 10000} are already in use by other services.
    PoolExhausted { taken: Vec<u16> },
}

impl std::fmt::Display for TailscalePortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailscalePortError::PoolExhausted { taken } => {
                let taken_str: Vec<String> = taken.iter().map(|p| p.to_string()).collect();
                write!(
                    f,
                    "tailscale serve only supports 3 HTTPS ports per node \
                     (443, 8443, 10000) and all are already used by ryra services: {}.\n\
                     Options: drop --tailscale on this service (run on plain HTTP and \
                     reach via tailnet IP), or front a path-routing reverse proxy on \
                     one of the existing ports.",
                    taken_str.join(", "),
                )
            }
        }
    }
}

/// Pick a free Tailscale HTTPS port for a new service, given the ports
/// already taken by services in `config.services`.
pub fn allocate_tailscale_port(config: &Config) -> Result<u16, TailscalePortError> {
    let taken: Vec<u16> = config
        .services
        .iter()
        .filter_map(|s| s.tailscale_port)
        .collect();
    for port in TAILSCALE_HTTPS_PORTS {
        if !taken.contains(&port) {
            return Ok(port);
        }
    }
    Err(TailscalePortError::PoolExhausted { taken })
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

    fn config_with_tailscale_ports(ports: &[u16]) -> Config {
        let services: Vec<InstalledService> = ports
            .iter()
            .enumerate()
            .map(|(i, p)| InstalledService {
                name: format!("svc-{i}"),
                version: "0.1.0".into(),
                repo: "bundled".into(),
                ports: BTreeMap::new(),
                auth_kind: None,
                url: None,
                tailscale_port: Some(*p),
                installed: true,
            })
            .collect();
        Config {
            services,
            ..Config::default()
        }
    }

    #[test]
    fn allocate_first_tailscale_port_is_443() {
        // Empty config → first allocation is the canonical primary port.
        let cfg = Config::default();
        assert_eq!(allocate_tailscale_port(&cfg), Ok(443));
    }

    #[test]
    fn allocate_skips_taken_ports_in_order() {
        let cfg = config_with_tailscale_ports(&[443]);
        assert_eq!(allocate_tailscale_port(&cfg), Ok(8443));
        let cfg = config_with_tailscale_ports(&[443, 8443]);
        assert_eq!(allocate_tailscale_port(&cfg), Ok(10000));
    }

    #[test]
    fn allocate_handles_out_of_order_takes() {
        // User installed services in a non-canonical order: 10000, then 443.
        // Allocator should still find 8443 — it's the *value* not the order.
        let cfg = config_with_tailscale_ports(&[10000, 443]);
        assert_eq!(allocate_tailscale_port(&cfg), Ok(8443));
    }

    #[test]
    fn allocate_errors_when_pool_exhausted() {
        let cfg = config_with_tailscale_ports(&[443, 8443, 10000]);
        let err = allocate_tailscale_port(&cfg).unwrap_err();
        match err {
            TailscalePortError::PoolExhausted { taken } => {
                // Order is the order ports appear in services, not the canonical order.
                assert_eq!(taken.len(), 3);
                assert!(taken.contains(&443));
                assert!(taken.contains(&8443));
                assert!(taken.contains(&10000));
            }
        }
    }

    #[test]
    fn pool_exhausted_display_names_taken_ports_and_options() {
        let err = TailscalePortError::PoolExhausted {
            taken: vec![443, 8443, 10000],
        };
        let s = format!("{err}");
        assert!(s.contains("443"));
        assert!(s.contains("8443"));
        assert!(s.contains("10000"));
        assert!(s.contains("path-routing")); // points user at the workaround
    }

}
