use std::collections::BTreeMap;

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::exposure::Exposure;
use crate::registry::service_def::{AuthKind, EnvFormat, ServiceDef};
use crate::system::secret;

/// Build the template context for rendering env var values.
/// Secrets are generated fresh using each env var's format + length.
///
/// Takes the resolved [`Exposure`] rather than a raw URL so the boundary
/// can't be fed a URL that disagrees with the exposure decision.
///
/// Returns an error if any provided URL (the exposure's or the stored
/// auth-provider URL) fails to parse or is missing a host — template
/// rendering downstream depends on those being well-formed.
pub fn build_context(
    config: &Config,
    service_def: &ServiceDef,
    host_port: Option<u16>,
    auth_kind: Option<&AuthKind>,
    exposure: &Exposure,
    enable_smtp: bool,
) -> Result<BTreeMap<String, String>> {
    let url: Option<&str> = exposure.url();
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
        // service.external_authority — `host` or `host:port`, matching what
        // appears in the URL. Services like Nextcloud whose Host-header
        // overrides don't carry the port need this so generated redirect
        // URIs retain `:8443` and match authelia's registered values.
        let authority = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        ctx.insert("service.external_authority".into(), authority);
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

    // (No tls.* template namespace anymore — TLS provider isn't a global
    // config knob. Service URLs are inspected directly when needed; if a
    // template ever needs to know "is this Caddy local?", it can match on
    // the URL hostname against `CADDY_LOCAL_DOMAIN`.)

    // auth.* — per-service OIDC credentials (when user chose to enable auth)
    if let (Some(_), Some(auth)) = (auth_kind, &config.auth) {
        let auth_base_url = auth.url().to_string();
        let installed_for_auth = crate::list_installed().unwrap_or_default();
        let caddy_installed = crate::is_service_installed("caddy");
        // auth.external_url — browser-accessible URL.
        // Uses the stored URL from the auth provider's installed record if available.
        // When Caddy is installed, ensures the URL includes Caddy's HTTPS port
        // so it matches the issuer in authelia's OIDC discovery response.
        let mut external_url = installed_for_auth
            .iter()
            .find(|s| s.name == auth.provider_name())
            .and_then(|s| s.exposure.url())
            .map(|u| u.to_string())
            .unwrap_or_else(|| auth_base_url.clone());
        if caddy_installed {
            let port = crate::caddy_https_port(config);
            external_url = with_caddy_port(&external_url, port)?;
        }
        // auth.internal_url — how containers reach the auth provider for OIDC
        // discovery and token exchange (server-to-server calls).
        //
        // Equal to external_url: both go through Caddy with HTTPS because
        // authelia requires X-Forwarded-Proto/Host headers for OIDC discovery,
        // which only Caddy provides. Service containers resolve the auth
        // domain to Caddy's IP via the `<authelia>:alias=<domain>` podman
        // network entry (see caddy::ensure_auth_provider_routed) and trust
        // Caddy's self-signed CA via the mounted CA bundle.
        //
        // `--auth` runs through Caddy as the *internal* TLS terminator —
        // auth_bridge::build inspects authelia's URL hostname and only
        // builds the bridge for `*.internal` URLs (Caddy local). Other
        // hostnames imply the user is running their own TLS path (Tailscale
        // serve, external proxy) and ryra returns no bridge: the CA bundle
        // and host-resolve helpers aren't generated, and runtime OIDC
        // across containers is out of scope until someone wires user-cert
        // mounting in.
        let internal_url = external_url.clone();
        ctx.insert("auth.url".into(), auth_base_url.clone());
        ctx.insert("auth.internal_url".into(), internal_url.clone());
        ctx.insert("auth.provider".into(), auth.provider_name().to_string());
        ctx.insert("auth.external_url".into(), external_url.clone());

        // OIDC issuer URL — must match authelia's discovery response.
        let issuer = match auth {
            crate::config::schema::AuthCredentials::Authelia { .. } => external_url.clone(),
            crate::config::schema::AuthCredentials::External { .. } => auth_base_url.clone(),
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

    // services.* — cross-service references from installed services.
    // Sourced from the quadlet directory (the source of truth for
    // installed services + their wiring), not preferences.toml.
    let _ = config; // formerly read installed list from here; now via scan
    for installed in crate::list_installed().unwrap_or_default() {
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

/// Ensure `url` reflects Caddy's HTTPS port without double-appending
/// when the URL already includes (or implies) it. `Url::port()` returns
/// `None` for the scheme's default port (443 for https, 80 for http), so
/// a naive `if parsed.port().is_none() { url.push_str(":port") }` check
/// produces `https://host:443:443/...` whenever Caddy is on 443 — which
/// is what low-ports mode does. Comparing `port_or_known_default()`
/// against the target port covers both the explicit and implicit cases.
fn with_caddy_port(url: &str, caddy_port: u16) -> Result<String> {
    let mut parsed = url::Url::parse(url)
        .map_err(|e| Error::Template(format!("invalid auth provider URL '{url}': {e}")))?;
    if parsed.port_or_known_default() == Some(caddy_port) {
        return Ok(url.to_string());
    }
    parsed.set_port(Some(caddy_port)).map_err(|_| {
        Error::Template(format!(
            "auth provider URL '{url}' is not a base URL — can't set port"
        ))
    })?;
    let mut s = parsed.to_string();
    // url::Url adds a trailing slash to bare-host URLs after a
    // round-trip; trim it so downstream `format!("{}/foo", url)` calls
    // don't produce `…:443//foo`.
    if s.ends_with('/') {
        s.pop();
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_caddy_port_already_explicit_default() {
        // Regression: low-ports mode (Caddy on 443) used to render
        // `https://authelia.internal:443:443/...` because url::Url::port()
        // reports `None` for default ports. Fix compares
        // `port_or_known_default()` instead.
        let out = with_caddy_port("https://authelia.internal:443", 443).unwrap();
        assert_eq!(out, "https://authelia.internal:443");
    }

    #[test]
    fn with_caddy_port_no_port_needs_default() {
        let out = with_caddy_port("https://authelia.internal", 443).unwrap();
        // Either form is fine — url::Url normalizes default ports out of
        // the string. What matters is no `:443:443`.
        assert!(out == "https://authelia.internal" || out == "https://authelia.internal:443");
    }

    #[test]
    fn with_caddy_port_replaces_mismatched_port() {
        // Caddy moved to a non-default port (e.g. high-port mode 8443):
        // the URL should be rewritten to match, even if it had a different
        // port baked in already.
        let out = with_caddy_port("https://authelia.internal:8443", 9443).unwrap();
        assert_eq!(out, "https://authelia.internal:9443");
    }

    #[test]
    fn with_caddy_port_appends_high_port_to_bare_host() {
        let out = with_caddy_port("https://authelia.internal", 8443).unwrap();
        assert_eq!(out, "https://authelia.internal:8443");
    }
}
