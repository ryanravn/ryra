use std::path::{Path, PathBuf};

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::generate::GeneratedFile;
use crate::generate::bundle::inject_networks;
use crate::{Step, WellKnownService};

/// Path to the ryra-managed Caddyfile.
///
/// Lives inside the caddy service's data dir so the existing volume mount
/// (`%h/config` -> `/etc/caddy/`) picks it up automatically.
pub fn caddyfile_path() -> Result<PathBuf> {
    Ok(crate::service_home(WellKnownService::Caddy.as_str())?
        .join("config")
        .join("Caddyfile"))
}

/// Path to the user-owned TLS snippet.
///
/// Site blocks emit `import ryra_tls`; the actual TLS strategy lives here.
/// Default is `tls internal` (LAN mode); `ryra add caddy --acme <email>`
/// seeds it with `tls <email>` (Let's Encrypt). After first write, ryra
/// never touches this file — users edit it for Cloudflare DNS-01,
/// wildcards, BYO certs, or anything else Caddy supports.
pub fn tls_snippet_path() -> Result<PathBuf> {
    Ok(crate::service_home(WellKnownService::Caddy.as_str())?
        .join("config")
        .join("tls.caddy"))
}

/// What ryra writes into `tls.caddy` on first install of caddy.
///
/// Modeled as an exhaustive enum so callers can't smuggle a third
/// state via empty-string sentinels — `match` on this and the planner,
/// install message, and snippet renderer stay in agreement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeMode {
    /// Self-signed via Caddy's internal CA (LAN-friendly default).
    /// Browsers warn unless ryra's CA is trusted.
    Internal,
    /// Let's Encrypt with no registration email — real certs but no
    /// renewal-notice email tied to an account.
    Anonymous,
    /// Let's Encrypt with a registration email for renewal notices.
    WithEmail(String),
}

impl AcmeMode {
    /// Renders the `(ryra_tls) { … }` snippet body for this mode.
    pub fn snippet(&self) -> String {
        match self {
            AcmeMode::Internal => "(ryra_tls) {\n\ttls internal\n}\n".to_string(),
            // Empty body — sites that import this get no `tls` directive,
            // which makes Caddy auto-issue LE certs for any public hostname.
            AcmeMode::Anonymous => "(ryra_tls) {\n}\n".to_string(),
            AcmeMode::WithEmail(email) => format!("(ryra_tls) {{\n\ttls {email}\n}}\n"),
        }
    }
}

/// Ensure Caddy is set up to route requests for the auth provider.
///
/// Returns steps to (a) add Caddy to the auth provider's podman network with
/// a domain alias so service containers resolving the auth FQDN reach Caddy,
/// and (b) install a site block in the Caddyfile that terminates TLS and
/// reverse-proxies to the auth provider (which requires proper
/// X-Forwarded-Proto/Host headers for OIDC).
///
/// A no-op when Caddy isn't installed — returns an empty Vec.
pub fn ensure_auth_provider_routed(
    config: &Config,
    auth_service: WellKnownService,
    auth_domain: &str,
    auth_container_port: u16,
    quadlet_dir: &Path,
) -> Result<Vec<Step>> {
    let caddy_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
    if !caddy_installed {
        return Ok(Vec::new());
    }

    let mut steps = Vec::new();
    let mut need_caddy_restart = false;

    // Caddy is installed, so caddy.container must exist — a missing or
    // unreadable file here means something has gone wrong that we should
    // surface, not swallow (OIDC discovery silently breaks otherwise).
    let caddy_quadlet = quadlet_dir.join("caddy.container");
    let content = std::fs::read_to_string(&caddy_quadlet).map_err(|source| Error::FileRead {
        path: caddy_quadlet.clone(),
        source,
    })?;
    let network_spec = format!("{auth_service}:alias={auth_domain}");
    if !content.contains(&format!("alias={auth_domain}")) {
        let updated = inject_networks(&content, &[network_spec]);
        steps.push(Step::WriteFile(GeneratedFile {
            path: caddy_quadlet,
            content: updated,
        }));
        need_caddy_restart = true;
    }

    let caddyfile_path = caddyfile_path()?;
    let caddyfile = std::fs::read_to_string(&caddyfile_path).map_err(|source| Error::FileRead {
        path: caddyfile_path.clone(),
        source,
    })?;
    if !caddyfile.contains(&format!("# ryra:{auth_service}")) {
        let block = render_site_block(&CaddySiteParams {
            service_name: auth_service.to_string(),
            domain: auth_domain.to_string(),
            container_port: auth_container_port,
            https_port: crate::caddy_https_port(config),
        });
        let updated = add_route(&caddyfile, auth_service.as_str(), &block);
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
        // Wait for caddy's ExecStartPost to export the CA cert. The cert is
        // needed by add_service to create the merged CA bundle for OIDC
        // services. `systemctl restart` returns after ExecStart but before
        // ExecStartPost completes.
        let ca_path = crate::service_home("caddy")?
            .parent()
            .map(|p| p.join("caddy-root-ca.crt"))
            .unwrap_or_default();
        steps.push(Step::WaitForFile {
            path: ca_path,
            timeout_secs: 15,
        });
    }

    Ok(steps)
}

/// Parameters for generating a Caddy site block.
pub struct CaddySiteParams {
    pub service_name: String,
    pub domain: String,
    /// Container port the service listens on (used with container DNS name).
    pub container_port: u16,
    /// Caddy's HTTPS listen port (from the installed caddy service's port map).
    pub https_port: u16,
}

/// Generate a Caddy site block for a service.
///
/// The block always starts with a `# ryra:<service_name>` marker comment
/// so that [`add_route`] and [`remove_route`] can locate it.
///
/// TLS strategy is delegated to the user-owned `(ryra_tls)` snippet
/// imported at the top of the Caddyfile — see [`tls_snippet_path`].
/// Exception: `*.internal` hosts always use `tls internal` directly,
/// because Let's Encrypt can't issue certs for the reserved `.internal`
/// TLD even when the user has flipped the snippet to ACME mode.
pub fn render_site_block(params: &CaddySiteParams) -> String {
    let mut block = format!("# ryra:{}\n", params.service_name);
    block.push_str(&format!("{}:{} {{\n", params.domain, params.https_port));
    if params.domain.ends_with(".internal") {
        block.push_str("    tls internal\n");
    } else {
        block.push_str("    import ryra_tls\n");
    }
    // Use the container name on caddy's shared network for direct communication.
    block.push_str(&format!(
        "    reverse_proxy {}:{}\n",
        params.service_name, params.container_port
    ));
    block.push_str("}\n");
    block
}

/// Add or replace a service's block in the Caddyfile content.
///
/// If a block with `# ryra:<service_name>` already exists, it is replaced.
/// Otherwise the new block is appended.
pub fn add_route(caddyfile: &str, service_name: &str, block: &str) -> String {
    let cleaned = remove_route(caddyfile, service_name);
    let mut result = cleaned.trim_end().to_string();
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str(block);
    result.push('\n');
    result
}

/// Remove a service's block from the Caddyfile content.
///
/// Finds the `# ryra:<service_name>` marker and removes everything from
/// that line through the closing `}` at brace depth 0, using brace-depth
/// tracking to correctly handle nested blocks.
pub fn remove_route(caddyfile: &str, service_name: &str) -> String {
    let marker = format!("# ryra:{service_name}");
    let lines: Vec<&str> = caddyfile.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        if lines[i].trim() == marker {
            // Skip the marker line and the entire site block that follows.
            // Caddyfile blocks open with `domain {` and close with `}` alone
            // on a line. Track depth by looking at line-ending `{` and
            // line-starting `}` (the only valid positions in Caddyfile syntax).
            i += 1;
            let mut depth: i32 = 0;
            let mut entered_block = false;
            while i < lines.len() {
                let trimmed = lines[i].trim();
                if trimmed.ends_with('{') {
                    depth += 1;
                    entered_block = true;
                }
                if trimmed.starts_with('}') {
                    depth -= 1;
                }
                i += 1;
                if entered_block && depth <= 0 {
                    break;
                }
            }
            // Skip trailing empty lines after the removed block
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
        } else {
            result.push(lines[i]);
            i += 1;
        }
    }

    // Clean up trailing blank lines
    while result.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        result.pop();
    }

    let mut out = result.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Parse ryra-managed domains from a Caddyfile.
///
/// Returns `(service_name, domain)` pairs extracted from `# ryra:` markers.
pub fn parse_domains(caddyfile: &str) -> Vec<(String, String)> {
    let mut domains = Vec::new();
    let mut current_service: Option<String> = None;

    for line in caddyfile.lines() {
        let trimmed = line.trim();
        if let Some(svc) = trimmed.strip_prefix("# ryra:") {
            current_service = Some(svc.to_string());
        } else if let Some(ref svc) = current_service {
            // Next non-comment line after marker should be "domain.com {"
            if let Some(domain) = trimmed.strip_suffix('{') {
                let domain = domain.trim();
                if !domain.is_empty() {
                    domains.push((svc.clone(), domain.to_string()));
                }
                current_service = None;
            }
        }
    }

    domains
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_basic_block() {
        let params = CaddySiteParams {
            service_name: "whoami".to_string(),
            domain: "whoami.example.com".to_string(),
            container_port: 8080,
            https_port: 8443,
        };
        let block = render_site_block(&params);
        assert!(block.starts_with("# ryra:whoami\n"));
        assert!(block.contains("whoami.example.com:8443 {"));
        assert!(block.contains("    import ryra_tls\n"));
        assert!(!block.contains("tls internal"));
        assert!(block.contains("    reverse_proxy whoami:8080"));
        assert!(block.ends_with("}\n"));
    }

    #[test]
    fn render_internal_domain_keeps_tls_internal() {
        // *.internal hosts can't get LE certs, so they must bypass the
        // user-owned snippet (which may have been flipped to ACME mode).
        let params = CaddySiteParams {
            service_name: "authelia".to_string(),
            domain: "auth.internal".to_string(),
            container_port: 9091,
            https_port: 8443,
        };
        let block = render_site_block(&params);
        assert!(block.contains("    tls internal\n"));
        assert!(!block.contains("import ryra_tls"));
    }

    #[test]
    fn acme_mode_internal_snippet() {
        let s = AcmeMode::Internal.snippet();
        assert!(s.starts_with("(ryra_tls) {"));
        assert!(s.contains("tls internal"));
    }

    #[test]
    fn acme_mode_with_email_snippet() {
        let s = AcmeMode::WithEmail("admin@example.com".to_string()).snippet();
        assert!(s.starts_with("(ryra_tls) {"));
        assert!(s.contains("tls admin@example.com"));
        assert!(!s.contains("tls internal"));
    }

    #[test]
    fn acme_mode_anonymous_snippet_omits_tls_directive() {
        let s = AcmeMode::Anonymous.snippet();
        assert!(s.starts_with("(ryra_tls) {"));
        // No `tls` *directive* inside the body — Caddy auto-issues
        // anonymously when the snippet is empty. The "tls" inside
        // `ryra_tls` is the snippet name and must remain.
        assert!(!s.contains("\ttls "));
        assert!(!s.contains("tls internal"));
        assert!(!s.contains("tls @"));
    }

    #[test]
    fn render_block_custom_https_port() {
        let params = CaddySiteParams {
            service_name: "app".to_string(),
            domain: "app.example.com".to_string(),
            container_port: 3000,
            https_port: 9443,
        };
        let block = render_site_block(&params);
        assert!(block.contains("app.example.com:9443 {"));
    }

    #[test]
    fn add_route_to_empty() {
        let block = "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let result = add_route("", "whoami", block);
        assert_eq!(result, format!("{block}\n"));
    }

    #[test]
    fn add_route_appends() {
        let existing =
            "# ryra:foo\nfoo.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n";
        let block =
            "# ryra:bar\nbar.example.com {\n    reverse_proxy host.containers.internal:4000\n}\n";
        let result = add_route(existing, "bar", block);
        assert!(result.contains("# ryra:foo"));
        assert!(result.contains("# ryra:bar"));
    }

    #[test]
    fn add_route_replaces_existing() {
        let existing = "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let new_block = "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:9090\n}\n";
        let result = add_route(existing, "whoami", new_block);
        assert!(!result.contains("8080"));
        assert!(result.contains("9090"));
    }

    #[test]
    fn remove_route_single() {
        let caddyfile = "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let result = remove_route(caddyfile, "whoami");
        assert_eq!(result, "");
    }

    #[test]
    fn remove_route_preserves_others() {
        let caddyfile = concat!(
            "# ryra:foo\nfoo.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n\n",
            "# ryra:bar\nbar.example.com {\n    reverse_proxy host.containers.internal:4000\n}\n",
        );
        let result = remove_route(caddyfile, "foo");
        assert!(!result.contains("foo"));
        assert!(result.contains("# ryra:bar"));
        assert!(result.contains("reverse_proxy host.containers.internal:4000"));
    }

    #[test]
    fn remove_route_preserves_user_blocks() {
        let caddyfile = concat!(
            "mysite.example.com {\n    root * /var/www\n    file_server\n}\n\n",
            "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n",
        );
        let result = remove_route(caddyfile, "whoami");
        assert!(result.contains("mysite.example.com"));
        assert!(result.contains("file_server"));
        assert!(!result.contains("ryra:whoami"));
    }

    #[test]
    fn remove_route_with_nested_braces() {
        let caddyfile = concat!(
            "# ryra:grafana\n",
            "grafana.example.com {\n",
            "    forward_auth host.containers.internal:9091 {\n",
            "        uri /api/authz/forward-auth\n",
            "    }\n",
            "    reverse_proxy host.containers.internal:3000\n",
            "}\n",
        );
        let result = remove_route(caddyfile, "grafana");
        assert_eq!(result, "");
    }

    #[test]
    fn parse_domains_basic() {
        let caddyfile = concat!(
            "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n\n",
            "# ryra:grafana\ngrafana.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n",
        );
        let domains = parse_domains(caddyfile);
        assert_eq!(domains.len(), 2);
        assert_eq!(
            domains[0],
            ("whoami".to_string(), "whoami.example.com".to_string())
        );
        assert_eq!(
            domains[1],
            ("grafana".to_string(), "grafana.example.com".to_string())
        );
    }

    #[test]
    fn parse_domains_ignores_user_blocks() {
        let caddyfile = concat!(
            "mysite.example.com {\n    file_server\n}\n\n",
            "# ryra:whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n",
        );
        let domains = parse_domains(caddyfile);
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].0, "whoami");
    }

    #[test]
    fn caddyfile_path_is_under_service_home() {
        let path = caddyfile_path().expect("HOME should be set in test environment");
        assert!(
            path.ends_with("ryra/caddy/config/Caddyfile"),
            "unexpected caddyfile path: {path:?}"
        );
    }
}
