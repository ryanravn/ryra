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
}

impl ConfigPaths {
    pub fn resolve() -> Result<Self> {
        let config_dir = PathBuf::from("/etc/ryra");
        let cache_dir = PathBuf::from("/var/cache/ryra");
        Ok(Self {
            config_file: config_dir.join("ryra.toml"),
            cache_dir,
            config_dir,
        })
    }

    pub fn ensure_cache_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir).map_err(|source| Error::DirCreate {
            path: self.cache_dir.clone(),
            source,
        })?;
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.cache_dir] {
            std::fs::create_dir_all(dir).map_err(|source| Error::DirCreate {
                path: dir.clone(),
                source,
            })?;
        }
        Ok(())
    }
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

pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let contents = toml::to_string_pretty(config)?;
    write_file(path, &contents)?;
    // Config contains credentials — owner-only access
    let _ = set_permissions(path, 0o600);
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
        // Try direct creation first, fall back to sudo
        if std::fs::create_dir_all(parent).is_err() {
            let _ = std::process::Command::new("sudo")
                .args(["mkdir", "-p", &parent.to_string_lossy()])
                .status();
        }
    }
    // Try direct write first, fall back to sudo tee
    match std::fs::write(path, contents) {
        Ok(()) => Ok(()),
        Err(_) => {
            use std::io::Write;
            let mut child = std::process::Command::new("sudo")
                .args(["tee", &path.to_string_lossy()])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .map_err(|source| Error::FileWrite {
                    path: path.to_path_buf(),
                    source,
                })?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(contents.as_bytes()).map_err(|source| Error::FileWrite {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
            let status = child.wait().map_err(|source| Error::FileWrite {
                path: path.to_path_buf(),
                source,
            })?;
            if !status.success() {
                return Err(Error::FileWrite {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sudo tee failed"),
                });
            }
            Ok(())
        }
    }
}
