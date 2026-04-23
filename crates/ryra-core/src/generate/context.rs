use std::collections::BTreeMap;

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::registry::service_def::{AuthKind, EnvFormat, ServiceDef};
use crate::system::secret;

/// Build the template context for rendering env var values.
/// Secrets are generated fresh using each env var's format + length.
///
/// Returns an error if any provided URL (service `--url` or the stored
/// auth-provider URL) fails to parse or is missing a host — template
/// rendering downstream depends on those being well-formed.
pub fn build_context(
    config: &Config,
    service_def: &ServiceDef,
    host_port: Option<u16>,
    auth_kind: Option<&AuthKind>,
    url: Option<&str>,
    enable_smtp: bool,
) -> Result<BTreeMap<String, String>> {
    let mut ctx = BTreeMap::new();

    // service.*
    ctx.insert("service.name".into(), service_def.service.name.clone());
    if let Some(port) = host_port {
        ctx.insert("service.port".into(), port.to_string());
    }
    // service.url — localhost-based, always includes the port
    let effective_port = host_port.or_else(|| service_def.ports.first().map(|p| p.container_port));
    let localhost_url = match effective_port {
        Some(port) => format!("http://127.0.0.1:{port}"),
        None => "http://127.0.0.1".to_string(),
    };
    ctx.insert("service.url".into(), localhost_url.clone());
    if let Some(url) = url {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Template(format!("invalid service URL '{url}': {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| Error::Template(format!("service URL '{url}' has no host")))?;
        ctx.insert("service.domain".into(), host.to_string());
        ctx.insert("service.scheme".into(), parsed.scheme().to_string());
        // service.external_url — browser-accessible URL as provided by the user.
        ctx.insert("service.external_url".into(), url.to_string());
    } else {
        ctx.insert("service.scheme".into(), "http".into());
    }
    // service.external_url falls back to service.url when no url is set.
    ctx.entry("service.external_url".into())
        .or_insert(localhost_url.clone());
    // Ensure service.url is always set (even when external url overrides it)
    ctx.entry("service.url".into()).or_insert(localhost_url);

    // admin.*
    // Always set admin.email so strict-mode templates don't error when
    // config.admin_email is None. Service definitions can still wrap with
    // `| default(...)` for clarity, but the fallback is applied here first
    // so the `admin` namespace is always present in the context.
    ctx.insert(
        "admin.email".into(),
        config
            .admin_email
            .clone()
            .unwrap_or_else(|| "admin@example.com".to_string()),
    );

    // smtp.* — only populated when the caller opted this service into SMTP.
    // Without smtp.host in the context, render_env_vars skips the service's
    // [mappings.smtp] block entirely, so the service comes up without email.
    if enable_smtp && let Some(smtp) = &config.smtp {
        ctx.insert("smtp.host".into(), smtp.host.clone());
        ctx.insert("smtp.port".into(), smtp.port.to_string());
        ctx.insert("smtp.username".into(), smtp.username.clone());
        ctx.insert("smtp.password".into(), smtp.password.clone());
        ctx.insert("smtp.from".into(), smtp.from.clone());
        ctx.insert("smtp.security".into(), smtp.security.as_str().into());
    }

    // tls.*
    if let Some(tls) = &config.tls {
        use crate::config::schema::TlsConfig;
        match tls {
            TlsConfig::Caddy => {
                ctx.insert("tls.provider".into(), "caddy".into());
            }
            TlsConfig::Custom { cert, key } => {
                ctx.insert("tls.provider".into(), "custom".into());
                ctx.insert("tls.cert".into(), cert.display().to_string());
                ctx.insert("tls.key".into(), key.display().to_string());
            }
            TlsConfig::None => {
                ctx.insert("tls.provider".into(), "none".into());
            }
        }
    }

    // auth.* — per-service OIDC credentials (when user chose to enable auth)
    if let (Some(_), Some(auth)) = (auth_kind, &config.auth) {
        let auth_localhost_url = auth.url().to_string();
        let caddy_installed = config
            .services
            .iter()
            .any(|s| crate::WellKnownService::Caddy.matches(&s.name) && s.installed);
        // auth.external_url — browser-accessible URL.
        // Uses the stored URL from the auth provider's installed record if available.
        // When Caddy is installed, ensures the URL includes Caddy's HTTPS port
        // so it matches the issuer in authelia's OIDC discovery response.
        let mut external_url = config
            .services
            .iter()
            .find(|s| s.name == auth.provider_name())
            .and_then(|s| s.url.as_ref())
            .cloned()
            .unwrap_or_else(|| auth_localhost_url.clone());
        if caddy_installed {
            let port = crate::caddy_https_port(config);
            let parsed = url::Url::parse(&external_url).map_err(|e| {
                Error::Template(format!("invalid auth provider URL '{external_url}': {e}"))
            })?;
            if parsed.port().is_none() {
                external_url = format!("{external_url}:{port}");
            }
        }
        // auth.internal_url — how containers reach the auth provider for OIDC
        // discovery and token exchange (server-to-server calls).
        //
        // Equal to external_url: both go through Caddy with HTTPS because
        // authelia requires X-Forwarded-Proto/Host headers for OIDC discovery,
        // which only Caddy provides. Services containers resolve the
        // .localhost domain to Caddy's IP via the `<authelia>:alias=<domain>`
        // podman network entry (see caddy::ensure_auth_provider_routed) and
        // trust Caddy's self-signed CA via the mounted CA bundle.
        //
        // `--auth` requires Caddy (auth_bridge::build returns None otherwise,
        // so the CA bundle and host-resolve helpers aren't generated and the
        // service's OIDC flow won't work end-to-end without Caddy).
        let internal_url = external_url.clone();
        ctx.insert("auth.url".into(), auth_localhost_url.clone());
        ctx.insert("auth.internal_url".into(), internal_url.clone());
        ctx.insert("auth.provider".into(), auth.provider_name().to_string());
        ctx.insert("auth.external_url".into(), external_url.clone());

        // OIDC issuer URL — must match authelia's discovery response.
        let issuer = match auth {
            crate::config::schema::AuthCredentials::Authelia { .. } => external_url.clone(),
            crate::config::schema::AuthCredentials::External { .. } => auth_localhost_url.clone(),
        };
        ctx.insert("auth.issuer".into(), issuer);
        ctx.insert(
            "auth.client_id".into(),
            secret::generate(&EnvFormat::Uuid, None),
        );
        ctx.insert(
            "auth.client_secret".into(),
            secret::generate(&EnvFormat::String, Some(64)),
        );
    }

    // services.* — cross-service references from installed services
    for installed in &config.services {
        let name = &installed.name;

        // services.<name>.port.<port_name> — from stored port mappings
        for (port_name, port) in &installed.ports {
            ctx.insert(
                format!("services.{name}.port.{port_name}"),
                port.to_string(),
            );
        }

        // services.<name>.env.<VAR> — read from the service's .env file.
        // A missing .env is fine (service was just recorded, files not yet written);
        // any other read error must propagate so we don't silently miss cross-service
        // template references like {{services.postgres.env.POSTGRES_PASSWORD}}.
        let env_file = crate::service_home(name)?.join(".env");
        let content = match std::fs::read_to_string(&env_file) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::FileRead {
                    path: env_file,
                    source,
                });
            }
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = line.split_once('=') {
                ctx.insert(format!("services.{name}.env.{key}"), val.to_string());
            }
        }
    }

    // secret.* — generate fresh values using the env var's format + length.
    // Three passes:
    // 1. Generate secrets that JWT signing keys depend on (e.g., jwt_secret)
    // 2. Generate JWT secrets (which reference signing key secrets)
    // 3. Generate remaining non-JWT secrets

    // Pass 1: collect signing key names and generate them first
    let jwt_signing_keys: Vec<String> = service_def
        .env
        .iter()
        .filter(|e| e.format == EnvFormat::JwtHs256)
        .filter_map(|e| e.jwt_signing_key.clone())
        .collect();
    for env in &service_def.env {
        if env.format == EnvFormat::JwtHs256 {
            continue;
        }
        for secret_name in crate::generate::extract_secret_refs(&env.value) {
            if jwt_signing_keys.contains(&secret_name) {
                let key = format!("secret.{secret_name}");
                ctx.entry(key)
                    .or_insert_with(|| secret::generate(&env.format, env.length));
            }
        }
    }

    // Pass 2: generate JWT secrets using the signing keys
    for env in &service_def.env {
        if env.format != EnvFormat::JwtHs256 {
            continue;
        }
        if let (Some(claims), Some(signing_key_name)) = (&env.jwt_claims, &env.jwt_signing_key) {
            let signing_key_ref = format!("secret.{signing_key_name}");
            let signing_key = ctx.get(&signing_key_ref).cloned().ok_or_else(|| {
                Error::Template(format!(
                    "JWT signing key '{signing_key_name}' not found in context — \
                     the referenced secret must be declared by a non-JWT env var in \
                     service.toml before the JWT env var that signs with it"
                ))
            })?;
            for secret_name in crate::generate::extract_secret_refs(&env.value) {
                let key = format!("secret.{secret_name}");
                ctx.entry(key)
                    .or_insert_with(|| secret::generate_jwt_hs256(&signing_key, claims));
            }
        }
    }

    // Pass 3: generate remaining secrets
    for env in &service_def.env {
        if env.format == EnvFormat::JwtHs256 {
            continue;
        }
        for secret_name in crate::generate::extract_secret_refs(&env.value) {
            let key = format!("secret.{secret_name}");
            ctx.entry(key)
                .or_insert_with(|| secret::generate(&env.format, env.length));
        }
    }

    Ok(ctx)
}
