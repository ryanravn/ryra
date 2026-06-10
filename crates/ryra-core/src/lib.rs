pub mod auth_bridge;
pub mod authelia;
pub mod backup;
pub mod caddy;
pub mod capability;
pub mod config;
pub mod configure;
pub mod data;
pub mod error;
pub mod exposure;
pub mod generate;
pub mod manifest;
pub mod metadata;
pub mod paths;
pub mod plan;
pub mod registry;
pub mod system;
pub mod upgrade;
pub mod well_known;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use config::ConfigPaths;
use config::schema::InstalledService;
use error::{Error, Result};

pub use capability::{
    Capability, any_installed_provider, find_installed_provider, installed_provides,
    service_provides,
};
pub use configure::{
    ConfigureChange, ConfigureResult, ExposureChange, Overrides as ConfigureOverrides,
    configure_service,
};
pub use exposure::{
    Exposure, check_auth_exposure_compat, is_caddy_local_url, is_public_url, is_tailscale_url,
};
pub use generate::GeneratedFile;
pub use manifest::{ManifestEntry, manifest_path};
pub use metadata::{Metadata, load_metadata};
pub use paths::{
    CONFIG_DIR_ENV, DATA_DIR_ENV, DEFAULT_REGISTRY_URL, REGISTRY_DEFAULT, REGISTRY_DIR_ENV,
    metadata_path, quadlet_dir, service_data_root, service_home, systemd_user_dir,
};
pub use plan::{AddResult, RemoveResult, ResetResult, Step, TailscalePort, TrackedEnv, Warning};
pub use upgrade::{
    BackupSnapshot, DEFAULT_BACKUP_KEEP, DiffEntry, DiffKind, DiffResult, EnvAddition,
    RevertResult, UpgradeResult, diff_service, list_backups, prune_backups, revert_service,
    upgrade_service,
};
pub use well_known::WellKnownService;

pub(crate) use paths::home_dir;
pub(crate) use well_known::caddy_https_port;

/// Resolve the registry directory for a service reference.
pub async fn resolve_registry_dir(service_ref: &registry::resolve::ServiceRef) -> Result<PathBuf> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_cache_dir()?;
    let config = config::load_or_default(&paths.config_file)?;
    registry::resolve::resolve_registry_dir(service_ref, &config, &paths.cache_dir).await
}

/// Build a ServiceRef from an installed service's stored registry name.
pub fn service_ref_from_installed(installed: &InstalledService) -> registry::resolve::ServiceRef {
    if installed.repo.is_empty() || installed.repo == REGISTRY_DEFAULT {
        registry::resolve::ServiceRef::Default(installed.name.clone())
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
    quadlet_path: &std::path::Path,
    _repo_dir: Option<&std::path::Path>,
) -> Vec<Step> {
    let mut steps = Vec::new();
    // Which join-relevant capability did the new service just become a
    // provider of? `OidcProvider` doesn't trigger this — auth-aware
    // services join its network at install time via the auth bridge,
    // not retroactively.
    let new_cap = if service_provides(new_service, Capability::ReverseProxy) {
        Capability::ReverseProxy
    } else if service_provides(new_service, Capability::SmtpRelay) {
        Capability::SmtpRelay
    } else {
        return steps;
    };

    let installed = list_installed().unwrap_or_default();
    for svc in &installed {
        // Providers (of any capability) don't join themselves to other
        // providers' networks via this path.
        if !svc.provides.is_empty() {
            continue;
        }
        let (network_name, should_join) = match new_cap {
            Capability::ReverseProxy => {
                // Services with a routed URL want the proxy network.
                // Tailscale-exposed services route via `tailscale serve`,
                // not the reverse proxy, so they skip.
                let wants_proxy = matches!(
                    svc.exposure,
                    Exposure::Internal { .. } | Exposure::Public { .. }
                );
                (new_service.to_string(), wants_proxy)
            }
            Capability::SmtpRelay => {
                // Any already-installed service whose .env points SMTP at
                // the relay's hostname needs to reach it.
                (
                    new_service.to_string(),
                    service_uses_smtp_relay(&svc.name, new_service),
                )
            }
            // Unreachable: `new_cap` was selected from the two cases
            // above. Match exhaustively so a new join-relevant capability
            // forces a compile error here.
            Capability::OidcProvider | Capability::ForwardAuthProvider => {
                continue;
            }
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
        let installed_names_owned: Vec<String> = installed.iter().map(|s| s.name.clone()).collect();
        let all_service_names: Vec<&str> =
            installed_names_owned.iter().map(|s| s.as_str()).collect();
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

/// Heuristic: does this service's `.env` point SMTP at the given relay's
/// container hostname? Matches any line whose value is `<relay>` or
/// `<relay>:<port>` — covers the common shape
/// `SOMETHING_SMTP_HOST=<relay>` and variants like
/// `FORGEJO__mailer__SMTP_ADDR=<relay>`.
fn service_uses_smtp_relay(service_name: &str, relay_host: &str) -> bool {
    let env_path = match service_home(service_name) {
        Ok(h) => h.join(".env"),
        Err(_) => return false,
    };
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let with_port = format!("{relay_host}:");
    content.lines().any(|line| {
        let Some((_, value)) = line.split_once('=') else {
            return false;
        };
        let v = value.trim();
        v == relay_host || v.starts_with(&with_port)
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
    has_url: bool,
    has_smtp: bool,
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
    let joins_caddy = (has_url || enable_auth || WellKnownService::Inbucket.matches(service_name))
        && caddy_installed
        && !WellKnownService::Caddy.matches(service_name);
    if joins_caddy && !networks.contains(&WellKnownService::Caddy.to_string()) {
        networks.push(WellKnownService::Caddy.to_string());
    }
    networks
}

/// Why the planner is running. The render path is shared between fresh
/// installs and re-renders (`ryra upgrade`); the side-effect steps are
/// not — re-registering an OIDC client on upgrade would mint a new
/// `client_id`/`client_secret` against authelia's existing entry, and
/// patching every other installed service's quadlet (retroactive network
/// joins) is install-time work. The mode gates those.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    /// Fresh install. Validate that the service isn't already on disk,
    /// register OIDC clients, retroactively patch other services to
    /// join shared networks, set up Tailscale, and start the unit.
    Add,
    /// Re-render an installed service to pick up registry changes.
    /// Skips the validation rejects and the install-time side effects;
    /// the upgrade caller handles diff/backup/restart.
    Upgrade,
}

/// The user's auth decision for one install.
///
/// Replaces the `(Option<AuthKind>, bool)` pair that previously travelled
/// through the planner, where `enable_auth = false` with a `Some` kind was
/// representable but meaningless. The two consumers read different facets:
/// [`Self::enabled`] drives the auth fabric (network joins, the auth
/// bridge, the no-native-OIDC validation), [`Self::native_kind`] drives
/// OIDC env templating and client registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthChoice {
    /// No auth integration.
    None,
    /// Auth with the service's native OIDC integration: OIDC env vars are
    /// templated and a client is registered with the provider.
    Native(registry::service_def::AuthKind),
    /// Auth requested for a service that declares no native auth kinds.
    /// The planner accepts this only when the service itself provides
    /// OIDC (the provider doesn't act as a client of itself) and rejects
    /// anything else with [`Error::NoOidcSupport`].
    Requested,
}

impl AuthChoice {
    /// True when the user asked for auth at all, natively or not.
    pub fn enabled(&self) -> bool {
        !matches!(self, AuthChoice::None)
    }

    /// The native OIDC kind, when the service has one.
    pub fn native_kind(&self) -> Option<&registry::service_def::AuthKind> {
        match self {
            AuthChoice::Native(kind) => Some(kind),
            AuthChoice::None | AuthChoice::Requested => None,
        }
    }
}

/// Inputs to [`add_service`]. One typed request instead of a positional
/// argument list, so call sites (CLI today, other frontends later) name
/// what they're asking for and the retry path can't drift out of sync
/// with the original call.
pub struct AddServiceParams<'a> {
    pub service_name: &'a str,
    pub exposure: &'a Exposure,
    pub auth: AuthChoice,
    pub enable_smtp: bool,
    pub enable_backup: bool,
    pub env_overrides: &'a BTreeMap<String, String>,
    pub enabled_groups: &'a std::collections::BTreeSet<String>,
    pub registry_name: &'a str,
    pub repo_dir: &'a Path,
    /// When provided, its secrets and auth credentials are reused instead
    /// of generating fresh ones. Pass the context from the interactive
    /// prompt phase so the values the user saw match what gets written.
    pub pre_built_ctx: Option<BTreeMap<String, String>>,
    pub port_in_use: &'a dyn Fn(u16) -> bool,
    pub acme_mode: Option<&'a caddy::AcmeMode>,
    pub mode: PlanMode,
    /// Pin specific port assignments by name (e.g. `{"http": 10005}`)
    /// instead of running the allocator. Used by upgrade so a re-render
    /// preserves the install's existing host ports — port_in_use would
    /// say they're taken (the running service holds them) and the
    /// allocator would skip to the next free one.
    pub port_overrides: &'a BTreeMap<String, u16>,
}

/// Add a service: generate config, return steps to execute.
pub fn add_service(params: AddServiceParams<'_>) -> Result<AddResult> {
    let AddServiceParams {
        service_name,
        exposure,
        auth,
        enable_smtp,
        enable_backup,
        env_overrides,
        enabled_groups,
        registry_name,
        repo_dir,
        pre_built_ctx,
        port_in_use,
        acme_mode,
        mode,
        port_overrides,
    } = params;
    // Derived views of the typed inputs, bound once for the many body
    // sites that only need one facet. Helpers that need the full picture
    // (env templating, exposure-variant dispatch) take `&Exposure` /
    // `&AuthChoice` facets directly.
    let auth_kind: Option<&registry::service_def::AuthKind> = auth.native_kind();
    let enable_auth: bool = auth.enabled();
    let url: Option<&str> = exposure.url();
    let paths = ConfigPaths::resolve()?;
    let config = config::load_or_default(&paths.config_file)?;

    // Quadlet directory is the source of truth: a marker'd `.container`
    // means the service is already installed.
    //
    // Upgrade explicitly *re-renders* an installed service — those rejects
    // would block the legitimate path.
    if mode == PlanMode::Add {
        if is_service_installed(service_name) {
            return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
        }

        // No config entry, but preserved volumes or a lingering home dir from
        // `ryra remove <svc>` (default Preserve mode) would make the fresh .env's
        // generated secrets disagree with what's already baked into the volume —
        // postgres writes POSTGRES_PASSWORD into pgdata on first init and then
        // skips reinit, so a new password in .env just restart-loops on auth
        // failures. Surface the same way as an incomplete install; the CLI's
        // existing purge-and-retry recovery handles it.
        if data::enumerate_service(service_name)?.is_some() {
            return Err(Error::ServiceIncomplete(service_name.to_string()));
        }
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
        .filter(|r| !is_service_installed(&r.service))
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

    // --auth requires native OIDC support; forward auth is no longer supported.
    // The exception is the OIDC provider itself, which doesn't need to act as
    // a client of itself.
    if enable_auth
        && reg_service.def.integrations.auth.is_empty()
        && !capability::def_provides(&reg_service.def, Capability::OidcProvider)
    {
        return Err(Error::NoOidcSupport(service_name.to_string()));
    }

    // --backup requires the service author to have certified backup
    // safety. Refusing here means a user typo can't silently produce
    // an install whose backups would never restore cleanly.
    if enable_backup && !reg_service.def.integrations.backup {
        return Err(Error::BackupNotSupported(service_name.to_string()));
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
        let host = if let Some(pinned) = port_overrides.get(&p.name) {
            // Upgrade passes the install's existing port here so re-renders
            // are stable. Trust the caller — port_in_use would say it's
            // taken (the running service holds it) and the allocator would
            // pick a different one.
            *pinned
        } else if let Some(hp) = p.host_port {
            hp
        } else {
            let privileged = p.container_port < 1024;
            let claimed_in_service = claimed.contains(&p.container_port);
            let in_use = port_in_use(p.container_port);
            if privileged || claimed_in_service || in_use {
                let allocated = system::port::allocate_port_excluding(&claimed, port_in_use)?;
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

    // Caddy on rootless podman can't bind <1024 by default — service.toml
    // therefore declares 8080/8443 as the host ports. When the kernel has
    // been retuned (`sysctl net.ipv4.ip_unprivileged_port_start=80`), we
    // can listen on 80/443 directly: cleaner URLs, no router NAT
    // translation needed. Override here so the quadlet's `PublishPort=`
    // and the stored config record both reflect the real listen port.
    if WellKnownService::Caddy.matches(service_name)
        && system::sysctl::rootless_can_bind_low_ports()
    {
        for (name, port) in resolved_ports.iter_mut() {
            match name.as_str() {
                "http" if *port == 8080 => *port = 80,
                "https" if *port == 8443 => *port = 443,
                _ => {}
            }
        }
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

    // Authoritative: capability presence in `list_installed` answers
    // "is there a reverse-proxy / OIDC IdP / SMTP relay / metrics
    // scraper installed?" without naming any specific provider.
    let installed_now = list_installed().unwrap_or_default();
    let authelia_installed =
        find_installed_provider(&installed_now, Capability::OidcProvider).is_some();
    let caddy_installed =
        find_installed_provider(&installed_now, Capability::ReverseProxy).is_some();
    let inbucket_installed =
        find_installed_provider(&installed_now, Capability::SmtpRelay).is_some();

    // Build auth-bridge artifacts (CA trust + dynamic /etc/hosts for the
    // auth provider's domain). Pure — all filesystem writes are
    // emitted as Step::WriteFile below, not performed here.
    let auth_bridge = auth_bridge::build(&auth_bridge::AuthBridgeParams {
        service_name,
        service_provides: &reg_service.def.capabilities.provides,
        enable_auth,
        config: &config,
        installed: &installed_now,
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
        url.is_some(),
        has_smtp,
    );

    let output = generate::generate_env(generate::GenerateEnvParams {
        config: &config,
        service_def: &reg_service.def,
        auth_kind,
        host_port,
        resolved_ports: &resolved_ports,
        env_overrides,
        exposure,
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
                format!("SERVICE_PORT_{}", name.to_uppercase()),
                port.to_string(),
            )
        })
        .collect();

    // Build install metadata persisted to `metadata.toml` in service_home.
    // This is what makes service state authoritative for "what's installed
    // and how is it wired" — every field a future `ryra list` needs to
    // reconstruct the install reads back from this file.
    // `smtp_enabled` captures *user intent* (the `enable_smtp` flag the
    // caller passed) rather than the gated render flag (`has_smtp`).
    // Otherwise an install on a host without globally-configured SMTP
    // records `false`, and a later `ryra configure` that doesn't touch
    // SMTP would still show as "modified" because the legacy default
    // (`true`) disagrees with what gets serialized. Storing intent keeps
    // metadata stable across re-renders and lets `ryra configure --smtp`
    // remember the choice even before global SMTP is configured.
    let install_metadata = Metadata {
        registry: registry_name.to_string(),
        url: url.map(str::to_string),
        auth: auth_kind.cloned(),
        provides: reg_service.def.capabilities.provides.clone(),
        backup_enabled: enable_backup,
        smtp_enabled: enable_smtp,
        enabled_groups: enabled_groups.iter().cloned().collect(),
        runtime: reg_service.def.service.runtime.clone(),
    };

    // Native services have no quadlet bundle / image: build the binary, install
    // it, write a plain systemd --user unit, and start it. Returns here so the
    // entire podman path below stays untouched. Reuses everything already
    // computed: home_dir, the generated .env (`output`), ports, and metadata.
    if reg_service.def.service.runtime == registry::service_def::Runtime::Native {
        let tracked_envs = collect_static_envs(&reg_service.def, &output.ctx, enabled_groups)?;
        let allocated_ports = resolved_ports.clone();
        let generated_secrets = collect_generated_secrets(&reg_service.def, env_overrides);
        return build_native_add(NativeAddParams {
            service_name,
            reg_service: &reg_service,
            home_dir: &home_dir,
            output,
            install_metadata: &install_metadata,
            registry_name,
            url,
            tracked_envs,
            allocated_ports,
            generated_secrets,
        });
    }

    // Process quadlet bundle from registry
    let bundle =
        generate::bundle::process_quadlet_bundle(&generate::bundle::ProcessBundleParams {
            service_dir: &reg_service.service_dir,
            service_name,
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

    // 3. Write quadlet files from bundle (real files live in service_home)
    //    and symlink each one into the systemd-mandated quadlet path so
    //    quadlet's generator finds them on daemon-reload.
    for file in bundle.quadlet_files {
        let link = file
            .path
            .file_name()
            .map(|n| quadlet_path.join(n))
            .ok_or_else(|| {
                Error::Bundle(format!("invalid quadlet path: {}", file.path.display()))
            })?;
        let target = file.path.clone();
        steps.push(Step::WriteFile(file));
        steps.push(Step::Symlink { link, target });
    }

    // 3b. Write metadata.toml — install record (registry, exposure, url,
    // auth) used by `ryra list` / `remove` / `status` to reconstruct the
    // install. Emitted *before* any step that can fail remotely (Tailscale
    // API calls, image pulls) so a partial install can still be torn down
    // by `remove_service` — which relies on metadata.toml to identify the
    // install. World-readable mode (atomic_write picks 0o644 by name).
    let metadata_content = toml::to_string_pretty(&install_metadata)?;
    steps.push(Step::WriteFile(GeneratedFile {
        path: metadata_path(service_name)?,
        content: metadata_content,
    }));

    // 3c. Tailscale Services — when `--tailscale` was used, the host's
    // existing tailscaled advertises the service at
    // `https://<name>.<tailnet>.ts.net` (TailVIP-routed). One-time
    // `TailscaleSetup` ensures ACL tags + auto-approval are in place;
    // `TailscaleEnable` defines the service via admin API and runs
    // `tailscale serve --service=...` from the host. No sidecar
    // containers, no per-service tailscaled.
    if mode == PlanMode::Add && exposure.is_tailscale() {
        // Scope the Tailscale Service name by host (`<service>-<host>`)
        // — Tailscale Services are global per tailnet, so without the
        // suffix two ryra machines that both `ryra add vikunja --tailscale`
        // would silently stomp each other's registration. The svc_name
        // falls out of the exposure URL (built by
        // `system::tailscale::derive_service_url` with the host suffix)
        // — keeping URL as the single source of
        // truth means `metadata.toml` round-trips and remove paths
        // recover the same name without re-shelling tailscale.
        let svc_name = exposure.tailscale_svc_name().ok_or_else(|| {
            Error::InvalidServiceRef(format!(
                "tailscale exposure for '{service_name}' has a malformed URL — \
                 expected `https://<service>-<host>.<tailnet>.ts.net/`"
            ))
        })?;
        // A multi-port service (e.g. ente: web UI + API) serves each
        // tailscale_https port; single-port services serve their primary
        // port at the web root. Empty only when there are no ports at all.
        let ts_ports = plan::tailscale_ports(&reg_service.def.ports, &resolved_ports, host_port);
        if !ts_ports.is_empty() {
            steps.push(Step::TailscaleSetup);
            steps.push(Step::TailscaleEnable {
                svc_name,
                ports: ts_ports,
            });
        }
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
    //
    // Skipped on upgrade: build_context generates a fresh client_id/secret
    // every call, so re-registering would append a second entry to authelia's
    // configuration.yml and break the existing OIDC integration.
    if mode == PlanMode::Add
        && let (
            Some(registry::service_def::AuthKind::Oidc),
            Some(config::schema::AuthCredentials::Authelia { .. }),
        ) = (auth_kind, config.auth.as_ref())
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
    // Tailscale exposures skip this: the service is already reachable on
    // its host port via the tailnet, and MagicDNS handles the hostname.
    if let Some(url) = url
        && !WellKnownService::Caddy.matches(service_name)
        && !exposure.is_tailscale()
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
            let primary_quadlet = reg_service
                .service_dir
                .join("quadlets")
                .join(format!("{service_name}.container"));
            let target_host = caddy::primary_container_name(&primary_quadlet, service_name);
            let block = caddy::render_site_block(&caddy::CaddySiteParams {
                service_name: service_name.to_string(),
                target_host,
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
            // --url was passed but no ryra-managed reverse proxy is installed.
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

    // 9b. When a shared-network provider (caddy, inbucket) is being
    // installed, retroactively patch services that were installed before
    // it so they can reach the new provider by container DNS.
    // resolve_extra_networks only decides at install time; without this
    // step, services installed earlier remain isolated.
    //
    // Skipped on upgrade: re-running on the same shared-network provider
    // would re-patch services unnecessarily, and a re-render of a regular
    // service shouldn't touch its peers' quadlets.
    if mode == PlanMode::Add {
        steps.extend(retroactive_network_joins(
            service_name,
            &quadlet_path,
            Some(repo_dir),
        ));
    }

    // 9d. Caddy: seed the user-owned `tls.caddy` snippet on first install.
    // Site blocks emit `import services_tls`; this file defines that snippet.
    // After first write ryra never touches it — users edit it directly
    // for Cloudflare DNS-01, wildcards, BYO certs, plain HTTP for Tunnel,
    // anything Caddy supports. seed-caddyfile.sh defensively recreates
    // the file as `tls internal` on container start if it goes missing.
    if WellKnownService::Caddy.matches(service_name) {
        let snippet_path = caddy::tls_snippet_path()?;
        if !snippet_path.exists() {
            let mode = acme_mode.cloned().unwrap_or(caddy::AcmeMode::Internal);
            steps.push(Step::WriteFile(GeneratedFile {
                path: snippet_path,
                content: mode.snippet(),
            }));
        }
    }

    // 9z. Manifest — sha256 list of every file we just emitted, written to
    // `~/.local/share/services/<svc>/service.manifest` so `ryra diff` and
    // `ryra upgrade` can detect drift between the registry and what's
    // actually on disk. `.env` is excluded because it carries generated
    // secrets that legitimately rotate at runtime; the manifest itself is
    // excluded to avoid the chicken-and-egg of hashing itself. CopyFile
    // sources (binary plugin payloads) are not yet covered — drift on
    // those is rare and adds I/O at plan time. Revisit if it bites.
    let manifest_path_for_svc = manifest::manifest_path(service_name)?;
    let env_filename = std::ffi::OsStr::new(".env");
    let mut manifest_entries: Vec<manifest::ManifestEntry> = Vec::new();
    for step in &steps {
        if let Step::WriteFile(file) = step {
            if file.path == manifest_path_for_svc {
                continue;
            }
            if file.path.file_name() == Some(env_filename) {
                continue;
            }
            manifest_entries.push(manifest::ManifestEntry {
                path: file.path.clone(),
                sha256: manifest::hash_bytes(file.content.as_bytes()),
            });
        }
    }
    // Static env vars — every registry-defined env whose template carries
    // no `{{secret.*}}` or `{{auth.*}}` reference. Tracked so `ryra
    // upgrade` can append registry-added env vars to the user's existing
    // `.env` without re-rendering it (which would clobber rotated
    // secrets). Append-only by design. The richer `tracked_envs` is what
    // upgrade uses to decide whether to prompt the user; the on-disk
    // manifest only records key+value (the `# env: KEY=VAL` lines).
    let tracked_envs = collect_static_envs(&reg_service.def, &output.ctx, enabled_groups)?;
    let manifest_envs: Vec<manifest::EnvEntry> = tracked_envs
        .iter()
        .map(|t| manifest::EnvEntry {
            key: t.key.clone(),
            value: t.value.clone(),
        })
        .collect();
    steps.push(Step::WriteFile(GeneratedFile {
        path: manifest_path_for_svc,
        content: manifest::format(&manifest_entries, &manifest_envs),
    }));

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
        tracked_envs,
    })
}

/// Secret names referenced by a service's env templates (for the install
/// summary; values live in `.env`, not state). Shared by add paths.
fn collect_generated_secrets(
    def: &registry::service_def::ServiceDef,
    env_overrides: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out: Vec<String> = def
        .env
        .iter()
        .filter(|e| !env_overrides.contains_key(&e.name))
        .flat_map(|e| generate::extract_secret_refs(&e.value))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Inputs for [`build_native_add`] — grouped to keep the signature sane.
struct NativeAddParams<'a> {
    service_name: &'a str,
    reg_service: &'a registry::RegistryService,
    home_dir: &'a Path,
    output: generate::EnvOutput,
    install_metadata: &'a Metadata,
    registry_name: &'a str,
    url: Option<&'a str>,
    tracked_envs: Vec<TrackedEnv>,
    allocated_ports: Vec<(String, u16)>,
    generated_secrets: Vec<String>,
}

/// Plan a `runtime = "native"` install: build the binary (unless prebuilt),
/// install it, write the service `.env`, render a plain `systemd --user` unit
/// and link it, then start. No image, no quadlet — but the same `.env`
/// contract (`SERVICE_PORT_HTTP`, etc.) and the same `service_home` the rest of
/// ryra (Caddy routing, whole-folder backups) already understands.
fn build_native_add(p: NativeAddParams<'_>) -> Result<AddResult> {
    let NativeAddParams {
        service_name,
        reg_service,
        home_dir,
        output,
        install_metadata,
        registry_name,
        url,
        tracked_envs,
        allocated_ports,
        generated_secrets,
    } = p;

    let run = reg_service.def.service.run.as_ref().ok_or_else(|| {
        Error::Bundle(format!(
            "native service '{service_name}' is missing its `run` command"
        ))
    })?;
    let build = reg_service.def.service.build.as_ref();

    let env_content = output.env_file.content.clone();
    let source_dir = reg_service.service_dir.clone();
    let mut steps = Vec::new();

    // The service home holds STATE only: data/, .env, the unit, the install
    // record. The service itself runs from its source dir (no binary copy), so
    // a plain `target/release/app`, `bun run src/index.ts`, or `cargo watch`
    // all work the same and a rebuild lands where the unit already looks.
    steps.push(Step::CreateDir(home_dir.to_path_buf()));
    steps.push(Step::CreateDir(home_dir.join("data")));

    // Optional build/prepare step (cargo build, bun install) in the source dir.
    if let Some(command) = build {
        steps.push(Step::Build {
            dir: source_dir.clone(),
            command: command.clone(),
        });
    }

    // Install record + the generated .env (carries SERVICE_PORT_HTTP).
    steps.push(Step::WriteFile(GeneratedFile {
        path: metadata_path(service_name)?,
        content: toml::to_string_pretty(install_metadata)?,
    }));
    steps.push(Step::WriteFile(output.env_file));

    // The unit: real file in the service home, symlinked into the systemd
    // --user dir so the unit is found on daemon-reload (mirrors quadlets).
    let unit_name = format!("{service_name}.service");
    let unit_path = home_dir.join(&unit_name);
    steps.push(Step::WriteFile(GeneratedFile {
        path: unit_path.clone(),
        content: native_unit(
            home_dir,
            &source_dir,
            run,
            &reg_service.def.service.description,
        ),
    }));
    steps.push(Step::Symlink {
        link: systemd_user_dir()?.join(&unit_name),
        target: unit_path,
    });

    steps.push(Step::DaemonReload);
    steps.push(Step::StartService {
        unit: service_name.to_string(),
    });

    Ok(AddResult {
        steps,
        warnings: Vec::new(),
        repo_url: registry_name.to_string(),
        allocated_ports,
        generated_secrets,
        env_content,
        url: url.map(|u| u.to_string()),
        tracked_envs,
    })
}

/// Render a plain `systemd --user` unit for a native service. `EnvironmentFile`
/// supplies the service `.env` (so `SERVICE_PORT_HTTP` and friends are present);
/// `SERVICE_HOME` points the process at its data dir, matching the contract a
/// container service gets via the quadlet.
fn native_unit(home_dir: &Path, source_dir: &Path, run: &str, description: &str) -> String {
    let home = home_dir.display();
    let source = source_dir.display();
    // ExecStart via `sh -c 'exec <run>'` so a binary path (`target/release/app`),
    // an interpreter command (`bun run src/index.ts`), and a watcher
    // (`cargo watch -x run`) all work the same. `exec` replaces the shell so
    // systemd tracks the real PID and stop/restart reach the process.
    //
    // A user-level unit's PATH is minimal, so toolchains installed under $HOME
    // (bun, cargo, deno, go, pipx) wouldn't be found. Prepend the common ones
    // (%h = the user's home) so `run = "bun ..."` / `"cargo ..."` just work.
    format!(
        "[Unit]\n\
         Description={description}\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={source}\n\
         EnvironmentFile={home}/.env\n\
         Environment=SERVICE_HOME={home}\n\
         Environment=PATH=%h/.local/bin:%h/.cargo/bin:%h/.bun/bin:%h/.deno/bin:%h/go/bin:/usr/local/bin:/usr/bin:/bin\n\
         ExecStart=/bin/sh -c 'exec {run}'\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    )
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
    // Reconstruct the InstalledService view from the quadlet's
    // `# Service-*` headers — that's the source of truth now.
    let installed_owned = build_installed_from_metadata(service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;
    let installed = &installed_owned;

    // Native services have no quadlets / podman objects: tear down the
    // systemd --user unit and (on purge) the home dir. Runtime comes from the
    // install record, so this works without the registry. Mirrors the native
    // add path's early return.
    if let Ok(Some(meta)) = metadata::load_metadata(service_name)
        && meta.runtime == registry::service_def::Runtime::Native
    {
        let url = installed.exposure.url().map(|s| s.to_string());
        return remove_native_service(service_name, mode, url);
    }

    // Stop all units belonging to this service (main + sidecars).
    // Quadlet files named {service_name}.ext or {service_name}-sidecar.ext.
    let quadlet_path = quadlet_dir()?;
    let mut steps = Vec::new();
    let mut volume_names = Vec::new();
    let mut networks: Vec<String> = Vec::new();
    let mut has_named_volumes = false;
    // Quadlet directory scan is authoritative — captures every
    // ryra-managed service so the "is foo-bar a sibling service?"
    // prefix check (used to scope file removal) sees every install.
    let name_pool = scan_managed_services().unwrap_or_default();
    let all_names: Vec<&str> = name_pool.iter().map(|s| s.as_str()).collect();

    // Disable the Tailscale Service before tearing the host port down.
    // Always emit when the service was tailscale-enabled — the API
    // delete is idempotent and `tailscale serve --service=svc:X off`
    // is fine to run on a service that's already cleared.
    //
    // svc_name comes from the stored exposure URL (the `<service>-<host>`
    // first label) — pulling it from the URL captured at install time
    // means a hostname change post-install doesn't break teardown. If
    // the URL is malformed, skip the step rather than blocking the
    // whole removal — a stale tailnet entry is a smaller harm than a
    // service that won't uninstall.
    if let Some(svc_name) = installed.exposure.tailscale_svc_name() {
        steps.push(Step::TailscaleDisable { svc_name });
    }

    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Catches both the service's own quadlets (foo.container,
            // foo-db.container, …) and its `ts-foo*` tailscale sidecar.
            if !quadlet_belongs_to(&name, service_name, &all_names) {
                continue;
            }
            // Stop each .container unit before removing files
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".network") {
                // Stop the generated `<net>-network` oneshot, and remember the
                // network so it can be dropped once every container is down.
                let net = name.trim_end_matches(".network").to_string();
                steps.push(Step::StopService {
                    unit: format!("{net}-network"),
                });
                networks.push(net);
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
    // Caddy-routed exposures (Internal / Public) had a `# Service-Source: registry/<svc>`
    // block written into the Caddyfile on add; remove it now. Loopback
    // and Tailscale never had one (no Caddy involvement), so skip.
    let had_caddy_route = matches!(
        installed.exposure,
        Exposure::Internal { .. } | Exposure::Public { .. }
    );
    if !WellKnownService::Caddy.matches(service_name) && had_caddy_route {
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

    // Reload systemd after removing quadlet files
    steps.push(Step::DaemonReload);

    // Drop the service's podman networks now that all its containers are
    // stopped and the network units unloaded. `ryra remove` previously left
    // these behind — it deleted the `.network` file but never the network
    // itself — and the leak broke the next install: the regenerated network
    // unit's `podman network create` hit the still-present network and failed.
    // Best-effort — a network still used by another service is correctly
    // skipped (the rm fails and is ignored by the executor).
    for net in networks {
        steps.push(Step::RemoveNetwork { name: net });
    }

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

    let url = installed.exposure.url().map(|s| s.to_string());

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
        url,
    })
}

/// Tear down a `runtime = "native"` install: stop its `systemd --user` unit,
/// drop the unit symlink, reload, and remove either the whole home (purge) or
/// just the rebuildable/ephemeral bits (preserve keeps `data/` + the install
/// record). The dual of [`build_native_add`].
fn remove_native_service(
    service_name: &str,
    mode: RemoveMode,
    url: Option<String>,
) -> Result<RemoveResult> {
    let home = service_home(service_name)?;
    let unit_name = format!("{service_name}.service");
    let mut steps = vec![
        Step::StopService {
            unit: service_name.to_string(),
        },
        Step::RemoveFile(systemd_user_dir()?.join(&unit_name)),
        Step::DaemonReload,
    ];

    match mode {
        RemoveMode::Purge => steps.push(Step::RemoveDir(home)),
        RemoveMode::Preserve => {
            // Keep data/ and metadata.toml; drop the rebuildable binary, the
            // generated .env, and the unit file (all re-created on re-add).
            for child in ["bin", ".env", unit_name.as_str()] {
                let p = home.join(child);
                match std::fs::metadata(&p) {
                    Ok(m) if m.is_dir() => steps.push(Step::RemoveDir(p)),
                    _ => steps.push(Step::RemoveFile(p)),
                }
            }
        }
    }

    Ok(RemoveResult {
        steps,
        service_name: service_name.to_string(),
        url,
    })
}

/// A lifecycle transition applied to an installed service's unit family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Start,
    Stop,
}

/// Plan a start/stop of an installed service's full unit family (main
/// container + sidecars). Errors with [`Error::ServiceNotInstalled`] if
/// the service isn't installed.
///
/// systemd cascades *start* through `Requires=`, but never cascades
/// *stop* — so every `.container` unit is named explicitly and the steps
/// are ordered to respect dependencies: the main app unit stops first
/// (before its db/cache sidecars) and starts last (after them).
pub fn lifecycle_steps(service_name: &str, action: Lifecycle) -> Result<Vec<Step>> {
    // Same validation + error surface as `remove_service`.
    build_installed_from_metadata(service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    // Native services are a single systemd --user unit named after the service
    // (no sidecars / quadlets).
    if matches!(
        metadata::load_metadata(service_name),
        Ok(Some(m)) if m.runtime == registry::service_def::Runtime::Native
    ) {
        let unit = service_name.to_string();
        return Ok(vec![match action {
            Lifecycle::Start => Step::StartService { unit },
            Lifecycle::Stop => Step::StopService { unit },
        }]);
    }

    let mut units = service_container_units(service_name)?;
    match action {
        // Main unit first → stops before the sidecars it depends on.
        Lifecycle::Stop => units.sort_by_key(|u| u != service_name),
        // Main unit last → starts after the sidecars it depends on.
        Lifecycle::Start => units.sort_by_key(|u| u == service_name),
    }

    Ok(units
        .into_iter()
        .map(|unit| match action {
            Lifecycle::Start => Step::StartService { unit },
            Lifecycle::Stop => Step::StopService { unit },
        })
        .collect())
}

/// systemd unit base names of every `.container` quadlet belonging to a
/// service (main container, sidecars, and the `ts-<svc>` tailscale
/// sidecar). Mirrors the family scan in [`remove_service`].
fn service_container_units(service_name: &str) -> Result<Vec<String>> {
    let quadlet_path = quadlet_dir()?;
    let name_pool = scan_managed_services().unwrap_or_default();
    let all_names: Vec<&str> = name_pool.iter().map(|s| s.as_str()).collect();

    let mut units = Vec::new();
    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !quadlet_belongs_to(&name, service_name, &all_names) {
                continue;
            }
            if name.ends_with(".container") {
                units.push(name.trim_end_matches(".container").to_string());
            }
        }
    }
    Ok(units)
}

/// Parameters for [`record_pending`].
pub struct RecordPendingParams<'a> {
    pub service_name: &'a str,
    pub auth_kind: Option<registry::service_def::AuthKind>,
    pub registry_name: &'a str,
    pub allocated_ports: &'a [(String, u16)],
    pub repo_dir: &'a Path,
    /// How the service is exposed to clients. Replaces the previous
    /// `(url: Option<&str>, tailscale_enabled: bool)` pair so callers
    /// can't construct invalid combinations like a `*.ts.net` URL with
    /// `tailscale_enabled = false`. Decomposed into the legacy storage
    /// fields inside `record_pending` until the schema migrates to
    /// hold the typed enum directly.
    pub exposure: &'a Exposure,
}

/// Record a service as pending installation (installed: false).
/// Called BEFORE executing steps so that partial failures are recoverable.
/// Persist install-time scaffolding to `preferences.toml`. This is now
/// the only side-effect — quadlet headers track the install itself,
/// preferences just remembers cross-cutting defaults so the next
/// `ryra add --auth` doesn't have to re-prompt for the OIDC issuer.
pub fn record_pending(params: RecordPendingParams<'_>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    // Auto-configure [auth] when an auth provider is installed so
    // future `ryra add <svc> --auth` calls know where to wire the
    // OIDC client. The `services` array is not touched — quadlet
    // headers are the source of truth for what's installed.
    if WellKnownService::Authelia.matches(params.service_name) {
        config.auth = Some(authelia::auth_config(
            params.allocated_ports,
            params.exposure.url(),
        )?);
        config::save_config(&paths.config_file, &config)?;
    }

    Ok(())
}

/// Drop the cached `[auth]` block when the auth provider is removed —
/// otherwise a later `ryra add <svc> --auth` thinks auth is still
/// configured and skips the auto-install path, then bombs out trying
/// to register an OIDC client against a non-existent authelia config.
/// The function name is preserved for caller compatibility; quadlet
/// removal is what actually finalises the install state.
pub fn finalize_remove(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_or_default(&paths.config_file)?;

    if WellKnownService::Authelia.matches(service_name)
        && let Some(auth) = &config.auth
        && auth.provider_name() == "authelia"
    {
        config.auth = None;
        config::save_config(&paths.config_file, &config)?;
    }

    Ok(())
}

/// Steps to purge leftover data/volumes for an orphan service — one
/// with data on disk but no live install (e.g., after `ryra remove
/// <svc>` in default Preserve mode, or after a partial install where
/// the quadlets landed but `metadata.toml` never did). Unlike
/// `remove_service`, this doesn't require an install record to exist.
/// Templates whose rendered value is "sensitive" — either because the
/// value itself is a secret/credential, or because it rotates with each
/// install (so tracking it as static produces false drift positives).
/// Anything referencing one of these is excluded from the manifest.
///
/// Crucially this is *narrower* than "every {{auth.*}} reference":
/// `{{auth.url}}`, `{{auth.issuer}}`, `{{auth.provider}}`, `{{auth.internal_url}}`
/// are all stable per-install URLs/strings that the user benefits from
/// having tracked (so a global authelia URL change is caught by diff).
/// Only the credential pair `auth.client_id` + `auth.client_secret`
/// rotate per install. Same for SMTP: `smtp.host`/`smtp.port`/`smtp.from`/
/// `smtp.security` are tracked, only `smtp.username` and `smtp.password`
/// are excluded.
const SENSITIVE_TEMPLATE_REFS: &[&str] = &[
    "{{secret.",
    "{{auth.client_id",
    "{{auth.client_secret",
    "{{smtp.username",
    "{{smtp.password",
];

fn is_static_template(value: &str) -> bool {
    !SENSITIVE_TEMPLATE_REFS.iter().any(|s| value.contains(s))
}

/// Render every static env var the registry expects in `.env` for the
/// service. "Static" means the template carries no reference to any
/// rotating per-install value (see `SENSITIVE_TEMPLATE_REFS`).
///
/// Walks four sources, in the same order they're rendered into `.env` by
/// `generate::generate_env`:
///   1. `service_def.env` — top-level static entries.
///   2. Each enabled `[[env_group]]` — opt-in bundles.
///   3. `service_def.mappings.smtp` — only when SMTP is configured globally
///      and the service opts in (`integrations.smtp`).
///   4. `service_def.mappings.auth` — only when `--auth` was used.
///
/// Capturing 3 and 4 is what makes global-config drift visible: when the
/// user reconfigures global SMTP / re-installs authelia, the per-service
/// mapping values change, and tracking them lets `ryra diff` notice.
fn collect_static_envs(
    service_def: &registry::service_def::ServiceDef,
    ctx: &BTreeMap<String, String>,
    enabled_groups: &std::collections::BTreeSet<String>,
) -> Result<Vec<plan::TrackedEnv>> {
    use registry::service_def::EnvKind;
    let mut out: Vec<plan::TrackedEnv> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push = |name: &str,
                value_template: &str,
                kind: EnvKind,
                prompt: Option<String>,
                out: &mut Vec<plan::TrackedEnv>,
                seen: &mut std::collections::HashSet<String>|
     -> Result<()> {
        if !is_static_template(value_template) {
            return Ok(());
        }
        if !seen.insert(name.to_string()) {
            return Ok(());
        }
        let value = generate::template::render(value_template, ctx)?;
        out.push(plan::TrackedEnv {
            key: name.to_string(),
            value,
            kind,
            prompt,
        });
        Ok(())
    };
    for env in &service_def.env {
        push(
            &env.name,
            &env.value,
            env.kind.clone(),
            env.prompt.clone(),
            &mut out,
            &mut seen,
        )?;
    }
    for group in &service_def.env_groups {
        if !enabled_groups.contains(&group.name) {
            continue;
        }
        for env in &group.env {
            push(
                &env.name,
                &env.value,
                env.kind.clone(),
                env.prompt.clone(),
                &mut out,
                &mut seen,
            )?;
        }
    }
    // Mirror the gating from `generate::render_env_vars`: SMTP mappings
    // only fire when smtp is configured globally; auth mappings only when
    // --auth was used. ctx-key presence is a faithful proxy for both.
    // Mapping-emitted env vars are always treated as Default (silent
    // append on upgrade) — there's no user-facing prompt label for them.
    if service_def.integrations.smtp && ctx.contains_key("smtp.host") {
        for (env_name, value_template) in &service_def.mappings.smtp {
            push(
                env_name,
                value_template,
                EnvKind::Default,
                None,
                &mut out,
                &mut seen,
            )?;
        }
    }
    if ctx.contains_key("auth.client_id") {
        for (env_name, value_template) in &service_def.mappings.auth {
            push(
                env_name,
                value_template,
                EnvKind::Default,
                None,
                &mut out,
                &mut seen,
            )?;
        }
    }
    Ok(out)
}

pub fn orphan_purge_steps(svc: &data::ServiceData) -> Vec<Step> {
    let mut steps = Vec::new();

    // Quadlet files in `~/.config/containers/systemd/` belonging to
    // this service. Mirrors `remove_service`'s sweep — filename match
    // via `quadlet_belongs_to` catches both regular files and
    // symlinks, so a re-`ryra add` after purge starts clean instead
    // of seeing a leftover `.volume` and re-prompting about orphan data.
    let mut had_quadlet = false;
    let mut networks: Vec<String> = Vec::new();
    if let Ok(qdir) = quadlet_dir()
        && qdir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&qdir)
    {
        let name_pool = scan_managed_services().unwrap_or_default();
        let all_names: Vec<&str> = name_pool.iter().map(|s| s.as_str()).collect();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if !quadlet_belongs_to(&name, &svc.service, &all_names) {
                continue;
            }
            // Stop generated units before removing files so the
            // upcoming daemon-reload unloads them cleanly instead of
            // leaving "loaded: not-found, active (exited)" entries.
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            } else if name.ends_with(".network") {
                let net = name.trim_end_matches(".network").to_string();
                steps.push(Step::StopService {
                    unit: format!("{net}-network"),
                });
                networks.push(net);
            } else if name.ends_with(".volume") {
                let unit = format!("{}-volume", name.trim_end_matches(".volume"));
                steps.push(Step::StopService { unit });
            }
            steps.push(Step::RemoveFile(entry.path()));
            had_quadlet = true;
        }
    }
    if had_quadlet {
        steps.push(Step::DaemonReload);
    }
    // Drop the podman networks once the containers are down (see remove_service).
    for net in networks {
        steps.push(Step::RemoveNetwork { name: net });
    }

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
    let mut steps = Vec::new();

    // Quadlet directory scan is the source of truth for ryra-managed
    // services — every install stamps a marker comment on the main
    // `.container`, so this catches every install regardless of the
    // state of `preferences.toml`.
    let managed_names = scan_managed_services().unwrap_or_default();

    // 0. Disable every Tailscale Service before tearing services down.
    // `TailscaleDisable` stops `tailscale serve --service=svc:<X>` and
    // deletes the admin-side service definition via the API, so the
    // tailnet is clean after reset and the next install gets bare
    // hostnames. Read exposure from the quadlet headers so this still
    // works after the services array goes away.
    for svc in list_installed().unwrap_or_default() {
        if let Some(svc_name) = svc.exposure.tailscale_svc_name() {
            steps.push(Step::TailscaleDisable { svc_name });
        }
    }

    // 1. Stop and remove only ryra-managed quadlet files (scoped by installed service names)
    let quadlet_path = quadlet_dir()?;
    let all_names: Vec<&str> = managed_names.iter().map(|s| s.as_str()).collect();
    let mut networks: Vec<String> = Vec::new();
    if quadlet_path.is_dir()
        && let Ok(entries) = std::fs::read_dir(&quadlet_path)
    {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Only touch files belonging to a ryra-managed service —
            // including the `ts-<service>` tailscale sidecars when the
            // service was installed with --tailscale.
            let is_ryra_file = managed_names
                .iter()
                .any(|svc| quadlet_belongs_to(&name, svc, &all_names));
            if !is_ryra_file {
                continue;
            }
            if name.ends_with(".container") {
                let unit = name.trim_end_matches(".container").to_string();
                steps.push(Step::StopService { unit });
            }
            if name.ends_with(".network") {
                let net = name.trim_end_matches(".network").to_string();
                steps.push(Step::StopService {
                    unit: format!("{net}-network"),
                });
                networks.push(net);
            }
            if name.ends_with(".volume") {
                let vol = name.trim_end_matches(".volume").to_string();
                // Quadlet auto-generates `<vol>-volume.service` for each
                // `.volume` file. Stopping it before we remove the file
                // makes systemd unload the unit on the upcoming
                // daemon-reload — without this, leftover oneshot units
                // sit in "loaded: not-found, active (exited)" forever
                // until logout.
                steps.push(Step::StopService {
                    unit: format!("{vol}-volume"),
                });
            }
            steps.push(Step::RemoveFile(entry.path()));
        }
    }

    // 1b. Native services keep their unit in the systemd --user dir, not the
    // quadlet dir, so the scan above misses them. Sweep ryra's native installs:
    // each home dir whose install record says `runtime = native` gets its unit
    // stopped and its `systemd/user` symlink removed (before the reload below
    // unloads it, and before step 4 wipes the data root).
    let user_unit_dir = systemd_user_dir()?;
    if let Ok(root) = service_data_root()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if matches!(
                metadata::load_metadata(&name),
                Ok(Some(m)) if m.runtime == registry::service_def::Runtime::Native
            ) {
                steps.push(Step::StopService { unit: name.clone() });
                steps.push(Step::RemoveFile(
                    user_unit_dir.join(format!("{name}.service")),
                ));
            }
        }
    }

    // 2. Reload user systemd after removing quadlets
    steps.push(Step::DaemonReload);

    // 2b. Drop the podman networks now that every container is stopped and the
    // network units unloaded (see remove_service for why the leak matters).
    for net in networks {
        steps.push(Step::RemoveNetwork { name: net });
    }

    // 3. Remove podman volumes for every ryra-visible service — installed
    // and orphaned. `enumerate_all` walks both the quadlet markers and the
    // data root, so volumes left behind by a `ryra remove --preserve`
    // (which drops the quadlet but keeps the named volume) get swept up
    // here too.
    let mut seen_volumes = std::collections::BTreeSet::new();
    for svc in data::enumerate_all().unwrap_or_default() {
        for vol in svc.volumes {
            if seen_volumes.insert(vol.name.clone()) {
                steps.push(Step::RemoveVolume { name: vol.name });
            }
        }
    }

    // 4. Nuke the entire service data root in one shot. The user-facing
    // reset prompt promises "Delete ~/.local/share/services/", so the
    // implementation must match — sweeping managed dirs, orphan dirs
    // (left by `--preserve` removes), the top-level caddy-root-ca.crt,
    // and any other ryra-written tooling state living under that root.
    let data_root = service_data_root()?;
    if data_root.exists() {
        steps.push(Step::RemoveDir(data_root));
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

/// Get the current status of the ryra installation. Considers ryra
/// "initialized" when EITHER a marker'd quadlet is on disk OR a
/// `preferences.toml` exists — quadlets are the source of truth for
/// installed services, but a preferences-only state (e.g. an SMTP relay
/// configured before any service install) still counts.
pub fn status() -> config::status::RyraStatus {
    let paths = match ConfigPaths::resolve() {
        Ok(p) => p,
        Err(_) => return config::status::RyraStatus::NotInitialized,
    };

    let has_quadlets = scan_managed_services()
        .map(|n| !n.is_empty())
        .unwrap_or(false);

    let config = match config::load_config(&paths.config_file) {
        Ok(c) => c,
        Err(Error::ConfigNotFound(_)) if has_quadlets => config::schema::Config::default(),
        Err(Error::ConfigNotFound(_)) => return config::status::RyraStatus::NotInitialized,
        Err(e) => return config::status::RyraStatus::Error(e.to_string()),
    };

    config::status::RyraStatus::Initialized(config::status::StatusInfo::from_config(
        paths.config_file,
        &config,
    ))
}

/// True if the named service is ryra-managed and *fully* installed —
/// the marker'd `.container` is present AND `metadata.toml` exists.
/// A partial install (quadlets written but the install plan errored
/// before metadata.toml landed) is treated as not-installed so that
/// `ryra remove <svc> --purge` routes through the orphan-cleanup path
/// instead of failing with "service is not installed". Same source of
/// truth as [`list_installed`].
pub fn is_service_installed(name: &str) -> bool {
    // The install record says whether (and how) a service is installed. Native
    // services have no quadlet — their presence is the systemd --user unit;
    // podman services are the marker'd quadlet. No metadata → not installed.
    let Ok(Some(meta)) = metadata::load_metadata(name) else {
        return false;
    };
    match meta.runtime {
        registry::service_def::Runtime::Native => systemd_user_dir()
            .map(|d| d.join(format!("{name}.service")).exists())
            .unwrap_or(false),
        registry::service_def::Runtime::Podman => scan_managed_services()
            .map(|names| names.iter().any(|n| n == name))
            .unwrap_or(false),
    }
}

/// Scan the user's quadlet directory for ryra-managed services. A
/// `.container` file is considered ryra-managed iff it carries a
/// `# Service-Source: registry/<name>` comment within its first 16
/// lines (added at install time). Returns the deduplicated set of
/// service names found.
///
/// This makes the on-disk quadlet directory the source of truth for
/// "which services are installed" — `preferences.toml` was historically
/// authoritative, but could drift (e.g. if config was wiped while
/// services kept running). Callers that want richer metadata (URL,
/// exposure) still need `preferences.toml`; for "is X installed" the
/// quadlet scan is reliable.
pub fn scan_managed_services() -> Result<Vec<String>> {
    let dir = match quadlet_dir() {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::FileRead { path: dir, source }),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("container") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().take(16) {
            if let Some(rest) = line.trim().strip_prefix("# Service-Source: registry/")
                && !rest.is_empty()
                && !names.iter().any(|n| n == rest)
            {
                names.push(rest.to_string());
                break;
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Build a full [`InstalledService`] from `metadata.toml` + `.env`.
/// Returns `None` if metadata.toml is missing — that's the signal that
/// either the service was never installed or it was installed by a
/// pre-metadata.toml ryra (in which case the caller should treat it as
/// not-installed and let the user reinstall).
fn build_installed_from_metadata(service_name: &str) -> Option<InstalledService> {
    let meta = load_metadata(service_name).ok().flatten()?;

    // Loopback when no URL; otherwise classify by hostname suffix.
    let exposure = match meta.url.as_deref() {
        None => Exposure::Loopback,
        Some(u) => Exposure::from_url(u),
    };

    let auth_kind = meta.auth.clone();

    // Ports come from the `.env` file ryra writes alongside the quadlet
    // — `SERVICE_PORT_<NAME>=<value>` lines map back to the BTreeMap
    // keyed by lowercase name. Missing `.env` is treated as empty (still
    // a valid install — services without published ports legitimately
    // omit it).
    let ports = service_home(service_name)
        .ok()
        .and_then(|home| std::fs::read_to_string(home.join(".env")).ok())
        .map(|env| {
            env.lines()
                .filter_map(|l| {
                    let l = l.trim();
                    if l.is_empty() || l.starts_with('#') {
                        return None;
                    }
                    let (key, val) = l.split_once('=')?;
                    let name = key.strip_prefix("SERVICE_PORT_")?.to_lowercase();
                    let port = val
                        .trim_matches(|c: char| c == '"' || c == '\'')
                        .parse::<u16>()
                        .ok()?;
                    Some((name, port))
                })
                .collect::<std::collections::BTreeMap<String, u16>>()
        })
        .unwrap_or_default();

    Some(InstalledService {
        name: service_name.to_string(),
        version: "0.1.0".to_string(),
        repo: meta.registry,
        ports,
        auth_kind,
        exposure,
        provides: meta.provides,
        installed: true,
    })
}

/// List installed services. **Quadlet directory is the source of
/// truth** — every service whose main `.container` file carries our
/// marker is reconstructed from its on-disk headers + `.env`. The
/// preferences file is only consulted as a fallback for entries the
/// scan can't see (e.g. partially-rolled-out installs from older
/// ryra versions before metadata headers landed).
pub fn list_installed() -> Result<Vec<InstalledService>> {
    let mut names: std::collections::BTreeSet<String> = scan_managed_services()
        .unwrap_or_default()
        .into_iter()
        .collect();
    // Native services carry no quadlet marker. Pick them up from the data root:
    // any home dir that `is_service_installed` confirms (runtime-aware) and that
    // the quadlet scan didn't already catch.
    if let Ok(root) = service_data_root()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && !names.contains(name)
                && is_service_installed(name)
            {
                names.insert(name.to_string());
            }
        }
    }
    let out: Vec<InstalledService> = names
        .iter()
        .filter_map(|n| build_installed_from_metadata(n))
        .collect();
    Ok(out)
}

/// Search available services in a repo, optionally filtered by query.
pub fn search_services(repo_dir: &Path, query: Option<&str>) -> Result<Vec<SearchResult>> {
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
            let installed = is_service_installed(name);
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
    let installed = build_installed_from_metadata(service_name)
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let service_ref = service_ref_from_installed(&installed);
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
    fn static_template_filter_excludes_secrets_and_credentials() {
        // Plain literal — tracked.
        assert!(is_static_template("3306"));
        assert!(is_static_template("mariadb"));
        // Stable template references — tracked.
        assert!(is_static_template("{{service.port}}"));
        assert!(is_static_template("{{service.url}}"));
        assert!(is_static_template("{{auth.url}}"));
        assert!(is_static_template("{{auth.issuer}}"));
        assert!(is_static_template("{{auth.provider}}"));
        assert!(is_static_template("{{auth.internal_url}}"));
        assert!(is_static_template("{{smtp.host}}"));
        assert!(is_static_template("{{smtp.port}}"));
        assert!(is_static_template("{{smtp.from}}"));
        // Composite template: stable + stable — tracked.
        assert!(is_static_template("{{service.url}}/oauth/callback"));

        // Secrets — never tracked.
        assert!(!is_static_template("{{secret.admin_password}}"));
        assert!(!is_static_template("{{secret.jwt_key}}"));
        // Per-install OIDC credentials — never tracked (rotates on auth provider reinstall).
        assert!(!is_static_template("{{auth.client_id}}"));
        assert!(!is_static_template("{{auth.client_secret}}"));
        // SMTP credentials — never tracked.
        assert!(!is_static_template("{{smtp.username}}"));
        assert!(!is_static_template("{{smtp.password}}"));
        // Composite templates carrying a sensitive ref must also be excluded.
        assert!(!is_static_template(
            "redis://:{{secret.redis_pw}}@host:6379"
        ));
    }

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

    #[test]
    fn public_url_accepts_public_domains() {
        assert!(is_public_url("https://seafile.ryra.no"));
        assert!(is_public_url("https://example.com"));
        assert!(is_public_url("https://docs.ryra.no:8443"));
    }

    #[test]
    fn public_url_rejects_lan_and_tailnet() {
        assert!(!is_public_url("https://nextcloud.internal:8443"));
        assert!(!is_public_url("https://service.localhost"));
        assert!(!is_public_url("https://something.local"));
        assert!(!is_public_url("https://localhost:8080"));
        assert!(!is_public_url("https://debian.cobbler-tuna.ts.net"));
        assert!(!is_public_url("http://127.0.0.1:10001"));
        assert!(!is_public_url("http://192.168.1.10"));
        assert!(!is_public_url("http://[::1]"));
        assert!(!is_public_url("not a url"));
    }

    // resolve_extra_networks positional args:
    // (name, enable_auth, authelia_installed, caddy_installed,
    //  inbucket_installed, has_url, has_smtp)

    #[test]
    fn networks_empty_when_no_auth() {
        let nets = resolve_extra_networks("whoami", false, false, false, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_empty_when_auth_but_no_authelia() {
        let nets = resolve_extra_networks("forgejo", true, false, false, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_authelia_when_auth_enabled() {
        let nets = resolve_extra_networks("forgejo", true, true, false, false, false, false);
        assert_eq!(nets, vec!["authelia"]);
    }

    #[test]
    fn networks_auth_with_caddy_includes_both() {
        let nets = resolve_extra_networks("forgejo", true, true, true, false, false, false);
        assert!(nets.contains(&"authelia".to_string()));
        assert!(nets.contains(&"caddy".to_string()));
    }

    #[test]
    fn networks_authelia_excluded_for_authelia_itself() {
        let nets = resolve_extra_networks("authelia", true, true, false, false, false, false);
        assert!(nets.is_empty());
    }

    #[test]
    fn networks_smtp_joins_inbucket_without_caddy() {
        // Reaching inbucket for SMTP must NOT require caddy.
        let nets = resolve_extra_networks("forgejo", false, false, false, true, false, true);
        assert_eq!(nets, vec!["inbucket"]);
    }

    #[test]
    fn networks_smtp_skips_inbucket_when_it_is_self() {
        let nets = resolve_extra_networks("inbucket", false, false, false, true, false, true);
        assert!(!nets.contains(&"inbucket".to_string()));
    }

    #[test]
    fn networks_smtp_skips_inbucket_when_not_installed() {
        let nets = resolve_extra_networks("forgejo", false, false, false, false, false, true);
        assert!(!nets.contains(&"inbucket".to_string()));
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
