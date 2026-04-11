pub mod authelia;
pub mod caddy;
pub mod config;
pub mod diff;
pub mod error;
pub mod generate;
pub mod integrations;
pub mod registry;
pub mod system;
pub mod verbose;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::ConfigPaths;
use config::schema::{Config, InstalledService};
use error::{Error, Result};
use generate::GeneratedFile;

// Well-known infrastructure service names used for cross-service integration.
// Using constants prevents typos and makes it easy to find all references.
pub const SERVICE_CADDY: &str = "caddy";
pub const SERVICE_AUTHELIA: &str = "authelia";

// --- Path conventions ---

/// Resolve the user's home directory, falling back to $HOME.
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .ok_or_else(|| {
            Error::Registry(
                "could not determine home directory: neither dirs::home_dir() nor $HOME are set"
                    .into(),
            )
        })
}

/// Data directory for a service: ~/.local/share/ryra/<name>
pub fn service_home(service_name: &str) -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .or_else(|| home_dir().ok().map(|h| h.join(".local/share")))
        .ok_or_else(|| {
            Error::Registry(
                "could not determine data directory: set $HOME or $XDG_DATA_HOME".into(),
            )
        })?;
    Ok(base.join("ryra").join(service_name))
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .or_else(|| home_dir().ok().map(|h| h.join(".config")))
        .ok_or_else(|| {
            Error::Registry(
                "could not determine config directory: set $HOME or $XDG_CONFIG_HOME".into(),
            )
        })?;
    Ok(base.join("containers").join("systemd"))
}

// --- Typed steps: what the CLI needs to execute ---

/// A discrete operation that the CLI executes. Pattern matching ensures
/// every step type is handled — no string parsing or if-chains.
pub enum Step {
    /// Write a file.
    WriteFile(GeneratedFile),
    /// Reload systemd for the current user.
    DaemonReload,
    /// Start a service under the current user's systemd.
    StartService { unit: String },
    /// Stop a service under the current user's systemd.
    StopService { unit: String },
    /// Restart a service under the current user's systemd.
    RestartService { unit: String },
    /// Reload Caddy's config without restarting the container.
    ReloadCaddy,
    /// Pull a container image.
    PullImage { image: String },
    /// Remove a file.
    RemoveFile(PathBuf),
    /// Remove a directory tree.
    RemoveDir(PathBuf),
    /// Remove a podman named volume.
    RemoveVolume { name: String },
    /// Create a directory (with parents).
    CreateDir(PathBuf),
}

impl Step {
    /// Render this step as a shell command (for dry-run display).
    pub fn to_command(&self) -> String {
        match self {
            Step::WriteFile(file) => format!("write {}", file.path.display()),
            Step::DaemonReload => "systemctl --user daemon-reload".into(),
            Step::StartService { unit } => format!("systemctl --user start {unit}"),
            Step::StopService { unit } => format!("systemctl --user stop {unit}"),
            Step::RestartService { unit } => format!("systemctl --user restart {unit}"),
            Step::ReloadCaddy => {
                "podman exec caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile"
                    .into()
            }
            Step::PullImage { image } => format!("podman pull {image}"),
            Step::RemoveFile(path) => format!("rm -f {}", path.display()),
            Step::RemoveDir(path) => format!("rm -rf {}", path.display()),
            Step::CreateDir(path) => format!("mkdir -p {}", path.display()),
            Step::RemoveVolume { name } => format!("podman volume rm {name}"),
        }
    }
}

// --- Warnings ---

/// Warnings generated during service operations that the CLI should display.
pub enum Warning {
    /// System RAM is below the service's minimum requirement.
    RamBelowMinimum {
        service_name: String,
        min_mb: u64,
        available_mb: u64,
    },
    /// System RAM is below the service's recommended level (but above minimum).
    RamBelowRecommended {
        service_name: String,
        recommended_mb: u64,
        available_mb: u64,
    },
}

// --- Result types ---

pub struct InitResult {
    pub steps: Vec<Step>,
}

pub struct AddResult {
    pub steps: Vec<Step>,
    pub warnings: Vec<Warning>,
    pub repo_url: String,
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in .env).
    pub generated_secrets: Vec<String>,
    /// The generated .env content (for post-install processing).
    pub env_content: String,
    /// Domain assigned to this service (if Caddy reverse proxy was configured).
    pub domain: Option<String>,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub service_name: String,
    /// Domain that was assigned to this service (if any).
    pub domain: Option<String>,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

pub struct UpdateResult {
    pub steps: Vec<Step>,
    pub changes: Vec<diff::Change>,
}

/// Resolve the registry directory for a service reference.
pub async fn resolve_registry_dir(
    service_ref: &registry::resolve::ServiceRef,
) -> Result<PathBuf> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_cache_dir()?;
    let config = config::load_or_default(&paths.config_file)?;
    registry::resolve::resolve_registry_dir(service_ref, &config, &paths.cache_dir).await
}

/// Build a ServiceRef from an installed service's stored registry name.
pub fn service_ref_from_installed(
    installed: &InstalledService,
) -> registry::resolve::ServiceRef {
    if installed.repo.is_empty() || installed.repo == "bundled" {
        registry::resolve::ServiceRef::Bundled(installed.name.clone())
    } else {
        registry::resolve::ServiceRef::Custom {
            registry: installed.repo.clone(),
            service: installed.name.clone(),
        }
    }
}

/// Initialize a new ryra project.
pub async fn init(config: Config) -> Result<InitResult> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;

    // Extract bundled registry to cache
    registry::bundled::ensure_bundled(&paths.cache_dir)?;

    // Preserve installed services from existing config
    let mut config = config;
    if let Ok(existing) = config::load_or_default(&paths.config_file)
        && !existing.services.is_empty()
    {
        config.services = existing.services;
    }

    // Write config
    let config_content = toml::to_string_pretty(&config)
        .map_err(|e| Error::Template(format!("failed to serialize config: {e}")))?;

    let steps = vec![Step::WriteFile(GeneratedFile {
        path: paths.config_file.clone(),
        content: config_content,
    })];

    Ok(InitResult { steps })
}

/// Determine which extra podman networks a service should join.
/// Services with a domain join caddy's network for reverse proxy routing.
/// Services with auth join authelia's network for OIDC communication.
fn resolve_extra_networks(
    service_name: &str,
    domain: Option<&str>,
    enable_auth: bool,
    caddy_installed: bool,
    authelia_installed: bool,
) -> Vec<String> {
    let mut networks = Vec::new();
    let needs_caddy = domain.is_some() || (enable_auth && caddy_installed);
    if needs_caddy && caddy_installed && service_name != SERVICE_CADDY {
        networks.push(SERVICE_CADDY.to_string());
    }
    if enable_auth && authelia_installed && service_name != SERVICE_AUTHELIA {
        networks.push(SERVICE_AUTHELIA.to_string());
    }
    networks
}

/// Add a service: generate config, return steps to execute.
pub fn add_service(
    service_name: &str,
    domain: Option<&str>,
    auth_kind: Option<registry::service_def::AuthKind>,
    enable_auth: bool,
    env_overrides: &BTreeMap<String, String>,
    registry_name: &str,
    repo_dir: &Path,
) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    if config.services.iter().any(|s| s.name == service_name) {
        return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
    }

    let reg_service = registry::find_service(repo_dir, service_name)?;

    // Validate: architecture compatibility
    if let Some(msg) = reg_service.def.check_architecture() {
        return Err(Error::UnsupportedArchitecture(msg));
    }

    // Validate: all required services must be installed
    let missing_requires: Vec<&str> = reg_service
        .def
        .requires
        .iter()
        .filter(|r| !config.services.iter().any(|s| s.name == r.service))
        .map(|r| r.service.as_str())
        .collect();
    if !missing_requires.is_empty() {
        return Err(Error::MissingRequiredServices {
            service: service_name.to_string(),
            missing: missing_requires.iter().map(|s| s.to_string()).collect(),
        });
    }

    // If the user chose to enable auth, an auth provider must be configured
    if auth_kind.is_some() && config.auth.is_none() {
        return Err(Error::AuthNotConfigured);
    }

    // Determine host port: use fixed host_port from service def if set,
    // allocate one if container port is privileged, otherwise use container port directly.
    let has_fixed_ports = reg_service.def.ports.iter().any(|p| p.host_port.is_some());
    let has_privileged_port = reg_service
        .def
        .ports
        .iter()
        .any(|p| p.host_port.is_none() && p.container_port < 1024);
    let host_port = if has_fixed_ports {
        // Fixed ports are set per-port in the service def (e.g. Caddy 80/443)
        None
    } else if has_privileged_port {
        Some(system::port::allocate_port(&config)?)
    } else {
        None
    };

    // Check for port conflicts by probing whether the port is already bound.
    for p in &reg_service.def.ports {
        let port = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
        if system::port::is_port_in_use(port) {
            return Err(Error::PortConflict { port });
        }
    }

    let home_dir = service_home(service_name)?;
    let quadlet_path = quadlet_dir()?;

    let mut extra_volumes = Vec::new();
    let mut extra_env: BTreeMap<String, String> = BTreeMap::new();

    // When auth is enabled and routes through Caddy (HTTPS), mount Caddy's
    // root CA cert so containers trust the self-signed TLS.
    // The cert is exported by caddy's config scripts to a known path.
    if enable_auth && caddy::is_installed() {
        let caddy_home = service_home(SERVICE_CADDY)?;
        let ca_cert = caddy_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(caddy_home)
            .join("caddy-root-ca.crt");
        if ca_cert.exists() && ca_cert.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            extra_volumes.push(format!(
                "{}:/etc/ssl/certs/caddy-root-ca.crt:ro,Z",
                ca_cert.display()
            ));
            extra_env.insert(
                "NODE_EXTRA_CA_CERTS".into(),
                "/etc/ssl/certs/caddy-root-ca.crt".into(),
            );
        }
    }

    let authelia_installed = config.services.iter().any(|s| s.name == SERVICE_AUTHELIA);
    let extra_networks = resolve_extra_networks(
        service_name,
        domain,
        enable_auth,
        caddy::is_installed(),
        authelia_installed,
    );

    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: auth_kind.as_ref(),
        host_port,
        env_overrides,
        domain,
        extra_env,
    })?;

    // When auth routes through Caddy, prevent the host's /etc/hosts from
    // leaking into the container (it would override podman DNS aliases).
    let podman_args: Vec<String> = if enable_auth && caddy::is_installed() {
        vec!["--no-hosts".to_string()]
    } else {
        Vec::new()
    };

    // Process quadlet bundle from registry
    let bundle =
        generate::bundle::process_quadlet_bundle(&generate::bundle::ProcessBundleParams {
            service_dir: &reg_service.service_dir,
            service_name,
            quadlet_dir: &quadlet_path,
            extra_networks: &extra_networks,
            extra_volumes: &extra_volumes,
            podman_args: &podman_args,
        })?;

    // Generate warnings
    let mut warnings = Vec::new();

    if let Some(ref reqs) = reg_service.def.requirements
        && let Some(total) = system::memory::total_ram_mb()
    {
        if total < reqs.ram.min {
            warnings.push(Warning::RamBelowMinimum {
                service_name: service_name.to_string(),
                min_mb: reqs.ram.min,
                available_mb: total,
            });
        } else if let Some(rec) = reqs.ram.recommended
            && total < rec
        {
            warnings.push(Warning::RamBelowRecommended {
                service_name: service_name.to_string(),
                recommended_mb: rec,
                available_mb: total,
            });
        }
    }

    // Build ordered steps
    let mut steps = Vec::new();

    // 1. Create service data directory
    steps.push(Step::CreateDir(home_dir.clone()));

    // Capture env content before it is moved into steps
    let env_content = output.env_file.content.clone();

    // 2. Pull all images (from quadlet bundle)
    for image in &bundle.images {
        steps.push(Step::PullImage {
            image: image.clone(),
        });
    }

    // 3. Write quadlet files from bundle
    for file in bundle.quadlet_files {
        steps.push(Step::WriteFile(file));
    }

    // 4. Write config files from bundle
    for file in bundle.config_files {
        steps.push(Step::WriteFile(file));
    }

    // 5. Write .env file
    steps.push(Step::WriteFile(output.env_file));

    // 6. Create bind mount directories (must exist before container starts)
    for dir in &bundle.bind_mount_dirs {
        steps.push(Step::CreateDir(dir.clone()));
    }

    // 7. Reload and start via systemd
    steps.push(Step::DaemonReload);
    // Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    // Register OIDC client with the auth provider
    if let (
        Some(registry::service_def::AuthKind::Oidc),
        Some(config::schema::AuthCredentials::Authelia { .. }),
    ) = (auth_kind.as_ref(), config.auth.as_ref())
    {
        steps.extend(authelia::register_oidc_client(
            service_name,
            &reg_service.def,
            domain,
            &output.ctx,
            &config,
            &quadlet_path,
        ));
    }

    // Collect post-install info
    let allocated_ports: Vec<(String, u16)> = reg_service
        .def
        .ports
        .iter()
        .map(|p| {
            let port = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
            (p.name.clone(), port)
        })
        .collect();

    // Secret names from env var templates (not stored in state)
    let mut generated_secrets: Vec<String> = reg_service
        .def
        .env
        .iter()
        .filter(|e| !env_overrides.contains_key(&e.name))
        .flat_map(|e| {
            let mut secrets = Vec::new();
            let mut rest = e.value.as_str();
            while let Some(start) = rest.find("{{secret.") {
                let after = &rest[start + 9..];
                if let Some(end) = after.find("}}") {
                    secrets.push(after[..end].to_string());
                    rest = &after[end + 2..];
                } else {
                    break;
                }
            }
            secrets
        })
        .collect();
    // Deduplicate — the same secret may be referenced by multiple env vars
    generated_secrets.dedup();
    generated_secrets.sort();
    generated_secrets.dedup();

    // Caddy reverse proxy: if a domain is provided and Caddy is installed,
    // add a site block to the Caddyfile and restart Caddy.
    if let Some(domain) = domain
        && caddy::is_installed()
    {
        // Use the first container port for the upstream (not the host-mapped port)
        let container_port = reg_service
            .def
            .ports
            .first()
            .map(|p| p.container_port)
            .unwrap_or(8080);

        // Use forward_auth when:
        // - User requested auth (--auth flag), AND
        // - The service has no native OIDC mappings (native OIDC handles auth itself), AND
        // - The service is not the auth provider itself
        let has_native_oidc = !reg_service.def.mappings.auth.is_empty();
        let is_auth_provider = service_name == SERVICE_AUTHELIA;
        let forward_auth = if enable_auth && !has_native_oidc && !is_auth_provider {
            // Look up authelia's container port from the registry, not the host port
            let authelia_container_port = registry::find_service(repo_dir, SERVICE_AUTHELIA)?
                .def
                .ports
                .first()
                .map(|p| p.container_port)
                .ok_or_else(|| {
                    Error::Registry("authelia service has no ports defined in registry".into())
                })?;
            Some(caddy::ForwardAuthParams {
                container_port: authelia_container_port,
                provider: caddy::AuthProvider::Authelia,
            })
        } else {
            None
        };

        let block = caddy::render_site_block(&caddy::CaddySiteParams {
            service_name: service_name.to_string(),
            domain: domain.to_string(),
            container_port,
            forward_auth,
        });

        let caddyfile = caddy::caddyfile_path()?;
        let current = if caddyfile.exists() {
            std::fs::read_to_string(&caddyfile).map_err(|source| Error::FileRead {
                path: caddyfile.clone(),
                source,
            })?
        } else {
            String::new()
        };
        let updated = caddy::add_route(&current, service_name, &block);

        steps.push(Step::WriteFile(GeneratedFile {
            path: caddyfile,
            content: updated,
        }));

        steps.push(Step::ReloadCaddy);
    }

    Ok(AddResult {
        steps,
        warnings,
        repo_url: registry_name.to_string(),
        allocated_ports,
        generated_secrets,
        env_content,
        domain: domain.map(|d| d.to_string()),
    })
}

/// Remove a service: update state, return cleanup steps.
pub fn remove_service(service_name: &str) -> Result<RemoveResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let installed = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    // Stop all units belonging to this service (main + sidecars).
    // Quadlet files named {service_name}*.container each produce a systemd unit.
    let quadlet_path = quadlet_dir()?;
    let mut steps = Vec::new();
    let mut volume_names = Vec::new();

    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !name.starts_with(service_name) {
                continue;
            }
            // Stop each .container unit before removing files
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            // Track volume names for cleanup after containers are stopped
            if name.ends_with(".volume") {
                let vol = name.trim_end_matches(".volume").to_string();
                // Quadlet prefixes volume names with "systemd-"
                volume_names.push(format!("systemd-{vol}"));
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // Reload systemd after removing quadlet files
    steps.push(Step::DaemonReload);

    // Remove podman volumes after containers and units are gone
    for vol_name in volume_names {
        steps.push(Step::RemoveVolume { name: vol_name });
    }

    // Remove Caddy route if the service had a domain
    if installed.domain.is_some() && caddy::is_installed() {
        let caddyfile = caddy::caddyfile_path()?;
        let current = if caddyfile.exists() {
            std::fs::read_to_string(&caddyfile).map_err(|source| Error::FileRead {
                path: caddyfile.clone(),
                source,
            })?
        } else {
            String::new()
        };
        let updated = caddy::remove_route(&current, service_name);
        steps.push(Step::WriteFile(GeneratedFile {
            path: caddyfile,
            content: updated.clone(),
        }));

        // Only reload if there are routes left — caddy rejects an empty Caddyfile
        if !updated.trim().is_empty() {
            steps.push(Step::ReloadCaddy);
        }
    }

    let domain = installed.domain.clone();

    // Remove service data directory
    steps.push(Step::RemoveDir(service_home(service_name)?));

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
        domain,
    })
}

/// Parameters for [`finalize_add`].
pub struct FinalizeAddParams<'a> {
    pub service_name: &'a str,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub registry_name: &'a str,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
    /// The generated .env content, used for post-install config (e.g., auth auto-setup).
    pub env_content: &'a str,
    /// Domain assigned to this service (for Caddy reverse proxy).
    pub domain: Option<&'a str>,
}

/// Called after add steps succeed — records the service in config and saves a snapshot.
pub fn finalize_add(params: FinalizeAddParams<'_>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    let ports: BTreeMap<String, u16> = params.allocated_ports.iter().cloned().collect();

    config.services.push(InstalledService {
        name: params.service_name.to_string(),
        version: "0.1.0".to_string(),
        repo: params.registry_name.to_string(),
        ports,
        auth_kind: params.auth_kind,
        domain: params.domain.map(|d| d.to_string()),
    });

    // Auto-configure [auth] when an auth provider is installed
    if params.service_name == SERVICE_AUTHELIA {
        config.auth = Some(authelia::auth_config(params.allocated_ports)?);
    }

    config::save_config(&paths.config_file, &config)?;

    // Save a snapshot of the service.toml for `ryra diff`
    let service_toml = params
        .repo_dir
        .join(params.service_name)
        .join("service.toml");
    if let Ok(content) = std::fs::read_to_string(&service_toml) {
        config::save_snapshot(&paths.snapshots_dir, params.service_name, &content)?;
    }

    Ok(())
}

pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    config.services.retain(|s| s.name != service_name);
    config::save_config(&paths.config_file, &config)?;
    config::remove_snapshot(&paths.snapshots_dir, service_name)?;

    Ok(())
}

/// Re-scaffold a service with the latest registry definition.
///
/// This is destructive: the service is stopped, all config files are regenerated
/// (including env vars and secrets), and the service is restarted. Volumes are
/// preserved but everything else is rebuilt from scratch.
pub fn update_service(
    service_name: &str,
    env_overrides: &BTreeMap<String, String>,
    repo_dir: &Path,
) -> Result<UpdateResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    // Load snapshot and compute changes
    let snapshot_content = config::load_snapshot(&paths.snapshots_dir, service_name)?;
    let old: registry::service_def::ServiceDef =
        toml::from_str(&snapshot_content).map_err(|source| Error::TomlParse {
            path: paths.snapshots_dir.join(format!("{service_name}.toml")),
            source,
        })?;
    let reg_service = registry::find_service(repo_dir, service_name)?;
    let changes = diff::compute_changes(&old, &reg_service.def);

    let quadlet_path = quadlet_dir()?;

    // Determine host_port from installed service's port mappings
    let host_port = service.ports.values().next().copied();

    let enable_auth = service.auth_kind.is_some();
    let authelia_installed = config.services.iter().any(|s| s.name == SERVICE_AUTHELIA);
    let extra_networks = resolve_extra_networks(
        service_name,
        service.domain.as_deref(),
        enable_auth,
        caddy::is_installed(),
        authelia_installed,
    );

    let mut steps = Vec::new();

    // 1. Stop the service
    steps.push(Step::StopService {
        unit: service_name.to_string(),
    });

    // 2. Generate .env
    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: service.auth_kind.as_ref(),
        host_port,
        env_overrides,
        domain: service.domain.as_deref(),
        extra_env: BTreeMap::new(),
    })?;

    // 3. Process quadlet bundle
    let bundle =
        generate::bundle::process_quadlet_bundle(&generate::bundle::ProcessBundleParams {
            service_dir: &reg_service.service_dir,
            service_name,
            quadlet_dir: &quadlet_path,
            extra_networks: &extra_networks,
            extra_volumes: &[],
            podman_args: &[],
        })?;

    // 4. Pull all images
    for image in &bundle.images {
        steps.push(Step::PullImage {
            image: image.clone(),
        });
    }

    // 5. Write files
    for file in bundle.quadlet_files {
        steps.push(Step::WriteFile(file));
    }
    for file in bundle.config_files {
        steps.push(Step::WriteFile(file));
    }
    steps.push(Step::WriteFile(output.env_file));

    // 6. Create bind mount directories
    for dir in &bundle.bind_mount_dirs {
        steps.push(Step::CreateDir(dir.clone()));
    }

    // 7. Reload and restart
    steps.push(Step::DaemonReload);
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    Ok(UpdateResult { steps, changes })
}

/// Called after update steps succeed — updates the snapshot to match the new registry version.
pub fn finalize_update(service_name: &str, repo_dir: &Path) -> Result<()> {
    let paths = ConfigPaths::resolve()?;

    // Update the snapshot
    let service_toml = repo_dir.join(service_name).join("service.toml");
    if let Ok(content) = std::fs::read_to_string(&service_toml) {
        config::save_snapshot(&paths.snapshots_dir, service_name, &content)?;
    }

    Ok(())
}

/// Reset ryra: tear down all services, infrastructure, and config.
pub fn reset() -> Result<ResetResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file).ok();

    let mut steps = Vec::new();
    let mut volume_names = Vec::new();

    // 1. Stop and remove only ryra-managed quadlet files (scoped by installed service names)
    let quadlet_path = quadlet_dir()?;
    if let Some(ref config) = config
        && quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Only touch files belonging to a ryra-installed service
            let is_ryra_file = config.services.iter().any(|s| name.starts_with(&s.name));
            if !is_ryra_file {
                continue;
            }
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".network") {
                let unit = format!("{}-network", name.trim_end_matches(".network"));
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".volume") {
                let vol = name.trim_end_matches(".volume").to_string();
                volume_names.push(format!("systemd-{vol}"));
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // 2. Reload user systemd after removing quadlets
    steps.push(Step::DaemonReload);

    // 3. Remove podman volumes
    for vol_name in volume_names {
        steps.push(Step::RemoveVolume { name: vol_name });
    }

    // 4. Remove service data directories
    if let Some(ref config) = config {
        for service in &config.services {
            if let Ok(data_dir) = service_home(&service.name)
                && data_dir.exists()
            {
                steps.push(Step::RemoveDir(data_dir));
            }
        }
    }

    Ok(ResetResult { steps })
}

/// Called after reset steps succeed — removes ryra's config directory.
pub fn finalize_reset() -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    if paths.config_dir.exists() {
        std::fs::remove_dir_all(&paths.config_dir).map_err(|source| Error::FileWrite {
            path: paths.config_dir,
            source,
        })?;
    }
    Ok(())
}

/// Get the current status of the ryra installation.
pub fn status() -> config::status::RyraStatus {
    let paths = match ConfigPaths::resolve() {
        Ok(p) => p,
        Err(_) => return config::status::RyraStatus::NotInitialized,
    };

    let config = match config::load_config(&paths.config_file) {
        Ok(c) => c,
        Err(Error::ConfigNotFound(_)) => return config::status::RyraStatus::NotInitialized,
        Err(e) => return config::status::RyraStatus::Error(e.to_string()),
    };

    config::status::RyraStatus::Initialized(config::status::StatusInfo::from_config(
        paths.config_file,
        &config,
    ))
}

/// List installed services.
pub fn list_installed() -> Result<Vec<InstalledService>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;
    Ok(config.services)
}

/// Search available services in a repo, optionally filtered by query.
pub fn search_services(repo_dir: &Path, query: Option<&str>) -> Result<Vec<SearchResult>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let available = registry::list_available(repo_dir)?;

    let results = available
        .into_iter()
        .filter(|reg_svc| match query {
            None => true,
            Some(q) => {
                let q = q.to_lowercase();
                reg_svc.def.service.name.to_lowercase().contains(&q)
                    || reg_svc.def.service.description.to_lowercase().contains(&q)
            }
        })
        .map(|reg_svc| {
            let name = &reg_svc.def.service.name;
            let installed = config.services.iter().any(|s| s.name == *name);
            SearchResult {
                name: name.clone(),
                description: reg_svc.def.service.description,
                installed,
            }
        })
        .collect();

    Ok(results)
}

pub struct SearchResult {
    pub name: String,
    pub description: String,
    pub installed: bool,
}

/// Get test definitions for an installed service by reading its `test.toml`.
pub async fn service_tests(service_name: &str) -> Result<ServiceTestInfo> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let installed = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let service_ref = service_ref_from_installed(installed);
    let repo_dir = resolve_registry_dir(&service_ref).await?;

    let test_toml_path = repo_dir.join(service_name).join("test.toml");
    let env_file = service_home(service_name)?.join(".env");

    if !test_toml_path.exists() {
        return Ok(ServiceTestInfo {
            service_name: service_name.to_string(),
            registry_name: service_ref.registry_name().to_string(),
            tests: vec![],
            env_file,
        });
    }

    let content = std::fs::read_to_string(&test_toml_path).map_err(|source| Error::FileRead {
        path: test_toml_path.clone(),
        source,
    })?;

    #[derive(serde::Deserialize)]
    struct TestFile {
        #[serde(default)]
        tests: Vec<registry::test_def::TestDef>,
    }

    let parsed: TestFile = toml::from_str(&content).map_err(|source| Error::TomlParse {
        path: test_toml_path,
        source,
    })?;

    Ok(ServiceTestInfo {
        service_name: service_name.to_string(),
        registry_name: service_ref.registry_name().to_string(),
        tests: parsed.tests,
        env_file,
    })
}

pub struct ServiceTestInfo {
    pub service_name: String,
    pub registry_name: String,
    pub tests: Vec<registry::test_def::TestDef>,
    pub env_file: PathBuf,
}

/// Get detailed info about a service from a repo.
pub fn service_info(repo_dir: &Path, service_name: &str) -> Result<ServiceDetail> {
    let reg_service = registry::find_service(repo_dir, service_name)?;
    let def = &reg_service.def;

    Ok(ServiceDetail {
        name: def.service.name.clone(),
        description: def.service.description.clone(),
        url: def.service.url.clone(),
        ports: def
            .ports
            .iter()
            .map(|p| (p.container_port, p.protocol.clone(), p.name.clone()))
            .collect(),
        env_vars: def
            .env
            .iter()
            .map(|e| (e.name.clone(), e.prompt.clone()))
            .collect(),
    })
}

pub struct ServiceDetail {
    pub name: String,
    pub description: String,
    pub url: Option<String>,
    pub ports: Vec<(u16, registry::service_def::PortProtocol, String)>,
    pub env_vars: Vec<(String, Option<String>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_empty_when_no_domain() {
        let nets = resolve_extra_networks("whoami", None, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_caddy_when_domain_and_caddy() {
        let nets = resolve_extra_networks("forgejo", Some("git.test.local"), false, true, false);
        assert_eq!(nets, vec!["caddy"]);
    }

    #[test]
    fn networks_empty_when_domain_but_no_caddy() {
        let nets = resolve_extra_networks("forgejo", Some("git.test.local"), false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_caddy_excluded_for_caddy_itself() {
        let nets = resolve_extra_networks("caddy", Some("caddy.test.local"), false, true, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_authelia_when_auth_enabled() {
        let nets = resolve_extra_networks("forgejo", Some("git.test.local"), true, true, true);
        assert_eq!(nets, vec!["caddy", "authelia"]);
    }

    #[test]
    fn networks_authelia_excluded_for_authelia_itself() {
        let nets = resolve_extra_networks("authelia", Some("auth.test.local"), true, true, true);
        assert_eq!(nets, vec!["caddy"]);
    }

    #[test]
    fn networks_caddy_when_auth_without_domain() {
        // Services with auth but no domain still need caddy network for OIDC discovery
        let nets = resolve_extra_networks("jellyfin", None, true, true, true);
        assert_eq!(nets, vec!["caddy", "authelia"]);
    }
}
