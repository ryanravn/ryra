use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use std::path::Path;

use ryra_core::Warning;
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::Config;
use ryra_core::registry::service_def::AuthKind;

use super::apply;
use super::prompts;

pub async fn run(services: &[String], repo: Option<&str>, dry_run: bool) -> Result<()> {
    let (repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;

    for service in services {
        let paths = ryra_core::config::ConfigPaths::resolve()?;
        let config = ryra_core::config::load_or_default(&paths.config_file)?;
        let interactive = std::io::stdin().is_terminal();

        // Look up the service definition
        let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;

        // Check architecture compatibility before any prompts
        if let Some(msg) = reg_service.def.check_architecture() {
            bail!("{msg}");
        }

        // Auth — ask user if they want to enable auth (if the service supports it)
        let auth_kind: Option<AuthKind> = if reg_service.def.integrations.auth.is_empty() {
            None
        } else if reg_service.def.integrations.auth.len() == 1 {
            let kind = &reg_service.def.integrations.auth[0];
            if interactive {
                let enable = Confirm::new()
                    .with_prompt(format!("Enable {kind} auth?"))
                    .default(true)
                    .interact()?;
                if enable {
                    // Ensure auth is configured
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
                // Non-interactive: enable auth if configured
                if config.auth.is_some() {
                    Some(kind.clone())
                } else {
                    None
                }
            }
        } else if interactive {
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
                    // Prompted: has a default, user can accept or change
                    let value: String = Input::new()
                        .with_prompt(format!("  {prompt_text}"))
                        .default(env.value.clone())
                        .interact_text()?;
                    if value != env.value {
                        env_overrides.insert(env.name.clone(), value);
                    }
                }
            }
            println!();
        } else if !interactive {
            // Non-interactive: read required env vars from the process environment,
            // fail if any are still missing.
            let mut missing_required = Vec::new();
            for env in &promptable {
                if env.kind == EnvKind::Required {
                    if let Ok(val) = std::env::var(&env.name) {
                        env_overrides.insert(env.name.clone(), val);
                    } else {
                        missing_required.push(env.name.as_str());
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

        let result = ryra_core::add_service(
            service,
            auth_kind.clone(),
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
                privileged: result.privileged,
            })?;
            let home_dir = ryra_core::service_home(service);
            println!("\n{service} is running.");

            // Connection info
            if !result.allocated_ports.is_empty() {
                for (port_name, host_port) in &result.allocated_ports {
                    println!("  Port ({port_name}): 127.0.0.1:{host_port}");
                }
            }
            if !result.generated_secrets.is_empty() {
                println!(
                    "  Secrets: {} (auto-generated)",
                    result.generated_secrets.join(", ")
                );
            }
            println!("  Config:  {}", home_dir.display());

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

/// Ensure auth is configured, possibly installing authentik inline.
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
        prompts::AuthSetupChoice::InstallAuthentik => {
            // Check if authentik is already installed but auth wasn't configured
            let authentik_installed = config.services.iter().any(|s| s.name == "authentik");
            if authentik_installed {
                println!();
                println!("Authentik is already installed — configuring auth...");
                if try_configure_auth_from_installed(config, paths)? {
                    return Ok(true);
                }
                println!("Could not auto-configure auth from installed authentik.");
                return Ok(false);
            }

            println!();
            println!("Installing authentik first...");
            println!();
            // Recursively install authentik, then reload config
            Box::pin(run(&["authentik".to_string()], Some(repo_url), dry_run)).await?;
            // Reload config — authentik's finalize_add auto-configures [auth]
            *config = ryra_core::config::load_or_default(&paths.config_file)?;
            if config.auth.is_some() {
                println!();
                Ok(true)
            } else {
                println!("Auth was not configured after installing authentik.");
                Ok(false)
            }
        }
        prompts::AuthSetupChoice::Skip => {
            println!("Skipped auth setup.");
            Ok(false)
        }
    }
}

/// Try to configure auth from an already-installed authentik instance.
/// The .env is user-readable under ~/.local/share/ryra/authentik/.env.
fn try_configure_auth_from_installed(config: &mut Config, paths: &ConfigPaths) -> Result<bool> {
    let env_path = ryra_core::service_home("authentik").join(".env");
    let env_content = match std::fs::read_to_string(&env_path) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };

    // Find the bootstrap token
    let token = env_content
        .lines()
        .find_map(|line| line.strip_prefix("AUTHENTIK_BOOTSTRAP_TOKEN="))
        .map(|v| v.to_string());

    let Some(token) = token else {
        return Ok(false);
    };

    // Find the URL from the installed service record — use the first allocated port
    let service = config.services.iter().find(|s| s.name == "authentik");
    let port = service
        .and_then(|s| s.ports.values().next().copied())
        .unwrap_or(9000);
    let url = format!("http://localhost:{port}");

    config.auth = Some(ryra_core::config::schema::AuthCredentials::Authentik {
        url,
        api_token: token,
    });
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, config)?;
    println!(
        "  Auth configured. Saved to {}",
        paths.config_file.display()
    );
    Ok(true)
}
