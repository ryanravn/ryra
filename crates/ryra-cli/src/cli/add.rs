use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{Config, TlsConfig};
use ryra_core::registry::resolve::ServiceRef;
use ryra_core::registry::service_def::{AuthKind, HttpsRequirement};
use ryra_core::{SERVICE_AUTHELIA, SERVICE_CADDY, Warning};

use super::apply;
use super::prompts;

pub async fn run(
    services: &[String],
    url: Option<&str>,
    auth: bool,
    dry_run: bool,
) -> Result<()> {
    if url.is_some() && services.len() > 1 {
        bail!("--url can only be used when adding a single service");
    }

    let interactive = std::io::stdin().is_terminal();

    // Auto-install authelia for --auth
    if !dry_run {
        ensure_dependencies(url, auth, interactive).await?;
    }

    for service_input in services {
        let service_ref = ServiceRef::parse(service_input)?;
        let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
        let service = service_ref.service_name();

        let paths = ryra_core::config::ConfigPaths::resolve()?;
        let config = ryra_core::config::load_or_default(&paths.config_file)?;

        // Look up the service definition
        let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;

        // Check architecture compatibility before any prompts
        if let Some(msg) = reg_service.def.check_architecture() {
            bail!("{msg}");
        }

        // TLS — prompt if service requires HTTPS or user passed an https:// URL.
        // When Caddy is the TLS provider and no --url was given, auto-generate
        // a .localhost domain so HTTPS works out of the box.
        let needs_https = reg_service.def.service.https == HttpsRequirement::Required
            || url.is_some_and(|u| u.starts_with("https://"));
        if needs_https && !dry_run {
            let config = ryra_core::config::load_or_default(&paths.config_file)?;
            ensure_tls_configured(&config, &paths, interactive).await?;
        }
        let auto_url: Option<String>;
        let url: Option<&str> = if needs_https && url.is_none() {
            let config = ryra_core::config::load_or_default(&paths.config_file)?;
            if matches!(config.tls, Some(TlsConfig::Caddy)) {
                let caddy_https_port = config
                    .services
                    .iter()
                    .find(|s| s.name == SERVICE_CADDY)
                    .and_then(|s| s.ports.get("https").copied())
                    .unwrap_or(8443);
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
        {
            let config = ryra_core::config::load_or_default(&paths.config_file)?;
            if config.smtp.is_none() {
                match prompts::prompt_smtp()? {
                    prompts::SmtpSetupChoice::Custom(smtp) => {
                        let mut config =
                            ryra_core::config::load_or_default(&paths.config_file)?;
                        config.smtp = Some(smtp);
                        paths.ensure_dirs()?;
                        ryra_core::config::save_config(&paths.config_file, &config)?;
                        println!(
                            "  SMTP configured. Saved to {}\n",
                            paths.config_file.display()
                        );
                    }
                    prompts::SmtpSetupChoice::Inbucket => {
                        // Install inbucket, then configure SMTP to point at it
                        let inbucket_installed = config
                            .services
                            .iter()
                            .any(|s| s.name == "inbucket");
                        if !inbucket_installed {
                            println!("\nInstalling inbucket...\n");
                            Box::pin(run(
                                &["inbucket".to_string()],
                                None,
                                false,
                                false,
                            ))
                            .await?;
                        }
                        // Read inbucket's allocated SMTP port from config
                        let config =
                            ryra_core::config::load_or_default(&paths.config_file)?;
                        let _smtp_port = config
                            .services
                            .iter()
                            .find(|s| s.name == "inbucket")
                            .and_then(|s| s.ports.get("smtp").copied())
                            .unwrap_or(2500);
                        // Use container name for SMTP host — services on the
                        // caddy network can reach inbucket directly. The host
                        // port isn't reachable from --no-hosts containers.
                        let smtp = ryra_core::config::schema::SmtpCredentials {
                            host: "inbucket".to_string(),
                            port: 2500, // inbucket's internal container port
                            username: String::new(),
                            password: String::new(),
                            from: "ryra@localhost".to_string(),
                            security: "off".to_string(),
                        };
                        let mut config =
                            ryra_core::config::load_or_default(&paths.config_file)?;
                        config.smtp = Some(smtp);
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
        }

        // Auth — determined by --auth flag
        let auth_kind: Option<AuthKind> = if auth {
            // --auth flag: use native OIDC if service supports it.
            // Core will error if the service doesn't support OIDC.
            if !reg_service.def.integrations.auth.is_empty() {
                Some(reg_service.def.integrations.auth[0].clone())
            } else {
                None
            }
        } else if !reg_service.def.integrations.auth.is_empty()
            && reg_service.def.integrations.auth.len() == 1
        {
            let kind = &reg_service.def.integrations.auth[0];
            if interactive {
                let enable = Confirm::new()
                    .with_prompt(format!("Enable {kind} auth?"))
                    .default(true)
                    .interact()?;
                if enable {
                    let mut config = config.clone();
                    if config.auth.is_none() {
                        match ensure_auth_for_add(&mut config, &paths, dry_run).await? {
                            true => {}
                            false => return Ok(()),
                        }
                    }
                    Some(kind.clone())
                } else {
                    None
                }
            } else {
                // Non-interactive without --auth: don't auto-enable
                None
            }
        } else if interactive && !reg_service.def.integrations.auth.is_empty() {
            let items: Vec<String> = std::iter::once("None".to_string())
                .chain(
                    reg_service
                        .def
                        .integrations
                        .auth
                        .iter()
                        .map(|k| k.to_string()),
                )
                .collect();
            let selection = dialoguer::Select::new()
                .with_prompt("Auth mode")
                .items(&items)
                .default(1)
                .interact()?;
            if selection == 0 {
                None
            } else {
                let kind = reg_service.def.integrations.auth[selection - 1].clone();
                let mut config = config.clone();
                if config.auth.is_none() {
                    match ensure_auth_for_add(&mut config, &paths, dry_run).await? {
                        true => {}
                        false => return Ok(()),
                    }
                }
                Some(kind)
            }
        } else {
            None
        };

        // Prompt for env vars based on their kind
        use ryra_core::registry::service_def::EnvKind;

        let mut env_overrides = BTreeMap::new();
        let promptable: Vec<_> = reg_service
            .def
            .env
            .iter()
            .filter(|e| matches!(e.kind, EnvKind::Prompted | EnvKind::Required))
            .collect();

        if !promptable.is_empty() && interactive {
            // Resolve template variables in defaults so prompts show real values
            let config_for_defaults = ryra_core::config::load_or_default(&paths.config_file)?;
            let default_ctx = ryra_core::generate::context::build_context(
                &config_for_defaults,
                &reg_service.def,
                None,
                auth_kind.as_ref(),
                url,
            );

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
        ) {
            Err(ryra_core::error::Error::ServiceIncomplete(_)) => {
                println!("{service} was partially installed — cleaning up before retry...");
                let remove_result = ryra_core::remove_service(service)?;
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
                    println!(
                        "  {port_name} port {original_port} → {assigned_port} ({reason})"
                    );
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
                eprintln!("\nError: {e}");
                eprintln!();
                eprintln!("{service} is partially installed. To clean up:");
                eprintln!("  ryra remove {service}");
                eprintln!();
                eprintln!("Or retry:");
                eprintln!("  ryra add {service}");
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
                let env_content = std::fs::read_to_string(&env_path).unwrap_or_default();
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
            println!(
                "  systemctl --user restart {service}  # restart (picks up .env changes)"
            );
            println!("  systemctl --user status {service}  # check if running");
            println!("  journalctl --user-unit {service}.service -f  # follow logs");
        }
    } // end for service_input in services

    Ok(())
}

/// Ensure hostnames resolve and CA is trusted for domain-based services.
/// Collects all needed domains and runs sudo commands with user confirmation.
fn setup_host_access(domains: &[&str]) {
    let mut commands = Vec::new();

    // Check /etc/hosts for each domain
    let hosts = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
    let missing_hosts: Vec<&&str> = domains
        .iter()
        .filter(|d| {
            !hosts.lines().any(|l| {
                let l = l.trim();
                !l.starts_with('#') && l.split_whitespace().any(|w| w == **d)
            })
        })
        .collect();
    if !missing_hosts.is_empty() {
        let hostnames = missing_hosts
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        commands.push(format!(
            "echo '127.0.0.1 {hostnames}' | sudo tee -a /etc/hosts"
        ));
    }

    // Check system CA trust (Fedora, Arch, Debian/Ubuntu)
    let ca_paths = [
        "/etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt",           // Fedora
        "/etc/ca-certificates/trust-source/anchors/ryra-caddy-ca.crt",  // Arch
        "/usr/local/share/ca-certificates/ryra-caddy-ca.crt",           // Debian/Ubuntu
    ];
    let ca_trusted = ca_paths.iter().any(|p| std::path::Path::new(p).exists());
    if !ca_trusted {
        let ca_src = ryra_core::service_home("caddy")
            .ok()
            .and_then(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")))
            .filter(|p| p.exists());
        if let Some(ca) = ca_src {
            if std::path::Path::new("/etc/pki/ca-trust").is_dir() {
                // Fedora / RHEL
                commands.push(format!(
                    "sudo cp {} /etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt && sudo update-ca-trust",
                    ca.display()
                ));
            } else if std::path::Path::new("/etc/ca-certificates/trust-source").is_dir() {
                // Arch Linux
                commands.push(format!(
                    "sudo cp {} /etc/ca-certificates/trust-source/anchors/ryra-caddy-ca.crt && sudo update-ca-trust",
                    ca.display()
                ));
            } else if std::path::Path::new("/usr/local/share/ca-certificates").is_dir() {
                // Debian / Ubuntu
                commands.push(format!(
                    "sudo cp {} /usr/local/share/ca-certificates/ryra-caddy-ca.crt && sudo update-ca-certificates",
                    ca.display()
                ));
            }
        }
    }

    // Check browser CA trust (Chromium/Brave/Chrome use NSS database)
    let nssdb = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".pki/nssdb"))
        .filter(|p| p.exists());
    if let Some(ref nssdb_path) = nssdb {
        let already_in_nss = std::process::Command::new("certutil")
            .args(["-d", &format!("sql:{}", nssdb_path.display()), "-L", "-n", "ryra-caddy-ca"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !already_in_nss {
            let ca_src = ryra_core::service_home("caddy")
                .ok()
                .and_then(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")))
                .filter(|p| p.exists());
            if let Some(ca) = ca_src {
                // No sudo needed — NSS db is user-owned
                let nss_cmd = format!(
                    "certutil -d sql:{} -A -t 'C,,' -n ryra-caddy-ca -i {}",
                    nssdb_path.display(),
                    ca.display()
                );
                // Run directly since it doesn't need sudo
                let status = std::process::Command::new("sh")
                    .args(["-c", &nss_cmd])
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
    }

    if commands.is_empty() {
        return;
    }

    println!();
    println!("  Domain setup (one-time, requires sudo):");
    for cmd in &commands {
        println!("    {cmd}");
    }

    let interactive = std::io::stdin().is_terminal();
    if interactive {
        let run = Confirm::new()
            .with_prompt("  Run these commands now?")
            .default(true)
            .interact()
            .unwrap_or(false);
        if run {
            for cmd in &commands {
                let status = std::process::Command::new("sh")
                    .args(["-c", cmd])
                    .status();
                match status {
                    Ok(s) if s.success() => {}
                    Ok(_) => eprintln!("  Command failed: {cmd}"),
                    Err(e) => eprintln!("  Failed to run command: {e}"),
                }
            }
            println!("  Done.");
        }
    }
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
                let caddy_installed = config.services.iter().any(|s| s.name == SERVICE_CADDY);
                if !caddy_installed {
                    println!("\nInstalling caddy (TLS provider)...\n");
                    Box::pin(run(
                        &[SERVICE_CADDY.to_string()],
                        None,
                        false,
                        false,
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

    // Not configured yet — prompt or error
    if !interactive {
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
            let caddy_installed = config.services.iter().any(|s| s.name == SERVICE_CADDY);
            if !caddy_installed {
                println!("\nInstalling caddy...\n");
                Box::pin(run(
                    &[SERVICE_CADDY.to_string()],
                    None,
                    false,
                    false,
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
            println!(
                "  Make sure HTTPS is configured externally before using this service."
            );
            TlsConfig::None
        }
    };

    // Reload config from disk — Caddy install may have updated it
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    config.tls = Some(tls);
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("  TLS configured. Saved to {}\n", paths.config_file.display());

    Ok(())
}

/// Auto-install authelia when --auth requires it.
async fn ensure_dependencies(
    _url: Option<&str>,
    auth: bool,
    interactive: bool,
) -> Result<()> {
    let config = ryra_core::config::load_or_default(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;
    let needs_authelia = auth
        && !config.services.iter().any(|s| s.name == SERVICE_AUTHELIA)
        && config.auth.is_none();

    if !needs_authelia {
        return Ok(());
    }

    // Caddy is needed for authelia OIDC when TLS provider is caddy.
    // The TLS prompt (ensure_tls_configured) handles Caddy installation
    // when the user picks caddy as their TLS provider, so we only need
    // to install Caddy here if tls is already set to caddy but Caddy
    // isn't installed yet (e.g., config was edited manually).
    let config_fresh = ryra_core::config::load_or_default(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;
    let is_caddy_tls = matches!(config_fresh.tls, Some(TlsConfig::Caddy));
    let caddy_installed = config_fresh
        .services
        .iter()
        .any(|s| s.name == SERVICE_CADDY);
    if is_caddy_tls && !caddy_installed {
        println!("\nInstalling caddy (TLS provider)...\n");
        Box::pin(run(
            &[SERVICE_CADDY.to_string()],
            None,
            false,
            false,
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
            &[SERVICE_AUTHELIA.to_string()],
            Some(&authelia_url),
            false,
            false,
        ))
        .await?;
        setup_host_access(&[&authelia_url]);
    } else {
        // Non-interactive: need AUTHELIA_URL in env
        let authelia_url =
            std::env::var("AUTHELIA_URL").unwrap_or_else(|_| "https://auth.localhost".to_string());
        println!("\nInstalling authelia...\n");
        Box::pin(run(
            &[SERVICE_AUTHELIA.to_string()],
            Some(&authelia_url),
            false,
            false,
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
            let authelia_installed = config.services.iter().any(|s| s.name == SERVICE_AUTHELIA);
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
            let authelia_url: String = if std::io::stdin().is_terminal() {
                Input::new()
                    .with_prompt("URL for Authelia")
                    .default("https://auth.localhost".to_string())
                    .interact_text()?
            } else {
                std::env::var("AUTHELIA_URL").unwrap_or_else(|_| "https://auth.localhost".to_string())
            };
            // Caddy is needed when TLS provider is caddy.
            let is_caddy_tls = matches!(config.tls, Some(TlsConfig::Caddy));
            let caddy_installed = config.services.iter().any(|s| s.name == SERVICE_CADDY);
            if is_caddy_tls && !caddy_installed {
                println!("\nInstalling caddy (TLS provider)...\n");
                Box::pin(run(
                    &[SERVICE_CADDY.to_string()],
                    None,
                    false,
                    dry_run,
                ))
                .await?;
                *config = ryra_core::config::load_or_default(&paths.config_file)?;
            }

            println!("\nInstalling authelia...\n");
            // Recursively install authelia, then reload config
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_url),
                false,
                dry_run,
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

/// Try to configure auth from an already-installed authelia instance.
/// The .env is user-readable under ~/.local/share/ryra/authelia/.env.
fn try_configure_auth_from_installed(config: &mut Config, paths: &ConfigPaths) -> Result<bool> {
    let env_path = ryra_core::service_home(SERVICE_AUTHELIA)?.join(".env");
    let env_content = match std::fs::read_to_string(&env_path) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };

    // Find the port from the installed service record
    let service = config.services.iter().find(|s| s.name == SERVICE_AUTHELIA);
    let port = service
        .and_then(|s| s.ports.values().next().copied())
        .unwrap_or(9091);

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
