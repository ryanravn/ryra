//! `ryra revert` — undo the last `ryra upgrade` for a service by restoring
//! its pre-upgrade backup snapshot. The actual planning lives in
//! `ryra-core::revert_service`; this module owns the user flow: list
//! snapshots when none is selected, show the plan, confirm, apply.

use std::collections::BTreeSet;

use anyhow::Result;
use console::style;
use dialoguer::Confirm;

use ryra_core::RevertResult;

use super::apply;

pub async fn run(
    services: &[String],
    at: Option<&str>,
    yes: bool,
    dry_run: bool,
    list: bool,
) -> Result<()> {
    if list {
        return list_snapshots(services);
    }

    let targets = resolve_targets(services)?;
    if targets.is_empty() {
        anyhow::bail!("no services specified — pass a service name, or run `ryra revert --list`");
    }

    let _lock = super::lock::MutationLock::acquire(dry_run)?;

    if at.is_some() && targets.len() != 1 {
        anyhow::bail!("--at can only be used when reverting a single service");
    }

    let mut plans: Vec<RevertResult> = Vec::with_capacity(targets.len());
    for service in &targets {
        let plan = ryra_core::revert_service(service, at)?;
        plans.push(plan);
    }

    print_summary(&plans);

    if dry_run {
        println!("Dry run — no changes made. Remove --dry-run to apply.\n");
        return Ok(());
    }

    if !yes {
        if super::is_interactive() {
            let proceed = Confirm::new()
                .with_prompt("Apply revert?")
                .default(true)
                .interact()?;
            if !proceed {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            anyhow::bail!(
                "non-interactive run without --yes — re-run with `ryra revert --yes` (or --dry-run to preview)"
            );
        }
    }

    for plan in &plans {
        println!();
        println!("Reverting {}…", style(&plan.service).bold());
        apply::execute_all(&plan.steps).await?;
    }
    println!();
    println!("Done.");
    Ok(())
}

fn print_summary(plans: &[RevertResult]) {
    for plan in plans {
        println!(
            "{} {} {}",
            style(&plan.service).bold(),
            style("←").cyan(),
            style(&plan.snapshot.timestamp).dim()
        );
        for path in &plan.files_to_restore {
            println!(
                "  {} {}  {}",
                style("~").yellow(),
                path.display(),
                style("restore from backup").yellow()
            );
        }
        for path in &plan.files_to_delete {
            println!(
                "  {} {}  {}",
                style("-").red(),
                path.display(),
                style("delete (added by upgrade)").red()
            );
        }
        println!(
            "  {} systemctl --user daemon-reload + restart {} (brief downtime)",
            style("→").cyan(),
            plan.service
        );
    }
    println!();
}

fn list_snapshots(services: &[String]) -> Result<()> {
    let targets = if services.is_empty() {
        ryra_core::list_installed()?
            .into_iter()
            .map(|s| s.name)
            .collect()
    } else {
        services.to_vec()
    };
    if targets.is_empty() {
        println!("No services installed.");
        return Ok(());
    }
    let mut any = false;
    for service in &targets {
        let snapshots = ryra_core::list_backups(service)?;
        if snapshots.is_empty() {
            continue;
        }
        any = true;
        println!("{}", style(service).bold());
        for snap in snapshots {
            println!("  {}  {}", snap.timestamp, style(snap.path.display()).dim());
        }
    }
    if !any {
        println!("No backups found. `ryra upgrade` creates them.");
    }
    Ok(())
}

fn resolve_targets(services: &[String]) -> Result<Vec<String>> {
    if services.is_empty() {
        return Ok(Vec::new());
    }
    for s in services {
        if !ryra_core::is_service_installed(s) {
            anyhow::bail!("service '{s}' is not installed");
        }
    }
    let mut out: Vec<String> = services.to_vec();
    let mut seen = BTreeSet::new();
    out.retain(|s| seen.insert(s.clone()));
    Ok(out)
}
