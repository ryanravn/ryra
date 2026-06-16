use std::path::{Path, PathBuf};

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
/// Site blocks emit `import services_tls`; the actual TLS strategy lives here.
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
    /// Bring-your-own certificate: Caddy serves the given cert + key files
    /// directly, issuing nothing. The right mode behind an upstream proxy that
    /// terminates the public TLS (Cloudflare in Full-Strict with an Origin CA
    /// cert), or anywhere the operator manages certs out of band.
    Byo { cert: String, key: String },
}

impl AcmeMode {
    /// Convert a user-supplied Let's Encrypt email into the matching LE
    /// mode: empty (user hit Enter at the prompt) means anonymous LE,
    /// anything else means LE with that email for renewal notices.
    pub fn from_email(email: &str) -> Self {
        if email.is_empty() {
            AcmeMode::Anonymous
        } else {
            AcmeMode::WithEmail(email.to_string())
        }
    }

    /// Renders the `(services_tls) { … }` snippet body for this mode.
    pub fn snippet(&self) -> String {
        match self {
            AcmeMode::Internal => "(services_tls) {\n\ttls internal\n}\n".to_string(),
            // Empty body — sites that import this get no `tls` directive,
            // which makes Caddy auto-issue LE certs for any public hostname.
            AcmeMode::Anonymous => "(services_tls) {\n}\n".to_string(),
            AcmeMode::WithEmail(email) => format!("(services_tls) {{\n\ttls {email}\n}}\n"),
            AcmeMode::Byo { cert, key } => {
                format!("(services_tls) {{\n\ttls {cert} {key}\n}}\n")
            }
        }
    }

    /// Best-effort reverse of [`Self::snippet`]: looks at the contents
    /// of `tls.caddy` and recognizes the three ryra-written shapes —
    /// `tls internal`, `tls <email>`, or an empty body. Returns `None`
    /// for user-customized snippets (Cloudflare DNS-01, BYO cert paths,
    /// arbitrary directives) so the install message can fall back to
    /// "user-managed" instead of misclassifying.
    pub fn detect_from_snippet(contents: &str) -> Option<Self> {
        // Strip the `(services_tls) { … }` wrapper and look at the body.
        let open = contents.find('{')?;
        let close = contents.rfind('}')?;
        if close <= open {
            return None;
        }
        let body = contents[open + 1..close].trim();
        if body.is_empty() {
            return Some(AcmeMode::Anonymous);
        }
        // Single-line body. Anything more complicated (multiple
        // directives, sub-blocks like `tls { dns cloudflare ... }`) is
        // user territory.
        if body.lines().count() != 1 {
            return None;
        }
        let line = body.trim();
        if line == "tls internal" {
            return Some(AcmeMode::Internal);
        }
        if let Some(rest) = line.strip_prefix("tls ") {
            let arg = rest.trim();
            // `tls <email>` — single token with an @.
            if arg.contains('@') && !arg.contains(' ') {
                return Some(AcmeMode::WithEmail(arg.to_string()));
            }
            // `tls <cert> <key>` — two tokens, both file paths (no @).
            let parts: Vec<&str> = arg.split_whitespace().collect();
            if parts.len() == 2 && !arg.contains('@') {
                return Some(AcmeMode::Byo {
                    cert: parts[0].to_string(),
                    key: parts[1].to_string(),
                });
            }
        }
        None
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
/// `issuer_port` is the port the internal vhost listens on — it must equal
/// the port in authelia's issuer URL so the back-channel reaches Caddy at the
/// exact host:port authelia's discovery response advertises. `*.internal`
/// carries Caddy's HTTPS port (e.g. `:8443`); `tailscale serve` / public
/// exposures are port-less (`:443`).
///
/// A no-op when Caddy isn't installed — returns an empty Vec.
pub fn ensure_auth_provider_routed(
    auth_service: WellKnownService,
    auth_domain: &str,
    auth_container_port: u16,
    issuer_port: u16,
    quadlet_dir: &Path,
) -> Result<Vec<Step>> {
    if !crate::is_service_installed("caddy") {
        return Ok(Vec::new());
    }

    let mut steps = Vec::new();
    let mut need_caddy_restart = false;

    // Caddy is installed, so caddy.container must exist — a missing or
    // unreadable file here means something has gone wrong that we should
    // surface, not swallow (OIDC discovery silently breaks otherwise).
    //
    // The path under quadlet_dir is a symlink ryra installs pointing at
    // the real file in service_home. Read through the symlink for content,
    // but write to the resolved target — Step::WriteFile refuses to
    // clobber symlinks, and we don't want to convert this one to a
    // regular file (the symlink is what systemd-quadlet looks up).
    let caddy_quadlet_link = quadlet_dir.join("caddy.container");
    let content =
        std::fs::read_to_string(&caddy_quadlet_link).map_err(|source| Error::FileRead {
            path: caddy_quadlet_link.clone(),
            source,
        })?;
    let caddy_quadlet_target =
        std::fs::canonicalize(&caddy_quadlet_link).map_err(|source| Error::FileRead {
            path: caddy_quadlet_link.clone(),
            source,
        })?;
    let network_spec = format!("{auth_service}:alias={auth_domain}");
    if !content.contains(&format!("alias={auth_domain}")) {
        let updated = inject_networks(&content, &[network_spec]);
        steps.push(Step::WriteFile(GeneratedFile {
            path: caddy_quadlet_target,
            content: updated,
        }));
        need_caddy_restart = true;
    }

    let caddyfile_path = caddyfile_path()?;
    let caddyfile = std::fs::read_to_string(&caddyfile_path).map_err(|source| Error::FileRead {
        path: caddyfile_path.clone(),
        source,
    })?;
    if !caddyfile.contains(&format!("# Service-Source: registry/{auth_service}")) {
        // The auth provider's primary container DNS name. For Authelia
        // today this matches the service name, but go through the same
        // resolver so a future provider with a non-default ContainerName
        // (or a multi-container layout) just works.
        let target_host = primary_container_name(
            &caddy_quadlet_link.with_file_name(format!("{auth_service}.container")),
            auth_service.as_str(),
        );
        let block = render_site_block(&CaddySiteParams {
            service_name: auth_service.to_string(),
            target_host,
            domain: auth_domain.to_string(),
            container_port: auth_container_port,
            https_port: issuer_port,
            // The auth provider's internal back-channel vhost always
            // terminates with Caddy's self-signed CA, even when the browser
            // reaches authelia at a real cert via tailscale/external proxy.
            force_internal_tls: true,
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
    /// Registry service name. Used as the `# Service-Source: registry/<name>`
    /// marker so [`add_route`] / [`remove_route`] can locate the block.
    pub service_name: String,
    pub domain: String,
    /// Container DNS name caddy reverse-proxies to. Often equals
    /// `service_name`, but multi-container services declare a different
    /// `ContainerName=` for their primary container (e.g. immich's main
    /// container is `immich-server`, not `immich`). See
    /// [`primary_container_name`] for the resolution helper.
    pub target_host: String,
    /// Container port the service listens on (used with container DNS name).
    pub container_port: u16,
    /// Caddy's HTTPS listen port (from the installed caddy service's port map).
    pub https_port: u16,
    /// Force `tls internal` (Caddy's self-signed CA) regardless of the
    /// domain suffix. Set for the auth provider's internal OIDC-terminator
    /// vhost: even when authelia is browser-reachable at a `*.ts.net` or
    /// public host (real cert via `tailscale serve` / external proxy), the
    /// *internal* back-channel hop terminates at Caddy with a self-signed
    /// cert that the service trusts via the mounted CA bundle.
    pub force_internal_tls: bool,
}

/// Generate a Caddy site block for a service.
///
/// The block always starts with a `# Service-Source: registry/<service_name>` marker comment
/// so that [`add_route`] and [`remove_route`] can locate it.
///
/// TLS strategy is delegated to the user-owned `(services_tls)` snippet
/// imported at the top of the Caddyfile — see [`tls_snippet_path`].
/// Exception: `*.internal` hosts always use `tls internal` directly,
/// because Let's Encrypt can't issue certs for the reserved `.internal`
/// TLD even when the user has flipped the snippet to ACME mode.
pub fn render_site_block(params: &CaddySiteParams) -> String {
    let mut block = format!("# Service-Source: registry/{}\n", params.service_name);
    block.push_str(&format!("{}:{} {{\n", params.domain, params.https_port));
    if params.force_internal_tls || params.domain.ends_with(".internal") {
        block.push_str("    tls internal\n");
    } else {
        block.push_str("    import services_tls\n");
    }
    // Use the primary container's DNS name on caddy's shared network.
    block.push_str(&format!(
        "    reverse_proxy {}:{}\n",
        params.target_host, params.container_port
    ));
    block.push_str("}\n");
    block
}

/// Read the `ContainerName=` directive from a quadlet file. Returns
/// `fallback` if the file can't be read or the directive is absent —
/// quadlets without `ContainerName=` are named after the unit by default
/// and that default matches the service name in ryra's convention.
pub fn primary_container_name(quadlet_path: &std::path::Path, fallback: &str) -> String {
    let Ok(content) = std::fs::read_to_string(quadlet_path) else {
        return fallback.to_string();
    };
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix("ContainerName=") {
            let name = rest.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    fallback.to_string()
}

/// Add or replace a service's block in the Caddyfile content.
///
/// If a block with `# Service-Source: registry/<service_name>` already exists, it is replaced.
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
/// Finds the `# Service-Source: registry/<service_name>` marker and removes everything from
/// that line through the closing `}` at brace depth 0, using brace-depth
/// tracking to correctly handle nested blocks.
pub fn remove_route(caddyfile: &str, service_name: &str) -> String {
    let marker = format!("# Service-Source: registry/{service_name}");
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
/// Returns `(service_name, domain)` pairs extracted from `# Service-Source: registry/` markers.
pub fn parse_domains(caddyfile: &str) -> Vec<(String, String)> {
    let mut domains = Vec::new();
    let mut current_service: Option<String> = None;

    for line in caddyfile.lines() {
        let trimmed = line.trim();
        if let Some(svc) = trimmed.strip_prefix("# Service-Source: registry/") {
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
    fn byo_cert_snippet_round_trips() {
        let mode = AcmeMode::Byo {
            cert: "/etc/ryra/certs/origin.pem".into(),
            key: "/etc/ryra/certs/origin.key".into(),
        };
        let snippet = mode.snippet();
        assert!(
            snippet.contains("tls /etc/ryra/certs/origin.pem /etc/ryra/certs/origin.key"),
            "got: {snippet}"
        );
        // Reading it back recovers the same mode (so `ryra add` can report
        // "your own certificate" instead of "user-customized").
        assert_eq!(AcmeMode::detect_from_snippet(&snippet), Some(mode));
    }

    #[test]
    fn detect_does_not_confuse_email_with_byo() {
        // A single token with @ is still LE-with-email, not a cert pair.
        assert_eq!(
            AcmeMode::detect_from_snippet("(services_tls) {\n\ttls me@example.com\n}\n"),
            Some(AcmeMode::WithEmail("me@example.com".into()))
        );
    }

    #[test]
    fn render_basic_block() {
        let params = CaddySiteParams {
            service_name: "whoami".to_string(),
            target_host: "whoami".to_string(),
            domain: "whoami.example.com".to_string(),
            container_port: 8080,
            https_port: 8443,
            force_internal_tls: false,
        };
        let block = render_site_block(&params);
        assert!(block.starts_with("# Service-Source: registry/whoami\n"));
        assert!(block.contains("whoami.example.com:8443 {"));
        assert!(block.contains("    import services_tls\n"));
        assert!(!block.contains("tls internal"));
        assert!(block.contains("    reverse_proxy whoami:8080"));
        assert!(block.ends_with("}\n"));
    }

    #[test]
    fn render_block_with_distinct_target_host() {
        // Multi-container services declare a primary ContainerName=
        // different from the service name (e.g. immich's main container
        // is `immich-server`). The Service-Source marker must stay
        // service-named so add_route/remove_route locate the block, but
        // the reverse_proxy target must use the actual container name.
        let params = CaddySiteParams {
            service_name: "immich".to_string(),
            target_host: "immich-server".to_string(),
            domain: "immich.internal".to_string(),
            container_port: 2283,
            https_port: 8443,
            force_internal_tls: false,
        };
        let block = render_site_block(&params);
        assert!(block.contains("# Service-Source: registry/immich\n"));
        assert!(block.contains("    reverse_proxy immich-server:2283"));
        assert!(!block.contains("reverse_proxy immich:"));
    }

    #[test]
    fn primary_container_name_reads_directive()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("immich.container");
        std::fs::write(
            &path,
            "[Container]\nImage=docker.io/immich/server\nContainerName=immich-server\nNetwork=immich.network\n",
        )?;
        assert_eq!(primary_container_name(&path, "immich"), "immich-server");
        Ok(())
    }

    #[test]
    fn primary_container_name_falls_back_when_directive_absent()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("whoami.container");
        std::fs::write(
            &path,
            "[Container]\nImage=docker.io/whoami\nNetwork=whoami.network\n",
        )?;
        assert_eq!(primary_container_name(&path, "whoami"), "whoami");
        Ok(())
    }

    #[test]
    fn primary_container_name_falls_back_when_file_missing() {
        let missing = std::path::Path::new("/nonexistent/missing.container");
        assert_eq!(primary_container_name(missing, "fallback"), "fallback");
    }

    #[test]
    fn render_internal_domain_keeps_tls_internal() {
        // *.internal hosts can't get LE certs, so they must bypass the
        // user-owned snippet (which may have been flipped to ACME mode).
        let params = CaddySiteParams {
            service_name: "authelia".to_string(),
            target_host: "authelia".to_string(),
            domain: "auth.internal".to_string(),
            container_port: 9091,
            https_port: 8443,
            force_internal_tls: false,
        };
        let block = render_site_block(&params);
        assert!(block.contains("    tls internal\n"));
        assert!(!block.contains("import services_tls"));
    }

    #[test]
    fn acme_mode_internal_snippet() {
        let s = AcmeMode::Internal.snippet();
        assert!(s.starts_with("(services_tls) {"));
        assert!(s.contains("tls internal"));
    }

    #[test]
    fn acme_mode_with_email_snippet() {
        let s = AcmeMode::WithEmail("admin@example.com".to_string()).snippet();
        assert!(s.starts_with("(services_tls) {"));
        assert!(s.contains("tls admin@example.com"));
        assert!(!s.contains("tls internal"));
    }

    #[test]
    fn acme_mode_detect_round_trips() {
        for mode in [
            AcmeMode::Internal,
            AcmeMode::Anonymous,
            AcmeMode::WithEmail("admin@example.com".into()),
        ] {
            let snippet = mode.snippet();
            let detected = AcmeMode::detect_from_snippet(&snippet);
            assert_eq!(detected, Some(mode));
        }
    }

    #[test]
    fn acme_mode_detect_user_customized_returns_none() {
        // Shapes ryra doesn't model (DNS-01 sub-blocks, multi-directive bodies)
        // must fall back to "user-managed" instead of being mis-classified.
        // Note: a plain `tls <cert> <key>` pair IS modeled now (AcmeMode::Byo),
        // covered by `byo_cert_snippet_round_trips`.
        let cf = "(services_tls) {\n\ttls {\n\t\tdns cloudflare {env.CF_API_TOKEN}\n\t}\n}\n";
        assert_eq!(AcmeMode::detect_from_snippet(cf), None);

        let extra = "(services_tls) {\n\ttls internal\n\theader X-Foo bar\n}\n";
        assert_eq!(AcmeMode::detect_from_snippet(extra), None);
    }

    #[test]
    fn acme_mode_anonymous_snippet_omits_tls_directive() {
        let s = AcmeMode::Anonymous.snippet();
        assert!(s.starts_with("(services_tls) {"));
        // No `tls` *directive* inside the body — Caddy auto-issues
        // anonymously when the snippet is empty. The "tls" inside
        // `services_tls` is the snippet name and must remain.
        assert!(!s.contains("\ttls "));
        assert!(!s.contains("tls internal"));
        assert!(!s.contains("tls @"));
    }

    #[test]
    fn render_block_custom_https_port() {
        let params = CaddySiteParams {
            service_name: "app".to_string(),
            target_host: "app".to_string(),
            domain: "app.example.com".to_string(),
            container_port: 3000,
            https_port: 9443,
            force_internal_tls: false,
        };
        let block = render_site_block(&params);
        assert!(block.contains("app.example.com:9443 {"));
    }

    #[test]
    fn render_force_internal_tls_on_public_domain() {
        // The auth provider's internal back-channel vhost terminates with
        // Caddy's self-signed CA even on a non-`.internal` host (e.g. a
        // `*.ts.net` issuer fronted by `tailscale serve`). The browser
        // never hits this block; only service containers do, trusting
        // Caddy's CA via the mounted bundle.
        let params = CaddySiteParams {
            service_name: "authelia".to_string(),
            target_host: "authelia".to_string(),
            domain: "auth.example.ts.net".to_string(),
            container_port: 9091,
            https_port: 443,
            force_internal_tls: true,
        };
        let block = render_site_block(&params);
        assert!(block.contains("auth.example.ts.net:443 {"));
        assert!(block.contains("    tls internal\n"));
        assert!(!block.contains("import services_tls"));
    }

    #[test]
    fn add_route_to_empty() {
        let block = "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let result = add_route("", "whoami", block);
        assert_eq!(result, format!("{block}\n"));
    }

    #[test]
    fn add_route_appends() {
        let existing = "# Service-Source: registry/foo\nfoo.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n";
        let block = "# Service-Source: registry/bar\nbar.example.com {\n    reverse_proxy host.containers.internal:4000\n}\n";
        let result = add_route(existing, "bar", block);
        assert!(result.contains("# Service-Source: registry/foo"));
        assert!(result.contains("# Service-Source: registry/bar"));
    }

    #[test]
    fn add_route_replaces_existing() {
        let existing = "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let new_block = "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:9090\n}\n";
        let result = add_route(existing, "whoami", new_block);
        assert!(!result.contains("8080"));
        assert!(result.contains("9090"));
    }

    #[test]
    fn remove_route_single() {
        let caddyfile = "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n";
        let result = remove_route(caddyfile, "whoami");
        assert_eq!(result, "");
    }

    #[test]
    fn remove_route_preserves_others() {
        let caddyfile = concat!(
            "# Service-Source: registry/foo\nfoo.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n\n",
            "# Service-Source: registry/bar\nbar.example.com {\n    reverse_proxy host.containers.internal:4000\n}\n",
        );
        let result = remove_route(caddyfile, "foo");
        assert!(!result.contains("foo"));
        assert!(result.contains("# Service-Source: registry/bar"));
        assert!(result.contains("reverse_proxy host.containers.internal:4000"));
    }

    #[test]
    fn remove_route_preserves_user_blocks() {
        let caddyfile = concat!(
            "mysite.example.com {\n    root * /var/www\n    file_server\n}\n\n",
            "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n",
        );
        let result = remove_route(caddyfile, "whoami");
        assert!(result.contains("mysite.example.com"));
        assert!(result.contains("file_server"));
        assert!(!result.contains("ryra:whoami"));
    }

    #[test]
    fn remove_route_with_nested_braces() {
        let caddyfile = concat!(
            "# Service-Source: registry/myapp\n",
            "myapp.example.com {\n",
            "    forward_auth host.containers.internal:9091 {\n",
            "        uri /api/authz/forward-auth\n",
            "    }\n",
            "    reverse_proxy host.containers.internal:3000\n",
            "}\n",
        );
        let result = remove_route(caddyfile, "myapp");
        assert_eq!(result, "");
    }

    #[test]
    fn parse_domains_basic() {
        let caddyfile = concat!(
            "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n\n",
            "# Service-Source: registry/myapp\nmyapp.example.com {\n    reverse_proxy host.containers.internal:3000\n}\n",
        );
        let domains = parse_domains(caddyfile);
        assert_eq!(domains.len(), 2);
        assert_eq!(
            domains[0],
            ("whoami".to_string(), "whoami.example.com".to_string())
        );
        assert_eq!(
            domains[1],
            ("myapp".to_string(), "myapp.example.com".to_string())
        );
    }

    #[test]
    fn parse_domains_ignores_user_blocks() {
        let caddyfile = concat!(
            "mysite.example.com {\n    file_server\n}\n\n",
            "# Service-Source: registry/whoami\nwhoami.example.com {\n    reverse_proxy host.containers.internal:8080\n}\n",
        );
        let domains = parse_domains(caddyfile);
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].0, "whoami");
    }

    #[test]
    fn caddyfile_path_is_under_service_home() {
        let path = caddyfile_path().expect("HOME should be set in test environment");
        assert!(
            path.ends_with("services/caddy/config/Caddyfile"),
            "unexpected caddyfile path: {path:?}"
        );
    }
}
