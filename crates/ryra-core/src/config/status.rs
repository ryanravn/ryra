use std::path::PathBuf;

use super::schema::*;

/// High-level status of the ryra installation.
pub enum RyraStatus {
    NotInitialized,
    Initialized(StatusInfo),
    Error(String),
}

pub struct StatusInfo {
    pub config_path: PathBuf,
    pub domain: String,
    pub cloudflare: CloudflareStatus,
    pub ssl: ProviderStatus,
    pub smtp: ProviderStatus,
    pub auth: ProviderStatus,
    pub default_repo: Option<String>,
    pub services: Vec<ServiceInfo>,
}

pub enum CloudflareStatus {
    None,
    Configured {
        zone_name: String,
        tunnel: bool,
    },
}

pub enum ProviderStatus {
    None,
    Configured { name: String },
}

pub struct ServiceInfo {
    pub name: String,
    pub domain: Option<String>,
    pub exposure: ExposureMode,
}

impl StatusInfo {
    pub fn from_config(config_path: PathBuf, config: &Config) -> Self {
        Self {
            config_path,
            domain: config.host.domain.clone().unwrap_or_default(),
            cloudflare: match &config.cloudflare {
                None => CloudflareStatus::None,
                Some(cf) => CloudflareStatus::Configured {
                    zone_name: cf.zone_name.clone(),
                    tunnel: cf.tunnel.is_some(),
                },
            },
            ssl: match &config.ssl {
                None => ProviderStatus::None,
                Some(SslConfig::Letsencrypt { email }) => ProviderStatus::Configured {
                    name: format!("letsencrypt ({email})"),
                },
                Some(SslConfig::Custom { cert_dir }) => ProviderStatus::Configured {
                    name: format!("custom ({cert_dir})"),
                },
            },
            smtp: match &config.smtp {
                None => ProviderStatus::None,
                Some(smtp) => ProviderStatus::Configured {
                    name: smtp.host.clone(),
                },
            },
            auth: match &config.auth {
                None => ProviderStatus::None,
                Some(AuthCredentials::Authentik { mode, url, .. }) => ProviderStatus::Configured {
                    name: match mode {
                        AuthentikMode::Managed => format!("authentik (managed, {url})"),
                        AuthentikMode::External => format!("authentik (external, {url})"),
                    },
                },
            },
            default_repo: config.default_repo.clone(),
            services: config
                .services
                .iter()
                .map(|s| ServiceInfo {
                    name: s.name.clone(),
                    domain: s.domain.clone(),
                    exposure: s.exposure.clone(),
                })
                .collect(),
        }
    }
}
