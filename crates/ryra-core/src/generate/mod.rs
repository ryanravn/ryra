pub mod context;
pub mod nginx;
pub mod quadlet;
pub mod template;
pub mod tunnel;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::schema::{Config, ExposureMode};
use crate::config::state::State;
use crate::error::{Error, Result};
use crate::registry::service_def::{DeployMode, EnvVar, ServiceDef};
use crate::system::{port, secret};

/// Everything generated for a service, ready to be written to disk.
pub enum GeneratedService {
    Quadlet {
        files: Vec<GeneratedFile>,
        nginx_site: Option<GeneratedFile>,
    },
    Compose {
        compose_file: GeneratedFile,
        env_file: GeneratedFile,
        /// Systemd unit to run `podman compose up` on boot.
        systemd_unit: GeneratedFile,
        nginx_site: Option<GeneratedFile>,
    },
}

pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Generate all files for a service based on its deploy mode.
pub fn generate_service(
    config: &Config,
    state: &mut State,
    service_def: &ServiceDef,
    domain: Option<&str>,
    exposure: &ExposureMode,
    quadlet_dir: &Path,
    nginx_dir: &Path,
    env_overrides: &BTreeMap<String, String>,
    service_dir: &Path,
    compose_file_override: Option<&str>,
) -> Result<GeneratedService> {
    let name = &service_def.service.name;

    // Common: allocate ports
    for port_def in &service_def.ports {
        port::allocate_port(state, name, &port_def.name)?;
    }

    // Common: generate secrets (skip overridden env vars)
    for env in &service_def.env {
        if !env_overrides.contains_key(&env.name) {
            extract_secret_refs(&env.value)
                .into_iter()
                .for_each(|secret_name| {
                    secret::ensure_secret(state, name, &secret_name);
                });
        }
    }

    // Common: build template context and render env vars
    let ctx = context::build_context(config, state, service_def, domain.unwrap_or_default());
    let rendered_env = render_env_vars(service_def, &ctx, env_overrides)?;

    // Common: nginx site config
    let nginx_site = generate_nginx_site(config, state, service_def, name, domain, exposure, nginx_dir)?;

    match &service_def.service.deploy {
        DeployMode::Quadlet { image } => {
            generate_quadlet(name, image, service_def, state, &rendered_env, exposure, quadlet_dir, nginx_site)
        }
        DeployMode::Compose { file, .. } => {
            let compose_filename = compose_file_override.unwrap_or(file);
            let home_dir = crate::service_home(name);
            generate_compose(name, service_def, state, &rendered_env, &home_dir, service_dir, compose_filename, quadlet_dir, nginx_site)
        }
    }
}

/// Generate quadlet files for a single-container service.
fn generate_quadlet(
    name: &str,
    image: &str,
    service_def: &ServiceDef,
    state: &State,
    rendered_env: &[EnvVar],
    exposure: &ExposureMode,
    quadlet_dir: &Path,
    nginx_site: Option<GeneratedFile>,
) -> Result<GeneratedService> {
    let mut files = Vec::new();

    let port_mappings: Vec<quadlet::PortMapping> = service_def
        .ports
        .iter()
        .filter_map(|p| {
            port::get_port(state, name, &p.name).map(|host_port| quadlet::PortMapping {
                host_port,
                container_port: p.container_port,
                protocol: p.protocol.clone(),
            })
        })
        .collect();

    let bind_address = match exposure {
        ExposureMode::HostPort => quadlet::BindAddress::Any,
        _ => quadlet::BindAddress::Localhost,
    };

    // Network
    let network_name = name.to_string();
    files.push(GeneratedFile {
        path: quadlet_dir.join(format!("{name}.network")),
        content: quadlet::render_network(&network_name),
    });

    // Volumes
    for vol in &service_def.volumes {
        let vol_name = format!("{name}-{}", vol.name);
        files.push(GeneratedFile {
            path: quadlet_dir.join(format!("{vol_name}.volume")),
            content: quadlet::render_volume(&vol_name),
        });
    }

    let owned_volume_mappings: Vec<(String, String)> = service_def
        .volumes
        .iter()
        .map(|v| (format!("{name}-{}", v.name), v.mount_path.clone()))
        .collect();

    let volume_refs: Vec<quadlet::VolumeMapping> = owned_volume_mappings
        .iter()
        .map(|(name, path)| quadlet::VolumeMapping {
            volume_name: name,
            mount_path: path,
        })
        .collect();

    // Container
    let container_params = quadlet::QuadletParams {
        service_name: name,
        image,
        env_vars: rendered_env,
        ports: &port_mappings,
        volumes: &volume_refs,
        network: &network_name,
        command: None,
        bind_address: &bind_address,
    };

    files.push(GeneratedFile {
        path: quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&container_params),
    });

    Ok(GeneratedService::Quadlet { files, nginx_site })
}

/// Generate compose files + .env for a multi-container stack.
fn generate_compose(
    name: &str,
    service_def: &ServiceDef,
    state: &State,
    rendered_env: &[EnvVar],
    home_dir: &Path,
    service_dir: &Path,
    compose_filename: &str,
    quadlet_dir: &Path,
    nginx_site: Option<GeneratedFile>,
) -> Result<GeneratedService> {
    // Read the compose file from the registry
    let compose_src = service_dir.join(compose_filename);
    let compose_content = std::fs::read_to_string(&compose_src).map_err(|source| Error::FileRead {
        path: compose_src,
        source,
    })?;

    // Build .env file: rendered env vars + port allocations
    let mut env_lines = Vec::new();
    env_lines.push("# Generated by ryra — do not edit manually".to_string());

    for env in rendered_env {
        env_lines.push(format!("{}={}", env.name, env.value));
    }

    // Expose allocated ports as RYRA_PORT_* so compose files can reference them
    for port_def in &service_def.ports {
        if let Some(host_port) = port::get_port(state, name, &port_def.name) {
            let var_name = format!("RYRA_PORT_{}", port_def.name.to_uppercase());
            env_lines.push(format!("{var_name}={host_port}"));
        }
    }

    let compose_file = GeneratedFile {
        path: home_dir.join("docker-compose.yml"),
        content: compose_content,
    };

    let env_file = GeneratedFile {
        path: home_dir.join(".env"),
        content: env_lines.join("\n") + "\n",
    };

    // Systemd unit to manage the compose stack lifecycle
    let username = crate::service_user(name);
    let systemd_unit = GeneratedFile {
        path: quadlet_dir.join(format!("{name}-compose.service")),
        content: render_compose_unit(name, &username, home_dir),
    };

    Ok(GeneratedService::Compose {
        compose_file,
        env_file,
        systemd_unit,
        nginx_site,
    })
}

/// Render a systemd user unit that manages a compose stack.
fn render_compose_unit(name: &str, _username: &str, home_dir: &Path) -> String {
    let dir = home_dir.display();
    format!(
        "[Unit]\n\
         Description=Ryra compose service: {name}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         WorkingDirectory={dir}\n\
         ExecStart=/usr/bin/podman compose up -d\n\
         ExecStop=/usr/bin/podman compose down\n\
         Restart=no\n\
         TimeoutStartSec=300\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

// --- Shared helpers ---

/// Render env vars with template substitution and user overrides.
fn render_env_vars(
    service_def: &ServiceDef,
    ctx: &BTreeMap<String, String>,
    env_overrides: &BTreeMap<String, String>,
) -> Result<Vec<EnvVar>> {
    let mut rendered: Vec<EnvVar> = service_def
        .env
        .iter()
        .map(|env| {
            let value = match env_overrides.get(&env.name) {
                Some(override_value) => override_value.clone(),
                None => template::render(&env.value, ctx)?,
            };
            Ok(EnvVar {
                name: env.name.clone(),
                value,
                prompt: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Integration mappings
    if service_def.integrations.smtp {
        for (env_name, value_template) in &service_def.mappings.smtp {
            let value = template::render(value_template, ctx)?;
            if !value.is_empty() {
                rendered.push(EnvVar {
                    name: env_name.clone(),
                    value,
                    prompt: None,
                });
            }
        }
    }
    if service_def.integrations.auth {
        for (env_name, value_template) in &service_def.mappings.auth {
            let value = template::render(value_template, ctx)?;
            if !value.is_empty() {
                rendered.push(EnvVar {
                    name: env_name.clone(),
                    value,
                    prompt: None,
                });
            }
        }
    }

    Ok(rendered)
}

/// Generate nginx site config if the service is a web service with a domain.
fn generate_nginx_site(
    config: &Config,
    state: &State,
    service_def: &ServiceDef,
    name: &str,
    domain: Option<&str>,
    exposure: &ExposureMode,
    nginx_dir: &Path,
) -> Result<Option<GeneratedFile>> {
    match (&service_def.nginx, domain) {
        (Some(nginx_def), Some(domain)) => {
            let upstream_port = port::get_port(state, name, &nginx_def.upstream_port)
                .ok_or_else(|| Error::Template(format!(
                    "upstream port '{}' not allocated for service '{name}'",
                    nginx_def.upstream_port
                )))?;

            let mode = match exposure {
                ExposureMode::Tunnel | ExposureMode::Local => nginx::SiteMode::HttpOnly,
                ExposureMode::Proxy => {
                    let (cert_path, key_path) =
                        crate::integrations::ssl::origin_cert_paths(domain);
                    nginx::SiteMode::Ssl { cert_path, key_path }
                }
                ExposureMode::DnsOnly => {
                    let (cert_path, key_path) = match &config.ssl {
                        Some(ssl) => crate::integrations::ssl::cert_paths_for_ssl(ssl, domain),
                        None => crate::integrations::ssl::letsencrypt_cert_paths(domain),
                    };
                    nginx::SiteMode::Ssl { cert_path, key_path }
                }
                ExposureMode::HostPort => nginx::SiteMode::HttpOnly,
            };

            Ok(Some(GeneratedFile {
                path: nginx_dir.join(format!("{name}.conf")),
                content: nginx::render_site(&nginx::NginxSiteParams {
                    service_name: name,
                    domain,
                    upstream_port,
                    mode,
                }),
            }))
        }
        _ => Ok(None),
    }
}

/// Extract secret names from template strings like "{{secret.db_password}}".
fn extract_secret_refs(value: &str) -> Vec<String> {
    let mut secrets = Vec::new();
    let mut rest = value;
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
}
