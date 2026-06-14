use std::collections::BTreeMap;
use std::path::Path;

use crate::config::schema::{AuthCredentials, Config};
use crate::error::Error;
use crate::generate::GeneratedFile;
use crate::registry::service_def::ServiceDef;
use crate::{Step, WellKnownService, service_home};

/// Authelia's HTTP listener port: the container port, and the fallback
/// when an installed instance doesn't expose a port mapping.
const DEFAULT_HTTP_PORT: u16 = 9091;

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

/// Whether the managed auth provider's config currently registers an OIDC
/// client for `service_name`, matched by the `client_name` that
/// [`register_oidc_client`] writes. `None` when there's no authelia config
/// to read (provider not installed, config unreadable); callers treat that
/// as "can't tell, don't flag". The read-only counterpart to registration:
/// `ryra doctor` uses it to catch a service whose metadata says `auth = on`
/// but whose client vanished from the provider (e.g. a `ryra backup restore`
/// of authelia from a snapshot predating the registration).
pub fn oidc_client_registered(service_name: &str) -> Option<bool> {
    let config_path = service_home(WellKnownService::Authelia.as_str())
        .ok()?
        .join("config")
        .join("configuration.yml");
    let yaml = std::fs::read_to_string(&config_path).ok()?;
    Some(yaml.contains(&format!("client_name: '{service_name}'")))
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

    // Build redirect_uris from mappings and common callback paths. The
    // mapping value is a template; render it with service.url /
    // service.external_url pinned to the resolved base_url, because an
    // explicit `url` argument overrides whatever the context carries.
    let redirect_url_from_mappings = match service_def.mappings.auth.get("OAUTH_REDIRECT_URL") {
        Some(v) => {
            let mut render_ctx = ctx.clone();
            render_ctx.insert("service.url".into(), base_url.clone());
            render_ctx.insert("service.external_url".into(), base_url.clone());
            Some(crate::generate::template::render(v, &render_ctx)?)
        }
        None => None,
    };
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
    let client_block = if matches!(
        service_def.integrations.token_auth_method,
        crate::registry::service_def::TokenAuthMethod::None
    ) {
        // Public PKCE client — no client_secret, require_pkce enforced.
        // Authelia rejects client_secret on public clients so we must omit it.
        format!(
            "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        public: true\n        token_endpoint_auth_method: 'none'\n        require_pkce: true\n        pkce_challenge_method: 'S256'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n          - 'groups'\n        authorization_policy: 'one_factor'"
        )
    } else {
        format!(
            "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        client_secret: '{client_secret}'\n        token_endpoint_auth_method: '{token_auth_method}'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n          - 'groups'\n        authorization_policy: 'one_factor'"
        )
    };

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

    // Ensure Caddy routes requests for the authelia domain so that OIDC
    // discovery goes through Caddy's TLS termination (authelia needs the
    // X-Forwarded-Proto/Host headers for issuer validation).
    let installed = crate::list_installed().unwrap_or_default();
    if let Some(parsed) = installed
        .iter()
        .find(|s| WellKnownService::Authelia.matches(&s.name))
        .and_then(|s| s.exposure.url())
        .and_then(|u| url::Url::parse(u).ok())
        && let Some(domain) = parsed.host_str()
    {
        // The internal vhost listens on the issuer's port so the back-channel
        // matches authelia's discovery response: `.internal` carries Caddy's
        // HTTPS port (e.g. :8443); tailscale/public are port-less (:443).
        let issuer_port = parsed.port().unwrap_or(443);
        steps.extend(crate::caddy::ensure_auth_provider_routed(
            WellKnownService::Authelia,
            domain,
            DEFAULT_HTTP_PORT,
            issuer_port,
            quadlet_dir,
        )?);
    }

    Ok(steps)
}

/// Remove the OIDC client block registered for `service_name` from authelia's
/// configuration.yml and restart authelia so it picks up the change.
///
/// Returns an empty Vec when there is nothing to do — authelia isn't installed,
/// its config file is absent, or no block matching `service_name` exists.
/// Authelia being uninstalled is fine: the config is gone with it.
pub fn unregister_oidc_client(service_name: &str) -> crate::error::Result<Vec<Step>> {
    let authelia_config_path = service_home(WellKnownService::Authelia.as_str())?
        .join("config")
        .join("configuration.yml");
    if !authelia_config_path.exists() {
        return Ok(Vec::new());
    }
    let yaml = std::fs::read_to_string(&authelia_config_path).map_err(|e| Error::FileRead {
        path: authelia_config_path.clone(),
        source: e,
    })?;
    let updated = remove_client_block(&yaml, service_name);
    if updated == yaml {
        return Ok(Vec::new());
    }
    Ok(vec![
        Step::WriteFile(GeneratedFile {
            path: authelia_config_path,
            content: updated,
        }),
        Step::RestartService {
            unit: WellKnownService::Authelia.to_string(),
        },
    ])
}

/// Remove the OIDC client block for `service_name` from an authelia YAML.
///
/// Client blocks are written by [`register_oidc_client`] with a fixed shape:
/// a line `      - client_id: '...'` at indent 6, followed by sibling fields
/// at indent 8 (including `client_name: '<service_name>'`) and list items at
/// indent 10. A block ends at the next line with indent < 8 that isn't empty.
fn remove_client_block(yaml: &str, service_name: &str) -> String {
    let name_marker = format!("client_name: '{service_name}'");
    let lines: Vec<&str> = yaml.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("      - client_id:") {
            kept.push(lines[i]);
            i += 1;
            continue;
        }
        let block_start = i;
        let mut j = i + 1;
        while j < lines.len() {
            let line = lines[j];
            if line.is_empty() {
                j += 1;
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if indent >= 8 {
                j += 1;
            } else {
                break;
            }
        }
        let block = &lines[block_start..j];
        let matches = block.iter().any(|l| l.trim() == name_marker);
        if !matches {
            kept.extend_from_slice(block);
        }
        i = j;
    }
    let mut out = kept.join("\n");
    if yaml.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    // If removing this client left no `- client_id:` lines at all, strip the
    // whole `identity_providers:` block too. Authelia 4.39 rejects an OIDC
    // provider section with an empty `clients:` list on startup, so leaving
    // the skeleton behind would wedge the service on next restart.
    if !out.lines().any(|l| l.starts_with("      - client_id:")) {
        out = strip_identity_providers_block(&out);
    }
    out
}

/// Remove the `identity_providers:` top-level block and everything under it,
/// leaving surrounding content intact. Used when the last OIDC client is
/// unregistered — authelia won't accept an empty clients list.
fn strip_identity_providers_block(yaml: &str) -> String {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("identity_providers:")
            && !lines[i].starts_with(' ')
            && !lines[i].starts_with('\t')
        {
            // Skip this line and every subsequent indented line until the
            // next top-level key or EOF.
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if l.is_empty() || l.starts_with(' ') || l.starts_with('\t') {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    // Trim trailing empty lines (leftover whitespace from the stripped block)
    // but keep exactly one trailing newline to match the input convention.
    while kept.last().is_some_and(|l| l.is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if yaml.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Build AuthCredentials for finalize_add when authelia is installed.
///
/// `installed_url` is the URL authelia is actually reachable at — the
/// browser-visible one passed via `--url`/`--tailscale`, or `None` for
/// localhost-only deployments. It's what gets templated into every other
/// service's OIDC config as the issuer/discovery URL, so it MUST match
/// what authelia advertises (mismatched issuer URLs make OIDC token
/// validation fail). Falls back to `http://localhost:<port>` only when
/// no public URL was set.
pub fn auth_config(
    allocated_ports: &[(String, u16)],
    installed_url: Option<&str>,
) -> crate::error::Result<AuthCredentials> {
    let port = allocated_ports
        .iter()
        .find(|(name, _)| name == "http")
        .map(|(_, p)| *p)
        .ok_or_else(|| {
            crate::error::Error::AuthContext(
                "authelia has no 'http' port allocated — cannot configure auth".into(),
            )
        })?;
    let url = installed_url
        .map(String::from)
        .unwrap_or_else(|| format!("http://localhost:{port}"));
    Ok(AuthCredentials::Authelia { url, port })
}

/// Configure `config.auth` from an already-installed authelia instance —
/// the "authelia is up but `[auth]` is missing from preferences" case
/// (hand-edited config, or an earlier run that died between install and
/// finalize). Returns `true` when auth was configured and saved, `false`
/// when the install doesn't look usable (no readable `.env`).
pub fn configure_auth_from_installed(
    config: &mut Config,
    paths: &crate::config::ConfigPaths,
) -> crate::error::Result<bool> {
    // The .env is user-readable under ~/.local/share/services/authelia/.env.
    // Its presence (and non-emptiness) is the signal that the install
    // completed far enough to be adopted.
    let env_path = service_home(WellKnownService::Authelia.as_str())?.join(".env");
    let env_content = match std::fs::read_to_string(&env_path) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };
    if env_content.is_empty() {
        return Ok(false);
    }

    // Find the port from the quadlet-derived InstalledService view. Select
    // by name, matching `auth_config`'s rule for fresh installs. A failed
    // listing propagates rather than silently configuring port 9091.
    let installed_all = crate::list_installed()?;
    let port = crate::find_installed_provider(&installed_all, crate::Capability::OidcProvider)
        .and_then(|s| s.ports.get("http").copied())
        .unwrap_or(DEFAULT_HTTP_PORT);

    let url = format!("http://localhost:{port}");
    config.auth = Some(AuthCredentials::Authelia { url, port });
    paths.ensure_dirs()?;
    crate::config::save_config(&paths.config_file, config)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_client_yaml() -> String {
        // Matches the shape register_oidc_client writes: indent 6 for
        // `- client_id`, indent 8 for sibling fields, indent 10 for list items.
        [
            "identity_providers:",
            "  oidc:",
            "    jwks:",
            "      - key_id: 'main'",
            "    clients:",
            "      - client_id: 'uuid-forgejo'",
            "        client_name: 'forgejo'",
            "        client_secret: 'secret-forgejo'",
            "        redirect_uris:",
            "          - 'https://forgejo.example.com/callback'",
            "        scopes:",
            "          - 'openid'",
            "        authorization_policy: 'one_factor'",
            "      - client_id: 'uuid-immich'",
            "        client_name: 'immich'",
            "        client_secret: 'secret-immich'",
            "        redirect_uris:",
            "          - 'https://immich.example.com/callback'",
            "        scopes:",
            "          - 'openid'",
            "        authorization_policy: 'one_factor'",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn removes_matching_client_block() {
        let yaml = two_client_yaml();
        let out = remove_client_block(&yaml, "forgejo");
        assert!(!out.contains("forgejo"));
        assert!(out.contains("client_name: 'immich'"));
        assert!(out.contains("uuid-immich"));
    }

    #[test]
    fn preserves_surrounding_structure() {
        let yaml = two_client_yaml();
        let out = remove_client_block(&yaml, "forgejo");
        assert!(out.starts_with("identity_providers:\n"));
        assert!(out.contains("    clients:"));
        assert!(out.contains("    jwks:"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn no_match_is_a_noop() {
        let yaml = two_client_yaml();
        let out = remove_client_block(&yaml, "paperless");
        assert_eq!(out, yaml);
    }

    #[test]
    fn removes_only_matching_block_when_names_are_prefixes() {
        // 'im' should not match 'immich' — we match on the full quoted name.
        let yaml = two_client_yaml();
        let out = remove_client_block(&yaml, "im");
        assert_eq!(out, yaml);
    }

    fn single_client_yaml() -> String {
        [
            "notifier:",
            "  smtp:",
            "    address: 'smtp://inbucket:2500'",
            "access_control:",
            "  default_policy: 'one_factor'",
            "",
            "identity_providers:",
            "  oidc:",
            "    jwks:",
            "      - key_id: 'main'",
            "    clients:",
            "      - client_id: 'uuid-zammad'",
            "        client_name: 'zammad'",
            "        public: true",
            "        redirect_uris:",
            "          - 'http://127.0.0.1:10000/callback'",
            "        scopes:",
            "          - 'openid'",
            "        authorization_policy: 'one_factor'",
            "",
        ]
        .join("\n")
    }

    #[test]
    fn strips_identity_providers_block_when_last_client_is_removed() {
        // Regression: authelia 4.39 rejects an OIDC provider section with an
        // empty `clients:` list. Removing the last client must also strip the
        // whole identity_providers block.
        let yaml = single_client_yaml();
        let out = remove_client_block(&yaml, "zammad");
        assert!(!out.contains("identity_providers"));
        assert!(!out.contains("zammad"));
        assert!(!out.contains("clients:"));
        // Surrounding config must survive.
        assert!(out.contains("notifier:"));
        assert!(out.contains("access_control:"));
        assert!(out.ends_with('\n'));
    }
}
