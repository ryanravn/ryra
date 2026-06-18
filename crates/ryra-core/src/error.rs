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

    #[error(
        "service {name} not found in any registry{}",
        crate::registry::format_service_suggestions(suggestions)
    )]
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

    #[error("tailscale: {0}")]
    Tailscale(String),

    #[error("directory creation failed for {path}: {source}")]
    DirCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("template rendering failed: {0}")]
    Template(String),

    #[error(
        "auth integration requires an auth provider to be configured first — run `ryra add authelia`"
    )]
    AuthNotConfigured,

    #[error("service {0} does not support native OIDC auth")]
    NoOidcSupport(String),

    #[error(
        "authelia is local-only at {auth_url}, but {service} will be reachable at \
         {service_url}. Off-host clients (e.g., other devices on your tailnet) can't \
         resolve `*.internal` hostnames, so the OIDC redirect from {service} back \
         to authelia would fail.\n\n\
         Fix: re-install authelia at the same exposure as {service}:\n  \
         ryra remove authelia --purge\n  \
         ryra add authelia --tailscale  (or --url <public-https-url>)"
    )]
    AuthExposureMismatch {
        auth_url: String,
        service: String,
        service_url: String,
    },

    #[error(
        "{service} uses --auth and authelia is exposed at {auth_url}, but no reverse \
         proxy is installed. The OIDC back-channel between {service} and authelia runs \
         through Caddy as the internal TLS terminator, so Caddy must be installed first.\n\n\
         Fix:\n  \
         ryra add caddy\n  \
         then re-run your `ryra add {service} --auth ...` command"
    )]
    AuthRequiresReverseProxy { service: String, auth_url: String },

    #[error("{0}")]
    UnsupportedArchitecture(String),

    #[error("service '{service}' has no env_group named '{group}'{hint}")]
    UnknownEnvGroup {
        service: String,
        group: String,
        hint: String,
    },

    #[error("`ryra config` can't change {field} for service '{service}'. {workaround}")]
    ConfigureUnsupported {
        service: String,
        field: String,
        workaround: String,
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

    #[error(
        "service '{service}' has no backup at timestamp {stamp} — run `ryra revert {service}` to use the most recent"
    )]
    BackupNotFound { service: String, stamp: String },

    #[error(
        "service '{0}' does not declare backup support — the service author must set `backup = true` under [integrations] in its service.toml first"
    )]
    BackupNotSupported(String),

    #[error("backup repository is not configured — run `ryra backup config` first")]
    BackupRepoNotConfigured,

    #[error("backup is not enabled for service '{0}' — re-install with `ryra add {0} --backup`")]
    BackupNotEnabled(String),

    #[error(
        "no snapshots found for service '{0}' in the backup repository — has `ryra backup run` ever succeeded?"
    )]
    BackupNoSnapshots(String),

    #[error("restic command failed: {0}")]
    Restic(String),

    #[error("backup hook '{hook}' for service '{service}' failed: {message}")]
    BackupHookFailed {
        service: String,
        hook: String,
        message: String,
    },

    #[error(
        "service '{service}' was backed up at manifest hash {backed_up} but current install is at {current} — pass --force to restore anyway, or --migrate-to=<dir> to extract without touching the live install"
    )]
    BackupVersionMismatch {
        service: String,
        backed_up: String,
        current: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
