use super::schema::*;
use super::state::State;

/// High-level status of the ryra installation.
pub enum RyraStatus {
    NotInitialized,
    Initialized(StatusInfo),
}

pub struct StatusInfo {
    pub domain: String,
    pub dns: ProviderStatus,
    pub tunnel: ProviderStatus,
    pub ssl: ProviderStatus,
    pub smtp: ProviderStatus,
    pub auth: ProviderStatus,
    pub registries: Vec<String>,
    pub services: Vec<ServiceInfo>,
    pub next_port: u16,
    pub ports_allocated: usize,
    pub secrets_count: usize,
}

pub enum ProviderStatus {
    None,
    Configured { name: String },
}

pub struct ServiceInfo {
    pub name: String,
    pub domain: String,
}

impl StatusInfo {
    pub fn from_config_and_state(config: &Config, state: &State) -> Self {
        Self {
            domain: config.host.domain.clone(),
            dns: match &config.dns {
                DnsConfig::None => ProviderStatus::None,
                DnsConfig::CloudflareProxy { zone_name, .. } => ProviderStatus::Configured {
                    name: format!("cloudflare proxy ({zone_name})"),
                },
                DnsConfig::CloudflareDns { zone_name, .. } => ProviderStatus::Configured {
                    name: format!("cloudflare dns-only ({zone_name})"),
                },
            },
            tunnel: match &config.tunnel {
                TunnelConfig::None => ProviderStatus::None,
                TunnelConfig::Cloudflare { .. } => ProviderStatus::Configured {
                    name: "cloudflare tunnel".into(),
                },
            },
            ssl: match &config.ssl {
                SslConfig::Letsencrypt { email } => ProviderStatus::Configured {
                    name: format!("letsencrypt ({email})"),
                },
                SslConfig::CloudflareOrigin => ProviderStatus::Configured {
                    name: "cloudflare origin cert".into(),
                },
                SslConfig::Custom { cert_dir } => ProviderStatus::Configured {
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
                })
                .collect(),
            next_port: state.next_port,
            ports_allocated: state.allocated.len(),
            secrets_count: state.secrets.len(),
        }
    }
}
