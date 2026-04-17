use anyhow::Result;
use dialoguer::Input;
use ryra_core::Step;
use ryra_core::data::{ServiceData, ServiceStatus, enumerate_all};

use super::apply;

pub async fn ls() -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let mut svcs = enumerate_all(&config)?;

    if svcs.is_empty() {
        println!("No service data found.");
        return Ok(());
    }

    // Order: Installed alphabetical, then Orphan alphabetical.
    svcs.sort_by(|a, b| {
        let a_key = (matches!(a.status, ServiceStatus::Orphan), &a.service);
        let b_key = (matches!(b.status, ServiceStatus::Orphan), &b.service);
        a_key.cmp(&b_key)
    });

    // Buffer the whole table: compute_total walks each volume's mountpoint
    // and can be slow. Emitting rows inline makes them trickle out one by
    // one, which feels janky. Collect everything first, then print once.
    let mut lines: Vec<String> = Vec::with_capacity(svcs.len() * 2 + 1);
    lines.push(format!(
        "{:<15} {:<10} {:<10} PATH + VOLUMES",
        "SERVICE", "STATUS", "SIZE"
    ));
    for svc in &svcs {
        lines.extend(format_service(svc));
    }
    println!("{}", lines.join("\n"));
    Ok(())
}

fn format_service(svc: &ServiceData) -> Vec<String> {
    let status = match svc.status {
        ServiceStatus::Installed => "installed",
        ServiceStatus::Orphan => "orphan",
    };
    // Total size: sum per-component sizes so a single unreadable component
    // (e.g. a subuid-owned volume mountpoint) doesn't abort the whole row.
    let size = match compute_total(svc) {
        Size::Bytes(b) => human_size(b),
        Size::Partial(b) => format!("{}+?", human_size(b)),
        Size::Unknown => "?".to_string(),
    };
    let first_path = svc.home_dir.display().to_string();
    let mut out = Vec::with_capacity(1 + svc.volumes.len());
    out.push(format!(
        "{:<15} {:<10} {:<10} {}",
        svc.service, status, size, first_path
    ));
    for v in &svc.volumes {
        out.push(format!("{:<15} {:<10} {:<10} volume:{}", "", "", "", v.name));
    }
    out
}

enum Size {
    /// Every component read cleanly.
    Bytes(u64),
    /// At least one component read cleanly; at least one could not.
    Partial(u64),
    /// No component could be read.
    Unknown,
}

fn compute_total(svc: &ServiceData) -> Size {
    use ryra_core::data::{dir_size_bytes, volumes::mountpoint_of};
    let mut total: u64 = 0;
    let mut any_ok = false;
    let mut any_err = false;
    for p in &svc.data_paths {
        match dir_size_bytes(p) {
            Ok(b) => {
                total += b;
                any_ok = true;
            }
            Err(_) => any_err = true,
        }
    }
    for v in &svc.volumes {
        let Some(mp) = mountpoint_of(&v.name) else {
            any_err = true;
            continue;
        };
        match dir_size_bytes(&mp) {
            Ok(b) => { total += b; any_ok = true; }
            Err(_) => any_err = true,
        }
    }
    match (any_ok, any_err) {
        (true, false) => Size::Bytes(total),
        (true, true) => Size::Partial(total),
        (false, true) => Size::Unknown,
        // No data_paths and no volumes — entry exists in ryra.toml but neither
        // a home dir nor any volume remains (config out of sync with filesystem).
        (false, false) => Size::Bytes(0),
    }
}

fn human_size(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    const KB: u64 = 1_000;

    if bytes >= GB {
        let val = bytes as f64 / GB as f64;
        return format_three_sig_fig(val, "GB");
    }
    if bytes >= MB {
        let val = bytes as f64 / MB as f64;
        // Guard against rounding up into the next unit at display time.
        if val >= 999.5 {
            return format_three_sig_fig(bytes as f64 / GB as f64, "GB");
        }
        return format_three_sig_fig(val, "MB");
    }
    if bytes >= KB {
        let val = bytes as f64 / KB as f64;
        if val >= 999.5 {
            return format_three_sig_fig(bytes as f64 / MB as f64, "MB");
        }
        return format_three_sig_fig(val, "KB");
    }
    // Bytes: integer-only.
    format!("{bytes} B")
}

fn format_three_sig_fig(val: f64, unit: &str) -> String {
    if val >= 100.0 {
        format!("{val:.0} {unit}")
    } else if val >= 10.0 {
        format!("{val:.1} {unit}")
    } else {
        format!("{val:.2} {unit}")
    }
}

pub async fn rm(
    service: &str,
    yes: bool,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let svc = ryra_core::data::enumerate_service(&config, service)?
        .ok_or_else(|| anyhow::anyhow!("no data found for service '{service}'"))?;

    if matches!(svc.status, ServiceStatus::Installed) && !force {
        anyhow::bail!(
            "'{service}' is currently installed. Use `ryra rm {service} --purge` to remove it together with data, or pass `--force` to delete data only."
        );
    }

    // If running on an installed service with --force, stop its containers
    // first so we don't pull data out from under a running workload.
    let mut steps: Vec<Step> = Vec::new();
    if matches!(svc.status, ServiceStatus::Installed) && force {
        let quadlet_path = ryra_core::quadlet_dir()?;
        let all_names: Vec<&str> = config.services.iter().map(|s| s.name.as_str()).collect();
        if quadlet_path.is_dir()
            && let Ok(entries) = std::fs::read_dir(&quadlet_path)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".container")
                    && ryra_core::quadlet_belongs_to(&name, service, &all_names)
                {
                    let unit = name.trim_end_matches(".container").to_string();
                    steps.push(Step::StopService { unit });
                }
            }
        }
    }

    // Data path deletions.
    for path in &svc.data_paths {
        if path.is_dir() {
            steps.push(Step::RemoveDir(path.clone()));
        } else {
            steps.push(Step::RemoveFile(path.clone()));
        }
    }
    // Remove the home-dir shell so no empty directory lingers after the
    // data subdirs are gone. Matters for services like vikunja whose data
    // lives entirely in named volumes (data_paths is empty, but the home
    // dir exists as an empty shell after `ryra rm` stripped ephemerals).
    if svc.home_dir.exists() {
        steps.push(Step::RemoveDir(svc.home_dir.clone()));
    }
    // Volume deletions for volumes we attributed to this service.
    for v in &svc.volumes {
        steps.push(Step::RemoveVolume { name: v.name.clone() });
    }

    if steps.is_empty() {
        println!("{service}: nothing to delete.");
        return Ok(());
    }

    if !yes && !dry_run {
        if !super::is_interactive() {
            anyhow::bail!("use --yes (-y) to confirm in non-interactive mode");
        }
        println!("This will delete:");
        // Preview stop steps so the user knows containers will be halted.
        let stop_units: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                Step::StopService { unit } => Some(unit.as_str()),
                _ => None,
            })
            .collect();
        if !stop_units.is_empty() {
            println!("  (first, stop: {})", stop_units.join(", "));
        }
        for p in &svc.data_paths {
            let sz = match ryra_core::data::dir_size_bytes(p) {
                Ok(b) => human_size(b),
                Err(_) => "?".to_string(),
            };
            println!("  {} ({})", p.display(), sz);
        }
        for v in &svc.volumes {
            println!("  volume:{}", v.name);
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
        super::print_dry_run(&steps);
    } else {
        apply::execute_all(&steps).await?;
        println!("{service}: data removed.");
    }
    Ok(())
}

pub async fn rm_all(yes: bool, dry_run: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let in_config: std::collections::HashSet<String> =
        config.services.iter().map(|s| s.name.clone()).collect();
    let all = ryra_core::data::enumerate_all(&config)?;
    let orphans: Vec<_> = all
        .into_iter()
        .filter(|s| {
            // True orphans only: no ryra.toml entry. Services with
            // `installed: false` are mid-install-failure state; they
            // belong to `ryra rm --purge` / `ryra add` retry, not to
            // bulk sweeping.
            matches!(s.status, ServiceStatus::Orphan) && !in_config.contains(&s.service)
        })
        .collect();

    if orphans.is_empty() {
        println!("No orphan data to delete.");
        return Ok(());
    }

    let mut steps: Vec<Step> = Vec::new();
    for svc in &orphans {
        for path in &svc.data_paths {
            if path.is_dir() {
                steps.push(Step::RemoveDir(path.clone()));
            } else {
                steps.push(Step::RemoveFile(path.clone()));
            }
        }
        // Clean up the home-dir shell (see rm() comment for rationale).
        if svc.home_dir.exists() {
            steps.push(Step::RemoveDir(svc.home_dir.clone()));
        }
        for v in &svc.volumes {
            steps.push(Step::RemoveVolume { name: v.name.clone() });
        }
    }

    if !yes && !dry_run {
        if !super::is_interactive() {
            anyhow::bail!("use --yes (-y) to confirm in non-interactive mode");
        }
        println!("This will delete data for {} orphan service(s):", orphans.len());
        for svc in &orphans {
            let sz = size_or_unknown(svc);
            println!("  {} ({})", svc.service, sz);
        }
        println!();
        let input: String = Input::new()
            .with_prompt("Type \"delete all\" to confirm")
            .interact_text()?;
        if input != "delete all" {
            println!("Cancelled.");
            return Ok(());
        }
    }

    if dry_run {
        super::print_dry_run(&steps);
    } else {
        apply::execute_all(&steps).await?;
        println!("Deleted data for {} orphan service(s).", orphans.len());
    }
    Ok(())
}

/// Total size for a service's data + volumes, rendered to the same
/// `<size>` / `<size>+?` / `?` shape `ryra data ls` uses. Private
/// helper to keep `rm_all`'s confirm preview aligned with the listing.
fn size_or_unknown(svc: &ryra_core::data::ServiceData) -> String {
    match compute_total(svc) {
        Size::Bytes(b) => human_size(b),
        Size::Partial(b) => format!("{}+?", human_size(b)),
        Size::Unknown => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_ranges() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(1_500), "1.50 KB");
        assert_eq!(human_size(15_000), "15.0 KB");
        assert_eq!(human_size(150_000), "150 KB");
        assert_eq!(human_size(2_300_000_000), "2.30 GB");
    }

    #[test]
    fn human_size_boundaries() {
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(999_499_999), "999 MB");
        assert_eq!(human_size(999_500_000), "1.00 GB");
        assert_eq!(human_size(999_999_999), "1.00 GB");
        assert_eq!(human_size(1_000_000_000), "1.00 GB");
    }
}
