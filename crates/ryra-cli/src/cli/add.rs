use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::{Confirm, Input};

use ryra_core::config::schema::ExposureMode;
use ryra_core::registry::service_def::DeployMode;
use ryra_core::Warning;

use super::apply;
use super::prompts;

pub async fn run(service: &str, domain: Option<&str>, repo: Option<&str>, dry_run: bool) -> Result<()> {
    let (repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;

    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    let interactive = std::io::stdin().is_terminal();

    // Look up the service definition
    let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;
    let has_nginx = reg_service.def.nginx.is_some();

    // Profile selection for compose services
    let compose_file_override = match &reg_service.def.service.deploy {
        DeployMode::Compose { profiles, .. } if !profiles.is_empty() && interactive => {
            let mut items: Vec<String> = vec!["default".to_string()];
            items.extend(
                profiles
                    .iter()
                    .map(|p| format!("{} — {}", p.name, p.description)),
            );
            let selection = dialoguer::Select::new()
                .with_prompt("Configuration profile")
                .items(&items)
                .default(0)
                .interact()?;
            if selection == 0 {
                None
            } else {
                Some(profiles[selection - 1].file.clone())
            }
        }
        _ => None,
    };

    // Show ALL modes the service supports, annotate which need setup
    let supported = ExposureMode::supported_modes(has_nginx);

    let exposure = if supported.len() == 1 {
        let mode = supported[0].clone();
        println!("Exposure mode: {} — {}", mode.label(), mode.description());
        mode
    } else if interactive {
        let items: Vec<String> = supported
            .iter()
            .map(|m| {
                let missing = m.missing_config(&config);
                if missing.is_empty() {
                    format!("{} — {}", m.label(), m.description())
                } else {
                    format!("{} — {} (setup required)", m.label(), m.description())
                }
            })
            .collect();
        let selection = dialoguer::Select::new()
            .with_prompt("Exposure mode")
            .items(&items)
            .default(0)
            .interact()?;
        supported[selection].clone()
    } else {
        // Non-interactive: pick first mode that needs no setup
        supported
            .iter()
            .find(|m| m.missing_config(&config).is_empty())
            .cloned()
            .unwrap_or(ExposureMode::Local)
    };

    // Just-in-time: prompt for missing config sections
    if !exposure.missing_config(&config).is_empty() {
        if interactive {
            if !prompts::ensure_config_for_mode(&mut config, &paths, &exposure).await? {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            bail!(
                "{} exposure requires additional config. Run interactively or use `ryra config`.",
                exposure.label()
            );
        }
    }

    // Domain — only for proxied modes (tunnel/proxy/dns-only)
    let domain = if exposure.needs_domain() {
        let default_domain = match config.base_domain() {
            Some(d) => format!("{service}.{d}"),
            None => format!("{service}.localhost"),
        };
        Some(match domain {
            Some(d) => d.to_string(),
            None if interactive => Input::new()
                .with_prompt(format!("Domain for {service}"))
                .default(default_domain.clone())
                .interact_text()?,
            None => default_domain,
        })
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
        // Non-interactive: fail if required env vars are missing
        let missing_required: Vec<&str> = promptable
            .iter()
            .filter(|e| e.kind == EnvKind::Required)
            .map(|e| e.name.as_str())
            .collect();
        if !missing_required.is_empty() {
            bail!(
                "required env vars not provided (run interactively or set via test env): {}",
                missing_required.join(", ")
            );
        }
    }

    let result = ryra_core::add_service(
        service,
        domain.as_deref(),
        exposure.clone(),
        &env_overrides,
        compose_file_override.as_deref(),
        &repo_url,
        &repo_dir,
    )?;

    // Show warnings and confirm
    if !result.warnings.is_empty() {
        println!();
        for warning in &result.warnings {
            match warning {
                Warning::NoAuthPublicExposure {
                    service_name,
                    exposure,
                } => {
                    println!(
                        "  WARNING: {service_name} has auth disabled and will be publicly exposed via {exposure}"
                    );
                }
                Warning::HostPortExposure {
                    service_name,
                    ports,
                } => {
                    let port_list: Vec<String> = ports
                        .iter()
                        .map(|(p, proto)| format!("{p}/{proto}"))
                        .collect();
                    println!(
                        "  WARNING: {service_name} will bind to 0.0.0.0 on ports: {}",
                        port_list.join(", ")
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
        if let Some(ref domain) = result.domain {
            println!("{service} will be available at https://{domain}");
        } else {
            println!("{service} will be started (no domain — non-web service)");
        }
    } else {
        println!("Setting up {service} as user {}...", result.username);
        apply::execute_all(&result.steps).await?;
        ryra_core::finalize_add(
            service,
            domain.as_deref(),
            exposure,
            result.deploy_mode,
            &result.repo_url,
            result.host_port,
        )?;
        let home_dir = ryra_core::service_home(service);
        if let Some(ref domain) = result.domain {
            println!("\n{service} is running at https://{domain}");
        } else {
            println!("\n{service} is running.");
        }

        // Connection info
        if !result.allocated_ports.is_empty() {
            for (port_name, host_port) in &result.allocated_ports {
                println!("  Port ({port_name}): 127.0.0.1:{host_port}");
            }
        }
        if !result.generated_secrets.is_empty() {
            println!("  Secrets: {} (auto-generated)", result.generated_secrets.join(", "));
        }
        println!("  Config:  {}", home_dir.display());

        let u = &result.username;
        println!();
        println!("Useful commands:");
        println!("  sudo cat {}", home_dir.join(".env").display());
        println!("  sudo systemctl --machine={u}@ --user status {service}");
        println!("  sudo journalctl _SYSTEMD_USER_UNIT={service}.service -f");
        println!("  sudo systemctl --machine={u}@ --user restart {service}");
    }

    Ok(())
}
