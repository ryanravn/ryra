use super::schema::*;
use super::state::State;

/// High-level status of the ryra installation.
pub enum RyraStatus {
    NotInitialized,
    Initialized(StatusInfo),
}

pub struct StatusInfo {
    pub domain: String,
    pub cloudflare: CloudflareStatus,
    pub ssl: ProviderStatus,
    pub smtp: ProviderStatus,
    pub auth: ProviderStatus,
    pub registries: Vec<String>,
    pub services: Vec<ServiceInfo>,
    pub next_port: u16,
    pub ports_allocated: usize,
    pub secrets_count: usize,
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
    pub domain: String,
    pub exposure: ExposureMode,
}

impl StatusInfo {
    pub fn from_config_and_state(config: &Config, state: &State) -> Self {
        Self {
            domain: config.host.domain.clone(),
            cloudflare: match &config.cloudflare {
                CloudflareConfig::None => CloudflareStatus::None,
                CloudflareConfig::Configured {
                    zone_name, tunnel, ..
                } => CloudflareStatus::Configured {
                    zone_name: zone_name.clone(),
                    tunnel: tunnel.is_some(),
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
                SmtpConfig::None => ProviderStatus::None,
                SmtpConfig::Configured { host, .. } => ProviderStatus::Configured {
                    name: host.clone(),
                },
            },
            auth: match &config.auth {
                AuthConfig::None => ProviderStatus::None,
                AuthConfig::Authentik { mode, url, .. } => ProviderStatus::Configured {
                    name: match mode {
                        AuthentikMode::Managed => format!("authentik (managed, {url})"),
                        AuthentikMode::External => format!("authentik (external, {url})"),
                    },
                },
            },
            registries: config.registries.iter().map(|r| r.name.clone()).collect(),
            services: config
                .services
                .iter()
                .map(|s| ServiceInfo {
                    name: s.name.clone(),
                    domain: s.domain.clone(),
                    exposure: s.exposure.clone(),
                })
                .collect(),
            next_port: state.next_port,
            ports_allocated: state.allocated.len(),
            secrets_count: state.secrets.len(),
        }
    }
}
