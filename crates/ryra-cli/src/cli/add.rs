use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::caddy::AcmeMode;
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::Config;
use ryra_core::registry::resolve::ServiceRef;
use ryra_core::registry::service_def::{AuthKind, HttpsRequirement};
use ryra_core::{REGISTRY_BUNDLED, Warning, WellKnownService};

use super::apply;
use super::prompts;

/// Default port for Caddy's HTTPS listener.
const DEFAULT_CADDY_HTTPS_PORT: u16 = 8443;
/// Default port for Authelia's HTTP listener.
const DEFAULT_AUTHELIA_PORT: u16 = 9091;
/// Inbucket's internal SMTP container port.
const INBUCKET_SMTP_PORT: u16 = 2500;

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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    services: &[String],
    url: Option<&str>,
    auth: bool,
    smtp: Option<SmtpProvider>,
    enable: &[String],
    tailscale: bool,
    acme: Option<&AcmeMode>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if url.is_some() && services.len() > 1 {
        bail!("--url can only be used when adding a single service");
    }
    if !enable.is_empty() && services.len() > 1 {
        bail!("--enable can only be used when adding a single service");
    }
    // --acme drives caddy's TLS mode. It's accepted on any `ryra add`:
    // when adding caddy directly it sets the snippet; when adding a
    // service with `--url <public>` it auto-installs caddy in ACME mode
    // before the service is added (non-interactive equivalent of the
    // TLS prompt below).

    // When this run is the one installing caddy in ACME mode, offer to
    // lower `net.ipv4.ip_unprivileged_port_start` so Caddy binds 80/443
    // directly. The check has to run before add_service, because that's
    // where the caddy quadlet's `PublishPort=` is generated based on the
    // current sysctl value. Skipped silently if already enabled, or for
    // LAN/Tailscale paths where 8080/8443 is fine.
    if acme.is_some()
        && services
            .iter()
            .any(|s| WellKnownService::Caddy.matches(s))
    {
        super::sysctl_low_ports::offer_enable().await?;
    }

    let preflight_paths = ryra_core::config::ConfigPaths::resolve()?;
    let preflight_config = ryra_core::config::load_or_default(&preflight_paths.config_file)?;
    if let Err(e) = ryra_core::system::preflight::check(&preflight_config) {
        bail!("preflight check failed:\n\n{e}");
    }

    // --tailscale check fires here so a missing CLI / unlogged tailnet
    // surfaces before any service planning.
    if tailscale
        && let Err(e) = ryra_core::system::preflight::check_tailscale_runtime()
    {
        bail!("--tailscale flag passed but {e}");
    }

    let interactive = super::is_interactive();

    // Tailscale admin API token acquisition: when `--tailscale` is set
    // and we don't already have one cached from a previous install,
    // prompt the user to paste one (or read RYRA_TS_API_KEY in
    // non-interactive runs). Saved to `config.tailscale.admin_api_key`.
    // Done up-front so a missing token fails fast, before any image
    // pulls or service planning. The token is needed at install time
    // (define service via API) and removal time (delete service).
    if tailscale && !dry_run {
        ensure_tailscale_admin_token(interactive).await?;
    }

    // "First add" = no ryra config on disk yet. Latch the answer before any
    // side-effect creates the file — we use this at the end to decide between
    // offering to enable lingering (ceremonial, worth the interaction) and
    // just warning (quieter, for every subsequent add).
    let first_run = !ryra_core::config::ConfigPaths::resolve()?
        .config_file
        .exists();

    // Auto-install authelia for --auth
    if !dry_run {
        ensure_dependencies(auth, tailscale, interactive).await?;
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

    // Serialize concurrent `ryra add --auth` runs so two processes don't
    // clobber each other's client entries when editing authelia's
    // configuration.yml in-memory then writing it back. The lock is
    // released when _auth_lock drops at end of this function.
    let _auth_lock = if auth && !dry_run {
        paths.ensure_dirs()?;
        let lock_path = paths.config_dir.join(".authelia-oidc.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        file.lock()?;
        Some(file)
    } else {
        None
    };

    for service_input in services {
        let service_ref = ServiceRef::parse(service_input)?;
        let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
        let service = service_ref.service_name();

        // Load config once — previous iterations or ensure_dependencies may have
        // modified it on disk (e.g., installing caddy or authelia).
        let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

        // Orphan-data check: `ryra remove <svc>` (preserve mode) drops the
        // service from config but leaves named volumes and data dirs on disk.
        // A fresh `ryra add` would silently inherit them — surprising when
        // the user wants a clean state. Surface it here so they choose.
        if !dry_run
            && !config.services.iter().any(|s| s.name == service)
            && let Some(orphan) = ryra_core::data::enumerate_service(&config, service)?
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
        if service_ref.registry_name() != REGISTRY_BUNDLED && !yes && !dry_run {
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
                        config.smtp = Some(smtp);
                        paths.ensure_dirs()?;
                        ryra_core::config::save_config(&paths.config_file, &config)?;
                        println!(
                            "  SMTP configured. Saved to {}\n",
                            paths.config_file.display()
                        );
                        true
                    }
                    prompts::SmtpSetupChoice::Inbucket => {
                        let inbucket_installed = config
                            .services
                            .iter()
                            .any(|s| WellKnownService::Inbucket.matches(&s.name));
                        if !inbucket_installed {
                            println!("\nInstalling inbucket...\n");
                            Box::pin(run(
                                &[WellKnownService::Inbucket.to_string()],
                                None,
                                false,
                                None,
                                &[],
                                false,
                                None,
                                false,
                                true,
                            ))
                            .await?;
                            // Reload — inbucket install modified config on disk
                            config = ryra_core::config::load_or_default(&paths.config_file)?;
                        }
                        // Use container name for SMTP host — services on the
                        // caddy network can reach inbucket directly. The host
                        // port isn't reachable from --no-hosts containers.
                        config.smtp = Some(ryra_core::config::schema::SmtpCredentials {
                            host: "inbucket".to_string(),
                            port: INBUCKET_SMTP_PORT, // inbucket's internal container port
                            username: String::new(),
                            password: String::new(),
                            from: "noreply@example.com".to_string(),
                            security: ryra_core::config::schema::SmtpSecurity::Off,
                        });
                        paths.ensure_dirs()?;
                        ryra_core::config::save_config(&paths.config_file, &config)?;
                        println!(
                            "  SMTP configured (inbucket). Saved to {}\n",
                            paths.config_file.display()
                        );
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

        // Resolve exposure BEFORE auto-installing authelia so the provider
        // inherits the parent's choice. Picking the two separately let users
        // mismatch (authelia local + service tailnet → tailnet clients can't
        // reach `authelia.internal` for OIDC redirects). One prompt, one
        // decision, propagated.
        let needs_https = needs_https(
            reg_service.def.service.https.clone(),
            auth_kind.is_some(),
            url,
        );

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
        //      External) when needs_https and Caddy isn't installed
        //   5. Loopback — service runs on plain http://127.0.0.1:<port>
        // Clap's `conflicts_with = "url"` on --tailscale means 1+2 don't collide.
        let exposure: ryra_core::Exposure = if let Some(u) = url {
            ryra_core::Exposure::from_url(u)
        } else if tailscale {
            let ts_url = derive_tailscale_url(service)?;
            println!("→ Using {ts_url} (Tailscale)");
            ryra_core::Exposure::Tailscale { url: ts_url }
        } else if needs_https {
            let caddy_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
            if caddy_installed {
                let caddy_https_port = config
                    .services
                    .iter()
                    .find(|s| WellKnownService::Caddy.matches(&s.name))
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
                let chosen = prompt_exposure_for(service, &config, will_install_authelia).await?;
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
        } else {
            ryra_core::Exposure::Loopback
        };
        // Derive locals for downstream code that still threads the legacy
        // shape (env templating, OIDC client registration). Goes away as
        // each call site migrates to take `&Exposure` directly.
        let url: Option<&str> = exposure.url();
        let tailscale_enabled: bool = exposure.is_tailscale();

        // Auto-install Caddy when the user gives a public URL but Caddy
        // isn't installed yet. Without this, the install would succeed
        // but the URL wouldn't actually route anywhere — the user would
        // have to know to add Caddy first, which the previous flow
        // forced and most people forget.
        let caddy_already_installed = config
            .services
            .iter()
            .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
        let need_caddy_for_public_url = url
            .is_some_and(ryra_core::is_public_url)
            && !caddy_already_installed
            && !WellKnownService::Caddy.matches(service)
            && !tailscale_enabled
            && !dry_run;
        if need_caddy_for_public_url {
            let chosen = match acme {
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
                    Box::pin(run(
                        &[WellKnownService::Caddy.to_string()],
                        None,
                        false,
                        None,
                        &[],
                        false,
                        Some(&mode),
                        false,
                        true,
                    ))
                    .await?;
                    config = ryra_core::config::load_or_default(&paths.config_file)?;
                }
                Some(TlsHandling::SelfSigned) => {
                    println!("\nInstalling caddy (self-signed LAN mode)...\n");
                    Box::pin(run(
                        &[WellKnownService::Caddy.to_string()],
                        None,
                        false,
                        None,
                        &[],
                        false,
                        None,
                        false,
                        true,
                    ))
                    .await?;
                    config = ryra_core::config::load_or_default(&paths.config_file)?;
                }
                Some(TlsHandling::External) | None => {
                    // Skip Caddy install — user is fronting with their own
                    // reverse proxy (Cloudflare Tunnel, nginx, etc.). The
                    // existing `UrlWithoutReverseProxy` warning still fires
                    // from add_service so the user knows routing is on them.
                }
            }
        } else if acme.is_some() && caddy_already_installed && !WellKnownService::Caddy.matches(service) {
            // --acme passed but Caddy is already installed — the snippet
            // is set; flipping mode means editing tls.caddy directly.
            // Warn but don't bail; let the install proceed.
            eprintln!(
                "\nNote: --acme is ignored — caddy is already installed.\n  \
                 Edit ~/.local/share/ryra/caddy/config/tls.caddy to switch TLS mode.\n"
            );
        }

        // Authelia already exists — make sure its exposure isn't narrower
        // than the service we're about to add. Local-only authelia + tailnet
        // service silently breaks OIDC redirects for off-host clients.
        if auth_kind.is_some() && config.auth.is_some() {
            check_auth_exposure_compat(&config, service, url)?;
        }

        // If the user chose auth but no provider is configured, install one,
        // passing `tailscale_enabled` so authelia inherits the parent's
        // exposure (Local → caddy already up from the prompt → auto-derives
        // `*.internal`; Tailscale → propagates --tailscale; Custom → falls
        // through to authelia's own exposure resolution since custom URLs
        // are per-service and can't be inherited).
        if will_install_authelia {
            if !ensure_auth_for_add(&mut config, &paths, dry_run, tailscale_enabled).await? {
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
        for g in enable {
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

        let has_promptable_top = reg_service
            .def
            .env
            .iter()
            .any(|e| matches!(e.kind, EnvKind::Prompted | EnvKind::Required));
        let has_groups = !reg_service.def.env_groups.is_empty();

        if (has_promptable_top || has_groups) && interactive {
            // Resolve template variables in defaults so prompts show real values.
            // This context is reused by add_service so the secrets the user saw
            // during prompts match what gets written to .env.
            let default_ctx = ryra_core::generate::context::build_context(
                &config,
                &reg_service.def,
                None,
                auth_kind.as_ref(),
                url,
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
            if !missing_required.is_empty() {
                bail!(
                    "required env vars not provided (run interactively or set via env): {}",
                    missing_required.join(", ")
                );
            }
        }

        // --acme only takes effect on first caddy install — after that
        // tls.caddy is user-managed. Filter so it only flows when adding
        // caddy (the top-level guard already rejects other combos).
        let acme_for_service = if WellKnownService::Caddy.matches(service) {
            acme
        } else {
            None
        };

        // If a previous add failed partway, clean up before retrying.
        let result = match ryra_core::add_service(
            service,
            &exposure,
            auth_kind.clone(),
            auth || auth_kind.is_some(),
            enable_smtp,
            &env_overrides,
            &enabled_groups,
            service_ref.registry_name(),
            &repo_dir,
            prompt_ctx.clone(),
            &super::is_port_in_use,
            acme_for_service,
        ) {
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

                // `remove_service` requires a config entry, so only use it
                // for the partial-install case. For the orphan case the
                // config entry is gone; fall back to `orphan_purge_steps`.
                let cleanup_cfg = ryra_core::config::load_or_default(&paths.config_file)?;
                if cleanup_cfg.services.iter().any(|s| s.name == service) {
                    let remove_result =
                        ryra_core::remove_service(service, ryra_core::RemoveMode::Purge)?;
                    apply::execute_all(&remove_result.steps).await?;
                    ryra_core::finalize_remove(service)?;
                } else {
                    let svc_data = ryra_core::data::enumerate_service(&cleanup_cfg, service)?
                        .ok_or_else(|| {
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
                ryra_core::add_service(
                    service,
                    &exposure,
                    auth_kind.clone(),
                    auth || auth_kind.is_some(),
                    enable_smtp,
                    &env_overrides,
                    &enabled_groups,
                    service_ref.registry_name(),
                    &repo_dir,
                    prompt_ctx.clone(),
                    &super::is_port_in_use,
                    acme_for_service,
                )?
            }
            other => other?,
        };

        // (--tailscale: the per-service sidecar quadlet — `ts-<service>` —
        // gets generated by add_service when `tailscale_enabled` is set.
        // No host-side `tailscale serve` step is needed; each service has
        // its own tailscaled now.)

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
                            "  WARNING: {port_name} port {original_port} → {assigned_port} ({reason})"
                        );
                    }
                }
                Warning::UrlWithoutReverseProxy {
                    service_name,
                    url,
                    host_port,
                } => {
                    println!(
                        "  NOTE: --url was set for {service_name} but no bundled reverse proxy \
                         (Caddy) is installed. Ryra will template {url} into the service but \
                         won't configure routing — point your own reverse proxy (nginx, \
                         Cloudflare Tunnel, Tailscale Funnel, etc.) at 127.0.0.1:{host_port}, \
                         or run `ryra add caddy` to let ryra handle it."
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
                            "  WARNING: {service_name} requires at least {min_mb} MB RAM, \
                         but this system has {available_mb} MB — service may fail to start"
                        );
                    }
                    Warning::RamBelowRecommended {
                        service_name,
                        recommended_mb,
                        available_mb,
                    } => {
                        println!(
                            "  NOTE: {service_name} recommends {recommended_mb} MB RAM, \
                         but this system has {available_mb} MB — performance may be degraded"
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
            ryra_core::record_pending(ryra_core::RecordPendingParams {
                service_name: service,
                auth_kind,
                registry_name: service_ref.registry_name(),
                allocated_ports: &result.allocated_ports,
                repo_dir: &repo_dir,
                exposure: &exposure,
            })?;

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
                eprintln!("\nError during setup: {e}");
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

            ryra_core::mark_installed(service)?;

            // Trust Caddy's self-signed CA, and register the service's
            // hostname in /etc/hosts for browser access. Only fires for
            // *.internal URLs (Caddy local) — Tailscale's MagicDNS handles
            // *.ts.net for free, and External hostnames are the user's
            // DNS to manage.
            if let Some(service_url) = result.url.as_deref()
                && service_url_is_caddy_local(service_url)
            {
                setup_host_access(&[service_url]);
            }

            let home_dir = ryra_core::service_home(service)?;
            if let Some(ref url) = result.url {
                println!("\n{service} is running at {url}");
            } else {
                println!("\n{service} is running.");
            }

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

            let env_path = home_dir.join(".env");
            println!();
            println!("Commands:");
            println!("  cat {}  # view config", env_path.display());
            println!("  systemctl --user restart {service}  # restart (picks up .env changes)");
            println!("  systemctl --user status {service}  # check if running");
            println!("  journalctl --user-unit {service}.service -f  # follow logs");

            // Caddy-only: tell the user which TLS mode they got and where
            // to switch to a different one. The snippet path is the only
            // thing they need to know to swap in Cloudflare DNS-01,
            // wildcards, BYO certs, plain HTTP for Tunnel, etc.
            if WellKnownService::Caddy.matches(service) {
                let snippet_pathbuf = ryra_core::caddy::tls_snippet_path().ok();
                let snippet_path = snippet_pathbuf
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.local/share/ryra/caddy/config/tls.caddy".to_string());
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
                }
                // If the snippet on disk doesn't match a ryra-written
                // shape at all, say so explicitly so the user isn't misled
                // into thinking ryra is managing it.
                if detected_mode.is_none() && acme_for_service.is_none() {
                    println!(
                        "  (note: tls.caddy looks user-customized — leaving it untouched)"
                    );
                }
                if matches!(displayed_mode, AcmeMode::WithEmail(_) | AcmeMode::Anonymous) {
                    let (http_port, https_port) = result
                        .allocated_ports
                        .iter()
                        .fold((8080u16, 8443u16), |(h, hs), (n, p)| match n.as_str() {
                            "http" => (*p, hs),
                            "https" => (h, *p),
                            _ => (h, hs),
                        });
                    println!("  For LE to issue certs Caddy must be reachable from the internet:");
                    println!("    - DNS A/AAAA for each --url host must point at this machine");
                    if http_port == 80 && https_port == 443 {
                        println!(
                            "    - Caddy listens on host 80/443; forward router 80→80 and 443→443"
                        );
                        println!(
                            "    - Firewall must allow 80/443 (ufw / firewalld / nft varies)"
                        );
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
                println!(
                    "  For Cloudflare DNS-01, wildcards, or BYO certs: edit {snippet_path}"
                );
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
/// `*.internal`, which unlike `*.localhost` does not auto-resolve).

/// the system trust bundle for curl/wget/Firefox-on-p11-kit users.
///
/// The rootless work covers Chromium-family browsers (via the user's
/// `~/.pki/nssdb`) and every Firefox profile with a `cert9.db`. That's the
/// mkcert pattern and enough for ~95% of browser traffic on Linux.
fn setup_host_access(domains: &[&str]) {
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
        .filter_map(|d| url::Url::parse(d).ok().and_then(|u| u.host_str().map(String::from)))
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
        let line = format!("127.0.0.1 {}", missing_hosts.join(" "));
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
            println!("  Added {} to /etc/hosts (via sudo).", missing_hosts.join(", "));
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
            eprintln!(
                "        Run:  echo '{line}' | sudo tee -a /etc/hosts"
            );
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
/// or reading from `RYRA_TS_API_KEY` (non-interactive) if not.
///
/// Persists to ryra.toml so the user pastes their token once and every
/// subsequent `--tailscale` install + remove reuses it for service
/// definition + ACL setup via the admin API.
async fn ensure_tailscale_admin_token(interactive: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.tailscale.is_some() {
        return Ok(()); // already cached
    }

    let admin_api_key = if interactive {
        prompt_tailscale_admin_token()?
    } else {
        std::env::var("RYRA_TS_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "--tailscale needs a Tailscale admin API token. Set RYRA_TS_API_KEY \
                 (tskey-api-…) or run interactively to be prompted.\n\
                 Generate one at https://login.tailscale.com/admin/settings/keys \
                 (use the \"API access token\" type, not an auth key)"
            )
        })?
    };

    config.tailscale = Some(ryra_core::config::schema::TailscaleConfig {
        admin_api_key,
        tailnet: None,
    });
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("  ✓ Tailscale admin token saved to {}", paths.config_file.display());
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

/// Build the `https://<service>.<tailnet>/` URL for a service installed
/// with `--tailscale`. Each service gets its own tailnet node (via a
/// sidecar tailscaled), so the hostname is `<service>` and the suffix
/// is the local node's tailnet (e.g. `cobbler-tuna.ts.net`).
///
/// No port — `tailscale serve --https=443` from the sidecar runs at
/// the standard HTTPS port, and putting `:443` in the URL trips up
/// OIDC libraries that string-compare issuer URLs.
fn derive_tailscale_url(service: &str) -> Result<String> {
    let node = ryra_core::system::tailscale::self_dns_name().ok_or_else(|| {
        anyhow::anyhow!("--tailscale: no logged-in tailnet (preflight should have caught this)")
    })?;
    let tailnet = ryra_core::system::tailscale::tailnet_suffix(&node).ok_or_else(|| {
        anyhow::anyhow!(
            "--tailscale: couldn't extract tailnet from MagicDNS name '{node}' \
             (expected three-label `<host>.<tailnet>.ts.net`)"
        )
    })?;
    Ok(format!("https://{service}.{tailnet}"))
}

/// True when a service URL targets Caddy's local-CA `*.internal` domain.
/// Used to gate `/etc/hosts` writes and CA trust setup — Tailscale and
/// External URLs handle DNS / trust through other paths.
fn service_url_is_caddy_local(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| {
            h.ends_with(&format!(".{}", ryra_core::config::schema::CADDY_LOCAL_DOMAIN))
        })
}

/// Bail when authelia is exposed locally (`*.internal`) but the service we're
/// about to add will be reachable somewhere broader (tailnet, custom URL).
/// In that combination, off-host clients (a phone on the tailnet, a public
/// browser) hit the service fine but can't follow the OIDC redirect to
/// `authelia.internal` because that hostname only resolves on the ryra host.
///
/// The reverse — authelia broader than the service — is fine: the local
/// browser reaches both, and `*.ts.net` resolves on the host via MagicDNS.
fn check_auth_exposure_compat(
    config: &Config,
    service: &str,
    service_url: Option<&str>,
) -> Result<()> {
    let Some(auth) = &config.auth else {
        return Ok(());
    };
    let auth_url = auth.url();
    let auth_is_local = service_url_is_caddy_local(auth_url);
    if !auth_is_local {
        return Ok(());
    }
    let Some(svc_url) = service_url else {
        return Ok(());
    };
    if service_url_is_caddy_local(svc_url) {
        return Ok(());
    }
    bail!(
        "authelia is local-only at {auth_url}, but {service} will be reachable at \
         {svc_url}. Off-host clients (e.g., other devices on your tailnet) can't \
         resolve `*.internal` hostnames, so the OIDC redirect from {service} back \
         to authelia would fail.\n\n\
         Fix: re-install authelia at the same exposure as {service}:\n  \
         ryra remove authelia --purge\n  \
         ryra add authelia --tailscale  (or --url <public-https-url>)"
    );
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

/// Convert an interactive email input into the right [`AcmeMode`] —
/// empty string (user hit Enter) means anonymous LE, anything else
/// means LE with that email for renewal notices.
fn acme_mode_from_email(email: String) -> AcmeMode {
    if email.is_empty() {
        AcmeMode::Anonymous
    } else {
        AcmeMode::WithEmail(email)
    }
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
                .with_prompt("Email for Let's Encrypt (optional — for renewal notices, press Enter to skip)")
                .allow_empty(true)
                .interact_text()?;
            Ok(TlsHandling::LetsEncrypt(acme_mode_from_email(email)))
        }
        1 => Ok(TlsHandling::SelfSigned),
        _ => Ok(TlsHandling::External),
    }
}

/// Interactive prompt: how should this service be reachable? Returns
/// the typed [`Exposure`] decision after applying any side-effects the
/// choice implies (installing Caddy, running tailscale preflight, etc.).
///
/// Called from the per-service URL resolver when needs_https is true and
/// neither `--url` nor `--tailscale` was given.
async fn prompt_exposure_for(
    service: &str,
    config: &Config,
    auth_will_inherit: bool,
) -> Result<ryra_core::Exposure> {
    let items = &[
        "Self-signed (LAN) — Caddy local CA at *.internal (browsers warn unless trusted)",
        "Tailscale — exposed on your tailnet (publicly-trusted cert)",
        "Public + Let's Encrypt — Caddy issues real certs (DNS + Caddy reachable from the internet)",
        "External — I have my own reverse proxy (Cloudflare Tunnel, nginx, etc.)",
    ];
    if auth_will_inherit {
        println!(
            "(authelia will inherit this choice — install it separately first if you need a different exposure)"
        );
    }
    let selection = dialoguer::Select::new()
        .with_prompt(format!("How will '{service}' be reachable?"))
        .items(items)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            // Self-signed (LAN): install Caddy in default mode if it isn't
            // already there, then derive the *.internal URL using its
            // allocated HTTPS port. Match the planner's `installed`-aware
            // check — a stale `installed = false` entry from a killed
            // previous install must not count as ready.
            let caddy_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
            if !caddy_installed {
                println!("\nInstalling caddy (self-signed LAN mode)...\n");
                Box::pin(run(
                    &[WellKnownService::Caddy.to_string()],
                    None,
                    false,
                    None,
                    &[],
                    false,
                    None,
                    false,
                    true,
                ))
                .await?;
            }
            let config = ryra_core::config::load_or_default(
                &ryra_core::config::ConfigPaths::resolve()?.config_file,
            )?;
            let caddy_https_port = config
                .services
                .iter()
                .find(|s| WellKnownService::Caddy.matches(&s.name))
                .and_then(|s| s.ports.get("https").copied())
                .unwrap_or(DEFAULT_CADDY_HTTPS_PORT);
            Ok(ryra_core::Exposure::Internal {
                url: format!(
                    "https://{service}.{}:{caddy_https_port}",
                    ryra_core::config::schema::CADDY_LOCAL_DOMAIN
                ),
            })
        }
        1 => {
            // Tailscale: same path as `--tailscale` flag — preflight,
            // ensure auth key, derive `https://<service>.<tailnet>/`.
            // Failures bail rather than save partial state.
            if let Err(e) = ryra_core::system::preflight::check_tailscale_runtime() {
                bail!("Tailscale not ready:\n\n{e}");
            }
            ensure_tailscale_admin_token(true).await?;
            Ok(ryra_core::Exposure::Tailscale {
                url: derive_tailscale_url(service)?,
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
            let caddy_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Caddy.matches(&s.name) && s.installed);
            if !caddy_installed {
                let email: String = Input::new()
                    .with_prompt("Email for Let's Encrypt (optional — for renewal notices, press Enter to skip)")
                    .allow_empty(true)
                    .interact_text()?;
                dns_preflight_for_acme(&url, true).await?;
                let mode = acme_mode_from_email(email);
                println!("\nInstalling caddy (Let's Encrypt mode)...\n");
                Box::pin(run(
                    &[WellKnownService::Caddy.to_string()],
                    None,
                    false,
                    None,
                    &[],
                    false,
                    Some(&mode),
                    false,
                    true,
                ))
                .await?;
            } else {
                eprintln!(
                    "  Note: caddy is already installed — using its existing TLS mode.\n  \
                     Edit ~/.local/share/ryra/caddy/config/tls.caddy to switch to Let's Encrypt."
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

/// Auto-install inbucket and point `config.smtp` at it for `--smtp=inbucket`.
/// Idempotent: does nothing if `config.smtp` is already set.
async fn ensure_smtp_for_add(provider: SmtpProvider) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

    if config.smtp.is_some() {
        // Already configured — whether by a previous --smtp, prompt, or a
        // hand-edited ryra.toml. Don't clobber it.
        return Ok(());
    }

    match provider {
        SmtpProvider::Inbucket => {
            let inbucket_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Inbucket.matches(&s.name));
            if !inbucket_installed {
                println!("\nInstalling inbucket...\n");
                Box::pin(run(
                    &[WellKnownService::Inbucket.to_string()],
                    None,
                    false,
                    None,
                    &[],
                    false,
                    None,
                    false,
                    true,
                ))
                .await?;
                // Reload — the inner run() mutated config on disk.
                config = ryra_core::config::load_or_default(&paths.config_file)?;
            }
            // Target inbucket by container name — services on the same
            // podman network resolve it via DNS. `:2500` is inbucket's
            // internal SMTP port; the host-side PublishPort isn't used.
            config.smtp = Some(ryra_core::config::schema::SmtpCredentials {
                host: "inbucket".to_string(),
                port: INBUCKET_SMTP_PORT,
                username: String::new(),
                password: String::new(),
                from: "noreply@example.com".to_string(),
                security: ryra_core::config::schema::SmtpSecurity::Off,
            });
            paths.ensure_dirs()?;
            ryra_core::config::save_config(&paths.config_file, &config)?;
            println!(
                "  SMTP configured (inbucket). Saved to {}\n",
                paths.config_file.display()
            );
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
    let needs_authelia = auth
        && !config
            .services
            .iter()
            .any(|s| WellKnownService::Authelia.matches(&s.name))
        && config.auth.is_none();

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
        &[WellKnownService::Authelia.to_string()],
        None,
        false,
        None,
        &[],
        tailscale,
        None,
        false,
        true,
    ))
    .await?;

    Ok(())
}

/// Ensure auth is configured, possibly installing authelia inline.
/// Returns true if auth is ready, false if user cancelled.
async fn ensure_auth_for_add(
    config: &mut Config,
    paths: &ConfigPaths,
    dry_run: bool,
    parent_tailscale: bool,
) -> Result<bool> {
    match prompts::ensure_auth_configured(config, paths).await? {
        prompts::AuthSetupChoice::External(_) => Ok(true),
        prompts::AuthSetupChoice::InstallAuthelia => {
            // Check if authelia is already installed but auth wasn't configured
            let authelia_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Authelia.matches(&s.name));
            if authelia_installed {
                println!();
                println!("Authelia is already installed — configuring auth...");
                if try_configure_auth_from_installed(config, paths)? {
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
                &[WellKnownService::Authelia.to_string()],
                None,
                false,
                None,
                &[],
                parent_tailscale,
                None,
                dry_run,
                true,
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

/// Warn about services from untrusted (non-bundled) registries.
/// Shows scripts and volume mounts that will run on the host, requires y/n.
fn warn_untrusted_service(
    service_dir: &std::path::Path,
    service: &str,
    interactive: bool,
) -> Result<()> {
    // Collect scripts (ExecStartPre/Post in quadlets)
    let quadlet_dir = service_dir.join("quadlets");
    let mut scripts: Vec<String> = Vec::new();
    let mut volumes: Vec<String> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&quadlet_dir) {
        for entry in entries.flatten() {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("ExecStartPre=") || trimmed.starts_with("ExecStartPost=")
                    {
                        scripts.push(trimmed.to_string());
                    }
                    if trimmed.starts_with("Volume=") {
                        let vol = trimmed.strip_prefix("Volume=").unwrap_or(trimmed);
                        // Only flag host bind mounts (contain %h or start with /)
                        if vol.contains("%h") || vol.starts_with('/') {
                            volumes.push(vol.to_string());
                        }
                    }
                }
            }
        }
    }

    // Collect config scripts
    let scripts_dir = service_dir.join("configs").join("scripts");
    let mut config_scripts: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&scripts_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                config_scripts.push(name.to_string());
            }
        }
    }

    println!();
    println!("  WARNING: {service} is from an external registry.");
    println!("  External services can run arbitrary code on your host.");
    if !scripts.is_empty() {
        println!();
        println!("  Quadlet hooks (run as your user):");
        for s in &scripts {
            println!("    {s}");
        }
    }
    if !config_scripts.is_empty() {
        println!();
        println!("  Scripts (copied to service data dir):");
        for s in &config_scripts {
            println!("    {s}");
        }
    }
    if !volumes.is_empty() {
        println!();
        println!("  Host bind mounts:");
        for v in &volumes {
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

/// Try to configure auth from an already-installed authelia instance.
/// The .env is user-readable under ~/.local/share/ryra/authelia/.env.
fn try_configure_auth_from_installed(config: &mut Config, paths: &ConfigPaths) -> Result<bool> {
    let env_path = ryra_core::service_home(WellKnownService::Authelia.as_str())?.join(".env");
    let env_content = match std::fs::read_to_string(&env_path) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };

    // Find the port from the installed service record
    let service = config
        .services
        .iter()
        .find(|s| WellKnownService::Authelia.matches(&s.name));
    let port = service
        .and_then(|s| s.ports.values().next().copied())
        .unwrap_or(DEFAULT_AUTHELIA_PORT);

    // Verify the .env file looks valid (has at least a port reference)
    if env_content.is_empty() {
        return Ok(false);
    }

    let url = format!("http://localhost:{port}");

    config.auth = Some(ryra_core::config::schema::AuthCredentials::Authelia { url, port });
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, config)?;
    println!(
        "  Auth configured. Saved to {}",
        paths.config_file.display()
    );
    Ok(true)
}

/// Decide whether this `ryra add` invocation must be promoted to HTTPS.
///
/// HTTPS is required when any of these hold:
///   1. The service declares `https = "always"` (e.g. authelia, vaultwarden).
///   2. The service declares `https = "auth"` AND the user chose OIDC auth
///      (via `--auth` or the interactive prompt). This is for services whose
///      OIDC stack refuses plain HTTP even on loopback (e.g. Nextcloud's
///      user_oidc won't render the SSO button over HTTP). Most OIDC-capable
///      services don't need this — RFC 8252 permits HTTP loopback callbacks.
///   3. The user passed an `https://…` URL explicitly.
fn needs_https(
    https_requirement: HttpsRequirement,
    auth_requested: bool,
    url: Option<&str>,
) -> bool {
    matches!(https_requirement, HttpsRequirement::Always)
        || (matches!(https_requirement, HttpsRequirement::Auth) && auth_requested)
        || url.is_some_and(|u| u.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_service_stays_http() {
        assert!(!needs_https(HttpsRequirement::Never, false, None));
        // Even with --auth, a service that didn't opt into HTTPS stays HTTP.
        // This is the RFC 8252 loopback case: http://127.0.0.1 is a valid
        // OIDC redirect_uri and most services (forgejo, grafana, etc.) work
        // fine that way.
        assert!(!needs_https(HttpsRequirement::Never, true, None));
        // Explicit http:// URL also stays HTTP.
        assert!(!needs_https(
            HttpsRequirement::Never,
            true,
            Some("http://foo.example.com"),
        ));
    }

    #[test]
    fn always_service_always_promotes() {
        assert!(needs_https(HttpsRequirement::Always, false, None));
        assert!(needs_https(
            HttpsRequirement::Always,
            false,
            Some("http://foo.example.com"),
        ));
    }

    #[test]
    fn auth_service_promotes_only_with_auth() {
        // The regression this guards: `ryra add nextcloud --auth` without
        // --url used to quietly install over HTTP and the SSO button never
        // rendered (user_oidc refuses to show it without HTTPS).
        assert!(needs_https(HttpsRequirement::Auth, true, None));
        // Without --auth, even an `https = "auth"` service stays HTTP.
        assert!(!needs_https(HttpsRequirement::Auth, false, None));
    }

    #[test]
    fn explicit_https_url_promotes() {
        assert!(needs_https(
            HttpsRequirement::Never,
            false,
            Some("https://foo.example.com"),
        ));
    }
}
