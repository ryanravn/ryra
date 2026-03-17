use serde::{Deserialize, Serialize};

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub domain: String,
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
    /// Cloudflare grey cloud; A record, Let's Encrypt cert.
    DnsOnly,
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
        match cf {
            None => vec![ExposureMode::Local],
            Some(CloudflareCredentials { tunnel: None, .. }) => {
                vec![ExposureMode::Local, ExposureMode::DnsOnly, ExposureMode::Proxy]
            }
            Some(CloudflareCredentials { tunnel: Some(_), .. }) => {
                vec![
                    ExposureMode::Local,
                    ExposureMode::DnsOnly,
                    ExposureMode::Proxy,
                    ExposureMode::Tunnel,
                ]
            }
        }
    }

    pub fn needs_cert(&self) -> bool {
        matches!(self, ExposureMode::DnsOnly)
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
            ExposureMode::Tunnel | ExposureMode::Proxy | ExposureMode::DnsOnly
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "tunnel",
            ExposureMode::Proxy => "proxy",
            ExposureMode::DnsOnly => "dns-only",
            ExposureMode::Local => "local",
            ExposureMode::HostPort => "host-port",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "CF tunnel routes traffic, no open ports needed",
            ExposureMode::Proxy => "CF proxy (orange cloud), DDoS protection + caching",
            ExposureMode::DnsOnly => "CF DNS (grey cloud), Let's Encrypt SSL",
            ExposureMode::Local => "localhost only, no DNS or tunnel",
            ExposureMode::HostPort => "bind to 0.0.0.0, reachable from network, no nginx/domain",
        }
    }
}

impl std::fmt::Display for ExposureMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// --- SSL (optional, only for DnsOnly mode) ---

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
}
