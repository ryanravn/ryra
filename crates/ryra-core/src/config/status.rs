use std::path::PathBuf;

use super::schema::*;
use crate::metadata::load_metadata;

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
    /// Backup repo configuration + count of services with
    /// `backup_enabled = true` in their metadata. Absent when no
    /// `[backup]` section exists in preferences.toml.
    pub backup: Option<BackupSummary>,
    /// Tailscale identity + count of services exposed via a tailscale
    /// URL. Absent when no `[tailscale]` section exists. The presence
    /// of a tailscale config alone is enough to surface the line;
    /// `advertised` may be zero if the user pasted a token but hasn't
    /// `ryra add --tailscale`'d anything yet.
    pub tailscale: Option<TailscaleSummary>,
    pub services: Vec<ServiceInfo>,
}

pub enum ProviderStatus {
    None,
    Configured { name: String },
}

pub struct BackupSummary {
    /// Short label for the backend (`"S3 (http://127.0.0.1:9000)"`,
    /// `"local (/var/backups/ryra)"`). Designed to fit on one line.
    pub backend_label: String,
    /// Number of installed services with `backup_enabled = true`.
    pub included: usize,
}

pub struct TailscaleSummary {
    /// Number of installed services with a `.ts.net` exposure.
    pub advertised: usize,
}

pub struct ServiceInfo {
    pub name: String,
    pub url: Option<String>,
    pub ports: std::collections::BTreeMap<String, u16>,
    pub installed: bool,
    /// True when the service's `metadata.toml` records
    /// `backup_enabled = true`. Used by `ryra status` to count
    /// services included in the next backup run.
    pub backup_enabled: bool,
    /// True when the service's URL classifies as a Tailscale
    /// exposure (`.ts.net` hostname). Used to count services
    /// advertised on the tailnet.
    pub tailscale_exposed: bool,
}

impl StatusInfo {
    pub fn from_config(config_path: PathBuf, config: &Config) -> Self {
        let services: Vec<ServiceInfo> = crate::list_installed()
            .unwrap_or_default()
            .into_iter()
            .map(|s| {
                // Per-install backup_enabled lives in metadata.toml, not
                // in the installed-service summary. Load it best-effort
                // — a missing metadata.toml just reads as "not enrolled."
                let backup_enabled = load_metadata(&s.name)
                    .ok()
                    .flatten()
                    .map(|m| m.backup_enabled)
                    .unwrap_or(false);
                let url = s.exposure.url().map(|u| u.to_string());
                let tailscale_exposed = url.as_deref().is_some_and(crate::is_tailscale_url);
                ServiceInfo {
                    name: s.name,
                    url,
                    ports: s.ports,
                    installed: s.installed,
                    backup_enabled,
                    tailscale_exposed,
                }
            })
            .collect();

        let backup = config.backup.as_ref().map(|b| BackupSummary {
            backend_label: format_backend(&b.backend),
            included: services.iter().filter(|s| s.backup_enabled).count(),
        });

        let tailscale = config.tailscale.as_ref().map(|_| TailscaleSummary {
            advertised: services.iter().filter(|s| s.tailscale_exposed).count(),
        });

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
            backup,
            tailscale,
            services,
        }
    }
}

/// One-line label for a backup backend. Strips credentials, keeps the
/// shape ("S3 at http://…", "local /path") so the user can recognise
/// where their snapshots go without exposing the secret key.
fn format_backend(backend: &BackupBackend) -> String {
    match backend {
        BackupBackend::S3 {
            endpoint, bucket, ..
        } => format!("S3 ({endpoint}/{bucket})"),
        BackupBackend::Local { path } => format!("local ({})", path.display()),
        BackupBackend::Managed => "ryra-managed".to_string(),
    }
}
