pub mod blueprint;
pub mod context;
pub mod nginx;
pub mod quadlet;
pub mod template;
pub mod tunnel;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::schema::{Config, ExposureMode};
use crate::error::{Error, Result};
use crate::registry::service_def::{DeployMode, EnvVar, ServiceDef};

/// Everything generated for a service, ready to be written to disk.
pub enum GeneratedService {
    Quadlet {
        files: Vec<GeneratedFile>,
        env_file: GeneratedFile,
        nginx_site: Option<GeneratedFile>,
    },
    Compose {
        compose_file: GeneratedFile,
        env_file: GeneratedFile,
        systemd_unit: GeneratedFile,
        nginx_site: Option<GeneratedFile>,
    },
}

pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Everything generated for a service, including files that belong to other services.
pub struct GenerationOutput {
    pub service: GeneratedService,
    /// Files that belong to other services' directories (e.g., auth blueprints).
    pub cross_service_files: Vec<GeneratedFile>,
}

/// Parameters for [`generate_service`].
pub struct GenerateServiceParams<'a> {
    pub config: &'a Config,
    pub service_def: &'a ServiceDef,
    pub domain: Option<&'a str>,
    pub exposure: &'a ExposureMode,
    pub host_port: Option<u16>,
    pub quadlet_dir: &'a Path,
    pub nginx_dir: &'a Path,
    pub env_overrides: &'a BTreeMap<String, String>,
    pub service_dir: &'a Path,
    pub compose_file_override: Option<&'a str>,
}

/// Generate all files for a service based on its deploy mode.
/// `host_port` is the allocated port for web services (None for non-web).
pub fn generate_service(params: GenerateServiceParams<'_>) -> Result<GenerationOutput> {
    let name = &params.service_def.service.name;

    // Build template context (generates fresh secrets based on each env var's format + length)
    let ctx = context::build_context(
        params.config,
        params.service_def,
        params.domain.unwrap_or_default(),
    );
    let rendered_env = render_env_vars(params.service_def, &ctx, params.env_overrides)?;

    // Build .env file content
    let home_dir = crate::service_home(name);
    let env_file = build_env_file(
        &home_dir,
        &rendered_env,
        params.service_def,
        params.host_port,
    );

    // Nginx site config
    let nginx_site = generate_nginx_site(
        params.config,
        params.service_def,
        name,
        params.domain,
        params.exposure,
        params.host_port,
        params.nginx_dir,
    )?;

    let service = match &params.service_def.service.deploy {
        DeployMode::Quadlet { image, command } => generate_quadlet(GenerateQuadletParams {
            name,
            image,
            command: command.as_deref(),
            service_def: params.service_def,
            exposure: params.exposure,
            host_port: params.host_port,
            quadlet_dir: params.quadlet_dir,
            env_file,
            nginx_site,
        }),
        DeployMode::Compose { file, .. } => {
            let compose_filename = params.compose_file_override.unwrap_or(file);
            generate_compose(
                name,
                params.service_dir,
                compose_filename,
                params.quadlet_dir,
                env_file,
                nginx_site,
            )
        }
    }?;

    // Generate auth blueprint if this service uses auth integration
    let mut cross_service_files = Vec::new();
    if params.service_def.integrations.auth {
        if let (Some(client_id), Some(client_secret)) = (
            ctx.get("auth.client_id"),
            ctx.get("auth.client_secret"),
        ) {
            let domain = params.domain.unwrap_or("localhost");
            cross_service_files.push(blueprint::generate_authentik_blueprint(
                name, domain, client_id, client_secret,
            ));
        }
    }

    Ok(GenerationOutput {
        service,
        cross_service_files,
    })
}

/// Build the .env file for a service (used by both quadlet and compose).
fn build_env_file(
    home_dir: &Path,
    rendered_env: &[EnvVar],
    service_def: &ServiceDef,
    host_port: Option<u16>,
) -> GeneratedFile {
    let mut lines = Vec::new();

    for env in rendered_env {
        lines.push(format!("{}={}", env.name, env.value));
    }

    // Expose port as RYRA_PORT_* for compose files
    for port_def in &service_def.ports {
        let port = host_port.unwrap_or(port_def.container_port);
        let var_name = format!("RYRA_PORT_{}", port_def.name.to_uppercase());
        lines.push(format!("{var_name}={port}"));
    }

    GeneratedFile {
        path: home_dir.join(".env"),
        content: lines.join("\n") + "\n",
    }
}

/// Parameters for [`generate_quadlet`].
struct GenerateQuadletParams<'a> {
    name: &'a str,
    image: &'a str,
    command: Option<&'a str>,
    service_def: &'a ServiceDef,
    exposure: &'a ExposureMode,
    host_port: Option<u16>,
    quadlet_dir: &'a Path,
    env_file: GeneratedFile,
    nginx_site: Option<GeneratedFile>,
}

/// Generate quadlet files for a single-container service.
fn generate_quadlet(params: GenerateQuadletParams<'_>) -> Result<GeneratedService> {
    let name = params.name;
    let mut files = Vec::new();

    let port_mappings: Vec<quadlet::PortMapping> = params
        .service_def
        .ports
        .iter()
        .map(|p| quadlet::PortMapping {
            host_port: params.host_port.unwrap_or(p.container_port),
            container_port: p.container_port,
            protocol: p.protocol.clone(),
        })
        .collect();

    let bind_address = match params.exposure {
        ExposureMode::HostPort => quadlet::BindAddress::Any,
        _ => quadlet::BindAddress::Localhost,
    };

    // Network
    let network_name = name.to_string();
    files.push(GeneratedFile {
        path: params.quadlet_dir.join(format!("{name}.network")),
        content: quadlet::render_network(&network_name),
    });

    // Volumes
    for vol in &params.service_def.volumes {
        let vol_name = format!("{name}-{}", vol.name);
        files.push(GeneratedFile {
            path: params.quadlet_dir.join(format!("{vol_name}.volume")),
            content: quadlet::render_volume(&vol_name),
        });
    }

    let owned_volume_mappings: Vec<(String, String)> = params
        .service_def
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
        image: params.image,
        ports: &port_mappings,
        volumes: &volume_refs,
        network: &network_name,
        command: params.command,
        bind_address: &bind_address,
    };

    files.push(GeneratedFile {
        path: params.quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&container_params),
    });

    Ok(GeneratedService::Quadlet {
        files,
        env_file: params.env_file,
        nginx_site: params.nginx_site,
    })
}

/// Generate compose files + .env for a multi-container stack.
fn generate_compose(
    name: &str,
    service_dir: &Path,
    compose_filename: &str,
    _quadlet_dir: &Path,
    env_file: GeneratedFile,
    nginx_site: Option<GeneratedFile>,
) -> Result<GeneratedService> {
    let compose_src = service_dir.join(compose_filename);
    let compose_content =
        std::fs::read_to_string(&compose_src).map_err(|source| Error::FileRead {
            path: compose_src,
            source,
        })?;

    let home_dir = crate::service_home(name);
    let username = crate::service_user(name);

    let compose_file = GeneratedFile {
        path: home_dir.join("docker-compose.yml"),
        content: compose_content,
    };

    // Compose units are regular systemd .service files — they go in
    // ~/.config/systemd/user/, not the quadlet directory (which only
    // handles .container, .volume, .network, etc.).
    let systemd_user_dir = home_dir.join(".config").join("systemd").join("user");
    let systemd_unit = GeneratedFile {
        path: systemd_user_dir.join(format!("{name}-compose.service")),
        content: render_compose_unit(name, &username, &home_dir),
    };

    Ok(GeneratedService::Compose {
        compose_file,
        env_file,
        systemd_unit,
        nginx_site,
    })
}

fn render_compose_unit(name: &str, _username: &str, home_dir: &Path) -> String {
    let dir = home_dir.display();
    format!(
        "[Unit]\n\
         Description={name}\n\
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
                kind: Default::default(),
                prompt: None,
                format: Default::default(),
                length: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if service_def.integrations.smtp {
        for (env_name, value_template) in &service_def.mappings.smtp {
            let value = template::render(value_template, ctx)?;
            if !value.is_empty() {
                rendered.push(EnvVar {
                    name: env_name.clone(),
                    value,
                    kind: Default::default(),
                    prompt: None,
                    format: Default::default(),
                    length: None,
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
                    kind: Default::default(),
                    prompt: None,
                    format: Default::default(),
                    length: None,
                });
            }
        }
    }

    Ok(rendered)
}

pub fn generate_nginx_site(
    config: &Config,
    service_def: &ServiceDef,
    name: &str,
    domain: Option<&str>,
    exposure: &ExposureMode,
    host_port: Option<u16>,
    nginx_dir: &Path,
) -> Result<Option<GeneratedFile>> {
    match (&service_def.nginx, domain, host_port) {
        (Some(_nginx_def), Some(domain), Some(upstream_port)) => {
            let mode = match exposure {
                ExposureMode::Tunnel | ExposureMode::Local => nginx::SiteMode::HttpOnly,
                ExposureMode::Proxy => {
                    let (cert_path, key_path) = crate::integrations::ssl::origin_cert_paths(domain);
                    nginx::SiteMode::Ssl {
                        cert_path,
                        key_path,
                    }
                }
                ExposureMode::DnsOnly | ExposureMode::Public => {
                    let (cert_path, key_path) = match &config.ssl {
                        Some(ssl) => crate::integrations::ssl::cert_paths_for_ssl(ssl, domain),
                        None => crate::integrations::ssl::letsencrypt_cert_paths(domain),
                    };
                    nginx::SiteMode::Ssl {
                        cert_path,
                        key_path,
                    }
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

pub fn extract_secret_refs(value: &str) -> Vec<String> {
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
