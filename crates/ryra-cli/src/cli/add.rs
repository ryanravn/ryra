use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::Config;
use ryra_core::registry::resolve::ServiceRef;
use ryra_core::registry::service_def::AuthKind;
use ryra_core::{SERVICE_AUTHELIA, SERVICE_CADDY, Warning};

use super::apply;
use super::prompts;

pub async fn run(
    services: &[String],
    domain: Option<&str>,
    auth: bool,
    dry_run: bool,
) -> Result<()> {
    if domain.is_some() && services.len() > 1 {
        bail!("--domain can only be used when adding a single service");
    }

    let interactive = std::io::stdin().is_terminal();

    // Auto-install dependencies: caddy for --domain/--auth, authelia for --auth
    if !dry_run {
        ensure_dependencies(domain, auth, interactive).await?;
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

        // Auth — determined by --auth flag
        let auth_kind: Option<AuthKind> = if auth {
            // --auth flag: use native OIDC if service supports it, otherwise
            // forward auth is handled by Caddy (no auth_kind needed for that)
            if !reg_service.def.integrations.auth.is_empty() {
                Some(reg_service.def.integrations.auth[0].clone())
            } else {
                // No native OIDC — forward auth via Caddy will be added automatically
                // when a domain is set and an auth provider is installed
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
                domain,
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
                    if value != resolved_default {
                        env_overrides.insert(env.name.clone(), value);
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
            domain,
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
                    domain,
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
        if !result.warnings.is_empty() {
            println!();
            for warning in &result.warnings {
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
                }
            }
            println!();

            if interactive && !dry_run {
                let confirmed = Confirm::new()
                    .with_prompt("Continue with these warnings?")
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
                domain: result.domain.as_deref(),
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
            let home_dir = ryra_core::service_home(service)?;
            if let Some(ref domain) = result.domain {
                println!("\n{service} is running at https://{domain}");
            } else {
                println!("\n{service} is running.");
            }

            // Connection info
            if !result.allocated_ports.is_empty() {
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

            // Set up /etc/hosts and CA trust for domain-based services
            if let Some(ref domain) = result.domain {
                let mut domain_list = vec![domain.clone()];
                // Also include authelia's domain if it has one
                if let Ok(installed) = ryra_core::list_installed() {
                    if let Some(auth_svc) = installed.iter().find(|s| s.name == SERVICE_AUTHELIA) {
                        if let Some(ref auth_domain) = auth_svc.domain {
                            if auth_domain != domain {
                                domain_list.push(auth_domain.clone());
                            }
                        }
                    }
                }
                let refs: Vec<&str> = domain_list.iter().map(|s| s.as_str()).collect();
                setup_host_access(&refs);
            }
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

    // Check CA trust
    let ca_trusted = std::path::Path::new("/etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt")
        .exists()
        || std::path::Path::new("/usr/local/share/ca-certificates/ryra-caddy-ca.crt").exists();
    if !ca_trusted {
        let ca_src = ryra_core::service_home("caddy")
            .ok()
            .and_then(|h| h.parent().map(|p| p.join("caddy-root-ca.crt")))
            .filter(|p| p.exists());
        if let Some(ca) = ca_src {
            if std::path::Path::new("/etc/pki/ca-trust").is_dir() {
                commands.push(format!(
                    "sudo cp {} /etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt && sudo update-ca-trust",
                    ca.display()
                ));
            } else if std::path::Path::new("/usr/local/share/ca-certificates").is_dir() {
                commands.push(format!(
                    "sudo cp {} /usr/local/share/ca-certificates/ryra-caddy-ca.crt && sudo update-ca-certificates",
                    ca.display()
                ));
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

/// Auto-install caddy and authelia when --domain or --auth requires them.
async fn ensure_dependencies(
    domain: Option<&str>,
    auth: bool,
    interactive: bool,
) -> Result<()> {
    let needs_caddy = (domain.is_some() || auth) && !ryra_core::caddy::is_installed();
    let config = ryra_core::config::load_or_default(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;
    let needs_authelia = auth
        && !config.services.iter().any(|s| s.name == SERVICE_AUTHELIA)
        && config.auth.is_none();

    if !needs_caddy && !needs_authelia {
        return Ok(());
    }

    // Install caddy first (authelia needs it for --domain)
    if needs_caddy {
        if interactive {
            let confirm = Confirm::new()
                .with_prompt("Caddy (reverse proxy) is not installed. Install it?")
                .default(true)
                .interact()?;
            if !confirm {
                bail!("caddy is required for --domain/--auth");
            }
        }
        println!("\nInstalling caddy...\n");
        Box::pin(run(&[SERVICE_CADDY.to_string()], None, false, false)).await?;
    }

    // Install authelia
    if needs_authelia {
        if interactive {
            let confirm = Confirm::new()
                .with_prompt("Authelia (SSO provider) is not installed. Install it?")
                .default(true)
                .interact()?;
            if !confirm {
                bail!("authelia is required for --auth");
            }
            // Prompt for authelia's domain
            let authelia_domain: String = Input::new()
                .with_prompt("Domain for Authelia")
                .default("auth.local".to_string())
                .interact_text()?;
            println!("\nInstalling authelia...\n");
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                false,
            ))
            .await?;
            setup_host_access(&[&authelia_domain]);
        } else {
            // Non-interactive: need AUTHELIA_ADMIN_PASSWORD in env
            let authelia_domain =
                std::env::var("AUTHELIA_DOMAIN").unwrap_or_else(|_| "auth.local".to_string());
            println!("\nInstalling authelia...\n");
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                false,
            ))
            .await?;
            setup_host_access(&[&authelia_domain]);
        }
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

            // Install caddy first if needed
            if !ryra_core::caddy::is_installed() && !dry_run {
                println!("\nInstalling caddy (needed for auth)...\n");
                Box::pin(run(&[SERVICE_CADDY.to_string()], None, false, dry_run)).await?;
            }

            // Prompt for authelia domain
            let authelia_domain: String = if std::io::stdin().is_terminal() {
                Input::new()
                    .with_prompt("Domain for Authelia")
                    .default("auth.local".to_string())
                    .interact_text()?
            } else {
                std::env::var("AUTHELIA_DOMAIN").unwrap_or_else(|_| "auth.local".to_string())
            };
            println!("\nInstalling authelia...\n");
            // Recursively install authelia, then reload config
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                dry_run,
            ))
            .await?;
            if !dry_run {
                setup_host_access(&[&authelia_domain]);
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
