use std::collections::BTreeMap;
use std::path::Path;

use crate::config::schema::{AuthCredentials, Config};
use crate::generate::GeneratedFile;
use crate::registry::service_def::ServiceDef;
use crate::{SERVICE_AUTHELIA, SERVICE_CADDY, Step, service_home};

/// Register an OIDC client with authelia by editing its configuration.yml.
/// Also ensures caddy has a network alias for the auth domain so OIDC
/// discovery (which must go through Caddy for correct issuer URLs) works
/// from service containers.
/// Returns steps to write the updated config and restart authelia.
pub fn register_oidc_client(
    service_name: &str,
    service_def: &ServiceDef,
    domain: Option<&str>,
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

    let base_url = match domain {
        Some(d) => format!("https://{d}:8443"),
        None => match ctx.get("service.url") {
            Some(url) if !url.is_empty() => url.clone(),
            _ => return steps, // no domain or service URL — cannot register OIDC client
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
    for suffix in [
        "/user/oauth2/Authelia/callback",              // Forgejo/Gitea
        "/auth/login",                                 // Immich
        "/oauth/oidc/callback",                        // Open WebUI
        "/accounts/oidc/authelia/login/callback/",      // Paperless-ngx (django-allauth)
        "/auth/openid/authelia",                       // Vikunja
        "/oauth2/callback",                            // generic
    ] {
        let uri = format!("{base_url}{suffix}");
        if !redirect_uris.contains(&uri) {
            redirect_uris.push(uri);
        }
    }
    let redirect_uris_yaml: String = redirect_uris
        .iter()
        .map(|u| format!("\n          - '{u}'"))
        .collect();
    let client_block = format!(
        "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        client_secret: '{client_secret}'\n        token_endpoint_auth_method: 'client_secret_post'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n          - 'groups'\n        authorization_policy: 'one_factor'"
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

    // OIDC discovery must go through Caddy so authelia returns browser-reachable
    // endpoints (authelia uses the request Host header as its issuer). Add the
    // auth domain as a network alias on caddy's container so OIDC services on
    // the caddy network can resolve it.
    let auth_domain = config
        .services
        .iter()
        .find(|s| s.name == SERVICE_AUTHELIA)
        .and_then(|s| s.domain.as_ref());
    if let Some(auth_domain) = auth_domain {
        let caddy_quadlet = quadlet_dir.join("caddy.container");
        if let Ok(content) = std::fs::read_to_string(&caddy_quadlet) {
            let alias = format!("Network=caddy.network:alias={auth_domain}");
            if !content.contains(&alias) {
                let updated: String = content
                    .lines()
                    .map(|line| {
                        if line == "Network=caddy.network"
                            || line.starts_with("Network=caddy.network:")
                        {
                            alias.clone()
                        } else {
                            line.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                if updated != content {
                    steps.push(Step::WriteFile(GeneratedFile {
                        path: caddy_quadlet,
                        content: updated,
                    }));
                    steps.push(Step::DaemonReload);
                    steps.push(Step::RestartService {
                        unit: SERVICE_CADDY.to_string(),
                    });
                }
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
