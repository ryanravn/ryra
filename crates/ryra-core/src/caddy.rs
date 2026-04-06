use std::path::PathBuf;

/// Path to the ryra-managed Caddyfile.
///
/// Lives inside the caddy service's data dir so the existing volume mount
/// (`%h/config` -> `/etc/caddy/`) picks it up automatically.
pub fn caddyfile_path() -> PathBuf {
    crate::service_home("caddy")
        .join("config")
        .join("Caddyfile")
}

/// Check if Caddy is installed (Caddyfile exists on disk).
pub fn is_installed() -> bool {
    caddyfile_path().exists()
}

/// Parameters for generating a Caddy site block.
pub struct CaddySiteParams {
    pub service_name: String,
    pub domain: String,
    pub upstream_port: u16,
    pub forward_auth: Option<ForwardAuthParams>,
}

/// Forward auth configuration for services without native OIDC.
pub struct ForwardAuthParams {
    pub port: u16,
    pub provider: AuthProvider,
}

/// Which auth provider is handling forward auth.
pub enum AuthProvider {
    Authelia,
    Authentik,
}

/// Generate a Caddy site block for a service.
///
/// The block always starts with a `# ryra:<service_name>` marker comment
/// so that [`add_route`] and [`remove_route`] can locate it.
pub fn render_site_block(params: &CaddySiteParams) -> String {
    let mut block = format!("# ryra:{}\n", params.service_name);
    block.push_str(&format!("{} {{\n", params.domain));
    block.push_str("    tls internal\n");

    if let Some(ref auth) = params.forward_auth {
        block.push_str(&format!(
            "    forward_auth host.containers.internal:{} {{\n",
            auth.port
        ));
        match auth.provider {
            AuthProvider::Authelia => {
                block.push_str("        uri /api/authz/forward-auth\n");
                block.push_str(
                    "        copy_headers Remote-User Remote-Groups Remote-Name Remote-Email\n",
                );
            }
            AuthProvider::Authentik => {
                block.push_str("        uri /outpost.goauthentik.io/auth/caddy\n");
                block.push_str(
                    "        copy_headers X-Authentik-Username X-Authentik-Groups X-Authentik-Email\n",
                );
            }
        }
        block.push_str("    }\n");
    }

    // Use host.containers.internal so Caddy (in its own podman network)
    // can reach services bound to 127.0.0.1 on the host.
    block.push_str(&format!(
        "    reverse_proxy host.containers.internal:{}\n",
        params.upstream_port
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
            // Skip the marker line
            i += 1;
            // Skip until we find the closing brace at depth 0
            let mut depth: i32 = 0;
            let mut found_open = false;
            while i < lines.len() {
                let trimmed = lines[i].trim();
                if trimmed.contains('{') {
                    depth += trimmed.matches('{').count() as i32;
                    found_open = true;
                }
                if trimmed.contains('}') {
                    depth -= trimmed.matches('}').count() as i32;
                }
                i += 1;
                if found_open && depth <= 0 {
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
            upstream_port: 8080,
            forward_auth: None,
        };
        let block = render_site_block(&params);
        assert!(block.starts_with("# ryra:whoami\n"));
        assert!(block.contains("whoami.example.com {"));
        assert!(block.contains("    reverse_proxy host.containers.internal:8080"));
        assert!(block.ends_with("}\n"));
    }

    #[test]
    fn render_block_with_forward_auth() {
        let params = CaddySiteParams {
            service_name: "grafana".to_string(),
            domain: "grafana.example.com".to_string(),
            upstream_port: 3000,
            forward_auth: Some(ForwardAuthParams {
                port: 9000,
                provider: AuthProvider::Authentik,
            }),
        };
        let block = render_site_block(&params);
        assert!(block.contains("forward_auth host.containers.internal:9000"));
        assert!(block.contains("uri /outpost.goauthentik.io/auth/caddy"));
        assert!(block.contains("copy_headers"));
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
            "    forward_auth host.containers.internal:9000 {\n",
            "        uri /outpost.goauthentik.io/auth/caddy\n",
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
        let path = caddyfile_path();
        assert!(
            path.ends_with("ryra/caddy/config/Caddyfile"),
            "unexpected caddyfile path: {path:?}"
        );
    }
}
