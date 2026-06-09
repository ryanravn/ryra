use std::path::{Path, PathBuf};

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::paths::{DEFAULT_REGISTRY_URL, REGISTRY_DIR_ENV};
use crate::registry;

/// A reference to a service in a registry.
///
/// - `Default("forgejo")` — refers to a service in the project-managed
///   default registry (cloned from [`DEFAULT_REGISTRY_URL`]).
/// - `Custom { registry: "acme", service: "forgejo" }` — refers to a
///   service in a user-added custom registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceRef {
    /// A service from the default registry. E.g., `forgejo`.
    Default(String),
    /// A service from a named custom registry. E.g., `acme/forgejo`.
    Custom { registry: String, service: String },
    /// A local project directory whose `service.toml` lives at its root
    /// (`ryra add .` / `ryra add ./path`). `dir` is absolute; `name` is the
    /// `[service].name` read from the file.
    Path { dir: PathBuf, name: String },
}

/// Whether a CLI argument is a local project path rather than a registry
/// reference. Purely *syntactic* — a path must carry an explicit marker (`.`,
/// `..`, `./`, `../`, `/`, `~`), exactly like `./script` vs `script` in a shell.
///
/// Deliberately does NOT probe the filesystem: if a bare name like `forgejo`
/// resolved to a local `./forgejo/` folder whenever one happened to exist, the
/// meaning of `ryra add forgejo` would depend on your cwd, and a planted
/// `forgejo/service.toml` (which can run arbitrary build/run commands) could
/// hijack a trusted registry name. A bare word is always a registry ref.
pub fn is_path_like(input: &str) -> bool {
    input == "."
        || input == ".."
        || input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with('/')
        || input.starts_with('~')
}

/// Build a [`ServiceRef::Path`] from a directory, reading its `service.toml`
/// for the canonical service name. The path is absolutized so the install
/// record survives a later `cd`.
pub fn path_ref(dir: &Path) -> Result<ServiceRef> {
    let svc = registry::load_project_service(dir)?;
    let abs = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    Ok(ServiceRef::Path {
        dir: abs,
        name: svc.def.service.name,
    })
}

impl ServiceRef {
    /// Parse a service reference from a string.
    ///
    /// - `"forgejo"` → `Default("forgejo")`
    /// - `"acme/forgejo"` → `Custom { registry: "acme", service: "forgejo" }`
    /// - `""`, `"/forgejo"`, `"acme/"`, `"acme/sub/forgejo"` → error
    pub fn parse(input: &str) -> Result<Self> {
        let parts: Vec<&str> = input.split('/').collect();
        match parts.as_slice() {
            [""] => Err(Error::InvalidServiceRef(
                "service reference cannot be empty".to_string(),
            )),
            [name] => {
                if name.is_empty() {
                    Err(Error::InvalidServiceRef(
                        "service reference cannot be empty".to_string(),
                    ))
                } else {
                    Ok(ServiceRef::Default((*name).to_string()))
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
                    registry: (*registry).to_string(),
                    service: (*service).to_string(),
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
            ServiceRef::Default(name) => name,
            ServiceRef::Custom { service, .. } => service,
            ServiceRef::Path { name, .. } => name,
        }
    }

    /// Returns the registry name for this reference.
    ///
    /// Returns `"default"` for default-registry services. For a local path
    /// install it returns the project directory, which is what gets recorded in
    /// metadata so `ryra upgrade` can re-read the same `service.toml`.
    pub fn registry_name(&self) -> &str {
        match self {
            ServiceRef::Default(_) => crate::paths::REGISTRY_DEFAULT,
            ServiceRef::Custom { registry, .. } => registry,
            ServiceRef::Path { dir, .. } => dir.to_str().unwrap_or("local"),
        }
    }
}

/// Resolve the on-disk directory of the default registry.
///
/// If `RYRA_REGISTRY_DIR` is set to an existing directory, that path is
/// returned as-is (no clone, no pull) — the escape hatch tests use to inject
/// `/opt/ryra-test-registry` inside the VM without network access. Otherwise
/// the registry is cloned (or pulled) from [`DEFAULT_REGISTRY_URL`] into
/// `<cache_dir>/default/`.
pub async fn resolve_default_registry_dir(cache_dir: &Path) -> Result<PathBuf> {
    if let Ok(override_path) = std::env::var(REGISTRY_DIR_ENV) {
        let path = PathBuf::from(override_path);
        if path.is_dir() {
            return Ok(path);
        }
    }

    let dest = cache_dir.join("default");
    registry::fetch::clone_or_pull(DEFAULT_REGISTRY_URL, &dest).await?;
    Ok(dest)
}

/// Resolve the registry directory for a service reference.
///
/// - For `Default`: see [`resolve_default_registry_dir`].
/// - For `Custom`: looks up the registry name in `config.registries` and clones/pulls it.
pub async fn resolve_registry_dir(
    service_ref: &ServiceRef,
    config: &Config,
    cache_dir: &Path,
) -> Result<PathBuf> {
    match service_ref {
        // A local project: the directory *is* the source, no clone/pull.
        ServiceRef::Path { dir, .. } => Ok(dir.clone()),
        ServiceRef::Default(_) => resolve_default_registry_dir(cache_dir).await,
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
/// - For `Default`: finds the service in the default registry.
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
    fn parse_default_service() {
        let r = ServiceRef::parse("forgejo").expect("should parse");
        assert_eq!(r, ServiceRef::Default("forgejo".to_string()));
        assert_eq!(r.service_name(), "forgejo");
        assert_eq!(r.registry_name(), "default");
    }

    #[test]
    fn path_detection_is_syntactic_only() {
        // Explicit markers → local path.
        for p in [".", "..", "./app", "../app", "/abs/app", "~/app"] {
            assert!(is_path_like(p), "{p} should be treated as a path");
        }
        // Bare names and registry refs are NEVER paths, regardless of what
        // directories exist in the cwd. This is the security property: a planted
        // `./forgejo/` folder can't hijack `ryra add forgejo`.
        for name in ["forgejo", "acme/forgejo", "caddy", "my-app", "a/b"] {
            assert!(!is_path_like(name), "{name} must stay a registry ref");
        }
    }

    #[test]
    fn parse_custom_service() {
        let r = ServiceRef::parse("acme/forgejo").expect("should parse");
        assert_eq!(
            r,
            ServiceRef::Custom {
                registry: "acme".to_string(),
                service: "forgejo".to_string(),
            }
        );
        assert_eq!(r.service_name(), "forgejo");
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
        let err = ServiceRef::parse("/forgejo").expect_err("leading slash should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("empty"),
            "expected 'empty' in error for '/forgejo', got: {msg}"
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
        let err = ServiceRef::parse("acme/sub/forgejo").expect_err("too many slashes should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid"),
            "expected 'invalid' in error message, got: {msg}"
        );
    }

    #[test]
    fn env_override_returns_path_directly() {
        // SAFETY: this test sets a process-global env var; running multiple
        // env-mutating tests in parallel within the same process can race.
        // Cargo runs tests in threads, so we scope to a tempdir that
        // exists for the duration of the call.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cache = tempfile::TempDir::new().expect("cache tempdir");
        // SAFETY: tests in this module are single-threaded by Cargo's
        // default scheduler; setting env here doesn't escape the call.
        unsafe { std::env::set_var(REGISTRY_DIR_ENV, tmp.path()) };

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let resolved = rt
            .block_on(resolve_default_registry_dir(cache.path()))
            .expect("resolve");
        assert_eq!(resolved, tmp.path());

        unsafe { std::env::remove_var(REGISTRY_DIR_ENV) };
    }
}
