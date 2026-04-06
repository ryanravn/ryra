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

// --- Path conventions ---

/// Data directory for a service: ~/.local/share/ryra/<name>
pub fn service_home(service_name: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/share")
        })
        .join("ryra")
        .join(service_name)
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        })
        .join("containers")
        .join("systemd")
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
    /// Pull a container image.
    PullImage { image: String },
    /// Remove a file (requires sudo).
    RemoveFile(PathBuf),
    /// Remove a directory tree (requires sudo).
    RemoveDir(PathBuf),
    /// Create a directory (with parents).
    CreateDir(PathBuf),
    /// Register an OAuth2 provider + application in authentik via its API.
    RegisterAuthProvider {
        service_name: String,
        api_url: String,
        api_token: String,
        client_id: String,
        client_secret: String,
        redirect_uri: String,
        launch_url: String,
    },
    /// Remove an OAuth2 application + provider from authentik via API.
    RemoveAuthProvider {
        service_name: String,
        api_url: String,
        api_token: String,
    },
    /// Run a post-start hook — a shell command on the host after the service is active.
    /// The service's .env is sourced before the command runs.
    PostStartHook {
        name: String,
        service_name: String,
        run: String,
        timeout: u64,
    },
}

impl Step {
    /// Render this step as a shell command (for dry-run display).
    pub fn to_command(&self) -> String {
        match self {
            Step::WriteFile(file) => format!("write {}", file.path.display()),
            Step::DaemonReload => "systemctl --user daemon-reload".into(),
            Step::StartService { unit } => format!("systemctl --user start {unit}"),
            Step::StopService { unit } => format!("systemctl --user stop {unit}"),
            Step::PullImage { image } => format!("podman pull {image}"),
            Step::RemoveFile(path) => format!("sudo rm -f {}", path.display()),
            Step::RemoveDir(path) => format!("sudo rm -rf {}", path.display()),
            Step::CreateDir(path) => format!("mkdir -p {}", path.display()),
            Step::RegisterAuthProvider { service_name, .. } => {
                format!("authentik: register OAuth2 provider for {service_name}")
            }
            Step::RemoveAuthProvider { service_name, .. } => {
                format!("authentik: remove OAuth2 provider for {service_name}")
            }
            Step::PostStartHook {
                name,
                service_name,
                run,
                ..
            } => {
                let home = service_home(service_name);
                format!(
                    "# post-start hook '{name}' for {service_name}\nsh -c '. {}/.env && {run}'",
                    home.display()
                )
            }
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
    /// The generated .env content (for post-install processing without needing sudo).
    pub env_content: String,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub service_name: String,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

pub struct UpdateResult {
    pub steps: Vec<Step>,
    pub changes: Vec<diff::Change>,
}

pub const DEFAULT_REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry");

/// Resolve which repo to use and ensure it's cached.
/// Returns (repo_url, repo_dir).
///
/// Resolution order:
/// 1. Explicit `--repo` argument
/// 2. `default_repo` from ryra.toml config
/// 3. Legacy `[[registries]]` from config
/// 4. Local `./registry/` directory (for development)
/// 5. Hardcoded default (GitHub)
pub async fn resolve_repo(repo: Option<&str>) -> Result<(String, PathBuf)> {
    let paths = ConfigPaths::resolve()?;

    let repo_url = match repo {
        Some(url) => url.to_string(),
        None => {
            let config = config::load_or_default(&paths.config_file).ok();
            config
                .as_ref()
                .and_then(|c| c.default_repo.clone())
                .or_else(|| {
                    config
                        .as_ref()
                        .and_then(|c| c.registries.first().map(|r| r.url.clone()))
                })
                .or_else(|| {
                    // Auto-detect local registry directory
                    let local = PathBuf::from("registry");
                    if local.is_dir() {
                        Some(local.to_string_lossy().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| DEFAULT_REPO.to_string())
        }
    };

    paths.ensure_cache_dir()?;
    let repo_dir = registry::fetch::ensure_repo(&repo_url, &paths.cache_dir).await?;
    Ok((repo_url, repo_dir))
}

/// Initialize a new ryra project.
pub async fn init(config: Config) -> Result<InitResult> {
    let paths = ConfigPaths::resolve()?;

    // Fetch default repo if configured (into cache, which may not exist yet)
    // Cache dir is under /etc/ryra which needs sudo — create it first if we can,
    // otherwise the repo fetch happens after steps execute
    let _ = paths.ensure_dirs();
    if let Some(ref repo_url) = config.default_repo {
        let _ = registry::fetch::ensure_repo(repo_url, &paths.cache_dir).await;
    }

    // Preserve installed services from existing config
    let mut config = config;
    if let Ok(existing) = config::load_or_default(&paths.config_file)
        && !existing.services.is_empty()
    {
        config.services = existing.services;
    }

    // Write config as a step (needs sudo for /etc/ryra)
    let config_content = toml::to_string_pretty(&config)
        .map_err(|e| Error::Template(format!("failed to serialize config: {e}")))?;

    let steps = vec![Step::WriteFile(GeneratedFile {
        path: paths.config_file.clone(),
        content: config_content,
    })];

    Ok(InitResult { steps })
}

/// Add a service: generate config, return steps to execute.
/// `repo_url` and `repo_dir` come from `resolve_repo()`.
pub fn add_service(
    service_name: &str,
    auth_kind: Option<registry::service_def::AuthKind>,
    env_overrides: &BTreeMap<String, String>,
    repo_url: &str,
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

    // Allocate a host port for all service ports
    let has_privileged_port = reg_service
        .def
        .ports
        .iter()
        .any(|p| p.container_port < 1024);
    let host_port = if has_privileged_port {
        Some(system::port::allocate_port(&config)?)
    } else {
        None
    };

    // Check for port conflicts by probing whether the port is already bound.
    for p in &reg_service.def.ports {
        let port = host_port.unwrap_or(p.container_port);
        if system::port::is_port_in_use(port) {
            return Err(Error::PortConflict { port });
        }
    }

    let home_dir = service_home(service_name);
    let quadlet_path = quadlet_dir();

    let output = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: auth_kind.as_ref(),
        host_port,
        quadlet_dir: &quadlet_path,
        env_overrides,
        service_dir: &reg_service.service_dir,
    })?;
    let generated = output.service;

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

    // Capture env content before it's moved into steps
    let env_content = generated.env_file.content.clone();

    // 2. Pull all images (primary + sidecars, deduplicated)
    for image in reg_service.def.all_images() {
        steps.push(Step::PullImage {
            image: image.to_string(),
        });
    }

    // 3. Write quadlet + .env files
    let generate::GeneratedService { files, env_file } = generated;

    for file in files {
        steps.push(Step::WriteFile(file));
    }
    steps.push(Step::WriteFile(env_file));

    // 4. Create bind mount directories (must exist before container starts)
    let all_volumes = reg_service.def.volumes.iter().chain(
        reg_service
            .def
            .containers
            .iter()
            .flat_map(|c| c.volumes.iter()),
    );
    for vol in all_volumes {
        if let Some(ref host_path) = vol.host_path {
            // Replace %h with actual home dir path
            let resolved = host_path.replace("%h", &home_dir.to_string_lossy());
            steps.push(Step::WriteFile(GeneratedFile {
                path: PathBuf::from(resolved).join(".keep"),
                content: String::new(),
            }));
        }
    }

    // 5. Reload and start via systemd
    steps.push(Step::DaemonReload);

    // Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    // Register OAuth provider in authentik via API
    if let (
        Some(registry::service_def::AuthKind::Oidc),
        Some(config::schema::AuthCredentials::Authentik { url, api_token }),
        Some(client_id),
        Some(client_secret),
        Some(service_url),
    ) = (
        auth_kind.as_ref(),
        config.auth.as_ref(),
        output.ctx.get("auth.client_id"),
        output.ctx.get("auth.client_secret"),
        output.ctx.get("service.url"),
    ) {
        steps.push(Step::RegisterAuthProvider {
            service_name: service_name.to_string(),
            api_url: url.clone(),
            api_token: api_token.clone(),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            redirect_uri: format!("{service_url}/.*"),
            launch_url: service_url.clone(),
        });
    }

    // 6. Post-start hooks — run after service is active
    let has_hooks = !reg_service.def.post_start.is_empty();
    for hook in &reg_service.def.post_start {
        steps.push(Step::PostStartHook {
            name: hook.name.clone(),
            service_name: service_name.to_string(),
            run: hook.run.clone(),
            timeout: hook.timeout,
        });
    }

    // Restart the service after post-start hooks so it picks up injected config
    if has_hooks {
        steps.push(Step::StopService {
            unit: service_name.to_string(),
        });
        steps.push(Step::StartService {
            unit: service_name.to_string(),
        });
    }

    // Collect post-install info
    let allocated_ports: Vec<(String, u16)> = reg_service
        .def
        .ports
        .iter()
        .map(|p| {
            let port = host_port.unwrap_or(p.container_port);
            (p.name.clone(), port)
        })
        .collect();

    // Secret names from env var templates (not stored in state)
    let generated_secrets: Vec<String> = reg_service
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

    Ok(AddResult {
        steps,
        warnings,
        repo_url: repo_url.to_string(),
        allocated_ports,
        generated_secrets,
        env_content,
    })
}

/// Remove a service: update state, return cleanup steps.
pub fn remove_service(service_name: &str) -> Result<RemoveResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let _service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    // Stop the service
    let mut steps = vec![Step::StopService {
        unit: service_name.to_string(),
    }];

    // Remove OAuth provider from authentik
    if let Some(config::schema::AuthCredentials::Authentik { url, api_token }) = &config.auth {
        steps.push(Step::RemoveAuthProvider {
            service_name: service_name.to_string(),
            api_url: url.clone(),
            api_token: api_token.clone(),
        });
        // Also clean up legacy blueprint file if it exists
        let blueprint = service_home("authentik")
            .join("blueprints")
            .join(format!("{service_name}.yaml"));
        if blueprint.exists() {
            steps.push(Step::RemoveFile(blueprint));
        }
    }

    // Remove quadlet files matching {service_name}*
    let quadlet_path = quadlet_dir();
    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name.starts_with(service_name) {
                steps.push(Step::RemoveFile(entry.path()));
            }
        }
    }

    // Reload systemd after removing quadlet files
    steps.push(Step::DaemonReload);

    // Remove service data directory
    steps.push(Step::RemoveDir(service_home(service_name)));

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
    })
}

/// Parameters for [`finalize_add`].
pub struct FinalizeAddParams<'a> {
    pub service_name: &'a str,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub repo: &'a str,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
    /// The generated .env content, used for post-install config (e.g., auth auto-setup).
    pub env_content: &'a str,
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
        repo: params.repo.to_string(),
        ports,
        auth_kind: params.auth_kind,
    });

    // Auto-configure [auth] when authentik is installed (so subsequent services
    // can use it for auth without manual setup).
    if params.service_name == "authentik"
        && let Some(token) = parse_env_var(params.env_content, "AUTHENTIK_BOOTSTRAP_TOKEN")
    {
        let port = params
            .allocated_ports
            .iter()
            .find(|(name, _)| name == "http")
            .map(|(_, p)| *p)
            .unwrap_or(9000);
        let url = format!("http://localhost:{port}");
        config.auth = Some(config::schema::AuthCredentials::Authentik {
            url,
            api_token: token,
        });
    }

    config::save_config(&paths.config_file, &config)?;

    // Save a snapshot of the service.toml for `ryra diff`
    let service_toml = params
        .repo_dir
        .join(params.service_name)
        .join("service.toml");
    if let Ok(content) = std::fs::read_to_string(&service_toml) {
        let _ = config::save_snapshot(&paths.snapshots_dir, params.service_name, &content);
    }

    Ok(())
}

/// Parse a `KEY=VALUE` line from a `.env` file, returning the value if found.
fn parse_env_var(env_content: &str, key: &str) -> Option<String> {
    for line in env_content.lines() {
        let line = line.trim();
        if let Some(val) = line
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
        {
            return Some(val.to_string());
        }
    }
    None
}

pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    config.services.retain(|s| s.name != service_name);
    config::save_config(&paths.config_file, &config)?;
    config::remove_snapshot(&paths.snapshots_dir, service_name);

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

    let quadlet_path = quadlet_dir();

    // Determine host_port from installed service's port mappings
    let host_port = service.ports.values().next().copied();

    let mut steps = Vec::new();

    // 1. Stop the service
    steps.push(Step::StopService {
        unit: service_name.to_string(),
    });

    // 2. Regenerate all files from the current registry definition
    let output = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: service.auth_kind.as_ref(),
        host_port,
        quadlet_dir: &quadlet_path,
        env_overrides,
        service_dir: &reg_service.service_dir,
    })?;
    let generated = output.service;

    // 3. Pull all images (primary + sidecars)
    for image in reg_service.def.all_images() {
        steps.push(Step::PullImage {
            image: image.to_string(),
        });
    }

    // 4. Write files and restart
    let generate::GeneratedService { files, env_file } = generated;

    for file in files {
        steps.push(Step::WriteFile(file));
    }
    steps.push(Step::WriteFile(env_file));

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
        let _ = config::save_snapshot(&paths.snapshots_dir, service_name, &content);
    }

    Ok(())
}

/// Reset ryra: tear down all services, infrastructure, and config.
pub fn reset() -> ResetResult {
    let config = ConfigPaths::resolve()
        .ok()
        .and_then(|p| config::load_config(&p.config_file).ok());

    let mut steps = Vec::new();

    // 1. Stop all services known from config
    if let Some(ref config) = config {
        for service in &config.services {
            steps.push(Step::StopService {
                unit: service.name.clone(),
            });
        }
    }

    // 2. Remove quadlet files
    let quadlet_path = quadlet_dir();
    if quadlet_path.is_dir() {
        steps.push(Step::RemoveDir(quadlet_path));
    }

    // 3. Reload user systemd after removing quadlets
    steps.push(Step::DaemonReload);

    // 4. Remove system-level directories
    if PathBuf::from("/etc/ryra").exists() {
        steps.push(Step::RemoveDir(PathBuf::from("/etc/ryra")));
    }

    // 5. Remove service data directories
    if let Some(ref config) = config {
        for service in &config.services {
            let data_dir = service_home(&service.name);
            if data_dir.exists() {
                steps.push(Step::RemoveDir(data_dir));
            }
        }
    }

    ResetResult { steps }
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
                has_sidecars: !reg_svc.def.containers.is_empty(),
                installed,
            }
        })
        .collect();

    Ok(results)
}

pub struct SearchResult {
    pub name: String,
    pub description: String,
    pub has_sidecars: bool,
    pub installed: bool,
}

/// Get test definitions for an installed service.
///
/// Looks up the service in the config, resolves its repo, and returns
/// the `[[tests]]` from its `service.toml`. If `repo_override` is set,
/// loads tests from that repo instead of the installed service's repo.
pub async fn service_tests(
    service_name: &str,
    repo_override: Option<&str>,
) -> Result<ServiceTestInfo> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let installed = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let repo_url = repo_override.unwrap_or(&installed.repo);
    let repo_dir = registry::fetch::ensure_repo(repo_url, &paths.cache_dir).await?;
    let reg_service = registry::find_service(&repo_dir, service_name)?;

    let env_file = service_home(service_name).join(".env");

    Ok(ServiceTestInfo {
        service_name: service_name.to_string(),
        repo_url: repo_url.to_string(),
        tests: reg_service.def.tests,
        env_file,
    })
}

pub struct ServiceTestInfo {
    pub service_name: String,
    pub repo_url: String,
    pub tests: Vec<registry::service_def::TestDef>,
    pub env_file: PathBuf,
}

/// Get test definitions for a multi-service test suite.
///
/// Looks in the registry's `tests/` directory for a matching `.toml` file.
pub async fn suite_tests(suite_name: &str, repo_override: Option<&str>) -> Result<SuiteTestInfo> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let repo_url =
        repo_override.unwrap_or_else(|| config.default_repo.as_deref().unwrap_or(DEFAULT_REPO));
    let repo_dir = registry::fetch::ensure_repo(repo_url, &paths.cache_dir).await?;

    let tests_dir = repo_dir.join("tests");
    let suite_file = tests_dir.join(format!("{suite_name}.toml"));

    if !suite_file.exists() {
        return Err(Error::ServiceNotFound(format!(
            "test suite '{suite_name}' not found at {}",
            suite_file.display()
        )));
    }

    let contents = std::fs::read_to_string(&suite_file).map_err(|source| Error::FileRead {
        path: suite_file.clone(),
        source,
    })?;
    let def: registry::service_def::MultiServiceTestDef =
        toml::from_str(&contents).map_err(|source| Error::TomlParse {
            path: suite_file,
            source,
        })?;

    Ok(SuiteTestInfo {
        name: def.test.name,
        repo_url: repo_url.to_string(),
        services: def.test.services,
        tests: def.tests,
    })
}

pub struct SuiteTestInfo {
    pub name: String,
    pub repo_url: String,
    pub services: Vec<String>,
    pub tests: Vec<registry::service_def::TestDef>,
}

/// Get detailed info about a service from a repo.
pub fn service_info(repo_dir: &Path, service_name: &str) -> Result<ServiceDetail> {
    let reg_service = registry::find_service(repo_dir, service_name)?;
    let def = &reg_service.def;

    Ok(ServiceDetail {
        name: def.service.name.clone(),
        description: def.service.description.clone(),
        url: def.service.url.clone(),
        has_sidecars: !def.containers.is_empty(),
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
    pub has_sidecars: bool,
    pub ports: Vec<(u16, registry::service_def::PortProtocol, String)>,
    pub env_vars: Vec<(String, Option<String>)>,
}
