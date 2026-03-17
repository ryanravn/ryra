pub mod fetch;
pub mod service_def;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use service_def::ServiceDef;

/// Represents a service found in a repo, with its source info.
pub struct RegistryService {
    pub def: ServiceDef,
    /// Path to the service directory (contains service.toml, compose files, etc.)
    pub service_dir: PathBuf,
}

/// Find a service by name in a repo directory.
pub fn find_service(repo_dir: &Path, name: &str) -> Result<RegistryService> {
    let svc_dir = repo_dir.join("services").join(name);
    let service_toml = svc_dir.join("service.toml");

    if !service_toml.exists() {
        return Err(Error::ServiceNotFound(name.to_string()));
    }

    let contents = std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
        path: service_toml.clone(),
        source,
    })?;
    let def: ServiceDef = toml::from_str(&contents).map_err(|source| Error::TomlParse {
        path: service_toml,
        source,
    })?;

    Ok(RegistryService {
        def,
        service_dir: svc_dir,
    })
}

/// List all available services in a repo directory.
pub fn list_available(repo_dir: &Path) -> Result<Vec<RegistryService>> {
    let services_dir = repo_dir.join("services");
    if !services_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&services_dir).map_err(|source| Error::FileRead {
        path: services_dir.clone(),
        source,
    })?;

    let mut services = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: services_dir.clone(),
            source,
        })?;
        let svc_dir = entry.path();
        let service_toml = svc_dir.join("service.toml");
        if service_toml.exists() {
            let contents =
                std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
                    path: service_toml.clone(),
                    source,
                })?;
            let def: ServiceDef =
                toml::from_str(&contents).map_err(|source| Error::TomlParse {
                    path: service_toml,
                    source,
                })?;
            services.push(RegistryService {
                def,
                service_dir: svc_dir,
            });
        }
    }

    services.sort_by(|a, b| a.def.service.name.cmp(&b.def.service.name));
    Ok(services)
}
