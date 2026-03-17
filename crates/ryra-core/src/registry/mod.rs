pub mod fetch;
pub mod service_def;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use service_def::ServiceDef;

/// Represents a service found in a registry, with its source info.
pub struct RegistryService {
    pub registry_name: String,
    pub def: ServiceDef,
    /// Path to the service directory in the registry (contains service.toml, compose files, etc.)
    pub service_dir: PathBuf,
}

/// Look up a service by name across all cached registries.
pub fn find_service(cache_dir: &Path, registries: &[(String, String)], name: &str) -> Result<RegistryService> {
    for (reg_name, _url) in registries {
        let svc_dir = cache_dir
            .join(reg_name)
            .join("services")
            .join(name);
        let service_toml = svc_dir.join("service.toml");

        if service_toml.exists() {
            let contents = std::fs::read_to_string(&service_toml).map_err(|source| {
                Error::FileRead {
                    path: service_toml.clone(),
                    source,
                }
            })?;
            let def: ServiceDef =
                toml::from_str(&contents).map_err(|source| Error::TomlParse {
                    path: service_toml,
                    source,
                })?;
            return Ok(RegistryService {
                registry_name: reg_name.clone(),
                def,
                service_dir: svc_dir,
            });
        }
    }
    Err(Error::ServiceNotFound(name.to_string()))
}

/// List all available services across cached registries.
pub fn list_available(cache_dir: &Path, registries: &[(String, String)]) -> Result<Vec<RegistryService>> {
    let mut services = Vec::new();

    for (reg_name, _url) in registries {
        let services_dir = cache_dir.join(reg_name).join("services");
        if !services_dir.exists() {
            continue;
        }

        let entries = std::fs::read_dir(&services_dir).map_err(|source| Error::FileRead {
            path: services_dir.clone(),
            source,
        })?;

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
                    registry_name: reg_name.clone(),
                    def,
                    service_dir: svc_dir,
                });
            }
        }
    }

    services.sort_by(|a, b| a.def.service.name.cmp(&b.def.service.name));
    Ok(services)
}
