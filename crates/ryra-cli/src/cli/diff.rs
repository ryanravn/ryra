//! `ryra diff` — show the gap between the current registry and what's on
//! disk for an installed service. Read-only; never mutates state.
//!
//! Output mirrors the verbs the user sees on `ryra upgrade`:
//!
//!   ~ /home/.../seafile.container        modified (registry changed)
//!   + /home/.../scripts/enable-seafdav.sh added (new file)
//!   - /home/.../old-helper.sh             removed (no longer in registry)
//!   ! /home/.../seafile.container        drift (you edited this)
//!
//! Drift entries (`!`) block `ryra upgrade` without `--force`; they're flagged
//! here so the user can decide what to do before running upgrade.

use std::collections::BTreeSet;

use anyhow::Result;
use console::style;

use ryra_core::{DiffKind, DiffResult};

pub async fn run(services: &[String]) -> Result<()> {
    let targets = resolve_targets(services)?;
    if targets.is_empty() {
        println!("No services installed.");
        return Ok(());
    }

    let mut any_drift = false;
    let mut any_change = false;

    for service in &targets {
        let diff = ryra_core::diff_service(service).await?;
        print_one(&diff);
        if !diff.drifted().is_empty() {
            any_drift = true;
        }
        if !diff.is_clean() {
            any_change = true;
        }
    }

    if !any_change {
        println!("Everything up to date.");
    } else if any_drift {
        println!();
        println!(
            "{} hand-edited files would block `ryra upgrade` — re-run with --force to overwrite, or back up your changes first.",
            style("!").red().bold()
        );
    } else {
        println!();
        println!("Run `ryra upgrade` to apply.");
    }

    Ok(())
}

fn print_one(diff: &DiffResult) {
    let header = if diff.is_clean() {
        format!("{} (clean)", style(&diff.service).bold())
    } else {
        style(&diff.service).bold().to_string()
    };
    println!("{header}");

    if diff.is_clean() {
        return;
    }

    for entry in &diff.entries {
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
                style("drift (hand-edited)").red().bold()
            ),
        }
    }
}

/// When `services` is empty, diff every installed service. Otherwise validate
/// each name before kicking off the (potentially slow) async diff run so the
/// user gets the typo error up front.
fn resolve_targets(services: &[String]) -> Result<Vec<String>> {
    if !services.is_empty() {
        for s in services {
            if !ryra_core::is_service_installed(s) {
                anyhow::bail!("service '{s}' is not installed");
            }
        }
        let mut out: Vec<String> = services.to_vec();
        // De-dup while preserving the user's order — `ryra diff foo foo bar`
        // shouldn't print foo twice but should keep the foo-before-bar order.
        let mut seen = BTreeSet::new();
        out.retain(|s| seen.insert(s.clone()));
        return Ok(out);
    }
    let installed = ryra_core::list_installed()?;
    Ok(installed.into_iter().map(|s| s.name).collect())
}
