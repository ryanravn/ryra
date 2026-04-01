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
use config::schema::{CloudflareCredentials, Config, ExposureMode, InstalledService};
use error::{Error, Result};
use generate::GeneratedFile;
use registry::service_def::PortProtocol;

// --- Per-service user conventions ---

pub fn service_user(service_name: &str) -> String {
    service_name.to_string()
}

pub fn service_home(service_name: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/{service_name}"))
}

pub fn service_quadlet_dir(service_name: &str) -> PathBuf {
    service_home(service_name)
        .join(".config")
        .join("containers")
        .join("systemd")
}

// --- Typed steps: what the CLI needs to execute ---

/// A discrete operation that the CLI executes. Pattern matching ensures
/// every step type is handled — no string parsing or if-chains.
pub enum Step {
    /// Ensure the `ryra` system group exists (idempotent).
    EnsureGroup,
    /// Remove the `ryra` system group.
    RemoveGroup,
    /// Create a system user for a service and add it to the `ryra` group.
    CreateUser { username: String, home_dir: PathBuf },
    /// Enable systemd linger so user services persist.
    EnableLinger { username: String },
    /// Disable systemd linger.
    DisableLinger { username: String },
    /// Terminate a user's systemd session.
    TerminateUserSession { username: String },
    /// Kill all processes owned by a user (SIGKILL). Ensures userdel can succeed.
    KillUserProcesses { username: String },
    /// Write a file (requires sudo — goes to service user home or /etc).
    WriteFile(GeneratedFile),
    /// Set ownership of a path to a user.
    Chown { path: PathBuf, username: String },
    /// Reload systemd for a service user.
    DaemonReload { username: String },
    /// Start a service under a user's systemd.
    StartService { username: String, unit: String },
    /// Stop a service under a user's systemd.
    StopService { username: String, unit: String },
    /// Reload the system-level systemd (for nginx).
    SystemDaemonReload,
    /// Start a system-level service.
    SystemStart { unit: String },
    /// Restart a system-level service (e.g., nginx after config change).
    SystemRestart { unit: String },
    /// Stop a system-level service.
    SystemStop { unit: String },
    /// Create a Cloudflare DNS A record.
    CreateDnsRecord {
        api_token: String,
        zone_id: String,
        domain: String,
        proxied: bool,
    },
    /// Delete a Cloudflare DNS A record.
    DeleteDnsRecord {
        api_token: String,
        zone_id: String,
        domain: String,
    },
    /// Obtain a Let's Encrypt certificate via certbot container.
    ObtainCert {
        domain: String,
        email: String,
        cloudflare_api_token: Option<String>,
    },
    /// Generate a self-signed origin cert (for Cloudflare proxy mode).
    GenerateOriginCert { domain: String },
    /// Start the cloudflared tunnel quadlet.
    StartTunnel,
    /// Stop the cloudflared tunnel.
    StopTunnel,
    /// Add a hostname to the tunnel ingress + create CNAME.
    AddTunnelRoute {
        api_token: String,
        account_id: String,
        tunnel_id: String,
        zone_id: String,
        domain: String,
    },
    /// Remove a hostname from the tunnel ingress + delete CNAME.
    RemoveTunnelRoute {
        api_token: String,
        account_id: String,
        tunnel_id: String,
        zone_id: String,
        domain: String,
    },
    /// Pull a container image. If username is set, pull as that user (rootless).
    PullImage {
        image: String,
        username: Option<String>,
    },
    /// Remove a file (requires sudo).
    RemoveFile(PathBuf),
    /// Remove a directory tree (requires sudo).
    RemoveDir(PathBuf),
    /// Remove a Linux user and their home directory.
    RemoveUser { username: String },
    /// Register an OAuth2 provider + application in authentik via its API.
    /// Replaces blueprints which silently fail on newer authentik versions.
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
            Step::EnsureGroup => "sudo groupadd --system ryra 2>/dev/null || true".into(),
            Step::RemoveGroup => "sudo groupdel ryra 2>/dev/null || true".into(),
            Step::CreateUser { username, home_dir } => format!(
                "sudo useradd --system --shell $(which nologin) --home-dir {} --create-home {username} && sudo usermod -aG ryra {username}",
                home_dir.display()
            ),
            Step::EnableLinger { username } => format!("sudo loginctl enable-linger {username}"),
            Step::DisableLinger { username } => format!("sudo loginctl disable-linger {username}"),
            Step::TerminateUserSession { username } => {
                format!("sudo loginctl terminate-user {username}")
            }
            Step::KillUserProcesses { username } => {
                format!("sudo pkill -9 -u {username} || true")
            }
            Step::WriteFile(file) => format!("write {}", file.path.display()),
            Step::Chown { path, username } => {
                format!("sudo chown -R {username}:{username} {}", path.display())
            }
            Step::DaemonReload { username } => {
                format!("sudo systemctl --machine={username}@ --user daemon-reload")
            }
            Step::StartService { username, unit } => {
                format!("sudo systemctl --machine={username}@ --user start {unit}")
            }
            Step::StopService { username, unit } => {
                format!("sudo systemctl --machine={username}@ --user stop {unit}")
            }
            Step::SystemDaemonReload => "sudo systemctl daemon-reload".into(),
            Step::SystemStart { unit } => format!("sudo systemctl start {unit}"),
            Step::SystemRestart { unit } => format!("sudo systemctl restart {unit}"),
            Step::SystemStop { unit } => format!("sudo systemctl stop {unit}"),
            Step::CreateDnsRecord {
                domain, proxied, ..
            } => {
                let mode = if *proxied { "proxied" } else { "DNS-only" };
                format!("cloudflare: create A record for {domain} ({mode})")
            }
            Step::DeleteDnsRecord { domain, .. } => {
                format!("cloudflare: delete A record for {domain}")
            }
            Step::ObtainCert { domain, .. } => {
                format!("certbot: obtain certificate for {domain}")
            }
            Step::GenerateOriginCert { domain } => {
                format!("openssl: generate self-signed origin cert for {domain}")
            }
            Step::StartTunnel => "sudo systemctl start cloudflared".into(),
            Step::StopTunnel => "sudo systemctl stop cloudflared".into(),
            Step::AddTunnelRoute { domain, .. } => {
                format!("cloudflare tunnel: add route {domain} -> http://localhost:80")
            }
            Step::RemoveTunnelRoute { domain, .. } => {
                format!("cloudflare tunnel: remove route for {domain}")
            }
            Step::PullImage { image, username } => match username {
                Some(u) => format!("sudo -u {u} podman pull {image}"),
                None => format!("sudo podman pull {image}"),
            },
            Step::RemoveFile(path) => format!("sudo rm -f {}", path.display()),
            Step::RemoveDir(path) => format!("sudo rm -rf {}", path.display()),
            Step::RemoveUser { username } => format!("sudo userdel --remove {username}"),
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
            } => format!(
                "# post-start hook '{name}' for {service_name}\nsudo sh -c '. /var/lib/{service_name}/.env && {run}'"
            ),
        }
    }
}

// --- Warnings ---

/// Warnings generated during service operations that the CLI should display.
pub enum Warning {
    /// Web service exposed publicly without auth protection.
    NoAuthPublicExposure {
        service_name: String,
        exposure: ExposureMode,
    },
    /// Service bound to 0.0.0.0 — reachable from network.
    HostPortExposure {
        service_name: String,
        ports: Vec<(u16, PortProtocol)>,
    },
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
    pub domain: Option<String>,
    pub username: String,
    pub warnings: Vec<Warning>,
    pub repo_url: String,
    pub host_port: Option<u16>,
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in .env).
    pub generated_secrets: Vec<String>,
    /// The generated .env content (for post-install processing without needing sudo).
    pub env_content: String,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub username: String,
    pub service_name: String,
    pub domain: Option<String>,
    pub exposure: ExposureMode,
}

pub struct ExposeResult {
    pub steps: Vec<Step>,
    pub warnings: Vec<Warning>,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

pub struct UpdateResult {
    pub steps: Vec<Step>,
    pub changes: Vec<diff::Change>,
    pub username: String,
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

    let steps = vec![
        Step::EnsureGroup,
        Step::WriteFile(GeneratedFile {
            path: paths.config_file.clone(),
            content: config_content,
        }),
    ];

    Ok(InitResult { steps })
}

/// Add a service: generate config, return steps to execute.
/// `repo_url` and `repo_dir` come from `resolve_repo()`.
pub fn add_service(
    service_name: &str,
    domain: Option<&str>,
    exposure: ExposureMode,
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

    let has_nginx = reg_service.def.nginx.is_some();
    let proxied = exposure.needs_domain();

    // Validate: proxied modes require nginx config
    if proxied && !has_nginx {
        return Err(Error::InvalidExposure(format!(
            "{} exposure requires an HTTP service (no [nginx] config)",
            exposure.label()
        )));
    }

    // Allocate a host port for proxied modes (nginx upstream) or when any
    // container port is privileged (<1024) — rootless podman cannot bind those.
    let has_privileged_port = reg_service
        .def
        .ports
        .iter()
        .any(|p| p.container_port < 1024);
    let host_port = if proxied || has_privileged_port {
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

    let username = service_user(service_name);
    let home_dir = service_home(service_name);
    let quadlet_dir = service_quadlet_dir(service_name);
    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    let output = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        domain,
        exposure: &exposure,
        auth_kind: auth_kind.as_ref(),
        host_port,
        quadlet_dir: &quadlet_dir,
        nginx_dir,
        env_overrides,
        service_dir: &reg_service.service_dir,
    })?;
    let generated = output.service;
    let cross_service_files = output.cross_service_files;

    // Generate warnings
    let mut warnings = Vec::new();

    if proxied && auth_kind.is_none() {
        warnings.push(Warning::NoAuthPublicExposure {
            service_name: service_name.to_string(),
            exposure: exposure.clone(),
        });
    }

    if exposure == ExposureMode::HostPort {
        let ports: Vec<(u16, PortProtocol)> = reg_service
            .def
            .ports
            .iter()
            .map(|p| (p.container_port, p.protocol.clone()))
            .collect();
        warnings.push(Warning::HostPortExposure {
            service_name: service_name.to_string(),
            ports,
        });
    }

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

    // nginx is needed when:
    // - a service is proxied (has a domain), OR
    // - a service uses auth with managed authentik (inter-service communication)
    let needs_auth_proxy = auth_kind.is_some()
        && matches!(config.auth, Some(config::schema::AuthCredentials::Authentik { .. }));
    let needs_nginx = proxied || needs_auth_proxy;

    // 0. Ensure nginx is set up
    if needs_nginx && !PathBuf::from("/etc/containers/systemd/nginx.container").exists() {
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/sites/.keep"),
            content: String::new(),
        }));
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/certs/.keep"),
            content: String::new(),
        }));
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/nginx.conf"),
            content: generate::nginx::render_nginx_base_conf(),
        }));
        steps.push(Step::PullImage {
            image: "docker.io/library/nginx:alpine".into(),
            username: None,
        });
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/containers/systemd/nginx.container"),
            content: generate::nginx::render_nginx_quadlet(),
        }));
        steps.push(Step::SystemDaemonReload);
        steps.push(Step::SystemStart {
            unit: "nginx".into(),
        });

        // Cloudflared (if tunnel configured and not already running)
        if let Some(CloudflareCredentials {
            tunnel: Some(ref ti),
            ..
        }) = config.cloudflare
            && !PathBuf::from("/etc/containers/systemd/cloudflared.container").exists()
        {
            steps.push(Step::WriteFile(GeneratedFile {
                path: PathBuf::from("/etc/containers/systemd/cloudflared.container"),
                content: generate::tunnel::render_cloudflared_quadlet(&ti.tunnel_token),
            }));
            steps.push(Step::SystemDaemonReload);
            steps.push(Step::StartTunnel);
        }
    }

    // 0b. Ensure the auth provider has an internal nginx site for inter-service traffic.
    // Other services' containers reach auth via http://host.containers.internal:<port>.
    let auth_internal_site = PathBuf::from("/etc/ryra/nginx/sites/auth-internal.conf");
    if needs_auth_proxy && !auth_internal_site.exists() {
        if let Some(config::schema::AuthCredentials::Authentik { url, .. }) = &config.auth {
            // Extract the port from the auth URL (e.g., "http://localhost:9000" → 9000)
            let auth_port = url
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(9000);
            steps.push(Step::WriteFile(GeneratedFile {
                path: auth_internal_site,
                content: generate::nginx::render_internal_site(
                    "authentik",
                    auth_port,
                    system::port::AUTH_INTERNAL_PORT,
                ),
            }));
            // Restart nginx to pick up the new site. When nginx is being installed
            // in the same `ryra add` call, the quadlet doesn't exist at planning time
            // but will be running by the time this step executes.
            steps.push(Step::SystemRestart {
                unit: "nginx".into(),
            });
        }
    }

    // 1. Networking: only for proxied modes
    if proxied {
        match (&exposure, domain) {
            (ExposureMode::Tunnel, Some(domain)) => {
                if let Some(cf) = &config.cloudflare
                    && let Some(ti) = &cf.tunnel
                {
                    steps.push(Step::AddTunnelRoute {
                        api_token: cf.api_token.clone(),
                        account_id: ti.account_id.clone(),
                        tunnel_id: ti.tunnel_id.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                    });
                }
            }
            (ExposureMode::Proxy, Some(domain)) => {
                if let Some(cf) = &config.cloudflare {
                    steps.push(Step::CreateDnsRecord {
                        api_token: cf.api_token.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                        proxied: true,
                    });
                }
            }
            (ExposureMode::DnsOnly, Some(domain)) => {
                if let Some(cf) = &config.cloudflare {
                    steps.push(Step::CreateDnsRecord {
                        api_token: cf.api_token.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                        proxied: false,
                    });
                }
            }
            _ => {}
        }

        // 2. SSL certificate — depends on exposure mode (web only)
        if let Some(domain) = domain {
            match &exposure {
                ExposureMode::Proxy => {
                    steps.push(Step::GenerateOriginCert {
                        domain: domain.to_string(),
                    });
                }
                ExposureMode::DnsOnly => {
                    // LE with DNS-01 via Cloudflare; custom certs skip this step
                    if let Some(config::schema::SslConfig::Letsencrypt { email }) = &config.ssl {
                        let cf_token = config.cloudflare.as_ref().map(|cf| cf.api_token.clone());
                        steps.push(Step::ObtainCert {
                            domain: domain.to_string(),
                            email: email.clone(),
                            cloudflare_api_token: cf_token,
                        });
                    }
                }
                ExposureMode::Public => {
                    // LE with HTTP-01 standalone (no Cloudflare); custom certs skip this step
                    if let Some(config::schema::SslConfig::Letsencrypt { email }) = &config.ssl {
                        steps.push(Step::ObtainCert {
                            domain: domain.to_string(),
                            email: email.clone(),
                            cloudflare_api_token: None,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // 3. Ensure ryra group exists, then create service user
    steps.push(Step::EnsureGroup);
    steps.push(Step::CreateUser {
        username: username.clone(),
        home_dir: home_dir.clone(),
    });
    steps.push(Step::EnableLinger {
        username: username.clone(),
    });

    // Capture env content before it's moved into steps
    let env_content = generated.env_file.content.clone();

    // 4. Pull all images (primary + sidecars, deduplicated)
    for image in reg_service.def.all_images() {
        steps.push(Step::PullImage {
            image: image.to_string(),
            username: Some(username.clone()),
        });
    }

    // 5. Write quadlet + .env + nginx files
    let generate::GeneratedService {
        files,
        env_file,
        nginx_site,
    } = generated;

    for file in files {
        steps.push(Step::WriteFile(file));
    }
    steps.push(Step::WriteFile(env_file));
    if let Some(nginx_site) = nginx_site {
        steps.push(Step::WriteFile(nginx_site));
    }

    // 6. Create bind mount directories (must exist before container starts)
    let all_volumes = reg_service
        .def
        .volumes
        .iter()
        .chain(reg_service.def.containers.iter().flat_map(|c| c.volumes.iter()));
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

    // 7. Fix ownership, start via systemd
    steps.push(Step::Chown {
        path: home_dir,
        username: username.clone(),
    });
    steps.push(Step::DaemonReload {
        username: username.clone(),
    });

    // 7. Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        username: username.clone(),
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

    // Reload nginx if proxied
    if proxied {
        steps.push(Step::SystemRestart {
            unit: "nginx".into(),
        });
    }

    // 8. Post-start hooks — run after service is active
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
            username: username.clone(),
            unit: service_name.to_string(),
        });
        steps.push(Step::StartService {
            username: username.clone(),
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
        domain: domain.map(|d| d.to_string()),
        username,
        warnings,
        repo_url: repo_url.to_string(),
        host_port,
        allocated_ports,
        generated_secrets,
        env_content,
    })
}

/// Remove a service: update state, return cleanup steps.
pub fn remove_service(service_name: &str) -> Result<RemoveResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let username = service_user(service_name);
    let domain = service.domain.clone();
    let was_proxied = domain.is_some();

    // Stop the service
    let mut steps = Vec::new();
    steps.push(Step::StopService {
        username: username.clone(),
        unit: service_name.to_string(),
    });
    steps.push(Step::DisableLinger {
        username: username.clone(),
    });
    steps.push(Step::TerminateUserSession {
        username: username.clone(),
    });
    steps.push(Step::RemoveUser {
        username: username.clone(),
    });

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

    // Clean up nginx if service was proxied
    if was_proxied {
        steps.push(Step::RemoveFile(PathBuf::from(format!(
            "/etc/ryra/nginx/sites/{service_name}.conf"
        ))));
        steps.push(Step::SystemRestart {
            unit: "nginx".into(),
        });
    }

    // Clean up networking based on stored exposure mode (web services only)
    if let (Some(cf), Some(domain)) = (&config.cloudflare, &domain) {
        match &service.exposure {
            ExposureMode::Tunnel => {
                if let Some(ti) = &cf.tunnel {
                    steps.push(Step::RemoveTunnelRoute {
                        api_token: cf.api_token.clone(),
                        account_id: ti.account_id.clone(),
                        tunnel_id: ti.tunnel_id.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.clone(),
                    });
                }
            }
            ExposureMode::Proxy | ExposureMode::DnsOnly => {
                steps.push(Step::DeleteDnsRecord {
                    api_token: cf.api_token.clone(),
                    zone_id: cf.zone_id.clone(),
                    domain: domain.clone(),
                });
            }
            ExposureMode::Public | ExposureMode::Local | ExposureMode::HostPort => {}
        }
    }

    Ok(RemoveResult {
        steps,
        username,
        service_name: service_name.to_string(),
        domain,
        exposure: service.exposure.clone(),
    })
}

/// Parameters for [`finalize_add`].
pub struct FinalizeAddParams<'a> {
    pub service_name: &'a str,
    pub domain: Option<&'a str>,
    pub exposure: ExposureMode,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub repo: &'a str,
    pub host_port: Option<u16>,
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
        domain: params.domain.map(|d| d.to_string()),
        version: "0.1.0".to_string(),
        exposure: params.exposure,
        repo: params.repo.to_string(),
        host_port: params.host_port,
        ports,
        auth_kind: params.auth_kind,
    });

    // Auto-configure [auth] when authentik is installed (so subsequent services
    // can use it for auth without manual setup).
    if params.service_name == "authentik" {
        if let Some(token) = parse_env_var(params.env_content, "AUTHENTIK_BOOTSTRAP_TOKEN") {
            let url = match params.domain {
                Some(domain) => format!("https://{domain}"),
                None => {
                    // Local mode — use localhost with the allocated port
                    let port = params.host_port.unwrap_or(9000);
                    format!("http://localhost:{port}")
                }
            };
            config.auth = Some(config::schema::AuthCredentials::Authentik {
                url,
                api_token: token,
            });
        }
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
        if let Some(val) = line.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')) {
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

    let username = service_user(service_name);
    let home_dir = service_home(service_name);
    let quadlet_dir = service_quadlet_dir(service_name);
    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    let mut steps = Vec::new();

    // 1. Stop the service
    steps.push(Step::StopService {
        username: username.clone(),
        unit: service_name.to_string(),
    });

    // 2. Regenerate all files from the current registry definition
    let output = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        domain: service.domain.as_deref(),
        exposure: &service.exposure,
        auth_kind: service.auth_kind.as_ref(),
        host_port: service.host_port,
        quadlet_dir: &quadlet_dir,
        nginx_dir,
        env_overrides,
        service_dir: &reg_service.service_dir,
    })?;
    let generated = output.service;
    let cross_service_files = output.cross_service_files;

    // 3. Pull all images (primary + sidecars)
    for image in reg_service.def.all_images() {
        steps.push(Step::PullImage {
            image: image.to_string(),
            username: Some(username.clone()),
        });
    }

    // 4. Write files and restart
    let generate::GeneratedService {
        files,
        env_file,
        nginx_site,
    } = generated;

    for file in files {
        steps.push(Step::WriteFile(file));
    }
    steps.push(Step::WriteFile(env_file));
    if let Some(nginx_site) = nginx_site {
        steps.push(Step::WriteFile(nginx_site));
    }

    steps.push(Step::Chown {
        path: home_dir,
        username: username.clone(),
    });
    steps.push(Step::DaemonReload {
        username: username.clone(),
    });
    steps.push(Step::StartService {
        username: username.clone(),
        unit: service_name.to_string(),
    });

    // Write cross-service files (e.g., auth blueprints)
    for file in cross_service_files {
        steps.push(Step::WriteFile(file));
    }

    // Reload nginx if proxied
    if service.domain.is_some() {
        steps.push(Step::SystemRestart {
            unit: "nginx".into(),
        });
    }

    Ok(UpdateResult {
        steps,
        changes,
        username,
    })
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

/// Change the exposure mode of an installed service.
/// Tears down old networking/nginx/certs, sets up new ones.
pub fn change_exposure(
    service_name: &str,
    new_exposure: ExposureMode,
    new_domain: Option<&str>,
    repo_dir: &Path,
) -> Result<ExposeResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let service = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let reg_service = registry::find_service(repo_dir, service_name)?;
    let has_nginx = reg_service.def.nginx.is_some();
    let new_proxied = new_exposure.needs_domain();

    // Validate: proxied modes require nginx config
    if new_proxied && !has_nginx {
        return Err(Error::InvalidExposure(format!(
            "{} exposure requires an HTTP service (no [nginx] config)",
            new_exposure.label()
        )));
    }

    let old_exposure = &service.exposure;
    let old_domain = service.domain.as_deref();
    let old_proxied = old_domain.is_some();
    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    let mut steps = Vec::new();

    // --- Tear down old networking ---
    if let (Some(cf), Some(domain)) = (&config.cloudflare, old_domain) {
        match old_exposure {
            ExposureMode::Tunnel => {
                if let Some(ti) = &cf.tunnel {
                    steps.push(Step::RemoveTunnelRoute {
                        api_token: cf.api_token.clone(),
                        account_id: ti.account_id.clone(),
                        tunnel_id: ti.tunnel_id.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                    });
                }
            }
            ExposureMode::Proxy | ExposureMode::DnsOnly => {
                steps.push(Step::DeleteDnsRecord {
                    api_token: cf.api_token.clone(),
                    zone_id: cf.zone_id.clone(),
                    domain: domain.to_string(),
                });
            }
            ExposureMode::Public | ExposureMode::Local | ExposureMode::HostPort => {}
        }
    }

    // Remove old nginx site if was proxied
    if old_proxied {
        steps.push(Step::RemoveFile(PathBuf::from(format!(
            "/etc/ryra/nginx/sites/{service_name}.conf"
        ))));
    }

    // --- Ensure nginx infra if going proxied for first time ---
    if new_proxied && !PathBuf::from("/etc/containers/systemd/nginx.container").exists() {
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/sites/.keep"),
            content: String::new(),
        }));
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/certs/.keep"),
            content: String::new(),
        }));
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/nginx.conf"),
            content: generate::nginx::render_nginx_base_conf(),
        }));
        steps.push(Step::PullImage {
            image: "docker.io/library/nginx:alpine".into(),
            username: None,
        });
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/containers/systemd/nginx.container"),
            content: generate::nginx::render_nginx_quadlet(),
        }));
        steps.push(Step::SystemDaemonReload);
        steps.push(Step::SystemStart {
            unit: "nginx".into(),
        });
    }

    // --- Set up new networking ---
    if new_proxied && let Some(domain) = new_domain {
        match &new_exposure {
            ExposureMode::Tunnel => {
                if let Some(cf) = &config.cloudflare
                    && let Some(ti) = &cf.tunnel
                {
                    steps.push(Step::AddTunnelRoute {
                        api_token: cf.api_token.clone(),
                        account_id: ti.account_id.clone(),
                        tunnel_id: ti.tunnel_id.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                    });
                }
            }
            ExposureMode::Proxy => {
                if let Some(cf) = &config.cloudflare {
                    steps.push(Step::CreateDnsRecord {
                        api_token: cf.api_token.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                        proxied: true,
                    });
                }
            }
            ExposureMode::DnsOnly => {
                if let Some(cf) = &config.cloudflare {
                    steps.push(Step::CreateDnsRecord {
                        api_token: cf.api_token.clone(),
                        zone_id: cf.zone_id.clone(),
                        domain: domain.to_string(),
                        proxied: false,
                    });
                }
            }
            ExposureMode::Public | ExposureMode::Local | ExposureMode::HostPort => {}
        }

        // SSL for new mode
        match &new_exposure {
            ExposureMode::Proxy => {
                steps.push(Step::GenerateOriginCert {
                    domain: domain.to_string(),
                });
            }
            ExposureMode::DnsOnly => {
                if let Some(config::schema::SslConfig::Letsencrypt { email }) = &config.ssl {
                    let cf_token = config.cloudflare.as_ref().map(|cf| cf.api_token.clone());
                    steps.push(Step::ObtainCert {
                        domain: domain.to_string(),
                        email: email.clone(),
                        cloudflare_api_token: cf_token,
                    });
                }
                // Custom certs: no step needed, certs already in place
            }
            ExposureMode::Public => {
                if let Some(config::schema::SslConfig::Letsencrypt { email }) = &config.ssl {
                    steps.push(Step::ObtainCert {
                        domain: domain.to_string(),
                        email: email.clone(),
                        cloudflare_api_token: None, // HTTP-01 standalone
                    });
                }
                // Custom certs: no step needed, certs already in place
            }
            _ => {}
        }

        // Generate new nginx site config
        let host_port = service.host_port.or_else(|| {
            // Allocate a port if we're going proxied and don't have one
            system::port::allocate_port(&config).ok()
        });

        if let Some(upstream_port) = host_port {
            let nginx_site = generate::generate_nginx_site(
                &config,
                &reg_service.def,
                service_name,
                Some(domain),
                &new_exposure,
                Some(upstream_port),
                nginx_dir,
            )?;
            if let Some(site_file) = nginx_site {
                steps.push(Step::WriteFile(site_file));
            }
        }
    }

    // Reload nginx if either old or new was proxied
    if old_proxied || new_proxied {
        steps.push(Step::SystemRestart {
            unit: "nginx".into(),
        });
    }

    // Warnings
    let mut warnings = Vec::new();
    if new_exposure == ExposureMode::HostPort {
        let ports: Vec<(u16, PortProtocol)> = reg_service
            .def
            .ports
            .iter()
            .map(|p| (p.container_port, p.protocol.clone()))
            .collect();
        if !ports.is_empty() {
            warnings.push(Warning::HostPortExposure {
                service_name: service_name.to_string(),
                ports,
            });
        }
    }

    Ok(ExposeResult { steps, warnings })
}

/// Called after expose steps succeed — updates the service record in config.
pub fn finalize_expose(
    service_name: &str,
    new_exposure: ExposureMode,
    new_domain: Option<&str>,
    new_host_port: Option<u16>,
) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    if let Some(svc) = config.services.iter_mut().find(|s| s.name == service_name) {
        svc.exposure = new_exposure;
        svc.domain = new_domain.map(|d| d.to_string());
        if new_host_port.is_some() {
            svc.host_port = new_host_port;
        }
    }
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

/// Reset ryra: tear down all services, infrastructure, and config.
/// Discovers service users from the `ryra` system group so orphaned users
/// (from partial installs) are always found — even without a config file.
pub fn reset() -> ResetResult {
    let config = ConfigPaths::resolve()
        .ok()
        .and_then(|p| config::load_config(&p.config_file).ok());

    let mut steps = Vec::new();

    // 1. Discover all ryra-managed users from the system group.
    //    This catches orphaned users that the config file doesn't know about.
    let group_users = ryra_group_members();

    // 2. Clean up services known from config (includes DNS/tunnel cleanup)
    let mut handled_users = Vec::new();
    if let Some(ref config) = config {
        for service in &config.services {
            let username = service_user(&service.name);
            push_service_teardown(&mut steps, &username, &service.name);
            handled_users.push(username);

            // Clean up networking based on stored exposure (web services only)
            if let (Some(cf), Some(domain)) = (&config.cloudflare, &service.domain) {
                match &service.exposure {
                    ExposureMode::Tunnel => {
                        if let Some(ti) = &cf.tunnel {
                            steps.push(Step::RemoveTunnelRoute {
                                api_token: cf.api_token.clone(),
                                account_id: ti.account_id.clone(),
                                tunnel_id: ti.tunnel_id.clone(),
                                zone_id: cf.zone_id.clone(),
                                domain: domain.clone(),
                            });
                        }
                    }
                    ExposureMode::Proxy | ExposureMode::DnsOnly => {
                        steps.push(Step::DeleteDnsRecord {
                            api_token: cf.api_token.clone(),
                            zone_id: cf.zone_id.clone(),
                            domain: domain.clone(),
                        });
                    }
                    ExposureMode::Public | ExposureMode::Local | ExposureMode::HostPort => {}
                }
            }
        }
    }

    // 3. Tear down orphaned users in the ryra group but not in config
    for username in &group_users {
        if handled_users.contains(username) {
            continue;
        }
        push_service_teardown(&mut steps, username, username);
    }

    // 4. Stop and remove cloudflared tunnel
    let has_tunnel = config
        .as_ref()
        .and_then(|c| c.cloudflare.as_ref())
        .and_then(|cf| cf.tunnel.as_ref())
        .is_some();
    if has_tunnel || PathBuf::from("/etc/containers/systemd/cloudflared.container").exists() {
        steps.push(Step::StopTunnel);
        steps.push(Step::RemoveFile(PathBuf::from(
            "/etc/containers/systemd/cloudflared.container",
        )));
    }

    // 5. Stop and remove nginx
    if PathBuf::from("/etc/containers/systemd/nginx.container").exists() {
        steps.push(Step::SystemStop {
            unit: "nginx".into(),
        });
        steps.push(Step::RemoveFile(PathBuf::from(
            "/etc/containers/systemd/nginx.container",
        )));
        steps.push(Step::SystemDaemonReload);
    }

    // 6. Remove system-level directories
    if PathBuf::from("/etc/ryra").exists() {
        steps.push(Step::RemoveDir(PathBuf::from("/etc/ryra")));
    }

    // 7. Remove the ryra group itself (after all users are deleted)
    if !group_users.is_empty() || config.is_some() {
        steps.push(Step::RemoveGroup);
    }

    ResetResult { steps }
}

/// Read members of the `ryra` system group from /etc/group.
/// Returns an empty vec if the group doesn't exist.
fn ryra_group_members() -> Vec<String> {
    let output = std::process::Command::new("getent")
        .args(["group", "ryra"])
        .output()
        .ok();

    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    // getent group ryra => "ryra:x:999:authentik,seafile,forgejo"
    let line = String::from_utf8_lossy(&output.stdout);
    let members = line.trim().split(':').nth(3).unwrap_or("");
    if members.is_empty() {
        return Vec::new();
    }
    members.split(',').map(|s| s.to_string()).collect()
}

/// Push the standard teardown steps for a single service user.
fn push_service_teardown(steps: &mut Vec<Step>, username: &str, service_name: &str) {
    steps.push(Step::StopService {
        username: username.to_string(),
        unit: service_name.to_string(),
    });
    steps.push(Step::DisableLinger {
        username: username.to_string(),
    });
    steps.push(Step::TerminateUserSession {
        username: username.to_string(),
    });
    steps.push(Step::KillUserProcesses {
        username: username.to_string(),
    });
    steps.push(Step::RemoveUser {
        username: username.to_string(),
    });
    steps.push(Step::RemoveFile(PathBuf::from(format!(
        "/etc/ryra/nginx/sites/{service_name}.conf"
    ))));
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
                is_web: reg_svc.def.nginx.is_some(),
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
    pub is_web: bool,
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
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let reg_service = registry::find_service(repo_dir, service_name)?;
    let def = &reg_service.def;
    let installed = config.services.iter().find(|s| s.name == service_name);

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
        installed_domain: installed.and_then(|s| s.domain.clone()),
        installed_exposure: installed.map(|s| s.exposure.clone()),
    })
}

pub struct ServiceDetail {
    pub name: String,
    pub description: String,
    pub url: Option<String>,
    pub has_sidecars: bool,
    pub ports: Vec<(u16, PortProtocol, String)>,
    pub env_vars: Vec<(String, Option<String>)>,
    pub installed_domain: Option<String>,
    pub installed_exposure: Option<ExposureMode>,
}
