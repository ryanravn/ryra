use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Confirm;

use super::apply;

pub async fn run(service: &str, repo: Option<&str>, yes: bool, dry_run: bool) -> Result<()> {
    let (_repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;

    let result = ryra_core::update_service(service, &BTreeMap::new(), &repo_dir)?;

    if result.changes.is_empty() {
        println!("{service} is already up to date.");
        return Ok(());
    }

    // Show what changed
    println!("Changes detected for {service}:\n");
    for change in &result.changes {
        println!("  - {change}");
    }
    println!();

    // Destructive warning
    println!("WARNING: This will stop {service}, regenerate ALL config files");
    println!("(including environment variables and secrets), and restart it.");
    println!("Volumes are preserved but secrets will be regenerated.");
    println!("This operation is destructive and cannot be undone.\n");

    if dry_run {
        super::print_dry_run(&result.steps);
        return Ok(());
    }

    // Require confirmation
    if !yes {
        let interactive = std::io::stdin().is_terminal();
        if !interactive {
            anyhow::bail!("update is destructive — pass --yes to confirm in non-interactive mode");
        }
        let confirmed = Confirm::new()
            .with_prompt("Proceed with update?")
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!("Updating {service}...");
    apply::execute_all(&result.steps).await?;
    ryra_core::finalize_update(service, &repo_dir)?;

    println!("\n{service} has been updated and restarted.");
    println!("\nUseful commands:");
    println!("  systemctl --user status {service}");
    println!("  journalctl --user-unit {service}.service -f");

    Ok(())
}
