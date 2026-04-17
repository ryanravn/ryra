use std::collections::BTreeMap;

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
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    if url.is_some() && services.len() > 1 {
        bail!("--url can only be used when adding a single service");
    }

    let interactive = super::is_interactive();

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

    for service_input in services {
        let service_ref = ServiceRef::parse(service_input)?;
        let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
        let service = service_ref.service_name();

        // Load config once — previous iterations or ensure_dependencies may have
        // modified it on disk (e.g., installing caddy or authelia).
        let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

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

        // SMTP — prompt if service supports it and not yet configured
        if reg_service.def.integrations.smtp
            && !reg_service.def.mappings.smtp.is_empty()
            && !dry_run
            && interactive
            && config.smtp.is_none()
        {
            match prompts::prompt_smtp()? {
                prompts::SmtpSetupChoice::Custom(smtp) => {
                    config.smtp = Some(smtp);
                    paths.ensure_dirs()?;
                    ryra_core::config::save_config(&paths.config_file, &config)?;
                    println!(
                        "  SMTP configured. Saved to {}\n",
                        paths.config_file.display()
                    );
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
                }
                prompts::SmtpSetupChoice::Skip => {}
            }
        }

        // Auth — determined by --auth flag or interactive prompt
        let auth_kind: Option<AuthKind> =
            resolve_auth_kind(auth, interactive, &reg_service.def.integrations.auth)?;

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
        let promptable: Vec<_> = reg_service
            .def
            .env
            .iter()
            .filter(|e| matches!(e.kind, EnvKind::Prompted | EnvKind::Required))
            .collect();

        if !promptable.is_empty() && interactive {
            // Resolve template variables in defaults so prompts show real values.
            // This context is reused by add_service so the secrets the user saw
            // during prompts match what gets written to .env.
            let default_ctx = ryra_core::generate::context::build_context(
                &config,
                &reg_service.def,
                None,
                auth_kind.as_ref(),
                url,
            );
            prompt_ctx = Some(default_ctx.clone());

            println!("\nConfigure {service}:");
            for env in &promptable {
                let prompt_text = env.prompt.as_deref().unwrap_or(&env.name);
                let is_required = env.kind == EnvKind::Required;

                if is_required {
                    // Required: must provide a value, no default
                    let value: String = Input::new()
                        .with_prompt(format!("  {prompt_text} (required)"))
                        .interact_text()?;
                    env_overrides.insert(env.name.clone(), value);
                } else {
                    // Resolve template in default value
                    let resolved_default =
                        ryra_core::generate::template::render(&env.value, &default_ctx)
                            .unwrap_or_else(|_| env.value.clone());
                    let value: String = Input::new()
                        .with_prompt(format!("  {prompt_text}"))
                        .default(resolved_default.clone())
                        .interact_text()?;
                    // Always save for secret-templated values — re-rendering
                    // would generate a different random secret.
                    if value != resolved_default {
                        env_overrides.insert(env.name.clone(), value);
                    } else if env.value.contains("{{secret.") {
                        env_overrides.insert(env.name.clone(), resolved_default);
                    }
                }
            }
            println!();
        } else if !interactive {
            // Non-interactive: read env vars from the process environment.
            // Required vars must be set; prompted vars use their default but
            // can be overridden via the environment.
            let mut missing_required = Vec::new();
            for env in &promptable {
                if let Ok(val) = std::env::var(&env.name) {
                    env_overrides.insert(env.name.clone(), val);
                } else if env.kind == EnvKind::Required {
                    missing_required.push(env.name.as_str());
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
            &env_overrides,
            service_ref.registry_name(),
            &repo_dir,
            prompt_ctx.clone(),
        ) {
            Err(ryra_core::error::Error::ServiceIncomplete(_)) => {
                println!("{service} was partially installed — cleaning up before retry...");
                let remove_result = ryra_core::remove_service(service, ryra_core::RemoveMode::Purge)?;
                apply::execute_all(&remove_result.steps).await?;
                ryra_core::finalize_remove(service)?;
                // Retry now that the partial state is gone
                ryra_core::add_service(
                    service,
                    url,
                    auth_kind.clone(),
                    auth || auth_kind.is_some(),
                    &env_overrides,
                    service_ref.registry_name(),
                    &repo_dir,
                    prompt_ctx.clone(),
                )?
            }
            other => other?,
        };

        // Show warnings and confirm
        // Show port reassignment notes
        for warning in &result.warnings {
            if let Warning::PortReassigned {
                port_name,
                original_port,
                assigned_port,
                reason,
                ..
            } = warning
            {
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
        }

        // Collect warnings that need confirmation (RAM + port conflicts)
        let needs_confirm: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| match w {
                Warning::RamBelowMinimum { .. } | Warning::RamBelowRecommended { .. } => true,
                Warning::PortReassigned { original_port, .. } => *original_port >= 1024,
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
                    Warning::PortReassigned { .. } => {} // already printed above
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

            println!("Setting up {service}...");
            if let Err(e) = apply::execute_all(&result.steps).await {
                eprintln!("\nError during setup: {e}");
                eprintln!("Cleaning up partial installation...");
                // Attempt cleanup so the user doesn't have to do it manually.
                // If cleanup fails, fall back to telling the user how to do it.
                match ryra_core::remove_service(service, ryra_core::RemoveMode::Purge) {
                    Ok(remove_result) => {
                        if let Err(cleanup_err) = apply::execute_all(&remove_result.steps).await {
                            eprintln!("Cleanup also failed: {cleanup_err}");
                            eprintln!("Run manually: ryra rm {service}");
                        } else {
                            if let Err(e) = ryra_core::finalize_remove(service) {
                                eprintln!("Warning: finalize_remove failed: {e}");
                            }
                            eprintln!("Cleaned up. Retry with: ryra add {service}");
                        }
                    }
                    Err(_) => {
                        eprintln!("Could not clean up automatically.");
                        eprintln!("Run manually: ryra rm {service}");
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
    // Only warn in non-dry-run; a plan shouldn't produce system-state
    // warnings.
    if !dry_run {
        super::linger::warn_if_disabled().await?;
    }

    Ok(())
}

/// Ensure hostnames resolve and CA is trusted for domain-based services.
/// Collects all needed operations and runs them with user confirmation.
/// All external commands use explicit args (no shell interpolation).
fn setup_host_access(domains: &[&str]) {
    use std::process::{Command, Stdio};

    let mut sudo_needed = false;

    // --- Detect what needs to be done ---

    // 1. Check /etc/hosts for missing domains
    let hosts = match std::fs::read_to_string("/etc/hosts") {
        Ok(content) => content,
        Err(e) => {
            eprintln!("  Warning: could not read /etc/hosts: {e}");
            return;
        }
    };
    let missing_hosts: Vec<&str> = domains
        .iter()
        .filter(|d| {
            !hosts.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.split_whitespace().any(|w| w == **d)
            })
        })
        .copied()
        .collect();
    if !missing_hosts.is_empty() {
        sudo_needed = true;
    }

    // 2. Check system CA trust — find the right target for this distro
    let ca_source = ryra_core::service_home("caddy")
        .map(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")))
        .unwrap_or_else(|e| {
            eprintln!("  Warning: could not resolve caddy service home: {e}");
            None
        })
        .filter(|p| p.exists());
    let ca_target = super::CA_TARGETS.iter().find(|t| {
        let dir = std::path::Path::new(t.cert_path)
            .parent()
            .unwrap_or(std::path::Path::new("/"));
        dir.is_dir()
    });
    let need_ca = ca_source.is_some()
        && ca_target.is_some()
        && !super::CA_TARGETS
            .iter()
            .any(|t| std::path::Path::new(t.cert_path).exists());
    if need_ca {
        sudo_needed = true;
    }

    // 3. Browser NSS trust (no sudo needed — handled separately)
    let nssdb = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".pki/nssdb"))
        .filter(|p| p.exists());
    if let Some(ref nssdb_path) = nssdb {
        let nss_arg = format!("sql:{}", nssdb_path.display());
        let already_in_nss = match Command::new("certutil")
            .args(["-d", &nss_arg, "-L", "-n", "ryra-caddy-ca"])
            .output()
        {
            Ok(o) => o.status.success(),
            Err(e) => {
                eprintln!("  Warning: could not check browser trust store: {e}");
                false
            }
        };
        if !already_in_nss && let Some(ref ca) = ca_source {
            let status = Command::new("certutil")
                .args([
                    "-d",
                    &nss_arg,
                    "-A",
                    "-t",
                    "C,,",
                    "-n",
                    "ryra-caddy-ca",
                    "-i",
                    &ca.display().to_string(),
                ])
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!("  Caddy CA added to browser trust store.");
                }
                _ => {
                    eprintln!("  Warning: could not add Caddy CA to browser trust store.");
                }
            }
        }
    }

    if !sudo_needed {
        return;
    }

    // --- Display what will be done ---
    println!();
    println!("  Domain setup (one-time, requires sudo):");
    if !missing_hosts.is_empty() {
        println!(
            "    sudo tee -a /etc/hosts  (add: {})",
            missing_hosts.join(" ")
        );
    }
    if let (true, Some(target)) = (need_ca, ca_target) {
        println!(
            "    sudo cp <caddy-ca> {} && sudo {}",
            target.cert_path, target.update_cmd
        );
    }

    // --- Confirm ---
    let interactive = super::is_interactive();
    if interactive {
        let run = match Confirm::new()
            .with_prompt("  Run these commands now?")
            .default(true)
            .interact()
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  Warning: could not read confirmation ({e}), skipping domain setup");
                return;
            }
        };
        if !run {
            println!();
            return;
        }
    }

    // --- Execute (no shell interpolation) ---
    if !missing_hosts.is_empty() {
        let entry = format!("127.0.0.1 {}\n", missing_hosts.join(" "));
        match Command::new("sudo")
            .args(["tee", "-a", "/etc/hosts"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    if let Err(e) = stdin.write_all(entry.as_bytes()) {
                        eprintln!("  Failed to write to /etc/hosts: {e}");
                    }
                }
                match child.wait() {
                    Ok(s) if s.success() => {}
                    _ => eprintln!("  Failed to update /etc/hosts"),
                }
            }
            Err(e) => eprintln!("  Failed to run sudo: {e}"),
        }
    }

    if let (true, Some(ca), Some(target)) = (need_ca, ca_source.as_ref(), ca_target) {
        let cp_ok = match Command::new("sudo")
            .args(["cp", &ca.display().to_string(), target.cert_path])
            .status()
        {
            Ok(s) => s.success(),
            Err(e) => {
                eprintln!("  Failed to install CA certificate: {e}");
                false
            }
        };
        if cp_ok {
            match Command::new("sudo").arg(target.update_cmd).status() {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("  Warning: {} exited with {s}", target.update_cmd),
                Err(e) => eprintln!("  Warning: failed to run {}: {e}", target.update_cmd),
            }
        } else {
            eprintln!("  Failed to install CA certificate");
        }
    }

    println!("  Done.");
    println!();
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
fn resolve_auth_kind(
    auth_flag: bool,
    interactive: bool,
    supported: &[AuthKind],
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
            .default(true)
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
        .default(1)
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
