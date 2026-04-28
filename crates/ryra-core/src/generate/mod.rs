pub mod bundle;
pub mod context;
pub mod template;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::registry::service_def::{AuthKind, EnvKind, EnvVar, ServiceDef};

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
    /// Used to emit `PORT_*` lines in the .env file — each entry here
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
    /// Whether this service should use the globally configured SMTP. When
    /// false, smtp.* is left out of the context and [mappings.smtp] is skipped
    /// — lets the user opt a single service out of email without clearing the
    /// global SMTP config.
    pub enable_smtp: bool,
    /// Names of `[[env_group]]` entries the user toggled on. Members of
    /// groups not listed here are fully omitted from the generated `.env`.
    pub enabled_groups: &'a BTreeSet<String>,
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
        params.enable_smtp,
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
        params.enabled_groups,
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
    lines.push(format!("SERVICE_HOME={}", home_dir.display()));

    // Expose each [[ports]] entry as SERVICE_PORT_<NAME> with its
    // resolved host port. The SERVICE_ prefix matches SERVICE_HOME and
    // makes ryra-emitted vars visually distinct from service-specific
    // ones (which carry their own naming, e.g. POSTGRES_PASSWORD).
    for (name, port) in resolved_ports {
        let var_name = format!("SERVICE_PORT_{}", name.to_uppercase());
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
    enabled_groups: &BTreeSet<String>,
) -> Result<Vec<EnvVar>> {
    let mut rendered: Vec<EnvVar> = service_def
        .env
        .iter()
        .map(|env| render_one(env, env_overrides, ctx, None))
        .collect::<Result<Vec<_>>>()?;

    // Append members of every enabled `[[env_group]]`. Groups not toggled
    // on are fully omitted — no partial state possible.
    for group in &service_def.env_groups {
        if !enabled_groups.contains(&group.name) {
            continue;
        }
        for env in &group.env {
            rendered.push(render_one(env, env_overrides, ctx, Some(&group.name))?);
        }
    }

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

/// Render a single `EnvVar` — apply an override if present, otherwise run
/// the template. Required group members without an override are a hard
/// error so the service never starts with half of a group configured.
fn render_one(
    env: &EnvVar,
    env_overrides: &BTreeMap<String, String>,
    ctx: &BTreeMap<String, String>,
    group: Option<&str>,
) -> Result<EnvVar> {
    let value = match env_overrides.get(&env.name) {
        Some(override_value) => override_value.clone(),
        None => {
            if let Some(group_name) = group
                && env.kind == EnvKind::Required
            {
                return Err(Error::Template(format!(
                    "required env var '{}' in group '{}' has no value — provide it via the interactive prompt or process env",
                    env.name, group_name
                )));
            }
            template::render(&env.value, ctx)?
        }
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
        EnvGroup, EnvKind, EnvVar, PortDef, ServiceDef, ServiceMeta,
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
            env_groups: vec![],
            requires: vec![],
            mappings: Default::default(),
            integrations: Default::default(),
            capabilities: Default::default(),
        }
    }

    fn plain_env(name: &str, value: &str, kind: EnvKind) -> EnvVar {
        EnvVar {
            name: name.into(),
            value: value.into(),
            kind,
            prompt: None,
            format: Default::default(),
            length: None,
            jwt_claims: None,
            jwt_signing_key: None,
        }
    }

    fn def_with_oauth_group() -> ServiceDef {
        let mut def = minimal_service_def();
        def.env_groups.push(EnvGroup {
            name: "google_oauth".into(),
            prompt: "Enable Google?".into(),
            env: vec![
                plain_env("CLIENT_ID", "", EnvKind::Required),
                plain_env("CLIENT_SECRET", "", EnvKind::Required),
                plain_env("CALLBACK_URL", "https://demo/cb", EnvKind::Default),
                plain_env("OAUTH_ENABLED", "true", EnvKind::Default),
            ],
        });
        def
    }

    fn gen_with_group(
        def: &ServiceDef,
        enabled_groups: &BTreeSet<String>,
        overrides: &BTreeMap<String, String>,
    ) -> Result<String> {
        let config = Config::default();
        let resolved = vec![("http".to_string(), 10002u16)];
        let output = generate_env(GenerateEnvParams {
            config: &config,
            service_def: def,
            auth_kind: None,
            host_port: Some(10002),
            resolved_ports: &resolved,
            env_overrides: overrides,
            url: None,
            extra_env: BTreeMap::new(),
            pre_built_ctx: None,
            enable_smtp: false,
            enabled_groups,
        })?;
        Ok(output.env_file.content)
    }

    #[test]
    fn env_group_disabled_writes_no_members() {
        let def = def_with_oauth_group();
        let no_groups = BTreeSet::new();
        let content = gen_with_group(&def, &no_groups, &BTreeMap::new())
            .expect("generate_env should succeed with no groups enabled");
        for name in [
            "CLIENT_ID",
            "CLIENT_SECRET",
            "CALLBACK_URL",
            "OAUTH_ENABLED",
        ] {
            assert!(
                !content.contains(&format!("{name}=")),
                "disabled group member '{name}' leaked into .env: {content}"
            );
        }
    }

    #[test]
    fn env_group_enabled_writes_all_members() {
        let def = def_with_oauth_group();
        let mut enabled = BTreeSet::new();
        enabled.insert("google_oauth".to_string());
        let mut overrides = BTreeMap::new();
        overrides.insert("CLIENT_ID".into(), "my-client".into());
        overrides.insert("CLIENT_SECRET".into(), "my-secret".into());
        let content = gen_with_group(&def, &enabled, &overrides)
            .expect("generate_env should succeed with the group enabled + overrides supplied");
        assert!(content.contains("CLIENT_ID=my-client"), "{content}");
        assert!(content.contains("CLIENT_SECRET=my-secret"), "{content}");
        assert!(
            content.contains("CALLBACK_URL=https://demo/cb"),
            "{content}"
        );
        assert!(content.contains("OAUTH_ENABLED=true"), "{content}");
    }

    #[test]
    fn env_group_enabled_required_member_without_override_errors() {
        let def = def_with_oauth_group();
        let mut enabled = BTreeSet::new();
        enabled.insert("google_oauth".to_string());
        // Intentionally leave CLIENT_SECRET out — required members with no
        // value must fail loudly, never produce an empty .env entry.
        let mut overrides = BTreeMap::new();
        overrides.insert("CLIENT_ID".into(), "my-client".into());
        let err = gen_with_group(&def, &enabled, &overrides)
            .expect_err("required member missing must surface as an error");
        let msg = err.to_string();
        assert!(
            msg.contains("CLIENT_SECRET") && msg.contains("google_oauth"),
            "error should name the missing member + group: {msg}"
        );
    }

    /// Regression: when the interactive CLI builds `pre_built_ctx` with
    /// `host_port: None` (the real port isn't allocated yet), `generate_env`
    /// must still produce a valid env file with the real `service.port`.
    /// Previously the pre-built ctx was reused wholesale, so any env value
    /// referencing `{{service.port}}` failed strict-mode rendering with
    /// "undefined value".
    #[test]
    fn generate_env_rebuilds_port_when_prebuilt_ctx_lacks_it() {
        let def = minimal_service_def();
        let config = Config::default();
        // Build the pre-built ctx as the interactive prompt phase does:
        // host_port is None, so `service.port` is absent from the ctx.
        let prebuilt = context::build_context(&config, &def, None, None, None, false)
            .expect("build_context with host_port=None should succeed");
        assert!(!prebuilt.contains_key("service.port"));
        let admin_secret = prebuilt
            .get("secret.admin")
            .expect("secret.admin should have been generated in the prompt phase")
            .clone();

        // Now run generate_env with the real allocated host_port.
        let resolved = vec![("http".to_string(), 10002u16)];
        let no_groups = BTreeSet::new();
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
            enable_smtp: false,
            enabled_groups: &no_groups,
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
