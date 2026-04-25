pub mod auth_bridge;
pub mod authelia;
pub mod caddy;
pub mod config;
pub mod data;
pub mod error;
pub mod generate;
pub mod prometheus;
pub mod registry;
pub mod system;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::ConfigPaths;
use config::schema::{Config, InstalledService};
use error::{Error, Result};
use generate::GeneratedFile;

// Well-known infrastructure service names used for cross-service integration.
pub const REGISTRY_BUNDLED: &str = "bundled";

/// Default Caddy HTTPS port, used when the caddy service record has no "https"
/// port entry (e.g., config was written by an older version).
const DEFAULT_CADDY_HTTPS_PORT: u16 = 8443;

/// Infrastructure services that ryra knows about for cross-service integration
/// (e.g., joining networks, configuring OIDC, setting up TLS).
///
/// Using an enum instead of string constants makes comparisons type-safe and
/// ensures the compiler catches typos or missing match arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WellKnownService {
    Caddy,
    Authelia,
    Inbucket,
    Prometheus,
}

impl WellKnownService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caddy => "caddy",
            Self::Authelia => "authelia",
            Self::Inbucket => "inbucket",
            Self::Prometheus => "prometheus",
        }
    }

    /// Try to match a service name to a well-known service.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "caddy" => Some(Self::Caddy),
            "authelia" => Some(Self::Authelia),
            "inbucket" => Some(Self::Inbucket),
            "prometheus" => Some(Self::Prometheus),
            _ => None,
        }
    }
}

impl std::fmt::Display for WellKnownService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl WellKnownService {
    /// Check if a string matches this well-known service name.
    pub fn matches(&self, name: &str) -> bool {
        self.as_str() == name
    }
}

// --- Path conventions ---

/// Resolve the user's home directory, falling back to $HOME.
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .ok_or(Error::HomeDirNotFound)
}

/// Root directory holding every installed service's home dir:
/// `~/.local/share/ryra/`.
pub fn service_data_root() -> Result<std::path::PathBuf> {
    let base = match dirs::data_local_dir() {
        Some(d) => d,
        None => home_dir()?.join(".local/share"),
    };
    Ok(base.join("ryra"))
}

/// Data directory for a service: ~/.local/share/ryra/<name>
pub fn service_home(service_name: &str) -> Result<PathBuf> {
    Ok(service_data_root()?.join(service_name))
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> Result<PathBuf> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("containers").join("systemd"))
}

/// True if the URL's host is a Tailscale MagicDNS name (`*.ts.net`). When
/// this matches, ryra skips the dances it does for `.internal` (Caddy route,
/// `/etc/hosts` entry, local CA trust) — Tailscale's tunnel already provides
/// routing, DNS, and encryption. Templates still populate normally so
/// service-specific config (trusted_domains, OIDC callbacks) picks up the
/// Tailscale hostname.
pub fn is_tailscale_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h.ends_with(".ts.net"))
}

/// Look up Caddy's HTTPS port from the installed service record.
pub(crate) fn caddy_https_port(config: &Config) -> u16 {
    config
        .services
        .iter()
        .find(|s| WellKnownService::Caddy.matches(&s.name))
        .and_then(|s| s.ports.get("https").copied())
        .unwrap_or(DEFAULT_CADDY_HTTPS_PORT)
}

// --- Typed steps: what the CLI needs to execute ---

/// A discrete operation that the CLI executes. Pattern matching ensures
/// every step type is handled — no string parsing or if-chains.
pub enum Step {
    /// Write a file.
    WriteFile(GeneratedFile),
    /// Reload systemd for the current user.
    DaemonReload,
    /// Start a service under the current user's systemd.
    StartService { unit: String },
    /// Stop a service under the current user's systemd.
    StopService { unit: String },
    /// Restart a service under the current user's systemd.
    RestartService { unit: String },
    /// Reload Caddy's config without restarting the container.
    ReloadCaddy,
    /// Pull a container image.
    PullImage { image: String },
    /// Remove a file.
    RemoveFile(PathBuf),
    /// Remove a directory tree.
    RemoveDir(PathBuf),
    /// Remove a podman named volume.
    RemoveVolume { name: String },
    /// Create a directory (with parents).
    CreateDir(PathBuf),
    /// Wait for a file to appear (with timeout).
    WaitForFile { path: PathBuf, timeout_secs: u32 },
    /// Copy a file from the registry (or similar source) to a destination.
    /// Used for vendored binary files (e.g. Jellyfin's SSO plugin DLLs)
    /// that don't fit the templated `configs/` pipeline.
    CopyFile { src: PathBuf, dst: PathBuf },
}

impl Step {
    /// Render this step as a shell command (for dry-run display).
    pub fn to_command(&self) -> String {
        match self {
            Step::WriteFile(file) => format!("write {}", file.path.display()),
            Step::DaemonReload => "systemctl --user daemon-reload".into(),
            Step::StartService { unit } => format!("systemctl --user start {unit}"),
            Step::StopService { unit } => format!("systemctl --user stop {unit}"),
            Step::RestartService { unit } => format!("systemctl --user restart {unit}"),
            Step::ReloadCaddy => {
                "podman exec caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile"
                    .into()
            }
            Step::PullImage { image } => format!("podman pull {image}"),
            Step::RemoveFile(path) => format!("rm -f {}", path.display()),
            Step::RemoveDir(path) => format!("rm -rf {}", path.display()),
            Step::CreateDir(path) => format!("mkdir -p {}", path.display()),
            Step::RemoveVolume { name } => format!("podman volume rm {name}"),
            Step::WaitForFile { path, timeout_secs } => {
                format!("wait for {} (up to {timeout_secs}s)", path.display())
            }
            Step::CopyFile { src, dst } => format!("cp {} {}", src.display(), dst.display()),
        }
    }
}

// --- Warnings ---

/// Warnings generated during service operations that the CLI should display.
pub enum Warning {
    /// System RAM is below the service's minimum requirement.
    RamBelowMinimum {
        service_name: String,
        min_mb: u64,
        available_mb: u64,
    },
    /// System RAM is below the service's recommended level (but above minimum).
    RamBelowRecommended {
        service_name: String,
        recommended_mb: u64,
        available_mb: u64,
    },
    /// A port was reassigned because the default was privileged or in use.
    PortReassigned {
        service_name: String,
        port_name: String,
        original_port: u16,
        assigned_port: u16,
        reason: String,
    },
    /// `--url` was passed but no bundled reverse proxy (Caddy) is installed.
    /// Ryra still templates the URL into env vars and OIDC config, but routing
    /// is the user's responsibility (nginx, Cloudflare Tunnel, Tailscale Funnel,
    /// external load balancer, etc.).
    UrlWithoutReverseProxy {
        service_name: String,
        url: String,
        host_port: u16,
    },
}

// --- Result types ---

pub struct AddResult {
    pub steps: Vec<Step>,
    pub warnings: Vec<Warning>,
    pub repo_url: String,
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in .env).
    pub generated_secrets: Vec<String>,
    /// The generated .env content (for post-install processing).
    pub env_content: String,
    /// Public URL for this service (if --url was provided).
    pub url: Option<String>,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub service_name: String,
    /// URL that was assigned to this service (if any).
    pub url: Option<String>,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

/// Resolve the registry directory for a service reference.
pub async fn resolve_registry_dir(service_ref: &registry::resolve::ServiceRef) -> Result<PathBuf> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_cache_dir()?;
    let config = config::load_or_default(&paths.config_file)?;
    registry::resolve::resolve_registry_dir(service_ref, &config, &paths.cache_dir).await
}

/// Build a ServiceRef from an installed service's stored registry name.
pub fn service_ref_from_installed(installed: &InstalledService) -> registry::resolve::ServiceRef {
    if installed.repo.is_empty() || installed.repo == REGISTRY_BUNDLED {
        registry::resolve::ServiceRef::Bundled(installed.name.clone())
    } else {
        registry::resolve::ServiceRef::Custom {
            registry: installed.repo.clone(),
            service: installed.name.clone(),
        }
    }
}

/// When a shared-network provider (caddy or inbucket) is installed, patch
/// already-installed services' primary quadlets to include `Network=<svc>.network`
/// if they should reach the new provider. Emits `WriteFile` + `DaemonReload`
/// + `RestartService` per patched service.
///
/// Scope is intentionally narrow: it only adds the network that the newly
/// installed provider owns. The install-time networking policy for each
/// patched service's OTHER networks is unchanged.
fn retroactive_network_joins(
    new_service: &str,
    config: &config::schema::Config,
    quadlet_path: &std::path::Path,
    repo_dir: Option<&std::path::Path>,
) -> Vec<Step> {
    let mut steps = Vec::new();
    let is_caddy = WellKnownService::Caddy.matches(new_service);
    let is_inbucket = WellKnownService::Inbucket.matches(new_service);
    let is_prometheus = WellKnownService::Prometheus.matches(new_service);
    if !is_caddy && !is_inbucket && !is_prometheus {
        return steps;
    }

    for svc in &config.services {
        if !svc.installed {
            continue;
        }
        if WellKnownService::Caddy.matches(&svc.name)
            || WellKnownService::Inbucket.matches(&svc.name)
            || WellKnownService::Prometheus.matches(&svc.name)
        {
            // Providers don't need to join themselves.
            continue;
        }
        let (network_name, should_join) = if is_caddy {
            // caddy: URL-having services want reverse proxy routing.
            ("caddy".to_string(), svc.url.is_some())
        } else if is_inbucket {
            // inbucket: any already-installed service whose .env points
            // SMTP at the "inbucket" hostname needs to reach it.
            ("inbucket".to_string(), service_uses_inbucket(&svc.name))
        } else {
            // prometheus: services whose registry definition declares
            // `[integrations].prometheus = true` join so prometheus can
            // reach them via container DNS. Also write the scrape-target
            // file for each one.
            let svc_supports = repo_dir
                .and_then(|rd| registry::find_service(rd, &svc.name).ok())
                .map(|rs| rs.def.integrations.prometheus)
                .unwrap_or(false);
            if svc_supports
                && let Some(rd) = repo_dir
                && let Ok(rs) = registry::find_service(rd, &svc.name)
                && let Ok(register_steps) =
                    prometheus::register_scrape_target(&svc.name, &rs.def, true)
            {
                steps.extend(register_steps);
            }
            ("prometheus".to_string(), svc_supports)
        };
        if !should_join {
            continue;
        }
        // Multi-container services (e.g. zammad with a separate railsserver
        // that actually sends mail) need the network on every component
        // container. Patch each `.container` file belonging to this service
        // and restart each unit so podman recreates the container with the
        // new network. Restarting only the primary unit doesn't cascade to
        // subunits — their containers would keep running on the old network.
        let all_service_names: Vec<&str> =
            config.services.iter().map(|s| s.name.as_str()).collect();
        let marker = format!("Network={network_name}.network");
        let mut units_to_restart: Vec<String> = Vec::new();
        let Ok(entries) = std::fs::read_dir(quadlet_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if n.ends_with(".container") => n.to_string(),
                _ => continue,
            };
            if !quadlet_belongs_to(&name, &svc.name, &all_service_names) {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if content.contains(&marker) {
                continue;
            }
            let updated =
                generate::bundle::inject_networks(&content, std::slice::from_ref(&network_name));
            steps.push(Step::WriteFile(GeneratedFile {
                path,
                content: updated,
            }));
            // Unit name is the .container filename minus extension; systemd's
            // generator turns `foo-bar.container` into `foo-bar.service`.
            let unit = name.trim_end_matches(".container").to_string();
            units_to_restart.push(unit);
        }
        if !units_to_restart.is_empty() {
            steps.push(Step::DaemonReload);
            for unit in units_to_restart {
                steps.push(Step::RestartService { unit });
            }
        }
    }
    steps
}

/// Heuristic: does this service's `.env` point SMTP at the inbucket container?
/// Matches any line whose value is `inbucket` or `inbucket:<port>` — covers
/// the common shape `SOMETHING_SMTP_HOST=inbucket` and variants like
/// `FORGEJO__mailer__SMTP_ADDR=inbucket`.
fn service_uses_inbucket(service_name: &str) -> bool {
    let env_path = match service_home(service_name) {
        Ok(h) => h.join(".env"),
        Err(_) => return false,
    };
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content.lines().any(|line| {
        let Some((_, value)) = line.split_once('=') else {
            return false;
        };
        let v = value.trim();
        v == "inbucket" || v.starts_with("inbucket:")
    })
}

/// Determine which extra podman networks a service should join.
///
/// Three providers own a shared network:
/// - `authelia.network` — services with `--auth` join so they can reach the
///   OIDC provider by container DNS.
/// - `inbucket.network` — services with SMTP configured join so they can
///   reach `inbucket:2500` without requiring caddy to be installed.
/// - `caddy.network` — URL-having services join for reverse-proxy routing;
///   auth-enabled services join so OIDC discovery goes through caddy's TLS;
///   inbucket itself joins so its web UI can be reverse-proxied when a URL
///   is supplied.
#[allow(clippy::too_many_arguments)]
fn resolve_extra_networks(
    service_name: &str,
    enable_auth: bool,
    authelia_installed: bool,
    caddy_installed: bool,
    inbucket_installed: bool,
    prometheus_installed: bool,
    has_url: bool,
    has_smtp: bool,
    has_prometheus: bool,
) -> Vec<String> {
    let mut networks = Vec::new();
    if enable_auth && authelia_installed && !WellKnownService::Authelia.matches(service_name) {
        networks.push(WellKnownService::Authelia.to_string());
    }
    // SMTP-using services reach inbucket via its own network — no caddy
    // dependency. This is symmetric with how auth services reach authelia.
    let joins_inbucket =
        has_smtp && inbucket_installed && !WellKnownService::Inbucket.matches(service_name);
    if joins_inbucket {
        networks.push(WellKnownService::Inbucket.to_string());
    }
    // Prometheus-supporting services join the prometheus network so
    // prometheus can scrape them by container DNS on their primary port.
    let joins_prometheus = has_prometheus
        && prometheus_installed
        && !WellKnownService::Prometheus.matches(service_name);
    if joins_prometheus {
        networks.push(WellKnownService::Prometheus.to_string());
    }
    let joins_caddy = (has_url || enable_auth || WellKnownService::Inbucket.matches(service_name))
        && caddy_installed
        && !WellKnownService::Caddy.matches(service_name);
    if joins_caddy && !networks.contains(&WellKnownService::Caddy.to_string()) {
        networks.push(WellKnownService::Caddy.to_string());
    }
    networks
}

/// Add a service: generate config, return steps to execute.
///
/// When `pre_built_ctx` is provided, its secrets and auth credentials are
/// reused instead of generating fresh ones. Pass the context from the
/// interactive prompt phase so the values the user saw match what gets written.
#[allow(clippy::too_many_arguments)]
pub fn add_service(
    service_name: &str,
    url: Option<&str>,
    auth_kind: Option<registry::service_def::AuthKind>,
    enable_auth: bool,
    enable_smtp: bool,
    env_overrides: &BTreeMap<String, String>,
    enabled_groups: &std::collections::BTreeSet<String>,
    registry_name: &str,
    repo_dir: &Path,
    pre_built_ctx: Option<BTreeMap<String, String>>,
    port_in_use: &dyn Fn(u16) -> bool,
) -> Result<AddResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    if let Some(existing) = config.services.iter().find(|s| s.name == service_name) {
        if existing.installed {
            return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
        }
        // installed: false — the CLI should clean up via remove_service +
        // finalize_remove before calling add_service again.
        return Err(Error::ServiceIncomplete(service_name.to_string()));
    }

    // No config entry, but preserved volumes or a lingering home dir from
    // `ryra remove <svc>` (default Preserve mode) would make the fresh .env's
    // generated secrets disagree with what's already baked into the volume —
    // postgres writes POSTGRES_PASSWORD into pgdata on first init and then
    // skips reinit, so a new password in .env just restart-loops on auth
    // failures. Surface the same way as an incomplete install; the CLI's
    // existing purge-and-retry recovery handles it.
    if data::enumerate_service(&config, service_name)?.is_some() {
        return Err(Error::ServiceIncomplete(service_name.to_string()));
    }

    let reg_service = registry::find_service(repo_dir, service_name)?;

    // Validate: architecture compatibility
    if let Some(msg) = reg_service.def.check_architecture() {
        return Err(Error::UnsupportedArchitecture(msg));
    }

    // Validate: all required services must be installed
    let missing_requires: Vec<&str> = reg_service
        .def
        .requires
        .iter()
        .filter(|r| !config.services.iter().any(|s| s.name == r.service))
        .map(|r| r.service.as_str())
        .collect();
    if !missing_requires.is_empty() {
        return Err(Error::MissingRequiredServices {
            service: service_name.to_string(),
            missing: missing_requires.iter().map(|s| s.to_string()).collect(),
        });
    }

    // If the user chose to enable auth, an auth provider must be configured
    if auth_kind.is_some() && config.auth.is_none() {
        return Err(Error::AuthNotConfigured);
    }

    // --auth requires native OIDC support; forward auth is no longer supported
    if enable_auth
        && reg_service.def.integrations.auth.is_empty()
        && !WellKnownService::Authelia.matches(service_name)
    {
        return Err(Error::NoOidcSupport(service_name.to_string()));
    }

    // Every `--enable <group>` must match a group defined on this service.
    // Surfacing unknown group names here (vs. silently ignoring them) means
    // a typo fails fast instead of producing a half-configured service.
    for g in enabled_groups {
        if !reg_service.def.env_groups.iter().any(|eg| &eg.name == g) {
            let known: Vec<String> = reg_service
                .def
                .env_groups
                .iter()
                .map(|eg| eg.name.clone())
                .collect();
            let hint = if known.is_empty() {
                " (service defines no env_groups)".to_string()
            } else {
                format!(" (known: {})", known.join(", "))
            };
            return Err(Error::UnknownEnvGroup {
                service: service_name.to_string(),
                group: g.clone(),
                hint,
            });
        }
    }

    // Resolve a host port for every entry in [[ports]]. Each port gets its
    // own distinct host port — a prior bug allocated one port and gave it
    // to every entry, so services with multiple [[ports]] (ente-web:
    // 3000/3002/3003, inbucket: http+smtp) hit `bind: address already in
    // use` on all but the first.
    let mut port_warnings: Vec<Warning> = Vec::new();
    let mut claimed: std::collections::HashSet<u16> = reg_service
        .def
        .ports
        .iter()
        .filter_map(|p| p.host_port)
        .collect();
    let mut resolved_ports: Vec<(String, u16)> = Vec::with_capacity(reg_service.def.ports.len());
    for p in &reg_service.def.ports {
        let host = if let Some(hp) = p.host_port {
            hp
        } else {
            let privileged = p.container_port < 1024;
            let claimed_in_service = claimed.contains(&p.container_port);
            let in_use = port_in_use(p.container_port);
            if privileged || claimed_in_service || in_use {
                let allocated =
                    system::port::allocate_port_excluding(&config, &claimed, port_in_use)?;
                let reason = if privileged {
                    "port is privileged (requires root)".to_string()
                } else if claimed_in_service {
                    format!(
                        "port {} is already claimed by another port in this service",
                        p.container_port
                    )
                } else {
                    format!("port {} is already in use", p.container_port)
                };
                port_warnings.push(Warning::PortReassigned {
                    service_name: service_name.to_string(),
                    port_name: p.name.clone(),
                    original_port: p.container_port,
                    assigned_port: allocated,
                    reason,
                });
                allocated
            } else {
                p.container_port
            }
        };
        claimed.insert(host);
        resolved_ports.push((p.name.clone(), host));
    }

    // Primary host port drives service.url / service.port templating.
    // Prefer "http", fall back to the first defined port.
    let host_port = resolved_ports
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("http"))
        .or_else(|| resolved_ports.first())
        .map(|(_, p)| *p);

    // Check for port conflicts by probing whether the port is already bound.
    // For explicitly-set host_port entries, allocate_port_excluding didn't run
    // so we re-check here.
    for (_, port) in &resolved_ports {
        if port_in_use(*port) {
            return Err(Error::PortConflict { port: *port });
        }
    }

    let home_dir = service_home(service_name)?;
    let quadlet_path = quadlet_dir()?;

    let authelia_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Authelia.matches(&s.name));
    let caddy_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
    let inbucket_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Inbucket.matches(&s.name) && s.installed);
    let prometheus_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Prometheus.matches(&s.name) && s.installed);

    // Build auth-bridge artifacts (CA trust + dynamic /etc/hosts for the
    // auth provider's domain). Pure — all filesystem writes are
    // emitted as Step::WriteFile below, not performed here.
    let auth_bridge = auth_bridge::build(&auth_bridge::AuthBridgeParams {
        service_name,
        enable_auth,
        config: &config,
        service_data: &home_dir,
    })?;

    let (extra_volumes, extra_env, extra_exec_start_pre, auth_bridge_steps) = match auth_bridge {
        Some(b) => (b.volumes, b.env, b.exec_start_pre, b.steps),
        None => (Vec::new(), BTreeMap::new(), Vec::new(), Vec::new()),
    };

    let has_smtp = enable_smtp
        && reg_service.def.integrations.smtp
        && !reg_service.def.mappings.smtp.is_empty()
        && config.smtp.is_some();
    let extra_networks = resolve_extra_networks(
        service_name,
        enable_auth,
        authelia_installed,
        caddy_installed,
        inbucket_installed,
        prometheus_installed,
        url.is_some(),
        has_smtp,
        reg_service.def.integrations.prometheus,
    );

    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: auth_kind.as_ref(),
        host_port,
        resolved_ports: &resolved_ports,
        env_overrides,
        url,
        extra_env,
        pre_built_ctx,
        enable_smtp: has_smtp,
        enabled_groups,
    })?;

    let podman_args: Vec<String> = Vec::new();

    // Build port variable expansions for quadlet PublishPort directives.
    // Each port has its own resolved host port (see `resolved_ports` above).
    let port_vars: Vec<(String, String)> = resolved_ports
        .iter()
        .map(|(name, port)| {
            (
                format!("RYRA_PORT_{}", name.to_uppercase()),
                port.to_string(),
            )
        })
        .collect();

    // Process quadlet bundle from registry
    let bundle =
        generate::bundle::process_quadlet_bundle(&generate::bundle::ProcessBundleParams {
            service_dir: &reg_service.service_dir,
            service_name,
            quadlet_dir: &quadlet_path,
            extra_networks: &extra_networks,
            extra_volumes: &extra_volumes,
            podman_args: &podman_args,
            extra_exec_start_pre: &extra_exec_start_pre,
            port_vars: &port_vars,
        })?;

    // Generate warnings
    let mut warnings = Vec::new();

    if let Some(ref reqs) = reg_service.def.requirements
        && let Some(total) = system::memory::total_ram_mb()
    {
        if total < reqs.ram.min {
            warnings.push(Warning::RamBelowMinimum {
                service_name: service_name.to_string(),
                min_mb: reqs.ram.min,
                available_mb: total,
            });
        } else if let Some(rec) = reqs.ram.recommended
            && total < rec
        {
            warnings.push(Warning::RamBelowRecommended {
                service_name: service_name.to_string(),
                recommended_mb: rec,
                available_mb: total,
            });
        }
    }
    warnings.extend(port_warnings);

    // Build ordered steps
    let mut steps = Vec::new();

    // 1. Create service data directory
    steps.push(Step::CreateDir(home_dir.clone()));

    // Capture env content before it is moved into steps
    let env_content = output.env_file.content.clone();

    // 2. Pull all images (from quadlet bundle)
    for image in &bundle.images {
        steps.push(Step::PullImage {
            image: image.clone(),
        });
    }

    // 3. Write quadlet files from bundle
    for file in bundle.quadlet_files {
        steps.push(Step::WriteFile(file));
    }

    // 4. Write config files from bundle
    for file in bundle.config_files {
        steps.push(Step::WriteFile(file));
    }

    // 4b. Copy vendored files (plugin DLLs, archives etc.) from the
    // registry into service_home. The config pipeline is UTF-8 /
    // template-only; binary payloads flow through CopyFile instead.
    for (src, dst) in bundle.files {
        steps.push(Step::CopyFile { src, dst });
    }

    // 5. Write .env file
    steps.push(Step::WriteFile(output.env_file));

    // 6. Create bind mount directories (must exist before container starts)
    for dir in &bundle.bind_mount_dirs {
        steps.push(Step::CreateDir(dir.clone()));
    }

    // 7. Auth-bridge artifacts (CA bundle, refresh script, host-resolve script,
    // placeholder /etc/hosts) — needed before container starts for TLS trust
    // and auth-domain resolution.
    steps.extend(auth_bridge_steps);

    // 8. Register OIDC client with the auth provider BEFORE starting the service.
    // This must happen first because the service's ExecStartPost (e.g., register-oidc.sh)
    // needs the auth provider configured and caddy's network alias in place so OIDC
    // discovery URLs resolve correctly from within the service container.
    if let (
        Some(registry::service_def::AuthKind::Oidc),
        Some(config::schema::AuthCredentials::Authelia { .. }),
    ) = (auth_kind.as_ref(), config.auth.as_ref())
    {
        steps.extend(authelia::register_oidc_client(
            service_name,
            &reg_service.def,
            url,
            &output.ctx,
            &config,
            &quadlet_path,
        )?);
    }

    // 9. Add Caddy route for services with a URL when Caddy is installed.
    // This creates a reverse proxy from the service's domain to its container port.
    //
    // Tailscale URLs (*.ts.net) skip this: the service is already reachable on
    // its host port via the tailnet, and MagicDNS handles the hostname.
    if let Some(url) = url
        && !WellKnownService::Caddy.matches(service_name)
        && !is_tailscale_url(url)
    {
        if caddy_installed {
            let parsed = url::Url::parse(url)
                .map_err(|e| Error::Template(format!("invalid service URL '{url}': {e}")))?;
            let domain = parsed.host_str().ok_or_else(|| {
                Error::Template(format!(
                    "service URL '{url}' has no host — Caddy needs a hostname to route to"
                ))
            })?;
            let container_port = reg_service
                .def
                .ports
                .first()
                .map(|p| p.container_port)
                .unwrap_or(80);
            let block = caddy::render_site_block(&caddy::CaddySiteParams {
                service_name: service_name.to_string(),
                domain: domain.to_string(),
                container_port,
                https_port: caddy_https_port(&config),
            });
            let caddyfile_path = caddy::caddyfile_path()?;
            let existing =
                std::fs::read_to_string(&caddyfile_path).map_err(|source| Error::FileRead {
                    path: caddyfile_path.clone(),
                    source,
                })?;
            let updated = caddy::add_route(&existing, service_name, &block);
            steps.push(Step::WriteFile(GeneratedFile {
                path: caddyfile_path,
                content: updated,
            }));
            steps.push(Step::ReloadCaddy);
        } else if let Some(primary) = host_port {
            // --url was passed but no bundled reverse proxy is installed.
            // Templating and OIDC still work, but the user is responsible for
            // routing <url> → 127.0.0.1:<primary> via nginx / Cloudflare Tunnel
            // / Tailscale Funnel / etc.
            warnings.push(Warning::UrlWithoutReverseProxy {
                service_name: service_name.to_string(),
                url: url.to_string(),
                host_port: primary,
            });
        }
    }

    // 9b. Register this service as a prometheus scrape target when both
    // prometheus is installed and the service declares the integration.
    // Writes a per-service JSON target file that prometheus's file_sd
    // picks up automatically (no reload needed).
    steps.extend(prometheus::register_scrape_target(
        service_name,
        &reg_service.def,
        prometheus_installed,
    )?);

    // 9c. When a shared-network provider (caddy, inbucket, prometheus) is
    // being installed, retroactively patch services that were installed
    // before it so they can reach the new provider by container DNS.
    // resolve_extra_networks only decides at install time; without this
    // step, services installed earlier remain isolated.
    steps.extend(retroactive_network_joins(
        service_name,
        &config,
        &quadlet_path,
        Some(repo_dir),
    ));

    // 10. Reload and start via systemd
    steps.push(Step::DaemonReload);
    // Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    // Collect post-install info
    let allocated_ports: Vec<(String, u16)> = resolved_ports.clone();

    // Secret names from env var templates (not stored in state)
    let mut generated_secrets: Vec<String> = reg_service
        .def
        .env
        .iter()
        .filter(|e| !env_overrides.contains_key(&e.name))
        .flat_map(|e| generate::extract_secret_refs(&e.value))
        .collect();
    // Deduplicate — the same secret may be referenced by multiple env vars
    generated_secrets.sort();
    generated_secrets.dedup();

    Ok(AddResult {
        steps,
        warnings,
        repo_url: registry_name.to_string(),
        allocated_ports,
        generated_secrets,
        env_content,
        url: url.map(|u| u.to_string()),
    })
}

/// Check if a quadlet filename belongs to a service.
///
/// Matches `{service_name}.container`, `{service_name}-db.volume`, etc.
/// but NOT `{service_name_prefix}-other.container` (e.g., "foo" must not
/// match "foo-bar.container" when "foo-bar" is a known service).
///
/// `all_service_names` contains every installed service name — used to detect
/// when a longer service name owns the file instead.
pub fn quadlet_belongs_to(filename: &str, service_name: &str, all_service_names: &[&str]) -> bool {
    if !filename.starts_with(service_name) {
        return false;
    }
    let rest = &filename[service_name.len()..];
    if rest.starts_with('.') {
        return true;
    }
    if !rest.starts_with('-') {
        return false;
    }
    // Check that no other installed service is a longer prefix match.
    // e.g., "foo-bar.container" with service "foo" — if "foo-bar"
    // is also installed, it owns this file.
    !all_service_names.iter().any(|&other| {
        other.len() > service_name.len()
            && other.starts_with(service_name)
            && filename.starts_with(other)
            && filename[other.len()..].starts_with(['.', '-'])
    })
}

/// How destructive `remove_service` should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveMode {
    /// Stop + remove quadlets + delete ephemeral config files, but keep
    /// the data subdirs under the service home dir and keep all podman
    /// named volumes. After this, `ryra data ls` reports the service as
    /// `Orphan`.
    Preserve,
    /// Stop + remove everything: quadlets, entire home dir, named volumes.
    Purge,
}

/// Remove a service: update state, return cleanup steps.
pub fn remove_service(service_name: &str, mode: RemoveMode) -> Result<RemoveResult> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let installed = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    // Stop all units belonging to this service (main + sidecars).
    // Quadlet files named {service_name}.ext or {service_name}-sidecar.ext.
    let quadlet_path = quadlet_dir()?;
    let mut steps = Vec::new();
    let mut volume_names = Vec::new();
    let mut has_named_volumes = false;
    let all_names: Vec<&str> = config.services.iter().map(|s| s.name.as_str()).collect();

    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !quadlet_belongs_to(&name, service_name, &all_names) {
                continue;
            }
            // Stop each .container unit before removing files
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".volume") {
                has_named_volumes = true;
                if matches!(mode, RemoveMode::Purge) {
                    let vol = name.trim_end_matches(".volume").to_string();
                    // Quadlet prefixes volume names with "systemd-"
                    volume_names.push(format!("systemd-{vol}"));
                }
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // Clean up ryra-managed Caddy site block + OIDC client registration
    // BEFORE the daemon reload, so the routing layers drop their stale
    // pointers while the doomed containers are already stopped.
    if !WellKnownService::Caddy.matches(service_name) && installed.url.is_some() {
        let caddyfile_path = caddy::caddyfile_path()?;
        if caddyfile_path.exists() {
            let existing =
                std::fs::read_to_string(&caddyfile_path).map_err(|source| Error::FileRead {
                    path: caddyfile_path.clone(),
                    source,
                })?;
            let updated = caddy::remove_route(&existing, service_name);
            if updated != existing {
                steps.push(Step::WriteFile(GeneratedFile {
                    path: caddyfile_path,
                    content: updated.clone(),
                }));
                // Skip reload if the Caddyfile is now empty — Caddy rejects
                // empty configs and will fail the reload.
                if !updated.trim().is_empty() {
                    steps.push(Step::ReloadCaddy);
                }
            }
        }
    }

    if !WellKnownService::Authelia.matches(service_name)
        && matches!(
            installed.auth_kind,
            Some(registry::service_def::AuthKind::Oidc)
        )
    {
        steps.extend(authelia::unregister_oidc_client(service_name)?);
    }

    // Drop this service's prometheus scrape-target file (no-op if never
    // registered — the file simply won't exist). Prometheus's file_sd
    // reloader notices the deletion and drops the target.
    steps.extend(prometheus::unregister_scrape_target(service_name)?);

    // Reload systemd after removing quadlet files
    steps.push(Step::DaemonReload);

    match mode {
        RemoveMode::Purge => {
            // Remove podman volumes after containers and units are gone
            for vol_name in volume_names {
                steps.push(Step::RemoveVolume { name: vol_name });
            }
            // Wipe entire service data directory
            steps.push(Step::RemoveDir(service_home(service_name)?));
        }
        RemoveMode::Preserve => {
            // Keep volumes intact — volume_names is guaranteed empty here
            // because accumulation is gated on Purge mode above.
            // Remove only ephemeral children of the home dir; keep data.
            let home = service_home(service_name)?;
            let (data, ephemeral) = crate::data::classify::classify_home_dir(&home)?;
            for path in ephemeral {
                match std::fs::metadata(&path) {
                    Ok(m) if m.is_dir() => steps.push(Step::RemoveDir(path)),
                    Ok(_) => steps.push(Step::RemoveFile(path)),
                    // Path vanished between scan and step emission.
                    // `rm -f` is a no-op on a missing path; keeping the step ensures a
                    // retry of the same plan is idempotent.
                    Err(_) => steps.push(Step::RemoveFile(path)),
                }
            }
            // If the service has no bind-mounted data *and* no podman
            // named volumes, preserve-mode has literally nothing to
            // preserve — the home dir would just be an empty ghost.
            // Drop it in that case. When volumes exist (twenty,
            // postgres, …) we keep the home dir so owner inference in
            // enumerate_all can still attribute the volumes back to
            // this service; `ryra list` then reports a real orphan.
            if data.is_empty() && !has_named_volumes && home.exists() {
                steps.push(Step::RemoveDir(home));
            }
        }
    }

    let url = installed.url.clone();

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
        url,
    })
}

/// Parameters for [`record_pending`].
pub struct RecordPendingParams<'a> {
    pub service_name: &'a str,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub registry_name: &'a str,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
    /// Public URL for this service (browser-visible, e.g., https://docs.example.com).
    pub url: Option<&'a str>,
}

/// Record a service as pending installation (installed: false).
/// Called BEFORE executing steps so that partial failures are recoverable.
pub fn record_pending(params: RecordPendingParams<'_>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    let ports: BTreeMap<String, u16> = params.allocated_ports.iter().cloned().collect();

    config.services.push(InstalledService {
        name: params.service_name.to_string(),
        version: "0.1.0".to_string(),
        repo: params.registry_name.to_string(),
        ports,
        auth_kind: params.auth_kind,
        url: params.url.map(|u| u.to_string()),
        installed: false,
    });

    // Auto-configure [auth] when an auth provider is installed
    if WellKnownService::Authelia.matches(params.service_name) {
        config.auth = Some(authelia::auth_config(params.allocated_ports)?);
    }

    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

/// Mark a pending service as fully installed.
/// Called AFTER all steps have executed successfully.
pub fn mark_installed(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;

    let service = config
        .services
        .iter_mut()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    service.installed = true;
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    config.services.retain(|s| s.name != service_name);
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

/// Steps to purge leftover data/volumes for an orphan service — one with
/// data on disk but no config entry (e.g., after `ryra remove <svc>` in
/// default Preserve mode). Unlike `remove_service`, this doesn't require
/// the service to be in `ryra.toml`.
pub fn orphan_purge_steps(svc: &data::ServiceData) -> Vec<Step> {
    let mut steps = Vec::new();
    for path in &svc.data_paths {
        if path.is_dir() {
            steps.push(Step::RemoveDir(path.clone()));
        } else {
            steps.push(Step::RemoveFile(path.clone()));
        }
    }
    if svc.home_dir.exists() {
        steps.push(Step::RemoveDir(svc.home_dir.clone()));
    }
    for v in &svc.volumes {
        steps.push(Step::RemoveVolume {
            name: v.name.clone(),
        });
    }
    steps
}

/// Reset ryra: tear down all services, infrastructure, and config.
pub fn reset() -> Result<ResetResult> {
    let paths = ConfigPaths::resolve()?;
    let config = match config::load_config(&paths.config_file) {
        Ok(c) => Some(c),
        Err(Error::ConfigNotFound(_)) => None,
        Err(e) => return Err(e),
    };

    let mut steps = Vec::new();
    let mut volume_names = Vec::new();

    // 1. Stop and remove only ryra-managed quadlet files (scoped by installed service names)
    let quadlet_path = quadlet_dir()?;
    let all_names: Vec<&str> = config
        .as_ref()
        .map(|c| c.services.iter().map(|s| s.name.as_str()).collect())
        .unwrap_or_default();
    if let Some(ref config) = config
        && quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Only touch files belonging to a ryra-installed service
            let is_ryra_file = config
                .services
                .iter()
                .any(|s| quadlet_belongs_to(&name, &s.name, &all_names));
            if !is_ryra_file {
                continue;
            }
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".network") {
                let unit = format!("{}-network", name.trim_end_matches(".network"));
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".volume") {
                let vol = name.trim_end_matches(".volume").to_string();
                volume_names.push(format!("systemd-{vol}"));
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // 2. Reload user systemd after removing quadlets
    steps.push(Step::DaemonReload);

    // 3. Remove podman volumes
    for vol_name in volume_names {
        steps.push(Step::RemoveVolume { name: vol_name });
    }

    // 4. Remove service data directories
    if let Some(ref config) = config {
        for service in &config.services {
            if let Ok(data_dir) = service_home(&service.name)
                && data_dir.exists()
            {
                steps.push(Step::RemoveDir(data_dir));
            }
        }
    }

    Ok(ResetResult { steps })
}

/// Called after reset steps succeed — removes ryra's config directory.
pub fn finalize_reset() -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    if paths.config_dir.exists() {
        std::fs::remove_dir_all(&paths.config_dir).map_err(|source| Error::FileWrite {
            path: paths.config_dir,
            source,
        })?;
    }
    Ok(())
}

/// Get the current status of the ryra installation.
pub fn status() -> config::status::RyraStatus {
    let paths = match ConfigPaths::resolve() {
        Ok(p) => p,
        Err(_) => return config::status::RyraStatus::NotInitialized,
    };

    let config = match config::load_config(&paths.config_file) {
        Ok(c) => c,
        Err(Error::ConfigNotFound(_)) => return config::status::RyraStatus::NotInitialized,
        Err(e) => return config::status::RyraStatus::Error(e.to_string()),
    };

    config::status::RyraStatus::Initialized(config::status::StatusInfo::from_config(
        paths.config_file,
        &config,
    ))
}

/// List installed services.
pub fn list_installed() -> Result<Vec<InstalledService>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;
    Ok(config.services)
}

/// Search available services in a repo, optionally filtered by query.
pub fn search_services(repo_dir: &Path, query: Option<&str>) -> Result<Vec<SearchResult>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    let available = registry::list_available(repo_dir)?;

    let results = available
        .into_iter()
        .filter(|reg_svc| match query {
            None => true,
            Some(q) => {
                let q = q.to_lowercase();
                reg_svc.def.service.name.to_lowercase().contains(&q)
                    || reg_svc.def.service.description.to_lowercase().contains(&q)
            }
        })
        .map(|reg_svc| {
            let name = &reg_svc.def.service.name;
            let installed = config.services.iter().any(|s| s.name == *name);
            let mut supports = Vec::new();
            for kind in &reg_svc.def.integrations.auth {
                supports.push(kind.to_string());
            }
            if reg_svc.def.integrations.smtp {
                supports.push("smtp".to_string());
            }
            if reg_svc.def.integrations.prometheus {
                supports.push("prometheus".to_string());
            }
            SearchResult {
                name: name.clone(),
                description: reg_svc.def.service.description,
                installed,
                supports,
            }
        })
        .collect();

    Ok(results)
}

pub struct SearchResult {
    pub name: String,
    pub description: String,
    pub installed: bool,
    /// Integrations this service supports (e.g., "oidc", "smtp").
    pub supports: Vec<String>,
}

/// Get test definitions for an installed service by reading its `test.toml`.
pub async fn service_tests(service_name: &str) -> Result<ServiceTestInfo> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let installed = config
        .services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let service_ref = service_ref_from_installed(installed);
    let repo_dir = resolve_registry_dir(&service_ref).await?;

    let test_toml_path = repo_dir.join(service_name).join("test.toml");
    let env_file = service_home(service_name)?.join(".env");

    if !test_toml_path.exists() {
        return Ok(ServiceTestInfo {
            service_name: service_name.to_string(),
            registry_name: service_ref.registry_name().to_string(),
            tests: vec![],
            env_file,
        });
    }

    let content = std::fs::read_to_string(&test_toml_path).map_err(|source| Error::FileRead {
        path: test_toml_path.clone(),
        source,
    })?;

    #[derive(serde::Deserialize)]
    struct TestFile {
        #[serde(default)]
        tests: Vec<registry::test_def::TestDef>,
    }

    let parsed: TestFile = toml::from_str(&content).map_err(|source| Error::TomlParse {
        path: test_toml_path,
        source,
    })?;

    Ok(ServiceTestInfo {
        service_name: service_name.to_string(),
        registry_name: service_ref.registry_name().to_string(),
        tests: parsed.tests,
        env_file,
    })
}

pub struct ServiceTestInfo {
    pub service_name: String,
    pub registry_name: String,
    pub tests: Vec<registry::test_def::TestDef>,
    pub env_file: PathBuf,
}

/// Get detailed info about a service from a repo.
pub fn service_info(repo_dir: &Path, service_name: &str) -> Result<ServiceDetail> {
    let reg_service = registry::find_service(repo_dir, service_name)?;
    let def = &reg_service.def;

    Ok(ServiceDetail {
        name: def.service.name.clone(),
        description: def.service.description.clone(),
        url: def.service.url.clone(),
        ports: def
            .ports
            .iter()
            .map(|p| (p.container_port, p.protocol.clone(), p.name.clone()))
            .collect(),
        env_vars: def
            .env
            .iter()
            .map(|e| (e.name.clone(), e.prompt.clone()))
            .collect(),
    })
}

pub struct ServiceDetail {
    pub name: String,
    pub description: String,
    pub url: Option<String>,
    pub ports: Vec<(u16, registry::service_def::PortProtocol, String)>,
    pub env_vars: Vec<(String, Option<String>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_url_matches() {
        assert!(is_tailscale_url("http://debian.cobbler-tuna.ts.net"));
        assert!(is_tailscale_url("http://debian.cobbler-tuna.ts.net:10001/"));
        assert!(is_tailscale_url("https://foo.example-net.ts.net"));
        assert!(is_tailscale_url("http://HOST.COBBLER-TUNA.TS.NET"));
    }

    #[test]
    fn tailscale_url_rejects() {
        assert!(!is_tailscale_url("https://nextcloud.internal:8443"));
        assert!(!is_tailscale_url("https://example.com"));
        assert!(!is_tailscale_url("http://127.0.0.1:10001"));
        // lookalike — must be exact `.ts.net` suffix
        assert!(!is_tailscale_url("https://ts.net"));
        assert!(!is_tailscale_url("https://evil-ts.net.example.com"));
        assert!(!is_tailscale_url("not a url"));
    }

    // resolve_extra_networks positional args:
    // (name, enable_auth, authelia_installed, caddy_installed, inbucket_installed,
    //  prometheus_installed, has_url, has_smtp, has_prometheus)

    #[test]
    fn networks_empty_when_no_auth() {
        let nets = resolve_extra_networks(
            "whoami", false, false, false, false, false, false, false, false,
        );
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_empty_when_auth_but_no_authelia() {
        let nets = resolve_extra_networks(
            "forgejo", true, false, false, false, false, false, false, false,
        );
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_authelia_when_auth_enabled() {
        let nets = resolve_extra_networks(
            "forgejo", true, true, false, false, false, false, false, false,
        );
        assert_eq!(nets, vec!["authelia"]);
    }

    #[test]
    fn networks_auth_with_caddy_includes_both() {
        let nets = resolve_extra_networks(
            "forgejo", true, true, true, false, false, false, false, false,
        );
        assert!(nets.contains(&"authelia".to_string()));
        assert!(nets.contains(&"caddy".to_string()));
    }

    #[test]
    fn networks_authelia_excluded_for_authelia_itself() {
        let nets = resolve_extra_networks(
            "authelia", true, true, false, false, false, false, false, false,
        );
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_smtp_joins_inbucket_without_caddy() {
        // Reaching inbucket for SMTP must NOT require caddy.
        let nets = resolve_extra_networks(
            "forgejo", false, false, false, true, false, false, true, false,
        );
        assert_eq!(nets, vec!["inbucket"]);
    }

    #[test]
    fn networks_smtp_skips_inbucket_when_it_is_self() {
        let nets = resolve_extra_networks(
            "inbucket", false, false, false, true, false, false, true, false,
        );
        assert!(!nets.contains(&"inbucket".to_string()));
    }

    #[test]
    fn networks_smtp_skips_inbucket_when_not_installed() {
        let nets = resolve_extra_networks(
            "forgejo", false, false, false, false, false, false, true, false,
        );
        assert!(!nets.contains(&"inbucket".to_string()));
    }

    #[test]
    fn networks_prometheus_joined_when_both_installed_and_supported() {
        // Service supports prometheus AND prometheus is installed → joins network.
        let nets = resolve_extra_networks(
            "forgejo", false, false, false, false, true, false, false, true,
        );
        assert_eq!(nets, vec!["prometheus"]);
    }

    #[test]
    fn networks_prometheus_skipped_when_not_installed() {
        // Service supports prometheus but prometheus not installed → don't join.
        let nets = resolve_extra_networks(
            "forgejo", false, false, false, false, false, false, false, true,
        );
        assert!(!nets.contains(&"prometheus".to_string()));
    }

    #[test]
    fn networks_prometheus_skipped_for_prometheus_itself() {
        let nets = resolve_extra_networks(
            "prometheus",
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            true,
        );
        assert!(!nets.contains(&"prometheus".to_string()));
    }

    #[test]
    fn quadlet_belongs_to_exact_match() {
        let all = &["foo", "foo-bar"];
        assert!(quadlet_belongs_to("foo.container", "foo", all));
        assert!(quadlet_belongs_to("foo.network", "foo", all));
    }

    #[test]
    fn quadlet_belongs_to_sidecar() {
        // foo-db is a sidecar, not a separate service
        let all = &["foo"];
        assert!(quadlet_belongs_to("foo-db.volume", "foo", all));
    }

    #[test]
    fn quadlet_belongs_to_rejects_prefix_collision() {
        let all = &["foo", "foo-bar"];
        assert!(!quadlet_belongs_to("foo-bar.container", "foo", all));
        assert!(!quadlet_belongs_to("foo-bar-db.volume", "foo", all));
    }

    #[test]
    fn quadlet_belongs_to_hyphenated_service() {
        let all = &["foo", "foo-bar"];
        assert!(quadlet_belongs_to("foo-bar.container", "foo-bar", all));
        assert!(quadlet_belongs_to("foo-bar-db.volume", "foo-bar", all));
        assert!(!quadlet_belongs_to("foo.container", "foo-bar", all));
    }
}
