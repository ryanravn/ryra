//! Map podman named volumes to owning services.
//!
//! Authoritative source: `.volume` quadlet files in the user's
//! `~/.config/containers/systemd/` directory. Each `foo.volume` becomes
//! podman volume `systemd-foo`. Volume ownership is derived from the
//! filename prefix matching a known service name.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRef {
    /// Podman volume name as it appears in `podman volume ls` (includes
    /// the `systemd-` prefix quadlet adds).
    pub name: String,
    /// Absolute path to the `.volume` quadlet file, if we found one.
    /// `None` means the volume exists in podman but no quadlet claims
    /// it — treat as truly orphaned.
    pub quadlet_file: Option<PathBuf>,
    /// Service name the volume belongs to, if a match was found.
    /// `None` means the filename doesn't prefix-match any known service.
    pub owner: Option<String>,
}

/// Parse every `*.volume` quadlet file in `quadlet_dir` and compute its
/// podman name (`systemd-<stem>`) and owning service.
///
/// `known_services` is the set of service names from ryra.toml plus any
/// service dir under `~/.local/share/ryra/`. Pass both so we match
/// volumes of orphaned services (home dir exists, ryra.toml entry gone).
pub fn parse_volume_quadlets(
    quadlet_dir: &Path,
    known_services: &[String],
) -> Result<Vec<VolumeRef>> {
    let mut out = Vec::new();
    if !quadlet_dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(quadlet_dir).map_err(|source| Error::FileRead {
        path: quadlet_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::FileRead {
            path: quadlet_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("volume") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let owner = match_owner(&stem, known_services);
        out.push(VolumeRef {
            name: format!("systemd-{stem}"),
            quadlet_file: Some(path),
            owner,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Return the longest known service name that is a prefix of `stem`
/// (matching full tokens split by `-`). Handles cases like
/// `nextcloud-db-data` (owned by `nextcloud`, not `nextcloud-db`).
pub fn match_owner(stem: &str, known_services: &[String]) -> Option<String> {
    known_services
        .iter()
        .filter(|s| stem == s.as_str() || stem.starts_with(&format!("{s}-")))
        .max_by_key(|s| s.len())
        .cloned()
}

/// Call `podman volume ls --format '{{.Name}}'` and return names.
/// Returns an empty list if podman is not installed / fails; callers
/// should treat that as "no volumes to consider" rather than an error.
pub fn list_podman_volumes() -> Vec<String> {
    let out = std::process::Command::new("podman")
        .args(["volume", "ls", "--format", "{{.Name}}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Given the set of volumes that quadlet files claim + the set that
/// podman reports, return the union. Entries present in podman but
/// with no matching quadlet get `quadlet_file: None`.
pub fn reconcile(quadlet_refs: Vec<VolumeRef>, podman_names: Vec<String>) -> Vec<VolumeRef> {
    let mut out: Vec<VolumeRef> = quadlet_refs;
    let seen: std::collections::HashSet<String> = out.iter().map(|r| r.name.clone()).collect();
    for name in podman_names {
        if seen.contains(&name) {
            continue;
        }
        // Podman-only entries have no quadlet file and no owner here. The
        // caller holds the list of known services and is responsible for
        // attributing ownership via match_owner() after reconcile() returns.
        out.push(VolumeRef {
            name,
            quadlet_file: None,
            owner: None,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Query podman for a volume's on-disk mountpoint. Returns None if
/// the volume doesn't exist or podman isn't available.
pub fn mountpoint_of(volume_name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("podman")
        .args([
            "volume",
            "inspect",
            volume_name,
            "--format",
            "{{.Mountpoint}}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Compute a volume's on-disk size in bytes.
///
/// Rootless podman stores volumes under subuid-owned directories the
/// host user can't stat — a direct filesystem walk returns `EACCES`.
/// `podman unshare` enters the user's subuid namespace where the walk
/// succeeds. `du -sb` is POSIX-portable (every supported distro ships
/// coreutils).
///
/// Returns `None` if podman or `du` isn't available, the volume
/// doesn't exist, or the output can't be parsed.
pub fn volume_size_bytes(volume_name: &str) -> Option<u64> {
    let mp = mountpoint_of(volume_name)?;
    let out = std::process::Command::new("podman")
        .args(["unshare", "du", "-sb", "--"])
        .arg(&mp)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `du -sb` prints `<bytes>\t<path>\n`
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.split_whitespace().next()?.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn match_owner_exact_and_prefix() {
        let known = vec!["nextcloud".into(), "ente".into(), "caddy".into()];
        assert_eq!(
            match_owner("nextcloud", &known).as_deref(),
            Some("nextcloud")
        );
        assert_eq!(
            match_owner("nextcloud-db-data", &known).as_deref(),
            Some("nextcloud")
        );
        assert_eq!(
            match_owner("ente-minio-data", &known).as_deref(),
            Some("ente")
        );
        assert_eq!(match_owner("unrelated-vol", &known), None);
    }

    #[test]
    fn match_owner_longest_wins() {
        let known = vec!["nextcloud".into(), "nextcloud-db".into()];
        assert_eq!(
            match_owner("nextcloud-db-data", &known).as_deref(),
            Some("nextcloud-db")
        );
    }

    #[test]
    fn match_owner_no_false_prefix() {
        // "caddyfile" must not match service "caddy"
        let known = vec!["caddy".into()];
        assert_eq!(match_owner("caddyfile-data", &known), None);
    }

    #[test]
    fn parse_volume_quadlets_reads_volume_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("nextcloud-db-data.volume"), "[Volume]").unwrap();
        fs::write(dir.path().join("ente-minio-data.volume"), "[Volume]").unwrap();
        fs::write(dir.path().join("nextcloud.container"), "[Container]").unwrap(); // ignored

        let known = vec!["nextcloud".into(), "ente".into()];
        let vols = parse_volume_quadlets(dir.path(), &known).unwrap();
        assert_eq!(vols.len(), 2);
        assert_eq!(vols[0].name, "systemd-ente-minio-data");
        assert_eq!(vols[0].owner.as_deref(), Some("ente"));
        assert_eq!(vols[1].name, "systemd-nextcloud-db-data");
        assert_eq!(vols[1].owner.as_deref(), Some("nextcloud"));
        assert!(vols[0].quadlet_file.is_some());
    }
}

#[cfg(test)]
mod tests_reconcile {
    use super::*;

    #[test]
    fn reconcile_adds_podman_only_volumes() {
        let quadlet = vec![VolumeRef {
            name: "systemd-nextcloud-db-data".into(),
            quadlet_file: Some("/fake/nextcloud-db-data.volume".into()),
            owner: Some("nextcloud".into()),
        }];
        let podman = vec![
            "systemd-nextcloud-db-data".to_string(), // already known
            "systemd-ghost-volume".to_string(),      // unknown
        ];
        let merged = reconcile(quadlet, podman);
        assert_eq!(merged.len(), 2);
        let ghost = merged
            .iter()
            .find(|r| r.name == "systemd-ghost-volume")
            .unwrap();
        assert!(ghost.quadlet_file.is_none());
        assert!(ghost.owner.is_none());
    }
}
