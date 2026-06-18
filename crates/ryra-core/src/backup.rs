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
    pub pre_restore_hook: Option<PathBuf>,
    pub post_restore_hook: Option<PathBuf>,
}

/// Plan a `ryra backup run <service>` invocation. Errors loudly when:
/// - the service isn't installed,
/// - its install metadata didn't opt into backups (`--backup` wasn't
///   passed at `ryra add`),
/// - the user hasn't run `ryra backup config` yet,
/// - the service author hasn't declared backup support (defensive —
///   the install-time check should have caught this earlier, but a
///   manifest change between install and backup is possible).
pub fn plan_backup_run(
    service_name: &str,
    config: &Config,
    repo_dir: &Path,
) -> Result<BackupRunPlan> {
    let metadata = load_install_metadata(service_name)?;
    if !metadata.backup_enabled {
        return Err(Error::BackupNotEnabled(service_name.to_string()));
    }
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

    Ok(BackupRunPlan {
        service_name: service_name.to_string(),
        service_home: home,
        repo: settings.backend.restic_repo(),
        password: settings.password.clone(),
        env: backend_env_map(&settings.backend),
        tags,
        paths,
        excludes,
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
    let metadata = load_install_metadata(service_name)?;
    if !metadata.backup_enabled {
        return Err(Error::BackupNotEnabled(service_name.to_string()));
    }
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

    Ok(BackupRestorePlan {
        service_name: service_name.to_string(),
        service_home: home,
        repo: settings.backend.restic_repo(),
        password: settings.password.clone(),
        env: backend_env_map(&settings.backend),
        snapshot: snapshot.to_string(),
        pre_restore_hook: pre,
        post_restore_hook: post,
    })
}

/// List installed services that have `backup_enabled = true` in their
/// metadata. The CLI's `ryra backup run` (no service argument) uses
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
    let status = cmd.status().context("spawning `restic restore`")?;
    if !status.success() {
        bail!("restic restore exited with {}", status.code().unwrap_or(-1));
    }
    Ok(())
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

/// Run a planned backup end-to-end: pre hook, restic, post hook. The
/// post hook runs even when restic fails (it usually cleans up a dump
/// file), but its own failure never masks restic's error.
pub fn execute_backup_run(plan: &BackupRunPlan) -> anyhow::Result<()> {
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
    restic_result
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
        };
        let env = backend_env_map(&settings.backend);
        assert_eq!(env.get("AWS_ACCESS_KEY_ID"), Some(&"id".to_string()));
        assert_eq!(
            env.get("AWS_SECRET_ACCESS_KEY"),
            Some(&"secret".to_string())
        );
    }
}
