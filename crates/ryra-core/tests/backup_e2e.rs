//! End-to-end test of the backup planner against a real `restic` binary
//! pointed at a local on-disk repository. The whole flow runs inside a
//! tempdir — no podman, no systemd, no MinIO — so it's safe to run in
//! CI on hosts that just have restic installed.
//!
//! Skipped at runtime when `restic` isn't on `$PATH`. This is the right
//! gate because the CLI layer also refuses to run without restic; we
//! don't want CI to fail noisily when the dependency is genuinely
//! absent.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

use ryra_core::backup::{plan_backup_forget, plan_backup_restore, plan_backup_run, restic_forget};
use ryra_core::config::schema::{BackupBackend, BackupSettings, Config, RetentionPolicy};

/// Tests in this file all mutate process-global env vars (`HOME`,
/// `XDG_*`) so they can't safely run in parallel. cargo test defaults
/// to one thread per CPU within a binary, so we serialise via a
/// process-wide mutex. Each test takes the guard at the top of its
/// body; tests that share a `Sandbox` hold it for that sandbox's
/// lifetime.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock poisoned")
}

fn restic_available() -> bool {
    Command::new("restic")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// One installed-and-configured ryra state living entirely in a
/// tempdir. The constructor wires up `HOME` and `XDG_*` so any ryra
/// helper that reads them lands in the sandbox.
struct Sandbox {
    _tmp: tempfile::TempDir,
    service_home: PathBuf,
    registry_dir: PathBuf,
    repo_path: PathBuf,
    config: Config,
}

impl Sandbox {
    fn new(service: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let home: PathBuf = tmp.path().to_path_buf();
        // Point every XDG var at the sandbox so service_home() and
        // related path helpers stay inside it.
        // SAFETY: tests run single-threaded enough for this; rayon-
        // parallel cargo tests don't race on these vars within one
        // test binary because they're set before any test code runs.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
            std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        }

        let service_home = home.join(".local/share/services").join(service);
        std::fs::create_dir_all(&service_home).expect("svc home");

        // Bundled-equivalent registry layout: one service dir holding
        // a service.toml whose [integrations] enables backup. The
        // planner reads this via registry::find_service(repo_dir, name).
        let registry_dir = home.join("fake-registry");
        let service_dir = registry_dir.join(service);
        std::fs::create_dir_all(&service_dir).expect("svc registry");
        std::fs::write(
            service_dir.join("service.toml"),
            format!(
                r#"
[service]
name = "{service}"
description = "test service"
kind = "infrastructure"
architecture = ["amd64", "arm64"]

[[ports]]
name = "http"
container_port = 8080

[integrations]
backup = true
"#,
            ),
        )
        .expect("write service.toml");
        // Empty quadlets dir so registry::find_service is happy.
        std::fs::create_dir_all(service_dir.join("quadlets")).expect("quadlets dir");
        std::fs::write(
            service_dir
                .join("quadlets")
                .join(format!("{service}.container")),
            "",
        )
        .expect("empty container file");

        let repo_path = home.join("restic-repo");

        let config = Config {
            backup: Some(BackupSettings {
                password: "test-password-abcdef1234567890".into(),
                backend: BackupBackend::Local {
                    path: repo_path.clone(),
                },
                retention: None,
            }),
            ..Config::default()
        };

        Self {
            _tmp: tmp,
            service_home,
            registry_dir,
            repo_path,
            config,
        }
    }

    /// Mark the service as installed by writing its metadata.toml with
    /// backup enabled.
    fn install(&self, service: &str) {
        let meta = r#"
registry = "fake"
backup_enabled = true
"#;
        std::fs::write(self.service_home.join("metadata.toml"), meta).expect("metadata");
        // `is_service_installed` looks for the marker'd .container in
        // the quadlet dir — not needed here because `plan_backup_run`
        // doesn't call that helper, only `load_metadata`.
        let _ = service;
    }

    fn write_data_file(&self, rel: &str, content: &str) {
        let path = self.service_home.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir for data");
        }
        std::fs::write(&path, content).expect("write data");
    }

    fn init_restic(&self) {
        let status = Command::new("restic")
            .arg("init")
            .arg("--repo")
            .arg(&self.repo_path)
            .env(
                "RESTIC_PASSWORD",
                &self.config.backup.as_ref().unwrap().password,
            )
            .status()
            .expect("spawn restic init");
        assert!(status.success(), "restic init failed");
    }
}

fn restic_backup_one(sandbox: &Sandbox, service: &str) {
    let plan = plan_backup_run(service, &sandbox.config, &sandbox.registry_dir).expect("plan run");
    assert!(plan.tags.contains(&format!("service:{service}")));
    assert!(
        plan.tags.iter().any(|t| t.starts_with("manifest_sha:")),
        "snapshots should carry a manifest hash, got tags = {:?}",
        plan.tags
    );
    let mut cmd = Command::new("restic");
    cmd.arg("backup")
        .arg("--repo")
        .arg(&plan.repo)
        .env("RESTIC_PASSWORD", &plan.password)
        .current_dir(&plan.service_home);
    for tag in &plan.tags {
        cmd.arg("--tag").arg(tag);
    }
    for excl in &plan.excludes {
        cmd.arg("--exclude").arg(excl);
    }
    for path in &plan.paths {
        cmd.arg(path);
    }
    let status = cmd.status().expect("spawn restic backup");
    assert!(status.success(), "restic backup failed");
}

fn restic_restore_one(sandbox: &Sandbox, service: &str) {
    let plan = plan_backup_restore(service, "latest", &sandbox.config, &sandbox.registry_dir)
        .expect("plan restore");
    let status = Command::new("restic")
        .arg("restore")
        .arg(&plan.snapshot)
        .arg("--repo")
        .arg(&plan.repo)
        .arg("--target")
        .arg("/")
        .arg("--tag")
        .arg(format!("service:{}", plan.service_name))
        .env("RESTIC_PASSWORD", &plan.password)
        .status()
        .expect("spawn restic restore");
    assert!(status.success(), "restic restore failed");
}

#[test]
fn round_trip_backup_and_restore() {
    let _guard = env_lock();
    if !restic_available() {
        eprintln!("skipping: restic not on PATH");
        return;
    }
    let service = "demo-backup";
    let sandbox = Sandbox::new(service);
    sandbox.install(service);
    sandbox.write_data_file("data/important.txt", "hello world");
    sandbox.write_data_file("data/another.txt", "more data");
    sandbox.init_restic();

    // 1. Back up the live state.
    restic_backup_one(&sandbox, service);

    // 2. Mutate the live state — delete one file, change another.
    std::fs::remove_file(sandbox.service_home.join("data/important.txt")).expect("delete");
    std::fs::write(
        sandbox.service_home.join("data/another.txt"),
        "MUTATED CONTENT",
    )
    .expect("mutate");

    // 3. Restore. Restic's --target=/ pastes the absolute paths back
    //    over the live tree, so we should see the originals come back.
    restic_restore_one(&sandbox, service);

    let restored_a = std::fs::read_to_string(sandbox.service_home.join("data/important.txt"))
        .expect("important.txt should exist after restore");
    assert_eq!(restored_a, "hello world");
    let restored_b = std::fs::read_to_string(sandbox.service_home.join("data/another.txt"))
        .expect("another.txt should exist after restore");
    assert_eq!(restored_b, "more data");
}

#[test]
fn manifest_sha_changes_between_snapshots_when_definition_changes() {
    let _guard = env_lock();
    if !restic_available() {
        eprintln!("skipping: restic not on PATH");
        return;
    }
    let service = "version-aware";
    let sandbox = Sandbox::new(service);
    sandbox.install(service);
    sandbox.write_data_file("data/x.txt", "snapshot 1");
    sandbox.init_restic();

    let plan_v1 = plan_backup_run(service, &sandbox.config, &sandbox.registry_dir).unwrap();
    let sha_v1 = extract_manifest_sha(&plan_v1.tags);

    // Mutate the registry's service.toml. A snapshot taken now should
    // carry a different manifest_sha tag, which is the whole point of
    // version-aware restores: a future `ryra backup restore` will
    // refuse to clobber a current install with mismatching tags.
    let svc_toml = sandbox.registry_dir.join(service).join("service.toml");
    let mut content = std::fs::read_to_string(&svc_toml).unwrap();
    content.push_str("\n# extra comment to bump the hash\n");
    std::fs::write(&svc_toml, content).unwrap();

    let plan_v2 = plan_backup_run(service, &sandbox.config, &sandbox.registry_dir).unwrap();
    let sha_v2 = extract_manifest_sha(&plan_v2.tags);
    assert_ne!(sha_v1, sha_v2, "manifest_sha must change with the file");
}

fn extract_manifest_sha(tags: &[String]) -> String {
    tags.iter()
        .find_map(|t| t.strip_prefix("manifest_sha:"))
        .expect("snapshot must carry manifest_sha tag")
        .to_string()
}

#[test]
fn retention_forget_prunes_to_keep_last() {
    let _guard = env_lock();
    if !restic_available() {
        eprintln!("skipping: restic not on PATH");
        return;
    }
    let service = "demo-retention";
    let mut sandbox = Sandbox::new(service);
    sandbox.install(service);
    sandbox.write_data_file("data/f.txt", "v1");
    sandbox.init_restic();
    // Keep only the single most recent snapshot.
    sandbox.config.backup.as_mut().unwrap().retention = Some(RetentionPolicy {
        keep_last: 1,
        keep_daily: 0,
        keep_weekly: 0,
        keep_monthly: 0,
    });

    // Three distinct snapshots.
    restic_backup_one(&sandbox, service);
    sandbox.write_data_file("data/f.txt", "v2");
    restic_backup_one(&sandbox, service);
    sandbox.write_data_file("data/f.txt", "v3");
    restic_backup_one(&sandbox, service);

    // Real sweep: keep 1, remove the other 2 (then prune).
    let plan = plan_backup_forget(service, &sandbox.config, false)
        .expect("plan forget")
        .expect("retention policy present");
    let (kept, removed) = restic_forget(&plan).expect("restic forget");
    assert_eq!(kept, 1, "keep-last 1 should keep exactly one snapshot");
    assert_eq!(removed, 2, "the other two should be removed");

    // A dry run on the now-pruned repo previews zero removals and deletes nothing.
    let dry = plan_backup_forget(service, &sandbox.config, true)
        .expect("plan dry-run forget")
        .expect("retention policy present");
    let (kept2, removed2) = restic_forget(&dry).expect("restic forget dry-run");
    assert_eq!(kept2, 1);
    assert_eq!(removed2, 0, "nothing left to prune");
}

#[test]
fn forget_is_none_without_a_policy() {
    // Pure planner check (no restic needed): absent policy => Ok(None).
    let _guard = env_lock();
    let service = "demo-noretention";
    let sandbox = Sandbox::new(service);
    sandbox.install(service);
    assert!(
        plan_backup_forget(service, &sandbox.config, false)
            .expect("plan")
            .is_none(),
        "no retention configured should plan to a no-op (None)"
    );
}
