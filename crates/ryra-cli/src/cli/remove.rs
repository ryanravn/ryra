use anyhow::Result;
use dialoguer::{Confirm, Input};
use ryra_core::Step;
use ryra_core::data::{ServiceData, ServiceStatus};

use super::apply;

pub async fn run(
    services: &[String],
    all: bool,
    orphans: bool,
    yes: bool,
    dry_run: bool,
    purge: bool,
) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;

    // `--orphans` purges every orphan service (leftover data with no
    // config entry). Never touches installed services. `--purge` is
    // implied — orphans have nothing else to preserve.
    // `-a` expands to every installed service. With `--purge` it also
    // sweeps every orphan. `ryra reset` remains distinct — it
    // additionally wipes ryra's own config, CAs, and registry caches.
    let (targets, effective_purge) = if orphans {
        let names: Vec<String> = ryra_core::data::enumerate_all()?
            .into_iter()
            .filter(|s| matches!(s.status, ServiceStatus::Orphan))
            .map(|s| s.service)
            .collect();
        if names.is_empty() {
            println!("No orphan data to purge.");
            return Ok(());
        }
        confirm_bulk(&names, true, yes, dry_run)?;
        (names, true)
    } else if all {
        let mut names: Vec<String> = ryra_core::scan_managed_services()?;
        if purge {
            for svc in ryra_core::data::enumerate_all()? {
                if matches!(svc.status, ServiceStatus::Orphan) && !names.contains(&svc.service) {
                    names.push(svc.service);
                }
            }
        }
        if names.is_empty() {
            println!("Nothing to remove.");
            return Ok(());
        }
        names.sort();
        confirm_bulk(&names, purge, yes, dry_run)?;
        (names, purge)
    } else {
        (services.to_vec(), purge)
    };

    // With `-a` or `--orphans`, the bulk prompt ran once up front. With
    // `-y` or `--dry-run`, we don't prompt at all. Otherwise prompt
    // per service.
    let skip_prompt = all || orphans || yes || dry_run;

    // Serialize concurrent removals so two processes don't clobber each
    // other's edits to authelia's configuration.yml when unregistering
    // OIDC clients. Matches the lock acquired in `add.rs::run`.
    let _auth_lock = if !dry_run {
        paths.ensure_dirs()?;
        let lock_path = paths.config_dir.join(".authelia-oidc.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        file.lock()?;
        Some(file)
    } else {
        None
    };

    for service in &targets {
        remove_one(service, effective_purge, skip_prompt, dry_run).await?;
    }
    Ok(())
}

/// Remove a single service. Handles both the "installed" path (stops +
/// deregisters via `remove_service`) and the "orphan + purge" path
/// (wipes leftover home dir + volumes directly).
async fn remove_one(service: &str, purge: bool, skip_prompt: bool, dry_run: bool) -> Result<()> {
    // Quadlet directory is the source of truth: if the marker'd
    // `.container` is present, the service is installed.
    let is_installed = ryra_core::is_service_installed(service);

    if is_installed {
        // Snapshot what preserve-mode would leave behind BEFORE anything
        // runs. After finalize_remove the service entry is gone and the
        // home dir may be gone too, which breaks volume→service owner
        // inference — enumerate_service would return None even though
        // the volumes still exist on disk.
        let preserved_snapshot = preserved_items(service);

        // Without --purge, if there's actually data on disk and we can
        // prompt, offer to upgrade to purge instead of leaving the user
        // to re-run with --purge.
        let effective_purge = purge
            || (!skip_prompt
                && !preserved_snapshot.is_empty()
                && super::is_interactive()
                && ask_purge_upgrade(service, &preserved_snapshot)?);

        let mode = if effective_purge {
            ryra_core::RemoveMode::Purge
        } else {
            ryra_core::RemoveMode::Preserve
        };
        let result = ryra_core::remove_service(service, mode)?;

        let preserved = if matches!(mode, ryra_core::RemoveMode::Preserve) {
            preserved_snapshot
        } else {
            Vec::new()
        };

        let tailnet_disable = result
            .steps
            .iter()
            .any(|s| matches!(s, Step::TailscaleDisable { .. }));

        if !skip_prompt {
            prompt_installed(service, mode, &preserved, tailnet_disable)?;
        }

        if dry_run {
            super::print_dry_run(&result.steps);
        } else {
            println!("Removing {service}...");
            apply::execute_all(&result.steps).await?;
            ryra_core::finalize_remove(&result.service_name)?;
            super::remove_hosts_entries(service);
            if ryra_core::WellKnownService::Caddy.matches(service) {
                super::remove_caddy_ca();
            }
            print_installed_tail(service, mode, &preserved)?;
        }
        return Ok(());
    }

    // Not installed — orphan path. Without --purge there's nothing to do
    // (the service is already deregistered) but if leftover data exists
    // and we can prompt, offer to wipe it instead of erroring out.
    let svc = ryra_core::data::enumerate_service(service)?;
    if !purge {
        match &svc {
            None => anyhow::bail!("no service named '{service}'"),
            Some(s) => {
                if skip_prompt || !super::is_interactive() {
                    anyhow::bail!(
                        "'{service}' is already removed but still has data. Run `ryra remove {service} --purge` to wipe it."
                    );
                }
                if !ask_orphan_purge_upgrade(s)? {
                    return Ok(());
                }
            }
        }
    }

    // Orphan + purge: purge its leftover data.
    let svc = svc.ok_or_else(|| anyhow::anyhow!("no service or leftover data for '{service}'"))?;
    let steps = ryra_core::orphan_purge_steps(&svc);
    if steps.is_empty() {
        println!("{service}: nothing to purge.");
        return Ok(());
    }
    if !skip_prompt {
        prompt_orphan(&svc)?;
    }
    if dry_run {
        super::print_dry_run(&steps);
    } else {
        println!("Purging {service}...");
        apply::execute_all(&steps).await?;
        // A killed `ryra add` can leave a config entry with `installed = false`
        // *and* data on disk. The orphan branch handles the data; this drops
        // the stale entry so `ryra list -a` doesn't keep showing the service.
        // No-op when there's no matching entry (orphan with no stale row).
        ryra_core::finalize_remove(service)?;
        println!("\n{service} purged.");
    }
    Ok(())
}

/// Without `--purge`, ryra preserves data and points users at
/// `--purge` to wipe it later. Surfacing the option here saves a
/// re-run when they wanted it gone in the first place. The
/// type-the-name confirm still gates the destruction.
fn ask_purge_upgrade(service: &str, preserved: &[String]) -> Result<bool> {
    println!("'{service}' has data that would be preserved:");
    for line in preserved {
        println!("  {line}");
    }
    let upgrade = Confirm::new()
        .with_prompt("Also delete this data?")
        .default(false)
        .interact()?;
    println!();
    Ok(upgrade)
}

/// Same idea for orphan data: rather than bail with "re-run with
/// --purge", offer the upgrade in-place. The type-the-name confirm in
/// `prompt_orphan` still gates the destruction.
fn ask_orphan_purge_upgrade(svc: &ServiceData) -> Result<bool> {
    println!("'{}' is already removed but still has data:", svc.service);
    for p in &svc.data_paths {
        println!("  {}", p.display());
    }
    if svc.home_dir.exists() && !svc.data_paths.iter().any(|p| p == &svc.home_dir) {
        println!("  {}", svc.home_dir.display());
    }
    for v in &svc.volumes {
        println!("  volume:{}", v.name);
    }
    let upgrade = Confirm::new()
        .with_prompt("Wipe this data?")
        .default(false)
        .interact()?;
    println!();
    Ok(upgrade)
}

fn prompt_installed(
    service: &str,
    mode: ryra_core::RemoveMode,
    preserved: &[String],
    tailnet_disable: bool,
) -> Result<()> {
    if !super::is_interactive() {
        anyhow::bail!("use --yes (-y) to confirm removal in non-interactive mode");
    }
    let home_dir = ryra_core::service_home(service)?;
    println!("This will:");
    println!("  - Stop and remove {service}");
    if tailnet_disable {
        println!("  - Remove {service} from your tailnet (deregister via Tailscale Admin API)");
    }
    match mode {
        ryra_core::RemoveMode::Purge => {
            println!("  - Delete ALL data and config at {}", home_dir.display());
            println!("  - Remove any podman named volumes for this service");
        }
        ryra_core::RemoveMode::Preserve => {
            println!("  - Delete config + .env at {}", home_dir.display());
            // Services like twenty keep every byte in podman named
            // volumes and leave the home dir empty after preserve-
            // remove — the old copy told users "data preserved at
            // <empty-dir>" and they'd wonder where it went.
            if preserved.is_empty() {
                println!("  - (nothing else to preserve — this service stores no data)");
            } else {
                println!("  - Preserve:");
                for line in preserved {
                    println!("      {line}");
                }
                println!("    (run `ryra remove {service} --purge` later to delete)");
            }
        }
    }
    println!();
    let input: String = Input::new()
        .with_prompt(format!("Type \"{service}\" to confirm"))
        .interact_text()?;
    if input != *service {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

/// Human-readable list of what `preserve`-mode removal will leave
/// behind: classified data paths under the home dir plus any podman
/// named volumes. Returns an empty vec for services that live entirely
/// in their `.env`/config.
fn preserved_items(service: &str) -> Vec<String> {
    let Ok(Some(svc)) = ryra_core::data::enumerate_service(service) else {
        return Vec::new();
    };
    let mut items: Vec<String> = svc
        .data_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    for v in &svc.volumes {
        items.push(format!("volume:{}", v.name));
    }
    items
}

fn prompt_orphan(svc: &ServiceData) -> Result<()> {
    if !super::is_interactive() {
        anyhow::bail!("use --yes (-y) to confirm in non-interactive mode");
    }
    println!("This will purge leftover data for '{}':", svc.service);
    for p in &svc.data_paths {
        println!("  {}", p.display());
    }
    if svc.home_dir.exists() {
        println!("  {}", svc.home_dir.display());
    }
    for v in &svc.volumes {
        println!("  volume:{}", v.name);
    }
    println!();
    let input: String = Input::new()
        .with_prompt(format!("Type \"{}\" to confirm", svc.service))
        .interact_text()?;
    if input != svc.service {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

fn confirm_bulk(names: &[String], purge: bool, yes: bool, dry_run: bool) -> Result<()> {
    if yes || dry_run {
        return Ok(());
    }
    if !super::is_interactive() {
        anyhow::bail!("use --yes (-y) to confirm in non-interactive mode");
    }
    println!("This will affect {} service(s):", names.len());
    for n in names {
        println!("  {n}");
    }
    println!();
    let installed = ryra_core::list_installed().unwrap_or_default();
    let tailnet_count = names
        .iter()
        .filter(|n| {
            installed.iter().any(|s| {
                &s.name == *n && matches!(s.exposure, ryra_core::Exposure::Tailscale { .. })
            })
        })
        .count();
    if tailnet_count > 0 {
        let plural = if tailnet_count == 1 { "" } else { "s" };
        println!(
            "{tailnet_count} service{plural} on your tailnet — will be deregistered via Tailscale Admin API."
        );
        println!();
    }
    if purge {
        println!("Mode: --purge — every listed service AND its data/volumes will be wiped.");
    } else {
        println!("Mode: data-preserving — run `ryra remove -a --purge` later to wipe data.");
    }
    println!();
    let input: String = Input::new()
        .with_prompt("Type \"remove all\" to confirm")
        .interact_text()?;
    if input != "remove all" {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

fn print_installed_tail(
    service: &str,
    mode: ryra_core::RemoveMode,
    preserved: &[String],
) -> Result<()> {
    match mode {
        ryra_core::RemoveMode::Purge => {
            println!("\n{service} removed (purged).");
        }
        ryra_core::RemoveMode::Preserve => {
            println!();
            if preserved.is_empty() {
                println!("{service} removed. No data was preserved.");
            } else {
                println!("{service} removed. Preserved:");
                for line in preserved {
                    println!("  {line}");
                }
                println!("Run `ryra remove {service} --purge` to delete.");
            }
        }
    }
    Ok(())
}
