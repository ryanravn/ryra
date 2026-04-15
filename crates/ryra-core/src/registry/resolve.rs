use std::path::{Path, PathBuf};

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::registry;

/// A reference to a service in a registry.
///
/// - `Bundled("jellyfin")` — refers to a service in the embedded bundled registry
/// - `Custom { registry: "acme", service: "jellyfin" }` — refers to a service in a named custom registry
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceRef {
    /// A service from the embedded bundled registry. E.g., `jellyfin`.
    Bundled(String),
    /// A service from a named custom registry. E.g., `acme/jellyfin`.
    Custom { registry: String, service: String },
}

impl ServiceRef {
    /// Parse a service reference from a string.
    ///
    /// - `"jellyfin"` → `Bundled("jellyfin")`
    /// - `"acme/jellyfin"` → `Custom { registry: "acme", service: "jellyfin" }`
    /// - `""`, `"/jellyfin"`, `"acme/"`, `"acme/sub/jellyfin"` → error
    pub fn parse(input: &str) -> Result<Self> {
        let parts: Vec<&str> = input.split('/').collect();
        match parts.as_slice() {
            [""] => Err(Error::InvalidServiceRef("service reference cannot be empty".to_string())),
            [name] => {
                if name.is_empty() {
                    Err(Error::InvalidServiceRef(
                        "service reference cannot be empty".to_string(),
                    ))
                } else {
                    Ok(ServiceRef::Bundled(name.to_string()))
                }
            }
            [registry, service] => {
                if registry.is_empty() {
                    return Err(Error::InvalidServiceRef(format!(
                        "registry name cannot be empty in reference '{input}'"
                    )));
                }
                if service.is_empty() {
                    return Err(Error::InvalidServiceRef(format!(
                        "service name cannot be empty in reference '{input}'"
                    )));
                }
                Ok(ServiceRef::Custom {
                    registry: registry.to_string(),
                    service: service.to_string(),
                })
            }
            _ => Err(Error::InvalidServiceRef(format!(
                "invalid service reference '{input}': expected 'service' or 'registry/service'"
            ))),
        }
    }

    /// Returns the service name part of this reference.
    pub fn service_name(&self) -> &str {
        match self {
            ServiceRef::Bundled(name) => name,
            ServiceRef::Custom { service, .. } => service,
        }
    }

    /// Returns the registry name for this reference.
    ///
    /// Returns `"bundled"` for bundled services.
    pub fn registry_name(&self) -> &str {
        match self {
            ServiceRef::Bundled(_) => "bundled",
            ServiceRef::Custom { registry, .. } => registry,
        }
    }
}

/// Resolve the registry directory for a service reference.
///
/// - For `Bundled`: extracts the bundled registry to `<cache_dir>/bundled/` if needed.
/// - For `Custom`: looks up the registry name in `config.registries` and clones/pulls it.
pub async fn resolve_registry_dir(
    service_ref: &ServiceRef,
    config: &Config,
    cache_dir: &Path,
) -> Result<PathBuf> {
    match service_ref {
        ServiceRef::Bundled(_) => registry::bundled::ensure_bundled(cache_dir),
        ServiceRef::Custom { registry, .. } => {
            let entry = config
                .registries
                .iter()
                .find(|r| r.name == *registry)
                .ok_or_else(|| Error::RegistryNotFound(registry.clone()))?;

            let dest = cache_dir.join("registries").join(registry);
            registry::fetch::clone_or_pull(&entry.url, &dest).await?;
            Ok(dest)
        }
    }
}

/// Resolve a service from a registry, returning its definition and directory.
///
/// - For `Bundled`: finds the service in the embedded bundled registry.
/// - For `Custom`: finds the service in the named custom registry.
pub async fn resolve_service(
    service_ref: &ServiceRef,
    config: &Config,
    cache_dir: &Path,
) -> Result<registry::RegistryService> {
    let repo_dir = resolve_registry_dir(service_ref, config, cache_dir).await?;
    registry::find_service(&repo_dir, service_ref.service_name())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bundled_service() {
        let r = ServiceRef::parse("jellyfin").expect("should parse");
        assert_eq!(r, ServiceRef::Bundled("jellyfin".to_string()));
        assert_eq!(r.service_name(), "jellyfin");
        assert_eq!(r.registry_name(), "bundled");
    }

    #[test]
    fn parse_custom_service() {
        let r = ServiceRef::parse("acme/jellyfin").expect("should parse");
        assert_eq!(
            r,
            ServiceRef::Custom {
                registry: "acme".to_string(),
                service: "jellyfin".to_string(),
            }
        );
        assert_eq!(r.service_name(), "jellyfin");
        assert_eq!(r.registry_name(), "acme");
    }

    #[test]
    fn parse_empty_fails() {
        let err = ServiceRef::parse("").expect_err("empty input should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected 'empty' in error message, got: {msg}"
        );
    }

    #[test]
    fn parse_empty_parts_fails() {
        let err = ServiceRef::parse("/jellyfin").expect_err("leading slash should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected 'empty' in error for '/jellyfin', got: {msg}"
        );

        let err = ServiceRef::parse("acme/").expect_err("trailing slash should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected 'empty' in error for 'acme/', got: {msg}"
        );
    }

    #[test]
    fn parse_too_many_slashes_fails() {
        let err = ServiceRef::parse("acme/sub/jellyfin").expect_err("too many slashes should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error message, got: {msg}"
        );
    }
}
