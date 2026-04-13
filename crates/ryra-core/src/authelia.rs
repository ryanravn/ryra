use std::collections::BTreeMap;
use std::path::Path;

use crate::config::schema::{AuthCredentials, Config};
use crate::generate::GeneratedFile;
use crate::generate::bundle::inject_networks;
use crate::registry::service_def::ServiceDef;
use crate::{SERVICE_AUTHELIA, Step, service_home};

/// Register an OIDC client with authelia by editing its configuration.yml.
/// Returns steps to write the updated config and restart authelia.
///
/// Also ensures Caddy joins authelia's network with a domain alias so that
/// OIDC discovery requests from service containers route through Caddy
/// (which sets proper X-Forwarded-Proto/Host headers).
pub fn register_oidc_client(
    service_name: &str,
    service_def: &ServiceDef,
    url: Option<&str>,
    ctx: &BTreeMap<String, String>,
    config: &Config,
    quadlet_dir: &Path,
) -> Vec<Step> {
    let mut steps = Vec::new();

    let client_id = match ctx.get("auth.client_id") {
        Some(id) => id.clone(),
        None => return steps,
    };
    let client_secret = match ctx.get("auth.client_secret") {
        Some(s) => s.clone(),
        None => return steps,
    };

    let Ok(authelia_home) = service_home(SERVICE_AUTHELIA) else {
        return steps;
    };
    let authelia_config_dir = authelia_home.join("config");
    let authelia_config_path = authelia_config_dir.join("configuration.yml");

    // RSA key generation is handled by authelia's pre_start hook.

    // Add OIDC section + client to authelia config
    if !authelia_config_path.exists() {
        return steps;
    }
    let Ok(mut yaml) = std::fs::read_to_string(&authelia_config_path) else {
        return steps;
    };

    let base_url = match url {
        Some(u) => u.to_string(),
        None => match ctx.get("service.url") {
            Some(u) if !u.is_empty() => u.clone(),
            _ => return steps, // no url or service URL — cannot register OIDC client
        },
    };

    // Build redirect_uris from mappings and common callback paths
    let redirect_url_from_mappings = service_def
        .mappings
        .auth
        .get("OAUTH_REDIRECT_URL")
        .map(|v| {
            v.replace("{{service.url}}", &base_url)
                .replace("{{service.external_url}}", &base_url)
        });
    let mut redirect_uris = Vec::new();
    if let Some(ref url) = redirect_url_from_mappings {
        redirect_uris.push(url.clone());
    }
    // Also register localhost-based URIs (users may access via either
    // 127.0.0.1 or localhost, and the browser address determines the
    // redirect_uri that vikunja sends to authelia).
    let mut base_urls = vec![base_url.clone()];
    if base_url.contains("127.0.0.1") {
        base_urls.push(base_url.replace("127.0.0.1", "localhost"));
    } else if base_url.contains("localhost") {
        base_urls.push(base_url.replace("localhost", "127.0.0.1"));
    }
    for suffix in [
        "/user/oauth2/Authelia/callback",              // Forgejo/Gitea
        "/auth/login",                                 // Immich
        "/oauth/oidc/callback",                        // Open WebUI
        "/accounts/oidc/authelia/login/callback/",      // Paperless-ngx (django-allauth)
        "/auth/openid/authelia",                       // Vikunja
        "/oauth2/callback",                            // generic
    ] {
        for base in &base_urls {
            let uri = format!("{base}{suffix}");
            if !redirect_uris.contains(&uri) {
                redirect_uris.push(uri);
            }
        }
    }
    let redirect_uris_yaml: String = redirect_uris
        .iter()
        .map(|u| format!("\n          - '{u}'"))
        .collect();
    let token_auth_method = &service_def.integrations.token_auth_method;
    let client_block = format!(
        "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        client_secret: '{client_secret}'\n        token_endpoint_auth_method: '{token_auth_method}'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n          - 'groups'\n        authorization_policy: 'one_factor'"
    );

    if !yaml.contains("identity_providers:") {
        yaml.push_str(&format!(
            "\nidentity_providers:\n  oidc:\n    jwks:\n      - key_id: 'main'\n        algorithm: 'RS256'\n        use: 'sig'\n        key: {{{{ secret \"/config/oidc.jwk.rsa.pem\" | mindent 10 \"|\" | msquote }}}}\n    clients:{client_block}\n",
        ));
    } else if !yaml.contains(&client_id) {
        yaml = yaml.replace("    clients:", &format!("    clients:{client_block}"));
    }

    steps.push(Step::WriteFile(GeneratedFile {
        path: authelia_config_path,
        content: yaml,
    }));

    steps.push(Step::RestartService {
        unit: SERVICE_AUTHELIA.to_string(),
    });

    // Ensure Caddy joins authelia's network with a domain alias so that
    // containers can resolve the auth FQDN → Caddy → authelia (with proper
    // X-Forwarded-Proto: https headers that authelia requires for OIDC).
    let caddy_installed = config.services.iter().any(|s| s.name == "caddy" && s.installed);
    let auth_domain = config
        .services
        .iter()
        .find(|s| s.name == SERVICE_AUTHELIA)
        .and_then(|s| s.url.as_ref())
        .and_then(|u| {
            u.split("://")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .map(|authority| authority.split(':').next().unwrap_or(authority))
        })
        .map(|s| s.to_string());

    if caddy_installed {
        if let Some(ref domain) = auth_domain {
            let mut need_caddy_restart = false;

            // 1. Add Caddy to authelia's podman network with a domain alias so
            // that containers resolving the auth domain reach Caddy (HTTPS on
            // port 8443). OIDC clients require the issuer URL to match exactly,
            // so services connect to the external URL (https://domain:8443)
            // which routes through Caddy's TLS termination to authelia.
            let caddy_quadlet = quadlet_dir.join("caddy.container");
            if let Ok(content) = std::fs::read_to_string(&caddy_quadlet) {
                let network_spec = format!("{SERVICE_AUTHELIA}:alias={domain}");
                if !content.contains(&format!("alias={domain}")) {
                    let updated = inject_networks(&content, &[network_spec]);
                    steps.push(Step::WriteFile(GeneratedFile {
                        path: caddy_quadlet,
                        content: updated,
                    }));
                    need_caddy_restart = true;
                }
            }

            // 2. Add an authelia site block to the Caddyfile so Caddy
            //    terminates TLS and reverse-proxies to authelia. Without this,
            //    Caddy has no route for the auth domain and returns 404.
            if let Ok(caddyfile_path) = crate::caddy::caddyfile_path() {
                if let Ok(caddyfile) = std::fs::read_to_string(&caddyfile_path) {
                    if !caddyfile.contains(&format!("# ryra:{SERVICE_AUTHELIA}")) {
                        let block = crate::caddy::render_site_block(
                            &crate::caddy::CaddySiteParams {
                                service_name: SERVICE_AUTHELIA.to_string(),
                                domain: domain.clone(),
                                container_port: 9091,
                            },
                        );
                        let updated =
                            crate::caddy::add_route(&caddyfile, SERVICE_AUTHELIA, &block);
                        steps.push(Step::WriteFile(GeneratedFile {
                            path: caddyfile_path,
                            content: updated,
                        }));
                        need_caddy_restart = true;
                    }
                }
            }

            if need_caddy_restart {
                steps.push(Step::DaemonReload);
                steps.push(Step::RestartService {
                    unit: "caddy".to_string(),
                });
            }
        }
    }

    steps
}

/// Build AuthCredentials for finalize_add when authelia is installed.
pub fn auth_config(allocated_ports: &[(String, u16)]) -> crate::error::Result<AuthCredentials> {
    let port = allocated_ports
        .iter()
        .find(|(name, _)| name == "http")
        .map(|(_, p)| *p)
        .ok_or_else(|| {
            crate::error::Error::Registry(
                "authelia has no 'http' port allocated — cannot configure auth".into(),
            )
        })?;
    let url = format!("http://localhost:{port}");
    Ok(AuthCredentials::Authelia { url, port })
}
