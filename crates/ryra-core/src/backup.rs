//! Backup planning. Pure functions that take service install state +
//! the user's backup config and produce typed plans the CLI executes.
//!
//! What lives here:
//! - [`BackupRunPlan`]: everything the CLI needs to push one service's
//!   data to the configured restic repository.
//! - [`BackupRestorePlan`]: same shape for the reverse operation.
//! - [`plan_backup_run`] / [`plan_backup_restore`]: the planners.
//!
//! What does *not* live here: spawning the `restic` subprocess, running
//! hook scripts, or any other side effect. The CLI layer owns those.
//! Keeping the planner pure means it round-trips cleanly in tests
//! against a tempdir without needing restic on the test runner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::config::ConfigPaths;
use crate::config::schema::{BackupBackend, Config};
use crate::error::{Error, Result};
use crate::metadata::{Metadata, load_metadata};
use crate::paths::service_home;
use crate::registry;
use crate::registry::service_def::ServiceDef;

const SERVICE_TOML_FILENAME: &str = "service.toml";

/// Concrete instructions for backing up one installed service.
///
/// The CLI consumes this by:
/// 1. Running every `pre_backup_hook` script in order.
/// 2. Spawning `restic backup` with `repo`, `password` (via
///    `RESTIC_PASSWORD` env), `env` set on the child, `--tag` for each
///    string in `tags`, and `--exclude` for each string in `excludes`,
///    with `paths` as the positional arguments.
/// 3. Running every `post_backup_hook` (even if step 2 failed —
///    failure-cleanup matters; see [`PlanHook::Cleanup`]).
#[derive(Debug, Clone)]
pub struct BackupRunPlan {
    pub service_name: String,
    pub service_home: PathBuf,
    pub repo: String,
    pub password: String,
    pub env: BTreeMap<String, String>,
    pub tags: Vec<String>,
    pub paths: Vec<PathBuf>,
    pub excludes: Vec<String>,
    /// False (the default) means a cold snapshot: ryra stops `units`, makes
    /// `data_paths` readable, snapshots, then restarts. True means ryra leaves
    /// the service running and only drives the hooks (see [`BackupConfig`]).
    pub online: bool,
    /// The service's systemd units (one per container quadlet), derived so ryra
    /// can stop the whole stack for a cold snapshot. Empty for an online service.
    pub units: Vec<String>,
    /// The service's data directories (absolute), derived from `[backup].paths`.
    /// Cold snapshots `podman unshare chown` these so restic can read them.
    pub data_paths: Vec<PathBuf>,
    pub pre_backup_hook: Option<PathBuf>,
    pub post_backup_hook: Option<PathBuf>,
}

/// Instructions for restoring one installed service from a specific
/// restic snapshot.
#[derive(Debug, Clone)]
pub struct BackupRestorePlan {
    pub service_name: String,
    pub service_home: PathBuf,
    pub repo: String,
    pub password: String,
    pub env: BTreeMap<String, String>,
    /// `latest` to grab the newest snapshot, or a specific restic
    /// snapshot id (hex prefix) when the user passed `--at <id>`.
    pub snapshot: String,
    /// Mirror of [`BackupRunPlan::online`]. A cold restore stops `units`, wipes
    /// `data_paths` to a clean tree, restores, then restarts. An online restore
    /// runs only the hooks around restic.
    pub online: bool,
    /// The service's systemd units, derived so ryra can stop the stack before a
    /// cold restore. Empty for an online service.
    pub units: Vec<String>,
    /// The service's data directories (absolute), derived from `[backup].paths`.
    /// A cold restore wipes these to a clean tree before `restic restore`.
    pub data_paths: Vec<PathBuf>,
    /// Also restore the global `preferences.toml` bundled in the snapshot.
    /// Default `false`: a per-service restore must NOT clobber global config
    /// (SMTP/auth/backup creds/other services) with a stale copy. `true` is the
    /// disaster-recovery opt-in (`ryra backup restore --config`).
    pub include_config: bool,
    pub pre_restore_hook: Option<PathBuf>,
    pub post_restore_hook: Option<PathBuf>,
}

/// Instructions for pruning one service's snapshots to the retention ladder.
/// Built from the configured retention policy; the CLI spawns `restic forget`
/// (then `--prune` to reclaim space) scoped to this service's `service:<name>`
/// tag, so one service's policy can't evict another's snapshots.
#[derive(Debug, Clone)]
pub struct BackupForgetPlan {
    pub service_name: String,
    pub repo: String,
    pub password: String,
    pub env: BTreeMap<String, String>,
    /// restic `--tag` filter (e.g. `service:<name>,mode:daily`) — forget only
    /// considers snapshots matching all of these comma-joined tags.
    pub tag: String,
    /// `--keep-*` flags from the policy. Never empty (the planner returns
    /// `None` for an absent/all-zero policy rather than an empty plan).
    pub keep_args: Vec<String>,
    /// Reclaim space after forgetting. Skipped in a dry run.
    pub prune: bool,
    /// Show what would be removed without removing it.
    pub dry_run: bool,
}

/// Plan a `ryra backup manual <service>` invocation. Errors loudly when:
/// - the service isn't installed,
/// - the user hasn't run `ryra backup connect` yet,
/// - the service author hasn't declared backup support (defensive —
///   the install-time check should have caught this earlier, but a
///   manifest change between install and backup is possible).
///
/// Note: a snapshot does NOT require the service to be enrolled
/// (`backup_enabled`). Enrollment only governs the daily/weekly schedule; a
/// manual one-off backup of any backup-capable install is allowed.
pub fn plan_backup_run(
    service_name: &str,
    config: &Config,
    repo_dir: &Path,
    mode: &str,
) -> Result<BackupRunPlan> {
    // Ensure it's installed (errors otherwise); enrollment is not required.
    load_install_metadata(service_name)?;
    let settings = config
        .backup
        .as_ref()
        .ok_or(Error::BackupRepoNotConfigured)?;

    let svc = registry::find_service(repo_dir, service_name)?;
    if !svc.def.integrations.backup {
        return Err(Error::BackupNotSupported(service_name.to_string()));
    }

    let home = service_home(service_name)?;
    let (mut paths, excludes) = resolve_paths(&svc.def, &home)?;

    // Every snapshot also carries the global `preferences.toml` (repo
    // creds, SMTP, auth, generated secrets). It's tiny and restic dedups
    // it across services, so the cost is ~nothing — and it means any
    // single service snapshot is enough to restore the global config.
    let prefs = ConfigPaths::resolve()?.config_file;
    if prefs.exists() {
        paths.push(prefs);
    }

    let manifest_sha = manifest_sha256(&svc.service_dir);
    let mut tags = vec![format!("service:{service_name}")];
    tags.push(format!("manifest_sha:{}", &manifest_sha[..16]));
    // Stamp the stable machine id so a snapshot self-identifies which machine it
    // came from (the label is the hostname, carried in restic's own `host`
    // field). Lets the bucket be read back machine-by-machine even with only the
    // backups in hand.
    if let Some(machine) = config.machine.as_ref() {
        tags.push(format!("machine_id:{}", machine.id));
    }
    // The cadence this snapshot belongs to (daily | weekly | manual). Drives
    // per-mode retention (keep the last N of a mode) and the grouped listing.
    tags.push(format!("mode:{mode}"));

    let backup = svc.def.backup.as_ref();
    let pre = resolve_hook(
        backup.and_then(|b| b.pre_backup.as_deref()),
        &home,
        "backup-pre.sh",
    );
    let post = resolve_hook(
        backup.and_then(|b| b.post_backup.as_deref()),
        &home,
        "backup-post.sh",
    );
    let online = backup.is_some_and(|b| b.online);
    // Cold snapshots stop the stack; online ones don't, so they need no units.
    let units = if online {
        Vec::new()
    } else {
        service_units(&home)
    };
    let data = data_paths(&svc.def, &home);

    Ok(BackupRunPlan {
        service_name: service_name.to_string(),
        service_home: home,
        repo: settings.backend.restic_repo(),
        password: settings.password.clone(),
        env: backend_env_map(&settings.backend),
        tags,
        paths,
        excludes,
        online,
        units,
        data_paths: data,
        pre_backup_hook: pre,
        post_backup_hook: post,
    })
}

/// Plan a `ryra backup restore <service>` invocation.
///
/// `snapshot` is either `latest` (newest snapshot tagged with this
/// service) or an explicit restic snapshot id. The CLI resolves the
/// actual id by querying restic; this planner stays pure and just
/// passes the user's choice through.
pub fn plan_backup_restore(
    service_name: &str,
    snapshot: &str,
    config: &Config,
    repo_dir: &Path,
) -> Result<BackupRestorePlan> {
    // Ensure it's installed (errors otherwise); a snapshot can be restored
    // whether or not the service is enrolled in the schedule.
    load_install_metadata(service_name)?;
    let settings = config
        .backup
        .as_ref()
        .ok_or(Error::BackupRepoNotConfigured)?;

    let svc = registry::find_service(repo_dir, service_name)?;
    let home = service_home(service_name)?;

    let backup = svc.def.backup.as_ref();
    let pre = resolve_hook(
        backup.and_then(|b| b.pre_restore.as_deref()),
        &home,
        "restore-pre.sh",
    );
    let post = resolve_hook(
        backup.and_then(|b| b.post_restore.as_deref()),
        &home,
        "restore-post.sh",
    );
    let online = backup.is_some_and(|b| b.online);
    let units = if online {
        Vec::new()
    } else {
        service_units(&home)
    };
    let data = data_paths(&svc.def, &home);

    Ok(BackupRestorePlan {
        service_name: service_name.to_string(),
        service_home: home,
        repo: settings.backend.restic_repo(),
        password: settings.password.clone(),
        env: backend_env_map(&settings.backend),
        snapshot: snapshot.to_string(),
        online,
        units,
        data_paths: data,
        // Per-service restore never touches the global config by default; the
        // CLI's `--config` flag flips this for disaster recovery.
        include_config: false,
        pre_restore_hook: pre,
        post_restore_hook: post,
    })
}

/// Plan a per-mode prune for one service: keep at most `keep` snapshots tagged
/// `mode:<mode>` (the daily or weekly cap), dropping the oldest beyond that.
/// Manual snapshots are never pruned, so callers only pass `daily`/`weekly`.
/// Returns `Ok(None)` when `keep == 0` (unlimited) rather than running a
/// keep-nothing forget.
pub fn plan_mode_prune(
    service_name: &str,
    config: &Config,
    mode: &str,
    keep: u32,
    dry_run: bool,
) -> Result<Option<BackupForgetPlan>> {
    if keep == 0 {
        return Ok(None);
    }
    let metadata = load_install_metadata(service_name)?;
    if !metadata.backup_enabled {
        return Err(Error::BackupNotEnabled(service_name.to_string()));
    }
    let settings = config
        .backup
        .as_ref()
        .ok_or(Error::BackupRepoNotConfigured)?;
    Ok(Some(BackupForgetPlan {
        service_name: service_name.to_string(),
        repo: settings.backend.restic_repo(),
        password: settings.password.clone(),
        env: backend_env_map(&settings.backend),
        // AND of both tags ("a,b"): only THIS service's snapshots in THIS mode.
        tag: format!("service:{service_name},mode:{mode}"),
        keep_args: vec!["--keep-last".to_string(), keep.to_string()],
        prune: true,
        dry_run,
    }))
}

/// List installed services that have `backup_enabled = true` in their
/// metadata. The CLI's `ryra backup manual` (no service argument) uses
/// this to iterate every enabled install.
pub fn list_backup_enabled() -> Result<Vec<String>> {
    let root = crate::paths::service_data_root()?;
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root).map_err(|source| Error::FileRead {
        path: root.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::FileRead {
            path: root.clone(),
            source,
        })?;
        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        if let Some(meta) = load_metadata(&name)?
            && meta.backup_enabled
        {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Enroll or unenroll a service in backups by flipping `backup_enabled` in its
/// `metadata.toml`. Returns whether the flag actually changed (`false` if the
/// service isn't installed, or was already in that state). This on-disk flag is
/// what [`list_backup_enabled`] and a no-argument `ryra backup manual` read, so it
/// is the single source of truth both the CLI picker and the rpc layer set.
pub fn set_backup_enabled(service: &str, enabled: bool) -> Result<bool> {
    let Some(mut meta) = load_metadata(service)? else {
        return Ok(false);
    };
    if meta.backup_enabled == enabled {
        return Ok(false);
    }
    meta.backup_enabled = enabled;
    let path = service_home(service)?.join("metadata.toml");
    let toml = toml::to_string_pretty(&meta)?;
    std::fs::write(&path, toml).map_err(|source| Error::FileWrite { path, source })?;
    Ok(true)
}

fn load_install_metadata(service_name: &str) -> Result<Metadata> {
    load_metadata(service_name)?.ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))
}

/// Resolve the set of absolute paths to feed restic, plus the list of
/// `--exclude` patterns.
///
/// Two routes:
/// - Explicit `[backup].paths`: trust the manifest, resolve each
///   entry against the service home.
/// - No explicit paths: ask the classifier "what's data here?" — that
///   covers every top-level child not in the install manifest (the
///   `data/` directory, the `db-data/` directory, anything the user
///   has dropped in). Also include `.backup/` if the manifest declared
///   any pre_backup hook, since that's the convention for dumping.
fn resolve_paths(def: &ServiceDef, home: &Path) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let backup = def.backup.as_ref();
    let excludes: Vec<String> = backup.map(|b| b.exclude.clone()).unwrap_or_default();

    // Whole-folder backup: capture the entire service home in one path.
    // This carries config (`.env`, `metadata.toml`, quadlets, rendered
    // configs) alongside data, so a restore reconstructs the install
    // without re-running `ryra add` — that's the difference between
    // "restore and go" and a hand rebuild.
    //
    // Database consistency is the hooks' job, not the path list's:
    //  - dump services (`backup-pre.sh` → mariadb-dump/pg_dump into
    //    `.backup/`) list their *live* DB dir in `[backup].exclude` so
    //    the consistent dump is authoritative, not the changing files;
    //  - cold-stop services stop the DB before the snapshot, so its dir
    //    is already consistent and is captured as part of the folder.
    //
    // `exclude` also drops regenerable caches. An explicit
    // `[backup].paths` still narrows the capture for the rare service
    // that needs it, but the default — and the recommendation — is the
    // whole folder.
    if let Some(b) = backup
        && !b.paths.is_empty()
    {
        // A curated `paths` list keeps regenerable junk (thumbnails,
        // transcodes) out of the snapshot — honour it for *data*, but
        // always add the config artifacts so a restore can still
        // reconstruct the install without `ryra add`.
        let mut abs: Vec<PathBuf> = b.paths.iter().map(|p| home.join(p)).collect();
        abs.extend(config_artifacts(home));
        abs.sort();
        abs.dedup();
        return Ok((abs, excludes));
    }

    Ok((vec![home.to_path_buf()], excludes))
}

/// The config artifacts that must travel with every backup so a restore
/// reconstructs the install without re-running `ryra add`: the generated
/// `.env`, `metadata.toml`, the render manifest, the rendered `configs/`
/// tree, and the quadlet unit files. Only existing paths are returned so
/// the list feeds straight to restic. (Services with no explicit
/// `paths` capture the whole folder, which already covers all of these.)
fn config_artifacts(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for f in [".env", "metadata.toml", "service.manifest"] {
        let p = home.join(f);
        if p.exists() {
            out.push(p);
        }
    }
    let configs = home.join("configs");
    if configs.is_dir() {
        out.push(configs);
    }
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let n = name.to_string_lossy();
            if n.ends_with(".container") || n.ends_with(".network") || n.ends_with(".volume") {
                out.push(entry.path());
            }
        }
    }
    out
}

/// The service's systemd units — one `<stem>.service` per `<stem>.container`
/// quadlet in the service home. This is the set ryra stops for a cold snapshot
/// or restore (`Requires=` governs startup, not shutdown, so the whole stack is
/// stopped explicitly). Sorted for determinism.
fn service_units(home: &Path) -> Vec<String> {
    let mut units = Vec::new();
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(stem) = name.to_string_lossy().strip_suffix(".container") {
                units.push(format!("{stem}.service"));
            }
        }
    }
    units.sort();
    units
}

/// The service's data directories (absolute) — the `[backup].paths` entries
/// resolved against the home. These are the trees a cold snapshot chowns so
/// restic can read them, and a cold restore wipes to a clean tree first. Config
/// artifacts and `preferences.toml` are never in this set (never wiped). Empty
/// when the service declares no explicit paths.
fn data_paths(def: &ServiceDef, home: &Path) -> Vec<PathBuf> {
    def.backup
        .as_ref()
        .map(|b| b.paths.iter().map(|p| home.join(p)).collect())
        .unwrap_or_default()
}

/// Whether backing up this service stops it. Cold services (the default) take a
/// stop-the-stack snapshot; `online` services snapshot live. Surfaced to the UI
/// so "Back up now" can warn about the brief downtime.
pub fn backup_stops_service(def: &ServiceDef) -> bool {
    def.backup.as_ref().is_some_and(|b| !b.online)
}

/// Whether restoring this service stops it. A cold restore always stops the
/// stack to wipe + replace its data; an `online` service stops only if it ships
/// restore hooks (e.g. it pauses the app while re-importing a dump). Surfaced to
/// the UI so the restore confirm can warn about downtime.
pub fn restore_stops_service(def: &ServiceDef, home: &Path) -> bool {
    match def.backup.as_ref() {
        None => false,
        Some(b) if !b.online => true,
        Some(b) => {
            resolve_hook(b.pre_restore.as_deref(), home, "restore-pre.sh").is_some()
                || resolve_hook(b.post_restore.as_deref(), home, "restore-post.sh").is_some()
        }
    }
}

fn hook_path(home: &Path, filename: &str) -> PathBuf {
    home.join("configs").join("scripts").join(filename)
}

/// Decide which hook script (if any) to invoke for a given lifecycle
/// phase. Priority:
/// 1. Explicit `[backup].pre_backup` (or sibling) in service.toml.
/// 2. Convention: `configs/scripts/<phase>.sh` on disk.
/// 3. None — phase is a no-op.
///
/// The convention path means a typical service.toml's `[backup]`
/// section is a single `paths = [...]` line; the four hook scripts
/// are auto-discovered when their conventional names are present in
/// `configs/scripts/`, and authors never have to repeat the
/// filenames in the manifest.
fn resolve_hook(explicit: Option<&str>, home: &Path, conventional: &str) -> Option<PathBuf> {
    if let Some(name) = explicit {
        return Some(hook_path(home, name));
    }
    let conv = hook_path(home, conventional);
    if conv.exists() { Some(conv) } else { None }
}

fn backend_env_map(backend: &BackupBackend) -> BTreeMap<String, String> {
    backend
        .env()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// Hex SHA256 of the service's `service.toml`. Used as the
/// `manifest_sha:` tag on each snapshot so a future restore can detect
/// version skew between the snapshot and the currently-installed
/// service definition.
///
/// Falls back to an all-zero hash if the file can't be read — the
/// caller's higher-level error handling will already have failed for
/// other reasons, and a sentinel hash is more useful than panicking.
pub fn manifest_sha256(service_dir: &Path) -> String {
    let path = service_dir.join(SERVICE_TOML_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return "0".repeat(64),
    };
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// Execution: shared by every frontend (CLI, ryra-api). restic runs as
// the invoking user; ownership round-trips via the hooks + quadlet `:U`
// (see the hook scripts in the registry).
// ---------------------------------------------------------------------------

/// Run a pre/post backup or restore hook with the service's `.env`
/// loaded, mirroring how quadlet ExecStartPre/Post scripts see it.
pub fn run_hook(
    kind: &str,
    service: &str,
    script: &std::path::Path,
    service_home: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;
    if !script.exists() {
        return Err(crate::error::Error::BackupHookFailed {
            service: service.to_string(),
            hook: kind.to_string(),
            message: format!("hook script not found: {}", script.display()),
        }
        .into());
    }
    let env_file = service_home.join(".env");
    let envs = if env_file.exists() {
        parse_env_file(&env_file)
    } else {
        Vec::new()
    };
    let mut cmd = std::process::Command::new("/bin/bash");
    cmd.arg(script)
        .env("SERVICE_HOME", service_home)
        .current_dir(service_home);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .with_context(|| format!("running hook {kind} for {service}"))?;
    if !status.success() {
        return Err(crate::error::Error::BackupHookFailed {
            service: service.to_string(),
            hook: kind.to_string(),
            message: format!("hook script exited with {}", status.code().unwrap_or(-1)),
        }
        .into());
    }
    Ok(())
}

/// Execute a planned backup with restic. Ownership of container-owned
/// bind mounts is the pre-hook's job (`podman unshare chown`); by this
/// point every file is readable by the invoking user.
pub fn restic_backup(plan: &BackupRunPlan) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("backup")
        .arg("--repo")
        .arg(&plan.repo)
        .env("RESTIC_PASSWORD", &plan.password);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    for tag in &plan.tags {
        cmd.arg("--tag").arg(tag);
    }
    for excl in &plan.excludes {
        // Excludes from service.toml are relative to the service home,
        // hence cwd below.
        cmd.arg("--exclude").arg(excl);
    }
    cmd.current_dir(&plan.service_home);
    for path in &plan.paths {
        cmd.arg(path);
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawning `restic backup` for {}", plan.service_name))?;
    if !status.success() {
        bail!("restic backup exited with {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Execute a planned restore. Files come back owned by the invoking
/// user; the next container start's `:U` re-chowns to the container's
/// USER. (Running inside `podman unshare` would preserve snapshot UIDs
/// but fails chowning `/home` outside the namespace mapping.)
pub fn restic_restore(plan: &BackupRestorePlan) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("restore")
        .arg(&plan.snapshot)
        .arg("--repo")
        .arg(&plan.repo)
        .arg("--target")
        .arg("/")
        .arg("--tag")
        .arg(format!("service:{}", plan.service_name))
        .env("RESTIC_PASSWORD", &plan.password);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    // Every snapshot bundles the global preferences.toml (for disaster
    // recovery), but a normal per-service restore must NOT overwrite the live
    // global config (SMTP/auth/backup creds/other services) with this snapshot's
    // possibly-stale copy. Exclude it unless the caller opted in.
    if !plan.include_config
        && let Ok(paths) = ConfigPaths::resolve()
    {
        cmd.arg("--exclude").arg(&paths.config_file);
    }
    let status = cmd.status().context("spawning `restic restore`")?;
    if !status.success() {
        bail!("restic restore exited with {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// Execute a planned retention sweep, returning `(kept, removed)` snapshot
/// counts. Runs `restic forget --json` filtered to the service's
/// `service:<name>` tag (so keep rules apply only to that service), parses the
/// keep/remove decision, then runs `restic prune` SEPARATELY to reclaim space
/// (real runs only, when something was actually removed). Splitting prune out
/// keeps the `--json` output clean to parse. In a dry run nothing is deleted
/// and `removed` is the count that WOULD be removed.
pub fn restic_forget(plan: &BackupForgetPlan) -> anyhow::Result<(u32, u32)> {
    use anyhow::{Context, bail};
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("forget")
        .arg("--repo")
        .arg(&plan.repo)
        .arg("--tag")
        .arg(&plan.tag)
        .arg("--json")
        .env("RESTIC_PASSWORD", &plan.password);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    for arg in &plan.keep_args {
        cmd.arg(arg);
    }
    if plan.dry_run {
        cmd.arg("--dry-run");
    }
    let output = cmd
        .output()
        .with_context(|| format!("spawning `restic forget` for {}", plan.service_name))?;
    if !output.status.success() {
        bail!(
            "restic forget exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    // `restic forget --json` is an array of groups, each with a `keep` list and
    // a `remove` list (the latter null/absent when nothing is dropped).
    #[derive(serde::Deserialize)]
    struct ForgetGroup {
        #[serde(default)]
        keep: Vec<serde_json::Value>,
        #[serde(default)]
        remove: Option<Vec<serde_json::Value>>,
    }
    let groups: Vec<ForgetGroup> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let kept: u32 = groups.iter().map(|g| g.keep.len() as u32).sum();
    let removed: u32 = groups
        .iter()
        .map(|g| g.remove.as_ref().map_or(0, Vec::len) as u32)
        .sum();
    // Reclaim space, but only for a real run that actually dropped snapshots.
    if !plan.dry_run && plan.prune && removed > 0 {
        let mut prune = std::process::Command::new("restic");
        prune
            .arg("prune")
            .arg("--repo")
            .arg(&plan.repo)
            .env("RESTIC_PASSWORD", &plan.password);
        for (k, v) in &plan.env {
            prune.env(k, v);
        }
        let status = prune
            .status()
            .with_context(|| format!("spawning `restic prune` for {}", plan.service_name))?;
        if !status.success() {
            bail!("restic prune exited with {}", status.code().unwrap_or(-1));
        }
    }
    Ok((kept, removed))
}

/// KEY=VALUE lines from a `.env` file; malformed lines are skipped the
/// same way systemd's EnvironmentFile= skips them.
pub fn parse_env_file(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// How long to let units settle after a stop before touching their data, so
/// the database has flushed and file handles are closed. Matches the `sleep`
/// the per-service hook scripts used to do.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Stop a service's units for a cold snapshot/restore. Best-effort, like the
/// hook scripts' `systemctl stop ... || true`: a unit that isn't running is not
/// an error. `Requires=` governs startup not shutdown, so every unit is listed.
fn stop_units(units: &[String]) {
    if units.is_empty() {
        return;
    }
    let mut cmd = std::process::Command::new("systemctl");
    cmd.arg("--user").arg("stop");
    for u in units {
        cmd.arg(u);
    }
    let _ = cmd.status();
}

/// Bring a service back up after a cold snapshot/restore: clear any
/// start-limit/failed state left by the stop+start churn, then start the
/// primary unit (`<service>.service`), whose `Requires=` cascades its sidecars.
/// Services that need more (extra units, or a DB-readiness wait) ship a
/// post_backup/post_restore hook instead.
fn start_service(service: &str) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "reset-failed"])
        .status();
    let unit = format!("{service}.service");
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", &unit])
        .status()
        .with_context(|| format!("spawning `systemctl --user start {unit}`"))?;
    if !status.success() {
        bail!(
            "`systemctl --user start {unit}` exited with {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Make container-owned bind mounts readable by the invoking user so restic can
/// snapshot them. `podman unshare chown -R 0:0` maps namespace-root (= this
/// user); the next container start re-applies `:U`. A no-op for data that is
/// already user-owned (no `:U` mount).
fn chown_for_read(paths: &[PathBuf]) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    for p in paths {
        if !p.exists() {
            continue;
        }
        let status = std::process::Command::new("podman")
            .args(["unshare", "chown", "-R", "0:0"])
            .arg(p)
            .status()
            .with_context(|| format!("spawning `podman unshare chown` on {}", p.display()))?;
        if !status.success() {
            bail!("`podman unshare chown` on {} failed", p.display());
        }
    }
    Ok(())
}

/// Wipe a cold service's data dirs to a clean tree before `restic restore`, so
/// files created after the snapshot don't linger and desync the restored state.
/// `podman unshare` so `:U`-chowned (container-uid) trees are removable; the
/// dirs are recreated empty since podman refuses to start a container whose
/// bind-mount source is missing.
fn wipe_for_restore(paths: &[PathBuf]) -> anyhow::Result<()> {
    use anyhow::Context;
    for p in paths {
        let _ = std::process::Command::new("podman")
            .args(["unshare", "rm", "-rf"])
            .arg(p)
            .status();
        std::fs::create_dir_all(p).with_context(|| format!("recreating {}", p.display()))?;
    }
    Ok(())
}

/// Run a planned backup end-to-end.
///
/// `online` services (a live dump, or safe-to-copy flat data) run only their
/// own hooks around restic: `pre_backup` -> restic -> `post_backup`. The post
/// hook runs even when restic fails (it usually cleans up a dump), but its own
/// failure never masks restic's error.
///
/// Cold services (the default) get the full lifecycle ryra derives from their
/// units + paths: stop the stack, make the data readable, optional `pre_backup`
/// prep, restic, then bring the service back (a `post_backup` hook if present,
/// else start the primary unit). The service is always brought back up, even
/// when restic fails, so a failed backup never leaves it down.
pub fn execute_backup_run(plan: &BackupRunPlan) -> anyhow::Result<()> {
    if plan.online {
        if let Some(hook) = &plan.pre_backup_hook {
            run_hook("pre_backup", &plan.service_name, hook, &plan.service_home)?;
        }
        let restic_result = restic_backup(plan);
        if let Some(hook) = &plan.post_backup_hook
            && let Err(e) = run_hook("post_backup", &plan.service_name, hook, &plan.service_home)
            && restic_result.is_ok()
        {
            return Err(e);
        }
        return restic_result;
    }

    // Cold snapshot: ryra owns the stop/chown/restart the hook scripts used to.
    stop_units(&plan.units);
    std::thread::sleep(SETTLE);
    chown_for_read(&plan.data_paths)?;
    if let Some(hook) = &plan.pre_backup_hook {
        run_hook("pre_backup", &plan.service_name, hook, &plan.service_home)?;
    }
    let restic_result = restic_backup(plan);
    // Always bring the service back, even if restic failed.
    let bring_up = match &plan.post_backup_hook {
        Some(hook) => run_hook("post_backup", &plan.service_name, hook, &plan.service_home),
        None => start_service(&plan.service_name),
    };
    match (restic_result, bring_up) {
        (Ok(()), bring) => bring,
        (Err(e), _) => Err(e),
    }
}

/// Run a planned restore end-to-end.
///
/// `online` services run only their own hooks around restic: `pre_restore` ->
/// restic -> `post_restore` (e.g. seafile pauses the app, restores the tree,
/// re-imports a live dump). Cold services (the default) get ryra's derived
/// lifecycle: stop the stack, wipe `data_paths` to a clean tree, optional
/// `pre_restore` (extra wipes), restic restore, then bring the service back (a
/// `post_restore` hook if present — typically a DB-readiness wait — else start
/// the primary unit).
pub fn execute_backup_restore(plan: &BackupRestorePlan) -> anyhow::Result<()> {
    if plan.online {
        if let Some(hook) = &plan.pre_restore_hook {
            run_hook("pre_restore", &plan.service_name, hook, &plan.service_home)?;
        }
        restic_restore(plan)?;
        if let Some(hook) = &plan.post_restore_hook {
            run_hook("post_restore", &plan.service_name, hook, &plan.service_home)?;
        }
        return Ok(());
    }

    // Cold restore: stop, wipe to a clean tree, restore, bring back up.
    stop_units(&plan.units);
    std::thread::sleep(SETTLE);
    wipe_for_restore(&plan.data_paths)?;
    if let Some(hook) = &plan.pre_restore_hook {
        run_hook("pre_restore", &plan.service_name, hook, &plan.service_home)?;
    }
    restic_restore(plan)?;
    match &plan.post_restore_hook {
        Some(hook) => run_hook("post_restore", &plan.service_name, hook, &plan.service_home)?,
        None => start_service(&plan.service_name)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{BackupBackend, BackupSettings};
    use crate::registry::service_def::{
        Arch, BackupConfig, HttpsRequirement, IntegrationFlags, PortDef, ServiceDef, ServiceMeta,
    };

    fn def_with_backup(backup_section: Option<BackupConfig>) -> ServiceDef {
        ServiceDef {
            service: ServiceMeta {
                name: "demo".into(),
                description: "demo".into(),
                url: None,
                kind: Default::default(),
                architecture: vec![Arch::Amd64, Arch::Arm64],
                https: HttpsRequirement::default(),
                runtime: Default::default(),
                run: None,
                build: None,
                post_install: None,
                deploy: Default::default(),
                health_check: None,
                health_timeout: None,
            },
            requirements: None,
            ports: vec![PortDef {
                name: "http".into(),
                container_port: 80,
                host_port: None,
                protocol: Default::default(),
                tailscale_https: None,
            }],
            env: vec![],
            env_groups: vec![],
            choices: vec![],
            requires: vec![],
            mappings: Default::default(),
            integrations: IntegrationFlags {
                backup: backup_section.is_some(),
                ..Default::default()
            },
            capabilities: Default::default(),
            backup: backup_section,
            metrics: None,
        }
    }

    #[test]
    fn resolve_paths_whole_folder_when_paths_empty() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // No explicit `paths` → capture the whole service folder.
        let def = def_with_backup(Some(BackupConfig::default()));
        let (paths, excludes) = resolve_paths(&def.clone(), home).unwrap();
        assert_eq!(paths, vec![home.to_path_buf()]);
        assert!(excludes.is_empty());
    }

    #[test]
    fn resolve_paths_explicit_list_plus_config_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Config artifacts present in the home travel with the data.
        std::fs::write(home.join(".env"), "x").unwrap();
        std::fs::write(home.join("metadata.toml"), "x").unwrap();
        let def = def_with_backup(Some(BackupConfig {
            paths: vec!["data/uploads".into(), ".backup/db.sql".into()],
            exclude: vec!["data/uploads/cache".into()],
            ..Default::default()
        }));
        let (paths, excludes) = resolve_paths(&def, home).unwrap();
        // Curated data paths honoured...
        assert!(paths.contains(&home.join("data/uploads")), "got {paths:?}");
        assert!(
            paths.contains(&home.join(".backup/db.sql")),
            "got {paths:?}"
        );
        // ...and config artifacts added so a restore can rebuild the install.
        assert!(paths.contains(&home.join(".env")), "got {paths:?}");
        assert!(paths.contains(&home.join("metadata.toml")), "got {paths:?}");
        assert_eq!(excludes, vec!["data/uploads/cache"]);
    }

    #[test]
    fn config_artifacts_collects_env_metadata_quadlets_configs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join(".env"), "x").unwrap();
        std::fs::write(home.join("metadata.toml"), "x").unwrap();
        std::fs::write(home.join("service.manifest"), "x").unwrap();
        std::fs::write(home.join("demo.container"), "x").unwrap();
        std::fs::write(home.join("demo.network"), "x").unwrap();
        std::fs::create_dir(home.join("configs")).unwrap();
        let names: Vec<String> = config_artifacts(home)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for want in [
            ".env",
            "metadata.toml",
            "service.manifest",
            "demo.container",
            "demo.network",
            "configs",
        ] {
            assert!(
                names.contains(&want.to_string()),
                "{want} missing: {names:?}"
            );
        }
    }

    #[test]
    fn hook_path_resolves_under_configs_scripts() {
        let home = PathBuf::from("/x/y");
        assert_eq!(
            hook_path(&home, "backup-pre.sh"),
            PathBuf::from("/x/y/configs/scripts/backup-pre.sh")
        );
    }

    #[test]
    fn resolve_hook_prefers_explicit_over_convention() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Both the conventional and a custom-named file exist; the
        // explicit field wins.
        let scripts = home.join("configs").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("backup-pre.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(scripts.join("custom.sh"), "#!/bin/sh\n").unwrap();
        let resolved = resolve_hook(Some("custom.sh"), home, "backup-pre.sh");
        assert_eq!(resolved.unwrap().file_name().unwrap(), "custom.sh");
    }

    #[test]
    fn resolve_hook_falls_back_to_convention_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let scripts = home.join("configs").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("backup-pre.sh"), "#!/bin/sh\n").unwrap();
        let resolved = resolve_hook(None, home, "backup-pre.sh");
        assert_eq!(resolved.unwrap().file_name().unwrap(), "backup-pre.sh");
    }

    #[test]
    fn resolve_hook_returns_none_when_no_script_exists() {
        let dir = tempfile::tempdir().unwrap();
        // No configs/scripts/ at all → no hook to run.
        assert!(resolve_hook(None, dir.path(), "backup-pre.sh").is_none());
    }

    #[test]
    fn manifest_sha256_changes_with_content() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("service.toml"), "v1").unwrap();
        std::fs::write(b.path().join("service.toml"), "v2").unwrap();
        assert_ne!(manifest_sha256(a.path()), manifest_sha256(b.path()));
    }

    #[test]
    fn manifest_sha256_stable_for_identical_content() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("service.toml"), "same").unwrap();
        std::fs::write(b.path().join("service.toml"), "same").unwrap();
        assert_eq!(manifest_sha256(a.path()), manifest_sha256(b.path()));
    }

    #[test]
    fn manifest_sha256_returns_zero_hash_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(manifest_sha256(dir.path()), "0".repeat(64));
    }

    #[test]
    fn service_units_one_per_container_quadlet() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Container quadlets -> .service units; network/volume quadlets don't.
        std::fs::write(home.join("forgejo.container"), "").unwrap();
        std::fs::write(home.join("forgejo-postgres.container"), "").unwrap();
        std::fs::write(home.join("forgejo.network"), "").unwrap();
        assert_eq!(
            service_units(home),
            vec![
                "forgejo-postgres.service".to_string(),
                "forgejo.service".to_string()
            ]
        );
    }

    #[test]
    fn data_paths_are_backup_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let def = def_with_backup(Some(BackupConfig {
            paths: vec!["db-data".into(), "data".into()],
            ..Default::default()
        }));
        assert_eq!(
            data_paths(&def, home),
            vec![home.join("db-data"), home.join("data")]
        );
        // No explicit paths -> nothing to chown/wipe (whole-folder backup).
        let whole = def_with_backup(Some(BackupConfig::default()));
        assert!(data_paths(&whole, home).is_empty());
    }

    #[test]
    fn stop_flags_track_online_and_restore_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Cold (default): both backup and restore stop the service.
        let cold = def_with_backup(Some(BackupConfig::default()));
        assert!(backup_stops_service(&cold));
        assert!(restore_stops_service(&cold, home));

        // Online with no restore hooks: neither stops (e.g. flat/append data).
        let online = def_with_backup(Some(BackupConfig {
            online: true,
            ..Default::default()
        }));
        assert!(!backup_stops_service(&online));
        assert!(!restore_stops_service(&online, home));

        // Online but ships a restore hook (e.g. seafile re-imports a dump):
        // backup runs live, restore still stops.
        let scripts = home.join("configs").join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("restore-post.sh"), "#!/bin/sh\n").unwrap();
        assert!(!backup_stops_service(&online));
        assert!(restore_stops_service(&online, home));

        // No backup support at all: never stops.
        let none = def_with_backup(None);
        assert!(!backup_stops_service(&none));
        assert!(!restore_stops_service(&none, home));
    }

    #[test]
    fn backend_env_map_round_trips_aws_creds() {
        let settings = BackupSettings {
            password: "p".into(),
            backend: BackupBackend::S3 {
                endpoint: "http://h:9000".into(),
                bucket: "b".into(),
                access_key_id: "id".into(),
                secret_access_key: "secret".into(),
                session_token: None,
                prefix: None,
            },
            daily: None,
            weekly: None,
        };
        let env = backend_env_map(&settings.backend);
        assert_eq!(env.get("AWS_ACCESS_KEY_ID"), Some(&"id".to_string()));
        assert_eq!(
            env.get("AWS_SECRET_ACCESS_KEY"),
            Some(&"secret".to_string())
        );
    }
}
