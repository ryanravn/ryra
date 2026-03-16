use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Confirm;
use ryra_core::config::schema::ExposureMode;

use super::apply;

pub async fn run(service: &str, yes: bool, dry_run: bool) -> Result<()> {
    let result = ryra_core::remove_service(service)?;

    if !yes && !dry_run {
        if std::io::stdin().is_terminal() {
            println!("This will:");
            println!("  - Stop {service} and remove user {}", result.username);
            match &result.exposure {
                ExposureMode::Tunnel => {
                    println!("  - Remove tunnel route for {}", result.domain);
                }
                ExposureMode::Proxy | ExposureMode::DnsOnly => {
                    println!("  - Delete DNS record for {}", result.domain);
                }
                ExposureMode::Local => {}
            }
            println!();

            let confirmed = Confirm::new()
                .with_prompt(format!("Remove {service}?"))
                .default(true)
                .interact()?;

            if !confirmed {
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
