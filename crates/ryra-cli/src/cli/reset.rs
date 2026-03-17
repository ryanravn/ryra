use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Confirm;

use super::apply;

pub async fn run(yes: bool, dry_run: bool) -> Result<()> {
    let result = ryra_core::reset(&[]);
    if result.steps.is_empty() {
        println!("Nothing to reset — no ryra artifacts found.");
        return Ok(());
    }

    if !yes && !dry_run {
        if std::io::stdin().is_terminal() {
            println!("This will:");
            println!("  - Stop and remove all installed services and their users");
            println!("  - Remove nginx and cloudflared containers");
            println!("  - Delete all certs, nginx configs, and ryra state");
            println!();

            let confirmed = Confirm::new()
                .with_prompt("Reset ryra completely?")
                .default(false)
                .interact()?;

            if !confirmed {
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
        println!("\nryra has been fully reset.");
    }

    Ok(())
}
