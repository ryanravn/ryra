use std::collections::BTreeMap;

use crate::config::schema::Config;
use crate::registry::service_def::{AuthKind, EnvFormat, ServiceDef};
use crate::system::secret;

/// Build the template context for rendering env var values.
/// Secrets are generated fresh using each env var's format + length.
pub fn build_context(
    config: &Config,
    service_def: &ServiceDef,
    host_port: Option<u16>,
    auth_kind: Option<&AuthKind>,
    url: Option<&str>,
) -> BTreeMap<String, String> {
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
        // Parse the URL to extract scheme and hostname
        let scheme = url.split("://").next().unwrap_or("https");
        let authority = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or(url);
        // service.domain is just the hostname (no port)
        let domain = authority.split(':').next().unwrap_or(authority);
        ctx.insert("service.domain".into(), domain.to_string());
        // service.external_url — browser-accessible URL as provided by the user.
        ctx.insert("service.external_url".into(), url.to_string());
        ctx.insert("service.scheme".into(), scheme.to_string());
    } else {
        ctx.insert("service.scheme".into(), "http".into());
    }
    // service.external_url falls back to service.url when no url is set.
    ctx.entry("service.external_url".into()).or_insert(localhost_url.clone());
    // Ensure service.url is always set (even when external url overrides it)
    ctx.entry("service.url".into()).or_insert(localhost_url);

    // admin.*
    if let Some(email) = &config.admin_email {
        ctx.insert("admin.email".into(), email.clone());
    }

    // smtp.*
    if let Some(smtp) = &config.smtp {
        ctx.insert("smtp.host".into(), smtp.host.clone());
        ctx.insert("smtp.port".into(), smtp.port.to_string());
        ctx.insert("smtp.username".into(), smtp.username.clone());
        ctx.insert("smtp.password".into(), smtp.password.clone());
        ctx.insert("smtp.from".into(), smtp.from.clone());
    }

    // auth.* — per-service OIDC credentials (when user chose to enable auth)
    if let (Some(_), Some(auth)) = (auth_kind, &config.auth) {
        let auth_localhost_url = auth.url().to_string();
        let caddy_installed = config.services.iter().any(|s| s.name == "caddy" && s.installed);
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
            let caddy_https_port = config
                .services
                .iter()
                .find(|s| s.name == "caddy")
                .and_then(|s| s.ports.get("https").copied());
            if let Some(port) = caddy_https_port {
                let has_port = external_url
                    .split("://")
                    .nth(1)
                    .and_then(|rest| rest.split('/').next())
                    .map(|authority| authority.contains(':'))
                    .unwrap_or(false);
                if !has_port {
                    external_url = format!("{external_url}:{port}");
                }
            }
        }
        // auth.internal_url — how containers reach the auth provider for OIDC
        // discovery and token exchange.
        //
        // When Caddy is installed, this equals auth.external_url because OIDC
        // clients validate that the configured URL matches the issuer in the
        // discovery response. Containers reach Caddy via a podman network alias
        // matching the auth domain.
        //
        // Without Caddy, containers talk directly to the auth provider.
        let internal_url = if caddy_installed {
            external_url.clone()
        } else {
            match auth.port() {
                Some(port) => format!("http://{}:{port}", auth.provider_name()),
                None => auth_localhost_url.clone(),
            }
        };
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

        // services.<name>.env.<VAR> — read from the service's .env file
        let Ok(service_dir) = crate::service_home(name) else {
            continue;
        };
        let env_file = service_dir.join(".env");
        if let Ok(content) = std::fs::read_to_string(&env_file) {
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
            let signing_key = ctx
                .get(&format!("secret.{signing_key_name}"))
                .cloned()
                .unwrap_or_default();
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

    ctx
}
