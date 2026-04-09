pub mod schema;
pub mod status;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use schema::Config;

/// Resolved paths for all ryra config/state files.
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
    pub snapshots_dir: PathBuf,
}

impl ConfigPaths {
    pub fn resolve() -> Result<Self> {
        let home = dirs::home_dir()
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .ok_or_else(|| {
                Error::Registry(
                    "could not determine home directory: set $HOME".into(),
                )
            })?;
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("ryra");
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| home.join(".cache"))
            .join("ryra");
        let snapshots_dir = config_dir.join("snapshots");
        Ok(Self {
            config_file: config_dir.join("ryra.toml"),
            cache_dir,
            config_dir,
            snapshots_dir,
        })
    }

    pub fn ensure_cache_dir(&self) -> Result<()> {
        ensure_dir(&self.cache_dir)
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        ensure_dir(&self.config_dir)?;
        ensure_dir(&self.cache_dir)?;
        Ok(())
    }
}

fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|source| Error::DirCreate {
        path: path.to_path_buf(),
        source,
    })
}

pub fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Err(Error::ConfigNotFound(path.to_path_buf()));
    }
    let contents = std::fs::read_to_string(path).map_err(|source| Error::FileRead {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|source| Error::TomlParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Load config from path, returning a default config if the file doesn't exist.
pub fn load_or_default(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    load_config(path)
}

pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let contents = toml::to_string_pretty(config)?;
    write_file(path, &contents)?;
    // Config contains credentials — owner-only access
    set_permissions(path, 0o600)?;
    Ok(())
}

fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|source| {
        Error::FileWrite {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Save a snapshot of a service.toml at install time.
pub fn save_snapshot(snapshots_dir: &Path, service_name: &str, content: &str) -> Result<()> {
    ensure_dir(snapshots_dir)?;
    let path = snapshots_dir.join(format!("{service_name}.toml"));
    write_file(&path, content)
}

/// Load a previously saved service.toml snapshot.
pub fn load_snapshot(snapshots_dir: &Path, service_name: &str) -> Result<String> {
    let path = snapshots_dir.join(format!("{service_name}.toml"));
    if !path.exists() {
        return Err(Error::ConfigNotFound(path));
    }
    std::fs::read_to_string(&path).map_err(|source| Error::FileRead { path, source })
}

/// Remove a snapshot when a service is removed.
pub fn remove_snapshot(snapshots_dir: &Path, service_name: &str) -> Result<()> {
    let path = snapshots_dir.join(format!("{service_name}.toml"));
    if path.exists() {
        std::fs::remove_file(&path).map_err(|source| Error::FileWrite { path, source })?;
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::DirCreate {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, contents).map_err(|source| Error::FileWrite {
        path: path.to_path_buf(),
        source,
    })
}
