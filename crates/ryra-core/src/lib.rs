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
use config::schema::{
    CloudflareCredentials, Config, ExposureMode, InstalledDeployMode, InstalledService,
};
use error::{Error, Result};
use generate::GeneratedFile;
use registry::service_def::DeployMode;
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
    /// Create a system user for a service.
    CreateUser { username: String, home_dir: PathBuf },
    /// Enable systemd linger so user services persist.
    EnableLinger { username: String },
    /// Disable systemd linger.
    DisableLinger { username: String },
    /// Terminate a user's systemd session (so userdel can succeed).
    TerminateUserSession { username: String },
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
    /// Pull images for a compose stack as a specific user.
    ComposePull {
        username: String,
        compose_dir: PathBuf,
    },
    /// Start a compose stack (`podman compose up -d`).
    ComposeUp {
        username: String,
        compose_dir: PathBuf,
    },
    /// Stop a compose stack (`podman compose down`).
    ComposeDown {
        username: String,
        compose_dir: PathBuf,
    },
}

impl Step {
    /// Render this step as a shell command (for dry-run display).
    pub fn to_command(&self) -> String {
        match self {
            Step::CreateUser { username, home_dir } => format!(
                "sudo useradd --system --shell $(which nologin) --home-dir {} --create-home {username}",
                home_dir.display()
            ),
            Step::EnableLinger { username } => format!("sudo loginctl enable-linger {username}"),
            Step::DisableLinger { username } => format!("sudo loginctl disable-linger {username}"),
            Step::TerminateUserSession { username } => {
                format!("sudo loginctl terminate-user {username}")
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
            Step::ComposePull {
                username,
                compose_dir,
            } => format!(
                "cd {} && sudo -H -u {username} podman compose pull",
                compose_dir.display()
            ),
            Step::ComposeUp {
                username,
                compose_dir,
            } => format!(
                "cd {} && sudo -H -u {username} podman compose up -d",
                compose_dir.display()
            ),
            Step::ComposeDown {
                username,
                compose_dir,
            } => format!(
                "cd {} && sudo -H -u {username} podman compose down",
                compose_dir.display()
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
    pub deploy_mode: InstalledDeployMode,
    pub repo_url: String,
    pub host_port: Option<u16>,
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in .env).
    pub generated_secrets: Vec<String>,
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

pub const DEFAULT_REPO: &str = "https://raw.githubusercontent.com/ryanravn/ryra/main/registry.json";

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
    domain: Option<&str>,
    exposure: ExposureMode,
    env_overrides: &BTreeMap<String, String>,
    compose_file_override: Option<&str>,
    repo_url: &str,
    repo_dir: &Path,
) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    if config.services.iter().any(|s| s.name == service_name) {
        return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
    }

    let reg_service = registry::find_service(repo_dir, service_name)?;

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

    let has_nginx = reg_service.def.nginx.is_some();
    let is_compose = reg_service.def.service.deploy.is_compose();
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

    let generated = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        domain,
        exposure: &exposure,
        host_port,
        quadlet_dir: &quadlet_dir,
        nginx_dir,
        env_overrides,
        service_dir: &reg_service.service_dir,
        compose_file_override,
    })?;

    // Generate warnings
    let mut warnings = Vec::new();

    if proxied && !reg_service.def.integrations.auth {
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

    // 0. Ensure nginx is set up (first proxied service triggers this)
    if proxied && !PathBuf::from("/etc/containers/systemd/nginx.container").exists() {
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

    // 3. Create service user
    steps.push(Step::CreateUser {
        username: username.clone(),
        home_dir: home_dir.clone(),
    });
    steps.push(Step::EnableLinger {
        username: username.clone(),
    });

    // 4-7: Deploy mode specific steps
    match generated {
        generate::GeneratedService::Quadlet {
            files,
            env_file,
            nginx_site,
        } => {
            // Pull main image
            if let DeployMode::Quadlet { ref image, .. } = reg_service.def.service.deploy {
                steps.push(Step::PullImage {
                    image: image.clone(),
                    username: Some(username.clone()),
                });
            }

            // Pull dependency images
            for dep in &reg_service.def.dependencies {
                steps.push(Step::PullImage {
                    image: dep.image.clone(),
                    username: Some(username.clone()),
                });
            }

            // Write quadlet + .env + nginx files
            for file in files {
                steps.push(Step::WriteFile(file));
            }
            steps.push(Step::WriteFile(env_file));
            if let Some(nginx_site) = nginx_site {
                steps.push(Step::WriteFile(nginx_site));
            }

            // Fix ownership, start via systemd
            steps.push(Step::Chown {
                path: home_dir,
                username: username.clone(),
            });
            steps.push(Step::DaemonReload {
                username: username.clone(),
            });

            // Dependencies start automatically via Requires=/After= in the quadlet
            steps.push(Step::StartService {
                username: username.clone(),
                unit: service_name.to_string(),
            });
        }
        generate::GeneratedService::Compose {
            compose_file,
            env_file,
            systemd_unit,
            nginx_site,
        } => {
            // Write compose file, .env, systemd unit
            steps.push(Step::WriteFile(compose_file));
            steps.push(Step::WriteFile(env_file));
            steps.push(Step::WriteFile(systemd_unit));
            if let Some(nginx_site) = nginx_site {
                steps.push(Step::WriteFile(nginx_site));
            }

            // Fix ownership, pull images, start compose stack
            steps.push(Step::Chown {
                path: home_dir.clone(),
                username: username.clone(),
            });
            steps.push(Step::ComposePull {
                username: username.clone(),
                compose_dir: home_dir,
            });
            steps.push(Step::DaemonReload {
                username: username.clone(),
            });
            steps.push(Step::StartService {
                username: username.clone(),
                unit: format!("{service_name}-compose"),
            });
        }
    }

    // Reload nginx if proxied
    if proxied {
        steps.push(Step::SystemRestart {
            unit: "nginx".into(),
        });
    }

    let deploy_mode = if is_compose {
        InstalledDeployMode::Compose
    } else {
        InstalledDeployMode::Quadlet
    };

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
        deploy_mode,
        repo_url: repo_url.to_string(),
        host_port,
        allocated_ports,
        generated_secrets,
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

    // Stop the service based on deploy mode
    let mut steps = Vec::new();
    match &service.deploy_mode {
        InstalledDeployMode::Quadlet => {
            steps.push(Step::StopService {
                username: username.clone(),
                unit: service_name.to_string(),
            });
        }
        InstalledDeployMode::Compose => {
            steps.push(Step::ComposeDown {
                username: username.clone(),
                compose_dir: service_home(service_name),
            });
        }
    }
    steps.push(Step::DisableLinger {
        username: username.clone(),
    });
    steps.push(Step::TerminateUserSession {
        username: username.clone(),
    });
    steps.push(Step::RemoveUser {
        username: username.clone(),
    });

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
    pub deploy_mode: InstalledDeployMode,
    pub repo: &'a str,
    pub host_port: Option<u16>,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
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
        deploy_mode: params.deploy_mode,
        repo: params.repo.to_string(),
        host_port: params.host_port,
        ports,
    });
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
    match &service.deploy_mode {
        InstalledDeployMode::Quadlet => {
            steps.push(Step::StopService {
                username: username.clone(),
                unit: service_name.to_string(),
            });
        }
        InstalledDeployMode::Compose => {
            steps.push(Step::ComposeDown {
                username: username.clone(),
                compose_dir: home_dir.clone(),
            });
        }
    }

    // 2. Regenerate all files from the current registry definition
    let generated = generate::generate_service(generate::GenerateServiceParams {
        config: &config,
        service_def: &reg_service.def,
        domain: service.domain.as_deref(),
        exposure: &service.exposure,
        host_port: service.host_port,
        quadlet_dir: &quadlet_dir,
        nginx_dir,
        env_overrides,
        service_dir: &reg_service.service_dir,
        compose_file_override: None,
    })?;

    // 3. Pull new image if it changed
    if let DeployMode::Quadlet { ref image, .. } = reg_service.def.service.deploy {
        steps.push(Step::PullImage {
            image: image.clone(),
            username: Some(username.clone()),
        });
    }
    for dep in &reg_service.def.dependencies {
        steps.push(Step::PullImage {
            image: dep.image.clone(),
            username: Some(username.clone()),
        });
    }

    // 4. Write files and restart
    match generated {
        generate::GeneratedService::Quadlet {
            files,
            env_file,
            nginx_site,
        } => {
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
        }
        generate::GeneratedService::Compose {
            compose_file,
            env_file,
            systemd_unit,
            nginx_site,
        } => {
            steps.push(Step::WriteFile(compose_file));
            steps.push(Step::WriteFile(env_file));
            steps.push(Step::WriteFile(systemd_unit));
            if let Some(nginx_site) = nginx_site {
                steps.push(Step::WriteFile(nginx_site));
            }

            steps.push(Step::Chown {
                path: home_dir.clone(),
                username: username.clone(),
            });
            steps.push(Step::ComposePull {
                username: username.clone(),
                compose_dir: home_dir,
            });
            steps.push(Step::DaemonReload {
                username: username.clone(),
            });
            steps.push(Step::StartService {
                username: username.clone(),
                unit: format!("{service_name}-compose"),
            });
        }
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
/// Always produces steps — discovers system artifacts even without config.
pub fn reset(system_ryra_users: &[String]) -> ResetResult {
    let config = ConfigPaths::resolve()
        .ok()
        .and_then(|p| config::load_config(&p.config_file).ok());

    let mut steps = Vec::new();

    // 1. Clean up services known from config (includes DNS/tunnel cleanup)
    if let Some(ref config) = config {
        for service in &config.services {
            let username = service_user(&service.name);
            push_service_teardown(&mut steps, &username, &service.name);

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

    // 2. Discover orphaned ryra-* users not tracked in config
    let known_users: Vec<String> = config
        .as_ref()
        .map(|c| c.services.iter().map(|s| service_user(&s.name)).collect())
        .unwrap_or_default();

    for username in system_ryra_users {
        if known_users.contains(username) {
            continue; // Already handled above
        }
        let service_name = username.as_str();
        push_service_teardown(&mut steps, username, service_name);
    }

    // 3. Stop and remove cloudflared tunnel
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

    // 4. Stop and remove nginx
    if PathBuf::from("/etc/containers/systemd/nginx.container").exists() {
        steps.push(Step::SystemStop {
            unit: "nginx".into(),
        });
        steps.push(Step::RemoveFile(PathBuf::from(
            "/etc/containers/systemd/nginx.container",
        )));
        steps.push(Step::SystemDaemonReload);
    }

    // 5. Remove system-level directories
    if PathBuf::from("/etc/ryra").exists() {
        steps.push(Step::RemoveDir(PathBuf::from("/etc/ryra")));
    }

    ResetResult { steps }
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
                is_compose: reg_svc.def.service.deploy.is_compose(),
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
    pub is_compose: bool,
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
        is_compose: def.service.deploy.is_compose(),
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
    pub is_compose: bool,
    pub ports: Vec<(u16, PortProtocol, String)>,
    pub env_vars: Vec<(String, Option<String>)>,
    pub installed_domain: Option<String>,
    pub installed_exposure: Option<ExposureMode>,
}
