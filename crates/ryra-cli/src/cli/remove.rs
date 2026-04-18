use anyhow::Result;
use dialoguer::Input;
use ryra_core::Step;
use ryra_core::config::schema::Config;
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
    let config = ryra_core::config::load_or_default(&paths.config_file)?;

    // `--orphans` purges every orphan service (leftover data with no
    // config entry). Never touches installed services. `--purge` is
    // implied — orphans have nothing else to preserve.
    // `-a` expands to every installed service. With `--purge` it also
    // sweeps every orphan. `ryra reset` remains distinct — it
    // additionally wipes ryra's own config, CAs, and registry caches.
    let (targets, effective_purge) = if orphans {
        let names: Vec<String> = ryra_core::data::enumerate_all(&config)?
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
        let mut names: Vec<String> = config
            .services
            .iter()
            .filter(|s| s.installed)
            .map(|s| s.name.clone())
            .collect();
        if purge {
            for svc in ryra_core::data::enumerate_all(&config)? {
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
        remove_one(&config, service, effective_purge, skip_prompt, dry_run).await?;
    }
    Ok(())
}

/// Remove a single service. Handles both the "installed" path (stops +
/// deregisters via `remove_service`) and the "orphan + purge" path
/// (wipes leftover home dir + volumes directly).
async fn remove_one(
    config: &ryra_core::config::schema::Config,
    service: &str,
    purge: bool,
    skip_prompt: bool,
    dry_run: bool,
) -> Result<()> {
    let is_installed = config
        .services
        .iter()
        .any(|s| s.name == service && s.installed);

    if is_installed {
        let mode = if purge {
            ryra_core::RemoveMode::Purge
        } else {
            ryra_core::RemoveMode::Preserve
        };
        let result = ryra_core::remove_service(service, mode)?;

        // Snapshot what preserve-mode will leave behind BEFORE anything
        // runs. After finalize_remove the service entry is gone and the
        // home dir may be gone too, which breaks volume→service owner
        // inference — enumerate_service would return None even though
        // the volumes still exist on disk.
        let preserved = if matches!(mode, ryra_core::RemoveMode::Preserve) {
            preserved_items(config, service)
        } else {
            Vec::new()
        };

        if !skip_prompt {
            prompt_installed(service, mode, &preserved)?;
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
            print_installed_tail(service, mode, &preserved)?;
        }
        return Ok(());
    }

    // Not installed — treat as orphan. `ryra remove <orphan>` without
    // --purge has nothing to do (the service is already deregistered).
    // Tell the user how to finish cleanup.
    if !purge {
        let svc = ryra_core::data::enumerate_service(config, service)?;
        if svc.is_some() {
            anyhow::bail!(
                "'{service}' is already removed but still has data. Run `ryra remove {service} --purge` to wipe it."
            );
        }
        anyhow::bail!("no service named '{service}'");
    }

    // Orphan + --purge: purge its leftover data.
    let svc = ryra_core::data::enumerate_service(config, service)?
        .ok_or_else(|| anyhow::anyhow!("no service or leftover data for '{service}'"))?;
    let steps = orphan_purge_steps(&svc);
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
        println!("\n{service} purged.");
    }
    Ok(())
}

fn orphan_purge_steps(svc: &ServiceData) -> Vec<Step> {
    let mut steps = Vec::new();
    for path in &svc.data_paths {
        if path.is_dir() {
            steps.push(Step::RemoveDir(path.clone()));
        } else {
            steps.push(Step::RemoveFile(path.clone()));
        }
    }
    if svc.home_dir.exists() {
        steps.push(Step::RemoveDir(svc.home_dir.clone()));
    }
    for v in &svc.volumes {
        steps.push(Step::RemoveVolume {
            name: v.name.clone(),
        });
    }
    steps
}

fn prompt_installed(
    service: &str,
    mode: ryra_core::RemoveMode,
    preserved: &[String],
) -> Result<()> {
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
                println!(
                    "    (run `ryra remove {service} --purge` later to delete)"
                );
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
fn preserved_items(config: &Config, service: &str) -> Vec<String> {
    let Ok(Some(svc)) = ryra_core::data::enumerate_service(config, service) else {
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
