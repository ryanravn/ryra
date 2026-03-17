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
/// `env_overrides` contains user-provided values that replace env var defaults.
pub fn generate_service(
    config: &Config,
    state: &mut State,
    service_def: &ServiceDef,
    domain: Option<&str>,
    exposure: &ExposureMode,
    quadlet_dir: &Path,
    nginx_dir: &Path,
    env_overrides: &BTreeMap<String, String>,
) -> Result<GeneratedService> {
    let name = &service_def.service.name;
    let mut quadlet_files = Vec::new();

    // Allocate ports
    for port_def in &service_def.ports {
        port::allocate_port(state, name, &port_def.name)?;
    }

    // Generate secrets for any {{secret.*}} references in env vars
    // (skip for env vars that have been overridden by user input)
    for env in &service_def.env {
        if !env_overrides.contains_key(&env.name) {
            extract_secret_refs(&env.value)
                .into_iter()
                .for_each(|secret_name| {
                    secret::ensure_secret(state, name, &secret_name);
                });
        }
    }

    // Build template context
    let ctx = context::build_context(config, state, service_def, domain.unwrap_or_default());

    // Render env vars — user overrides replace the template value
    let mut rendered_env: Vec<EnvVar> = service_def
        .env
        .iter()
        .map(|env| {
            let value = match env_overrides.get(&env.name) {
                Some(override_value) => override_value.clone(),
                None => template::render(&env.value, &ctx)?,
            };
            Ok(EnvVar {
                name: env.name.clone(),
                value,
                prompt: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Apply integration mappings (smtp, auth) — these map global config
    // to service-specific env var names
    if service_def.integrations.smtp {
        for (env_name, value_template) in &service_def.mappings.smtp {
            let rendered = template::render(value_template, &ctx)?;
            if !rendered.is_empty() {
                rendered_env.push(EnvVar {
                    name: env_name.clone(),
                    value: rendered,
                    prompt: None,
                });
            }
        }
    }
    if service_def.integrations.auth {
        for (env_name, value_template) in &service_def.mappings.auth {
            let rendered = template::render(value_template, &ctx)?;
            if !rendered.is_empty() {
                rendered_env.push(EnvVar {
                    name: env_name.clone(),
                    value: rendered,
                    prompt: None,
                });
            }
        }
    }

    // Build port mappings
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

    // Determine bind address from exposure mode
    let bind_address = match exposure {
        ExposureMode::HostPort => quadlet::BindAddress::Any,
        _ => quadlet::BindAddress::Localhost,
    };

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
        bind_address: &bind_address,
    };

    quadlet_files.push(GeneratedFile {
        path: quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&container_params),
    });

    // Generate nginx site config (only for web services with a domain)
    let nginx_site = match (&service_def.nginx, domain) {
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
                ExposureMode::HostPort => nginx::SiteMode::HttpOnly,
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
        }
        _ => None,
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
