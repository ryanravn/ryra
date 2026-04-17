use anyhow::Result;

use ryra_core::data::{ServiceData, ServiceStatus, enumerate_all};

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

    println!("{:<15} {:<10} {:<10} PATH + VOLUMES", "SERVICE", "STATUS", "SIZE");
    for svc in &svcs {
        print_service(svc)?;
    }
    Ok(())
}

fn print_service(svc: &ServiceData) -> Result<()> {
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
    println!("{:<15} {:<10} {:<10} {}", svc.service, status, size, first_path);
    for v in &svc.volumes {
        println!("{:<15} {:<10} {:<10} volume:{}", "", "", "", v.name);
    }
    Ok(())
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
            continue;
        };
        match dir_size_bytes(&mp) {
            Ok(b) => {
                total += b;
                any_ok = true;
            }
            Err(_) => any_err = true,
        }
    }
    match (any_ok, any_err) {
        (true, false) => Size::Bytes(total),
        (true, true) => Size::Partial(total),
        (false, true) => Size::Unknown,
        // No volumes AND no data paths — orphan with nothing left; report 0.
        (false, false) => Size::Bytes(0),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("KB", 1_000),
        ("B", 1),
    ];
    for (unit, div) in UNITS {
        if bytes >= div {
            let val = bytes as f64 / div as f64;
            return if val >= 100.0 {
                format!("{val:.0} {unit}")
            } else if val >= 10.0 {
                format!("{val:.1} {unit}")
            } else {
                format!("{val:.2} {unit}")
            };
        }
    }
    "0 B".to_string()
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
    fn size_formatting_variants() {
        // Direct checks on human_size for the partial display shape.
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1_500_000), "1.50 MB");
    }
}
