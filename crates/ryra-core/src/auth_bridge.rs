//! Auth bridge: artifacts that let an auth-enabled service talk to Caddy+Authelia.
//!
//! Services with `--auth` need to reach Authelia for OIDC. When Caddy is
//! handling HTTPS with a self-signed cert, the service container must trust
//! that cert and be able to resolve the auth provider's hostname to Caddy's
//! (dynamic) container IP.
//!
//! This module is a pure builder: it reads probe files under `/etc/ssl/certs`
//! and constructs [`Step::WriteFile`] entries for the caller to execute. It
//! performs no writes itself, so planning remains side-effect-free and
//! respects `--dry-run`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::schema::Config;
use crate::error::{Error, Result};
use crate::generate::GeneratedFile;
use crate::{Step, WellKnownService};

/// System CA bundle locations probed in order. First hit wins.
const SYSTEM_CA_PATHS: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
];

/// Quadlet + filesystem artifacts a caller should merge into the service's
/// generated unit and execute as part of the install plan.
pub struct AuthBridge {
    /// Extra `Volume=` entries for the service's `.container` unit.
    pub volumes: Vec<String>,
    /// Extra env vars for the service's `.env` file (CA trust for Python/Node).
    pub env: BTreeMap<String, String>,
    /// Extra `ExecStartPre=` entries for the service's `.container` unit.
    pub exec_start_pre: Vec<String>,
    /// Files to write before the service starts (CA bundle + helper scripts).
    pub steps: Vec<Step>,
}

/// Inputs to [`build`].
pub struct AuthBridgeParams<'a> {
    pub service_name: &'a str,
    pub enable_auth: bool,
    pub config: &'a Config,
    /// Absolute path to the service's data dir (`~/.local/share/ryra/<name>`).
    pub service_data: &'a Path,
}

/// Build auth-bridge artifacts for a service. Returns `Ok(None)` when the
/// bridge does not apply — the caller isn't using auth, the service is
/// authelia/caddy itself, authelia/Caddy aren't yet installed, or
/// authelia's URL isn't a Caddy-local hostname (`*.internal`).
///
/// Dispatch is driven by authelia's URL: when the hostname is `*.internal`,
/// Caddy is the internal TLS terminator and we build the existing bridge
/// (CA bundle, alias-to-Caddy, host-resolve script). Other URLs (Tailscale
/// FQDNs, public domains) imply the user is running their own internal
/// trust path, which ryra doesn't construct yet — bridge returns None and
/// runtime OIDC across containers is the user's responsibility for now.
pub fn build(params: &AuthBridgeParams<'_>) -> Result<Option<AuthBridge>> {
    if !params.enable_auth {
        return Ok(None);
    }
    if WellKnownService::Authelia.matches(params.service_name)
        || WellKnownService::Caddy.matches(params.service_name)
    {
        return Ok(None);
    }
    let authelia = params
        .config
        .services
        .iter()
        .find(|s| WellKnownService::Authelia.matches(&s.name));
    let Some(authelia) = authelia else {
        return Ok(None);
    };
    // Bridge applies only when authelia is reachable via a Caddy-fronted
    // *.internal hostname. Other hostnames mean another trust path is in
    // play (Tailscale serve, user's external proxy) and ryra doesn't have
    // matching client-side plumbing yet.
    let authelia_is_caddy_local = authelia
        .url
        .as_deref()
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h.ends_with(".internal"));
    if !authelia_is_caddy_local {
        return Ok(None);
    }
    let caddy_installed = params
        .config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
    if !caddy_installed {
        return Ok(None);
    }

    let ryra_dir: PathBuf = params
        .service_data
        .parent()
        .ok_or_else(|| Error::Bundle("service data dir has no parent directory".into()))?
        .to_path_buf();

    let merged_bundle = params.service_data.join("ca-bundle.crt");
    let refresh_ca_script = params.service_data.join("refresh-ca-bundle.sh");
    let auth_host_script = params.service_data.join("resolve-auth-host.sh");
    let auth_hosts = params.service_data.join("auth-hosts.txt");

    let mut volumes = Vec::new();
    let mut env = BTreeMap::new();
    let mut exec_start_pre = Vec::new();
    let mut steps = Vec::new();

    // --- CA bundle: system CAs + caddy's self-signed CA (if already present) ---
    //
    // Reading from /etc/ssl/certs is a read-only probe. If no system bundle is
    // found, we start with an empty string — the refresh-ca-bundle.sh hook
    // rebuilds it each start. If caddy's CA isn't on disk yet (caddy is being
    // installed alongside), refresh-ca-bundle.sh will pick it up at first
    // start. Either way, the placeholder is safe.
    let ca_cert_host = ryra_dir.join("caddy-root-ca.crt");
    let mut bundle = String::new();
    for sys_path in SYSTEM_CA_PATHS {
        if let Ok(content) = std::fs::read_to_string(sys_path) {
            bundle = content;
            break;
        }
    }
    if let Ok(caddy_ca) = std::fs::read_to_string(&ca_cert_host) {
        bundle.push_str("\n# ryra-caddy-ca\n");
        bundle.push_str(&caddy_ca);
    }
    steps.push(Step::WriteFile(GeneratedFile {
        path: merged_bundle.clone(),
        content: bundle,
    }));
    volumes.push(format!(
        "{}:/etc/ssl/certs/ca-certificates.crt:ro,z",
        merged_bundle.display()
    ));
    // Python (requests/certifi) and Node don't honour the system CA store —
    // they need explicit env vars.
    for var in ["REQUESTS_CA_BUNDLE", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"] {
        env.insert(var.into(), "/etc/ssl/certs/ca-certificates.crt".into());
    }

    // --- refresh-ca-bundle.sh: rebuild bundle at each service start ---
    let refresh_script = render_refresh_ca_script(&ryra_dir, params.service_data);
    steps.push(Step::WriteFile(GeneratedFile {
        path: refresh_ca_script.clone(),
        content: refresh_script,
    }));
    exec_start_pre.push(format!("-/bin/bash {}", refresh_ca_script.display()));

    // --- resolve-auth-host.sh: dynamic /etc/hosts for auth domain ---
    //
    // The auth provider's hostname (typically an ICANN `.internal` address)
    // isn't resolvable via normal DNS, so we write a small hosts file at
    // service-start time mapping it to caddy's current container IP and
    // bind-mount that over /etc/hosts.
    if let Some(auth_url) = authelia.url.as_deref()
        && let Ok(parsed) = url::Url::parse(auth_url)
        && let Some(host) = parsed.host_str()
    {
        let resolve_script = render_resolve_auth_host_script(params.service_data, host);
        steps.push(Step::WriteFile(GeneratedFile {
            path: auth_host_script.clone(),
            content: resolve_script,
        }));
        steps.push(Step::WriteFile(GeneratedFile {
            path: auth_hosts.clone(),
            content: format!("127.0.0.1 {host}\n"),
        }));
        exec_start_pre.push(format!("-/bin/bash {}", auth_host_script.display()));
        volumes.push(format!("{}:/etc/hosts:z", auth_hosts.display()));
    }

    Ok(Some(AuthBridge {
        volumes,
        env,
        exec_start_pre,
        steps,
    }))
}

fn render_refresh_ca_script(ryra_dir: &Path, service_data: &Path) -> String {
    format!(
        "#!/bin/bash\n\
         CADDY_CA=\"{ryra_dir}/caddy-root-ca.crt\"\n\
         MERGED=\"{service_data}/ca-bundle.crt\"\n\
         [ -f \"$CADDY_CA\" ] || exit 0\n\
         for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do\n\
           if [ -f \"$f\" ]; then cp \"$f\" \"$MERGED\"; break; fi\n\
         done\n\
         cat \"$CADDY_CA\" >> \"$MERGED\" 2>/dev/null || true\n\
         exit 0\n",
        ryra_dir = ryra_dir.display(),
        service_data = service_data.display(),
    )
}

fn render_resolve_auth_host_script(service_data: &Path, host: &str) -> String {
    // `timeout 5` guards against a wedged podman socket: this runs in
    // ExecStartPre, and without the guard a hung podman blocks service
    // startup indefinitely. On timeout we fall through to 127.0.0.1 —
    // OIDC won't work, but the service comes up instead of hanging.
    format!(
        "#!/bin/bash\n\
         # Resolve caddy's current IP for the auth domain\n\
         HOSTS=\"{service_data}/auth-hosts.txt\"\n\
         CADDY_IP=$(timeout 5 podman inspect caddy --format '{{{{range .NetworkSettings.Networks}}}}{{{{.IPAddress}}}} {{{{end}}}}' 2>/dev/null | awk '{{print $1}}')\n\
         if [ -n \"$CADDY_IP\" ]; then\n\
           echo \"$CADDY_IP {host}\" > \"$HOSTS\"\n\
         else\n\
           echo \"127.0.0.1 {host}\" > \"$HOSTS\"\n\
         fi\n\
         exit 0\n",
        service_data = service_data.display(),
        host = host,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{AuthCredentials, InstalledService};
    use std::collections::BTreeMap;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn installed(name: &str, url: Option<&str>) -> InstalledService {
        InstalledService {
            name: name.into(),
            version: "0.1.0".into(),
            repo: "bundled".into(),
            ports: BTreeMap::new(),
            auth_kind: None,
            url: url.map(String::from),
            tailscale_port: None,
            installed: true,
        }
    }

    fn config_with(services: Vec<InstalledService>, auth: Option<AuthCredentials>) -> Config {
        Config {
            services,
            auth,
            ..Config::default()
        }
    }

    fn write_paths(bridge: &AuthBridge) -> Vec<&Path> {
        bridge
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::WriteFile(f) => Some(f.path.as_path()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn returns_none_when_auth_disabled() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![installed("authelia", Some("https://auth.internal"))],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: false,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_when_authelia_not_installed() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(vec![installed("caddy", None)], None);
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_when_caddy_not_installed() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![installed("authelia", Some("https://auth.internal"))],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_for_authelia_itself() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![
                installed("authelia", Some("https://auth.internal")),
                installed("caddy", None),
            ],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "authelia",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_for_non_internal_authelia_url() -> TestResult {
        // Authelia at a non-`*.internal` URL means another trust path is
        // in play (Tailscale serve, user's external proxy). ryra doesn't
        // construct that bridge — runtime OIDC across containers is the
        // user's responsibility for non-Caddy deployments.
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![
                installed("authelia", Some("https://auth.test.local")),
                installed("caddy", None),
            ],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_for_authelia_url_without_host() -> TestResult {
        // Defensive: a URL that fails to parse or lacks a host shouldn't
        // crash the builder — it should bail cleanly.
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![
                installed("authelia", Some("not-a-url")),
                installed("caddy", None),
            ],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn returns_none_for_caddy_itself() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![
                installed("authelia", Some("https://auth.internal")),
                installed("caddy", None),
            ],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "caddy",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn build_does_not_write_to_service_data() -> TestResult {
        // The whole point of this refactor: a pure planning call must not
        // touch the service's data dir.
        let tmp = tempfile::tempdir()?;
        let service_data = tmp.path().join("forgejo");
        std::fs::create_dir_all(&service_data)?;

        let cfg = config_with(
            vec![
                installed("authelia", Some("https://auth.internal")),
                installed("caddy", None),
            ],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: &service_data,
        })?;
        assert!(out.is_some());

        let entries: Vec<_> = std::fs::read_dir(&service_data)?.collect();
        assert!(
            entries.is_empty(),
            "build() must not write to service_data, found: {entries:?}"
        );
        Ok(())
    }

    fn build_forgejo_bridge(service_data: &Path, authelia_url: Option<&str>) -> Result<AuthBridge> {
        let cfg = config_with(
            vec![
                installed("authelia", authelia_url),
                installed("caddy", None),
            ],
            None,
        );
        build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data,
        })?
        .ok_or_else(|| {
            Error::Bundle(
                "auth bridge unexpectedly returned None for forgejo + authelia + caddy".into(),
            )
        })
    }

    #[test]
    fn emits_expected_write_file_steps() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let service_data = tmp.path().join("forgejo");
        let bridge = build_forgejo_bridge(&service_data, Some("https://auth.internal"))?;

        let paths = write_paths(&bridge);
        assert!(paths.contains(&service_data.join("ca-bundle.crt").as_path()));
        assert!(paths.contains(&service_data.join("refresh-ca-bundle.sh").as_path()));
        assert!(paths.contains(&service_data.join("resolve-auth-host.sh").as_path()));
        assert!(paths.contains(&service_data.join("auth-hosts.txt").as_path()));
        Ok(())
    }

    #[test]
    fn returns_none_when_authelia_has_no_url() -> TestResult {
        // Bridge dispatch reads authelia's URL hostname; without one, ryra
        // can't tell if the deployment is Caddy-local or something else, so
        // it bails rather than guessing.
        let tmp = tempfile::tempdir()?;
        let cfg = config_with(
            vec![installed("authelia", None), installed("caddy", None)],
            None,
        );
        let out = build(&AuthBridgeParams {
            service_name: "forgejo",
            enable_auth: true,
            config: &cfg,
            service_data: tmp.path(),
        })?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn emits_ca_trust_volume_and_env() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let service_data = tmp.path().join("forgejo");
        let bridge = build_forgejo_bridge(&service_data, Some("https://auth.internal"))?;

        let bundle_mount = format!(
            "{}:/etc/ssl/certs/ca-certificates.crt:ro,z",
            service_data.join("ca-bundle.crt").display()
        );
        assert!(bridge.volumes.contains(&bundle_mount));
        assert_eq!(
            bridge.env.get("REQUESTS_CA_BUNDLE").map(String::as_str),
            Some("/etc/ssl/certs/ca-certificates.crt")
        );
        assert_eq!(
            bridge.env.get("SSL_CERT_FILE").map(String::as_str),
            Some("/etc/ssl/certs/ca-certificates.crt")
        );
        assert_eq!(
            bridge.env.get("NODE_EXTRA_CA_CERTS").map(String::as_str),
            Some("/etc/ssl/certs/ca-certificates.crt")
        );
        Ok(())
    }

    #[test]
    fn auth_hosts_contains_authelia_hostname() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let service_data = tmp.path().join("forgejo");
        // *.internal is the bridge's domain — non-internal URLs fall
        // outside Caddy-local dispatch and don't get a bridge.
        let bridge = build_forgejo_bridge(&service_data, Some("https://auth.internal"))?;

        let hosts_step = bridge
            .steps
            .iter()
            .find_map(|s| match s {
                Step::WriteFile(f) if f.path == service_data.join("auth-hosts.txt") => Some(f),
                _ => None,
            })
            .ok_or("auth-hosts.txt step missing")?;
        assert_eq!(hosts_step.content, "127.0.0.1 auth.internal\n");
        Ok(())
    }

    #[test]
    fn exec_start_pre_references_emitted_scripts() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let service_data = tmp.path().join("forgejo");
        let bridge = build_forgejo_bridge(&service_data, Some("https://auth.internal"))?;

        let refresh = format!(
            "-/bin/bash {}",
            service_data.join("refresh-ca-bundle.sh").display()
        );
        let resolve = format!(
            "-/bin/bash {}",
            service_data.join("resolve-auth-host.sh").display()
        );
        assert!(bridge.exec_start_pre.contains(&refresh));
        assert!(bridge.exec_start_pre.contains(&resolve));
        Ok(())
    }
}
