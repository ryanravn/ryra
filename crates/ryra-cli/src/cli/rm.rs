use anyhow::Result;
use dialoguer::Input;

use super::apply;

pub async fn run(services: &[String], yes: bool, dry_run: bool, purge: bool) -> Result<()> {
    let mode = if purge {
        ryra_core::RemoveMode::Purge
    } else {
        ryra_core::RemoveMode::Preserve
    };

    for service in services {
        let result = ryra_core::remove_service(service, mode)?;

        if !yes && !dry_run {
            if super::is_interactive() {
                let home_dir = ryra_core::service_home(service)?;
                println!("This will:");
                println!("  - Stop and remove {service}");
                match mode {
                    ryra_core::RemoveMode::Purge => {
                        println!(
                            "  - Delete ALL data and config at {}",
                            home_dir.display()
                        );
                        println!("  - Remove any podman named volumes for this service");
                    }
                    ryra_core::RemoveMode::Preserve => {
                        println!("  - Delete config + .env at {}", home_dir.display());
                        println!(
                            "  - Keep data subdirs and podman volumes (run `ryra data rm {service}` later to delete)"
                        );
                    }
                }
                println!();

                let input: String = Input::new()
                    .with_prompt(format!("Type \"{service}\" to confirm"))
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
            if ryra_core::WellKnownService::Caddy.matches(service) {
                super::remove_caddy_ca();
            }
            match mode {
                ryra_core::RemoveMode::Purge => {
                    println!("\n{service} removed (purged).");
                }
                ryra_core::RemoveMode::Preserve => {
                    println!(
                        "\n{service} removed. Data preserved at {}.",
                        ryra_core::service_home(service)?.display()
                    );
                    println!("Run `ryra data rm {service}` to delete.");
                }
            }
        }
    }

    Ok(())
}
