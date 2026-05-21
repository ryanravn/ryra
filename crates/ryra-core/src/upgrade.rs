//! Diff and upgrade flows for already-installed services.
//!
//! "Upgrade" means: re-render an installed service's quadlet + configs
//! against the current registry, replace any files whose content changed,
//! and restart the unit. The render path is shared with `add_service`
//! (driven via [`PlanMode::Upgrade`]); the side-effect steps differ.
//!
//! Drift detection is grounded in `service.manifest` — the per-install render
//! manifest written by `ryra add`. Each tracked file is in one of these
//! states:
//!
//! - **Unchanged**: on-disk content matches what the registry would render.
//! - **Modified**: registry rendered output differs, but on-disk hash still
//!   matches the manifest, so we know the file is ours and can be safely
//!   overwritten.
//! - **Drift**: on-disk hash matches *neither* the manifest nor the planned
//!   content — i.e. the user hand-edited it. Refused without `--force`.
//! - **Added**: file is in the planned set but not in the manifest (registry
//!   added it).
//! - **Removed**: file is in the manifest but not in the planned set (registry
//!   stopped shipping it).
//!
//! `.env` is excluded throughout: it carries generated secrets that legitimately
//! drift across restarts, and re-rendering it on upgrade would clobber rotated
//! credentials. Its absence from the manifest is the source of truth for that.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::exposure::Exposure;
use crate::generate::GeneratedFile;
use crate::manifest;
use crate::metadata::load_metadata;
use crate::registry::resolve::ServiceRef;
use crate::{
    AddResult, PlanMode, REGISTRY_BUNDLED, Step, add_service, is_service_installed,
    resolve_registry_dir, service_home,
};

/// Per-file diff classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    /// On-disk content matches the planned render. Nothing to do.
    Unchanged,
    /// Registry now renders different content. On-disk hash still matches
    /// the manifest, so the file is ryra-owned and safe to overwrite.
    Modified,
    /// On-disk hash differs from both the manifest and the planned render —
    /// the user hand-edited this file. Upgrade refuses without `--force`.
    /// Includes the case where there is no manifest entry to compare against
    /// (service installed before the manifest feature; treated conservatively
    /// as drift until the user confirms with `--force`).
    Drift,
    /// File is in the planned render but absent from the manifest — registry
    /// added it.
    Added,
    /// File is in the manifest but no longer rendered by the registry —
    /// registry stopped shipping it. Upgrade deletes it.
    Removed,
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: PathBuf,
    pub kind: DiffKind,
}

/// One env var the registry expects in `.env` that the user's `.env`
/// doesn't have. By design env tracking is *append-only* — we never flag
/// a present-but-different value as drift, and we never propose
/// removing a key. Users may have manually edited values or added their
/// own keys; clobbering those would be the larger harm.
///
/// `kind` and `prompt` come straight from the registry's `EnvVar`
/// definition, so the CLI can route Prompted / Required additions
/// through the same interactive prompt that `ryra add` uses, while
/// silently appending Default ones.
#[derive(Debug, Clone)]
pub struct EnvAddition {
    pub key: String,
    pub value: String,
    pub kind: crate::registry::service_def::EnvKind,
    pub prompt: Option<String>,
}

/// Result of comparing the registry's render to what's on disk.
#[derive(Debug, Clone)]
pub struct DiffResult {
    pub service: String,
    pub entries: Vec<DiffEntry>,
    /// Static env vars the registry expects but the user's `.env` is
    /// missing. Empty when the `.env` already covers everything tracked.
    pub env_additions: Vec<EnvAddition>,
}

impl DiffResult {
    /// True when nothing about the install would change — neither files
    /// nor env vars.
    pub fn is_clean(&self) -> bool {
        self.entries
            .iter()
            .all(|e| matches!(e.kind, DiffKind::Unchanged))
            && self.env_additions.is_empty()
    }

    /// Files the user hand-edited. Upgrade must refuse to overwrite these
    /// without `--force`.
    pub fn drifted(&self) -> Vec<&DiffEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, DiffKind::Drift))
            .collect()
    }
}

/// Reconstruct the planning inputs we stashed at install time and feed them
/// back through `add_service` in upgrade mode. Returns the planned step
/// list and the planned-file content map (path → content). The richer
/// per-env metadata lives on `AddResult.tracked_envs`.
async fn replan(service_name: &str) -> Result<(AddResult, BTreeMap<PathBuf, String>)> {
    if !is_service_installed(service_name) {
        return Err(Error::ServiceNotInstalled(service_name.to_string()));
    }
    let metadata = load_metadata(service_name)?
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let exposure = match metadata.url.as_deref() {
        Some(url) => Exposure::from_url(url),
        None => Exposure::Loopback,
    };

    let service_ref = if metadata.registry.is_empty() || metadata.registry == REGISTRY_BUNDLED {
        ServiceRef::Bundled(service_name.to_string())
    } else {
        ServiceRef::Custom {
            registry: metadata.registry.clone(),
            service: service_name.to_string(),
        }
    };
    let repo_dir = resolve_registry_dir(&service_ref).await?;

    // Recover existing host ports from the install's `.env` so the
    // re-render lands on the same numbers. Without this every dynamically
    // allocated port shifts because `port_in_use` reports them taken.
    let port_overrides = read_existing_ports(service_name)?;

    // Trivial port-in-use closure: the upgrade caller pins every port via
    // `port_overrides`, so the closure is never consulted. Returning false
    // unconditionally is safe — no allocation runs.
    let port_in_use = |_p: u16| false;

    let result = add_service(
        service_name,
        &exposure,
        metadata.auth.clone(),
        metadata.auth.is_some(),
        // SMTP is gated inside add_service by `config.smtp.is_some()` and
        // the service's own `integrations.smtp` flag, so passing `true`
        // here mirrors what `ryra add` does and is harmless when SMTP is
        // not configured.
        true,
        // Backup enablement is per-install state; preserve whatever the
        // user picked at the original `ryra add`.
        metadata.backup_enabled,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &metadata.registry,
        &repo_dir,
        None,
        &port_in_use,
        // ACME mode is only consumed when adding the reverse proxy itself;
        // upgrade never needs to seed the TLS snippet.
        None,
        PlanMode::Upgrade,
        &port_overrides,
    )?;

    let mut planned: BTreeMap<PathBuf, String> = BTreeMap::new();
    for step in &result.steps {
        if let Step::WriteFile(file) = step {
            planned.insert(file.path.clone(), file.content.clone());
        }
    }
    Ok((result, planned))
}

/// Parse the on-disk `.env` for a service into a key→value map. Lines
/// without `=`, comments, and blanks are skipped. Returns an empty map if
/// the file is absent — caller decides whether that's a soft error.
fn read_existing_env_keys(service_name: &str) -> Result<BTreeMap<String, String>> {
    let env_path = service_home(service_name)?.join(".env");
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => {
            return Err(Error::FileRead {
                path: env_path,
                source,
            });
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    Ok(out)
}

/// Parse `SERVICE_PORT_<NAME>=<port>` lines out of an installed service's
/// `.env`. Returns a name → port map (lowercased name, matching the
/// `[[ports]]` definition in service.toml).
fn read_existing_ports(service_name: &str) -> Result<BTreeMap<String, u16>> {
    let env_path = service_home(service_name)?.join(".env");
    let mut overrides = BTreeMap::new();
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        // No .env yet means a half-installed service; let the planner
        // re-allocate. (`add_service` will then surface a richer error if
        // the install is genuinely broken.)
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(overrides),
        Err(source) => {
            return Err(Error::FileRead {
                path: env_path,
                source,
            });
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(name) = key.strip_prefix("SERVICE_PORT_") else {
            continue;
        };
        if let Ok(port) = value.trim().parse::<u16>() {
            overrides.insert(name.to_ascii_lowercase(), port);
        }
    }
    Ok(overrides)
}

/// Lockfile-tracked files we never want to flag as drift. The `.env` carries
/// generated secrets that rotate at runtime; `service.manifest` itself is the
/// manifest, not a tracked file. Both are excluded from the planned set
/// during diffing so they don't appear as Removed/Added.
fn should_skip_path(path: &std::path::Path, manifest_file: &std::path::Path) -> bool {
    if path == manifest_file {
        return true;
    }
    matches!(path.file_name().and_then(|n| n.to_str()), Some(".env"))
}

/// Compute the diff between the registry's render and what's on disk for an
/// installed service.
pub async fn diff_service(service_name: &str) -> Result<DiffResult> {
    let (result, planned) = replan(service_name).await?;
    let manifest_file = manifest::manifest_path(service_name)?;
    let (manifest_entries, _manifest_envs) = manifest::load(service_name)?.unwrap_or_default();
    let manifest_by_path: BTreeMap<PathBuf, String> = manifest_entries
        .into_iter()
        .map(|e| (e.path, e.sha256))
        .collect();

    // Env additions: registry-expected static keys missing from the user's
    // `.env`. Append-only — we ignore present-but-different values
    // (could be a manual override) and never propose removals (could be
    // a key the user added themselves that the registry happens not to
    // ship). The registry-side list comes from the freshly-rendered
    // `tracked_envs` (which carries kind + prompt for the CLI), not the
    // on-disk manifest — that's the source of truth.
    let existing_env = read_existing_env_keys(service_name)?;
    let env_additions: Vec<EnvAddition> = result
        .tracked_envs
        .iter()
        .filter(|p| !existing_env.contains_key(&p.key))
        .map(|p| EnvAddition {
            key: p.key.clone(),
            value: p.value.clone(),
            kind: p.kind.clone(),
            prompt: p.prompt.clone(),
        })
        .collect();

    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

    // Walk planned files first — Added / Modified / Drift / Unchanged.
    for (path, content) in &planned {
        if should_skip_path(path, &manifest_file) {
            continue;
        }
        seen.insert(path.clone());
        let planned_hash = manifest::hash_bytes(content.as_bytes());
        let on_disk_hash = if path.exists() {
            Some(manifest::hash_file(path)?)
        } else {
            None
        };
        let manifest_hash = manifest_by_path.get(path);

        let kind = match (on_disk_hash.as_deref(), manifest_hash.map(String::as_str)) {
            // File doesn't exist on disk.
            (None, Some(_)) | (None, None) => match manifest_hash {
                Some(_) => DiffKind::Modified, // we wrote it, user deleted it; restore
                None => DiffKind::Added,       // registry adds it, fresh write
            },
            // On-disk content already matches what the registry would render.
            (Some(d), _) if d == planned_hash => DiffKind::Unchanged,
            // No manifest entry → can't tell if the user touched it.
            // Conservative: treat as drift so --force is required once.
            (Some(_), None) => DiffKind::Drift,
            // On-disk matches the manifest but not the planned render →
            // ryra-owned, safe to overwrite.
            (Some(d), Some(l)) if d == l => DiffKind::Modified,
            // On-disk matches neither lock nor plan → user hand-edited.
            (Some(_), Some(_)) => DiffKind::Drift,
        };
        entries.push(DiffEntry {
            path: path.clone(),
            kind,
        });
    }

    // Walk manifest entries that the planner no longer emits — Removed.
    for path in manifest_by_path.keys() {
        if seen.contains(path) {
            continue;
        }
        if should_skip_path(path, &manifest_file) {
            continue;
        }
        entries.push(DiffEntry {
            path: path.clone(),
            kind: DiffKind::Removed,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffResult {
        service: service_name.to_string(),
        entries,
        env_additions,
    })
}

/// Plan an upgrade for an installed service.
///
/// Returns the steps to execute and the backup directory where displaced
/// files will be copied. The backup dir is *also* baked into the steps
/// (as `Step::CopyFile` entries placed before each `Step::WriteFile`).
pub async fn upgrade_service(service_name: &str, force: bool) -> Result<UpgradeResult> {
    let diff = diff_service(service_name).await?;

    if !force {
        let drifted = diff.drifted();
        if !drifted.is_empty() {
            return Err(Error::HandEditedFiles {
                service: service_name.to_string(),
                paths: drifted.iter().map(|e| e.path.clone()).collect(),
            });
        }
    }

    let (result, planned) = replan(service_name).await?;
    let manifest_file = manifest::manifest_path(service_name)?;
    let env_file = service_home(service_name)?.join(".env");

    // Hard-fail if `.env` is missing. Append-only env handling can't
    // reconstruct generated secrets (mysql_root_password, jwt_key, etc.)
    // and would silently produce a half-written file that fails on
    // restart. Surface the real problem instead.
    if !env_file.exists() {
        return Err(Error::Template(format!(
            "{service_name}: `.env` is missing at {} — upgrade can't reconstruct generated secrets. \
             Restore the file from a backup or reinstall the service.",
            env_file.display()
        )));
    }

    // Decide the backup directory once per upgrade run. Used whenever any
    // file would be overwritten *or* the existing service.manifest exists (the
    // lock is always backed up so `ryra revert` can reconstruct the
    // pre-upgrade state). Empty when neither holds — keeps
    // `~/.local/state/ryra/` from accumulating no-op dirs.
    let backup_dir = backup_directory(service_name)?;
    let needs_backup: BTreeSet<PathBuf> = diff
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                DiffKind::Modified | DiffKind::Drift | DiffKind::Removed
            )
        })
        .map(|e| e.path.clone())
        .collect();
    let manifest_will_be_backed_up = manifest_file.exists();
    let backup_used = !needs_backup.is_empty() || manifest_will_be_backed_up;

    // Filter the planned step list down to what an upgrade should actually do.
    // - WriteFile for `.env` is dropped (preserve secrets).
    // - PullImage stays (idempotent if cached, fetches new tag if registry bumped).
    // - StartService is replaced with RestartService at the very end.
    // - CreateDir / Symlink stay (idempotent and may be needed for new files).
    // - DaemonReload stays.
    // - CopyFile stays (vendored binaries; rare to upgrade but handled the same).
    // - TailscaleSetup / TailscaleEnable were already gated out by PlanMode::Upgrade.
    let mut steps: Vec<Step> = Vec::new();
    if backup_used {
        steps.push(Step::CreateDir(backup_dir.clone()));
    }
    let unchanged: BTreeSet<PathBuf> = diff
        .entries
        .iter()
        .filter(|e| matches!(e.kind, DiffKind::Unchanged))
        .map(|e| e.path.clone())
        .collect();

    let env_filename = std::ffi::OsStr::new(".env");
    for step in result.steps {
        match step {
            // .env stays untouched on upgrade — generated secrets in the
            // running service must not be regenerated.
            Step::WriteFile(GeneratedFile { ref path, .. })
                if path.file_name() == Some(env_filename) =>
            {
                continue;
            }
            // Identical content already on disk — skip the write entirely
            // so the file's mtime stays put and `sha256sum -c` stays clean
            // for unchanged entries.
            Step::WriteFile(GeneratedFile { ref path, .. }) if unchanged.contains(path) => {
                // The manifest is special: even if "unchanged" by content, we
                // re-emit it because path-level adds/removes mean its content
                // has changed and we need the new hashes recorded.
                if path == &manifest_file {
                    steps.push(step);
                }
                continue;
            }
            Step::WriteFile(ref file) => {
                // Always back up the existing service.manifest too, even though
                // it's filtered out of the diff. `ryra revert` reads the
                // backed-up lock to know which files were Added during the
                // upgrade (current lock − pre-upgrade lock) so it can delete
                // them on revert. Without this, revert would leave
                // upgrade-added files orphaned.
                let should_backup = (needs_backup.contains(&file.path)
                    || file.path == manifest_file)
                    && file.path.exists();
                if should_backup {
                    let rel = backup_relpath(&file.path);
                    let dst = backup_dir.join(rel);
                    if let Some(parent) = dst.parent() {
                        steps.push(Step::CreateDir(parent.to_path_buf()));
                    }
                    steps.push(Step::CopyFile {
                        src: file.path.clone(),
                        dst,
                    });
                }
                steps.push(step);
            }
            // The replanned step list always ends with StartService; we
            // strip it and append a RestartService at the very end so the
            // unit picks up the new quadlet.
            Step::StartService { .. } => continue,
            other => steps.push(other),
        }
    }

    // Removed files: back them up then delete.
    for entry in &diff.entries {
        if !matches!(entry.kind, DiffKind::Removed) {
            continue;
        }
        if entry.path.exists() {
            let rel = backup_relpath(&entry.path);
            let dst = backup_dir.join(rel);
            if let Some(parent) = dst.parent() {
                steps.push(Step::CreateDir(parent.to_path_buf()));
            }
            steps.push(Step::CopyFile {
                src: entry.path.clone(),
                dst,
            });
        }
        steps.push(Step::RemoveFile(entry.path.clone()));
    }

    // Env additions: append registry-required static env vars that the
    // user's .env doesn't have. Append-only — we never rewrite the
    // existing .env (that would clobber rotated secrets and any manual
    // edits) and we never remove keys (the user might have added their
    // own that the registry happens not to ship). The .env is
    // intentionally NOT backed up: it only ever gains lines and the
    // pre-existing content survives unchanged.
    if !diff.env_additions.is_empty() {
        let mut content = match std::fs::read_to_string(&env_file) {
            Ok(c) => c,
            // Service installed but .env missing? Treat the add as a
            // fresh write — odd state, but the right one to recover to.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(Error::FileRead {
                    path: env_file.clone(),
                    source,
                });
            }
        };
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for add in &diff.env_additions {
            content.push_str(&format!("{}={}\n", add.key, add.value));
        }
        steps.push(Step::WriteFile(GeneratedFile {
            path: env_file,
            content,
        }));
    }

    // Pick up the new quadlet by restarting. RestartService is enough to
    // re-read the env file, re-run ExecStartPre/Post, and pull in any new
    // ExecStartPost script (the seafile case).
    steps.push(Step::RestartService {
        unit: service_name.to_string(),
    });

    Ok(UpgradeResult {
        service: service_name.to_string(),
        diff,
        steps,
        backup_dir: if backup_used { Some(backup_dir) } else { None },
        // The replanned env content is irrelevant for upgrade (we don't
        // write it), but expose the template-render context bag in case
        // future callers need it. Keep it empty for now to avoid
        // confusing consumers.
        planned_files: planned,
    })
}

pub struct UpgradeResult {
    pub service: String,
    pub diff: DiffResult,
    pub steps: Vec<Step>,
    /// `None` when no files would be overwritten or removed.
    pub backup_dir: Option<PathBuf>,
    pub planned_files: BTreeMap<PathBuf, String>,
}

/// One available backup snapshot for a service.
#[derive(Debug, Clone)]
pub struct BackupSnapshot {
    /// Filesystem path: `~/.local/state/ryra/backups/<timestamp>/<service>/`.
    pub path: PathBuf,
    /// `YYYY-MM-DDTHH-MM-SSZ` timestamp from the parent dir name.
    pub timestamp: String,
}

pub struct RevertResult {
    pub service: String,
    pub snapshot: BackupSnapshot,
    pub steps: Vec<Step>,
    /// Files to be copied from backup back to their original locations.
    pub files_to_restore: Vec<PathBuf>,
    /// Files added by the upgrade that didn't exist before — will be
    /// removed by revert. Empty when the snapshot pre-dates the manifest
    /// feature (we can't reconstruct what was added without it).
    pub files_to_delete: Vec<PathBuf>,
}

/// List every backup snapshot for a service, newest first. Empty result
/// means there's nothing to revert from.
/// How many backup snapshots `ryra upgrade` retains per service before
/// auto-pruning. Each snapshot is small (~tens of KB — config files +
/// the manifest) so the cap is more about mental clutter than disk; 5
/// is enough to revert a few iterations back without filling the
/// `~/.local/state/ryra/backups/` tree with dead snapshots from years
/// of upgrades.
pub const DEFAULT_BACKUP_KEEP: usize = 5;

/// Drop snapshots older than the most recent `keep` for this service.
/// Returns the paths that were removed (newest-first within the
/// removed set; the kept set keeps the same order). The shared
/// timestamp dir is also removed when this was the last service-
/// scoped subdir under it (multi-service upgrade runs share a
/// timestamp dir; we don't want to nuke other services' state).
pub fn prune_backups(service_name: &str, keep: usize) -> Result<Vec<PathBuf>> {
    let backups_root = state_dir()?.join("backups");
    prune_backups_in(&backups_root, service_name, keep)
}

/// Pure inner that operates on an explicit `<state>/backups/` root.
/// Split out so tests can drive it against a tmp tree without touching
/// the real XDG state dir.
fn prune_backups_in(
    backups_root: &std::path::Path,
    service_name: &str,
    keep: usize,
) -> Result<Vec<PathBuf>> {
    let snapshots = list_backups_in(backups_root, service_name)?;
    if snapshots.len() <= keep {
        return Ok(Vec::new());
    }
    let mut removed: Vec<PathBuf> = Vec::new();
    for snap in snapshots.into_iter().skip(keep) {
        if let Err(e) = std::fs::remove_dir_all(&snap.path) {
            eprintln!(
                "warning: failed to prune backup {}: {e}",
                snap.path.display()
            );
            continue;
        }
        removed.push(snap.path.clone());
        if let Some(parent) = snap.path.parent()
            && let Ok(mut entries) = std::fs::read_dir(parent)
            && entries.next().is_none()
        {
            let _ = std::fs::remove_dir(parent);
        }
    }
    Ok(removed)
}

pub fn list_backups(service_name: &str) -> Result<Vec<BackupSnapshot>> {
    let backups_root = state_dir()?.join("backups");
    list_backups_in(&backups_root, service_name)
}

fn list_backups_in(
    backups_root: &std::path::Path,
    service_name: &str,
) -> Result<Vec<BackupSnapshot>> {
    if !backups_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut snapshots: Vec<BackupSnapshot> = Vec::new();
    let entries = std::fs::read_dir(backups_root).map_err(|source| Error::FileRead {
        path: backups_root.to_path_buf(),
        source,
    })?;
    for entry in entries.flatten() {
        let stamp_dir = entry.path();
        if !stamp_dir.is_dir() {
            continue;
        }
        let svc_dir = stamp_dir.join(service_name);
        if !svc_dir.is_dir() {
            continue;
        }
        let Some(stamp) = stamp_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        snapshots.push(BackupSnapshot {
            path: svc_dir,
            timestamp: stamp.to_string(),
        });
    }
    // Newest first: timestamp is `YYYY-MM-DDTHH-MM-SSZ`, lexical-descending == reverse-chronological.
    snapshots.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(snapshots)
}

/// Plan a revert for an installed service.
///
/// `at` selects a specific backup timestamp; `None` picks the most recent.
/// The returned plan: restore every file from the backup tree to its
/// original location, delete files added by the upgrade, daemon-reload,
/// restart the unit.
pub fn revert_service(service_name: &str, at: Option<&str>) -> Result<RevertResult> {
    if !is_service_installed(service_name) {
        return Err(Error::ServiceNotInstalled(service_name.to_string()));
    }
    let snapshot = pick_snapshot(service_name, at)?;

    // Files to restore: walk the backup tree and reconstruct the original
    // absolute path for each one. The backup mirrors absolute paths under
    // `<snapshot>/<original-path-without-leading-slash>`, so the inverse is
    // simply prefixing `/` to each path-relative-to-snapshot.
    let mut files_to_restore: Vec<PathBuf> = Vec::new();
    walk_backup_files(&snapshot.path, &mut files_to_restore)?;

    // Files to delete: anything in the *current* lock that isn't in the
    // *backed-up* lock was added by the upgrade and should disappear on
    // revert. If either lock is absent, leave the delete set empty —
    // safest no-op for snapshots that pre-date this feature.
    let backup_manifest_file =
        absolute_to_backup_path(&snapshot.path, &manifest::manifest_path(service_name)?);
    let (backup_manifest_entries, _) = read_manifest_at(&backup_manifest_file)?;
    let (current_manifest_entries, _) = manifest::load(service_name)?.unwrap_or_default();

    let backup_manifest_set: BTreeSet<PathBuf> = backup_manifest_entries
        .iter()
        .map(|e| e.path.clone())
        .collect();
    let mut files_to_delete: Vec<PathBuf> = if backup_manifest_entries.is_empty() {
        // Pre-feature snapshot: no way to know what was added.
        Vec::new()
    } else {
        current_manifest_entries
            .iter()
            .map(|e| e.path.clone())
            .filter(|p| !backup_manifest_set.contains(p))
            .collect()
    };
    files_to_delete.sort();

    // Build the step list.
    let mut steps: Vec<Step> = Vec::new();
    // Restore: backup → original. CopyFile creates parents itself, so no
    // CreateDir needed.
    for backup_path in &files_to_restore {
        let original = backup_to_absolute_path(&snapshot.path, backup_path);
        steps.push(Step::CopyFile {
            src: backup_path.clone(),
            dst: original,
        });
    }
    // Delete: each Added file, plus any orphan symlink in the quadlet dir
    // that pointed at it (only the actual file is in the lock; the
    // companion symlink in `~/.config/containers/systemd/` is not).
    let qd = crate::quadlet_dir()?;
    for path in &files_to_delete {
        if path.exists() {
            steps.push(Step::RemoveFile(path.clone()));
        }
        if let Some(name) = path.file_name() {
            let symlink = qd.join(name);
            if std::fs::symlink_metadata(&symlink).is_ok() {
                steps.push(Step::RemoveFile(symlink));
            }
        }
    }
    steps.push(Step::DaemonReload);
    steps.push(Step::RestartService {
        unit: service_name.to_string(),
    });

    let files_to_restore_orig: Vec<PathBuf> = files_to_restore
        .iter()
        .map(|p| backup_to_absolute_path(&snapshot.path, p))
        .collect();
    Ok(RevertResult {
        service: service_name.to_string(),
        snapshot,
        steps,
        files_to_restore: files_to_restore_orig,
        files_to_delete,
    })
}

/// Resolve the snapshot to revert to. `at` is a timestamp string (e.g.
/// `2026-05-05T13-33-50Z`); when absent, the most recent snapshot wins.
fn pick_snapshot(service_name: &str, at: Option<&str>) -> Result<BackupSnapshot> {
    let snapshots = list_backups(service_name)?;
    if snapshots.is_empty() {
        return Err(Error::NoBackup(service_name.to_string()));
    }
    match at {
        None => Ok(snapshots
            .into_iter()
            .next()
            .expect("non-empty checked above")),
        Some(stamp) => snapshots
            .into_iter()
            .find(|s| s.timestamp == stamp)
            .ok_or_else(|| Error::BackupNotFound {
                service: service_name.to_string(),
                stamp: stamp.to_string(),
            }),
    }
}

/// Recursively collect every regular file under `root` into `out`. Symlinks
/// are followed; we don't expect any in a backup tree (we always copied
/// targets, never link entries).
fn walk_backup_files(root: &std::path::Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(root).map_err(|source| Error::FileRead {
        path: root.to_path_buf(),
        source,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            walk_backup_files(&path, out)?;
        } else if meta.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Inverse of `backup_relpath`: a backup path `<root>/home/user/foo`
/// maps back to `/home/user/foo`.
fn backup_to_absolute_path(root: &std::path::Path, backup: &std::path::Path) -> PathBuf {
    let rel = backup.strip_prefix(root).unwrap_or(backup);
    PathBuf::from("/").join(rel)
}

/// Forward variant: `<root>` + `/home/user/foo` → `<root>/home/user/foo`.
fn absolute_to_backup_path(root: &std::path::Path, abs: &std::path::Path) -> PathBuf {
    let rel = abs.to_string_lossy();
    let stripped = rel.trim_start_matches('/');
    root.join(stripped)
}

/// Read a manifest at the given path. Missing-file is treated as an empty
/// list — pre-feature backups simply have no lock to reference.
fn read_manifest_at(
    path: &std::path::Path,
) -> Result<(Vec<manifest::ManifestEntry>, Vec<manifest::EnvEntry>)> {
    if !path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let content = std::fs::read_to_string(path).map_err(|source| Error::FileRead {
        path: path.to_path_buf(),
        source,
    })?;
    manifest::parse(&content)
}

/// `~/.local/state/ryra/backups/<timestamp>/<service>/`. Timestamp uses an
/// ISO-8601-ish form that sorts lexically (no colons — Windows-friendly,
/// not that it matters today, but the cost is zero).
fn backup_directory(service_name: &str) -> Result<PathBuf> {
    let state = state_dir()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::Template(format!("system clock before UNIX epoch: {e}")))?
        .as_secs();
    let stamp = format_timestamp(now);
    Ok(state.join("backups").join(stamp).join(service_name))
}

/// XDG state dir under `ryra/`. Created on demand by the CreateDir step.
fn state_dir() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .ok_or(Error::HomeDirNotFound)?;
    Ok(base.join("ryra"))
}

/// Format a UNIX epoch into `YYYY-MM-DDTHH-MM-SSZ`. Avoids the chrono
/// dependency — we just need stable lexical sort.
fn format_timestamp(secs: u64) -> String {
    // Days from 1970-01-01.
    const SECS_PER_DAY: u64 = 86_400;
    let days = secs / SECS_PER_DAY;
    let time_of_day = secs % SECS_PER_DAY;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    let (y, mo, d) = ymd_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{m:02}-{s:02}Z")
}

/// Convert "days since 1970-01-01" into `(year, month, day)` using the
/// civil-from-days algorithm (Howard Hinnant's date library, MIT). Self-
/// contained so we don't add a chrono/time dep just for backup naming.
fn ymd_from_days(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Map an absolute path into the backup tree. We strip the leading `/` so the
/// joined path doesn't escape the backup dir; everything else is preserved
/// verbatim so the user can `diff -r` across the original location.
fn backup_relpath(path: &std::path::Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_numbers() {
        // 2026-01-01T00-00-00Z — sanity check on the calendar conversion.
        // 1767225600 = days from epoch * 86400 for 2026-01-01.
        // (epoch 0 = 1970-01-01; 56 years incl. leap days = 20454 days.)
        // Easier: just verify a known value end-to-end.
        let s = format_timestamp(0);
        assert_eq!(s, "1970-01-01T00-00-00Z");
        let s = format_timestamp(86_400);
        assert_eq!(s, "1970-01-02T00-00-00Z");
        let s = format_timestamp(31_536_000); // not a leap year (1970)
        assert_eq!(s, "1971-01-01T00-00-00Z");
    }

    #[test]
    fn backup_relpath_strips_leading_slash() {
        let p = backup_relpath(std::path::Path::new("/home/user/foo/bar"));
        assert_eq!(p, PathBuf::from("home/user/foo/bar"));
    }

    /// Stand up a tmp backups tree with the given timestamps and a
    /// service subdir under each, then run `prune_backups_in` against it.
    /// Returns (kept timestamps newest-first, removed paths). Hermetic:
    /// no env vars touched, no shared global state.
    fn setup_and_prune(stamps: &[&str], keep: usize) -> (Vec<String>, Vec<PathBuf>) {
        let tmp = std::env::temp_dir().join(format!(
            "ryra-prune-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let backups_root = tmp.join("backups");
        for s in stamps {
            std::fs::create_dir_all(backups_root.join(s).join("svc")).unwrap();
        }
        let removed = prune_backups_in(&backups_root, "svc", keep).unwrap();
        let mut kept: Vec<String> = std::fs::read_dir(&backups_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        kept.sort();
        kept.reverse();
        let _ = std::fs::remove_dir_all(&tmp);
        (kept, removed)
    }

    #[test]
    fn prune_keeps_newest_n() {
        // Five timestamps, keep=3 — the two oldest (lex-smallest) should go.
        let (kept, removed) = setup_and_prune(
            &[
                "2026-01-01T00-00-00Z",
                "2026-02-01T00-00-00Z",
                "2026-03-01T00-00-00Z",
                "2026-04-01T00-00-00Z",
                "2026-05-01T00-00-00Z",
            ],
            3,
        );
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0], "2026-05-01T00-00-00Z");
        assert_eq!(kept[2], "2026-03-01T00-00-00Z");
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn prune_no_op_when_under_keep() {
        let (kept, removed) = setup_and_prune(&["2026-01-01T00-00-00Z", "2026-02-01T00-00-00Z"], 5);
        assert_eq!(kept.len(), 2);
        assert!(removed.is_empty());
    }

    #[test]
    fn should_skip_path_excludes_env_and_manifest() {
        let lock = PathBuf::from("/svc/service.manifest");
        assert!(should_skip_path(&PathBuf::from("/svc/.env"), &lock));
        assert!(should_skip_path(&lock, &lock));
        assert!(!should_skip_path(
            &PathBuf::from("/svc/configs/x.sh"),
            &lock
        ));
    }
}
