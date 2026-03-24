use std::path::Path;

/// Total physical RAM in megabytes, read from /proc/meminfo.
/// Returns `None` if the file cannot be read or parsed (e.g., non-Linux).
pub fn total_ram_mb() -> Option<u64> {
    total_ram_mb_from(Path::new("/proc/meminfo"))
}

fn total_ram_mb_from(path: &Path) -> Option<u64> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
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
        let result = total_ram_mb_from(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(result, Some(7860)); // 8048752 / 1024
    }

    #[test]
    fn returns_none_for_missing_file() {
        let result = total_ram_mb_from(Path::new("/nonexistent/meminfo"));
        assert_eq!(result, None);
    }
}
