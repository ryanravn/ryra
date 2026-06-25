//! `ryra upgrade` — re-render an installed service against the current
//! registry, back up files that change, and restart the unit. The render
//! itself happens in `ryra-core::upgrade_service`; this module owns the
//! user-facing flow: print the diff, confirm, apply.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use console::style;
use dialoguer::{Confirm, Input};

use ryra_core::registry::service_def::EnvKind;
use ryra_core::{DiffKind, EnvAddition, GeneratedFile, Step, UpgradeResult};

use super::apply;

pub async fn run(services: &[String], yes: bool, force: bool, dry_run: bool) -> Result<()> {
    let targets = resolve_targets(services)?;
    if targets.is_empty() {
        println!("No services installed.");
        return Ok(());
    }

    let _lock = super::lock::MutationLock::acquire(dry_run)?;

    // Plan every service first. Aborting early on a hand-edit (without
    // --force) means we never partially upgrade some services and refuse
    // others.
    let mut plans: Vec<UpgradeResult> = Vec::with_capacity(targets.len());
    for service in &targets {
        match ryra_core::ops::plan_upgrade(&ryra_core::ops::UpgradeRequest {
            service: service.to_string(),
            force,
        })
        .await
        {
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

    let any_change = plans.iter().any(|p| !p.diff.is_clean() || p.force_apply);
    if !any_change {
        println!("Everything up to date.");
        return Ok(());
    }

    print_summary(&plans);

    if dry_run {
        println!("Dry run — no changes made. Remove --dry-run to apply.\n");
        return Ok(());
    }

    // Prompt for any env additions that the registry marks as Prompted /
    // Required — these are user-facing values (admin email, OAuth client
    // ids, etc.) where the registry's literal default is a placeholder
    // ("admin@example.com") and silently appending it would be wrong.
    // Default-kind additions are appended as-is. Non-interactive runs
    // accept defaults for Prompted but bail on Required (no value to
    // use, and we don't want to silently write nothing).
    for plan in plans.iter_mut() {
        prompt_and_patch_env_additions(plan)?;
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
        if plan.diff.is_clean() && !plan.force_apply {
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
        // Cap the backup tree to the most recent N snapshots per
        // service so a long history of upgrades doesn't accumulate
        // forever. Best-effort — a prune failure doesn't fail the
        // upgrade (the new backup is already on disk).
        match ryra_core::prune_backups(&plan.service, ryra_core::DEFAULT_BACKUP_KEEP) {
            Ok(pruned) if !pruned.is_empty() => {
                println!(
                    "  {} pruned {} older backup(s) (keep={})",
                    style("⌫").dim(),
                    pruned.len(),
                    ryra_core::DEFAULT_BACKUP_KEEP
                );
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("  warning: backup prune failed: {e}");
            }
        }
    }
    println!();
    println!("Done.");
    Ok(())
}

/// Walk `plan.diff.env_additions`, prompt for any Prompted / Required
/// entries, and rewrite the plan's `.env` write step to use the
/// user-chosen values. Default-kind entries flow through unchanged.
fn prompt_and_patch_env_additions(plan: &mut UpgradeResult) -> Result<()> {
    if plan.diff.env_additions.is_empty() {
        return Ok(());
    }
    let needs_prompt: Vec<&EnvAddition> = plan
        .diff
        .env_additions
        .iter()
        .filter(|a| matches!(a.kind, EnvKind::Prompted | EnvKind::Required))
        .collect();
    if needs_prompt.is_empty() {
        return Ok(());
    }
    let interactive = super::is_interactive();
    let has_required = needs_prompt
        .iter()
        .any(|a| matches!(a.kind, EnvKind::Required));
    if !interactive && has_required {
        anyhow::bail!(
            "{}: registry adds required env var(s); re-run interactively or pre-populate them in `.env`:\n  {}",
            plan.service,
            needs_prompt
                .iter()
                .filter(|a| matches!(a.kind, EnvKind::Required))
                .map(|a| a.key.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    let mut overrides: BTreeMap<String, String> = BTreeMap::new();
    if interactive {
        println!();
        println!("Confirm env values for {}:", style(&plan.service).bold());
        for add in &needs_prompt {
            let label = add.prompt.as_deref().unwrap_or(&add.key);
            let value = match add.kind {
                EnvKind::Required => Input::<String>::new()
                    .with_prompt(format!("  {label} (required)"))
                    .interact_text()?,
                EnvKind::Prompted => Input::<String>::new()
                    .with_prompt(format!("  {label}"))
                    .default(add.value.clone())
                    .interact_text()?,
                EnvKind::Default => continue,
            };
            if value != add.value {
                overrides.insert(add.key.clone(), value);
            }
        }
    }
    if overrides.is_empty() {
        // User accepted every default — nothing to patch.
        return Ok(());
    }
    // Apply overrides to the addition list (so the summary the user
    // already saw remains coherent with what gets written).
    for add in plan.diff.env_additions.iter_mut() {
        if let Some(v) = overrides.get(&add.key) {
            add.value = v.clone();
        }
    }
    // Rebuild the .env write step with the new values. The old step's
    // content was a `read existing + append` so we redo that with the
    // patched additions.
    let env_path = ryra_core::service_home(&plan.service)?.join(".env");
    let mut content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("read {}: {e}", env_path.display()),
    };
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for add in &plan.diff.env_additions {
        content.push_str(&format!("{}={}\n", add.key, add.value));
    }
    // Find and replace the env step in plan.steps. Add a fresh step at
    // the end of the file-write region if it isn't there (defensive —
    // shouldn't happen).
    let mut replaced = false;
    for step in plan.steps.iter_mut() {
        if let Step::WriteFile(file) = step
            && file.path == env_path
        {
            file.content = content.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        plan.steps.push(Step::WriteFile(GeneratedFile {
            path: env_path,
            content,
        }));
    }
    Ok(())
}

fn print_summary(plans: &[UpgradeResult]) {
    for plan in plans {
        if plan.diff.is_clean() {
            // Native services rebuild from source even with a clean config
            // diff; say so rather than showing an empty entry.
            if plan.force_apply {
                println!(
                    "{}  {}",
                    style(&plan.service).bold(),
                    style("(rebuild from source)").dim()
                );
            }
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
