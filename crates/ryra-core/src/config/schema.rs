use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::registry::service_def::AuthKind;

/// Top-level preferences.toml configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Ryra version that last wrote this config. Written on every save,
    /// checked on load to reject configs from newer versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Legacy — reads old configs with [host], never written back.
    #[serde(default, skip_serializing)]
    pub host: HostConfig,
    /// Admin email used as the default for services that need an admin account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_email: Option<String>,
    pub smtp: Option<SmtpCredentials>,
    pub auth: Option<AuthCredentials>,
    /// Tailscale auth credential + cached tailnet metadata. Set on first
    /// `--tailscale` install; reused for every subsequent service so the
    /// user only ever pastes their key once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<TailscaleConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<RegistryEntry>,
    /// Backup repository + encryption password. Set by
    /// `ryra backup configure`; consumed by every `ryra backup run`,
    /// `ryra backup restore`, and `ryra backup list` invocation.
    /// `None` means the user hasn't configured backups yet — every
    /// backup command refuses with [`Error::BackupRepoNotConfigured`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupSettings>,
}

impl Config {
    /// True iff this config carries credentials/tokens that must be
    /// protected from casual disclosure: SMTP user/password, Tailscale
    /// admin API token, and anything similar added in the future.
    /// Callers use this to fire a one-time warning the first time
    /// preferences.toml acquires sensitive content.
    pub fn has_secrets(&self) -> bool {
        self.smtp.is_some() || self.tailscale.is_some() || self.backup.is_some()
    }
}

// --- Backup ---

/// Top-level backup repository configuration. Persisted in
/// preferences.toml under `[backup]`. Storing the password here (vs.
/// requiring it on every invocation) is the only ergonomic way to run
/// `ryra backup run` from a systemd timer — but the file is already
/// 0600 and contains comparably-sensitive SMTP and Tailscale tokens,
/// so the threat model doesn't change.
///
/// Losing this password = losing access to every snapshot. Surfaced
/// once by `ryra backup configure` with a print-and-confirm step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSettings {
    /// The restic encryption password. Forms the only key that can
    /// decrypt the repo's content.
    pub password: String,
    /// Storage backend the snapshots are pushed to. Typed enum
    /// (instead of a raw restic URL string + opaque env map) so
    /// invalid combinations of credentials are unrepresentable and
    /// the CLI can prompt for the right fields per backend.
    pub backend: BackupBackend,
}

/// Storage backend for the backup repository. The variants map to
/// restic's supported backends; each carries exactly the fields restic
/// needs to authenticate, no more.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BackupBackend {
    /// Any S3-compatible object store: MinIO, AWS S3, Backblaze B2 via
    /// S3 API, Cloudflare R2, Wasabi. The `endpoint` is the full URL
    /// to the API (e.g. `http://127.0.0.1:9000` for a local MinIO,
    /// `https://s3.us-east-1.amazonaws.com` for AWS).
    S3 {
        endpoint: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
        /// Optional path prefix inside the bucket. Lets one bucket
        /// host multiple ryra installs (one per host or per user) by
        /// scoping each to a sub-prefix.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// A local filesystem path. Primarily a testing affordance — point
    /// at a tempdir and round-trip backup/restore without spinning up
    /// MinIO. Production users should prefer the S3 variant pointed at
    /// off-machine storage; a "local" backup gives no protection from
    /// disk failure.
    Local { path: std::path::PathBuf },
}

impl BackupBackend {
    /// The `--repo` argument passed to the restic binary. restic uses
    /// a single colon-prefixed string to identify the backend ("s3:",
    /// "rest:", a raw path for local). This builder centralises the
    /// formatting so callers never hand-construct it.
    pub fn restic_repo(&self) -> String {
        match self {
            BackupBackend::S3 {
                endpoint,
                bucket,
                prefix,
                ..
            } => {
                let stripped = endpoint
                    .trim_end_matches('/')
                    .trim_start_matches("http://")
                    .trim_start_matches("https://");
                // Keep the scheme: restic distinguishes
                // s3:http://… (plain HTTP) from s3:https://….
                let scheme = if endpoint.starts_with("http://") {
                    "http://"
                } else {
                    "https://"
                };
                let base = format!("s3:{scheme}{stripped}/{bucket}");
                match prefix.as_deref().map(|p| p.trim_matches('/')) {
                    Some(p) if !p.is_empty() => format!("{base}/{p}"),
                    _ => base,
                }
            }
            BackupBackend::Local { path } => path.display().to_string(),
        }
    }

    /// Environment variables restic needs to authenticate to this
    /// backend. Returned as a vec of `(key, value)` pairs so the
    /// caller can decide whether to set them on a `Command` or via
    /// `std::env::set_var` (the former is preferred — keeps the
    /// process env clean and per-invocation).
    pub fn env(&self) -> Vec<(&'static str, String)> {
        match self {
            BackupBackend::S3 {
                access_key_id,
                secret_access_key,
                ..
            } => vec![
                ("AWS_ACCESS_KEY_ID", access_key_id.clone()),
                ("AWS_SECRET_ACCESS_KEY", secret_access_key.clone()),
            ],
            BackupBackend::Local { .. } => vec![],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub domain: Option<String>,
}

// --- SMTP ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpCredentials {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    #[serde(default)]
    pub security: SmtpSecurity,
}

/// Inbucket's internal SMTP container port. Services on the shared podman
/// network reach inbucket by container name, so this (not the host-side
/// `PublishPort=`) is what goes into `config.smtp`.
pub const INBUCKET_SMTP_PORT: u16 = 2500;

impl SmtpCredentials {
    /// SMTP settings for a ryra-managed inbucket install: target the
    /// container by name on the shared podman network, no auth, no TLS.
    /// (The host port isn't reachable from `--no-hosts` containers.)
    pub fn inbucket() -> Self {
        Self {
            host: "inbucket".to_string(),
            port: INBUCKET_SMTP_PORT,
            username: String::new(),
            password: String::new(),
            from: "noreply@example.com".to_string(),
            security: SmtpSecurity::Off,
        }
    }
}

/// SMTP transport security mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    #[default]
    Starttls,
    ForceTls,
    Off,
}

impl SmtpSecurity {
    pub fn as_str(&self) -> &'static str {
        match self {
            SmtpSecurity::Starttls => "starttls",
            SmtpSecurity::ForceTls => "force_tls",
            SmtpSecurity::Off => "off",
        }
    }
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

// --- Caddy local domain ---

/// Hardcoded Caddy domain. Caddy in ryra exists for local HTTPS during
/// development and OIDC testing — services are reachable at
/// `<service>.internal:<caddy_https_port>` from the host. There's no
/// global "TLS provider" config; the URL on each `InstalledService`
/// is the source of truth for how that service is reached, and ryra
/// inspects URL hostnames (`*.internal` → Caddy local) when behavior
/// has to dispatch on it (auth bridge, /etc/hosts writes).
pub const CADDY_LOCAL_DOMAIN: &str = "internal";

// --- Tailscale ---

/// Tag ryra applies to the host advertising services. Required by
/// Tailscale Services (service hosts must be tagged), declared in the
/// tailnet ACL by `ensure_setup`. Single per-tailnet tag — every ryra
/// host shares it.
pub const HOST_TAG: &str = "tag:ryra-host";

/// Tag ryra applies to defined services. Used by autoApprovers in the
/// ACL so every ryra-defined service auto-approves its host without
/// manual admin clicks.
pub const SERVICE_TAG: &str = "tag:ryra-service";

/// Admin API token + cached tailnet metadata for Tailscale Services.
/// Stored in preferences.toml under `[tailscale]` so the user pastes the
/// admin token once and every subsequent `--tailscale` install reuses
/// it for service definition + ACL setup. Same file mode (0600) as
/// SMTP/auth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailscaleConfig {
    /// Admin API token (`tskey-api-…`). Used to manage Tailscale
    /// Services: define services, update ACL with auto-approval, tag
    /// the host. Stored locally because every `--tailscale` install
    /// (and every `--tailscale` removal) calls the API.
    pub admin_api_key: String,
    /// Cached tailnet suffix (e.g. `cobbler-tuna.ts.net`). Resolved
    /// lazily from `tailscale status --json` and remembered so we don't
    /// re-shell out on every install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
}

// --- Registry entry ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub url: String,
}

// --- Installed service record ---

/// In-memory view of a single installed service. Reconstructed by
/// `ryra_core::list_installed()` from the quadlet directory's
/// `# Service-*` headers + the per-service `.env` file. No longer
/// persisted to `preferences.toml` — the on-disk artifacts are the
/// source of truth.
#[derive(Debug, Clone)]
pub struct InstalledService {
    pub name: String,
    pub version: String,
    pub repo: String,
    /// All allocated host ports by name (e.g., "http" → 8080, "tcp" → 5432).
    pub ports: BTreeMap<String, u16>,
    /// The auth kind the user chose when installing this service, if any.
    pub auth_kind: Option<AuthKind>,
    /// How this service is reachable.
    pub exposure: crate::Exposure,
    /// Capabilities this service provides — the persisted snapshot of
    /// `service.toml`'s `[capabilities] provides` taken at install time.
    /// Empty for services whose service.toml didn't declare any (i.e.
    /// most application services, all of which are pure consumers).
    pub provides: Vec<Capability>,
    /// Whether the service was fully installed. Always `true` when
    /// reconstructed from the quadlet scan (a marker'd `.container`
    /// only exists for completed installs).
    pub installed: bool,
}

impl Config {
    /// Validate structural invariants after deserialization.
    pub fn validate(&self) -> Result<(), String> {
        // Future invariants land here. Per-service uniqueness is no
        // longer a Config concern: the source of truth for installed
        // services is the quadlet directory, where each service has a
        // single `.container` by definition.
        let _ = self;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_config_round_trip() {
        let cfg = Config {
            tailscale: Some(TailscaleConfig {
                admin_api_key: "tskey-api-XXXX".into(),
                tailnet: Some("cobbler-tuna.ts.net".into()),
            }),
            ..Config::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("[tailscale]"));
        assert!(serialized.contains("admin_api_key = \"tskey-api-XXXX\""));
        assert!(serialized.contains("tailnet = \"cobbler-tuna.ts.net\""));
        let parsed: Config = toml::from_str(&serialized).unwrap();
        let ts = parsed.tailscale.expect("[tailscale] should round-trip");
        assert_eq!(ts.admin_api_key, "tskey-api-XXXX");
        assert_eq!(ts.tailnet.as_deref(), Some("cobbler-tuna.ts.net"));
    }

    #[test]
    fn tailscale_config_tailnet_optional() {
        // Cached tailnet should be skipped on serialize when None — the
        // first install resolves it lazily and writes it back; serialize
        // shouldn't emit `tailnet = ""` for fresh configs.
        let cfg = Config {
            tailscale: Some(TailscaleConfig {
                admin_api_key: "tskey-api-YYY".into(),
                tailnet: None,
            }),
            ..Config::default()
        };
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("tailnet"));
    }

    #[test]
    fn backup_s3_repo_string_is_restic_compatible() {
        let backend = BackupBackend::S3 {
            endpoint: "http://127.0.0.1:9000".into(),
            bucket: "ryra-backups".into(),
            access_key_id: "minio".into(),
            secret_access_key: "minio123".into(),
            prefix: None,
        };
        assert_eq!(
            backend.restic_repo(),
            "s3:http://127.0.0.1:9000/ryra-backups"
        );
    }

    #[test]
    fn backup_s3_repo_with_prefix() {
        let backend = BackupBackend::S3 {
            endpoint: "https://s3.eu-west-1.amazonaws.com".into(),
            bucket: "shared-bucket".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            prefix: Some("hosts/laptop".into()),
        };
        assert_eq!(
            backend.restic_repo(),
            "s3:https://s3.eu-west-1.amazonaws.com/shared-bucket/hosts/laptop"
        );
    }

    #[test]
    fn backup_s3_trims_trailing_endpoint_slashes() {
        // Sloppy user input shouldn't double-slash the resulting URL —
        // restic accepts both but the canonical form is cleaner.
        let backend = BackupBackend::S3 {
            endpoint: "http://127.0.0.1:9000/".into(),
            bucket: "b".into(),
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            prefix: None,
        };
        assert_eq!(backend.restic_repo(), "s3:http://127.0.0.1:9000/b");
    }

    #[test]
    fn backup_local_repo_is_path_string() {
        let backend = BackupBackend::Local {
            path: "/tmp/ryra-test-repo".into(),
        };
        assert_eq!(backend.restic_repo(), "/tmp/ryra-test-repo");
    }

    #[test]
    fn backup_s3_env_carries_aws_credentials() {
        let backend = BackupBackend::S3 {
            endpoint: "http://127.0.0.1:9000".into(),
            bucket: "b".into(),
            access_key_id: "the_id".into(),
            secret_access_key: "the_secret".into(),
            prefix: None,
        };
        let env: std::collections::HashMap<_, _> = backend.env().into_iter().collect();
        assert_eq!(env.get("AWS_ACCESS_KEY_ID"), Some(&"the_id".to_string()));
        assert_eq!(
            env.get("AWS_SECRET_ACCESS_KEY"),
            Some(&"the_secret".to_string())
        );
    }

    #[test]
    fn backup_local_env_is_empty() {
        let backend = BackupBackend::Local {
            path: "/tmp/x".into(),
        };
        assert!(backend.env().is_empty());
    }

    #[test]
    fn backup_settings_round_trip() {
        let cfg = Config {
            backup: Some(BackupSettings {
                password: "the-key".into(),
                backend: BackupBackend::S3 {
                    endpoint: "http://127.0.0.1:9000".into(),
                    bucket: "ryra".into(),
                    access_key_id: "minio".into(),
                    secret_access_key: "minio123".into(),
                    prefix: None,
                },
            }),
            ..Config::default()
        };
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("[backup]"), "expected [backup] table: {text}");
        assert!(text.contains("password = \"the-key\""), "{text}");
        assert!(text.contains("kind = \"s3\""), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        let b = parsed.backup.expect("backup round-trips");
        assert_eq!(b.password, "the-key");
        match b.backend {
            BackupBackend::S3 { bucket, .. } => assert_eq!(bucket, "ryra"),
            other => panic!("unexpected backend: {other:?}"),
        }
    }

    #[test]
    fn backup_settings_counted_in_has_secrets() {
        // Triggers the "first time secrets are saved" warning the same
        // way SMTP / Tailscale do.
        let cfg = Config {
            backup: Some(BackupSettings {
                password: "x".into(),
                backend: BackupBackend::Local {
                    path: "/tmp/r".into(),
                },
            }),
            ..Config::default()
        };
        assert!(cfg.has_secrets());
    }
}
