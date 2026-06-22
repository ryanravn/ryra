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

use ryra_core::backup::{plan_backup_restore, plan_backup_run, plan_mode_prune, restic_forget};
use ryra_core::config::schema::{BackupBackend, BackupSettings, Config, MachineConfig};

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
                daily: None,
                weekly: None,
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

fn restic_backup_one(sandbox: &Sandbox, service: &str, mode: &str) {
    let plan =
        plan_backup_run(service, &sandbox.config, &sandbox.registry_dir, mode).expect("plan run");
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
    restic_backup_one(&sandbox, service, "manual");

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

    let plan_v1 = plan_backup_run(service, &sandbox.config, &sandbox.registry_dir, "manual").unwrap();
    let sha_v1 = extract_manifest_sha(&plan_v1.tags);

    // Mutate the registry's service.toml. A snapshot taken now should
    // carry a different manifest_sha tag, which is the whole point of
    // version-aware restores: a future `ryra backup restore` will
    // refuse to clobber a current install with mismatching tags.
    let svc_toml = sandbox.registry_dir.join(service).join("service.toml");
    let mut content = std::fs::read_to_string(&svc_toml).unwrap();
    content.push_str("\n# extra comment to bump the hash\n");
    std::fs::write(&svc_toml, content).unwrap();

    let plan_v2 = plan_backup_run(service, &sandbox.config, &sandbox.registry_dir, "manual").unwrap();
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
    let sandbox = Sandbox::new(service);
    sandbox.install(service);
    sandbox.write_data_file("data/f.txt", "v1");
    sandbox.init_restic();

    // Three distinct DAILY snapshots, plus one MANUAL that must survive the
    // per-mode prune (manual is unlimited).
    restic_backup_one(&sandbox, service, "daily");
    sandbox.write_data_file("data/f.txt", "v2");
    restic_backup_one(&sandbox, service, "daily");
    sandbox.write_data_file("data/f.txt", "v3");
    restic_backup_one(&sandbox, service, "daily");
    restic_backup_one(&sandbox, service, "manual");

    // Prune the DAILY mode to keep-last 1: 2 of the 3 dailies go; the manual is
    // untouched (different mode).
    let plan = plan_mode_prune(service, &sandbox.config, "daily", 1, false)
        .expect("plan prune")
        .expect("keep > 0 yields a plan");
    let (kept, removed) = restic_forget(&plan).expect("restic forget");
    assert_eq!(kept, 1, "keep-last 1 keeps exactly one daily");
    assert_eq!(removed, 2, "the other two dailies are removed");

    // The manual snapshot still lists (mode prune never touched it).
    let manual_left = plan_mode_prune(service, &sandbox.config, "manual", 1, true)
        .expect("plan dry prune")
        .expect("plan");
    let (manual_kept, _) = restic_forget(&manual_left).expect("dry forget manual");
    assert_eq!(manual_kept, 1, "the manual snapshot survived the daily prune");
}

#[test]
fn machine_id_mints_persists_and_is_stable() {
    // No restic needed — pure config identity.
    let _guard = env_lock();
    unsafe { std::env::remove_var("RYRA_MACHINE_ID") };
    let _sandbox = Sandbox::new("mid-stable"); // sets HOME/XDG into a tempdir
    let paths = ryra_core::config::ConfigPaths::resolve().expect("paths");
    let id1 = ryra_core::config::machine_id(&paths).expect("mint");
    assert!(!id1.is_empty(), "an id should be minted");
    let id2 = ryra_core::config::machine_id(&paths).expect("again");
    assert_eq!(id1, id2, "machine id must be stable across calls (no re-mint)");
    let cfg = ryra_core::config::load_or_default(&paths.config_file).expect("load");
    assert_eq!(
        cfg.machine.expect("persisted to [machine]").id,
        id1,
        "the minted id is persisted, so a rename/restart never re-mints"
    );
}

#[test]
fn machine_id_adopts_orchestrator_env() {
    let _guard = env_lock();
    let _sandbox = Sandbox::new("mid-managed");
    unsafe { std::env::set_var("RYRA_MACHINE_ID", "orch-id-abc") };
    let paths = ryra_core::config::ConfigPaths::resolve().expect("paths");
    let id = ryra_core::config::machine_id(&paths).expect("mint");
    unsafe { std::env::remove_var("RYRA_MACHINE_ID") };
    assert_eq!(id, "orch-id-abc", "managed boxes adopt RYRA_MACHINE_ID");
}

#[test]
fn plan_tags_include_machine_id() {
    let _guard = env_lock();
    let service = "mid-tag";
    let mut sandbox = Sandbox::new(service);
    sandbox.install(service);
    sandbox.config.machine = Some(MachineConfig { id: "MID-XYZ".into() });
    let plan =
        plan_backup_run(service, &sandbox.config, &sandbox.registry_dir, "manual").expect("plan");
    assert!(
        plan.tags.iter().any(|t| t == "machine_id:MID-XYZ"),
        "snapshot must be tagged with the machine id; got {:?}",
        plan.tags
    );
}

#[test]
fn prune_is_none_when_keep_zero() {
    // Pure planner check (no restic needed): keep == 0 means unlimited => Ok(None).
    let _guard = env_lock();
    let service = "demo-noretention";
    let sandbox = Sandbox::new(service);
    sandbox.install(service);
    assert!(
        plan_mode_prune(service, &sandbox.config, "daily", 0, false)
            .expect("plan")
            .is_none(),
        "keep == 0 should plan to a no-op (None)"
    );
}
