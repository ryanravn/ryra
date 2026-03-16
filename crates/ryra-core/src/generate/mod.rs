pub mod context;
pub mod nginx;
pub mod quadlet;
pub mod template;
pub mod tunnel;

use std::path::{Path, PathBuf};

use crate::config::schema::{Config, ExposureMode};
use crate::config::state::State;
use crate::error::{Error, Result};
use crate::registry::service_def::{EnvVar, ServiceDef};
use crate::system::{port, secret};

/// Everything generated for a service, ready to be written to disk.
pub struct GeneratedService {
    pub quadlet_files: Vec<GeneratedFile>,
    pub nginx_site: Option<GeneratedFile>,
}

pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Generate all files for a standalone service (v0.1 — no dependency support).
pub fn generate_service(
    config: &Config,
    state: &mut State,
    service_def: &ServiceDef,
    domain: &str,
    exposure: &ExposureMode,
    quadlet_dir: &Path,
    nginx_dir: &Path,
) -> Result<GeneratedService> {
    let name = &service_def.service.name;
    let mut quadlet_files = Vec::new();

    // Allocate ports
    for port_def in &service_def.ports {
        port::allocate_port(state, name, &port_def.name)?;
    }

    // Generate secrets for any {{secret.*}} references in env vars
    for env in &service_def.env {
        extract_secret_refs(&env.value)
            .into_iter()
            .for_each(|secret_name| {
                secret::ensure_secret(state, name, &secret_name);
            });
    }

    // Build template context
    let ctx = context::build_context(config, state, service_def, domain);

    // Render env vars
    let rendered_env: Vec<EnvVar> = service_def
        .env
        .iter()
        .map(|env| {
            let rendered_value = template::render(&env.value, &ctx)?;
            Ok(EnvVar {
                name: env.name.clone(),
                value: rendered_value,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Build port mappings
    let port_mappings: Vec<quadlet::PortMapping> = service_def
        .ports
        .iter()
        .filter_map(|p| {
            port::get_port(state, name, &p.name).map(|host_port| quadlet::PortMapping {
                host_port,
                container_port: p.container_port,
            })
        })
        .collect();

    // Generate network
    let network_name = name.to_string();
    quadlet_files.push(GeneratedFile {
        path: quadlet_dir.join(format!("{name}.network")),
        content: quadlet::render_network(&network_name),
    });

    // Generate volumes
    for vol in &service_def.volumes {
        let vol_name = format!("{name}-{}", vol.name);
        quadlet_files.push(GeneratedFile {
            path: quadlet_dir.join(format!("{vol_name}.volume")),
            content: quadlet::render_volume(&vol_name),
        });
    }

    // We need owned strings for volume names to satisfy lifetime requirements
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

    // Generate container
    let container_params = quadlet::QuadletParams {
        service_name: name,
        image: &service_def.service.image,
        env_vars: &rendered_env,
        ports: &port_mappings,
        volumes: &volume_refs,
        network: &network_name,
        command: None,
    };

    quadlet_files.push(GeneratedFile {
        path: quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&container_params),
    });

    // Generate nginx site config
    let nginx_site = if let Some(nginx_def) = &service_def.nginx {
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
                nginx::SiteMode::Ssl {
                    cert_path,
                    key_path,
                }
            }
            ExposureMode::DnsOnly => {
                let (cert_path, key_path) = match &config.ssl {
                    Some(ssl) => crate::integrations::ssl::cert_paths_for_ssl(ssl, domain),
                    None => crate::integrations::ssl::letsencrypt_cert_paths(domain),
                };
                nginx::SiteMode::Ssl {
                    cert_path,
                    key_path,
                }
            }
        };

        Some(GeneratedFile {
            path: nginx_dir.join(format!("{name}.conf")),
            content: nginx::render_site(&nginx::NginxSiteParams {
                service_name: name,
                domain,
                upstream_port,
                mode,
            }),
        })
    } else {
        None
    };

    Ok(GeneratedService {
        quadlet_files,
        nginx_site,
    })
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
