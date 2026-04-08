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
    domain: Option<&str>,
) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();

    // service.*
    ctx.insert("service.name".into(), service_def.service.name.clone());
    if let Some(port) = host_port {
        ctx.insert("service.port".into(), port.to_string());
    }
    // service.url — always localhost-based now
    let url = match host_port {
        Some(port) => format!("http://localhost:{port}"),
        None => "http://localhost".to_string(),
    };
    ctx.insert("service.url".into(), url);
    if let Some(domain) = domain {
        ctx.insert("service.domain".into(), domain.to_string());
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
        let url = auth.url().to_string();
        // auth.internal_url is how containers reach the auth provider
        // (via container DNS name on caddy's shared network).
        let internal_url = match auth.port() {
            Some(port) => format!("http://systemd-{}:{port}", auth.provider_name()),
            None => url.clone(),
        };
        ctx.insert("auth.url".into(), url.clone());
        ctx.insert("auth.internal_url".into(), internal_url.clone());
        ctx.insert("auth.provider".into(), auth.provider_name().to_string());

        // auth.external_url — browser-accessible URL via Caddy (HTTPS with domain)
        let external_url = config
            .services
            .iter()
            .find(|s| s.name == auth.provider_name())
            .and_then(|s| s.domain.as_ref())
            .map(|d| format!("https://{d}:8443"))
            .unwrap_or_else(|| url.clone());
        ctx.insert("auth.external_url".into(), external_url.clone());

        // OIDC issuer URL — must be browser-reachable so authorization redirects work.
        // Containers reach authelia via --add-host + Caddy CA cert when a domain is set.
        let issuer = match auth {
            crate::config::schema::AuthCredentials::Authelia { .. } => {
                // Use external URL (domain-based) so browsers can follow OIDC redirects.
                // Falls back to localhost URL when no domain is configured.
                external_url.clone()
            }
            crate::config::schema::AuthCredentials::External { .. } => url.clone(),
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
        let env_file = crate::service_home(name).join(".env");
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
    for env in &service_def.env {
        for secret_name in crate::generate::extract_secret_refs(&env.value) {
            let key = format!("secret.{secret_name}");
            ctx.entry(key)
                .or_insert_with(|| secret::generate(&env.format, env.length));
        }
    }

    ctx
}
