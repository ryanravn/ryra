use anyhow::Result;
use dialoguer::Input;

use super::apply;

pub async fn run(
    services: &[String],
    all: bool,
    yes: bool,
    dry_run: bool,
    purge: bool,
) -> Result<()> {
    let mode = if purge {
        ryra_core::RemoveMode::Purge
    } else {
        ryra_core::RemoveMode::Preserve
    };

    // `-a` mode expands to every installed service and uses a single
    // confirmation prompt. `ryra reset` stays the nuke-everything
    // command — it additionally wipes ryra's own config, CAs, snapshots,
    // registry caches. `rm -a` only touches services.
    let targets: Vec<String> = if all {
        let paths = ryra_core::config::ConfigPaths::resolve()?;
        let config = ryra_core::config::load_or_default(&paths.config_file)?;
        let names: Vec<String> = config
            .services
            .iter()
            .filter(|s| s.installed)
            .map(|s| s.name.clone())
            .collect();
        if names.is_empty() {
            println!("No installed services.");
            return Ok(());
        }
        if !yes && !dry_run {
            if !super::is_interactive() {
                anyhow::bail!("use --yes (-y) to confirm in non-interactive mode");
            }
            println!("This will remove {} service(s):", names.len());
            for n in &names {
                println!("  {n}");
            }
            println!();
            match mode {
                ryra_core::RemoveMode::Purge => {
                    println!("Mode: --purge — data and volumes will also be DELETED.");
                }
                ryra_core::RemoveMode::Preserve => {
                    println!(
                        "Mode: data-preserving — run `ryra data rm --all` later to clean."
                    );
                }
            }
            println!();
            let input: String = Input::new()
                .with_prompt("Type \"remove all\" to confirm")
                .interact_text()?;
            if input != "remove all" {
                println!("Cancelled.");
                return Ok(());
            }
        }
        names
    } else {
        services.to_vec()
    };

    // `--all` runs the bulk prompt once up front, so per-service prompts
    // are redundant there. `-y` and `--dry-run` also skip the prompt.
    let skip_per_service_prompt = all || yes || dry_run;

    for service in &targets {
        let service = service.as_str();
        let result = ryra_core::remove_service(service, mode)?;

        if !skip_per_service_prompt {
            if !super::is_interactive() {
                anyhow::bail!("use --yes (-y) to confirm removal in non-interactive mode");
            }
            let home_dir = ryra_core::service_home(service)?;
            println!("This will:");
            println!("  - Stop and remove {service}");
            match mode {
                ryra_core::RemoveMode::Purge => {
                    println!("  - Delete ALL data and config at {}", home_dir.display());
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
