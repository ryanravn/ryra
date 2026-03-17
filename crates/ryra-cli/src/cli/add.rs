use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::{Confirm, Input};

use ryra_core::config::schema::ExposureMode;
use ryra_core::config::schema::SslConfig;
use ryra_core::Warning;

use super::apply;

pub async fn run(service: &str, domain: Option<&str>, dry_run: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_config(&paths.config_file)?;
    let interactive = std::io::stdin().is_terminal();

    // Look up the service definition to determine if it's a web service
    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();
    let reg_service =
        ryra_core::registry::find_service(&paths.cache_dir, &reg_pairs, service)?;
    let is_web = reg_service.def.nginx.is_some();

    // Only prompt for domain if this is a web service
    let domain = if is_web {
        let default_domain = format!("{service}.{}", config.host.domain);
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

    // Compute available exposure modes
    let available = ExposureMode::available_modes(&config.cloudflare, is_web);

    let exposure = match available.len() {
        0 => bail!("No exposure modes available (this shouldn't happen)"),
        1 => {
            let mode = available.into_iter().next().unwrap_or(ExposureMode::Local);
            println!("  Exposure: {} (only available option)", mode.label());
            mode
        }
        _ if interactive => {
            let items: Vec<String> = available
                .iter()
                .map(|m| format!("{} — {}", m.label(), m.description()))
                .collect();
            let selection = dialoguer::Select::new()
                .with_prompt("Exposure mode")
                .items(&items)
                .default(0)
                .interact()?;
            available[selection].clone()
        }
        _ => {
            // Non-interactive: default to first available (Local)
            available.into_iter().next().unwrap_or(ExposureMode::Local)
        }
    };

    // If DnsOnly and no SSL config, ask for LE email
    if exposure == ExposureMode::DnsOnly && config.ssl.is_none() {
        if interactive {
            let le_email: String = Input::new()
                .with_prompt("Email for Let's Encrypt SSL certificates")
                .interact_text()?;
            config.ssl = Some(SslConfig::Letsencrypt { email: le_email });
            ryra_core::config::save_config(&paths.config_file, &config)?;
        } else {
            bail!("DnsOnly exposure requires --email for Let's Encrypt SSL");
        }
    }

    // Prompt for configurable env vars (those with `prompt` set in service.toml)
    let mut env_overrides = BTreeMap::new();
    let promptable: Vec<_> = reg_service
        .def
        .env
        .iter()
        .filter(|e| e.prompt.is_some())
        .collect();

    if !promptable.is_empty() {
        if interactive {
            println!("\nConfigure {service}:");
            for env in &promptable {
                let prompt_text = env.prompt.as_deref().unwrap_or(&env.name);
                let value: String = Input::new()
                    .with_prompt(format!("  {prompt_text}"))
                    .default(env.value.clone())
                    .interact_text()?;
                if value != env.value {
                    env_overrides.insert(env.name.clone(), value);
                }
            }
            println!();
        }
        // Non-interactive: use defaults silently
    }

    let result =
        ryra_core::add_service(service, domain.as_deref(), exposure.clone(), &env_overrides)?;

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
        // Only record the service after all steps succeed
        ryra_core::finalize_add(service, domain.as_deref(), exposure)?;
        let quadlet_dir = ryra_core::service_quadlet_dir(service);
        if let Some(ref domain) = result.domain {
            println!("\n{service} is running at https://{domain}");
        } else {
            println!("\n{service} is running.");
        }
        println!("  Config: {}", quadlet_dir.display());
        println!(
            "  Restart: sudo systemctl --machine={}@ --user restart {service}",
            result.username
        );
    }

    Ok(())
}
