use std::collections::BTreeMap;
use std::path::Path;

use crate::config::schema::{AuthCredentials, Config};
use crate::error::Error;
use crate::generate::GeneratedFile;
use crate::generate::bundle::inject_networks;
use crate::registry::service_def::ServiceDef;
use crate::{Step, WellKnownService, service_home};

/// Replace the host portion of a URL while preserving scheme, port, and path.
fn url_with_host(base_url: &str, new_host: &str) -> Option<String> {
    let mut parsed = url::Url::parse(base_url).ok()?;
    parsed.set_host(Some(new_host)).ok()?;
    let mut result = parsed.to_string();
    // url::Url always adds a trailing slash; strip it if the original didn't have one
    if !base_url.ends_with('/') && result.ends_with('/') {
        result.pop();
    }
    Some(result)
}

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
) -> crate::error::Result<Vec<Step>> {
    let mut steps = Vec::new();

    let client_id = match ctx.get("auth.client_id") {
        Some(id) => id.clone(),
        None => {
            return Err(Error::AuthContext(
                "auth.client_id not found in template context".into(),
            ));
        }
    };
    let client_secret = match ctx.get("auth.client_secret") {
        Some(s) => s.clone(),
        None => {
            return Err(Error::AuthContext(
                "auth.client_secret not found in template context".into(),
            ));
        }
    };

    let authelia_home = service_home(WellKnownService::Authelia.as_str())?;
    let authelia_config_dir = authelia_home.join("config");
    let authelia_config_path = authelia_config_dir.join("configuration.yml");

    // RSA key generation is handled by authelia's pre_start hook.

    // Add OIDC section + client to authelia config
    if !authelia_config_path.exists() {
        return Err(Error::FileRead {
            path: authelia_config_path,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "authelia configuration.yml not found",
            ),
        });
    }
    let mut yaml = std::fs::read_to_string(&authelia_config_path).map_err(|e| Error::FileRead {
        path: authelia_config_path.clone(),
        source: e,
    })?;

    let base_url = match url {
        Some(u) => u.to_string(),
        None => match ctx.get("service.url") {
            Some(u) if !u.is_empty() => u.clone(),
            _ => {
                return Err(Error::AuthContext(
                    "no URL provided and no service.url in template context — cannot register OIDC client".into(),
                ))
            }
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
    // Also register the localhost/127.0.0.1 alternate so browser redirects work from either address
    if let Some(alt) = url_with_host(&base_url, "localhost")
        && alt != base_url
        && !base_urls.contains(&alt)
    {
        base_urls.push(alt);
    }
    if let Some(alt) = url_with_host(&base_url, "127.0.0.1")
        && alt != base_url
        && !base_urls.contains(&alt)
    {
        base_urls.push(alt);
    }
    let callbacks = if service_def.integrations.oidc_callbacks.is_empty() {
        // Fallback for services that haven't declared their callbacks yet.
        vec!["/oauth2/callback".to_string()]
    } else {
        service_def.integrations.oidc_callbacks.clone()
    };
    for suffix in &callbacks {
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
    let token_auth_method = service_def.integrations.token_auth_method.as_str();
    let client_block = format!(
        "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        client_secret: '{client_secret}'\n        token_endpoint_auth_method: '{token_auth_method}'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n          - 'groups'\n        authorization_policy: 'one_factor'"
    );

    if !yaml.contains("identity_providers:") {
        yaml.push_str(&format!(
            "\nidentity_providers:\n  oidc:\n    jwks:\n      - key_id: 'main'\n        algorithm: 'RS256'\n        use: 'sig'\n        key: {{{{ secret \"/config/oidc.jwk.rsa.pem\" | mindent 10 \"|\" | msquote }}}}\n    clients:{client_block}\n",
        ));
    } else if !yaml.contains(&client_id) {
        // Find `clients:` under `identity_providers: > oidc:` specifically,
        // not just any `clients:` line in the file.
        let mut found = false;
        let mut in_identity_providers = false;
        let mut in_oidc = false;
        let mut insert_pos = None;
        for (i, line) in yaml.lines().enumerate() {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            if trimmed.starts_with("identity_providers:") {
                in_identity_providers = true;
            } else if in_identity_providers && indent == 2 && trimmed.starts_with("oidc:") {
                in_oidc = true;
            } else if in_identity_providers
                && in_oidc
                && indent == 4
                && trimmed.starts_with("clients:")
            {
                // Found the right `clients:` under identity_providers.oidc
                // Insert position is right after this line
                let byte_offset: usize = yaml.lines().take(i + 1).map(|l| l.len() + 1).sum();
                insert_pos = Some(byte_offset.min(yaml.len()));
                found = true;
                break;
            } else if in_oidc && indent <= 2 && !trimmed.is_empty() && !trimmed.starts_with('#') {
                // Left the oidc section without finding clients:
                break;
            }
        }
        if !found {
            return Err(Error::AuthContext(format!(
                "failed to inject OIDC client into authelia config — \
                 'clients:' under 'identity_providers.oidc' not found in {}",
                authelia_config_path.display()
            )));
        }
        if let Some(pos) = insert_pos {
            // client_block starts with \n but we're inserting after a line
            // that already ends with \n, so trim the leading newline.
            // Ensure the block ends with \n so existing content isn't concatenated.
            let block = client_block.strip_prefix('\n').unwrap_or(&client_block);
            let block = if block.ends_with('\n') {
                block.to_string()
            } else {
                format!("{block}\n")
            };
            yaml.insert_str(pos, &block);
        }
    }

    steps.push(Step::WriteFile(GeneratedFile {
        path: authelia_config_path,
        content: yaml,
    }));

    steps.push(Step::RestartService {
        unit: WellKnownService::Authelia.to_string(),
    });

    // Ensure Caddy joins authelia's network with a domain alias so that
    // containers can resolve the auth FQDN → Caddy → authelia (with proper
    // X-Forwarded-Proto: https headers that authelia requires for OIDC).
    let caddy_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
    let auth_domain = config
        .services
        .iter()
        .find(|s| WellKnownService::Authelia.matches(&s.name))
        .and_then(|s| s.url.as_ref())
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|parsed| parsed.host_str().map(|h| h.to_string()));

    if caddy_installed && let Some(ref domain) = auth_domain {
        let mut need_caddy_restart = false;
        let authelia = WellKnownService::Authelia;

        // 1. Add Caddy to authelia's podman network with a domain alias so
        // that containers resolving the auth domain reach Caddy (HTTPS on
        // port 8443). OIDC clients require the issuer URL to match exactly,
        // so services connect to the external URL (https://domain:8443)
        // which routes through Caddy's TLS termination to authelia.
        let caddy_quadlet = quadlet_dir.join("caddy.container");
        if let Ok(content) = std::fs::read_to_string(&caddy_quadlet) {
            let network_spec = format!("{authelia}:alias={domain}");
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
        if let Ok(caddyfile_path) = crate::caddy::caddyfile_path()
            && let Ok(caddyfile) = std::fs::read_to_string(&caddyfile_path)
            && !caddyfile.contains(&format!("# ryra:{authelia}"))
        {
            let block = crate::caddy::render_site_block(&crate::caddy::CaddySiteParams {
                service_name: authelia.to_string(),
                domain: domain.clone(),
                container_port: 9091,
                https_port: crate::caddy_https_port(config),
            });
            let updated = crate::caddy::add_route(&caddyfile, authelia.as_str(), &block);
            steps.push(Step::WriteFile(GeneratedFile {
                path: caddyfile_path,
                content: updated,
            }));
            need_caddy_restart = true;
        }

        if need_caddy_restart {
            steps.push(Step::DaemonReload);
            steps.push(Step::RestartService {
                unit: "caddy".to_string(),
            });
            // Wait for caddy's ExecStartPost to export the CA cert.
            // The cert is needed by add_service to create the merged CA bundle
            // for OIDC services. systemctl restart returns after ExecStart but
            // before ExecStartPost completes.
            let ca_path = crate::service_home("caddy")?
                .parent()
                .map(|p| p.join("caddy-root-ca.crt"))
                .unwrap_or_default();
            steps.push(Step::WaitForFile {
                path: ca_path,
                timeout_secs: 15,
            });
        }
    }

    Ok(steps)
}

/// Build AuthCredentials for finalize_add when authelia is installed.
pub fn auth_config(allocated_ports: &[(String, u16)]) -> crate::error::Result<AuthCredentials> {
    let port = allocated_ports
        .iter()
        .find(|(name, _)| name == "http")
        .map(|(_, p)| *p)
        .ok_or_else(|| {
            crate::error::Error::AuthContext(
                "authelia has no 'http' port allocated — cannot configure auth".into(),
            )
        })?;
    let url = format!("http://localhost:{port}");
    Ok(AuthCredentials::Authelia { url, port })
}
