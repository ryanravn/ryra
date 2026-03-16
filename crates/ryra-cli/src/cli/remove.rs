use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Confirm;

use super::apply;

pub async fn run(service: &str, yes: bool, dry_run: bool) -> Result<()> {
    if !yes {
        if std::io::stdin().is_terminal() {
            let confirmed = Confirm::new()
                .with_prompt(format!(
                    "Remove {service}? This will stop the service, delete its files, and remove user {}.",
                    ryra_core::service_user(service)
                ))
                .default(false)
                .interact()?;

            if !confirmed {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            anyhow::bail!("use --yes (-y) to confirm removal in non-interactive mode");
        }
    }

    let result = ryra_core::remove_service(service)?;

    if dry_run {
        super::print_dry_run(&result.steps);
    } else {
        println!("Removing {service}...");
        apply::execute_all(&result.steps).await?;
        // Only clean up ryra state after all steps succeed
        ryra_core::finalize_remove(&result.service_name)?;
        println!("\n{service} removed.");
    }

    Ok(())
}
