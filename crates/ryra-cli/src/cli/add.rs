use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{Config, TlsConfig};
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

pub async fn run(
    services: &[String],
    url: Option<&str>,
    auth: bool,
    smtp: Option<SmtpProvider>,
    enable: &[String],
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if url.is_some() && services.len() > 1 {
        bail!("--url can only be used when adding a single service");
    }
    if !enable.is_empty() && services.len() > 1 {
        bail!("--enable can only be used when adding a single service");
    }

    let interactive = super::is_interactive();

    // "First add" = no ryra config on disk yet. Latch the answer before any
    // side-effect creates the file — we use this at the end to decide between
    // offering to enable lingering (ceremonial, worth the interaction) and
    // just warning (quieter, for every subsequent add).
    let first_run = !ryra_core::config::ConfigPaths::resolve()?
        .config_file
        .exists();

    // Auto-install authelia for --auth
    if !dry_run {
        ensure_dependencies(auth, interactive).await?;
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

        // TLS — prompt if service requires HTTPS or user passed an https:// URL.
        // When Caddy is the TLS provider and no --url was given, auto-generate
        // a .localhost domain so HTTPS works out of the box.
        let needs_https = reg_service.def.service.https == HttpsRequirement::Required
            || url.is_some_and(|u| u.starts_with("https://"));
        if needs_https && !dry_run {
            ensure_tls_configured(&config, &paths, interactive).await?;
            // Reload — ensure_tls_configured may have installed caddy
            config = ryra_core::config::load_or_default(&paths.config_file)?;
        }
        let auto_url: Option<String>;
        let url: Option<&str> = if needs_https && url.is_none() {
            if matches!(config.tls, Some(TlsConfig::Caddy)) {
                let caddy_https_port = config
                    .services
                    .iter()
                    .find(|s| WellKnownService::Caddy.matches(&s.name))
                    .and_then(|s| s.ports.get("https").copied())
                    .unwrap_or(DEFAULT_CADDY_HTTPS_PORT);
                auto_url = Some(format!("https://{service}.localhost:{caddy_https_port}"));
                auto_url.as_deref()
            } else {
                url
            }
        } else {
            url
        };

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

        // If the user chose auth but no provider is configured, install one
        if auth_kind.is_some() && config.auth.is_none() {
            if !ensure_auth_for_add(&mut config, &paths, dry_run).await? {
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

        // If a previous add failed partway, clean up before retrying.
        let result = match ryra_core::add_service(
            service,
            url,
            auth_kind.clone(),
            auth || auth_kind.is_some(),
            enable_smtp,
            &env_overrides,
            &enabled_groups,
            service_ref.registry_name(),
            &repo_dir,
            prompt_ctx.clone(),
            &super::is_port_in_use,
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
                // Retry now that the partial state is gone
                ryra_core::add_service(
                    service,
                    url,
                    auth_kind.clone(),
                    auth || auth_kind.is_some(),
                    enable_smtp,
                    &env_overrides,
                    &enabled_groups,
                    service_ref.registry_name(),
                    &repo_dir,
                    prompt_ctx.clone(),
                    &super::is_port_in_use,
                )?
            }
            other => other?,
        };

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
                url: result.url.as_deref(),
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

            // Trust Caddy's self-signed CA if this service uses HTTPS via Caddy
            if result.url.is_some() {
                let config = ryra_core::config::load_or_default(&paths.config_file)?;
                if matches!(config.tls, Some(TlsConfig::Caddy)) {
                    setup_host_access(&[]);
                }
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
/// print (but never run) hints for the stores that need sudo — /etc/hosts
/// for non-.localhost domains, and the system trust bundle for
/// curl/wget/Firefox-on-p11-kit users.
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

    // --- Sudo hints (printed only; ryra never runs them) ---
    let hosts_content = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
    let missing_hosts: Vec<&str> = domains
        .iter()
        .filter(|d| {
            !hosts_content.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.split_whitespace().any(|w| w == **d)
            })
        })
        .copied()
        .collect();

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

/// Ensure TLS is configured when a service needs HTTPS.
/// Prompts the user to choose a TLS provider if not already configured.
async fn ensure_tls_configured(
    config: &Config,
    paths: &ConfigPaths,
    interactive: bool,
) -> Result<()> {
    // Already configured
    if let Some(ref tls) = config.tls {
        match tls {
            TlsConfig::Caddy => {
                // Ensure Caddy is installed (may have been removed or config edited manually)
                let caddy_installed = config
                    .services
                    .iter()
                    .any(|s| WellKnownService::Caddy.matches(&s.name));
                if !caddy_installed {
                    println!("\nInstalling caddy (TLS provider)...\n");
                    Box::pin(run(
                        &[WellKnownService::Caddy.to_string()],
                        None,
                        false,
                        None,
                        &[],
                        false,
                        true,
                    ))
                    .await?;
                }
            }
            TlsConfig::None => {
                println!(
                    "  NOTE: This service requires HTTPS — make sure it's configured externally."
                );
            }
            TlsConfig::Custom { .. } => {}
        }
        return Ok(());
    }

    // Not configured yet. In non-interactive mode, auto-configure TLS via Caddy
    // if Caddy is already installed — the user has implicitly opted in by
    // installing Caddy first. Otherwise bail with a helpful message.
    if !interactive {
        let caddy_installed = config
            .services
            .iter()
            .any(|s| WellKnownService::Caddy.matches(&s.name));
        if caddy_installed {
            let mut cfg = config.clone();
            cfg.tls = Some(TlsConfig::Caddy);
            paths.ensure_dirs()?;
            ryra_core::config::save_config(&paths.config_file, &cfg)?;
            println!("  TLS auto-configured (provider: caddy)");
            return Ok(());
        }
        bail!(
            "this service requires HTTPS — configure [tls] in ryra.toml first\n\
             Example:\n  [tls]\n  provider = \"caddy\""
        );
    }

    println!("\nThis service requires HTTPS.\n");
    let items = &[
        "Caddy (automatic HTTPS — recommended)",
        "Custom certificates (provide cert/key paths)",
        "None (I'll handle TLS myself)",
    ];
    let selection = dialoguer::Select::new()
        .with_prompt("How would you like to handle TLS?")
        .items(items)
        .default(0)
        .interact()?;

    let tls = match selection {
        0 => {
            // Ensure Caddy is installed
            let caddy_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Caddy.matches(&s.name));
            if !caddy_installed {
                println!("\nInstalling caddy...\n");
                Box::pin(run(
                    &[WellKnownService::Caddy.to_string()],
                    None,
                    false,
                    None,
                    &[],
                    false,
                    true,
                ))
                .await?;
            }
            TlsConfig::Caddy
        }
        1 => {
            let cert: String = Input::new()
                .with_prompt("  Path to certificate (fullchain.pem)")
                .interact_text()?;
            let key: String = Input::new()
                .with_prompt("  Path to private key (privkey.pem)")
                .interact_text()?;
            let cert_path = std::path::PathBuf::from(&cert);
            let key_path = std::path::PathBuf::from(&key);
            if !cert_path.exists() {
                bail!("certificate file not found: {cert}");
            }
            if !key_path.exists() {
                bail!("private key file not found: {key}");
            }
            TlsConfig::Custom {
                cert: cert_path,
                key: key_path,
            }
        }
        _ => {
            println!("  Make sure HTTPS is configured externally before using this service.");
            TlsConfig::None
        }
    };

    // Reload config from disk — Caddy install may have updated it
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    config.tls = Some(tls);
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!(
        "  TLS configured. Saved to {}\n",
        paths.config_file.display()
    );

    Ok(())
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

/// Auto-install authelia when --auth requires it.
async fn ensure_dependencies(auth: bool, interactive: bool) -> Result<()> {
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

    // Caddy is needed for authelia OIDC when TLS provider is caddy.
    // The TLS prompt (ensure_tls_configured) handles Caddy installation
    // when the user picks caddy as their TLS provider, so we only need
    // to install Caddy here if tls is already set to caddy but Caddy
    // isn't installed yet (e.g., config was edited manually).
    let is_caddy_tls = matches!(config.tls, Some(TlsConfig::Caddy));
    let caddy_installed = config
        .services
        .iter()
        .any(|s| WellKnownService::Caddy.matches(&s.name));
    if is_caddy_tls && !caddy_installed {
        println!("\nInstalling caddy (TLS provider)...\n");
        Box::pin(run(
            &[WellKnownService::Caddy.to_string()],
            None,
            false,
            None,
            &[],
            false,
            true,
        ))
        .await?;
    }

    // Install authelia
    if interactive {
        let confirm = Confirm::new()
            .with_prompt("Authelia (SSO provider) is not installed. Install it?")
            .default(true)
            .interact()?;
        if !confirm {
            bail!("authelia is required for --auth");
        }
        // Prompt for authelia's URL
        let authelia_url: String = Input::new()
            .with_prompt("URL for Authelia")
            .default("https://auth.localhost".to_string())
            .interact_text()?;
        println!("\nInstalling authelia...\n");
        Box::pin(run(
            &[WellKnownService::Authelia.to_string()],
            Some(&authelia_url),
            false,
            None,
            &[],
            false,
            true,
        ))
        .await?;
        setup_host_access(&[&authelia_url]);
    } else {
        // Non-interactive: need AUTHELIA_URL in env
        let authelia_url =
            std::env::var("AUTHELIA_URL").unwrap_or_else(|_| "https://auth.localhost".to_string());
        println!("\nInstalling authelia...\n");
        Box::pin(run(
            &[WellKnownService::Authelia.to_string()],
            Some(&authelia_url),
            false,
            None,
            &[],
            false,
            true,
        ))
        .await?;
        setup_host_access(&[&authelia_url]);
    }

    Ok(())
}

/// Ensure auth is configured, possibly installing authelia inline.
/// Returns true if auth is ready, false if user cancelled.
async fn ensure_auth_for_add(
    config: &mut Config,
    paths: &ConfigPaths,
    dry_run: bool,
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

            // Prompt for authelia URL
            let authelia_url: String = if super::is_interactive() {
                Input::new()
                    .with_prompt("URL for Authelia")
                    .default("https://auth.localhost".to_string())
                    .interact_text()?
            } else {
                std::env::var("AUTHELIA_URL")
                    .unwrap_or_else(|_| "https://auth.localhost".to_string())
            };
            // Caddy is needed when TLS provider is caddy.
            let is_caddy_tls = matches!(config.tls, Some(TlsConfig::Caddy));
            let caddy_installed = config
                .services
                .iter()
                .any(|s| WellKnownService::Caddy.matches(&s.name));
            if is_caddy_tls && !caddy_installed {
                println!("\nInstalling caddy (TLS provider)...\n");
                Box::pin(run(
                    &[WellKnownService::Caddy.to_string()],
                    None,
                    false,
                    None,
                    &[],
                    dry_run,
                    true,
                ))
                .await?;
                *config = ryra_core::config::load_or_default(&paths.config_file)?;
            }

            println!("\nInstalling authelia...\n");
            // Recursively install authelia, then reload config
            Box::pin(run(
                &[WellKnownService::Authelia.to_string()],
                Some(&authelia_url),
                false,
                None,
                &[],
                dry_run,
                true,
            ))
            .await?;
            if !dry_run {
                setup_host_access(&[&authelia_url]);
            }
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
