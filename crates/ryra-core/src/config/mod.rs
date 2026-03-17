pub mod schema;
pub mod state;
pub mod status;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use schema::Config;
use state::State;

/// Resolved paths for all ryra config/state files.
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub cache_dir: PathBuf,
}

impl ConfigPaths {
    pub fn resolve() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| Error::FileRead {
                path: "~/.config".into(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "could not determine config directory",
                ),
            })?
            .join("ryra");
        Ok(Self {
            config_file: config_dir.join("ryra.toml"),
            state_file: config_dir.join("state.toml"),
            cache_dir: config_dir.join("cache").join("registries"),
            config_dir,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir).map_err(|source| Error::DirCreate {
                path: dir.clone(),
                source,
            })?;
        }
        set_permissions(&self.config_dir, 0o700)?;
        Ok(())
    }
}

pub fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Err(Error::ConfigNotFound(path.to_path_buf()));
    }
    if let Some(parent) = path.parent() {
        check_permissions(parent)?;
    }
    check_permissions(path)?;
    let contents = std::fs::read_to_string(path).map_err(|source| Error::FileRead {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&contents).map_err(|source| Error::TomlParse {
        path: path.to_path_buf(),
        source,
    })
}

pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let contents = toml::to_string_pretty(config)?;
    write_file(path, &contents)?;
    // Config contains credentials — owner-only access
    set_permissions(path, 0o600)?;
    if let Some(parent) = path.parent() {
        set_permissions(parent, 0o700)?;
    }
    Ok(())
}

pub fn load_state(path: &Path) -> Result<State> {
    if !path.exists() {
        return Ok(State::default());
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

pub fn save_state(path: &Path, state: &State) -> Result<()> {
    let contents = toml::to_string_pretty(state)?;
    write_file(path, &contents)
}

/// Refuse to load config if permissions are too open (like SSH does).
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|source| Error::FileRead {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(Error::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
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
