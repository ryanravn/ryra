use anyhow::Result;
use dialoguer::Input;
use ryra_core::Step;

use super::{apply, remove_caddy_ca};

pub async fn run(yes: bool, dry_run: bool) -> Result<()> {
    let result = ryra_core::reset()?;
    if result.steps.is_empty() {
        println!("Nothing to reset — no ryra artifacts found.");
        return Ok(());
    }

    let tailnet_count = result
        .steps
        .iter()
        .filter(|s| matches!(s, Step::TailscaleDisable { .. }))
        .count();

    if !yes && !dry_run {
        if super::is_interactive() {
            println!("This will:");
            println!("  - Stop and remove all installed services");
            if tailnet_count > 0 {
                let plural = if tailnet_count == 1 { "" } else { "s" };
                println!(
                    "  - Remove {tailnet_count} service{plural} from your tailnet (deregister via Tailscale Admin API)"
                );
            }
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
        // Clear failed-state for any unit that died at some point during
        // the user's session (e.g. a service that crashed before reset
        // ran). Without this, `systemctl --user list-units` keeps showing
        // those units in failed state long after their files are gone.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "reset-failed"])
            .status();
        remove_caddy_ca();
        super::sysctl_low_ports::offer_disable().await?;
        super::linger::note_if_enabled().await;
        println!("\nryra has been fully reset.");
    }

    Ok(())
}
