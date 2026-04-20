pub mod bundle;
pub mod context;
pub mod template;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::registry::service_def::{AuthKind, EnvVar, ServiceDef};

#[derive(Debug)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Parameters for [`generate_env`].
pub struct GenerateEnvParams<'a> {
    pub config: &'a Config,
    pub service_def: &'a ServiceDef,
    /// The auth kind the user chose to enable, if any.
    pub auth_kind: Option<&'a AuthKind>,
    /// Primary host port (for `service.url` / `service.port` templating).
    pub host_port: Option<u16>,
    /// Per-port resolved host ports, keyed by port name (e.g. "http", "smtp").
    /// Used to emit `RYRA_PORT_*` lines in the .env file — each entry here
    /// corresponds to one `[[ports]]` definition in service.toml.
    pub resolved_ports: &'a [(String, u16)],
    pub env_overrides: &'a BTreeMap<String, String>,
    /// Public URL for the service (used in templates as `{{service.external_url}}`).
    pub url: Option<&'a str>,
    /// Additional env vars to append to the .env file (e.g., CA cert trust vars).
    pub extra_env: BTreeMap<String, String>,
    /// Pre-built template context. When provided, secrets and auth credentials
    /// from this context are reused instead of generating fresh ones. This
    /// ensures the values shown during interactive prompts match what gets
    /// written to the .env file.
    pub pre_built_ctx: Option<BTreeMap<String, String>>,
}

/// Result of generating env for a service.
pub struct EnvOutput {
    pub env_file: GeneratedFile,
    /// The template context used during generation (for auth registration, etc.).
    pub ctx: BTreeMap<String, String>,
}

/// Generate the .env file for a service based on its definition and context.
pub fn generate_env(params: GenerateEnvParams<'_>) -> Result<EnvOutput> {
    let name = &params.service_def.service.name;

    // Always build a fresh context with the now-known host_port. Overlay
    // secret.* and auth.* entries from the pre-built context so randomly
    // generated values the user saw during prompts stay stable.
    let mut ctx = context::build_context(
        params.config,
        params.service_def,
        params.host_port,
        params.auth_kind,
        params.url,
    )?;
    if let Some(prebuilt) = params.pre_built_ctx {
        for (key, value) in prebuilt {
            if key.starts_with("secret.") || key.starts_with("auth.") {
                ctx.insert(key, value);
            }
        }
    }
    let rendered_env = render_env_vars(
        params.service_def,
        &ctx,
        params.env_overrides,
        params.auth_kind,
    )?;

    // Build .env file content
    let home_dir = crate::service_home(name)?;
    let mut env_file = build_env_file(&home_dir, &rendered_env, params.resolved_ports);

    // Append extra env vars (e.g., CA cert trust for OIDC)
    for (key, value) in &params.extra_env {
        env_file.content.push_str(&format!("{key}={value}\n"));
    }

    Ok(EnvOutput { env_file, ctx })
}

/// Build the .env file for a service.
fn build_env_file(
    home_dir: &std::path::Path,
    rendered_env: &[EnvVar],
    resolved_ports: &[(String, u16)],
) -> GeneratedFile {
    let mut lines = Vec::new();

    for env in rendered_env {
        // Write raw KEY=VALUE for podman --env-file. Podman does NOT strip
        // quotes (single or double), so any shell-style quoting ends up as
        // literal characters in the container. Tests that source the .env
        // must stick to values that survive unquoted bash parsing.
        lines.push(format!("{}={}", env.name, env.value));
    }

    // Expose service home path so scripts can reference it
    lines.push(format!("RYRA_SERVICE_HOME={}", home_dir.display()));

    // Expose each [[ports]] entry as RYRA_PORT_<NAME> with its resolved
    // host port. Caller passes the per-port mapping computed in
    // `add_service` so multi-port services get distinct values.
    for (name, port) in resolved_ports {
        let var_name = format!("RYRA_PORT_{}", name.to_uppercase());
        lines.push(format!("{var_name}={port}"));
    }

    GeneratedFile {
        path: home_dir.join(".env"),
        content: lines.join("\n") + "\n",
    }
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
                jwt_claims: None,
                jwt_signing_key: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if service_def.integrations.smtp && ctx.contains_key("smtp.host") {
        for (env_name, value_template) in &service_def.mappings.smtp {
            let value = template::render(value_template, ctx)?;
            // Empty values are valid — e.g., inbucket doesn't need username/password.
            // Static values (no template) are always included as-is.
            rendered.push(EnvVar {
                name: env_name.clone(),
                value,
                kind: Default::default(),
                prompt: None,
                format: Default::default(),
                length: None,
                jwt_claims: None,
                jwt_signing_key: None,
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
                jwt_claims: None,
                jwt_signing_key: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;
    use crate::registry::service_def::{
        EnvKind, EnvVar, PortDef, ServiceDef, ServiceMeta,
    };

    fn minimal_service_def() -> ServiceDef {
        ServiceDef {
            service: ServiceMeta {
                name: "demo".into(),
                description: "demo".into(),
                url: None,
                kind: Default::default(),
                architecture: vec![],
                https: Default::default(),
            },
            requirements: None,
            ports: vec![PortDef {
                name: "http".into(),
                container_port: 80,
                host_port: None,
                protocol: Default::default(),
            }],
            env: vec![
                EnvVar {
                    name: "HOSTPORT".into(),
                    value: "{{service.port}}".into(),
                    kind: EnvKind::Default,
                    prompt: None,
                    format: Default::default(),
                    length: None,
                    jwt_claims: None,
                    jwt_signing_key: None,
                },
                EnvVar {
                    name: "ADMIN_PASSWORD".into(),
                    value: "{{secret.admin}}".into(),
                    kind: EnvKind::Default,
                    prompt: None,
                    format: Default::default(),
                    length: Some(16),
                    jwt_claims: None,
                    jwt_signing_key: None,
                },
            ],
            requires: vec![],
            mappings: Default::default(),
            integrations: Default::default(),
        }
    }

    /// Regression: when the interactive CLI builds `pre_built_ctx` with
    /// `host_port: None` (the real port isn't allocated yet), `generate_env`
    /// must still produce a valid env file with the real `service.port`.
    /// Previously the pre-built ctx was reused wholesale, so any env value
    /// referencing `{{service.port}}` (e.g., seafile) failed strict-mode
    /// rendering with "undefined value".
    #[test]
    fn generate_env_rebuilds_port_when_prebuilt_ctx_lacks_it() {
        let def = minimal_service_def();
        let config = Config::default();
        // Build the pre-built ctx as the interactive prompt phase does:
        // host_port is None, so `service.port` is absent from the ctx.
        let prebuilt = context::build_context(&config, &def, None, None, None)
            .expect("build_context with host_port=None should succeed");
        assert!(!prebuilt.contains_key("service.port"));
        let admin_secret = prebuilt
            .get("secret.admin")
            .expect("secret.admin should have been generated in the prompt phase")
            .clone();

        // Now run generate_env with the real allocated host_port.
        let resolved = vec![("http".to_string(), 10002u16)];
        let output = generate_env(GenerateEnvParams {
            config: &config,
            service_def: &def,
            auth_kind: None,
            host_port: Some(10002),
            resolved_ports: &resolved,
            env_overrides: &BTreeMap::new(),
            url: None,
            extra_env: BTreeMap::new(),
            pre_built_ctx: Some(prebuilt),
        })
        .expect("generate_env must succeed with the real host_port");

        // The resulting .env must carry the allocated port, and the randomly
        // generated secret from the prompt phase must be preserved verbatim.
        assert!(
            output.env_file.content.contains("HOSTPORT=10002"),
            ".env missing real port: {}",
            output.env_file.content,
        );
        assert!(
            output
                .env_file
                .content
                .contains(&format!("ADMIN_PASSWORD={admin_secret}")),
            "prompt-phase secret not preserved in .env: {}",
            output.env_file.content,
        );
    }
}
