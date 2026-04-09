use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use std::path::Path;

use ryra_core::{SERVICE_AUTHELIA, SERVICE_CADDY, Warning};
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::Config;
use ryra_core::registry::service_def::AuthKind;

use super::apply;
use super::prompts;

pub async fn run(
    services: &[String],
    domain: Option<&str>,
    auth: bool,
    repo: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if domain.is_some() && services.len() > 1 {
        bail!("--domain can only be used when adding a single service");
    }

    let (repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;
    let interactive = std::io::stdin().is_terminal();

    // Auto-install dependencies: caddy for --domain/--auth, authelia for --auth
    if !dry_run {
        ensure_dependencies(domain, auth, interactive, &repo_url, &repo_dir).await?;
    }

    for service in services {
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
                        match ensure_auth_for_add(
                            &mut config,
                            &paths,
                            &repo_url,
                            &repo_dir,
                            dry_run,
                        )
                        .await?
                        {
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
                    match ensure_auth_for_add(&mut config, &paths, &repo_url, &repo_dir, dry_run)
                        .await?
                    {
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

        let result = ryra_core::add_service(
            service,
            domain,
            auth_kind.clone(),
            auth,
            &env_overrides,
            &repo_url,
            &repo_dir,
        )?;

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
            println!("Setting up {service}...");
            apply::execute_all(&result.steps).await?;
            ryra_core::finalize_add(ryra_core::FinalizeAddParams {
                service_name: service,
                auth_kind,
                repo: &result.repo_url,
                allocated_ports: &result.allocated_ports,
                repo_dir: &repo_dir,
                env_content: &result.env_content,
                domain: result.domain.as_deref(),
            })?;
            let home_dir = ryra_core::service_home(service);
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
                    if let Some(line) = matching_env {
                        if let Some((key, val)) = line.split_once('=') {
                            println!("    {key}={val}");
                            continue;
                        }
                    }
                    println!("    {secret_name} (see .env)");
                }
            }
            println!("  Config:  {}", home_dir.display());

            if let Some(note) = &result.note {
                println!();
                for line in note.lines() {
                    println!("  {line}");
                }
            }

            println!();
            println!("Useful commands:");
            println!("  cat {}", home_dir.join(".env").display());
            println!("  systemctl --user status {service}");
            println!("  journalctl --user-unit {service}.service -f");
            println!("  systemctl --user restart {service}");
        }
    } // end for service in services

    Ok(())
}

/// Auto-install caddy and authelia when --domain or --auth requires them.
async fn ensure_dependencies(
    domain: Option<&str>,
    auth: bool,
    interactive: bool,
    repo_url: &str,
    _repo_dir: &Path,
) -> Result<()> {
    let needs_caddy = (domain.is_some() || auth) && !ryra_core::caddy::is_installed();
    let config = ryra_core::config::load_or_default(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;
    let needs_authelia =
        auth && !config.services.iter().any(|s| s.name == SERVICE_AUTHELIA) && config.auth.is_none();

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
        Box::pin(run(
            &[SERVICE_CADDY.to_string()],
            None,
            false,
            Some(repo_url),
            false,
        ))
        .await?;
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
                .default("auth.localhost".to_string())
                .interact_text()?;
            println!("\nInstalling authelia...\n");
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                Some(repo_url),
                false,
            ))
            .await?;
        } else {
            // Non-interactive: need AUTHELIA_ADMIN_PASSWORD in env
            let authelia_domain =
                std::env::var("AUTHELIA_DOMAIN").unwrap_or_else(|_| "auth.localhost".to_string());
            println!("\nInstalling authelia...\n");
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                Some(repo_url),
                false,
            ))
            .await?;
        }
    }

    Ok(())
}

/// Ensure auth is configured, possibly installing authelia inline.
/// Returns true if auth is ready, false if user cancelled.
async fn ensure_auth_for_add(
    config: &mut Config,
    paths: &ConfigPaths,
    repo_url: &str,
    _repo_dir: &Path,
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
                Box::pin(run(
                    &[SERVICE_CADDY.to_string()],
                    None,
                    false,
                    Some(repo_url),
                    dry_run,
                ))
                .await?;
            }

            // Prompt for authelia domain
            let authelia_domain: String = if std::io::stdin().is_terminal() {
                Input::new()
                    .with_prompt("Domain for Authelia")
                    .default("auth.localhost".to_string())
                    .interact_text()?
            } else {
                std::env::var("AUTHELIA_DOMAIN").unwrap_or_else(|_| "auth.localhost".to_string())
            };
            println!("\nInstalling authelia...\n");
            // Recursively install authelia, then reload config
            Box::pin(run(
                &[SERVICE_AUTHELIA.to_string()],
                Some(&authelia_domain),
                false,
                Some(repo_url),
                dry_run,
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

/// Try to configure auth from an already-installed authelia instance.
/// The .env is user-readable under ~/.local/share/ryra/authelia/.env.
fn try_configure_auth_from_installed(config: &mut Config, paths: &ConfigPaths) -> Result<bool> {
    let env_path = ryra_core::service_home(SERVICE_AUTHELIA).join(".env");
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
