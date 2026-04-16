pub mod authelia;
pub mod caddy;
pub mod config;
pub mod diff;
pub mod error;
pub mod generate;
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
}

impl WellKnownService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caddy => "caddy",
            Self::Authelia => "authelia",
            Self::Inbucket => "inbucket",
        }
    }

    /// Try to match a service name to a well-known service.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "caddy" => Some(Self::Caddy),
            "authelia" => Some(Self::Authelia),
            "inbucket" => Some(Self::Inbucket),
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

/// Data directory for a service: ~/.local/share/ryra/<name>
pub fn service_home(service_name: &str) -> Result<PathBuf> {
    let base = match dirs::data_local_dir() {
        Some(d) => d,
        None => home_dir()?.join(".local/share"),
    };
    Ok(base.join("ryra").join(service_name))
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> Result<PathBuf> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("containers").join("systemd"))
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
}

// --- Result types ---

pub struct InitResult {
    pub steps: Vec<Step>,
}

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

/// Initialize a new ryra project.
pub async fn init(config: Config) -> Result<InitResult> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;

    // Extract bundled registry to cache
    registry::bundled::ensure_bundled(&paths.cache_dir)?;

    // Preserve installed services from existing config
    let mut config = config;
    if let Ok(existing) = config::load_or_default(&paths.config_file)
        && !existing.services.is_empty()
    {
        config.services = existing.services;
    }

    // Write config — stamp the current version
    config.version = Some(config::VERSION.to_string());
    let config_content = toml::to_string_pretty(&config)
        .map_err(|e| Error::Template(format!("failed to serialize config: {e}")))?;

    let steps = vec![Step::WriteFile(GeneratedFile {
        path: paths.config_file.clone(),
        content: config_content,
    })];

    Ok(InitResult { steps })
}

/// Determine which extra podman networks a service should join.
/// Services with auth join authelia's network for OIDC communication.
fn resolve_extra_networks(
    service_name: &str,
    enable_auth: bool,
    authelia_installed: bool,
    caddy_installed: bool,
    has_url: bool,
    has_smtp: bool,
) -> Vec<String> {
    let mut networks = Vec::new();
    if enable_auth && authelia_installed && !WellKnownService::Authelia.matches(service_name) {
        networks.push(WellKnownService::Authelia.to_string());
    }
    // Services join Caddy's network when they need to reach containers
    // on that network: URL-based services for reverse proxy, auth services
    // to reach the OIDC provider via caddy's TLS, inbucket itself,
    // and SMTP-using services to reach inbucket by container name.
    let joins_caddy =
        (has_url || has_smtp || enable_auth || WellKnownService::Inbucket.matches(service_name))
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
    env_overrides: &BTreeMap<String, String>,
    registry_name: &str,
    repo_dir: &Path,
    pre_built_ctx: Option<BTreeMap<String, String>>,
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

    // Determine host port: use fixed host_port from service def if set,
    // allocate one if container port is privileged or already in use,
    // otherwise use container port directly.
    let has_fixed_ports = reg_service.def.ports.iter().any(|p| p.host_port.is_some());
    let mut port_warnings: Vec<Warning> = Vec::new();
    let needs_allocation = reg_service.def.ports.iter().any(|p| {
        p.host_port.is_none()
            && (p.container_port < 1024 || system::port::is_port_in_use(p.container_port))
    });
    let host_port = if has_fixed_ports {
        // Fixed ports are set per-port in the service def (e.g. Caddy 80/443)
        None
    } else if needs_allocation {
        let allocated = system::port::allocate_port(&config)?;
        // Warn about each port that was reassigned
        for p in &reg_service.def.ports {
            if p.host_port.is_none() {
                let reason = if p.container_port < 1024 {
                    "port is privileged (requires root)".to_string()
                } else if system::port::is_port_in_use(p.container_port) {
                    format!("port {} is already in use", p.container_port)
                } else {
                    continue;
                };
                port_warnings.push(Warning::PortReassigned {
                    service_name: service_name.to_string(),
                    port_name: p.name.clone(),
                    original_port: p.container_port,
                    assigned_port: allocated,
                    reason,
                });
            }
        }
        Some(allocated)
    } else {
        None
    };

    // Check for port conflicts by probing whether the port is already bound.
    for p in &reg_service.def.ports {
        let port = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
        if system::port::is_port_in_use(port) {
            return Err(Error::PortConflict { port });
        }
    }

    let home_dir = service_home(service_name)?;
    let quadlet_path = quadlet_dir()?;

    let mut extra_volumes = Vec::new();
    let mut extra_env: BTreeMap<String, String> = BTreeMap::new();

    let authelia_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Authelia.matches(&s.name));
    let caddy_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);

    // When auth is enabled and Caddy handles TLS, mount the Caddy root CA cert
    // into service containers so they trust the self-signed HTTPS cert. OIDC
    // clients connect to Caddy's HTTPS port (via network alias), which requires
    // TLS trust.
    if enable_auth
        && authelia_installed
        && caddy_installed
        && !WellKnownService::Authelia.matches(service_name)
        && !WellKnownService::Caddy.matches(service_name)
    {
        // Create a merged CA bundle: system CAs + caddy's self-signed CA.
        // Always create the bundle and mount — caddy-root-ca.crt may not
        // exist yet (caddy is restarted during `ryra add`), but it will be
        // available by service start time. Services with ExecStartPre
        // merge-ca-bundle.sh will refresh it before the container starts.
        let ca_cert_host = service_home(WellKnownService::Caddy.as_str())?
            .parent()
            .ok_or_else(|| Error::Bundle("caddy service home has no parent directory".into()))?
            .join("caddy-root-ca.crt");
        let service_data = service_home(service_name)?;
        std::fs::create_dir_all(&service_data).ok();
        let merged_bundle = service_data.join("ca-bundle.crt");
        if !merged_bundle.exists() {
            let mut bundle = String::new();
            // Try common system CA bundle paths
            for sys_path in &[
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
            ] {
                if let Ok(content) = std::fs::read_to_string(sys_path) {
                    bundle = content;
                    break;
                }
            }
            // Append caddy's CA if available
            if let Ok(caddy_ca) = std::fs::read_to_string(&ca_cert_host) {
                bundle.push_str("\n# ryra-caddy-ca\n");
                bundle.push_str(&caddy_ca);
            }
            std::fs::write(&merged_bundle, &bundle).ok();
        }
        // Mount the merged bundle as the system CA store
        // :z relabels for SELinux (shared across containers).
        extra_volumes.push(format!(
            "{}:/etc/ssl/certs/ca-certificates.crt:ro,z",
            merged_bundle.display()
        ));
        // Python (requests/certifi) and Node.js don't use the system CA
        // bundle — they need explicit env vars to find the cert.
        extra_env.insert(
            "REQUESTS_CA_BUNDLE".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );
        extra_env.insert(
            "SSL_CERT_FILE".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );
        extra_env.insert(
            "NODE_EXTRA_CA_CERTS".into(),
            "/etc/ssl/certs/ca-certificates.crt".into(),
        );

        // Create a refresh-ca-bundle.sh script that rebuilds the merged CA
        // bundle at service start time. This ensures caddy's self-signed CA
        // is included even if it wasn't available during `ryra add`.
        let service_data = service_home(service_name)?;
        std::fs::create_dir_all(&service_data).ok();
        let refresh_ca_script = service_data.join("refresh-ca-bundle.sh");
        {
            let script = format!(
                "#!/bin/bash\n\
                 CADDY_CA=\"{ryra_dir}/caddy-root-ca.crt\"\n\
                 MERGED=\"{service_data}/ca-bundle.crt\"\n\
                 [ -f \"$CADDY_CA\" ] || exit 0\n\
                 for f in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt; do\n\
                   if [ -f \"$f\" ]; then cp \"$f\" \"$MERGED\"; break; fi\n\
                 done\n\
                 cat \"$CADDY_CA\" >> \"$MERGED\" 2>/dev/null || true\n\
                 exit 0\n",
                ryra_dir = service_data
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                service_data = service_data.display(),
            );
            std::fs::write(&refresh_ca_script, &script).ok();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &refresh_ca_script,
                    std::fs::Permissions::from_mode(0o755),
                )
                .ok();
            }
        }

        // Create a resolve-auth-host.sh script that dynamically resolves
        // caddy's IP at service start time and writes an /etc/hosts override.
        // This is needed because .localhost domains resolve to 127.0.0.1
        // inside containers (RFC 6761), and caddy's IP changes on restart.
        let auth_host_script = service_data.join("resolve-auth-host.sh");
        if let Some(auth_service) = config
            .services
            .iter()
            .find(|s| WellKnownService::Authelia.matches(&s.name))
            && let Some(ref auth_url) = auth_service.url
            && let Ok(parsed) = url::Url::parse(auth_url)
            && let Some(host) = parsed.host_str()
        {
            let script = format!(
                "#!/bin/bash\n\
                 # Resolve caddy's current IP for .localhost auth domains\n\
                 HOSTS=\"{service_data}/auth-hosts.txt\"\n\
                 CADDY_IP=$(podman inspect caddy --format '{{{{range .NetworkSettings.Networks}}}}{{{{.IPAddress}}}} {{{{end}}}}' 2>/dev/null | awk '{{print $1}}')\n\
                 if [ -n \"$CADDY_IP\" ]; then\n\
                   echo \"$CADDY_IP {host}\" > \"$HOSTS\"\n\
                 else\n\
                   echo \"127.0.0.1 {host}\" > \"$HOSTS\"\n\
                 fi\n\
                 exit 0\n",
                service_data = service_data.display(),
                host = host,
            );
            std::fs::write(&auth_host_script, &script).ok();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&auth_host_script, std::fs::Permissions::from_mode(0o755))
                    .ok();
            }
            // Create placeholder hosts file for Volume= mount
            let auth_hosts = service_data.join("auth-hosts.txt");
            if !auth_hosts.exists() {
                std::fs::write(&auth_hosts, format!("127.0.0.1 {host}\n")).ok();
            }
            // Mount the dynamic hosts file
            extra_volumes.push(format!("{}:/etc/hosts:z", auth_hosts.display()));
        }
    }
    let has_smtp = reg_service.def.integrations.smtp
        && !reg_service.def.mappings.smtp.is_empty()
        && config.smtp.is_some();
    let extra_networks = resolve_extra_networks(
        service_name,
        enable_auth,
        authelia_installed,
        caddy_installed,
        url.is_some(),
        has_smtp,
    );

    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind: auth_kind.as_ref(),
        host_port,
        env_overrides,
        url,
        extra_env,
        pre_built_ctx,
    })?;

    let podman_args: Vec<String> = Vec::new();

    // Note: .localhost auth domains (e.g. auth.localhost) can't be resolved
    // inside containers — RFC 6761 hardcodes them to 127.0.0.1. The caddy
    // container's IP is dynamic and changes on restart, so we can't use
    // static --add-host. Instead, auth-enabled services get a shared
    // resolve-auth-host.sh ExecStartPre script that dynamically resolves
    // caddy's current IP at service start time.

    // Build port variable expansions for quadlet PublishPort directives
    let port_vars: Vec<(String, String)> = reg_service
        .def
        .ports
        .iter()
        .map(|p| {
            let resolved = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
            (
                format!("RYRA_PORT_{}", p.name.to_uppercase()),
                resolved.to_string(),
            )
        })
        .collect();

    // Collect ExecStartPre commands for auth scripts
    let mut extra_exec_start_pre: Vec<String> = Vec::new();
    let refresh_ca = service_home(service_name)
        .ok()
        .map(|d| d.join("refresh-ca-bundle.sh"));
    if let Some(ref script) = refresh_ca
        && script.exists()
    {
        extra_exec_start_pre.push(format!("-/bin/bash {}", script.display()));
    }
    let auth_host_script = service_home(service_name)
        .ok()
        .map(|d| d.join("resolve-auth-host.sh"));
    if let Some(ref script) = auth_host_script
        && script.exists()
    {
        extra_exec_start_pre.push(format!("-/bin/bash {}", script.display()));
    }

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

    // 5. Write .env file
    steps.push(Step::WriteFile(output.env_file));

    // 6. Create bind mount directories (must exist before container starts)
    for dir in &bundle.bind_mount_dirs {
        steps.push(Step::CreateDir(dir.clone()));
    }

    // 7. Register OIDC client with the auth provider BEFORE starting the service.
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

    // 8. Add Caddy route for services with a URL when Caddy is installed.
    // This creates a reverse proxy from the service's domain to its container port.
    if let Some(url) = url
        && caddy_installed
        && !WellKnownService::Caddy.matches(service_name)
    {
        let parsed = url::Url::parse(url)
            .map_err(|e| Error::Template(format!("invalid service URL '{url}': {e}")))?;
        let domain = parsed.host_str().unwrap_or(url);
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
    }

    // 9. Reload and start via systemd
    steps.push(Step::DaemonReload);
    // Start — dependencies start automatically via Requires=/After= in the quadlet
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    // Collect post-install info
    let allocated_ports: Vec<(String, u16)> = reg_service
        .def
        .ports
        .iter()
        .map(|p| {
            let port = p.host_port.unwrap_or(host_port.unwrap_or(p.container_port));
            (p.name.clone(), port)
        })
        .collect();

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
fn quadlet_belongs_to(filename: &str, service_name: &str, all_service_names: &[&str]) -> bool {
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

/// Remove a service: update state, return cleanup steps.
pub fn remove_service(service_name: &str) -> Result<RemoveResult> {
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
            // Track volume names for cleanup after containers are stopped
            if name.ends_with(".volume") {
                let vol = name.trim_end_matches(".volume").to_string();
                // Quadlet prefixes volume names with "systemd-"
                volume_names.push(format!("systemd-{vol}"));
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // Reload systemd after removing quadlet files
    steps.push(Step::DaemonReload);

    // Remove podman volumes after containers and units are gone
    for vol_name in volume_names {
        steps.push(Step::RemoveVolume { name: vol_name });
    }

    let url = installed.url.clone();

    // Remove service data directory
    steps.push(Step::RemoveDir(service_home(service_name)?));

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

    // Save a snapshot of the service.toml for `ryra diff`
    let service_toml = params
        .repo_dir
        .join(params.service_name)
        .join("service.toml");
    let content = std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
        path: service_toml,
        source,
    })?;
    config::save_snapshot(&paths.snapshots_dir, params.service_name, &content)?;

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
    config::remove_snapshot(&paths.snapshots_dir, service_name)?;

    Ok(())
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
    fn networks_empty_when_no_auth() {
        let nets = resolve_extra_networks("whoami", false, false, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_empty_when_auth_but_no_authelia() {
        let nets = resolve_extra_networks("forgejo", true, false, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_authelia_when_auth_enabled() {
        let nets = resolve_extra_networks("forgejo", true, true, false, false, false);
        assert_eq!(nets, vec!["authelia"]);
    }

    #[test]
    fn networks_auth_with_caddy_includes_both() {
        let nets = resolve_extra_networks("forgejo", true, true, true, false, false);
        assert!(nets.contains(&"authelia".to_string()));
        assert!(nets.contains(&"caddy".to_string()));
    }

    #[test]
    fn networks_authelia_excluded_for_authelia_itself() {
        let nets = resolve_extra_networks("authelia", true, true, false, false, false);
        assert!(nets.is_empty());
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
