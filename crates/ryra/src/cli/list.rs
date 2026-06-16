use std::collections::{HashMap, HashSet};

use anyhow::Result;
use ryra_core::config::schema::InstalledService;
use ryra_core::data::{ServiceData, ServiceStatus, enumerate_all};

use super::style;

pub fn run(all: bool, long: bool, json: bool) -> Result<()> {
    let mut svcs = enumerate_all()?;

    // Machine-readable path: emit every service (installed + orphan) with a
    // status + url, regardless of -a/-l. Programmatic callers filter as needed.
    if json {
        return run_json(&svcs);
    }

    // Fast path when there's literally nothing to show. Short-circuits
    // the status probe + volume sizing.
    if svcs.is_empty() {
        println!("No services installed. Run `ryra search` to browse available services.");
        return Ok(());
    }

    // Installed (running/stopped) first, alphabetical. Then removed.
    svcs.sort_by(|a, b| {
        let a_key = (matches!(a.status, ServiceStatus::Orphan), &a.service);
        let b_key = (matches!(b.status, ServiceStatus::Orphan), &b.service);
        a_key.cmp(&b_key)
    });

    // `-l` always includes removed services — when you're asking for
    // sizes you're deciding what to purge, which is a superset of
    // what's currently running.
    let show_removed = all || long;

    let removed_count = svcs
        .iter()
        .filter(|s| matches!(s.status, ServiceStatus::Orphan))
        .count();

    let visible: Vec<&ServiceData> = svcs
        .iter()
        .filter(|s| show_removed || matches!(s.status, ServiceStatus::Installed))
        .collect();

    if visible.is_empty() {
        if removed_count > 0 {
            println!(
                "No installed services. {removed_count} removed service(s) with preserved data — use `ryra list -a` to see."
            );
        } else {
            println!("No services installed. Run `ryra search` to browse available services.");
        }
        return Ok(());
    }

    // Lookup the InstalledService entry for each visible service (if any —
    // removed services won't have one). Drives URL + port derivation.
    // Reads via `list_installed()` so quadlet headers are the source of
    // truth: even when preferences.toml is missing or stale, services
    // still show their full URL/exposure/auth via the metadata stamped
    // on the main `.container` file at install time.
    let installed_full = ryra_core::list_installed().unwrap_or_default();
    let by_name: HashMap<&str, &InstalledService> = installed_full
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    // One subprocess for every service's systemd state — cheaper than N
    // `systemctl is-active` calls.
    let active_units = active_user_units();

    let home = std::env::var("HOME").unwrap_or_default();
    if long {
        print_long(&visible, &by_name, &active_units, &home);
    } else {
        print_short(&visible, &by_name, &active_units);
    }

    // Nudge about hidden removed services when running the default view.
    if !show_removed && removed_count > 0 {
        println!();
        println!(
            "{removed_count} removed service(s) with preserved data — `ryra list -a` to see, `ryra remove <name> --purge` to delete."
        );
    }
    Ok(())
}

/// One service in the machine-readable `--json` output.
#[derive(serde::Serialize)]
struct JsonService {
    name: String,
    /// "running" | "stopped" | "removed".
    status: &'static str,
    url: Option<String>,
}

/// Emit every service as JSON. Mirrors the human table's status + url, but as
/// structured data with `null` (not the "no url" dash) when there's no URL.
fn run_json(svcs: &[ServiceData]) -> Result<()> {
    let installed_full = ryra_core::list_installed().unwrap_or_default();
    let by_name: HashMap<&str, &InstalledService> = installed_full
        .iter()
        .map(|s| (s.name.as_str(), s))
        .collect();
    let active = active_user_units();

    let out: Vec<JsonService> = svcs
        .iter()
        .map(|svc| {
            let installed = by_name.get(svc.service.as_str()).copied();
            let status = if matches!(svc.status, ServiceStatus::Orphan) {
                "removed"
            } else if active.contains(&svc.service) {
                "running"
            } else {
                "stopped"
            };
            JsonService {
                name: svc.service.clone(),
                status,
                url: service_url(installed),
            }
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// The service's URL as `Option`, deriving it the same way the human table's
/// `url_for` does (exposure URL, then the http port, then the lowest port) but
/// returning `None` instead of a display dash.
fn service_url(installed: Option<&InstalledService>) -> Option<String> {
    let entry = installed?;
    if let Some(url) = entry.exposure.url() {
        return Some(url.to_string());
    }
    if let Some(http_port) = entry.ports.get("http") {
        return Some(format!("http://127.0.0.1:{http_port}"));
    }
    let mut ports: Vec<(&String, &u16)> = entry.ports.iter().collect();
    ports.sort_by_key(|(_, p)| *p);
    ports.first().map(|(_, port)| format!("127.0.0.1:{port}"))
}

fn print_short(
    svcs: &[&ServiceData],
    by_name: &HashMap<&str, &InstalledService>,
    active: &HashSet<String>,
) {
    let name_w = svcs
        .iter()
        .map(|s| s.service.len())
        .max()
        .unwrap_or(7)
        .max(7);
    println!("{:<name_w$} {:<8}  URL", "SERVICE", "STATUS");
    for svc in svcs {
        let installed = by_name.get(svc.service.as_str()).copied();
        let status = style::list_status(&svc.status, active.contains(&svc.service), 8);
        let url = url_for(svc, installed);
        println!("{:<name_w$} {}  {}", svc.service, status, url);
    }
}

fn print_long(
    svcs: &[&ServiceData],
    by_name: &HashMap<&str, &InstalledService>,
    active: &HashSet<String>,
    home: &str,
) {
    // Pre-fetch volume sizes in parallel (`podman unshare du` per volume).
    let owned: Vec<ServiceData> = svcs.iter().map(|s| (*s).clone()).collect();
    let vol_sizes = prefetch_volume_sizes(&owned);

    let name_w = svcs
        .iter()
        .map(|s| s.service.len())
        .max()
        .unwrap_or(7)
        .max(7);
    // URL width — cap at 45 so a long --url doesn't push SIZE/STORAGE
    // off the screen; overly long URLs just wrap softly.
    let url_w = svcs
        .iter()
        .map(|s| url_for(s, by_name.get(s.service.as_str()).copied()).len())
        .max()
        .unwrap_or(3)
        .clamp(3, 45);
    let size_w = 8;
    println!(
        "{:<name_w$} {:<8}  {:<url_w$}  {:<size_w$}  STORAGE",
        "SERVICE", "STATUS", "URL", "SIZE"
    );
    for svc in svcs {
        let installed = by_name.get(svc.service.as_str()).copied();
        let status = style::list_status(&svc.status, active.contains(&svc.service), 8);
        let url = url_for(svc, installed);
        let size = match compute_total(svc, &vol_sizes) {
            Size::Bytes(b) => human_size(b),
            Size::Partial(b) => format!("{}+?", human_size(b)),
            Size::Unknown => "?".to_string(),
        };
        let storage = storage_label(svc, home);
        println!(
            "{:<name_w$} {}  {:<url_w$}  {:<size_w$}  {}",
            svc.service, status, url, size, storage
        );
    }
}

/// Resolve the primary URL for a service:
///   1. `--url` set → use it verbatim.
///   2. port named "http" → `http://127.0.0.1:<port>`.
///   3. any other port → `127.0.0.1:<port>` (no scheme — postgres, …).
///   4. nothing → `—`.
fn url_for(svc: &ServiceData, installed: Option<&InstalledService>) -> String {
    let Some(entry) = installed else {
        return "—".to_string();
    };
    if let Some(url) = entry.exposure.url() {
        return url.to_string();
    }
    if let Some(http_port) = entry.ports.get("http") {
        return format!("http://127.0.0.1:{http_port}");
    }
    // Pick the lowest-valued other port for determinism across runs.
    let mut ports: Vec<(&String, &u16)> = entry.ports.iter().collect();
    ports.sort_by_key(|(_, p)| *p);
    if let Some((_, port)) = ports.first() {
        return format!("127.0.0.1:{port}");
    }
    // Avoid noisy empty column for the (uncommon) no-ports service.
    let _ = svc;
    "—".to_string()
}

/// What's actually on disk for this service: "home" if the service
/// bind-mounts into `~/.local/share/services/<svc>/`, "N volume(s)" for
/// podman named volumes, or both joined with ` + `.
fn storage_label(svc: &ServiceData, home: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !svc.data_paths.is_empty() {
        parts.push(shorten_home(&svc.home_dir.display().to_string(), home));
    }
    let n = svc.volumes.len();
    match n {
        0 => {}
        1 => parts.push("1 volume".to_string()),
        _ => parts.push(format!("{n} volumes")),
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" + ")
    }
}

/// One `systemctl --user list-units` call returns every active unit on
/// the user manager. Faster than N `is-active` probes when `ryra list`
/// covers a dozen services.
pub(crate) fn active_user_units() -> HashSet<String> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--type=service",
            "--state=active",
            "--no-legend",
            "--plain",
            "--no-pager",
        ])
        .output();
    let Ok(out) = out else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|unit| unit.strip_suffix(".service"))
        .map(|s| s.to_string())
        .collect()
}

fn prefetch_volume_sizes(svcs: &[ServiceData]) -> HashMap<String, Option<u64>> {
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
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    })
}

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
    Bytes(u64),
    Partial(u64),
    Unknown,
}

fn compute_total(svc: &ServiceData, vol_sizes: &HashMap<String, Option<u64>>) -> Size {
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
