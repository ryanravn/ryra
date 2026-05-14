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
    let volume_count = result
        .steps
        .iter()
        .filter(|s| matches!(s, Step::RemoveVolume { .. }))
        .count();

    if !yes && !dry_run {
        if super::is_interactive() {
            println!("This will:");
            println!("  - Stop all installed services");
            println!(
                "  - Delete ~/.local/share/services/  (per-service quadlets, configs, .env, metadata.toml, bind-mounted data)"
            );
            println!("  - Delete ~/.config/services/        (ryra preferences)");
            println!("  - Clear ryra symlinks from ~/.config/containers/systemd/");
            if tailnet_count > 0 {
                let plural = if tailnet_count == 1 { "" } else { "s" };
                println!(
                    "  - Remove {tailnet_count} service{plural} from your tailnet (deregister via Tailscale Admin API)"
                );
            }
            if volume_count > 0 {
                let plural = if volume_count == 1 { "" } else { "s" };
                println!("  - Remove {volume_count} podman named volume{plural}");
            }
            println!("  - Remove the local Caddy CA cert from your trust store (if present)");
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
