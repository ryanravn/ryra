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
    /// Public URL for this service (if --url was provided).
    pub url: Option<String>,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub service_name: String,
    /// URL that was assigned to this service (if any).
    pub url: Option<String>,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
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
/// Services with auth join authelia's network for OIDC communication.
fn resolve_extra_networks(
    service_name: &str,
    enable_auth: bool,
    authelia_installed: bool,
) -> Vec<String> {
    let mut networks = Vec::new();
    if enable_auth && authelia_installed && service_name != SERVICE_AUTHELIA {
        networks.push(SERVICE_AUTHELIA.to_string());
    }
    networks
}

/// Add a service: generate config, return steps to execute.
pub fn add_service(
    service_name: &str,
    url: Option<&str>,
    auth_kind: Option<registry::service_def::AuthKind>,
    enable_auth: bool,
    env_overrides: &BTreeMap<String, String>,
    registry_name: &str,
    repo_dir: &Path,
) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    if let Some(existing) = config.services.iter().find(|s| s.name == service_name) {
        if existing.installed {
            return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
        }
        // installed: false — the CLI should clean up via remove_service +
        // finalize_remove before calling add_service again.
        return Err(Error::ServiceIncomplete(service_name.to_string()));
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

    // --auth requires native OIDC support; forward auth is no longer supported
    if enable_auth && reg_service.def.integrations.auth.is_empty() && service_name != SERVICE_AUTHELIA {
        return Err(Error::NoOidcSupport(service_name.to_string()));
    }

    // Determine host port: use fixed host_port from service def if set,
    // allocate one if container port is privileged or already in use,
    // otherwise use container port directly.
    let has_fixed_ports = reg_service.def.ports.iter().any(|p| p.host_port.is_some());
    let needs_allocation = reg_service.def.ports.iter().any(|p| {
        p.host_port.is_none()
            && (p.container_port < 1024 || system::port::is_port_in_use(p.container_port))
    });
    let host_port = if has_fixed_ports {
        // Fixed ports are set per-port in the service def (e.g. Caddy 80/443)
        None
    } else if needs_allocation {
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

    let authelia_installed = config.services.iter().any(|s| s.name == SERVICE_AUTHELIA);
    let caddy_installed = config.services.iter().any(|s| s.name == SERVICE_CADDY && s.installed);

    // When auth is enabled and Caddy handles TLS, mount the Caddy root CA cert
    // into service containers so they trust the self-signed HTTPS cert. OIDC
    // clients connect to Caddy's HTTPS port (via network alias), which requires
    // TLS trust.
    if enable_auth && authelia_installed && caddy_installed && service_name != SERVICE_AUTHELIA && service_name != SERVICE_CADDY {
        // The CA cert is exported by caddy's ExecStartPost to a well-known path
        let ca_cert_host = service_home(SERVICE_CADDY)
            .map(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")).unwrap_or_default())
            .unwrap_or_default();
        if ca_cert_host.exists() {
            // Mount the Caddy CA cert as the standard Linux CA bundle path so
            // Go, Python, etc. pick it up automatically.
            // :z relabels for SELinux (shared across containers).
            extra_volumes.push(format!(
                "{}:/etc/ssl/certs/ca-certificates.crt:ro,z",
                ca_cert_host.display()
            ));
        }
    }
    let extra_networks = resolve_extra_networks(
        service_name,
        enable_auth,
        authelia_installed,
    );

    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: auth_kind.as_ref(),
        host_port,
        env_overrides,
        url,
        extra_env,
    })?;

    let mut podman_args: Vec<String> = Vec::new();

    // Prevent /etc/hosts from leaking into containers with auth — host entries
    // (e.g., 127.0.0.1 auth.local) would override podman DNS aliases that route
    // to Caddy for OIDC.
    if enable_auth && caddy_installed && service_name != SERVICE_AUTHELIA && service_name != SERVICE_CADDY {
        podman_args.push("--no-hosts".into());
    }

    // Build port variable expansions for quadlet PublishPort directives
    let port_vars: Vec<(String, String)> = reg_service
        .def
        .ports
        .iter()
        .map(|p| {
            let resolved = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
            (
                format!("RYRA_PORT_{}", p.name.to_uppercase()),
                resolved.to_string(),
            )
        })
        .collect();

    // Process quadlet bundle from registry
    let bundle =
        generate::bundle::process_quadlet_bundle(&generate::bundle::ProcessBundleParams {
            service_dir: &reg_service.service_dir,
            service_name,
            quadlet_dir: &quadlet_path,
            extra_networks: &extra_networks,
            extra_volumes: &extra_volumes,
            podman_args: &podman_args,
            port_vars: &port_vars,
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

    // 7. Register OIDC client with the auth provider BEFORE starting the service.
    // This must happen first because the service's ExecStartPost (e.g., register-oidc.sh)
    // needs the auth provider configured and caddy's network alias in place so OIDC
    // discovery URLs resolve correctly from within the service container.
    if let (
        Some(registry::service_def::AuthKind::Oidc),
        Some(config::schema::AuthCredentials::Authelia { .. }),
    ) = (auth_kind.as_ref(), config.auth.as_ref())
    {
        steps.extend(authelia::register_oidc_client(
            service_name,
            &reg_service.def,
            url,
            &output.ctx,
            &config,
            &quadlet_path,
        ));
    }

    // 8. Reload and start via systemd
    steps.push(Step::DaemonReload);
    // Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

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

    Ok(AddResult {
        steps,
        warnings,
        repo_url: registry_name.to_string(),
        allocated_ports,
        generated_secrets,
        env_content,
        url: url.map(|u| u.to_string()),
    })
}

/// Check if a quadlet filename belongs to a service.
///
/// Matches `{service_name}.container`, `{service_name}-db.volume`, etc.
/// but NOT `{service_name_prefix}-other.container` (e.g., "whoami" must not
/// match "whoami-auth.container" when "whoami-auth" is a known service).
///
/// `all_service_names` contains every installed service name — used to detect
/// when a longer service name owns the file instead.
fn quadlet_belongs_to(filename: &str, service_name: &str, all_service_names: &[&str]) -> bool {
    if !filename.starts_with(service_name) {
        return false;
    }
    let rest = &filename[service_name.len()..];
    if rest.starts_with('.') {
        return true;
    }
    if !rest.starts_with('-') {
        return false;
    }
    // Check that no other installed service is a longer prefix match.
    // e.g., "whoami-auth.container" with service "whoami" — if "whoami-auth"
    // is also installed, it owns this file.
    !all_service_names.iter().any(|&other| {
        other.len() > service_name.len()
            && other.starts_with(service_name)
            && filename.starts_with(other)
            && filename[other.len()..].starts_with(['.', '-'])
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
    // Quadlet files named {service_name}.ext or {service_name}-sidecar.ext.
    let quadlet_path = quadlet_dir()?;
    let mut steps = Vec::new();
    let mut volume_names = Vec::new();
    let all_names: Vec<&str> = config.services.iter().map(|s| s.name.as_str()).collect();

    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !quadlet_belongs_to(&name, service_name, &all_names) {
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

    let url = installed.url.clone();

    // Remove service data directory
    steps.push(Step::RemoveDir(service_home(service_name)?));

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
        url,
    })
}

/// Parameters for [`record_pending`].
pub struct RecordPendingParams<'a> {
    pub service_name: &'a str,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub registry_name: &'a str,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
    /// Public URL for this service (browser-visible, e.g., https://docs.example.com).
    pub url: Option<&'a str>,
}

/// Record a service as pending installation (installed: false).
/// Called BEFORE executing steps so that partial failures are recoverable.
pub fn record_pending(params: RecordPendingParams<'_>) -> Result<()> {
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
        url: params.url.map(|u| u.to_string()),
        installed: false,
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
    let content = std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
        path: service_toml,
        source,
    })?;
    config::save_snapshot(&paths.snapshots_dir, params.service_name, &content)?;

    Ok(())
}

/// Mark a pending service as fully installed.
/// Called AFTER all steps have executed successfully.
pub fn mark_installed(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;

    let service = config
        .services
        .iter_mut()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    service.installed = true;
    config::save_config(&paths.config_file, &config)?;

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

/// Reset ryra: tear down all services, infrastructure, and config.
pub fn reset() -> Result<ResetResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file).ok();

    let mut steps = Vec::new();
    let mut volume_names = Vec::new();

    // 1. Stop and remove only ryra-managed quadlet files (scoped by installed service names)
    let quadlet_path = quadlet_dir()?;
    let all_names: Vec<&str> = config
        .as_ref()
        .map(|c| c.services.iter().map(|s| s.name.as_str()).collect())
        .unwrap_or_default();
    if let Some(ref config) = config
        && quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Only touch files belonging to a ryra-installed service
            let is_ryra_file = config.services.iter().any(|s| quadlet_belongs_to(&name, &s.name, &all_names));
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
    fn networks_empty_when_no_auth() {
        let nets = resolve_extra_networks("whoami", false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_empty_when_auth_but_no_authelia() {
        let nets = resolve_extra_networks("forgejo", true, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_authelia_when_auth_enabled() {
        let nets = resolve_extra_networks("forgejo", true, true);
        assert_eq!(nets, vec!["authelia"]);
    }

    #[test]
    fn networks_authelia_excluded_for_authelia_itself() {
        let nets = resolve_extra_networks("authelia", true, true);
        assert!(nets.is_empty());
    }

    #[test]
    fn quadlet_belongs_to_exact_match() {
        let all = &["whoami", "whoami-auth"];
        assert!(quadlet_belongs_to("whoami.container", "whoami", all));
        assert!(quadlet_belongs_to("whoami.network", "whoami", all));
    }

    #[test]
    fn quadlet_belongs_to_sidecar() {
        // whoami-db is a sidecar, not a separate service
        let all = &["whoami"];
        assert!(quadlet_belongs_to("whoami-db.volume", "whoami", all));
    }

    #[test]
    fn quadlet_belongs_to_rejects_prefix_collision() {
        let all = &["whoami", "whoami-auth"];
        assert!(!quadlet_belongs_to("whoami-auth.container", "whoami", all));
        assert!(!quadlet_belongs_to("whoami-auth-db.volume", "whoami", all));
    }

    #[test]
    fn quadlet_belongs_to_hyphenated_service() {
        let all = &["whoami", "whoami-auth"];
        assert!(quadlet_belongs_to("whoami-auth.container", "whoami-auth", all));
        assert!(quadlet_belongs_to("whoami-auth-db.volume", "whoami-auth", all));
        assert!(!quadlet_belongs_to("whoami.container", "whoami-auth", all));
    }
}
