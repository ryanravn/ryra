use std::path::Path;

/// Total physical RAM in megabytes, read from /proc/meminfo.
/// Returns `None` if the file cannot be read or parsed (e.g., non-Linux).
pub fn total_ram_mb() -> Option<u64> {
    meminfo_field_mb(Path::new("/proc/meminfo"), "MemTotal:")
}

/// Total swap in megabytes, read from /proc/meminfo. `Some(0)` means swap is
/// configured-but-empty *or* simply absent; `None` means the file is
/// unreadable (non-Linux). Callers treat `Some(0)` as "no cushion".
pub fn swap_total_mb() -> Option<u64> {
    meminfo_field_mb(Path::new("/proc/meminfo"), "SwapTotal:")
}

/// Parse a `<field>  <n> kB` line out of a /proc/meminfo-shaped file, in MB.
fn meminfo_field_mb(path: &Path, field: &str) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let rest = rest.trim();
            let kb_str = rest
                .strip_suffix("kB")
                .or_else(|| rest.strip_suffix("KB"))?
                .trim();
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo() {
        let dir = std::env::temp_dir().join("ryra-test-meminfo");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("meminfo");
        std::fs::write(
            &path,
            "MemTotal:        8048752 kB\nMemFree:         1234567 kB\n",
        )
        .unwrap();
        let result = meminfo_field_mb(&path, "MemTotal:");
        assert_eq!(result, Some(7860)); // 8048752 / 1024
        assert_eq!(meminfo_field_mb(&path, "SwapTotal:"), None); // absent
        std::fs::write(&path, "MemTotal: 100 kB\nSwapTotal:  4096000 kB\n").unwrap();
        assert_eq!(meminfo_field_mb(&path, "SwapTotal:"), Some(4000));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
