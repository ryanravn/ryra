use serde::{Deserialize, Serialize};

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub host: HostConfig,
    #[serde(default)]
    pub dns: DnsConfig,
    #[serde(default)]
    pub tunnel: TunnelConfig,
    pub ssl: SslConfig,
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

// --- DNS (optional, manages records automatically) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum DnsConfig {
    /// No automatic DNS — user manages records manually.
    None,
    /// Cloudflare DNS with proxy (orange cloud).
    CloudflareProxy {
        api_token: String,
        zone_id: String,
        zone_name: String,
    },
    /// Cloudflare DNS-only (grey cloud).
    CloudflareDns {
        api_token: String,
        zone_id: String,
        zone_name: String,
    },
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self::None
    }
}

impl DnsConfig {
    pub fn cloudflare_credentials(&self) -> Option<(&str, &str, &str)> {
        match self {
            DnsConfig::CloudflareProxy {
                api_token,
                zone_id,
                zone_name,
            }
            | DnsConfig::CloudflareDns {
                api_token,
                zone_id,
                zone_name,
            } => Some((api_token, zone_id, zone_name)),
            DnsConfig::None => None,
        }
    }

    pub fn is_proxied(&self) -> bool {
        matches!(self, DnsConfig::CloudflareProxy { .. })
    }
}

// --- Tunnel (optional, exposes services without port forwarding) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum TunnelConfig {
    /// No tunnel — services exposed via direct port binding.
    None,
    /// Cloudflare Tunnel — outbound connection, no ports needed.
    Cloudflare {
        tunnel_token: String,
        tunnel_id: String,
        account_id: String,
    },
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self::None
    }
}

impl TunnelConfig {
    pub fn is_enabled(&self) -> bool {
        !matches!(self, TunnelConfig::None)
    }
}

// --- SSL ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SslConfig {
    /// Let's Encrypt (DNS-01 with Cloudflare, or HTTP-01 standalone).
    Letsencrypt { email: String },
    /// Cloudflare handles SSL — origin uses self-signed cert.
    /// Valid with CloudflareProxy DNS or Cloudflare Tunnel.
    CloudflareOrigin,
    /// User-provided certs at a custom path.
    Custom { cert_dir: String },
}

// --- SMTP ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SmtpConfig {
    None,
    Configured {
        host: String,
        port: u16,
        username: String,
        password: String,
        from: String,
    },
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self::None
    }
}

// --- Auth ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AuthConfig {
    None,
    Authentik {
        mode: AuthentikMode,
        url: String,
        api_token: String,
    },
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self::None
    }
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
}
