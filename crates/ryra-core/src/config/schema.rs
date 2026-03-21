use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Legacy — reads old configs with [host], never written back.
    #[serde(default, skip_serializing)]
    pub host: HostConfig,
    pub cloudflare: Option<CloudflareCredentials>,
    pub ssl: Option<SslConfig>,
    pub smtp: Option<SmtpCredentials>,
    pub auth: Option<AuthCredentials>,
    #[serde(default)]
    pub default_repo: Option<String>,
    /// Legacy field — reads old configs with [[registries]], never written back.
    #[serde(default, skip_serializing)]
    pub registries: Vec<RegistryEntry>,
    #[serde(default)]
    pub services: Vec<InstalledService>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub domain: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: HostConfig::default(),
            cloudflare: None,
            ssl: None,
            smtp: None,
            auth: None,
            default_repo: Some(crate::DEFAULT_REPO.to_string()),
            registries: vec![],
            services: vec![],
        }
    }
}

impl Config {
    /// Derive the base domain from cloudflare zone_name, falling back to legacy host.domain.
    pub fn base_domain(&self) -> Option<&str> {
        self.cloudflare
            .as_ref()
            .map(|cf| cf.zone_name.as_str())
            .or(self.host.domain.as_deref())
    }
}

// --- Cloudflare (credentials + shared tunnel resource) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareCredentials {
    pub api_token: String,
    pub zone_id: String,
    pub zone_name: String,
    pub tunnel: Option<TunnelInfo>,
}

impl CloudflareCredentials {
    pub fn credentials(&self) -> (&str, &str, &str) {
        (&self.api_token, &self.zone_id, &self.zone_name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelInfo {
    pub tunnel_token: String,
    pub tunnel_id: String,
    pub account_id: String,
}

// --- Per-service exposure mode ---

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExposureMode {
    /// Cloudflare Tunnel routes traffic; nginx HTTP on localhost, no certs.
    Tunnel,
    /// Cloudflare orange cloud; A record proxied, self-signed origin cert.
    Proxy,
    /// Cloudflare grey cloud; A record, Let's Encrypt DNS-01 cert.
    DnsOnly,
    /// nginx reverse proxy with your own domain + SSL (LE HTTP-01 or custom certs). No Cloudflare.
    Public,
    /// No DNS, no tunnel; localhost only.
    Local,
    /// Binds to 0.0.0.0; reachable from network, no nginx/domain/DNS.
    HostPort,
}

impl ExposureMode {
    /// What modes are available given the current Cloudflare config and service capabilities?
    /// `has_nginx` means the service can be proxied (has an HTTP interface).
    pub fn available_modes(cf: &Option<CloudflareCredentials>, has_nginx: bool) -> Vec<ExposureMode> {
        if !has_nginx {
            return vec![ExposureMode::Local, ExposureMode::HostPort];
        }
        let mut modes = vec![ExposureMode::Local, ExposureMode::HostPort, ExposureMode::Public];
        match cf {
            Some(CloudflareCredentials { tunnel: None, .. }) => {
                modes.extend([ExposureMode::DnsOnly, ExposureMode::Proxy]);
            }
            Some(CloudflareCredentials { tunnel: Some(_), .. }) => {
                modes.extend([ExposureMode::DnsOnly, ExposureMode::Proxy, ExposureMode::Tunnel]);
            }
            None => {}
        }
        modes
    }

    pub fn needs_cert(&self) -> bool {
        matches!(self, ExposureMode::DnsOnly | ExposureMode::Public)
    }

    pub fn needs_origin_cert(&self) -> bool {
        matches!(self, ExposureMode::Proxy)
    }

    pub fn needs_dns_record(&self) -> bool {
        matches!(self, ExposureMode::Proxy | ExposureMode::DnsOnly)
    }

    pub fn needs_tunnel_route(&self) -> bool {
        matches!(self, ExposureMode::Tunnel)
    }

    pub fn is_proxied(&self) -> bool {
        matches!(self, ExposureMode::Proxy)
    }

    /// Whether this mode requires a domain and nginx proxy.
    pub fn needs_domain(&self) -> bool {
        matches!(
            self,
            ExposureMode::Tunnel | ExposureMode::Proxy | ExposureMode::DnsOnly | ExposureMode::Public
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "cloudflare-tunnel",
            ExposureMode::Proxy => "cloudflare-proxy",
            ExposureMode::DnsOnly => "cloudflare",
            ExposureMode::Public => "public",
            ExposureMode::Local => "local",
            ExposureMode::HostPort => "host",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "no open ports needed, traffic routed through tunnel",
            ExposureMode::Proxy => "DDoS protection + caching, orange cloud",
            ExposureMode::DnsOnly => "DNS + Let's Encrypt SSL",
            ExposureMode::Public => "nginx reverse proxy, your own domain + SSL",
            ExposureMode::Local => "localhost only, no DNS or tunnel",
            ExposureMode::HostPort => "bind to 0.0.0.0, reachable from network, no nginx/domain",
        }
    }

    /// All modes a service can support based on its capabilities (ignoring config state).
    pub fn supported_modes(has_nginx: bool) -> Vec<ExposureMode> {
        if has_nginx {
            vec![
                ExposureMode::Local,
                ExposureMode::HostPort,
                ExposureMode::Public,
                ExposureMode::DnsOnly,
                ExposureMode::Proxy,
                ExposureMode::Tunnel,
            ]
        } else {
            vec![ExposureMode::Local, ExposureMode::HostPort]
        }
    }

    /// What global config sections are missing for this exposure mode?
    pub fn missing_config(&self, config: &Config) -> Vec<ConfigRequirement> {
        let mut missing = Vec::new();
        match self {
            ExposureMode::Tunnel => {
                if config.cloudflare.is_none() {
                    missing.push(ConfigRequirement::Cloudflare);
                    missing.push(ConfigRequirement::CloudflareTunnel);
                } else if config
                    .cloudflare
                    .as_ref()
                    .and_then(|cf| cf.tunnel.as_ref())
                    .is_none()
                {
                    missing.push(ConfigRequirement::CloudflareTunnel);
                }
            }
            ExposureMode::Proxy => {
                if config.cloudflare.is_none() {
                    missing.push(ConfigRequirement::Cloudflare);
                }
            }
            ExposureMode::DnsOnly => {
                if config.cloudflare.is_none() {
                    missing.push(ConfigRequirement::Cloudflare);
                }
                if config.ssl.is_none() {
                    missing.push(ConfigRequirement::Ssl);
                }
            }
            ExposureMode::Public => {
                if config.ssl.is_none() {
                    missing.push(ConfigRequirement::Ssl);
                }
            }
            ExposureMode::Local | ExposureMode::HostPort => {}
        }
        missing
    }
}

// --- Config requirements for exposure modes ---

/// What global config section an exposure mode needs that may be missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRequirement {
    Cloudflare,
    CloudflareTunnel,
    Ssl,
}

impl std::fmt::Display for ExposureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// --- SSL (for Public and DnsOnly exposure modes) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SslConfig {
    /// Let's Encrypt (DNS-01 with Cloudflare, or HTTP-01 standalone).
    Letsencrypt { email: String },
    /// User-provided certs at a custom path.
    Custom { cert_dir: String },
}

// --- SMTP ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

// --- Auth ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AuthCredentials {
    Authentik {
        mode: AuthentikMode,
        url: String,
        api_token: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthentikMode {
    Managed,
    External,
}

// --- Registry entry ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub url: String,
}

// --- Installed service record ---

/// How the installed service was deployed (stored in config for removal).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstalledDeployMode {
    #[default]
    Quadlet,
    Compose,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledService {
    pub name: String,
    pub domain: Option<String>,
    pub version: String,
    pub exposure: ExposureMode,
    #[serde(default)]
    pub deploy_mode: InstalledDeployMode,
    #[serde(default)]
    pub repo: String,
    /// Allocated host port for web services (nginx upstream). None for non-web.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    /// All allocated host ports by name (e.g., "http" → 8080, "tcp" → 5432).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
}
