//! Per-service data enumeration for `ryra list` + the
//! data-preserving variant of `ryra remove`.

pub mod classify;
pub mod volumes;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Service is present in preferences.toml with installed=true.
    Installed,
    /// Service has data but preferences.toml has no entry (or installed=false).
    Orphan,
}

#[derive(Debug, Clone)]
pub struct ServiceData {
    pub service: String,
    pub status: ServiceStatus,
    /// `~/.local/share/services/<service>/` — may not exist if only volumes remain.
    pub home_dir: PathBuf,
    /// Top-level children of `home_dir` classified as data (not ephemeral).
    pub data_paths: Vec<PathBuf>,
    pub volumes: Vec<volumes::VolumeRef>,
}

/// Top-level dirs under `~/.local/share/services/` that are NOT services —
/// written by ryra itself for tooling (e.g. test reports). Skip them so
/// `ryra data ls` doesn't surface them as orphan services.
const NON_SERVICE_DIRS: &[&str] = &["test-reports"];

/// Walk every ryra-visible service and return one `ServiceData` per service.
pub fn enumerate_all() -> Result<Vec<ServiceData>> {
    let home_root = crate::service_data_root()?;
    let quadlet = crate::quadlet_dir()?;

    // Candidate service names: every quadlet with our `# Service-Source:`
    // marker + every dir under the data root. The marker scan is the
    // authoritative source for "installed"; data-root dirs catch
    // orphan data (services that were removed in Preserve mode and
    // still have a home dir or volumes lying around).
    let managed_via_marker: std::collections::HashSet<String> =
        crate::scan_managed_services()
            .unwrap_or_default()
            .into_iter()
            .collect();
    let mut names: std::collections::BTreeSet<String> =
        managed_via_marker.iter().cloned().collect();
    if home_root.is_dir() {
        let entries = std::fs::read_dir(&home_root).map_err(|source| Error::FileRead {
            path: home_root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::FileRead {
                path: home_root.clone(),
                source,
            })?;
            if entry
                .file_type()
                .map_err(|source| Error::FileRead {
                    path: entry.path(),
                    source,
                })?
                .is_dir()
                && let Some(n) = entry.file_name().to_str()
                && !NON_SERVICE_DIRS.contains(&n)
            {
                names.insert(n.to_string());
            }
        }
    }

    let known: Vec<String> = names.iter().cloned().collect();
    let quadlet_vols = volumes::parse_volume_quadlets(&quadlet, &known)?;
    let podman_vols = volumes::list_podman_volumes();
    let mut all_vols = volumes::reconcile(quadlet_vols, podman_vols);
    // Owner inference for podman-only volumes (quadlet parse couldn't see them).
    for vr in &mut all_vols {
        if vr.owner.is_none() {
            let stem = vr.name.strip_prefix("systemd-").unwrap_or(&vr.name);
            vr.owner = volumes::match_owner(stem, &known);
        }
    }

    // Any volume with an owner that is NOT in names → add the owner as a service candidate.
    for vr in &all_vols {
        if let Some(owner) = &vr.owner {
            names.insert(owner.clone());
        }
    }

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        // Marker present → installed. Marker absent but home dir or
        // volumes still around → orphan (typically left by a Preserve
        // mode `ryra remove`, awaiting `--purge`).
        let status = if managed_via_marker.contains(&name) {
            ServiceStatus::Installed
        } else {
            ServiceStatus::Orphan
        };
        let home_dir = home_root.join(&name);
        let data_paths = if home_dir.exists() {
            classify::classify_home_dir(&home_dir)?.0
        } else {
            Vec::new()
        };
        let svc_vols: Vec<volumes::VolumeRef> = all_vols
            .iter()
            .filter(|v| v.owner.as_deref() == Some(name.as_str()))
            .cloned()
            .collect();
        // Skip entries with no home dir AND no volumes (happens when a name
        // slipped in but has nothing associated; shouldn't occur in practice).
        if !home_dir.exists() && svc_vols.is_empty() {
            continue;
        }
        out.push(ServiceData {
            service: name,
            status,
            home_dir,
            data_paths,
            volumes: svc_vols,
        });
    }
    Ok(out)
}

/// Look up a single service. Queries the filesystem + podman directly for
/// the given name rather than walking every service via `enumerate_all`.
///
/// This matters for true orphans: after `ryra remove <svc>` in Preserve
/// mode the config entry is gone AND the home dir is often deleted (empty
/// once .env is wiped), so `enumerate_all`'s owner inference has no
/// `known_services` hint to match `systemd-<svc>-data` against and those
/// volumes end up unattributed. Looking up by name dodges that because
/// the name itself seeds the owner match.
pub fn enumerate_service(name: &str) -> Result<Option<ServiceData>> {
    let home_root = crate::service_data_root()?;
    let quadlet = crate::quadlet_dir()?;
    let home_dir = home_root.join(name);
    let known = [name.to_string()];

    let quadlet_vols = volumes::parse_volume_quadlets(&quadlet, &known)?;
    let podman_vols = volumes::list_podman_volumes();
    let mut all_vols = volumes::reconcile(quadlet_vols, podman_vols);
    for vr in &mut all_vols {
        if vr.owner.is_none() {
            let stem = vr.name.strip_prefix("systemd-").unwrap_or(&vr.name);
            vr.owner = volumes::match_owner(stem, &known);
        }
    }
    let svc_vols: Vec<volumes::VolumeRef> = all_vols
        .into_iter()
        .filter(|v| v.owner.as_deref() == Some(name))
        .collect();

    let data_paths = if home_dir.exists() {
        classify::classify_home_dir(&home_dir)?.0
    } else {
        Vec::new()
    };

    if !home_dir.exists() && svc_vols.is_empty() {
        return Ok(None);
    }

    let status = if crate::is_service_installed(name) {
        ServiceStatus::Installed
    } else {
        ServiceStatus::Orphan
    };

    Ok(Some(ServiceData {
        service: name.to_string(),
        status,
        home_dir,
        data_paths,
        volumes: svc_vols,
    }))
}

/// Walk `path` recursively, summing file sizes. Does not follow symlinks.
/// Returns 0 if the path does not exist.
pub fn dir_size_bytes(path: &Path) -> Result<u64> {
    let root_meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(Error::FileRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    // Caller passed a symlink root — don't follow it.
    if root_meta.file_type().is_symlink() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let meta = std::fs::symlink_metadata(&p).map_err(|source| Error::FileRead {
            path: p.clone(),
            source,
        })?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_file() {
            total += meta.len();
        } else if meta.is_dir() {
            let entries = std::fs::read_dir(&p).map_err(|source| Error::FileRead {
                path: p.clone(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| Error::FileRead {
                    path: p.clone(),
                    source,
                })?;
                stack.push(entry.path());
            }
        }
    }
    Ok(total)
}

/// Sum of all data paths + all volume mountpoints for a service.
pub fn size_bytes(data: &ServiceData) -> Result<u64> {
    let mut total = 0;
    for p in &data.data_paths {
        total += dir_size_bytes(p)?;
    }
    for v in &data.volumes {
        if let Some(mp) = volumes::mountpoint_of(&v.name) {
            total += dir_size_bytes(&mp)?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_size_sums_file_sizes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.bin"), vec![0u8; 250]).unwrap();
        assert_eq!(dir_size_bytes(dir.path()).unwrap(), 350);
    }

    #[test]
    fn dir_size_missing_path_is_zero() {
        assert_eq!(
            dir_size_bytes(std::path::Path::new("/nonexistent-xyz-789")).unwrap(),
            0
        );
    }

    #[test]
    fn dir_size_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.bin"), vec![0u8; 200]).unwrap();
        // A dangling symlink inside the dir — must not cause a read error
        // and must not be counted.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nonexistent-target", dir.path().join("link")).unwrap();
        assert_eq!(dir_size_bytes(dir.path()).unwrap(), 200);
    }
}
