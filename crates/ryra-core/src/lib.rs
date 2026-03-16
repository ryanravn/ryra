pub mod config;
pub mod error;
pub mod generate;
pub mod integrations;
pub mod registry;
pub mod system;
pub mod verbose;

use std::path::{Path, PathBuf};

use config::schema::{Config, InstalledService, RegistryEntry};
use config::state::State;
use config::ConfigPaths;
use error::{Error, Result};
use generate::GeneratedFile;

// --- Per-service user conventions ---

pub fn service_user(service_name: &str) -> String {
    format!("ryra-{service_name}")
}

pub fn service_home(service_name: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/ryra/{service_name}"))
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
    /// Remove a file (requires sudo).
    RemoveFile(PathBuf),
    /// Remove a Linux user and their home directory.
    RemoveUser { username: String },
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
                format!("cloudflare tunnel: add route {domain} -> https://localhost:443")
            }
            Step::RemoveTunnelRoute { domain, .. } => {
                format!("cloudflare tunnel: remove route for {domain}")
            }
            Step::RemoveFile(path) => format!("sudo rm -f {}", path.display()),
            Step::RemoveUser { username } => format!("sudo userdel --remove {username}"),
        }
    }
}

// --- Result types ---

pub struct InitResult {
    pub steps: Vec<Step>,
}

pub struct AddResult {
    pub steps: Vec<Step>,
    pub domain: String,
    pub username: String,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub username: String,
    pub service_name: String,
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

    let use_tunnel = config.tunnel.is_enabled();

    let nginx_exposure = match use_tunnel {
        true => generate::nginx::NginxExposure::LocalOnly,
        false => generate::nginx::NginxExposure::Public,
    };

    let mut steps = Vec::new();

    // Nginx config + quadlet
    steps.push(Step::WriteFile(GeneratedFile {
        path: PathBuf::from("/etc/ryra/nginx/sites/.keep"),
        content: String::new(),
    }));
    steps.push(Step::WriteFile(GeneratedFile {
        path: PathBuf::from("/etc/ryra/nginx/nginx.conf"),
        content: generate::nginx::render_nginx_base_conf(),
    }));
    steps.push(Step::WriteFile(GeneratedFile {
        path: PathBuf::from("/etc/containers/systemd/nginx.container"),
        content: generate::nginx::render_nginx_quadlet(&nginx_exposure),
    }));

    // Cloudflared quadlet (if tunnel configured)
    if let config::schema::TunnelConfig::Cloudflare { tunnel_token, .. } = &config.tunnel {
        steps.push(Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/containers/systemd/cloudflared.container"),
            content: generate::tunnel::render_cloudflared_quadlet(tunnel_token),
        }));
    }

    steps.push(Step::SystemDaemonReload);
    steps.push(Step::SystemStart {
        unit: "nginx".into(),
    });

    if use_tunnel {
        steps.push(Step::StartTunnel);
    }

    Ok(InitResult { steps })
}

/// Add a service: generate config, return steps to execute.
pub fn add_service(service_name: &str, domain: &str) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;
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

    let username = service_user(service_name);
    let home_dir = service_home(service_name);
    let quadlet_dir = service_quadlet_dir(service_name);
    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    let generated = generate::generate_service(
        &config,
        &mut state,
        &reg_service.def,
        domain,
        &quadlet_dir,
        nginx_dir,
    )?;

    // Save ryra's own state
    config::save_state(&paths.state_file, &state)?;
    config.services.push(InstalledService {
        name: service_name.to_string(),
        domain: domain.to_string(),
        version: "0.1.0".to_string(),
    });
    config::save_config(&paths.config_file, &config)?;

    // Build ordered steps
    let mut steps = Vec::new();

    // 1. Networking: tunnel route OR DNS record
    match &config.tunnel {
        config::schema::TunnelConfig::Cloudflare {
            tunnel_id,
            account_id,
            ..
        } => {
            // Tunnel handles routing — need CF credentials for CNAME + ingress
            if let Some((api_token, zone_id, _)) = config.dns.cloudflare_credentials() {
                steps.push(Step::AddTunnelRoute {
                    api_token: api_token.to_string(),
                    account_id: account_id.clone(),
                    tunnel_id: tunnel_id.clone(),
                    zone_id: zone_id.to_string(),
                    domain: domain.to_string(),
                });
            }
        }
        config::schema::TunnelConfig::None => {
            // No tunnel — create A record if Cloudflare DNS configured
            if let Some((api_token, zone_id, _)) = config.dns.cloudflare_credentials() {
                steps.push(Step::CreateDnsRecord {
                    api_token: api_token.to_string(),
                    zone_id: zone_id.to_string(),
                    domain: domain.to_string(),
                    proxied: config.dns.is_proxied(),
                });
            }
        }
    }

    // 2. SSL certificate — skip entirely when tunnel handles SSL
    match config.tunnel.is_enabled() {
        true => {} // Tunnel → Cloudflare edge handles SSL, no origin cert needed
        false => match &config.ssl {
            config::schema::SslConfig::Letsencrypt { email } => {
                let cf_token = config
                    .dns
                    .cloudflare_credentials()
                    .map(|(token, _, _)| token.to_string());
                steps.push(Step::ObtainCert {
                    domain: domain.to_string(),
                    email: email.clone(),
                    cloudflare_api_token: cf_token,
                });
            }
            config::schema::SslConfig::CloudflareOrigin => {
                steps.push(Step::GenerateOriginCert {
                    domain: domain.to_string(),
                });
            }
            config::schema::SslConfig::Custom { .. } => {
                // User manages certs — nothing to do
            }
        },
    }

    // 3. Create service user
    steps.push(Step::CreateUser {
        username: username.clone(),
        home_dir: home_dir.clone(),
    });
    steps.push(Step::EnableLinger {
        username: username.clone(),
    });

    // 4. Write all generated files (quadlets + nginx)
    for file in generated.quadlet_files {
        steps.push(Step::WriteFile(file));
    }
    if let Some(nginx_site) = generated.nginx_site {
        steps.push(Step::WriteFile(nginx_site));
    }

    // 5. Fix ownership after writing to the service user's home
    steps.push(Step::Chown {
        path: home_dir,
        username: username.clone(),
    });

    // 6. Start service + reload nginx
    steps.push(Step::DaemonReload {
        username: username.clone(),
    });
    steps.push(Step::StartService {
        username: username.clone(),
        unit: service_name.to_string(),
    });
    steps.push(Step::SystemRestart {
        unit: "nginx".into(),
    });

    Ok(AddResult {
        steps,
        domain: domain.to_string(),
        username,
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

    let mut steps = vec![
        Step::StopService {
            username: username.clone(),
            unit: service_name.to_string(),
        },
        Step::DisableLinger {
            username: username.clone(),
        },
        Step::TerminateUserSession {
            username: username.clone(),
        },
        Step::RemoveUser {
            username: username.clone(),
        },
        Step::RemoveFile(PathBuf::from(format!(
            "/etc/ryra/nginx/sites/{service_name}.conf"
        ))),
        Step::SystemRestart {
            unit: "nginx".into(),
        },
    ];

    // Clean up networking: tunnel route or DNS record
    match &config.tunnel {
        config::schema::TunnelConfig::Cloudflare {
            tunnel_id,
            account_id,
            ..
        } => {
            if let Some((api_token, zone_id, _)) = config.dns.cloudflare_credentials() {
                steps.push(Step::RemoveTunnelRoute {
                    api_token: api_token.to_string(),
                    account_id: account_id.clone(),
                    tunnel_id: tunnel_id.clone(),
                    zone_id: zone_id.to_string(),
                    domain,
                });
            }
        }
        config::schema::TunnelConfig::None => {
            if let Some((api_token, zone_id, _)) = config.dns.cloudflare_credentials() {
                steps.push(Step::DeleteDnsRecord {
                    api_token: api_token.to_string(),
                    zone_id: zone_id.to_string(),
                    domain,
                });
            }
        }
    }

    Ok(RemoveResult {
        steps,
        username,
        service_name: service_name.to_string(),
    })
}

/// Called after remove steps succeed — cleans up ryra's internal state.
pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;
    let mut state = config::load_state(&paths.state_file)?;

    system::port::deallocate_ports(&mut state, service_name);
    system::secret::remove_secrets(&mut state, service_name);
    config::save_state(&paths.state_file, &state)?;
    config.services.retain(|s| s.name != service_name);
    config::save_config(&paths.config_file, &config)?;

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
        config::status::StatusInfo::from_config_and_state(&config, &state),
    )
}

/// List services: what's available vs installed.
pub fn list_services() -> Result<Vec<ServiceStatus>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();

    let available = registry::list_available(&paths.cache_dir, &reg_pairs)?;

    let statuses = available
        .into_iter()
        .map(|reg_svc| {
            let name = &reg_svc.def.service.name;
            let installed = config.services.iter().find(|s| s.name == *name);
            match installed {
                Some(inst) => ServiceStatus::Installed {
                    name: name.clone(),
                    description: reg_svc.def.service.description,
                    domain: inst.domain.clone(),
                },
                None => ServiceStatus::Available {
                    name: name.clone(),
                    description: reg_svc.def.service.description,
                },
            }
        })
        .collect();

    Ok(statuses)
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

#[derive(Debug)]
pub enum ServiceStatus {
    Available {
        name: String,
        description: String,
    },
    Installed {
        name: String,
        description: String,
        domain: String,
    },
}
