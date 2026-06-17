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
    /// Scope-aware config paths. User scope is the per-user layout
    /// ([`resolve`]); System scope is a fixed host-wide layout under
    /// `/etc/ryra` + `/var/lib/ryra/cache`, tied to the scope rather than to
    /// whoever invokes ryra (so running as root never reads `/root/.config`).
    pub fn resolve_for(scope: crate::scope::Scope) -> Result<Self> {
        match scope {
            crate::scope::Scope::User => Self::resolve(),
            crate::scope::Scope::System => {
                let config_dir = PathBuf::from("/etc/ryra");
                Ok(Self {
                    config_file: config_dir.join("preferences.toml"),
                    cache_dir: PathBuf::from("/var/lib/ryra/cache"),
                    config_dir,
                })
            }
        }
    }

    pub fn resolve() -> Result<Self> {
        let home = dirs::home_dir()
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
            .ok_or(Error::HomeDirNotFound)?;
        // RYRA_CONFIG_DIR overrides where preferences.toml lives (used by the
        // test harness to isolate host runs from the user's real credentials).
        let config_dir = match std::env::var_os(crate::paths::CONFIG_DIR_ENV) {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => dirs::config_dir()
                .unwrap_or_else(|| home.join(".config"))
                .join("services"),
        };
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| home.join(".cache"))
            .join("services");
        Ok(Self {
            config_file: config_dir.join("preferences.toml"),
            cache_dir,
            config_dir,
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

/// The version of this ryra binary, set at compile time from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Err(Error::ConfigNotFound(path.to_path_buf()));
    }
    let contents = std::fs::read_to_string(path).map_err(|source| Error::FileRead {
        path: path.to_path_buf(),
        source,
    })?;
    let config: Config = toml::from_str(&contents).map_err(|source| Error::TomlParse {
        path: path.to_path_buf(),
        source,
    })?;
    if let Err(msg) = config.validate() {
        return Err(Error::ConfigValidation(msg));
    }
    check_version(&config);
    Ok(config)
}

/// Warn if the config was written by a newer major.minor version of ryra.
fn check_version(config: &Config) {
    let config_version = match &config.version {
        Some(v) => v,
        None => return, // pre-version config, accept silently
    };
    let binary = parse_major_minor(VERSION);
    let config_v = parse_major_minor(config_version);
    if let (Some((b_major, b_minor)), Some((c_major, c_minor))) = (binary, config_v)
        && (c_major, c_minor) > (b_major, b_minor)
    {
        eprintln!(
            "Warning: config was written by ryra {config_version}, \
             but this is ryra {VERSION} — consider upgrading"
        );
    }
}

fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Load config from path, returning a default config if the file doesn't exist.
pub fn load_or_default(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    load_config(path)
}

pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let mut config = config.clone();
    config.version = Some(VERSION.to_string());
    let contents = toml::to_string_pretty(&config)?;
    // Atomic write with 0o600 from byte zero — config contains SMTP + auth
    // credentials, so it must never be briefly world-readable and must never
    // appear half-written if the process dies mid-save.
    crate::system::atomic_write::atomic_write(path, contents.as_bytes(), 0o600)?;
    Ok(())
}
