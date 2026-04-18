use anyhow::Result;
use dialoguer::Input;

use super::{apply, remove_caddy_ca};

pub async fn run(yes: bool, dry_run: bool) -> Result<()> {
    let result = ryra_core::reset()?;
    if result.steps.is_empty() {
        println!("Nothing to reset — no ryra artifacts found.");
        return Ok(());
    }

    if !yes && !dry_run {
        if super::is_interactive() {
            println!("This will:");
            println!("  - Stop and remove all installed services");
            println!("  - Delete all ryra state and configuration");
            println!();

            let input: String = Input::new()
                .with_prompt("Type \"reset\" to confirm")
                .interact_text()?;

            if input != "reset" {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            anyhow::bail!("use --yes (-y) to confirm reset in non-interactive mode");
        }
    }

    if dry_run {
        super::print_dry_run(&result.steps);
    } else {
        println!("Resetting ryra...");
        apply::execute_all(&result.steps).await?;
        ryra_core::finalize_reset()?;
        remove_caddy_ca();
        super::linger::offer_disable().await?;
        println!("\nryra has been fully reset.");
    }

    Ok(())
}
