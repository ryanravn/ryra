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

    #[error("service {0} not found in any registry")]
    ServiceNotFound(String),

    #[error("service {0} is already installed")]
    ServiceAlreadyInstalled(String),

    #[error("service {0} is not installed")]
    ServiceNotInstalled(String),

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

    #[error("nginx reload failed: {0}")]
    NginxReload(String),

    #[error("directory creation failed for {path}: {source}")]
    DirCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("template rendering failed: {0}")]
    Template(String),

    #[error("cloudflare API: {0}")]
    Cloudflare(String),

    #[error("invalid exposure mode: {0}")]
    InvalidExposure(String),

    #[error("registry error: {0}")]
    Registry(String),
}

pub type Result<T> = std::result::Result<T, Error>;
