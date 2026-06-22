pub mod bundle;
pub mod context;
pub mod template;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::exposure::Exposure;
use crate::registry::service_def::{AuthKind, EnvKind, EnvVar, PortDef, ServiceDef};

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
    /// How the service is exposed; its URL (if any) feeds templates like
    /// `{{service.external_url}}` / `{{service.domain}}`.
    pub exposure: &'a Exposure,
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
    /// `[[choice]]` selections (`choice name -> option name`). Only the
    /// selected option's members are written; absent choices fall back to
    /// their declared `default`.
    pub selected_choices: &'a BTreeMap<String, String>,
    /// Raw contents of the service's existing on-disk `.env`, when one is
    /// already present (a re-add, or a hand-authored "bring your own" file).
    /// The generated env is *merged into* this rather than overwriting it:
    /// existing lines, comments, and keys the registry doesn't know about are
    /// preserved; only keys the user set this run (`env_overrides`) are
    /// updated in place; new declared keys are appended. `None` for a fresh
    /// install (no file yet) — the generated content is written as-is.
    pub existing_env_file: Option<&'a str>,
    /// Skip-setup: render an unset `Required` var that is a member of an
    /// enabled group / selected choice to an empty value instead of erroring,
    /// so an install can proceed with the operator filling the blanks in
    /// `.env` afterwards. `false` keeps the strict default (a missing required
    /// member is a hard error). A top-level required var renders empty either
    /// way — it's gated at the CLI prompt layer, not here.
    pub allow_unset_required: bool,
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
        params.exposure,
        params.enable_smtp,
    )?;
    if let Some(prebuilt) = params.pre_built_ctx {
        for (key, value) in prebuilt {
            if key.starts_with("secret.") || key.starts_with("auth.") {
                ctx.insert(key, value);
            }
        }
    }
    // Effective ports = top-level plus the selected choice option's gated ports
    // (so a gated port's port_url resolves; mirrors the allocator in lib.rs).
    let mut eff_ports: Vec<&PortDef> = params.service_def.ports.iter().collect();
    for choice in &params.service_def.choices {
        let sel = params
            .selected_choices
            .get(&choice.name)
            .unwrap_or(&choice.default);
        if let Some(opt) = choice.options.iter().find(|o| &o.name == sel) {
            eff_ports.extend(opt.ports.iter());
        }
    }
    insert_port_urls(
        &mut ctx,
        &eff_ports,
        params.resolved_ports,
        params.exposure.url(),
    );

    let rendered_env = render_env_vars(
        params.service_def,
        &ctx,
        params.env_overrides,
        params.auth_kind,
        params.enabled_groups,
        params.selected_choices,
        params.allow_unset_required,
    )?;

    // Ordered KEY=VALUE pairs the registry render produces: declared vars,
    // ryra's own SERVICE_* lines, CA-trust/extra vars, then any operator keys
    // the user supplied this run (e.g. via --env-file) that the registry
    // doesn't declare — so those aren't silently dropped.
    let home_dir = crate::service_home(name)?;
    let generated = build_env_pairs(
        &home_dir,
        &rendered_env,
        params.resolved_ports,
        &params.extra_env,
        params.env_overrides,
    );

    // Keys the user set explicitly this run win over what's on disk; every
    // other existing line (untouched values, comments, undeclared operator
    // vars) is preserved by the merge.
    let explicit: BTreeSet<&str> = params.env_overrides.keys().map(String::as_str).collect();
    let content = merge_env_file(params.existing_env_file, &generated, &explicit);
    let env_file = GeneratedFile {
        path: home_dir.join(".env"),
        content,
    };

    Ok(EnvOutput { env_file, ctx })
}

/// Merge the registry-rendered env into a service's existing `.env`.
///
/// The `.env` carries runtime-rotated secrets and operator-authored keys the
/// registry never sees, so a re-render must never blindly overwrite it. With
/// `existing` present (a re-add or a hand-authored file) we walk the file
/// line by line: comments and blanks pass through verbatim, a key the user set
/// this run (`explicit`) is updated in place, and every other line is kept as
/// is. Declared keys absent from the file are appended. With `existing` `None`
/// (a fresh install, no file yet) the generated content is written as-is.
fn merge_env_file(
    existing: Option<&str>,
    generated: &[(String, String)],
    explicit: &BTreeSet<&str>,
) -> String {
    let render_fresh = || {
        generated
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    let Some(existing) = existing else {
        return render_fresh();
    };
    let gen_map: BTreeMap<&str, &str> = generated
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push(line.to_string());
            continue;
        }
        let Some((raw_key, _)) = line.split_once('=') else {
            // Not a KEY=VALUE line — preserve verbatim.
            out.push(line.to_string());
            continue;
        };
        let key = raw_key.trim();
        seen.insert(key);
        // Only a key the user set this run replaces its on-disk value; an
        // untouched key keeps whatever's there (a rotated secret, a manual edit).
        if explicit.contains(key)
            && let Some(value) = gen_map.get(key)
        {
            out.push(format!("{key}={value}"));
            continue;
        }
        out.push(line.to_string());
    }
    // Append declared / SERVICE_* / operator keys the file doesn't have yet.
    for (key, value) in generated {
        if !seen.contains(key.as_str()) {
            out.push(format!("{key}={value}"));
        }
    }
    out.join("\n") + "\n"
}

/// Insert `service.port_url.<name>` for every declared port — the URL at
/// which that specific port is reachable from a browser.
///
/// For single-endpoint services every port resolves to `external_url`.
/// Multi-port services (ente: web UI on 443, API on 8080, served on separate
/// Tailscale HTTPS ports) get a distinct URL per port, so a template like
/// `ENTE_API_ORIGIN = {{service.port_url.http}}` points at the API endpoint
/// while the bare hostname serves the UI — in every exposure mode (loopback,
/// raw `--url`, or `--tailscale`).
fn insert_port_urls(
    ctx: &mut BTreeMap<String, String>,
    // Effective ports: top-level plus the selected choice option's, so a gated
    // port (e.g. ente's bundled minio) gets `service.port_url.*` too.
    ports: &[&PortDef],
    resolved_ports: &[(String, u16)],
    url: Option<&str>,
) {
    // Bare allocated host port per name, for templating into values like a
    // DATABASE_URL that points at a gated container's published loopback port
    // (`@127.0.0.1:{{service.ports.db}}`). Covers top-level and choice-gated
    // ports alike, since `resolved_ports` already includes both. Plural
    // `service.ports.*` so it doesn't collide with the `service.port` leaf
    // (the template engine nests dotted keys; a key can't be leaf and parent).
    for (name, port) in resolved_ports {
        ctx.insert(format!("service.ports.{name}"), port.to_string());
    }
    // The primary port (named "http", else the first) answers at the root
    // URL — for it, `port_url` is exactly `external_url` outside Tailscale.
    let primary = ports
        .iter()
        .copied()
        .find(|p| p.name.eq_ignore_ascii_case("http"))
        .or_else(|| ports.first().copied())
        .map(|p| p.name.clone());
    let parsed = url.and_then(|u| url::Url::parse(u).ok());
    let host = parsed
        .as_ref()
        .and_then(|u| u.host_str())
        .map(str::to_string);
    let scheme = parsed.as_ref().map(|u| u.scheme().to_string());
    let is_ts = host.as_deref().is_some_and(|h| h.ends_with(".ts.net"));
    let external_url = ctx.get("service.external_url").cloned();

    for p in ports.iter().copied() {
        let host_port = resolved_ports
            .iter()
            .find(|(n, _)| n == &p.name)
            .map(|(_, hp)| *hp)
            .or(p.host_port)
            .unwrap_or(p.container_port);
        let is_primary = primary.as_deref() == Some(p.name.as_str());
        let port_url =
            if let (true, Some(https), Some(h)) = (is_ts, p.tailscale_https, host.as_deref()) {
                // Tailscale: this port answers at the service hostname on its
                // HTTPS port (443 is the bare hostname, no explicit port).
                if https == 443 {
                    format!("https://{h}")
                } else {
                    format!("https://{h}:{https}")
                }
            } else if is_primary && let Some(ext) = &external_url {
                ext.clone()
            } else if let (Some(s), Some(h)) = (scheme.as_deref(), host.as_deref()) {
                // Non-primary port under a raw `--url`: directly published at the
                // same host on its own host port.
                format!("{s}://{h}:{host_port}")
            } else {
                format!("http://127.0.0.1:{host_port}")
            };
        ctx.insert(format!("service.port_url.{}", p.name), port_url);
    }
}

/// Ordered KEY=VALUE pairs the registry render produces for a service's `.env`,
/// before merging with any existing file. Order: declared/group/choice vars,
/// then ryra's own `SERVICE_*` lines, then CA-trust/extra vars, then any
/// operator keys the user supplied this run (`--env-file`) that the registry
/// doesn't declare — so a bring-your-own key isn't silently dropped.
fn build_env_pairs(
    home_dir: &std::path::Path,
    rendered_env: &[EnvVar],
    resolved_ports: &[(String, u16)],
    extra_env: &BTreeMap<String, String>,
    env_overrides: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();

    for env in rendered_env {
        // Raw KEY=VALUE for podman --env-file. Podman does NOT strip quotes
        // (single or double), so any shell-style quoting ends up as literal
        // characters in the container. Tests that source the .env must stick
        // to values that survive unquoted bash parsing.
        pairs.push((env.name.clone(), env.value.clone()));
    }

    // Expose service home path so scripts can reference it.
    pairs.push(("SERVICE_HOME".to_string(), home_dir.display().to_string()));

    // Expose each [[ports]] entry as SERVICE_PORT_<NAME> with its resolved
    // host port. The SERVICE_ prefix matches SERVICE_HOME and makes
    // ryra-emitted vars visually distinct from service-specific ones (which
    // carry their own naming, e.g. POSTGRES_PASSWORD).
    for (name, port) in resolved_ports {
        pairs.push((
            format!("SERVICE_PORT_{}", name.to_uppercase()),
            port.to_string(),
        ));
    }

    // Extra vars (e.g. CA-cert trust for OIDC).
    for (key, value) in extra_env {
        pairs.push((key.clone(), value.clone()));
    }

    // Operator keys the user passed this run (`--env-file`) that none of the
    // above emitted — undeclared, but explicitly provided, so write them
    // rather than drop them. (Process-env overrides only ever carry declared
    // keys, which are already covered above.)
    let emitted: BTreeSet<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
    for (key, value) in env_overrides {
        if !emitted.contains(key.as_str()) {
            pairs.push((key.clone(), value.clone()));
        }
    }

    pairs
}

// --- Shared helpers ---

fn render_env_vars(
    service_def: &ServiceDef,
    ctx: &BTreeMap<String, String>,
    env_overrides: &BTreeMap<String, String>,
    auth_kind: Option<&AuthKind>,
    enabled_groups: &BTreeSet<String>,
    selected_choices: &BTreeMap<String, String>,
    allow_unset_required: bool,
) -> Result<Vec<EnvVar>> {
    let mut rendered: Vec<EnvVar> = service_def
        .env
        .iter()
        .map(|env| render_one(env, env_overrides, ctx, None, allow_unset_required))
        .collect::<Result<Vec<_>>>()?;

    // Append members of every enabled `[[env_group]]`. Groups not toggled
    // on are fully omitted, no partial state possible.
    for group in &service_def.env_groups {
        if !enabled_groups.contains(&group.name) {
            continue;
        }
        let loc = format!("group '{}'", group.name);
        for env in &group.env {
            rendered.push(render_one(
                env,
                env_overrides,
                ctx,
                Some(&loc),
                allow_unset_required,
            )?);
        }
    }

    // Append the selected option of every `[[choice]]`. With no recorded
    // selection (a choice added after install) we fall back to the choice's
    // `default`, which validate() guarantees names a real option. Only the
    // selected option's members are written; the rest never appear.
    for choice in &service_def.choices {
        let selected = selected_choices
            .get(&choice.name)
            .unwrap_or(&choice.default);
        let Some(option) = choice.options.iter().find(|o| &o.name == selected) else {
            continue;
        };
        let loc = format!("choice '{}' option '{}'", choice.name, option.name);
        for env in &option.env {
            rendered.push(render_one(
                env,
                env_overrides,
                ctx,
                Some(&loc),
                allow_unset_required,
            )?);
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
/// error so the service never starts with half of a group configured —
/// unless `allow_unset_required` (skip-setup), where they render empty for
/// the operator to fill in afterwards.
fn render_one(
    env: &EnvVar,
    env_overrides: &BTreeMap<String, String>,
    ctx: &BTreeMap<String, String>,
    // Location phrase for the "required member has no value" error, e.g.
    // `"group 'stripe'"` or `"choice 'billing' option 'live'"`. `None` for a
    // top-level var (a required top-level var is caught earlier at prompt time).
    member_of: Option<&str>,
    allow_unset_required: bool,
) -> Result<EnvVar> {
    let value = match env_overrides.get(&env.name) {
        Some(override_value) => override_value.clone(),
        None => {
            if let Some(loc) = member_of
                && env.kind == EnvKind::Required
                && !allow_unset_required
            {
                return Err(Error::Template(format!(
                    "required env var '{}' in {loc} has no value; provide it via the interactive prompt or process env (or `--no-setup` to install and fill it in later)",
                    env.name
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
                runtime: Default::default(),
                run: None,
                build: None,
                post_install: None,
                deploy: Default::default(),
                health_check: None,
                health_timeout: None,
            },
            requirements: None,
            ports: vec![PortDef {
                name: "http".into(),
                container_port: 80,
                host_port: None,
                protocol: Default::default(),
                tailscale_https: None,
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
            choices: vec![],
            requires: vec![],
            mappings: Default::default(),
            integrations: Default::default(),
            capabilities: Default::default(),
            backup: None,
            metrics: None,
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

    /// Two-endpoint service like ente: museum API ("http") served over
    /// Tailscale on :8080, Photos UI ("photos") at the bare hostname (:443).
    fn multiport_def() -> ServiceDef {
        let mut def = minimal_service_def();
        def.ports = vec![
            PortDef {
                name: "http".into(),
                container_port: 8080,
                host_port: None,
                protocol: Default::default(),
                tailscale_https: Some(8080),
            },
            PortDef {
                name: "photos".into(),
                container_port: 3000,
                host_port: None,
                protocol: Default::default(),
                tailscale_https: Some(443),
            },
        ];
        def
    }

    fn port_urls(url: Option<&str>, external_url: &str) -> BTreeMap<String, String> {
        let def = multiport_def();
        let resolved = vec![
            ("http".to_string(), 8080u16),
            ("photos".to_string(), 10002u16),
        ];
        let mut ctx = BTreeMap::new();
        ctx.insert("service.external_url".to_string(), external_url.to_string());
        let ports: Vec<&PortDef> = def.ports.iter().collect();
        insert_port_urls(&mut ctx, &ports, &resolved, url);
        ctx
    }

    #[test]
    fn merge_fresh_install_writes_generated_verbatim() {
        let generated = vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ];
        // No existing file → full overwrite (the original fresh-install behaviour).
        assert_eq!(
            merge_env_file(None, &generated, &BTreeSet::new()),
            "A=1\nB=2\n"
        );
    }

    #[test]
    fn merge_preserves_operator_keys_comments_and_untouched_values() {
        // A re-add over a file carrying an operator key, a comment, and a
        // value the user customised — none declared-explicit this run.
        let existing = "# operator notes\nRYRA_TOKEN=secret-abc\nSITE_TITLE=Custom\n";
        let generated = vec![
            ("SITE_TITLE".to_string(), "Default".to_string()),
            ("ADMIN_EMAIL".to_string(), String::new()),
            ("SERVICE_HOME".to_string(), "/home/x".to_string()),
        ];
        let merged = merge_env_file(Some(existing), &generated, &BTreeSet::new());
        // Comment + undeclared operator key survive verbatim (the Bug A fix).
        assert!(merged.contains("# operator notes"));
        assert!(merged.contains("RYRA_TOKEN=secret-abc"));
        // A non-explicit existing value is kept, not reset to the default.
        assert!(merged.contains("SITE_TITLE=Custom"));
        assert!(!merged.contains("SITE_TITLE=Default"));
        // New declared keys are appended.
        assert!(merged.contains("ADMIN_EMAIL="));
        assert!(merged.contains("SERVICE_HOME=/home/x"));
    }

    #[test]
    fn merge_updates_only_explicitly_set_keys() {
        let existing = "SITE_TITLE=Old\nKEEP=stays\n";
        let generated = vec![
            ("SITE_TITLE".to_string(), "New".to_string()),
            ("KEEP".to_string(), "regenerated".to_string()),
        ];
        let explicit = BTreeSet::from(["SITE_TITLE"]);
        let merged = merge_env_file(Some(existing), &generated, &explicit);
        assert!(merged.contains("SITE_TITLE=New")); // explicit → updated in place
        assert!(merged.contains("KEEP=stays")); // untouched → preserved
        assert!(!merged.contains("KEEP=regenerated"));
    }

    #[test]
    fn port_url_loopback_uses_host_ports() {
        // No --url: primary == external_url (localhost), others at 127.0.0.1:<port>.
        let ctx = port_urls(None, "http://127.0.0.1:8080");
        assert_eq!(ctx["service.port_url.http"], "http://127.0.0.1:8080");
        assert_eq!(ctx["service.port_url.photos"], "http://127.0.0.1:10002");
    }

    #[test]
    fn port_url_raw_ip_url_exposes_each_port() {
        // Raw --url at a tailnet IP: museum == the url, photos directly published.
        let ctx = port_urls(Some("http://100.69.58.21:8080"), "http://100.69.58.21:8080");
        assert_eq!(ctx["service.port_url.http"], "http://100.69.58.21:8080");
        assert_eq!(ctx["service.port_url.photos"], "http://100.69.58.21:10002");
    }

    #[test]
    fn port_url_tailscale_splits_root_and_api() {
        // --tailscale: photos answers at the bare hostname, museum on :8080.
        let url = "https://ente-debian.cobbler-tuna.ts.net";
        let ctx = port_urls(Some(url), url);
        assert_eq!(
            ctx["service.port_url.http"],
            "https://ente-debian.cobbler-tuna.ts.net:8080"
        );
        assert_eq!(
            ctx["service.port_url.photos"],
            "https://ente-debian.cobbler-tuna.ts.net"
        );
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
            exposure: &Exposure::Loopback,
            extra_env: BTreeMap::new(),
            pre_built_ctx: None,
            enable_smtp: false,
            enabled_groups,
            selected_choices: &BTreeMap::new(),
            existing_env_file: None,
            allow_unset_required: false,
        })?;
        Ok(output.env_file.content)
    }

    fn gen_with_choices(
        def: &ServiceDef,
        selected: &BTreeMap<String, String>,
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
            exposure: &Exposure::Loopback,
            extra_env: BTreeMap::new(),
            pre_built_ctx: None,
            enable_smtp: false,
            enabled_groups: &BTreeSet::new(),
            selected_choices: selected,
            existing_env_file: None,
            allow_unset_required: false,
        })?;
        Ok(output.env_file.content)
    }

    fn def_with_billing_choice() -> ServiceDef {
        toml::from_str(
            r#"
[service]
name = "billed"
description = "x"

[[ports]]
name = "http"
container_port = 8080

[[choice]]
name = "billing"
prompt = "Billing mode"
default = "mock"

[[choice.option]]
name = "live"
[[choice.option.env]]
name = "BILLING_MODE"
value = "live"
[[choice.option.env]]
name = "STRIPE_SECRET_KEY"
value = ""
kind = "required"

[[choice.option]]
name = "mock"
[[choice.option.env]]
name = "BILLING_MODE"
value = "mock"
"#,
        )
        .expect("parse")
    }

    #[test]
    fn choice_writes_only_selected_option_members() {
        let def = def_with_billing_choice();
        let mut selected = BTreeMap::new();
        selected.insert("billing".to_string(), "mock".to_string());
        let content =
            gen_with_choices(&def, &selected, &BTreeMap::new()).expect("mock selection renders");
        assert!(content.contains("BILLING_MODE=mock"), "got: {content}");
        // The `live`-only Stripe var must not appear.
        assert!(!content.contains("STRIPE_SECRET_KEY"), "got: {content}");
    }

    #[test]
    fn choice_option_secret_is_generated() {
        // Regression: a `{{secret.*}}` referenced only inside a choice option
        // must still be minted. Secret generation used to scan top-level env
        // only, so this rendered to an undefined value and `ryra add` failed.
        let def = toml::from_str::<ServiceDef>(
            r#"
[service]
name = "s"
description = "x"
[[ports]]
name = "http"
container_port = 8080
[[choice]]
name = "database"
prompt = "Database"
default = "internal"
[[choice.option]]
name = "internal"
[[choice.option.env]]
name = "DB_PASSWORD"
value = "{{secret.db_password}}"
[[choice.option]]
name = "external"
[[choice.option.env]]
name = "DB_PASSWORD"
value = ""
kind = "required"
"#,
        )
        .expect("parse");
        let mut selected = BTreeMap::new();
        selected.insert("database".to_string(), "internal".to_string());
        let content = gen_with_choices(&def, &selected, &BTreeMap::new())
            .expect("renders with generated secret");
        let line = content
            .lines()
            .find(|l| l.starts_with("DB_PASSWORD="))
            .expect("DB_PASSWORD present");
        let val = line.trim_start_matches("DB_PASSWORD=");
        assert!(!val.is_empty() && !val.contains("{{"), "got: {line}");
    }

    #[test]
    fn choice_falls_back_to_default_when_unselected() {
        let def = def_with_billing_choice();
        // Empty selection map -> the `default` (mock) is rendered.
        let content = gen_with_choices(&def, &BTreeMap::new(), &BTreeMap::new())
            .expect("default selection renders");
        assert!(content.contains("BILLING_MODE=mock"), "got: {content}");
    }

    #[test]
    fn choice_required_member_needs_a_value() {
        // Selecting `live` without providing STRIPE_SECRET_KEY must error,
        // mirroring a required group member with no value.
        let def = def_with_billing_choice();
        let mut selected = BTreeMap::new();
        selected.insert("billing".to_string(), "live".to_string());
        let err = gen_with_choices(&def, &selected, &BTreeMap::new())
            .expect_err("required member without value must fail");
        assert!(
            format!("{err}").contains("STRIPE_SECRET_KEY"),
            "error names the missing var: {err}"
        );
    }

    #[test]
    fn choice_required_member_value_is_written() {
        let def = def_with_billing_choice();
        let mut selected = BTreeMap::new();
        selected.insert("billing".to_string(), "live".to_string());
        let mut overrides = BTreeMap::new();
        overrides.insert("STRIPE_SECRET_KEY".to_string(), "sk_test_123".to_string());
        let content = gen_with_choices(&def, &selected, &overrides).expect("live renders");
        assert!(content.contains("BILLING_MODE=live"), "got: {content}");
        assert!(
            content.contains("STRIPE_SECRET_KEY=sk_test_123"),
            "got: {content}"
        );
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
        let prebuilt =
            context::build_context(&config, &def, None, None, &Exposure::Loopback, false)
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
            exposure: &Exposure::Loopback,
            extra_env: BTreeMap::new(),
            pre_built_ctx: Some(prebuilt),
            enable_smtp: false,
            enabled_groups: &no_groups,
            selected_choices: &BTreeMap::new(),
            existing_env_file: None,
            allow_unset_required: false,
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
