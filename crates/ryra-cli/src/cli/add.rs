use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Input;

use super::apply;

pub async fn run(service: &str, domain: Option<&str>, dry_run: bool) -> Result<()> {
    let config = ryra_core::config::load_config(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;

    let default_domain = format!("{service}.{}", config.host.domain);

    let domain = match domain {
        Some(d) => d.to_string(),
        None if std::io::stdin().is_terminal() => Input::new()
            .with_prompt(format!("Domain for {service}"))
            .default(default_domain.clone())
            .interact_text()?,
        None => default_domain,
    };

    let result = ryra_core::add_service(service, &domain)?;

    if dry_run {
        super::print_dry_run(&result.steps);
        println!("{service} will be available at https://{domain}");
    } else {
        println!("Setting up {service} as user {}...", result.username);
        apply::execute_all(&result.steps).await?;
        println!("\n{service} is running at https://{domain}");
    }

    Ok(())
}
