use serde::{Deserialize, Serialize};

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub host: HostConfig,
    #[serde(default)]
    pub cloudflare: CloudflareConfig,
    pub ssl: Option<SslConfig>,
    #[serde(default)]
    pub smtp: SmtpConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub registries: Vec<RegistryEntry>,
    #[serde(default)]
    pub services: Vec<InstalledService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub domain: String,
}

// --- Cloudflare (credentials + shared tunnel resource) ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CloudflareConfig {
    /// No Cloudflare — user manages DNS manually.
    #[default]
    None,
    /// Cloudflare API configured, optional tunnel.
    Configured {
        api_token: String,
        zone_id: String,
        zone_name: String,
        #[serde(default)]
        tunnel: Option<TunnelInfo>,
    },
}


impl CloudflareConfig {
    pub fn credentials(&self) -> Option<(&str, &str, &str)> {
        match self {
            CloudflareConfig::Configured {
                api_token,
                zone_id,
                zone_name,
                ..
            } => Some((api_token, zone_id, zone_name)),
            CloudflareConfig::None => None,
        }
    }

    pub fn tunnel_info(&self) -> Option<&TunnelInfo> {
        match self {
            CloudflareConfig::Configured { tunnel, .. } => tunnel.as_ref(),
            CloudflareConfig::None => None,
        }
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
}

impl ExposureMode {
    /// What modes are available given the current Cloudflare config?
    pub fn available_modes(cf: &CloudflareConfig) -> Vec<ExposureMode> {
        match cf {
            CloudflareConfig::None => vec![ExposureMode::Local],
            CloudflareConfig::Configured { tunnel: None, .. } => {
                vec![ExposureMode::Local, ExposureMode::DnsOnly, ExposureMode::Proxy]
            }
            CloudflareConfig::Configured { tunnel: Some(_), .. } => {
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

    pub fn label(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "tunnel",
            ExposureMode::Proxy => "proxy",
            ExposureMode::DnsOnly => "dns-only",
            ExposureMode::Local => "local",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ExposureMode::Tunnel => "CF tunnel routes traffic, no open ports needed",
            ExposureMode::Proxy => "CF proxy (orange cloud), DDoS protection + caching",
            ExposureMode::DnsOnly => "CF DNS (grey cloud), Let's Encrypt SSL",
            ExposureMode::Local => "localhost only, no DNS or tunnel",
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SmtpConfig {
    #[default]
    None,
    Configured {
        host: String,
        port: u16,
        username: String,
        password: String,
        from: String,
    },
}


// --- Auth ---

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AuthConfig {
    #[default]
    None,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledService {
    pub name: String,
    pub domain: String,
    pub version: String,
    pub exposure: ExposureMode,
}
