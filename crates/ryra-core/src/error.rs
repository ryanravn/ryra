use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config not found at {0}")]
    ConfigNotFound(PathBuf),

    #[error("failed to read {path}: {source}")]
    FileRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write {path}: {source}")]
    FileWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}")]
    TomlParse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("failed to serialize config: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("service {name} not found in any registry{}",
        crate::registry::format_service_suggestions(suggestions))]
    ServiceNotFound {
        name: String,
        suggestions: Vec<String>,
    },

    #[error("service {0} is already installed")]
    ServiceAlreadyInstalled(String),

    #[error("service {0} is not installed")]
    ServiceNotInstalled(String),

    #[error(
        "service {0} has leftover state from a prior install (incomplete install or preserved remove)"
    )]
    ServiceIncomplete(String),

    #[error("{service} requires the following services to be installed first: {}", missing.join(", "))]
    MissingRequiredServices {
        service: String,
        missing: Vec<String>,
    },

    #[error("registry {0} not found")]
    RegistryNotFound(String),

    #[error("no ports available in range {start}–{end}")]
    PortsExhausted { start: u16, end: u16 },

    #[error("port {port} is already in use")]
    PortConflict { port: u16 },

    #[error("git command failed: {0}")]
    Git(String),

    #[error("systemctl command failed: {0}")]
    Systemctl(String),

    #[error("directory creation failed for {path}: {source}")]
    DirCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("template rendering failed: {0}")]
    Template(String),

    #[error(
        "auth integration requires an auth provider to be configured first — run `ryra config auth`"
    )]
    AuthNotConfigured,

    #[error("service {0} does not support native OIDC auth")]
    NoOidcSupport(String),

    #[error("{0}")]
    UnsupportedArchitecture(String),

    #[error("service '{service}' has no env_group named '{group}'{hint}")]
    UnknownEnvGroup {
        service: String,
        group: String,
        hint: String,
    },

    #[error("could not determine home directory: set $HOME")]
    HomeDirNotFound,

    #[error("invalid service reference: {0}")]
    InvalidServiceRef(String),

    #[error("registry configuration error: {0}")]
    RegistryConfig(String),

    #[error("quadlet bundle error: {0}")]
    Bundle(String),

    #[error("auth context error: {0}")]
    AuthContext(String),

    #[error("config validation failed: {0}")]
    ConfigValidation(String),

    #[error(
        "{service}: {} hand-edited file(s) would be overwritten — re-run with --force to overwrite, or back up your changes first:\n  {}",
        paths.len(),
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
    )]
    HandEditedFiles {
        service: String,
        paths: Vec<PathBuf>,
    },

    #[error("no backups found for service '{0}' — `ryra upgrade` creates them, run that first")]
    NoBackup(String),

    #[error("service '{service}' has no backup at timestamp {stamp} — run `ryra revert {service}` to use the most recent")]
    BackupNotFound { service: String, stamp: String },
}

pub type Result<T> = std::result::Result<T, Error>;
