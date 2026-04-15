use anyhow::Result;
use dialoguer::Input;

use super::apply;

pub async fn run(services: &[String], yes: bool, dry_run: bool) -> Result<()> {
    for service in services {
        let result = ryra_core::remove_service(service)?;

        if !yes && !dry_run {
            if super::is_interactive() {
                let home_dir = ryra_core::service_home(service)?;
                println!("This will:");
                println!("  - Stop and remove {service}");
                println!("  - Delete all data and config at {}", home_dir.display());
                println!();

                let input: String = Input::new()
                    .with_prompt(format!("Type \"{service}\" to confirm removal"))
                    .interact_text()?;

                if input != *service {
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
            if service == ryra_core::SERVICE_CADDY {
                super::remove_caddy_ca();
            }
            println!("\n{service} removed.");
        }
    } // end for service in services

    Ok(())
}
