use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::Input;

use ryra_core::config::schema::{ExposureMode, SslConfig};

use super::apply;

pub async fn run(service: &str, domain: Option<&str>, dry_run: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_config(&paths.config_file)?;
    let interactive = std::io::stdin().is_terminal();

    let default_domain = format!("{service}.{}", config.host.domain);

    let domain = match domain {
        Some(d) => d.to_string(),
        None if interactive => Input::new()
            .with_prompt(format!("Domain for {service}"))
            .default(default_domain.clone())
            .interact_text()?,
        None => default_domain,
    };

    // Compute available exposure modes
    let available = ExposureMode::available_modes(&config.cloudflare);

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

    let result = ryra_core::add_service(service, &domain, exposure.clone())?;

    if dry_run {
        super::print_dry_run(&result.steps);
        println!("{service} will be available at https://{domain}");
    } else {
        println!("Setting up {service} as user {}...", result.username);
        apply::execute_all(&result.steps).await?;
        // Only record the service after all steps succeed
        ryra_core::finalize_add(service, &domain, exposure)?;
        println!("\n{service} is running at https://{domain}");
    }

    Ok(())
}
