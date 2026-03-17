pub mod config;
pub mod error;
pub mod generate;
pub mod integrations;
pub mod registry;
pub mod system;
pub mod verbose;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::schema::{CloudflareCredentials, Config, ExposureMode, InstalledDeployMode, InstalledService, RegistryEntry};
use registry::service_def::DeployMode;
use config::state::State;
use config::ConfigPaths;
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
    /// Create a system user for a service.
    CreateUser {
        username: String,
        home_dir: PathBuf,
    },
    /// Enable systemd linger so user services persist.
    EnableLinger { username: String },
    /// Disable systemd linger.
    DisableLinger { username: String },
    /// Terminate a user's systemd session (so userdel can succeed).
    TerminateUserSession { username: String },
    /// Write a file (requires sudo — goes to service user home or /etc).
    WriteFile(GeneratedFile),
    /// Set ownership of a path to a user.
    Chown {
        path: PathBuf,
        username: String,
    },
    /// Reload systemd for a service user.
    DaemonReload { username: String },
    /// Start a service under a user's systemd.
    StartService {
        username: String,
        unit: String,
    },
    /// Stop a service under a user's systemd.
    StopService {
        username: String,
        unit: String,
    },
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
    GenerateOriginCert {
        domain: String,
    },
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
                "sudo useradd --system --shell /usr/sbin/nologin --home-dir {} --create-home {username}",
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
            Step::ComposePull { username, compose_dir } => format!(
                "cd {} && sudo -H -u {username} podman compose pull",
                compose_dir.display()
            ),
            Step::ComposeUp { username, compose_dir } => format!(
                "cd {} && sudo -H -u {username} podman compose up -d",
                compose_dir.display()
            ),
            Step::ComposeDown { username, compose_dir } => format!(
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
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in state.toml).
    pub generated_secrets: Vec<String>,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub username: String,
    pub service_name: String,
    pub domain: Option<String>,
    pub exposure: ExposureMode,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

/// Initialize a new ryra project.
pub async fn init(config: Config) -> Result<InitResult> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;

    config::save_config(&paths.config_file, &config)?;
    config::save_state(&paths.state_file, &State::default())?;

    // Fetch registries
    for reg in &config.registries {
        let source_path = Path::new(&reg.url);
        if source_path.exists() && source_path.is_dir() {
            registry::fetch::add_local_registry(source_path, &paths.cache_dir, &reg.name)?;
        } else {
            registry::fetch::fetch_registry(&reg.url, &paths.cache_dir, &reg.name).await?;
        }
    }

    // Create dirs and write nginx config + quadlet
    let mut steps = vec![
        Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/sites/.keep"),
            content: String::new(),
        }),
        Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/certs/.keep"),
            content: String::new(),
        }),
        Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/ryra/nginx/nginx.conf"),
            content: generate::nginx::render_nginx_base_conf(),
        }),
        Step::PullImage {
            image: "docker.io/library/nginx:alpine".into(),
            username: None,
        },
        Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/containers/systemd/nginx.container"),
            content: generate::nginx::render_nginx_quadlet(),
        }),
    ];

    // Cloudflared quadlet (if tunnel configured)
    if let Some(CloudflareCredentials { tunnel: Some(ref ti), .. }) = config.cloudflare {
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/containers/systemd/cloudflared.container"),
            content: generate::tunnel::render_cloudflared_quadlet(&ti.tunnel_token),
        }));
    }

    steps.push(Step::SystemDaemonReload);
    steps.push(Step::SystemStart {
        unit: "nginx".into(),
    });

    if config.cloudflare.as_ref().and_then(|cf| cf.tunnel.as_ref()).is_some() {
        steps.push(Step::StartTunnel);
    }

    Ok(InitResult { steps })
}

/// Add a service: generate config, return steps to execute.
/// `env_overrides` contains user-provided values for env vars with `prompt` set.
/// `compose_file_override` selects a specific compose profile file.
pub fn add_service(
    service_name: &str,
    domain: Option<&str>,
    exposure: ExposureMode,
    env_overrides: &BTreeMap<String, String>,
    compose_file_override: Option<&str>,
) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;
    let mut state = config::load_state(&paths.state_file)?;

    if config.services.iter().any(|s| s.name == service_name) {
        return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
    }

    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();
    let reg_service = registry::find_service(&paths.cache_dir, &reg_pairs, service_name)?;

    let is_web = reg_service.def.nginx.is_some();
    let is_compose = reg_service.def.service.deploy.is_compose();

    // Validate exposure vs service type
    if is_web && exposure == ExposureMode::HostPort {
        return Err(Error::InvalidExposure(
            "web services cannot use host-port exposure (they require nginx)".to_string(),
        ));
    }
    if !is_web && exposure.is_web_only() {
        return Err(Error::InvalidExposure(format!(
            "non-web services cannot use {} exposure (no nginx config)",
            exposure.label()
        )));
    }

    let username = service_user(service_name);
    let home_dir = service_home(service_name);
    let quadlet_dir = service_quadlet_dir(service_name);
    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    let generated = generate::generate_service(
        &config,
        &mut state,
        &reg_service.def,
        domain,
        &exposure,
        &quadlet_dir,
        nginx_dir,
        env_overrides,
        &reg_service.service_dir,
        compose_file_override,
    )?;

    // Save port/secret allocations (needed even if steps fail, so ports aren't reused)
    config::save_state(&paths.state_file, &state)?;

    // Generate warnings
    let mut warnings = Vec::new();

    if is_web
        && !reg_service.def.integrations.auth
        && matches!(
            exposure,
            ExposureMode::Tunnel | ExposureMode::Proxy | ExposureMode::DnsOnly
        )
    {
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
            .filter_map(|p| {
                system::port::get_port(&state, service_name, &p.name)
                    .map(|hp| (hp, p.protocol.clone()))
            })
            .collect();
        warnings.push(Warning::HostPortExposure {
            service_name: service_name.to_string(),
            ports,
        });
    }

    // Build ordered steps
    let mut steps = Vec::new();

    // 1. Networking: based on exposure mode (web services only)
    if is_web {
        match (&exposure, domain) {
            (ExposureMode::Tunnel, Some(domain)) => {
                if let Some(cf) = &config.cloudflare {
                    if let Some(ti) = &cf.tunnel {
                        steps.push(Step::AddTunnelRoute {
                            api_token: cf.api_token.clone(),
                            account_id: ti.account_id.clone(),
                            tunnel_id: ti.tunnel_id.clone(),
                            zone_id: cf.zone_id.clone(),
                            domain: domain.to_string(),
                        });
                    }
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
                    let email = match &config.ssl {
                        Some(config::schema::SslConfig::Letsencrypt { email }) => email.clone(),
                        _ => return Err(Error::Template(
                            "DnsOnly exposure requires SSL config with Let's Encrypt email"
                                .to_string(),
                        )),
                    };
                    let cf_token = config.cloudflare.as_ref().map(|cf| cf.api_token.clone());
                    steps.push(Step::ObtainCert {
                        domain: domain.to_string(),
                        email,
                        cloudflare_api_token: cf_token,
                    });
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
        generate::GeneratedService::Quadlet { files, env_file, nginx_site } => {
            // Pull image
            if let DeployMode::Quadlet { ref image } = reg_service.def.service.deploy {
                steps.push(Step::PullImage {
                    image: image.clone(),
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

    // Reload nginx if web service
    if is_web {
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
    let allocated_ports: Vec<(String, u16)> = state
        .allocated
        .iter()
        .filter(|a| a.service == service_name)
        .map(|a| (a.port_name.clone(), a.host_port))
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
    let is_web = domain.is_some();

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

    // Only clean up nginx for web services
    if is_web {
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
            ExposureMode::Local | ExposureMode::HostPort => {}
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

/// Called after add steps succeed — records the service in config.
pub fn finalize_add(
    service_name: &str,
    domain: Option<&str>,
    exposure: ExposureMode,
    deploy_mode: InstalledDeployMode,
) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;

    config.services.push(InstalledService {
        name: service_name.to_string(),
        domain: domain.map(|d| d.to_string()),
        version: "0.1.0".to_string(),
        exposure,
        deploy_mode,
    });
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;
    let mut state = config::load_state(&paths.state_file)?;

    system::port::deallocate_ports(&mut state, service_name);
    config::save_state(&paths.state_file, &state)?;
    config.services.retain(|s| s.name != service_name);
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
                    ExposureMode::Local | ExposureMode::HostPort => {}
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
        Err(_) => return config::status::RyraStatus::NotInitialized,
    };

    let state = config::load_state(&paths.state_file).unwrap_or_default();

    config::status::RyraStatus::Initialized(
        config::status::StatusInfo::from_config_and_state(paths.config_file, &config, &state),
    )
}

/// List installed services.
pub fn list_installed() -> Result<Vec<InstalledService>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;
    Ok(config.services)
}

/// Search available services in registries, optionally filtered by query.
pub fn search_services(query: Option<&str>) -> Result<Vec<SearchResult>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();

    let available = registry::list_available(&paths.cache_dir, &reg_pairs)?;

    let results = available
        .into_iter()
        .filter(|reg_svc| {
            match query {
                None => true,
                Some(q) => {
                    let q = q.to_lowercase();
                    reg_svc.def.service.name.to_lowercase().contains(&q)
                        || reg_svc.def.service.description.to_lowercase().contains(&q)
                }
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

/// Get detailed info about a service from the registry.
pub fn service_info(service_name: &str) -> Result<ServiceDetail> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();

    let reg_service = registry::find_service(&paths.cache_dir, &reg_pairs, service_name)?;
    let def = &reg_service.def;
    let installed = config.services.iter().find(|s| s.name == service_name);

    Ok(ServiceDetail {
        name: def.service.name.clone(),
        description: def.service.description.clone(),
        url: def.service.url.clone(),
        is_compose: def.service.deploy.is_compose(),
        ports: def.ports.iter().map(|p| (p.container_port, p.protocol.clone(), p.name.clone())).collect(),
        env_vars: def.env.iter().map(|e| (e.name.clone(), e.prompt.clone())).collect(),
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

/// Add a registry to the config and fetch it.
pub async fn add_registry(name: &str, url: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;

    if config.registries.iter().any(|r| r.name == name) {
        return Err(Error::RegistryAlreadyExists {
            name: name.to_string(),
        });
    }

    let source_path = Path::new(url);
    if source_path.exists() && source_path.is_dir() {
        registry::fetch::add_local_registry(source_path, &paths.cache_dir, name)?;
    } else {
        registry::fetch::fetch_registry(url, &paths.cache_dir, name).await?;
    }

    config.registries.push(RegistryEntry {
        name: name.to_string(),
        url: url.to_string(),
    });
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

