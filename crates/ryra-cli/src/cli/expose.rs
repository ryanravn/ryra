use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use ryra_core::Warning;
use ryra_core::config::schema::ExposureMode;

use super::apply;
use super::prompts;

pub async fn run(service: &str, domain: Option<&str>, dry_run: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    let interactive = std::io::stdin().is_terminal();

    // Find the installed service
    let installed = config
        .services
        .iter()
        .find(|s| s.name == service)
        .ok_or_else(|| anyhow::anyhow!("{service} is not installed"))?;

    let current_exposure = installed.exposure.clone();
    let current_domain = installed.domain.clone();

    println!(
        "Current exposure: {} — {}",
        current_exposure.label(),
        current_exposure.description()
    );
    if let Some(ref d) = current_domain {
        println!("Current domain: {d}");
    }

    // Load service def to know capabilities
    let (repo_url, repo_dir) = ryra_core::resolve_repo(
        config
            .services
            .iter()
            .find(|s| s.name == service)
            .map(|s| s.repo.as_str()),
    )
    .await?;
    let _ = repo_url;

    let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;
    let has_nginx = reg_service.def.nginx.is_some();

    // Show all supported modes
    let supported = ExposureMode::supported_modes(has_nginx);

    let new_exposure = if !interactive {
        bail!("Expose command requires interactive mode");
    } else {
        let items: Vec<String> = supported
            .iter()
            .map(|m| {
                let label = format!("{} — {}", m.label(), m.description());
                let missing = m.missing_config(&config);
                if *m == current_exposure {
                    format!("{label} (current)")
                } else if !missing.is_empty() {
                    format!("{label} (setup required)")
                } else {
                    label
                }
            })
            .collect();

        let current_idx = supported
            .iter()
            .position(|m| *m == current_exposure)
            .unwrap_or(0);

        let selection = dialoguer::Select::new()
            .with_prompt("New exposure mode")
            .items(&items)
            .default(current_idx)
            .interact()?;

        supported[selection].clone()
    };

    if new_exposure == current_exposure {
        println!("No change.");
        return Ok(());
    }

    // Just-in-time config for new mode
    if !new_exposure.missing_config(&config).is_empty()
        && !prompts::ensure_config_for_mode(&mut config, &paths, &new_exposure).await?
    {
        println!("Cancelled.");
        return Ok(());
    }

    // Domain for new mode
    let new_domain = if new_exposure.needs_domain() {
        let default_domain = if new_exposure == ExposureMode::Tailscale {
            match ryra_core::integrations::tailscale::detect_fqdn() {
                Some(fqdn) => fqdn,
                None => {
                    bail!("Tailscale is not running or has no FQDN. Is tailscaled active?");
                }
            }
        } else {
            match config.base_domain() {
                Some(d) => format!("{service}.{d}"),
                None => format!("{service}.localhost"),
            }
        };
        let default = current_domain.unwrap_or(default_domain);
        Some(match domain {
            Some(d) => d.to_string(),
            None => Input::new()
                .with_prompt(format!("Domain for {service}"))
                .default(default)
                .interact_text()?,
        })
    } else {
        None
    };

    let result = ryra_core::change_exposure(
        service,
        new_exposure.clone(),
        new_domain.as_deref(),
        &repo_dir,
    )?;

    // Show warnings
    if !result.warnings.is_empty() {
        println!();
        for warning in &result.warnings {
            match warning {
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
                Warning::NoAuthPublicExposure {
                    service_name,
                    exposure,
                } => {
                    println!(
                        "  WARNING: {service_name} has auth disabled and will be publicly exposed via {exposure}"
                    );
                }
                Warning::OidcLocalExposure {
                    service_name,
                    exposure,
                } => {
                    println!(
                        "  NOTE: {service_name} has OIDC auth enabled with {exposure} exposure."
                    );
                    println!(
                        "        OIDC login via browser (e.g. SSH tunnel) will likely fail because"
                    );
                    println!(
                        "        the browser and server disagree on redirect URLs."
                    );
                    println!(
                        "        Consider using 'tailscale' exposure mode for OIDC to work."
                    );
                }
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
    }

    if dry_run {
        super::print_dry_run(&result.steps);
    } else {
        if !result.steps.is_empty() {
            let confirmed = Confirm::new()
                .with_prompt(format!(
                    "Change {service} from {} to {}?",
                    current_exposure.label(),
                    new_exposure.label()
                ))
                .default(true)
                .interact()?;
            if !confirmed {
                println!("Cancelled.");
                return Ok(());
            }
        }

        println!("Updating {service}...");
        apply::execute_all(&result.steps).await?;

        // Find host_port from installed service (may have been allocated)
        let host_port = config
            .services
            .iter()
            .find(|s| s.name == service)
            .and_then(|s| s.host_port);

        ryra_core::finalize_expose(
            service,
            new_exposure.clone(),
            new_domain.as_deref(),
            host_port,
        )?;

        if let Some(ref domain) = new_domain {
            println!(
                "{service} now exposed at https://{domain} via {}",
                new_exposure.label()
            );
        } else {
            println!("{service} exposure changed to {}", new_exposure.label());
        }
    }

    Ok(())
}
