pub mod context;
pub mod nginx;
pub mod quadlet;
pub mod template;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::schema::{Config, ExposureMode};
use crate::error::Result;
use crate::registry::service_def::{AuthKind, EnvVar, ServiceDef};

/// Everything generated for a service, ready to be written to disk.
pub struct GeneratedService {
    pub files: Vec<GeneratedFile>,
    pub env_file: GeneratedFile,
    pub nginx_site: Option<GeneratedFile>,
}

pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Everything generated for a service.
pub struct GenerationOutput {
    pub service: GeneratedService,
    /// The template context used during generation (for auth registration, etc.).
    pub ctx: std::collections::BTreeMap<String, String>,
}

/// Parameters for [`generate_service`].
pub struct GenerateServiceParams<'a> {
    pub config: &'a Config,
    pub service_def: &'a ServiceDef,
    pub domain: Option<&'a str>,
    pub exposure: &'a ExposureMode,
    /// The auth kind the user chose to enable, if any.
    pub auth_kind: Option<&'a AuthKind>,
    pub host_port: Option<u16>,
    pub quadlet_dir: &'a Path,
    pub nginx_dir: &'a Path,
    pub env_overrides: &'a BTreeMap<String, String>,
    pub service_dir: &'a Path,
}

/// Generate all files for a service based on its deploy mode.
/// `host_port` is the allocated port for web services (None for non-web).
pub fn generate_service(params: GenerateServiceParams<'_>) -> Result<GenerationOutput> {
    let name = &params.service_def.service.name;

    // Build template context (generates fresh secrets based on each env var's format + length)
    // Local mode has no domain — use <service>.localhost as a valid FQDN fallback.
    let fallback_domain = format!("{name}.localhost");
    let domain = params.domain.unwrap_or(&fallback_domain);
    let ctx = context::build_context(
        params.config,
        params.service_def,
        domain,
        params.host_port,
        params.auth_kind,
    );
    let rendered_env = render_env_vars(
        params.service_def,
        &ctx,
        params.env_overrides,
        params.auth_kind,
    )?;

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

    let service = generate_quadlet(GenerateQuadletParams {
        name,
        service_def: params.service_def,
        exposure: params.exposure,
        host_port: params.host_port,
        quadlet_dir: params.quadlet_dir,
        env_file,
        nginx_site,
    })?;

    Ok(GenerationOutput { service, ctx })
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
    service_def: &'a ServiceDef,
    exposure: &'a ExposureMode,
    host_port: Option<u16>,
    quadlet_dir: &'a Path,
    env_file: GeneratedFile,
    nginx_site: Option<GeneratedFile>,
}

/// Generate quadlet files for a service (primary + sidecar containers).
fn generate_quadlet(params: GenerateQuadletParams<'_>) -> Result<GeneratedService> {
    let name = params.name;
    let service_def = params.service_def;
    let mut files = Vec::new();

    let bind_address = match params.exposure {
        ExposureMode::HostPort => quadlet::BindAddress::Any,
        _ => quadlet::BindAddress::Localhost,
    };

    // Network — shared by all containers
    let network_name = name.to_string();
    files.push(GeneratedFile {
        path: params.quadlet_dir.join(format!("{name}.network")),
        content: quadlet::render_network(&network_name),
    });

    // Volumes — primary container
    for vol in &service_def.volumes {
        if vol.host_path.is_none() {
            let vol_name = format!("{name}-{}", vol.name);
            files.push(GeneratedFile {
                path: params.quadlet_dir.join(format!("{vol_name}.volume")),
                content: quadlet::render_volume(&vol_name),
            });
        }
    }

    // Volumes — sidecar containers
    for container in &service_def.containers {
        for vol in &container.volumes {
            if vol.host_path.is_none() {
                let vol_name = format!("{name}-{}", vol.name);
                // Avoid duplicating volume files
                let vol_path = params.quadlet_dir.join(format!("{vol_name}.volume"));
                if !files.iter().any(|f| f.path == vol_path) {
                    files.push(GeneratedFile {
                        path: vol_path,
                        content: quadlet::render_volume(&vol_name),
                    });
                }
            }
        }
    }

    // Primary container — depends on all sidecars
    let sidecar_units: Vec<String> = service_def
        .containers
        .iter()
        .map(|c| format!("{name}-{}", c.name))
        .collect();

    let port_mappings: Vec<quadlet::PortMapping> = service_def
        .ports
        .iter()
        .map(|p| quadlet::PortMapping {
            host_port: params.host_port.unwrap_or(p.container_port),
            container_port: p.container_port,
            protocol: p.protocol.clone(),
        })
        .collect();

    let primary_volumes = build_volume_mappings(name, &service_def.volumes);

    files.push(GeneratedFile {
        path: params.quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&quadlet::QuadletParams {
            service_name: name,
            image: &service_def.service.image,
            ports: &port_mappings,
            volumes: &primary_volumes,
            network: &network_name,
            command: service_def.service.command.as_deref(),
            bind_address: &bind_address,
            depends_on: &sidecar_units,
            healthcheck: None,
            env_file: true,
            container_name: None,
            init: false,
        }),
    });

    // Sidecar containers
    for container in &service_def.containers {
        let container_name = format!("{name}-{}", container.name);

        // Resolve dependencies to unit names
        let deps: Vec<String> = container
            .depends_on
            .iter()
            .map(|d| format!("{name}-{d}"))
            .collect();

        let sidecar_volumes = build_volume_mappings(name, &container.volumes);

        files.push(GeneratedFile {
            path: params
                .quadlet_dir
                .join(format!("{container_name}.container")),
            content: quadlet::render_container(&quadlet::QuadletParams {
                service_name: &container_name,
                image: &container.image,
                ports: &[],
                volumes: &sidecar_volumes,
                network: &network_name,
                command: container.command.as_deref(),
                bind_address: &quadlet::BindAddress::Localhost,
                depends_on: &deps,
                healthcheck: container.healthcheck.as_ref(),
                env_file: container.env_file,
                container_name: Some(&container.name),
                init: container.init,
            }),
        });
    }

    Ok(GeneratedService {
        files,
        env_file: params.env_file,
        nginx_site: params.nginx_site,
    })
}

/// Build volume mappings for quadlet container rendering.
fn build_volume_mappings(
    service_name: &str,
    volumes: &[crate::registry::service_def::VolumeDef],
) -> Vec<quadlet::VolumeMapping> {
    volumes
        .iter()
        .map(|v| {
            if let Some(ref host_path) = v.host_path {
                quadlet::VolumeMapping::Bind {
                    host_path: host_path.clone(),
                    mount_path: v.mount_path.clone(),
                }
            } else {
                quadlet::VolumeMapping::Named {
                    volume_name: format!("{service_name}-{}", v.name),
                    mount_path: v.mount_path.clone(),
                }
            }
        })
        .collect()
}

// --- Shared helpers ---

fn render_env_vars(
    service_def: &ServiceDef,
    ctx: &BTreeMap<String, String>,
    env_overrides: &BTreeMap<String, String>,
    auth_kind: Option<&AuthKind>,
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
    if auth_kind.is_some() {
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
                ExposureMode::Local => nginx::SiteMode::HttpOnly,
                ExposureMode::Public => {
                    let (cert_path, key_path) = match &config.ssl {
                        Some(ssl) => crate::integrations::ssl::cert_paths_for_ssl(ssl, domain),
                        None => crate::integrations::ssl::letsencrypt_cert_paths(domain),
                    };
                    nginx::SiteMode::Ssl {
                        cert_path,
                        key_path,
                    }
                }
                ExposureMode::Tailscale => {
                    let (cert_path, key_path) = crate::integrations::tailscale::cert_paths(domain);
                    nginx::SiteMode::SslPort {
                        listen_port: upstream_port,
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
