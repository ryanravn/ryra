pub mod bundle;
pub mod context;
pub mod quadlet;
pub mod template;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::registry::service_def::{AuthKind, EnvVar, ServiceDef};

/// Everything generated for a service, ready to be written to disk.
pub struct GeneratedService {
    pub files: Vec<GeneratedFile>,
    pub env_file: GeneratedFile,
}

#[derive(Debug)]
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
    /// The auth kind the user chose to enable, if any.
    pub auth_kind: Option<&'a AuthKind>,
    pub host_port: Option<u16>,
    pub quadlet_dir: &'a Path,
    pub env_overrides: &'a BTreeMap<String, String>,
    pub service_dir: &'a Path,
    /// Extra host entries for containers (e.g., auth domain → host IP).
    pub add_hosts: Vec<(String, String)>,
    /// Extra volume mounts for containers (e.g., CA cert).
    pub extra_volumes: Vec<String>,
    /// Domain for the service (used in templates as `{{service.domain}}`).
    pub domain: Option<&'a str>,
    /// Additional networks to join (e.g., caddy's network for reverse proxy).
    pub extra_networks: Vec<String>,
    /// Extra env vars to append to the .env file (e.g., CA cert trust vars).
    pub extra_env: BTreeMap<String, String>,
}

/// Generate all files for a service based on its deploy mode.
/// `host_port` is the allocated port for the service (None if using container port directly).
pub fn generate_service(params: GenerateServiceParams<'_>) -> Result<GenerationOutput> {
    let name = &params.service_def.service.name;

    // Build template context (generates fresh secrets based on each env var's format + length)
    let ctx = context::build_context(
        params.config,
        params.service_def,
        params.host_port,
        params.auth_kind,
        params.domain,
    );
    let rendered_env = render_env_vars(
        params.service_def,
        &ctx,
        params.env_overrides,
        params.auth_kind,
    )?;

    // Build .env file content
    let home_dir = crate::service_home(name)?;
    let mut env_file = build_env_file(
        &home_dir,
        &rendered_env,
        params.service_def,
        params.host_port,
    );

    // Append extra env vars (e.g., CA cert trust for OIDC)
    for (key, value) in &params.extra_env {
        env_file.content.push_str(&format!("{key}={value}\n"));
    }

    let service = generate_quadlet(GenerateQuadletParams {
        name,
        service_def: params.service_def,
        host_port: params.host_port,
        quadlet_dir: params.quadlet_dir,
        env_file,
        add_hosts: &params.add_hosts,
        extra_volumes: &params.extra_volumes,
        extra_networks: &params.extra_networks,
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
        // Quote values with spaces so `set -a && . .env` works in shell
        if env.value.contains(' ') {
            lines.push(format!("{}=\"{}\"", env.name, env.value));
        } else {
            lines.push(format!("{}={}", env.name, env.value));
        }
    }

    // Expose port as RYRA_PORT_* — use the fixed host_port from the port definition
    // if set, otherwise the allocated host port, otherwise the container port.
    for port_def in &service_def.ports {
        let port = port_def
            .host_port
            .or(host_port)
            .unwrap_or(port_def.container_port);
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
    host_port: Option<u16>,
    quadlet_dir: &'a Path,
    env_file: GeneratedFile,
    add_hosts: &'a [(String, String)],
    extra_volumes: &'a [String],
    extra_networks: &'a [String],
}

/// Generate quadlet files for a service (primary + sidecar containers).
fn generate_quadlet(params: GenerateQuadletParams<'_>) -> Result<GeneratedService> {
    let name = params.name;
    let service_def = params.service_def;
    let mut files = Vec::new();

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
            host_port: p
                .host_port
                .unwrap_or(params.host_port.unwrap_or(p.container_port)),
            container_port: p.container_port,
            protocol: p.protocol.clone(),
        })
        .collect();

    let primary_volumes = build_volume_mappings(name, &service_def.volumes)?;
    let env_path = params.env_file.path.to_string_lossy().to_string();

    files.push(GeneratedFile {
        path: params.quadlet_dir.join(format!("{name}.container")),
        content: quadlet::render_container(&quadlet::QuadletParams {
            service_name: name,
            image: &service_def.service.image,
            ports: &port_mappings,
            volumes: &primary_volumes,
            network: &network_name,
            command: service_def.service.command.as_deref(),
            depends_on: &sidecar_units,
            healthcheck: None,
            env_file: Some(&env_path),
            container_name: None,
            init: false,
            add_hosts: params.add_hosts,
            extra_volumes: params.extra_volumes,
            extra_networks: params.extra_networks,
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

        let sidecar_volumes = build_volume_mappings(name, &container.volumes)?;

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
                depends_on: &deps,
                healthcheck: container.healthcheck.as_ref(),
                env_file: if container.env_file {
                    Some(&env_path)
                } else {
                    None
                },
                container_name: Some(&container.name),
                init: container.init,
                add_hosts: params.add_hosts,
                extra_volumes: params.extra_volumes,
                extra_networks: params.extra_networks,
            }),
        });
    }

    Ok(GeneratedService {
        files,
        env_file: params.env_file,
    })
}

/// Build volume mappings for quadlet container rendering.
/// Resolves `%h` in host paths to the actual service data directory.
fn build_volume_mappings(
    service_name: &str,
    volumes: &[crate::registry::service_def::VolumeDef],
) -> crate::error::Result<Vec<quadlet::VolumeMapping>> {
    let home = crate::service_home(service_name)?;
    Ok(volumes
        .iter()
        .map(|v| {
            if let Some(ref host_path) = v.host_path {
                let resolved = host_path.replace("%h", &home.to_string_lossy());
                quadlet::VolumeMapping::Bind {
                    host_path: resolved,
                    mount_path: v.mount_path.clone(),
                }
            } else {
                quadlet::VolumeMapping::Named {
                    volume_name: format!("{service_name}-{}", v.name),
                    mount_path: v.mount_path.clone(),
                }
            }
        })
        .collect())
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
            if value.is_empty() {
                return Err(Error::Template(format!(
                    "SMTP mapping {env_name} rendered to empty value from template: {value_template}"
                )));
            }
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
    if auth_kind.is_some() {
        for (env_name, value_template) in &service_def.mappings.auth {
            let value = template::render(value_template, ctx)?;
            if value.is_empty() {
                return Err(Error::Template(format!(
                    "auth mapping {env_name} rendered to empty value from template: {value_template}"
                )));
            }
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

    Ok(rendered)
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
