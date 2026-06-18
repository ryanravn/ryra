use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::caddy::AcmeMode;
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::Config;
use ryra_core::registry::resolve::ServiceRef;
use ryra_core::registry::service_def::{AuthKind, ServiceKind};
use ryra_core::{
    Capability, REGISTRY_DEFAULT, Warning, WellKnownService, find_installed_provider,
    service_provides,
};

use super::apply;
use super::prompts;

/// Default port for Caddy's HTTPS listener.
const DEFAULT_CADDY_HTTPS_PORT: u16 = 8443;

/// Non-interactive choice for `--smtp=…` on `ryra add`.
///
/// Modelled as an enum so the compiler rejects typos at the CLI boundary
/// and so we can grow additional providers (e.g. an explicit
/// `--smtp=custom --smtp-host=… --smtp-port=…` combo) without breaking
/// the existing call sites.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SmtpProvider {
    /// Auto-install inbucket and wire `config.smtp` to it. For testing
    /// and local development — inbucket is a disposable SMTP sink with a
    /// web UI and an HTTP API for inspecting received mail.
    Inbucket,
}

/// How the user asked the service to be reachable, before resolution.
/// One value instead of the `(Option<url>, bool tailscale)` pair, so
/// "--url and --tailscale at once" is unrepresentable for every caller
/// (clap's `conflicts_with` only protects the top-level parse).
#[derive(Debug, Clone, Default)]
pub enum ExposureRequest {
    /// No explicit flag: resolve via policy and prompts (Caddy
    /// auto-derive, the exposure prompt, or loopback).
    #[default]
    Auto,
    /// Explicit `--url <X>`.
    Url(String),
    /// Explicit `--tailscale`.
    Tailscale,
}

/// One `ryra add` invocation, typed. Built from clap flags in main, and
/// by the dependency auto-installs (caddy / authelia / inbucket) here.
#[derive(Debug, Clone, Default)]
pub struct AddRequest {
    pub services: Vec<String>,
    pub exposure: ExposureRequest,
    pub auth: bool,
    pub smtp: Option<SmtpProvider>,
    pub enable: Vec<String>,
    /// `[[choice]]` selections as raw `CHOICE=OPTION` strings.
    pub choose: Vec<String>,
    pub backup: bool,
    pub acme: Option<AcmeMode>,
    pub dry_run: bool,
    pub yes: bool,
}

impl AddRequest {
    /// A non-interactive install of a well-known dependency (caddy,
    /// authelia, inbucket) triggered from inside another add: `--yes`,
    /// auto exposure, nothing else enabled. Callers layer extras on with
    /// struct-update syntax (`AddRequest { acme: …, ..Self::dependency(…) }`).
    fn dependency(service: WellKnownService) -> Self {
        Self {
            services: vec![service.to_string()],
            yes: true,
            ..Self::default()
        }
    }

    /// Propagate a parent install's `--tailscale` choice to a dependency,
    /// so e.g. authelia inherits the exposure of the service that pulled
    /// it in.
    fn tailscale(mut self, enabled: bool) -> Self {
        if enabled {
            self.exposure = ExposureRequest::Tailscale;
        }
        self
    }
}

pub async fn run(request: AddRequest) -> Result<()> {
    let AddRequest {
        services,
        exposure: exposure_request,
        auth,
        smtp,
        enable,
        choose,
        backup,
        acme,
        dry_run,
        yes,
    } = request;
    // Convenience views of the exposure request for the guards and the
    // per-service resolution below. The typed request already rules out
    // the url+tailscale combination.
    let url: Option<&str> = match &exposure_request {
        ExposureRequest::Url(u) => Some(u),
        ExposureRequest::Auto | ExposureRequest::Tailscale => None,
    };
    let tailscale = matches!(exposure_request, ExposureRequest::Tailscale);

    if url.is_some() && services.len() > 1 {
        bail!("--url can only be used when adding a single service");
    }
    if !enable.is_empty() && services.len() > 1 {
        bail!("--enable can only be used when adding a single service");
    }
    if !choose.is_empty() && services.len() > 1 {
        bail!("--choose can only be used when adding a single service");
    }
    // Parse `--choose CHOICE=OPTION` once, up front, so a malformed flag
    // fails before any install work. Names are validated against the
    // service's registered choices per-service below.
    let mut choose_pairs: Vec<(String, String)> = Vec::new();
    for raw in &choose {
        match raw.split_once('=') {
            Some((c, o)) if !c.is_empty() && !o.is_empty() => {
                choose_pairs.push((c.to_string(), o.to_string()))
            }
            _ => bail!("--choose expects CHOICE=OPTION, got '{raw}'"),
        }
    }
    // --acme drives caddy's TLS mode. It's accepted on any `ryra add`:
    // when adding caddy directly it sets the snippet; when adding a
    // service with `--url <public>` it auto-installs caddy in ACME mode
    // before the service is added (non-interactive equivalent of the
    // TLS prompt below).

    // Caddy's host-port binding is the `binding` choice: `direct` (80/443)
    // needs the unprivileged low-port sysctl; `proxied` (default, 8080/8443)
    // doesn't. `--acme` implies `direct` — Let's Encrypt's HTTP-01 challenge
    // and a public hostname both want 80/443, so high ports would break it.
    // When this run installs a reverse proxy in direct mode, pin the choice
    // and offer to lower the sysctl. This has to happen before add_service,
    // where the caddy quadlet's `PublishPort=` is generated from the choice +
    // sysctl. Skipped for the default proxied path (LAN/behind-a-proxy), where
    // 8080/8443 is correct.
    let adding_reverse_proxy = services
        .iter()
        .any(|s| service_provides(s, Capability::ReverseProxy));
    let wants_direct = acme.is_some()
        || choose_pairs
            .iter()
            .any(|(c, o)| c == "binding" && o == "direct");
    if adding_reverse_proxy && wants_direct {
        // --acme implies direct: pin it unless the user set `binding` explicitly.
        if !choose_pairs.iter().any(|(c, _)| c == "binding") {
            choose_pairs.push(("binding".to_string(), "direct".to_string()));
        }
        super::sysctl_low_ports::offer_enable().await?;
    }

    let preflight_paths = ryra_core::config::ConfigPaths::resolve()?;
    let preflight_config = ryra_core::config::load_or_default(&preflight_paths.config_file)?;
    let issues = ryra_core::system::doctor::check_all(&preflight_config);
    let blockers: Vec<&ryra_core::system::doctor::Issue> = issues
        .iter()
        .filter(|i| i.severity() == ryra_core::system::doctor::Severity::Blocker)
        .collect();
    if !blockers.is_empty() {
        let rendered = blockers
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n\n");
        bail!("ryra doctor reports blockers:\n\n{rendered}");
    }
    // Surface non-blocking issues but don't gate the install — `ryra
    // doctor` is the place to see the full list with fixes.
    if issues.iter().any(|i| {
        matches!(
            i.severity(),
            ryra_core::system::doctor::Severity::Warning
                | ryra_core::system::doctor::Severity::Info
        )
    }) {
        eprintln!("Note: `ryra doctor` reports issues — run it to see them.");
    }

    // --tailscale check fires here so a missing CLI / unlogged tailnet
    // surfaces before any service planning. Stays separate from the
    // unified doctor view because tailscale issues are only relevant
    // when the user explicitly opts into the tailscale path.
    if tailscale && let Err(e) = ryra_core::system::doctor::check_tailscale_runtime() {
        bail!("--tailscale flag passed but {e}");
    }

    let interactive = super::is_interactive();

    // Tailscale admin API token acquisition: when `--tailscale` is set
    // and we don't already have one cached from a previous install,
    // prompt the user to paste one (or read TAILSCALE_API_KEY in
    // non-interactive runs). Saved to `config.tailscale.admin_api_key`.
    // Done up-front so a missing token fails fast, before any image
    // pulls or service planning. The token is needed at install time
    // (define service via API) and removal time (delete service).
    if tailscale && !dry_run {
        ensure_tailscale_admin_token(interactive).await?;
        super::tailscale_sudoers::offer_enable().await?;
    }

    // "First add" = no ryra config on disk yet. Latch the answer before any
    // side-effect creates the file — we use this at the end to decide between
    // offering to enable lingering (ceremonial, worth the interaction) and
    // just warning (quieter, for every subsequent add).
    let first_run = !ryra_core::config::ConfigPaths::resolve()?
        .config_file
        .exists();

    // Auto-install authelia for --auth — unless a requested service IS the
    // auth provider, in which case `ryra add authelia --auth` would
    // auto-install authelia here and then fail the actual add with
    // "already installed".
    let installing_provider = services
        .iter()
        .any(|s| service_provides(s, Capability::OidcProvider));
    if !dry_run {
        ensure_dependencies(auth && !installing_provider, tailscale, interactive).await?;
    }

    // --smtp=<provider>: non-interactive equivalent of the prompts below.
    // Runs once before the per-service loop so every SMTP-using service in
    // the same batch picks up the newly-written config.smtp.
    if let Some(provider) = smtp
        && !dry_run
    {
        ensure_smtp_for_add(provider).await?;
    }

    let paths = ryra_core::config::ConfigPaths::resolve()?;

    // Serialize concurrent auth-enabled `ryra add` runs so two processes
    // don't clobber each other's client entries when editing authelia's
    // configuration.yml in-memory then writing it back. Taken here for the
    // --auth flag; the per-service loop takes it lazily when auth is chosen
    // at the interactive prompt instead. Released when auth_lock drops at
    // end of this function.
    let mut auth_lock = if auth && !dry_run {
        Some(acquire_auth_lock(&paths)?)
    } else {
        None
    };

    // Local installs are explicit: `ryra add .` (not a bare `ryra add`), so the
    // command always names what it's installing — a registry ref or a path.
    if services.is_empty() {
        bail!(
            "name a service (e.g. `ryra add forgejo`), or run `ryra add .` in a project directory that has a service.toml"
        );
    }

    for service_input in &services {
        // A path-like arg (`.`, `./x`, `/abs`, or an existing dir) installs the
        // project's `service.toml` directly; otherwise it's a registry ref.
        let service_ref = if ryra_core::registry::resolve::is_path_like(service_input) {
            ryra_core::registry::resolve::path_ref(std::path::Path::new(service_input))?
        } else {
            ServiceRef::parse(service_input)?
        };
        let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
        let service = service_ref.service_name();

        // Bail before prompts — the same check fires deeper in
        // `ryra_core::add_service`, but only after SMTP / auth prompts have
        // already burned the user's time. Surface it up-front instead.
        if !dry_run && ryra_core::is_service_installed(service) {
            bail!("service {service} is already installed");
        }

        // Load config once — previous iterations or ensure_dependencies may have
        // modified it on disk (e.g., installing caddy or authelia).
        let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

        // Orphan-data check: `ryra remove <svc>` (preserve mode) drops the
        // service from config but leaves named volumes and data dirs on disk.
        // A fresh `ryra add` would silently inherit them — surprising when
        // the user wants a clean state. Surface it here so they choose.
        if !dry_run
            && !ryra_core::is_service_installed(service)
            && let Some(orphan) = ryra_core::data::enumerate_service(service)?
            && orphan.status == ryra_core::data::ServiceStatus::Orphan
            && (!orphan.volumes.is_empty() || !orphan.data_paths.is_empty())
        {
            println!("\n  '{service}' has data from a previous install (orphaned on disk):");
            for v in &orphan.volumes {
                println!("    volume: {}", v.name);
            }
            for p in &orphan.data_paths {
                println!("    data:   {}", p.display());
            }
            println!("\n  Proceeding will reuse this data (podman reuses named volumes by name).");
            if interactive && !yes {
                let proceed = Confirm::new()
                    .with_prompt(format!("Continue adding {service} with existing data?"))
                    .default(true)
                    .interact()?;
                if !proceed {
                    println!("\nCancelled. To purge and start clean:");
                    println!("  ryra remove {service} --purge");
                    println!("  ryra add {service}");
                    return Ok(());
                }
            } else {
                println!(
                    "  (use --yes to auto-accept, or run `ryra remove {service} --purge` first to start clean)"
                );
            }
        }

        // Look up the service definition
        let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;

        // Check architecture compatibility before any prompts
        if let Some(msg) = reg_service.def.check_architecture() {
            bail!("{msg}");
        }

        // Warn about untrusted registry services — they can run arbitrary
        // scripts via quadlet ExecStartPre/Post and mount host directories.
        if !matches!(service_ref, ServiceRef::Path { .. })
            && service_ref.registry_name() != REGISTRY_DEFAULT
            && !yes
            && !dry_run
        {
            warn_untrusted_service(&reg_service.service_dir, service, interactive)?;
        }

        // SMTP — decide whether to wire this specific service into email.
        //   * Global SMTP already configured → Confirm default yes (reuse it).
        //   * No global SMTP yet → show Skip/Custom/Inbucket select, default Skip.
        //   * Non-interactive → opt in iff SMTP is already configured globally.
        let service_supports_smtp =
            reg_service.def.integrations.smtp && !reg_service.def.mappings.smtp.is_empty();
        let enable_smtp: bool = if !service_supports_smtp || dry_run {
            false
        } else if interactive {
            if config.smtp.is_some() {
                Confirm::new()
                    .with_prompt("Use SMTP for this service?")
                    .default(true)
                    .interact()?
            } else {
                match prompts::prompt_smtp()? {
                    prompts::SmtpSetupChoice::Custom(smtp) => {
                        let had_secrets_before = config.has_secrets();
                        config.smtp = Some(smtp);
                        paths.ensure_dirs()?;
                        ryra_core::config::save_config(&paths.config_file, &config)?;
                        println!(
                            "  SMTP configured. Saved to {}\n",
                            paths.config_file.display()
                        );
                        warn_if_first_secret_save(&paths, had_secrets_before, &config);
                        true
                    }
                    prompts::SmtpSetupChoice::Inbucket => {
                        if !ryra_core::is_service_installed("inbucket") {
                            println!("\nInstalling inbucket...\n");
                            Box::pin(run(AddRequest::dependency(WellKnownService::Inbucket)))
                                .await?;
                            // Reload — inbucket install modified config on disk
                            config = ryra_core::config::load_or_default(&paths.config_file)?;
                        }
                        let had_secrets_before = config.has_secrets();
                        config.smtp = Some(ryra_core::config::schema::SmtpCredentials::inbucket());
                        paths.ensure_dirs()?;
                        ryra_core::config::save_config(&paths.config_file, &config)?;
                        println!(
                            "  SMTP configured (inbucket). Saved to {}\n",
                            paths.config_file.display()
                        );
                        warn_if_first_secret_save(&paths, had_secrets_before, &config);
                        true
                    }
                    prompts::SmtpSetupChoice::Skip => false,
                }
            }
        } else {
            config.smtp.is_some()
        };

        // Auth — determined by --auth flag or interactive prompt
        let auth_kind: Option<AuthKind> = resolve_auth_kind(
            auth,
            interactive,
            &reg_service.def.integrations.auth,
            config.auth.is_some(),
        )?;

        // The user's auth decision as one typed value. --auth on a service
        // with no native OIDC kinds fails fast here, before any prompts
        // burn the user's time. The auth provider itself is the exception:
        // it doesn't act as a client of itself, so --auth is a no-op note.
        let auth_choice = match (&auth_kind, auth) {
            (Some(kind), _) => ryra_core::AuthChoice::Native(kind.clone()),
            (None, true) => {
                if service_provides(service, Capability::OidcProvider) {
                    println!(
                        "  Note: {service} is the auth provider itself; --auth has no effect."
                    );
                    ryra_core::AuthChoice::None
                } else {
                    return Err(ryra_core::error::Error::NoOidcSupport(service.to_string()).into());
                }
            }
            (None, false) => ryra_core::AuthChoice::None,
        };

        // Auth chosen at the interactive prompt (no --auth flag) registers
        // an OIDC client too — take the cross-process lock if the flag path
        // didn't already.
        if auth_kind.is_some() && auth_lock.is_none() && !dry_run {
            auth_lock = Some(acquire_auth_lock(&paths)?);
        }

        // Resolve exposure BEFORE auto-installing authelia so the provider
        // inherits the parent's choice. Picking the two separately let users
        // mismatch (authelia local + service tailnet → tailnet clients can't
        // reach `authelia.internal` for OIDC redirects). One prompt, one
        // decision, propagated.
        let needs_https = reg_service
            .def
            .service
            .https
            .needs_https(auth_kind.is_some(), url);

        // True iff this run will auto-install authelia. Used both to show a
        // "(authelia will inherit this choice)" hint in the exposure prompt
        // and to drive the propagation below.
        let will_install_authelia = auth_kind.is_some() && config.auth.is_none();

        // Resolve a single typed `Exposure` once — replaces the prior
        // `(resolved_url, tailscale_enabled)` pair so downstream code
        // can pattern-match instead of juggling a parallel
        // `(Option<String>, bool)` that allowed silently invalid combos
        // like a `*.ts.net` URL with `tailscale_enabled = false`.
        //
        // Precedence:
        //   1. explicit --url (classified by hostname suffix)
        //   2. --tailscale → derive `<service>.<tailnet>.ts.net`
        //   3. Caddy installed + needs_https → auto-derive `*.internal`
        //   4. interactive prompt (Self-signed / Tailscale / Public+LE /
        //      External) when needs_https and Caddy isn't installed —
        //      Loopback isn't offered because the service rejects HTTP.
        //   5. interactive prompt for any user-facing app (kind=application)
        //      that doesn't require HTTPS — same options plus Loopback as
        //      the default. Surfaces tailscale/public exposure for services
        //      like vikunja that work fine over HTTP but the user might
        //      still want on a tailnet.
        //   6. Loopback — service runs on plain http://127.0.0.1:<port>
        //      (non-interactive default, and the choice for infrastructure
        //      services like inbucket that aren't user-facing).
        // Clap's `conflicts_with = "url"` on --tailscale means 1+2 don't collide.
        let is_user_facing_app = matches!(reg_service.def.service.kind, ServiceKind::Application);
        let exposure: ryra_core::Exposure = if let Some(u) = url {
            ryra_core::Exposure::from_url(u)
        } else if tailscale {
            let ts_url = ryra_core::system::tailscale::derive_service_url(service)?;
            println!("→ Using {ts_url} (Tailscale)");
            ryra_core::Exposure::Tailscale { url: ts_url }
        } else if needs_https {
            if ryra_core::is_service_installed("caddy") {
                let installed_all = ryra_core::list_installed().unwrap_or_default();
                let caddy_https_port =
                    find_installed_provider(&installed_all, Capability::ReverseProxy)
                        .and_then(|s| s.ports.get("https").copied())
                        .unwrap_or(DEFAULT_CADDY_HTTPS_PORT);
                let default_url = format!(
                    "https://{service}.{}:{caddy_https_port}",
                    ryra_core::config::schema::CADDY_LOCAL_DOMAIN
                );
                let chosen = if interactive && !dry_run {
                    Input::new()
                        .with_prompt(format!("URL for '{service}'"))
                        .default(default_url)
                        .interact_text()?
                } else {
                    default_url
                };
                ryra_core::Exposure::from_url(&chosen)
            } else if interactive && !dry_run {
                let chosen = prompt_exposure_for(service, will_install_authelia, false).await?;
                // Reload after potential Caddy install inside the prompt.
                config = ryra_core::config::load_or_default(&paths.config_file)?;
                chosen
            } else {
                bail!(
                    "service '{service}' requires HTTPS but no exposure was selected.\n\
                     Pass --tailscale, --url <X>, or `ryra add caddy` first to enable \
                     local HTTPS."
                );
            }
        } else if is_user_facing_app && interactive && !dry_run {
            let chosen = prompt_exposure_for(service, false, true).await?;
            // Reload — picking Self-signed / Public+LE installs Caddy, which
            // mutates config on disk.
            config = ryra_core::config::load_or_default(&paths.config_file)?;
            chosen
        } else {
            ryra_core::Exposure::Loopback
        };
        // Resolved-exposure view for the rest of this iteration: the
        // browser-visible URL, if the variant carries one.
        let url: Option<&str> = exposure.url();

        // Auto-install Caddy when the user gives a public URL but Caddy
        // isn't installed yet. Without this, the install would succeed
        // but the URL wouldn't actually route anywhere — the user would
        // have to know to add Caddy first, which the previous flow
        // forced and most people forget.
        let caddy_already_installed = ryra_core::is_service_installed("caddy");
        let need_caddy_for_public_url = url.is_some_and(ryra_core::is_public_url)
            && !caddy_already_installed
            && !service_provides(service, Capability::ReverseProxy)
            && !exposure.is_tailscale()
            && !dry_run;
        if need_caddy_for_public_url {
            let chosen = match acme.as_ref() {
                Some(mode) => Some(TlsHandling::LetsEncrypt(mode.clone())),
                None if interactive => Some(prompt_tls_for_public_url(url.unwrap_or("")).await?),
                None => None,
            };
            match chosen {
                Some(TlsHandling::LetsEncrypt(mode)) => {
                    if let Some(u) = url {
                        dns_preflight_for_acme(u, interactive).await?;
                    }
                    println!("\nInstalling caddy (Let's Encrypt mode)...\n");
                    Box::pin(run(AddRequest {
                        acme: Some(mode),
                        ..AddRequest::dependency(WellKnownService::Caddy)
                    }))
                    .await?;
                    config = ryra_core::config::load_or_default(&paths.config_file)?;
                }
                Some(TlsHandling::SelfSigned) => {
                    println!("\nInstalling caddy (self-signed LAN mode)...\n");
                    Box::pin(run(AddRequest::dependency(WellKnownService::Caddy))).await?;
                    config = ryra_core::config::load_or_default(&paths.config_file)?;
                }
                Some(TlsHandling::External) | None => {
                    // Skip Caddy install — user is fronting with their own
                    // reverse proxy (Cloudflare Tunnel, nginx, etc.). The
                    // existing `UrlWithoutReverseProxy` warning still fires
                    // from add_service so the user knows routing is on them.
                }
            }
        } else if acme.is_some()
            && caddy_already_installed
            && !service_provides(service, Capability::ReverseProxy)
        {
            // --acme passed but Caddy is already installed — the snippet
            // is set; flipping mode means editing tls.caddy directly.
            // Warn but don't bail; let the install proceed.
            eprintln!(
                "\nNote: --acme is ignored — caddy is already installed.\n  \
                 Edit ~/.local/share/services/caddy/config/tls.caddy to switch TLS mode.\n"
            );
        }

        // Authelia already exists — make sure its exposure isn't narrower
        // than the service we're about to add. Local-only authelia + tailnet
        // service silently breaks OIDC redirects for off-host clients.
        if auth_kind.is_some() && config.auth.is_some() {
            ryra_core::check_auth_exposure_compat(&config, service, url)?;
        }

        // If the user chose auth but no provider is configured, install one,
        // passing the tailscale choice so authelia inherits the parent's
        // exposure (Local → caddy already up from the prompt → auto-derives
        // `*.internal`; Tailscale → propagates --tailscale; Custom → falls
        // through to authelia's own exposure resolution since custom URLs
        // are per-service and can't be inherited).
        if will_install_authelia {
            // A dry run must not mutate anything, but this path installs
            // authelia and/or writes [auth] into preferences.toml (the
            // configure-from-installed branch used to do so even under
            // --dry-run). Bail with instructions instead.
            if dry_run {
                bail!(
                    "--auth needs a configured auth provider, which --dry-run won't set up.\n\
                     Run `ryra add authelia` first, then re-run with --dry-run."
                );
            }
            if !ensure_auth_for_add(&mut config, &paths, exposure.is_tailscale()).await? {
                return Ok(());
            }
            // Reload — ensure_auth_for_add may have installed authelia
            config = ryra_core::config::load_or_default(&paths.config_file)?;
        }

        // Prompt for env vars based on their kind
        use ryra_core::registry::service_def::EnvKind;

        let mut env_overrides = BTreeMap::new();
        let mut prompt_ctx: Option<BTreeMap<String, String>> = None;

        // Validate every --enable name up front — fail fast on typos rather
        // than silently ignoring unknown groups.
        let known_group_names: BTreeSet<&str> = reg_service
            .def
            .env_groups
            .iter()
            .map(|g| g.name.as_str())
            .collect();
        for g in &enable {
            if !known_group_names.contains(g.as_str()) {
                let hint = if known_group_names.is_empty() {
                    format!("service '{service}' defines no env_groups")
                } else {
                    let known: Vec<&str> = known_group_names.iter().copied().collect();
                    format!(
                        "service '{service}' has no env_group '{g}' (known: {})",
                        known.join(", ")
                    )
                };
                bail!("{hint}");
            }
        }
        let mut enabled_groups: BTreeSet<String> = enable.iter().cloned().collect();

        // Seed choice selections from `--choose`, validated against this
        // service's registered choices and options (fail fast on typos).
        let mut selected_choices: BTreeMap<String, String> = BTreeMap::new();
        for (cname, oname) in &choose_pairs {
            let Some(c) = reg_service.def.choices.iter().find(|c| &c.name == cname) else {
                let known: Vec<&str> = reg_service
                    .def
                    .choices
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect();
                let hint = if known.is_empty() {
                    format!("service '{service}' defines no choices")
                } else {
                    format!(
                        "service '{service}' has no choice '{cname}' (known: {})",
                        known.join(", ")
                    )
                };
                bail!("{hint}");
            };
            if !c.options.iter().any(|o| &o.name == oname) {
                let known: Vec<&str> = c.options.iter().map(|o| o.name.as_str()).collect();
                bail!(
                    "choice '{cname}' has no option '{oname}' (known: {})",
                    known.join(", ")
                );
            }
            selected_choices.insert(cname.clone(), oname.clone());
        }

        let has_promptable_top = reg_service
            .def
            .env
            .iter()
            .any(|e| matches!(e.kind, EnvKind::Prompted | EnvKind::Required));
        let has_groups = !reg_service.def.env_groups.is_empty();
        let has_choices = !reg_service.def.choices.is_empty();

        if (has_promptable_top || has_groups || has_choices) && interactive {
            // Resolve template variables in defaults so prompts show real values.
            // This context is reused by add_service so the secrets the user saw
            // during prompts match what gets written to .env.
            let default_ctx = ryra_core::generate::context::build_context(
                &config,
                &reg_service.def,
                None,
                auth_kind.as_ref(),
                &exposure,
                enable_smtp,
            )?;
            prompt_ctx = Some(default_ctx.clone());

            println!("\nConfigure {service}:");

            // Interactive group toggles — ask y/N for each group, then prompt
            // its required/prompted members if the user opted in. Groups
            // passed via --enable are treated as already on (no re-prompt).
            for group in &reg_service.def.env_groups {
                if !enabled_groups.contains(&group.name) {
                    let enabled = Confirm::new()
                        .with_prompt(format!("  {}", group.prompt))
                        .default(false)
                        .interact()?;
                    if !enabled {
                        continue;
                    }
                    enabled_groups.insert(group.name.clone());
                }
                for env in &group.env {
                    if !matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                        continue;
                    }
                    prompt_env(env, &default_ctx, &mut env_overrides)?;
                }
            }

            // Interactive choice selection: one Select per choice (default
            // pre-highlighted), then prompt the chosen option's members.
            // Choices fixed via --choose are not re-prompted.
            for choice in &reg_service.def.choices {
                if !selected_choices.contains_key(&choice.name) {
                    let labels: Vec<&str> = choice
                        .options
                        .iter()
                        .map(|o| o.label.as_deref().unwrap_or(&o.name))
                        .collect();
                    let default_idx = choice
                        .options
                        .iter()
                        .position(|o| o.name == choice.default)
                        .unwrap_or(0);
                    let sel = dialoguer::Select::new()
                        .with_prompt(format!("  {}", choice.prompt))
                        .items(&labels)
                        .default(default_idx)
                        .interact()?;
                    selected_choices.insert(choice.name.clone(), choice.options[sel].name.clone());
                }
                let chosen = selected_choices[&choice.name].clone();
                if let Some(option) = choice.options.iter().find(|o| o.name == chosen) {
                    for env in &option.env {
                        if matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                            prompt_env(env, &default_ctx, &mut env_overrides)?;
                        }
                    }
                }
            }

            // Top-level prompted/required envs.
            for env in &reg_service.def.env {
                if !matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                    continue;
                }
                prompt_env(env, &default_ctx, &mut env_overrides)?;
            }
            println!();
        } else if !interactive {
            // Non-interactive: read env vars from the process environment.
            // Required vars must be set; prompted vars use their default but
            // can be overridden via the environment. Members of groups that
            // aren't `--enable`d are ignored entirely — they won't be written
            // to `.env`, so missing them is not an error.
            let mut missing_required = Vec::new();
            for env in &reg_service.def.env {
                if !matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                    continue;
                }
                collect_non_interactive(env, &mut env_overrides, &mut missing_required);
            }
            for group in &reg_service.def.env_groups {
                if !enabled_groups.contains(&group.name) {
                    continue;
                }
                for env in &group.env {
                    if !matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                        continue;
                    }
                    collect_non_interactive(env, &mut env_overrides, &mut missing_required);
                }
            }
            // Choices: default any not set via --choose, then collect the
            // selected option's required/prompted members from the process env.
            for choice in &reg_service.def.choices {
                let chosen = selected_choices
                    .entry(choice.name.clone())
                    .or_insert_with(|| choice.default.clone())
                    .clone();
                if let Some(option) = choice.options.iter().find(|o| o.name == chosen) {
                    for env in &option.env {
                        if !matches!(env.kind, EnvKind::Prompted | EnvKind::Required) {
                            continue;
                        }
                        collect_non_interactive(env, &mut env_overrides, &mut missing_required);
                    }
                }
            }
            if !missing_required.is_empty() {
                bail!(
                    "required env vars not provided (run interactively or set via env): {}",
                    missing_required.join(", ")
                );
            }
        }

        // --acme only takes effect when first installing the reverse
        // proxy itself — after that, the TLS snippet is user-managed.
        // Filter so it only flows for the reverse-proxy service (the
        // top-level guard already rejects other combos).
        let acme_for_service: Option<&AcmeMode> =
            if service_provides(service, Capability::ReverseProxy) {
                acme.as_ref()
            } else {
                None
            };

        // The prompts above resolved fuzzy intent into the typed,
        // frontend-neutral operation vocabulary; planning goes through
        // the same `ops::plan_add` every frontend uses, so the CLI
        // can't support more (or different) semantics than the API.
        let op_req = ryra_core::ops::AddRequest {
            service: service.to_string(),
            exposure: match &exposure {
                ryra_core::Exposure::Loopback => ryra_core::ops::ExposureRequest::Loopback,
                ryra_core::Exposure::Tailscale { url } => {
                    ryra_core::ops::ExposureRequest::Tailscale(url.clone())
                }
                ryra_core::Exposure::Internal { url } | ryra_core::Exposure::Public { url } => {
                    ryra_core::ops::ExposureRequest::Url(url.clone())
                }
            },
            auth: match &auth_choice {
                ryra_core::AuthChoice::Native(kind) => {
                    ryra_core::ops::AuthRequested::Kind(kind.clone())
                }
                ryra_core::AuthChoice::None => ryra_core::ops::AuthRequested::No,
            },
            smtp: Some(enable_smtp),
            backup,
            env: env_overrides.clone(),
            enable_groups: enabled_groups.clone(),
            choose: selected_choices.clone(),
        };
        // One context builder for both the initial attempt and the
        // post-cleanup retry, so the two calls can't drift apart.
        let plan_ctx = || ryra_core::ops::PlanContext {
            port_in_use: &super::is_port_in_use,
            resolved: Some((&service_ref, repo_dir.as_path())),
            pre_built_ctx: prompt_ctx.clone(),
            port_overrides: BTreeMap::new(),
            mode: ryra_core::PlanMode::Add,
            acme: acme_for_service,
        };

        // If a previous add failed partway, clean up before retrying.
        let planned = match ryra_core::ops::plan_add(&op_req, plan_ctx()).await {
            Err(ryra_core::error::Error::ServiceIncomplete(_)) => {
                // Two cases land here: a previous `ryra add` crashed mid-way
                // (config entry with installed=false), or the user ran
                // `ryra remove <svc>` without --purge and now wants to
                // re-add (no config entry but volumes/home-dir survive).
                // Both produce credential mismatches if we just regenerate
                // secrets on top of existing volumes — recover by purging
                // + reinstalling, but don't do that silently: data loss
                // deserves a confirmation.
                if interactive && !yes {
                    println!("\n  '{service}' has preserved data from a previous install.");
                    println!("  Reinstalling will delete the named volume(s) and service dir.");
                    println!("  Inspect with: ryra data ls\n");
                    let proceed = Confirm::new()
                        .with_prompt(format!("Purge existing data and reinstall {service}?"))
                        .default(false)
                        .interact()?;
                    if !proceed {
                        println!("\nCancelled. To purge and reinstall later:");
                        println!("  ryra remove {service} --purge");
                        println!("  ryra add {service}");
                        return Ok(());
                    }
                }
                println!("{service} has leftover state — cleaning up before retry...");

                // `remove_service` reconstructs metadata from the
                // quadlet headers when available — use it whenever the
                // marker'd `.container` is on disk. For pure-orphan
                // state (data only, no quadlet) fall back to the
                // orphan-purge path.
                if ryra_core::is_service_installed(service) {
                    let remove_result =
                        ryra_core::remove_service(service, ryra_core::RemoveMode::Purge)?;
                    apply::execute_all(&remove_result.steps).await?;
                    ryra_core::finalize_remove(service)?;
                } else {
                    let svc_data =
                        ryra_core::data::enumerate_service(service)?.ok_or_else(|| {
                            anyhow::anyhow!(
                                "internal: ServiceIncomplete for '{service}' but no state found"
                            )
                        })?;
                    let steps = ryra_core::orphan_purge_steps(&svc_data);
                    apply::execute_all(&steps).await?;
                }
                // The killed previous install may have reserved a Tailscale
                // port; cleanup just removed that reservation, so re-allocate
                // against the freshly-cleaned config to reclaim the originally
                // intended port (e.g. 443 instead of skipping to 8443).
                // (No URL re-resolution needed: with per-service tailnet
                // nodes, the URL is `https://<service>.<tailnet>/` and
                // doesn't depend on any pool that the killed previous
                // install might have reserved.)
                // Retry now that the partial state is gone
                ryra_core::ops::plan_add(&op_req, plan_ctx()).await?
            }
            other => other?,
        };
        for note in &planned.notes {
            println!("  Note: {note}");
        }
        let result = &planned.result;

        // (Tailscale exposures: add_service emits the TailscaleSetup /
        // TailscaleEnable steps itself; nothing extra to do here.)

        // Show warnings and confirm
        // Show port reassignment notes + reverse-proxy hints (both informational).
        for warning in &result.warnings {
            match warning {
                Warning::PortReassigned {
                    port_name,
                    original_port,
                    assigned_port,
                    reason,
                    ..
                } => {
                    // Privileged ports are just informational — nothing the user can do
                    // in rootless podman. "In use" ports are actionable — the user might
                    // want to stop whatever is occupying the port.
                    if *original_port < 1024 {
                        println!("  {port_name} port {original_port} → {assigned_port} ({reason})");
                    } else {
                        println!(
                            "  {} {port_name} port {original_port} → {assigned_port} ({reason})",
                            super::style::warning()
                        );
                    }
                }
                Warning::UrlWithoutReverseProxy {
                    service_name,
                    url,
                    host_port,
                } => {
                    println!(
                        "  {} --url was set for {service_name} but no ryra-managed reverse \
                         proxy (Caddy) is installed. Ryra will template {url} into the service \
                         but won't configure routing — point your own reverse proxy (nginx, \
                         Cloudflare Tunnel, Tailscale Funnel, etc.) at 127.0.0.1:{host_port}, \
                         or run `ryra add caddy` to let ryra handle it.",
                        super::style::note()
                    );
                }
                Warning::RamBelowMinimum { .. } | Warning::RamBelowRecommended { .. } => {}
            }
        }

        // Collect warnings that need confirmation (RAM + port conflicts)
        let needs_confirm: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| match w {
                Warning::RamBelowMinimum { .. } | Warning::RamBelowRecommended { .. } => true,
                Warning::PortReassigned { original_port, .. } => *original_port >= 1024,
                Warning::UrlWithoutReverseProxy { .. } => false,
            })
            .collect();

        if !needs_confirm.is_empty() {
            // Show RAM warnings
            for warning in &needs_confirm {
                match warning {
                    Warning::RamBelowMinimum {
                        service_name,
                        min_mb,
                        available_mb,
                    } => {
                        println!(
                            "  {} {service_name} requires at least {min_mb} MB RAM, \
                         but this system has {available_mb} MB — service may fail to start",
                            super::style::warning()
                        );
                    }
                    Warning::RamBelowRecommended {
                        service_name,
                        recommended_mb,
                        available_mb,
                    } => {
                        println!(
                            "  {} {service_name} recommends {recommended_mb} MB RAM, \
                         but this system has {available_mb} MB — performance may be degraded",
                            super::style::note()
                        );
                    }
                    Warning::PortReassigned { .. } | Warning::UrlWithoutReverseProxy { .. } => {}
                }
            }
            println!();

            if interactive && !dry_run {
                let confirmed = Confirm::new()
                    .with_prompt("Continue?")
                    .default(true)
                    .interact()?;
                if !confirmed {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
        }

        if dry_run {
            super::print_dry_run(&result.steps);
            println!("{service} will be started.");
        } else {
            // Record the service as pending before executing steps.
            // If execution fails, ryra knows about the service and can clean up.
            planned.record_pending()?;

            // Preview what's about to happen — the user sees "pulls / writes /
            // starts" before the steps run. If --url wasn't set, fall back to
            // the primary loopback address so the line always ends with a URL.
            let url_display = result.url.clone().or_else(|| {
                result
                    .allocated_ports
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("http"))
                    .or_else(|| result.allocated_ports.first())
                    .map(|(_, p)| format!("http://127.0.0.1:{p}"))
            });
            super::print_plan_header(&result.steps, service, url_display.as_deref());

            if let Err(e) = apply::execute_all(&result.steps).await {
                eprintln!(
                    "\n{} {e}",
                    super::style::error_prefix("Error during setup:")
                );
                eprintln!("Cleaning up partial installation...");
                // Attempt cleanup so the user doesn't have to do it manually.
                // If cleanup fails, fall back to telling the user how to do it.
                match ryra_core::remove_service(service, ryra_core::RemoveMode::Purge) {
                    Ok(remove_result) => {
                        if let Err(cleanup_err) = apply::execute_all(&remove_result.steps).await {
                            eprintln!("Cleanup also failed: {cleanup_err}");
                            eprintln!("Run manually: ryra remove {service}");
                        } else {
                            if let Err(e) = ryra_core::finalize_remove(service) {
                                eprintln!("Warning: finalize_remove failed: {e}");
                            }
                            // Clear lingering `failed` flags on user units so the
                            // next `ryra add` isn't poisoned by stale systemd state.
                            let _ = std::process::Command::new("systemctl")
                                .args(["--user", "reset-failed"])
                                .status();
                            eprintln!("Cleaned up. Retry with: ryra add {service}");
                        }
                    }
                    Err(_) => {
                        eprintln!("Could not clean up automatically.");
                        eprintln!("Run manually: ryra remove {service}");
                    }
                }
                return Err(e);
            }

            // Trust Caddy's self-signed CA, and register the service's
            // hostname in /etc/hosts for browser access. Only fires for
            // *.internal URLs (Caddy local) — Tailscale's MagicDNS handles
            // *.ts.net for free, and External hostnames are the user's
            // DNS to manage.
            if let Some(service_url) = result.url.as_deref()
                && ryra_core::is_caddy_local_url(service_url)
            {
                setup_host_access(service, &[service_url]);
            }

            let home_dir = ryra_core::service_home(service)?;
            if let Some(ref url) = result.url {
                println!("\n{service} is running at {url}");
                // Tailnet URLs route via Tailscale Service VIPs, which a
                // host advertising the service can't reach back through —
                // tailscaled doesn't add a route to its own service VIPs.
                // Without this nudge the user opens the URL in their
                // browser on the same machine, gets a hang, and assumes
                // the install is broken. The loopback fallback is HTTP
                // only — services that mandate HTTPS (vaultwarden,
                // authelia) won't accept it, so we flag that too.
                if ryra_core::is_tailscale_url(url) {
                    println!("  Note: tailnet URLs don't loop back to this host. From other");
                    println!("        tailnet devices, the URL above works (HTTPS via Tailscale).");
                    if let Some((_, p)) = result.allocated_ports.first() {
                        println!("        Locally on this host: http://127.0.0.1:{p} (HTTP only —");
                        println!("        services that require HTTPS won't accept it; reinstall");
                        println!(
                            "        without --tailscale for Caddy-local HTTPS at *.internal)."
                        );
                    }
                }
            } else {
                println!("\n{service} is running.");
            }
            println!("  May take a moment to start. Check: systemctl --user status {service}");

            // Connection info — skip localhost URLs when a proper URL is displayed
            if result.url.is_none() && !result.allocated_ports.is_empty() {
                for (_, host_port) in &result.allocated_ports {
                    println!("  URL: http://127.0.0.1:{host_port}");
                }
            }
            if !result.generated_secrets.is_empty() {
                // Show generated secret values so the user can log in
                let env_path = home_dir.join(".env");
                let env_content = match std::fs::read_to_string(&env_path) {
                    Ok(content) => content,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        eprintln!("  Warning: could not read {}: {e}", env_path.display());
                        String::new()
                    }
                };
                println!("  Secrets (auto-generated):");
                for secret_name in &result.generated_secrets {
                    // Find the env var that used this secret template
                    let matching_env = env_content.lines().find(|l| {
                        l.split_once('=')
                            .map(|(k, _)| k.to_lowercase().contains(secret_name))
                            .unwrap_or(false)
                    });
                    if let Some(line) = matching_env
                        && let Some((key, val)) = line.split_once('=')
                    {
                        println!("    {key}={val}");
                        continue;
                    }
                    println!("    {secret_name} (see .env)");
                }
            }
            println!("  Config:  {}", home_dir.display());

            // Mail-capable service installed without a configured SMTP relay:
            // its email features (account verification, login codes, password
            // resets, notifications) silently won't send. Flag once at install
            // so the user isn't mystified later — e.g. Ente gates both signup
            // and every new login on an emailed code, which otherwise only
            // lands in the service logs.
            if reg_service.def.integrations.smtp && !enable_smtp {
                println!();
                println!(
                    "  Note: no SMTP configured — email features (e.g. account verification /"
                );
                println!(
                    "        login codes) won't send. Wire mail with: ryra config {service} --smtp"
                );
            }

            let env_path = home_dir.join(".env");
            println!();
            println!("Commands:");
            println!("  cat {}  # view config", env_path.display());
            println!("  systemctl --user restart {service}  # restart (picks up .env changes)");
            println!("  journalctl --user-unit {service}.service -f  # follow logs");

            // Registry-authored guidance for the manual steps that can't be
            // automated (web wizards, recommended dashboard imports, …).
            if let Some(notes) = &reg_service.def.service.post_install {
                println!();
                println!("Next steps:");
                for line in notes.trim_end().lines() {
                    println!("  {line}");
                }
            }

            // Caddy-only: tell the user which TLS mode they got and where
            // to switch to a different one. The snippet path is the only
            // thing they need to know to swap in Cloudflare DNS-01,
            // wildcards, BYO certs, plain HTTP for Tunnel, etc.
            if WellKnownService::Caddy.matches(service) {
                let snippet_pathbuf = ryra_core::caddy::tls_snippet_path().ok();
                let snippet_path = snippet_pathbuf
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| {
                        "~/.local/share/services/caddy/config/tls.caddy".to_string()
                    });
                // Read tls.caddy from disk and report what's actually in
                // effect — not what `--acme` asked for. This matters when a
                // pre-existing snippet was preserved across re-installs
                // (or hand-edited to a custom shape ryra doesn't write).
                // Falls back to the flag value if the read fails or the
                // snippet doesn't match a known ryra-written shape.
                let detected_mode: Option<AcmeMode> = snippet_pathbuf
                    .as_ref()
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|s| AcmeMode::detect_from_snippet(&s));
                println!();
                let displayed_mode: AcmeMode = detected_mode
                    .clone()
                    .or_else(|| acme_for_service.cloned())
                    .unwrap_or(AcmeMode::Internal);
                match &displayed_mode {
                    AcmeMode::WithEmail(email) => {
                        println!("TLS: Let's Encrypt ({email})");
                    }
                    AcmeMode::Anonymous => {
                        println!("TLS: Let's Encrypt (anonymous — no renewal notices)");
                    }
                    AcmeMode::Internal => {
                        println!(
                            "TLS: self-signed (LAN — browsers warn unless ryra's CA is trusted)"
                        );
                    }
                    AcmeMode::Byo { cert, key } => {
                        println!("TLS: your own certificate ({cert} + {key})");
                    }
                }
                // If the snippet on disk doesn't match a ryra-written
                // shape at all, say so explicitly so the user isn't misled
                // into thinking ryra is managing it.
                if detected_mode.is_none() && acme_for_service.is_none() {
                    println!("  (note: tls.caddy looks user-customized — leaving it untouched)");
                }
                if matches!(displayed_mode, AcmeMode::WithEmail(_) | AcmeMode::Anonymous) {
                    let (http_port, https_port) = result.allocated_ports.iter().fold(
                        (8080u16, 8443u16),
                        |(h, hs), (n, p)| match n.as_str() {
                            "http" => (*p, hs),
                            "https" => (h, *p),
                            _ => (h, hs),
                        },
                    );
                    println!("  For LE to issue certs Caddy must be reachable from the internet:");
                    println!("    - DNS A/AAAA for each --url host must point at this machine");
                    if http_port == 80 && https_port == 443 {
                        println!(
                            "    - Caddy listens on host 80/443; forward router 80→80 and 443→443"
                        );
                        println!("    - Firewall must allow 80/443 (ufw / firewalld / nft varies)");
                    } else {
                        println!(
                            "    - Caddy listens on host {http_port}/{https_port} (rootless); \
                             forward router 80→{http_port} and 443→{https_port}"
                        );
                        println!(
                            "    - Firewall must allow {http_port}/{https_port} \
                             (ufw / firewalld / nft varies)"
                        );
                    }
                    println!("  Cert issuance is async — watch progress with:");
                    println!("    journalctl --user -u caddy -f");
                } else {
                    println!(
                        "  For Let's Encrypt:  ryra remove caddy && ryra add caddy --acme you@example.com"
                    );
                }
                println!("  For Cloudflare DNS-01, wildcards, or BYO certs: edit {snippet_path}");
            }
        }
    } // end for service_input in services

    // Remind the user about lingering — if they reboot or log out without
    // it, every service we just wrote a quadlet for will silently stop.
    // On the very first `ryra add`, offer to enable it inline (the user's
    // paying attention). On later adds, just warn so we're not noisy.
    // Skip in dry-run; a plan shouldn't produce system-state prompts.
    if !dry_run {
        if first_run {
            super::linger::offer_enable().await?;
        } else {
            super::linger::warn_if_disabled().await?;
        }
    }

    Ok(())
}

/// Prompt for a single `prompted`/`required` env var during interactive add.
/// Shared between top-level `[[env]]` and group members so both follow the
/// same default-resolution + override-capture rules.
fn prompt_env(
    env: &ryra_core::registry::service_def::EnvVar,
    default_ctx: &BTreeMap<String, String>,
    env_overrides: &mut BTreeMap<String, String>,
) -> Result<()> {
    use ryra_core::registry::service_def::EnvKind;
    let prompt_text = env.prompt.as_deref().unwrap_or(&env.name);
    if env.kind == EnvKind::Required {
        let value: String = Input::new()
            .with_prompt(format!("    {prompt_text} (required)"))
            .interact_text()?;
        env_overrides.insert(env.name.clone(), value);
    } else {
        let resolved_default = ryra_core::generate::template::render(&env.value, default_ctx)
            .unwrap_or_else(|_| env.value.clone());
        let value: String = Input::new()
            .with_prompt(format!("    {prompt_text}"))
            .default(resolved_default.clone())
            .interact_text()?;
        // Always save for secret-templated values — re-rendering would
        // generate a different random secret.
        if value != resolved_default {
            env_overrides.insert(env.name.clone(), value);
        } else if env.value.contains("{{secret.") {
            env_overrides.insert(env.name.clone(), resolved_default);
        }
    }
    Ok(())
}

/// Pull a single env var's value from the process environment for
/// non-interactive add. `Required` vars go on the `missing` list if absent
/// so the caller can bail with a single consolidated error.
fn collect_non_interactive<'a>(
    env: &'a ryra_core::registry::service_def::EnvVar,
    env_overrides: &mut BTreeMap<String, String>,
    missing: &mut Vec<&'a str>,
) {
    use ryra_core::registry::service_def::EnvKind;
    if let Ok(val) = std::env::var(&env.name) {
        env_overrides.insert(env.name.clone(), val);
    } else if env.kind == EnvKind::Required {
        missing.push(env.name.as_str());
    }
}

/// Register Caddy's CA with every rootless trust store we can reach, and
/// print (but never run) hints for the stores that need sudo — `/etc/hosts`
/// for any hostname the user's resolver can't reach (including
/// `*.internal`, which unlike `*.localhost` does not auto-resolve), and
/// the system trust bundle for curl/wget/Firefox-on-p11-kit users.
///
/// The rootless work covers Chromium-family browsers (via the user's
/// `~/.pki/nssdb`) and every Firefox profile with a `cert9.db`. That's the
/// mkcert pattern and enough for ~95% of browser traffic on Linux.
fn setup_host_access(service: &str, domains: &[&str]) {
    use std::process::Command;

    // --- CA source: whatever Caddy's first start wrote out ---
    let ca_source = ryra_core::service_home("caddy")
        .map(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: could not resolve caddy service home: {e}");
            None
        })
        .filter(|p| p.exists());

    // certutil (from `nss-tools` / `libnss3-tools`) drives every rootless
    // trust step below. If it's missing, skip them all and point the user
    // at the package — otherwise each certutil call would fail with a
    // confusing `No such file or directory` warning.
    let have_certutil = Command::new("certutil").arg("-V").output().is_ok();
    if !have_certutil {
        println!();
        println!("  Note: `certutil` is not installed, so ryra can't register the Caddy CA");
        println!("  with Chromium or Firefox automatically. To enable rootless trust:");
        println!("    Fedora/RHEL:   sudo dnf install nss-tools");
        println!("    Debian/Ubuntu: sudo apt install libnss3-tools");
        println!("    Arch:          sudo pacman -S nss");
        println!("  Then re-run `ryra add caddy` (or click through the browser warning).");
    }

    // --- Rootless: user NSS DB (Chromium, Edge, Brave, Opera, Vivaldi) ---
    if have_certutil && let (Some(nssdb_path), Some(ca)) = (super::nssdb_dir(), ca_source.as_ref())
    {
        if !nssdb_path.exists() {
            if let Err(e) = std::fs::create_dir_all(&nssdb_path) {
                eprintln!("  Warning: could not create {}: {e}", nssdb_path.display());
            } else {
                let init = Command::new("certutil")
                    .args([
                        "-N",
                        "-d",
                        &format!("sql:{}", nssdb_path.display()),
                        "--empty-password",
                    ])
                    .status();
                match init {
                    Ok(s) if s.success() => {}
                    _ => eprintln!(
                        "  Warning: could not initialize NSS DB at {}",
                        nssdb_path.display()
                    ),
                }
            }
        }
        if nssdb_path.exists() {
            add_ca_to_nssdb(
                &format!("sql:{}", nssdb_path.display()),
                ca,
                "Chromium family",
            );
        }
    }

    // --- Rootless: every Firefox profile we can find ---
    if have_certutil && let Some(ca) = ca_source.as_ref() {
        for profile in super::firefox_profile_dirs() {
            add_ca_to_nssdb(
                &format!("sql:{}", profile.display()),
                ca,
                &format!("Firefox profile {}", profile.display()),
            );
        }
    }

    // --- /etc/hosts: write via sudo -n if available, else print hint ---
    //
    // `.internal` doesn't auto-resolve; ryra-managed services need a
    // `127.0.0.1 <host>` entry. We try `sudo -n` (non-interactive) first
    // so CI/test VMs with passwordless sudo get it transparently, and
    // fall back to a printed hint for interactive users whose sudo
    // requires a password.
    let hostnames: Vec<String> = domains
        .iter()
        .filter_map(|d| {
            url::Url::parse(d)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
        })
        // Tailscale MagicDNS already resolves *.ts.net — skip /etc/hosts dance.
        .filter(|h| !h.to_ascii_lowercase().ends_with(".ts.net"))
        .collect();
    let hosts_content = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
    let mut missing_hosts: Vec<&str> = hostnames
        .iter()
        .filter(|h| {
            !hosts_content.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.split_whitespace().any(|w| w == h.as_str())
            })
        })
        .map(String::as_str)
        .collect();

    if !missing_hosts.is_empty() {
        // Append a sentinel comment so `ryra remove` can later identify
        // and remove only entries it added — handwritten entries (no
        // marker) are left alone.
        let line = format!(
            "127.0.0.1 {}  # Service-Source: registry/{service}",
            missing_hosts.join(" ")
        );
        // First, try non-interactive sudo — works on CI/test VMs with
        // passwordless sudo and is silent otherwise. If that fails AND
        // stderr is a TTY, escalate to interactive sudo so the user can
        // punch in their password. In headless contexts (stderr not a
        // TTY, e.g. `ryra test --no-vm` capturing output) we skip the
        // interactive prompt and just surface a loud warning below.
        let cmd = format!("echo '{line}' >> /etc/hosts");
        let sudo_n = Command::new("sudo")
            .args(["-n", "sh", "-c", &cmd])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let wrote = if sudo_n {
            true
        } else if std::io::stderr().is_terminal() {
            eprintln!(
                "  Adding {} to /etc/hosts (sudo required):",
                missing_hosts.join(", ")
            );
            Command::new("sudo")
                .args(["sh", "-c", &cmd])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        };
        if wrote {
            println!(
                "  Added {} to /etc/hosts (via sudo).",
                missing_hosts.join(", ")
            );
            missing_hosts.clear();
        } else {
            // Emit a loud warning to stderr so it survives stdout capture
            // in test harnesses. Without this entry, the service's URL
            // won't resolve and subsequent health probes will silently
            // spin until they hit the polling limit.
            eprintln!();
            eprintln!(
                "  WARN: {} not in /etc/hosts — the service URL won't resolve.",
                missing_hosts.join(", ")
            );
            eprintln!("        Run:  echo '{line}' | sudo tee -a /etc/hosts");
            eprintln!();
        }
    }

    let ca_target = super::CA_TARGETS.iter().find(|t| {
        let dir = std::path::Path::new(t.cert_path)
            .parent()
            .unwrap_or(std::path::Path::new("/"));
        dir.is_dir()
    });
    let need_system_ca = ca_source.is_some()
        && ca_target.is_some()
        && !super::CA_TARGETS
            .iter()
            .any(|t| std::path::Path::new(t.cert_path).exists());

    if missing_hosts.is_empty() && !need_system_ca {
        return;
    }

    println!();
    println!("  Optional (requires sudo) — run yourself if you need these:");
    if !missing_hosts.is_empty() {
        println!(
            "    echo '127.0.0.1 {}' | sudo tee -a /etc/hosts",
            missing_hosts.join(" ")
        );
    }
    if let (true, Some(ca), Some(target)) = (need_system_ca, ca_source.as_ref(), ca_target) {
        println!(
            "    sudo cp {} {} && sudo {}",
            ca.display(),
            target.cert_path,
            target.update_cmd,
        );
        println!("    (lets curl/wget and Firefox with p11-kit trust the Caddy CA too)");
    }
    println!();
}

/// Try to add the Caddy CA to an NSS DB at `nss_arg` (e.g. `sql:~/.pki/nssdb`
/// or `sql:<firefox-profile-dir>`). No-op if it's already there. The `context`
/// is included in any warning so the user can tell which store failed.
fn add_ca_to_nssdb(nss_arg: &str, ca: &std::path::Path, context: &str) {
    use std::process::Command;
    let present = Command::new("certutil")
        .args(["-d", nss_arg, "-L", "-n", super::CADDY_CA_NICKNAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return;
    }
    let status = Command::new("certutil")
        .args([
            "-d",
            nss_arg,
            "-A",
            "-t",
            "C,,",
            "-n",
            super::CADDY_CA_NICKNAME,
            "-i",
            &ca.display().to_string(),
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("  Caddy CA added to {context}."),
        Ok(s) => eprintln!("  Warning: certutil exited with {s} for {context}"),
        Err(e) => eprintln!("  Warning: could not run certutil for {context}: {e}"),
    }
}

/// Ensure `config.tailscale.admin_api_key` is set, prompting (interactive)
/// or reading from `TAILSCALE_API_KEY` (non-interactive) if not.
///
/// Persists to preferences.toml so the user pastes their token once and every
/// subsequent `--tailscale` install + remove reuses it for service
/// definition + ACL setup via the admin API.
/// Run [`ensure_tailscale_admin_token`] if the plan needs to *register*
/// something with the tailnet (Setup or Enable). Disable is intentionally
/// excluded: if there's no `[tailscale]` config at remove/configure time
/// the executor skips the API call rather than demanding a token the
/// user only needs for setup — cleaning up corrupted install state
/// shouldn't require "first-time Tailscale setup."
pub(super) async fn ensure_tailscale_token_for_steps(
    steps: &[ryra_core::Step],
    interactive: bool,
) -> Result<()> {
    let needs = steps.iter().any(|s| {
        matches!(
            s,
            ryra_core::Step::TailscaleSetup | ryra_core::Step::TailscaleEnable { .. }
        )
    });
    if needs {
        ensure_tailscale_admin_token(interactive).await?;
    }
    Ok(())
}

pub(super) async fn ensure_tailscale_admin_token(interactive: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.tailscale.is_some() {
        return Ok(()); // already cached
    }

    let admin_api_key = if interactive {
        prompt_tailscale_admin_token()?
    } else {
        std::env::var("TAILSCALE_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "--tailscale needs a Tailscale admin API token. Set TAILSCALE_API_KEY \
                 (tskey-api-…) or run interactively to be prompted.\n\
                 Generate one at https://login.tailscale.com/admin/settings/keys \
                 (use the \"API access token\" type, not an auth key)"
            )
        })?
    };

    let had_secrets_before = config.has_secrets();
    config.tailscale = Some(ryra_core::config::schema::TailscaleConfig {
        admin_api_key,
        tailnet: None,
    });
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!(
        "  ✓ Tailscale admin token saved to {}",
        paths.config_file.display()
    );
    warn_if_first_secret_save(&paths, had_secrets_before, &config);
    Ok(())
}

/// Interactive prompt for a Tailscale admin API token.
///
/// Validates the `tskey-api-` prefix so pastes of the wrong thing (e.g.
/// a pre-auth key, an OAuth client secret) get rejected with a clear
/// hint instead of failing later when the API call returns 401.
fn prompt_tailscale_admin_token() -> Result<String> {
    println!();
    println!("First-time Tailscale setup — paste an admin API token.");
    println!("  Generate at: https://login.tailscale.com/admin/settings/keys");
    println!("  Type:        \"API access token\" (NOT an auth key)");
    println!();
    println!("  ryra uses this to define Tailscale Services in your tailnet,");
    println!("  set up the ACL with auto-approval, and apply tag:ryra-host");
    println!("  to this machine — all so `ryra add … --tailscale` is one step.");
    println!();

    let raw: String = Input::new()
        .with_prompt("Tailnet admin API token")
        .validate_with(|input: &String| -> std::result::Result<(), &str> {
            let s = input.trim();
            if s.starts_with("tskey-api-") {
                Ok(())
            } else {
                Err(
                    "Admin API tokens start with `tskey-api-`. The other tskey-* \
                     prefixes (auth-, client-) are for joining devices, not for \
                     admin operations. Generate one at \
                     https://login.tailscale.com/admin/settings/keys with type \
                     \"API access token\".",
                )
            }
        })
        .interact_text()?;
    Ok(raw.trim().to_string())
}

/// Smoke-test DNS for `host` so we can warn before burning a Let's Encrypt
/// validation slot on a hostname that isn't pointed anywhere yet. Only
/// catches the "DNS not configured at all" case — a record pointing at
/// the wrong IP would still fail in Caddy. That's fine: this exists to
/// stop the most common rate-limit pitfall, not validate the full setup.
async fn dns_resolves(host: &str) -> bool {
    tokio::net::lookup_host((host, 0u16))
        .await
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Before we hand a public URL to Caddy in ACME mode, check that DNS
/// resolves for it. If it doesn't, warn loudly about LE rate limits and
/// (if interactive) ask the user to confirm before continuing — failed
/// validations count against ~5/hour limits per registered domain, so
/// looping `ryra add` against a half-configured domain can lock you out
/// of real issuance for hours.
async fn dns_preflight_for_acme(url: &str, interactive: bool) -> Result<()> {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
    else {
        return Ok(());
    };
    if dns_resolves(&host).await {
        return Ok(());
    }
    eprintln!("\n  Warning: DNS for '{host}' doesn't resolve.");
    eprintln!("  Caddy will request a Let's Encrypt cert and fail repeatedly until DNS is fixed.");
    eprintln!(
        "  Each failure counts against LE rate limits (~5 failed validations/hour per domain)."
    );
    if !interactive {
        eprintln!("  (Continuing — you're running non-interactively.)");
        return Ok(());
    }
    let proceed = Confirm::new()
        .with_prompt(format!("Continue installing Caddy with LE for '{host}'?"))
        .default(false)
        .interact()?;
    if !proceed {
        bail!("aborted: fix DNS for '{host}' and re-run, or pick a different TLS option");
    }
    Ok(())
}

/// User's choice for how Caddy should issue TLS for a public URL.
/// The `LetsEncrypt` variant carries an [`AcmeMode`] — the LE-specific
/// subset is `Anonymous` or `WithEmail(...)` — so the install-time
/// translation to the snippet is a single match instead of an
/// `if email.is_empty()` branch.
enum TlsHandling {
    /// Caddy auto-issues real certs from Let's Encrypt. Requires DNS
    /// pointing at this host and Caddy reachable from the internet
    /// (ACME challenge).
    LetsEncrypt(AcmeMode),
    /// Caddy issues self-signed certs from its internal CA. Browsers warn
    /// unless the CA is trusted. LAN-friendly default.
    SelfSigned,
    /// Don't install Caddy — the user is fronting with their own reverse
    /// proxy (Cloudflare Tunnel, nginx, external Caddy, etc.).
    External,
}

/// Prompt the user how TLS should be handled for `url` when Caddy isn't
/// installed. Fires from the per-service add when `--url` points at a
/// public host. Picks the email inline for the LE branch.
async fn prompt_tls_for_public_url(url: &str) -> Result<TlsHandling> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|s| s.to_string()))
        .unwrap_or_else(|| url.to_string());
    println!();
    println!("'{host}' is a public URL but Caddy (reverse proxy) isn't installed.");
    let items = &[
        "Let's Encrypt — Caddy auto-issues real certs (DNS + Caddy reachable from the internet)",
        "Self-signed (LAN) — Caddy local CA, browsers warn unless trusted",
        "External — I'll handle TLS myself (Cloudflare Tunnel, nginx, etc.)",
    ];
    let selection = dialoguer::Select::new()
        .with_prompt("How should TLS be handled?")
        .items(items)
        .default(0)
        .interact()?;
    match selection {
        0 => {
            let email: String = Input::new()
                .with_prompt(
                    "Email for Let's Encrypt (optional — for renewal notices, press Enter to skip)",
                )
                .allow_empty(true)
                .interact_text()?;
            Ok(TlsHandling::LetsEncrypt(AcmeMode::from_email(&email)))
        }
        1 => Ok(TlsHandling::SelfSigned),
        _ => Ok(TlsHandling::External),
    }
}

/// Interactive prompt: how should this service be reachable? Returns
/// the typed [`Exposure`] decision after applying any side-effects the
/// choice implies (installing Caddy, running tailscale preflight, etc.).
///
/// Called from the per-service URL resolver in two cases:
///   * `needs_https = true` and neither `--url` nor `--tailscale` was
///     given: `allow_loopback = false`, default = Tailscale (recommended).
///   * `kind = Application` without an HTTPS requirement: surfaces the
///     same exposure choices (so users can put e.g. vikunja on a tailnet)
///     with `allow_loopback = true`, default = Loopback.
async fn prompt_exposure_for(
    service: &str,
    auth_will_inherit: bool,
    allow_loopback: bool,
) -> Result<ryra_core::Exposure> {
    // Keep Loopback as option 0 when allowed so default-Enter preserves
    // the previous "no exposure prompt" behavior. The match arms below
    // adjust their indices via `loopback_offset` to absorb the shift.
    let mut items: Vec<&str> = Vec::with_capacity(5);
    if allow_loopback {
        items.push("Local only — http://127.0.0.1 on this machine (no proxy)");
    }
    items.extend_from_slice(&[
        "Tailscale (recommended): access from anywhere in your own global network",
        "Self-signed (LAN) — Caddy local CA at *.internal (browsers warn unless trusted)",
        "Public + Let's Encrypt — Caddy issues real certs (DNS + Caddy reachable from the internet)",
        "External — I have my own reverse proxy (Cloudflare Tunnel, nginx, etc.)",
    ]);
    if auth_will_inherit {
        println!(
            "(authelia will inherit this choice — install it separately first if you need a different exposure)"
        );
    }
    let selection = dialoguer::Select::new()
        .with_prompt(format!("How will '{service}' be reachable?"))
        .items(&items)
        .default(0)
        .interact()?;

    let loopback_offset: usize = if allow_loopback { 1 } else { 0 };
    if allow_loopback && selection == 0 {
        return Ok(ryra_core::Exposure::Loopback);
    }
    match selection - loopback_offset {
        0 => {
            // Tailscale: same path as `--tailscale` flag — preflight,
            // ensure auth key, derive `https://<service>.<tailnet>/`.
            // Failures bail rather than save partial state.
            if let Err(e) = ryra_core::system::doctor::check_tailscale_runtime() {
                bail!("Tailscale not ready:\n\n{e}");
            }
            ensure_tailscale_admin_token(true).await?;
            Ok(ryra_core::Exposure::Tailscale {
                url: ryra_core::system::tailscale::derive_service_url(service)?,
            })
        }
        1 => {
            // Self-signed (LAN): install Caddy in default mode if it isn't
            // already there, then derive the *.internal URL using its
            // allocated HTTPS port. Match the planner's `installed`-aware
            // check — a stale `installed = false` entry from a killed
            // previous install must not count as ready.
            if !ryra_core::is_service_installed("caddy") {
                println!("\nInstalling caddy (self-signed LAN mode)...\n");
                Box::pin(run(AddRequest::dependency(WellKnownService::Caddy))).await?;
            }
            let installed_all = ryra_core::list_installed().unwrap_or_default();
            let caddy_https_port =
                find_installed_provider(&installed_all, Capability::ReverseProxy)
                    .and_then(|s| s.ports.get("https").copied())
                    .unwrap_or(DEFAULT_CADDY_HTTPS_PORT);
            Ok(ryra_core::Exposure::Internal {
                url: format!(
                    "https://{service}.{}:{caddy_https_port}",
                    ryra_core::config::schema::CADDY_LOCAL_DOMAIN
                ),
            })
        }
        2 => {
            // Public + Let's Encrypt: ask for the public URL and the LE
            // registration email, install Caddy in ACME mode if it isn't
            // already there, then return the URL. If Caddy is already
            // installed (rare here — this prompt only fires when caddy is
            // missing — but still possible if a prior step installed it),
            // skip re-installing and warn that tls.caddy may need editing.
            let url: String = Input::new()
                .with_prompt(format!("Public URL for '{service}'"))
                .interact_text()?;
            if !ryra_core::is_service_installed("caddy") {
                let email: String = Input::new()
                    .with_prompt("Email for Let's Encrypt (optional — for renewal notices, press Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;
                dns_preflight_for_acme(&url, true).await?;
                let mode = AcmeMode::from_email(&email);
                println!("\nInstalling caddy (Let's Encrypt mode)...\n");
                Box::pin(run(AddRequest {
                    acme: Some(mode),
                    ..AddRequest::dependency(WellKnownService::Caddy)
                }))
                .await?;
            } else {
                eprintln!(
                    "  Note: caddy is already installed — using its existing TLS mode.\n  \
                     Edit ~/.local/share/services/caddy/config/tls.caddy to switch to Let's Encrypt."
                );
            }
            Ok(ryra_core::Exposure::Public { url })
        }
        _ => {
            // External: user is fronting with their own reverse proxy.
            // Prompt for the URL but don't touch Caddy.
            let url: String = Input::new()
                .with_prompt(format!("Public URL for '{service}'"))
                .interact_text()?;
            Ok(ryra_core::Exposure::Public { url })
        }
    }
}

/// Open and lock the cross-process OIDC registration lock file. Serializes
/// concurrent auth-enabled `ryra add` runs that edit authelia's
/// configuration.yml in-memory and write it back, so one process's client
/// entry can't be clobbered by another's. Released when the returned file
/// drops.
fn acquire_auth_lock(paths: &ryra_core::config::ConfigPaths) -> Result<std::fs::File> {
    paths.ensure_dirs()?;
    let lock_path = paths.config_dir.join(".authelia-oidc.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    file.lock()?;
    Ok(file)
}

/// Fire a one-time security note when preferences.toml just acquired its
/// first credential (SMTP, Tailscale token, etc.). Compares pre- and
/// post-save state so the message only prints on the transition, not on
/// every save. Terse on purpose: the file mode is already 0600.
fn warn_if_first_secret_save(
    paths: &ryra_core::config::ConfigPaths,
    had_secrets_before: bool,
    config: &ryra_core::config::schema::Config,
) {
    if !had_secrets_before && config.has_secrets() {
        println!(
            "  Note: credentials saved to {} (mode 0600 / do not commit or share).",
            paths.config_file.display()
        );
    }
}

/// Auto-install inbucket and point `config.smtp` at it for `--smtp=inbucket`.
/// Idempotent: does nothing if `config.smtp` is already set.
async fn ensure_smtp_for_add(provider: SmtpProvider) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

    if config.smtp.is_some() {
        // Already configured — whether by a previous --smtp, prompt, or a
        // hand-edited preferences.toml. Don't clobber it.
        return Ok(());
    }

    match provider {
        SmtpProvider::Inbucket => {
            if !ryra_core::is_service_installed("inbucket") {
                println!("\nInstalling inbucket...\n");
                Box::pin(run(AddRequest::dependency(WellKnownService::Inbucket))).await?;
                // Reload — the inner run() mutated config on disk.
                config = ryra_core::config::load_or_default(&paths.config_file)?;
            }
            let had_secrets_before = config.has_secrets();
            config.smtp = Some(ryra_core::config::schema::SmtpCredentials::inbucket());
            paths.ensure_dirs()?;
            ryra_core::config::save_config(&paths.config_file, &config)?;
            println!(
                "  SMTP configured (inbucket). Saved to {}\n",
                paths.config_file.display()
            );
            warn_if_first_secret_save(&paths, had_secrets_before, &config);
        }
    }

    Ok(())
}

/// Auto-install authelia when `--auth` requires it.
///
/// Propagates the parent invocation's `--tailscale` flag so that
/// `ryra add seafile --auth --tailscale` ends up with both seafile *and*
/// authelia exposed on the tailnet — without that propagation, seafile
/// would hit a tailnet hostname and authelia would fall back to Caddy
/// local, which a tailnet device can't resolve.
///
/// Authelia's URL is otherwise resolved by the recursive `run()` call:
/// it goes through the same `--url` / `--tailscale` / Caddy-auto / prompt
/// flow as any service install, so all the URL logic lives in one place.
async fn ensure_dependencies(auth: bool, tailscale: bool, interactive: bool) -> Result<()> {
    let config = ryra_core::config::load_or_default(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;
    let needs_authelia =
        auth && !ryra_core::is_service_installed("authelia") && config.auth.is_none();

    if !needs_authelia {
        return Ok(());
    }

    if interactive {
        let confirm = Confirm::new()
            .with_prompt("Authelia (SSO provider) is not installed. Install it?")
            .default(true)
            .interact()?;
        if !confirm {
            bail!("authelia is required for --auth");
        }
    }

    println!("\nInstalling authelia...\n");
    Box::pin(run(
        AddRequest::dependency(WellKnownService::Authelia).tailscale(tailscale)
    ))
    .await?;

    Ok(())
}

/// Ensure auth is configured, possibly installing authelia inline.
/// Returns true if auth is ready, false if user cancelled.
/// Never called under --dry-run (the caller bails first): every branch
/// here either installs authelia or writes [auth] to preferences.toml.
async fn ensure_auth_for_add(
    config: &mut Config,
    paths: &ConfigPaths,
    parent_tailscale: bool,
) -> Result<bool> {
    match prompts::ensure_auth_configured(config, paths).await? {
        prompts::AuthSetupChoice::External(_) => Ok(true),
        prompts::AuthSetupChoice::InstallAuthelia => {
            // Check if authelia is already installed but auth wasn't configured
            if ryra_core::is_service_installed("authelia") {
                println!();
                println!("Authelia is already installed — configuring auth...");
                if ryra_core::authelia::configure_auth_from_installed(config, paths)? {
                    println!(
                        "  Auth configured. Saved to {}",
                        paths.config_file.display()
                    );
                    return Ok(true);
                }
                println!("Could not auto-configure auth from installed authelia.");
                return Ok(false);
            }

            println!("\nInstalling authelia...\n");
            // Inherit the parent service's exposure choice instead of asking
            // the user a second time (which let them pick mismatched setups,
            // e.g. local authelia + tailscale service → tailnet OIDC broken).
            // For Local: caddy is already installed by the parent's prompt,
            // so authelia's recursive URL resolution auto-derives `*.internal`.
            // For Tailscale: pass --tailscale through.
            // For Custom: fall through (custom URLs are per-service — authelia
            // gets its own exposure prompt).
            Box::pin(run(
                AddRequest::dependency(WellKnownService::Authelia).tailscale(parent_tailscale)
            ))
            .await?;
            // Reload config — authelia's finalize_add auto-configures [auth]
            *config = ryra_core::config::load_or_default(&paths.config_file)?;
            if config.auth.is_some() {
                println!();
                Ok(true)
            } else {
                println!("Auth was not configured after installing authelia.");
                Ok(false)
            }
        }
        prompts::AuthSetupChoice::Skip => {
            println!("Skipped auth setup.");
            Ok(false)
        }
    }
}

/// Determine which auth kind to use based on CLI flags and service capabilities.
///
/// The prompt's default tracks whether an auth provider is already configured
/// globally: first install → default no (don't push users into auth setup);
/// after Authelia is up → default yes (reuse it by habit).
fn resolve_auth_kind(
    auth_flag: bool,
    interactive: bool,
    supported: &[AuthKind],
    auth_configured: bool,
) -> Result<Option<AuthKind>> {
    // --auth flag: use the first supported auth kind (core validates further)
    if auth_flag {
        return Ok(supported.first().cloned());
    }

    // No auth support in service definition
    if supported.is_empty() || !interactive {
        return Ok(None);
    }

    // Single auth option: simple yes/no prompt
    if supported.len() == 1 {
        let kind = &supported[0];
        let enable = Confirm::new()
            .with_prompt(format!("Enable {kind} auth?"))
            .default(auth_configured)
            .interact()?;
        return Ok(if enable { Some(kind.clone()) } else { None });
    }

    // Multiple options: selection prompt
    let items: Vec<String> = std::iter::once("None".to_string())
        .chain(supported.iter().map(|k| k.to_string()))
        .collect();
    let selection = dialoguer::Select::new()
        .with_prompt("Auth mode")
        .items(&items)
        .default(if auth_configured { 1 } else { 0 })
        .interact()?;
    Ok(if selection == 0 {
        None
    } else {
        Some(supported[selection - 1].clone())
    })
}

/// Warn about services from untrusted (non-default) registries.
/// Shows scripts and volume mounts that will run on the host, requires y/n.
fn warn_untrusted_service(
    service_dir: &std::path::Path,
    service: &str,
    interactive: bool,
) -> Result<()> {
    let report = ryra_core::registry::trust_report(service_dir);

    println!();
    println!(
        "  {} {service} is from an external registry.",
        super::style::warning()
    );
    println!("  External services can run arbitrary code on your host.");
    if !report.quadlet_hooks.is_empty() {
        println!();
        println!("  Quadlet hooks (run as your user):");
        for s in &report.quadlet_hooks {
            println!("    {s}");
        }
    }
    if !report.config_scripts.is_empty() {
        println!();
        println!("  Scripts (copied to service data dir):");
        for s in &report.config_scripts {
            println!("    {s}");
        }
    }
    if !report.host_mounts.is_empty() {
        println!();
        println!("  Host bind mounts:");
        for v in &report.host_mounts {
            println!("    {v}");
        }
    }
    println!();

    if !interactive {
        bail!("{service} is from an external registry — use --yes to accept or run interactively");
    }

    let proceed = Confirm::new()
        .with_prompt("  Install this service?")
        .default(false)
        .interact()?;
    if !proceed {
        bail!("cancelled");
    }

    Ok(())
}
