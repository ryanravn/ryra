use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Input;
use ryra_core::config::schema::ExposureMode;

use super::apply;

pub async fn run(service: &str, yes: bool, dry_run: bool) -> Result<()> {
    let result = ryra_core::remove_service(service)?;

    if !yes && !dry_run {
        if std::io::stdin().is_terminal() {
            let home_dir = ryra_core::service_home(service);
            println!("This will:");
            println!("  - Stop {service} and remove user {}", result.username);
            println!("  - Delete all data and config at {}", home_dir.display());
            match (&result.exposure, &result.domain) {
                (ExposureMode::Tunnel, Some(domain)) => {
                    println!("  - Remove tunnel route for {domain}");
                }
                (ExposureMode::Proxy | ExposureMode::DnsOnly, Some(domain)) => {
                    println!("  - Delete DNS record for {domain}");
                }
                _ => {}
            }
            println!();

            let input: String = Input::new()
                .with_prompt(format!("Type \"{service}\" to confirm removal"))
                .interact_text()?;

            if input != service {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            anyhow::bail!("use --yes (-y) to confirm removal in non-interactive mode");
        }
    }

    if dry_run {
        super::print_dry_run(&result.steps);
    } else {
        println!("Removing {service}...");
        apply::execute_all(&result.steps).await?;
        ryra_core::finalize_remove(&result.service_name)?;
        println!("\n{service} removed.");
    }

    Ok(())
}
