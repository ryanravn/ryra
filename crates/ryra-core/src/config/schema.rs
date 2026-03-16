use serde::{Deserialize, Serialize};

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub host: HostConfig,
    #[serde(default)]
    pub dns: DnsConfig,
    #[serde(default)]
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
    pub data_dir: String,
}

// --- DNS ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum DnsConfig {
    None,
    Cloudflare {
        api_token: String,
        zone_id: String,
    },
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self::None
    }
}

// --- SSL ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum SslConfig {
    None,
    Letsencrypt { email: String },
}

impl Default for SslConfig {
    fn default() -> Self {
        Self::None
    }
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
