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
    pub smtp: ProviderStatus,
    pub auth: ProviderStatus,
    pub services: Vec<ServiceInfo>,
}

pub enum ProviderStatus {
    None,
    Configured { name: String },
}

pub struct ServiceInfo {
    pub name: String,
    pub url: Option<String>,
    pub ports: std::collections::BTreeMap<String, u16>,
    pub installed: bool,
}

impl StatusInfo {
    pub fn from_config(config_path: PathBuf, config: &Config) -> Self {
        Self {
            config_path,
            smtp: match &config.smtp {
                None => ProviderStatus::None,
                Some(smtp) => ProviderStatus::Configured {
                    name: smtp.host.clone(),
                },
            },
            auth: match &config.auth {
                None => ProviderStatus::None,
                Some(auth) => ProviderStatus::Configured {
                    name: format!("{} ({})", auth.provider_name(), auth.url()),
                },
            },
            services: config
                .services
                .iter()
                .map(|s| ServiceInfo {
                    name: s.name.clone(),
                    url: s.url.clone(),
                    ports: s.ports.clone(),
                    installed: s.installed,
                })
                .collect(),
        }
    }
}
