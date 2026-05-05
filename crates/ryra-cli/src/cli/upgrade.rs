//! `ryra upgrade` — re-render an installed service against the current
//! registry, back up files that change, and restart the unit. The render
//! itself happens in `ryra-core::upgrade_service`; this module owns the
//! user-facing flow: print the diff, confirm, apply.

use std::collections::BTreeSet;

use anyhow::Result;
use console::style;
use dialoguer::Confirm;

use ryra_core::{DiffKind, UpgradeResult};

use super::apply;

pub async fn run(services: &[String], yes: bool, force: bool, dry_run: bool) -> Result<()> {
    let targets = resolve_targets(services)?;
    if targets.is_empty() {
        println!("No services installed.");
        return Ok(());
    }

    // Plan every service first. Aborting early on a hand-edit (without
    // --force) means we never partially upgrade some services and refuse
    // others.
    let mut plans: Vec<UpgradeResult> = Vec::with_capacity(targets.len());
    for service in &targets {
        match ryra_core::upgrade_service(service, force).await {
            Ok(plan) => plans.push(plan),
            Err(ryra_core::error::Error::HandEditedFiles { service, paths }) => {
                eprintln!(
                    "{} {service}: {} hand-edited file(s):",
                    style("error").red().bold(),
                    paths.len()
                );
                for p in &paths {
                    eprintln!("  {} {}", style("!").red().bold(), p.display());
                }
                eprintln!();
                eprintln!(
                    "Re-run with {} to overwrite (backups land in {}), or back up the changes first.",
                    style("--force").bold(),
                    style("~/.local/state/ryra/backups/").dim()
                );
                anyhow::bail!("upgrade aborted");
            }
            Err(e) => return Err(e.into()),
        }
    }

    let any_change = plans.iter().any(|p| !p.diff.is_clean());
    if !any_change {
        println!("Everything up to date.");
        return Ok(());
    }

    print_summary(&plans);

    if dry_run {
        println!("Dry run — no changes made. Remove --dry-run to apply.\n");
        return Ok(());
    }

    if !yes {
        if super::is_interactive() {
            let proceed = Confirm::new()
                .with_prompt("Apply upgrade?")
                .default(true)
                .interact()?;
            if !proceed {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            // No TTY and no --yes: refuse to apply silently. `ryra upgrade`
            // restarts services (brief downtime) and edits running quadlets,
            // so silently proceeding from a script is the wrong default.
            anyhow::bail!(
                "non-interactive run without --yes — re-run with `ryra upgrade --yes` (or --dry-run to preview)"
            );
        }
    }

    for plan in &plans {
        if plan.diff.is_clean() {
            continue;
        }
        println!();
        println!("Upgrading {}…", style(&plan.service).bold());
        apply::execute_all(&plan.steps).await?;
        if let Some(backup) = &plan.backup_dir {
            println!(
                "  {} {}",
                style("Backed up to").dim(),
                style(backup.display()).dim()
            );
        }
    }
    println!();
    println!("Done.");
    Ok(())
}

fn print_summary(plans: &[UpgradeResult]) {
    for plan in plans {
        if plan.diff.is_clean() {
            continue;
        }
        println!("{}", style(&plan.service).bold());
        for entry in &plan.diff.entries {
            match entry.kind {
                DiffKind::Unchanged => {}
                DiffKind::Added => println!(
                    "  {} {}  {}",
                    style("+").green().bold(),
                    entry.path.display(),
                    style("added").green()
                ),
                DiffKind::Modified => println!(
                    "  {} {}  {}",
                    style("~").yellow(),
                    entry.path.display(),
                    style("modified").yellow()
                ),
                DiffKind::Removed => println!(
                    "  {} {}  {}",
                    style("-").red(),
                    entry.path.display(),
                    style("removed").red()
                ),
                DiffKind::Drift => println!(
                    "  {} {}  {}",
                    style("!").red().bold(),
                    entry.path.display(),
                    style("drift (overwriting via --force)").red().bold()
                ),
            }
        }
        for add in &plan.diff.env_additions {
            println!(
                "  {} env: {}={}  {}",
                style("+").green().bold(),
                add.key,
                add.value,
                style("appended to .env (registry-added)").green()
            );
        }
        // Surface the side effects the user is consenting to. Restart
        // means a brief downtime; backup path tells them where to look
        // if something goes sideways; the revert hint is the get-out-of-jail
        // card so they don't have to figure out how to roll back manually.
        println!(
            "  {} systemctl --user daemon-reload + restart {} (brief downtime)",
            style("→").cyan(),
            plan.service
        );
        if let Some(backup) = &plan.backup_dir {
            println!(
                "  {} backup of replaced files: {}",
                style("→").cyan(),
                style(backup.display()).dim()
            );
            println!(
                "  {} undo with: {}",
                style("→").cyan(),
                style(format!("ryra revert {}", plan.service)).dim()
            );
        }
    }
    println!();
}

fn resolve_targets(services: &[String]) -> Result<Vec<String>> {
    if !services.is_empty() {
        for s in services {
            if !ryra_core::is_service_installed(s) {
                anyhow::bail!("service '{s}' is not installed");
            }
        }
        let mut out: Vec<String> = services.to_vec();
        let mut seen = BTreeSet::new();
        out.retain(|s| seen.insert(s.clone()));
        return Ok(out);
    }
    let installed = ryra_core::list_installed()?;
    Ok(installed.into_iter().map(|s| s.name).collect())
}
