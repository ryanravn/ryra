use anyhow::Result;
use ryra_core::data::{ServiceData, ServiceStatus, enumerate_all};

pub fn run(all: bool, long: bool) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let mut svcs = enumerate_all(&config)?;

    if svcs.is_empty() {
        println!("No services installed. Run `ryra search` to browse available services.");
        return Ok(());
    }

    // Order: Installed alphabetical, then Orphan alphabetical.
    svcs.sort_by(|a, b| {
        let a_key = (matches!(a.status, ServiceStatus::Orphan), &a.service);
        let b_key = (matches!(b.status, ServiceStatus::Orphan), &b.service);
        a_key.cmp(&b_key)
    });

    // Count orphans BEFORE filtering so we can hint about them when
    // they're hidden from the default view.
    let orphan_count = svcs
        .iter()
        .filter(|s| matches!(s.status, ServiceStatus::Orphan))
        .count();

    let visible: Vec<&ServiceData> = svcs
        .iter()
        .filter(|s| all || matches!(s.status, ServiceStatus::Installed))
        .collect();

    if visible.is_empty() {
        if orphan_count > 0 {
            println!(
                "No installed services. {orphan_count} orphan(s) with leftover data — use `ryra ls -a` to see."
            );
        } else {
            println!("No services installed. Run `ryra search` to browse available services.");
        }
        return Ok(());
    }

    let home = std::env::var("HOME").unwrap_or_default();
    if long {
        print_long(&visible, &home, all);
    } else {
        print_short(&visible, &home, all);
    }

    // Nudge about hidden orphans when the user ran the default view.
    if !all && orphan_count > 0 {
        println!();
        println!(
            "{orphan_count} orphan service(s) with leftover data — use `ryra ls -a` to see."
        );
    }
    Ok(())
}

/// Fast path: name [status] data path. STATUS column only appears when
/// orphans may be in the mix (i.e. `-a` was passed) — otherwise every
/// row would read `installed` and add nothing.
fn print_short(svcs: &[&ServiceData], home: &str, show_status: bool) {
    let mut lines: Vec<String> = Vec::with_capacity(svcs.len() + 1);
    if show_status {
        lines.push(format!("{:<15} {:<10} DATA", "SERVICE", "STATUS"));
    } else {
        lines.push(format!("{:<15} DATA", "SERVICE"));
    }
    for svc in svcs {
        let path = shorten_home(&svc.home_dir.display().to_string(), home);
        if show_status {
            let status = match svc.status {
                ServiceStatus::Installed => "installed",
                ServiceStatus::Orphan => "orphan",
            };
            lines.push(format!("{:<15} {:<10} {}", svc.service, status, path));
        } else {
            lines.push(format!("{:<15} {}", svc.service, path));
        }
    }
    println!("{}", lines.join("\n"));
}

/// Long path: adds SIZE column (and STATUS if `-a`), plus volume
/// sub-rows and a cleanup footer. Pays the ~250 ms cost of parallel
/// `podman unshare du` per volume.
fn print_long(svcs: &[&ServiceData], home: &str, show_status: bool) {
    // Pre-compute every volume's size in parallel (see prefetch_volume_sizes).
    let owned: Vec<ServiceData> = svcs.iter().map(|s| (*s).clone()).collect();
    let vol_sizes = prefetch_volume_sizes(&owned);

    let mut lines: Vec<String> = Vec::with_capacity(svcs.len() * 2 + 1);
    if show_status {
        lines.push(format!(
            "{:<15} {:<10} {:<10} DATA",
            "SERVICE", "STATUS", "SIZE"
        ));
    } else {
        lines.push(format!("{:<15} {:<10} DATA", "SERVICE", "SIZE"));
    }
    for svc in svcs {
        lines.extend(format_service(svc, home, &vol_sizes, show_status));
    }
    println!("{}", lines.join("\n"));
}

/// Spawn one OS thread per unique volume name and shell out to
/// `podman unshare du -sb` concurrently. Returns a map of
/// `volume_name -> Some(bytes)` on success, `volume_name -> None` when
/// the walk failed (volume missing, podman unavailable, subuid mismatch).
fn prefetch_volume_sizes(
    svcs: &[ServiceData],
) -> std::collections::HashMap<String, Option<u64>> {
    use ryra_core::data::volumes::volume_size_bytes;
    let mut names: Vec<String> = svcs
        .iter()
        .flat_map(|s| s.volumes.iter().map(|v| v.name.clone()))
        .collect();
    names.sort();
    names.dedup();
    std::thread::scope(|s| {
        let handles: Vec<_> = names
            .iter()
            .map(|n| {
                let n = n.clone();
                s.spawn(move || (n.clone(), volume_size_bytes(&n)))
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect()
    })
}

fn format_service(
    svc: &ServiceData,
    home: &str,
    vol_sizes: &std::collections::HashMap<String, Option<u64>>,
    show_status: bool,
) -> Vec<String> {
    // Total size: sum per-component sizes so a single unreadable component
    // (e.g. a subuid-owned volume mountpoint) doesn't abort the whole row.
    let size = match compute_total(svc, vol_sizes) {
        Size::Bytes(b) => human_size(b),
        Size::Partial(b) => format!("{}+?", human_size(b)),
        Size::Unknown => "?".to_string(),
    };
    let path = shorten_home(&svc.home_dir.display().to_string(), home);
    let mut out = Vec::with_capacity(1 + svc.volumes.len());
    if show_status {
        let status = match svc.status {
            ServiceStatus::Installed => "installed",
            ServiceStatus::Orphan => "orphan",
        };
        out.push(format!(
            "{:<15} {:<10} {:<10} {}",
            svc.service, status, size, path
        ));
        for v in &svc.volumes {
            out.push(format!(
                "{:<15} {:<10} {:<10} volume:{}",
                "", "", "", v.name
            ));
        }
    } else {
        out.push(format!("{:<15} {:<10} {}", svc.service, size, path));
        for v in &svc.volumes {
            out.push(format!("{:<15} {:<10} volume:{}", "", "", v.name));
        }
    }
    out
}

/// Replace the user's home-dir prefix with `~` for display. Keeps long
/// paths readable without hiding where they actually live.
fn shorten_home(path: &str, home: &str) -> String {
    if !home.is_empty()
        && let Some(rest) = path.strip_prefix(home)
    {
        format!("~{rest}")
    } else {
        path.to_string()
    }
}

enum Size {
    /// Every component read cleanly.
    Bytes(u64),
    /// At least one component read cleanly; at least one could not.
    Partial(u64),
    /// No component could be read.
    Unknown,
}

fn compute_total(
    svc: &ServiceData,
    vol_sizes: &std::collections::HashMap<String, Option<u64>>,
) -> Size {
    use ryra_core::data::dir_size_bytes;
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
        match vol_sizes.get(&v.name).copied().flatten() {
            Some(b) => {
                total += b;
                any_ok = true;
            }
            None => any_err = true,
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
