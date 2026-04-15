use std::path::Path;

use crate::config;
use crate::config::ConfigPaths;
use crate::config::schema::RegistryEntry;
use crate::error::{Error, Result};
use crate::registry;

/// Add a named custom registry.
pub async fn add(name: &str, url: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    if config.registries.iter().any(|r| r.name == name) {
        return Err(Error::RegistryConfig(format!(
            "registry '{name}' already exists — remove it first to change the URL"
        )));
    }

    // Clone the repo to validate the URL
    let dest = paths.cache_dir.join("registries").join(name);
    registry::fetch::clone_or_pull(url, &dest).await?;

    config.registries.push(RegistryEntry {
        name: name.to_string(),
        url: url.to_string(),
    });

    config::save_config(&paths.config_file, &config)?;
    Ok(())
}

/// Remove a named custom registry.
pub fn remove(name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    if !config.registries.iter().any(|r| r.name == name) {
        return Err(Error::RegistryNotFound(name.to_string()));
    }

    config.registries.retain(|r| r.name != name);
    config::save_config(&paths.config_file, &config)?;

    // Remove cached clone
    let dest = paths.cache_dir.join("registries").join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|source| Error::FileWrite {
            path: dest,
            source,
        })?;
    }

    Ok(())
}

/// Update (git pull) custom registries. If `name` is Some, update only that one.
pub async fn update(name: Option<&str>) -> Result<Vec<UpdatedRegistry>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let entries: Vec<&RegistryEntry> = match name {
        Some(n) => {
            let entry = config
                .registries
                .iter()
                .find(|r| r.name == n)
                .ok_or_else(|| Error::RegistryNotFound(n.to_string()))?;
            vec![entry]
        }
        None => config.registries.iter().collect(),
    };

    let mut results = Vec::new();
    for entry in entries {
        let dest = paths.cache_dir.join("registries").join(&entry.name);
        registry::fetch::clone_or_pull(&entry.url, &dest).await?;
        let service_count = count_services(&dest);
        results.push(UpdatedRegistry {
            name: entry.name.clone(),
            service_count,
        });
    }

    Ok(results)
}

/// List all registered custom registries with service counts.
pub fn list() -> Result<Vec<RegistryInfo>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let mut infos = Vec::new();
    for entry in &config.registries {
        let dest = paths.cache_dir.join("registries").join(&entry.name);
        let service_count = if dest.exists() {
            count_services(&dest)
        } else {
            0
        };
        infos.push(RegistryInfo {
            name: entry.name.clone(),
            url: entry.url.clone(),
            service_count,
        });
    }

    Ok(infos)
}

pub struct UpdatedRegistry {
    pub name: String,
    pub service_count: usize,
}

pub struct RegistryInfo {
    pub name: String,
    pub url: String,
    pub service_count: usize,
}

/// Count how many service directories exist in a registry directory.
fn count_services(dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "warning: could not read registry directory {}: {e}",
                dir.display()
            );
            return 0;
        }
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("service.toml").exists())
        .count()
}
