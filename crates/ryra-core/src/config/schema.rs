use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::registry::service_def::AuthKind;

/// Top-level ryra.toml configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
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

impl Default for Config {
    fn default() -> Self {
        Self {
            host: HostConfig::default(),
            admin_email: None,
            smtp: None,
            auth: None,
            registries: vec![],
            services: vec![],
        }
    }
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
    /// Domain assigned to this service (used for Caddy reverse proxy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
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
