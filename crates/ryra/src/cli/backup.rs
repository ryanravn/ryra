//! `ryra backup`: configure repos, push snapshots, restore, list.
//!
//! All flows shell out to the `restic` binary; ryra-core stays free of
//! the subprocess plumbing so its planning is unit-testable without
//! restic on the test runner.
//!
//! Status state ("when did this service last get backed up?") lives in
//! `~/.local/state/ryra/backup-status.toml`. It's a small read-only
//! convenience for `ryra backup status` and `ryra list`; the source of
//! truth for what's backed up is always the snapshots in the remote
//! restic repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use console::style;
use dialoguer::{Confirm, Input, Password};
use serde::{Deserialize, Serialize};

use ryra_core::REGISTRY_DEFAULT;
use ryra_core::backup::{
    BackupRestorePlan, BackupRunPlan, list_backup_enabled, plan_backup_restore, plan_backup_run,
};
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{BackupBackend, BackupSettings, Config};
use ryra_core::metadata::load_metadata;
use ryra_core::registry::resolve::ServiceRef;

#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// Set up the encrypted backup repository (run once, then again
    /// to change the backend or rotate the password).
    Configure {
        /// `s3` (any S3-compatible store: MinIO, AWS S3, R2, B2-S3,
        /// Wasabi) or `local` (testing only, local disk, no
        /// off-machine protection).
        #[arg(long, value_enum)]
        backend: Option<BackendKind>,
        /// S3 endpoint URL (e.g. http://127.0.0.1:9000 for MinIO,
        /// https://s3.us-east-1.amazonaws.com for AWS).
        #[arg(long)]
        endpoint: Option<String>,
        /// S3 bucket name.
        #[arg(long)]
        bucket: Option<String>,
        /// S3 access key id.
        #[arg(long)]
        access_key_id: Option<String>,
        /// S3 secret access key.
        #[arg(long)]
        secret_access_key: Option<String>,
        /// Optional path prefix inside the bucket. Lets one bucket
        /// host multiple ryra installs.
        #[arg(long)]
        prefix: Option<String>,
        /// Local-backend path. Use only for testing.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Encryption password. Omit to generate a fresh 32-byte
        /// random key (recommended). The password is the only thing
        /// that decrypts snapshots; store it somewhere safe.
        #[arg(long)]
        password: Option<String>,
        /// Skip the "save this password somewhere" interactive
        /// confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Push a snapshot of each backup-enabled install (or just the
    /// listed services) to the configured restic repository.
    Run {
        /// Service name(s). Omit to back up every enabled install.
        services: Vec<String>,
    },
    /// Restore a service's user data from a snapshot.
    Restore {
        /// Service name.
        service: String,
        /// Specific restic snapshot id (hex prefix). Omit to use the
        /// newest snapshot tagged for this service.
        #[arg(long)]
        at: Option<String>,
        /// Restore even if the snapshot was taken against a different
        /// version of the service manifest. May fail to start:
        /// expect to migrate by hand.
        #[arg(long)]
        force: bool,
    },
    /// List snapshots for one or all backup-enabled services.
    List {
        /// Service name(s). Omit to list snapshots for every enabled
        /// install.
        services: Vec<String>,
    },
    /// Show repository overview, per-service last-run timestamps,
    /// and total repo size.
    Status,
    /// Install or remove a systemd --user timer that runs
    /// `ryra backup run` on a schedule.
    Schedule {
        /// `daily` (3am), `weekly` (Sunday 3am), `hourly`, or
        /// `disable` to remove the existing timer.
        interval: ScheduleInterval,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    S3,
    Local,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleInterval {
    Hourly,
    Daily,
    Weekly,
    Disable,
}

impl ScheduleInterval {
    /// The systemd `OnCalendar=` expression for this interval.
    fn on_calendar(self) -> &'static str {
        match self {
            ScheduleInterval::Hourly => "hourly",
            // 3am is the standard low-traffic window; not configurable
            // here because if you want a specific time you can edit
            // ~/.config/systemd/user/ryra-backup.timer directly. Keeping
            // the schedule subcommand to a fixed small set prevents the
            // service from sprouting a half-built cron DSL.
            ScheduleInterval::Daily => "*-*-* 03:00:00",
            ScheduleInterval::Weekly => "Sun *-*-* 03:00:00",
            // unreachable: Disable doesn't write a timer
            ScheduleInterval::Disable => "",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ScheduleInterval::Hourly => "hourly",
            ScheduleInterval::Daily => "daily at 03:00",
            ScheduleInterval::Weekly => "Sunday at 03:00",
            ScheduleInterval::Disable => "disabled",
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(action: BackupAction) -> Result<()> {
    require_restic_installed()?;
    match action {
        BackupAction::Configure {
            backend,
            endpoint,
            bucket,
            access_key_id,
            secret_access_key,
            prefix,
            path,
            password,
            yes,
        } => {
            configure(ConfigureArgs {
                backend,
                endpoint,
                bucket,
                access_key_id,
                secret_access_key,
                prefix,
                path,
                password,
                yes,
            })
            .await
        }
        BackupAction::Run { services } => run_backup(services).await,
        BackupAction::Restore { service, at, force } => restore(service, at, force).await,
        BackupAction::List { services } => list(services).await,
        BackupAction::Status => status().await,
        BackupAction::Schedule { interval } => schedule(interval).await,
    }
}

/// Resolve the registry directory for an installed service. Reads
/// `metadata.toml` to learn which registry the service came from
/// (default vs. custom name), then asks ryra-core to materialise that
/// registry on disk (git clone/pull).
async fn resolve_repo_dir_for_install(service_name: &str) -> Result<PathBuf> {
    let meta = load_metadata(service_name)?.ok_or_else(|| {
        anyhow!(ryra_core::error::Error::ServiceNotInstalled(
            service_name.to_string()
        ))
    })?;
    let service_ref = if meta.registry.is_empty() || meta.registry == REGISTRY_DEFAULT {
        ServiceRef::Default(service_name.to_string())
    } else {
        ServiceRef::Custom {
            registry: meta.registry,
            service: service_name.to_string(),
        }
    };
    Ok(ryra_core::resolve_registry_dir(&service_ref).await?)
}

fn require_restic_installed() -> Result<()> {
    if which::which("restic").is_err() {
        bail!(
            "the `restic` binary is required for `ryra backup`. Install it from your distro \
             package manager (apt install restic, dnf install restic) or from \
             https://restic.net/."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// configure
// ---------------------------------------------------------------------------

struct ConfigureArgs {
    backend: Option<BackendKind>,
    endpoint: Option<String>,
    bucket: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    prefix: Option<String>,
    path: Option<PathBuf>,
    password: Option<String>,
    yes: bool,
}

async fn configure(args: ConfigureArgs) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    let interactive = super::is_interactive();

    // If a backup repo is already configured, default action is "retry
    // init with the saved settings". That recovers cleanly from the
    // common case where a previous `configure` saved settings but the
    // init then failed (backend not yet reachable, ACL not yet
    // propagated, TLS cert still being issued). The user can opt to
    // reconfigure from scratch if they actually want to change backends.
    let mode = if config.backup.is_some() {
        if interactive {
            prompt_existing_config_choice()?
        } else if args.backend.is_some() {
            // Non-interactive caller passed fresh backend flags: honour
            // that as an explicit reconfigure.
            ConfigureMode::Fresh
        } else {
            // Bare `ryra backup configure` after a prior failed init:
            // retry with existing settings.
            ConfigureMode::Retry
        }
    } else {
        ConfigureMode::Fresh
    };

    let settings = match mode {
        ConfigureMode::Retry => config
            .backup
            .clone()
            .ok_or_else(|| anyhow!("retry mode requires existing backup settings"))?,
        ConfigureMode::Fresh => collect_new_settings(&args, interactive)?,
    };

    // Init first, then save. Restic talks to the backend during init,
    // so failures here (auth, network, missing bucket) surface BEFORE
    // we touch preferences.toml. That means a failed configure leaves
    // no stale state behind to clean up.
    init_repo_if_needed(&settings)?;

    if matches!(mode, ConfigureMode::Fresh) {
        config.backup = Some(settings.clone());
        paths.ensure_dirs()?;
        ryra_core::config::save_config(&paths.config_file, &config)?;
        println!(
            "\n  Backup repository saved to {}",
            paths.config_file.display()
        );
    }
    println!(
        "  Repository ready: {}",
        style(settings.backend.restic_repo()).dim()
    );

    // Offer to install a daily systemd timer. Default `no` because
    // not every user wants their first action after configure to be
    // a background scheduled job, but the prompt makes the
    // typical-case answer one keypress away.
    if interactive && !args.yes && read_schedule_state().is_none() {
        let want = Confirm::new()
            .with_prompt("Schedule daily backups at 03:00?")
            .default(false)
            .interact()?;
        if want {
            schedule(ScheduleInterval::Daily).await?;
        }
    }

    Ok(())
}

enum ConfigureMode {
    /// No prior config: prompt, init, save.
    Fresh,
    /// Existing config present: reuse it, only re-run init.
    Retry,
}

fn prompt_existing_config_choice() -> Result<ConfigureMode> {
    println!("\n  A backup repository is already configured.");
    println!("  1. Retry connection         (reuse saved settings)");
    println!("  2. Reconfigure from scratch (replace saved settings)");
    println!("  3. Cancel");
    let choice: u32 = Input::new()
        .with_prompt("Choose")
        .default(1)
        .interact_text()?;
    match choice {
        1 => Ok(ConfigureMode::Retry),
        2 => Ok(ConfigureMode::Fresh),
        3 => bail!("cancelled"),
        n => bail!("invalid choice: {n} (expected 1, 2, or 3)"),
    }
}

fn collect_new_settings(args: &ConfigureArgs, interactive: bool) -> Result<BackupSettings> {
    let kind = match args.backend {
        Some(k) => k,
        None if interactive => prompt_backend()?,
        None => bail!("--backend is required in non-interactive mode (s3 or local)"),
    };

    let backend = match kind {
        BackendKind::S3 => collect_s3(args, interactive)?,
        BackendKind::Local => collect_local(args, interactive)?,
    };

    let password = match &args.password {
        Some(p) if p.trim().is_empty() => bail!("--password may not be empty"),
        Some(p) => p.clone(),
        None => {
            let generated = generate_password();
            if interactive && !args.yes {
                println!(
                    "\n  {}: {}",
                    style("Generated encryption password").bold(),
                    style(&generated).cyan()
                );
                println!(
                    "  Store this somewhere safe: it's the only key that can decrypt your backups."
                );
                let confirm = Confirm::new()
                    .with_prompt("Have you saved the password?")
                    .default(false)
                    .interact()?;
                if !confirm {
                    bail!("aborting: confirm the password is saved before continuing");
                }
            }
            generated
        }
    };

    Ok(BackupSettings { password, backend })
}

fn prompt_backend() -> Result<BackendKind> {
    println!("\nWhich backup backend?");
    println!("  1. S3-compatible  (MinIO, AWS, Backblaze B2, R2, Wasabi)");
    println!("  2. Local path     (testing only, no off-machine protection)");
    let choice: u32 = Input::new()
        .with_prompt("Choose")
        .default(1)
        .interact_text()?;
    match choice {
        1 => Ok(BackendKind::S3),
        2 => Ok(BackendKind::Local),
        n => bail!("expected 1 or 2, got {n}"),
    }
}

fn collect_s3(args: &ConfigureArgs, interactive: bool) -> Result<BackupBackend> {
    let endpoint = match args.endpoint.clone() {
        Some(v) => v,
        None if interactive => Input::new()
            .with_prompt("S3 endpoint URL (e.g. http://127.0.0.1:9000)")
            .interact_text()?,
        None => bail!("--endpoint required for S3 backend"),
    };
    let bucket = match args.bucket.clone() {
        Some(v) => v,
        None if interactive => Input::new().with_prompt("Bucket name").interact_text()?,
        None => bail!("--bucket required for S3 backend"),
    };
    let access_key_id = match args.access_key_id.clone() {
        Some(v) => v,
        None if interactive => Input::new().with_prompt("Access key id").interact_text()?,
        None => bail!("--access-key-id required for S3 backend"),
    };
    let secret_access_key = match args.secret_access_key.clone() {
        Some(v) => v,
        None if interactive => Password::new()
            .with_prompt("Secret access key")
            .interact()?,
        None => bail!("--secret-access-key required for S3 backend"),
    };
    let prefix = args.prefix.clone().filter(|p| !p.is_empty());

    Ok(BackupBackend::S3 {
        endpoint,
        bucket,
        access_key_id,
        secret_access_key,
        prefix,
    })
}

fn collect_local(args: &ConfigureArgs, interactive: bool) -> Result<BackupBackend> {
    let path = match args.path.clone() {
        Some(p) => p,
        None if interactive => {
            let s: String = Input::new()
                .with_prompt("Local repository path")
                .interact_text()?;
            PathBuf::from(s)
        }
        None => bail!("--path required for local backend"),
    };
    Ok(BackupBackend::Local { path })
}

fn generate_password() -> String {
    // 32-char alphanumeric: ~190 bits of entropy, safe to copy-paste,
    // no special characters to escape in shell environments.
    ryra_core::system::secret::generate_secret()
}

fn init_repo_if_needed(settings: &BackupSettings) -> Result<()> {
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("init")
        .arg("--repo")
        .arg(settings.backend.restic_repo())
        .env("RESTIC_PASSWORD", &settings.password)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in settings.backend.env() {
        cmd.env(k, v);
    }
    let output = cmd.output().context("spawning `restic init`")?;
    if output.status.success() {
        println!("  Initialised new restic repository.");
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("already initialized") || stderr.contains("config file already exists") {
        return Ok(());
    }
    bail!("restic init failed: {}", stderr.trim());
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

async fn run_backup(services: Vec<String>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup configure").cyan()
        );
    }

    let targets = if services.is_empty() {
        let enabled = list_backup_enabled()?;
        if enabled.is_empty() {
            println!(
                "No services have backups enabled. Pass {} on `ryra add`, or use \
                 `ryra add <svc> --backup` to opt in.",
                style("--backup").cyan()
            );
            return Ok(());
        }
        enabled
    } else {
        services
    };

    let mut any_failed = false;
    for svc in &targets {
        match run_one(svc, &config).await {
            Ok(()) => {
                record_status(svc, BackupOutcome::Success)?;
            }
            Err(e) => {
                eprintln!("{} {svc}: {e:#}", style("backup failed:").red().bold());
                record_status(svc, BackupOutcome::Failure(e.to_string()))?;
                any_failed = true;
            }
        }
    }
    if any_failed {
        bail!("one or more services failed to back up");
    }
    Ok(())
}

async fn run_one(service_name: &str, config: &Config) -> Result<()> {
    let repo_dir = resolve_repo_dir_for_install(service_name).await?;
    let plan = plan_backup_run(service_name, config, &repo_dir)?;
    println!(
        "\n{} {} ({} path(s))",
        style("backing up:").cyan().bold(),
        plan.service_name,
        plan.paths.len()
    );

    if let Some(hook) = &plan.pre_backup_hook {
        run_hook("pre_backup", &plan.service_name, hook, &plan.service_home)?;
    }

    let restic_result = restic_backup(&plan);

    if let Some(hook) = &plan.post_backup_hook {
        // Run the post hook even if restic failed: its purpose is
        // usually cleanup (removing a dump file). Failing to clean
        // up shouldn't mask the original error.
        if let Err(e) = run_hook("post_backup", &plan.service_name, hook, &plan.service_home)
            && restic_result.is_ok()
        {
            return Err(e);
        }
    }
    restic_result
}

fn restic_backup(plan: &BackupRunPlan) -> Result<()> {
    // restic runs as the ryra user; reading the container-owned
    // bind-mount tree (postgres data, www-data html, etc.) is the
    // hook's job; backup-pre.sh chowns those volumes to namespace
    // root via `podman unshare chown -R 0:0`, so by the time we
    // get here every file is owned by ryra and freely readable.
    // The next container start's `:U` re-chowns to the container's
    // USER, round-tripping the ownership.
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
        // restic --exclude works on patterns relative to the working
        // dir; the backup runs with cwd=service_home so excludes
        // listed in service.toml as `data/cache` resolve correctly.
        cmd.arg("--exclude").arg(excl);
    }
    cmd.current_dir(&plan.service_home);
    for path in &plan.paths {
        // Pass absolute paths so restic's snapshot tree mirrors the
        // service home layout regardless of the cwd setting above.
        cmd.arg(path);
    }
    let status = cmd.status().with_context(|| {
        format!(
            "spawning `podman unshare restic backup` for {}",
            plan.service_name
        )
    })?;
    if !status.success() {
        bail!("restic backup exited with {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

async fn restore(service: String, at: Option<String>, force: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        bail!("no backup repository configured: run `ryra backup configure` first");
    }
    let repo_dir = resolve_repo_dir_for_install(&service).await?;
    let snapshot = at.unwrap_or_else(|| "latest".to_string());
    let plan = plan_backup_restore(&service, &snapshot, &config, &repo_dir)?;

    if !force {
        check_version_match(&plan, &repo_dir).await?;
    }

    println!(
        "\n{} {} (snapshot {})",
        style("restoring:").cyan().bold(),
        plan.service_name,
        plan.snapshot
    );

    if let Some(hook) = &plan.pre_restore_hook {
        run_hook("pre_restore", &plan.service_name, hook, &plan.service_home)?;
    }

    restic_restore(&plan)?;

    if let Some(hook) = &plan.post_restore_hook {
        run_hook("post_restore", &plan.service_name, hook, &plan.service_home)?;
    }
    println!(
        "\n{} {} restored. Run `{}` if the service didn't restart cleanly.",
        style("done:").green().bold(),
        plan.service_name,
        style(format!("systemctl --user restart {}", plan.service_name)).cyan()
    );
    Ok(())
}

async fn check_version_match(plan: &BackupRestorePlan, repo_dir: &Path) -> Result<()> {
    // Snapshots are tagged with `manifest_sha:<16hex>`. Fetch the
    // snapshot's tags via `restic snapshots --json` and compare to the
    // current install's hash so the user gets a loud warning if the
    // data they're restoring predates schema changes.
    let snapshot_tags = list_snapshot_tags(plan, &plan.snapshot)?;
    let backed_up = snapshot_tags
        .iter()
        .find_map(|t| t.strip_prefix("manifest_sha:"))
        .unwrap_or("");

    let svc = ryra_core::registry::find_service(repo_dir, &plan.service_name)?;
    let current = current_manifest_sha(&svc.service_dir);
    let current_short: String = current.chars().take(16).collect();

    if !backed_up.is_empty() && backed_up != current_short {
        return Err(ryra_core::error::Error::BackupVersionMismatch {
            service: plan.service_name.clone(),
            backed_up: backed_up.to_string(),
            current: current_short,
        }
        .into());
    }
    Ok(())
}

fn current_manifest_sha(service_dir: &Path) -> String {
    ryra_core::backup::manifest_sha256(service_dir)
}

fn list_snapshot_tags(plan: &BackupRestorePlan, snapshot: &str) -> Result<Vec<String>> {
    // `latest` is interpreted by restic relative to the host+tag
    // filter, so pass --tag service:<name> --host <hostname> for
    // correctness when the same repo is shared across machines.
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("snapshots")
        .arg("--json")
        .arg("--repo")
        .arg(&plan.repo)
        .arg("--tag")
        .arg(format!("service:{}", plan.service_name))
        .env("RESTIC_PASSWORD", &plan.password);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    if snapshot != "latest" {
        cmd.arg(snapshot);
    }
    let output = cmd.output().context("spawning `restic snapshots`")?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("restic snapshots failed: {err}");
    }
    #[derive(Deserialize)]
    struct Snap {
        #[serde(default)]
        tags: Vec<String>,
    }
    let snaps: Vec<Snap> = serde_json::from_slice(&output.stdout)
        .with_context(|| "parsing `restic snapshots --json` output")?;
    let last = snaps.into_iter().next_back().ok_or_else(|| {
        anyhow!(ryra_core::error::Error::BackupNoSnapshots(
            plan.service_name.clone()
        ))
    })?;
    Ok(last.tags)
}

fn restic_restore(plan: &BackupRestorePlan) -> Result<()> {
    // restic runs as the ryra user. The snapshot files come back
    // owned by ryra; the next container start's `:U` walks the
    // bind-mount tree and chowns to the container's USER, so the
    // service sees the right ownership. (Running inside `podman
    // unshare` would let restic preserve the snapshot's UIDs but
    // also forces it to try to chown `/home` and `/home/ryra`,
    // which sit outside the user namespace's mapping and fail.)
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

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(services: Vec<String>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let settings = config
        .backup
        .as_ref()
        .ok_or_else(|| anyhow!("no backup repository configured"))?;

    let targets = if services.is_empty() {
        list_backup_enabled()?
    } else {
        services
    };
    if targets.is_empty() {
        println!("No services with backups enabled.");
        return Ok(());
    }

    for svc in &targets {
        println!("\n{} {}", style("service:").cyan().bold(), svc);
        let mut cmd = std::process::Command::new("restic");
        cmd.arg("snapshots")
            .arg("--repo")
            .arg(settings.backend.restic_repo())
            .arg("--tag")
            .arg(format!("service:{svc}"))
            .env("RESTIC_PASSWORD", &settings.password);
        for (k, v) in settings.backend.env() {
            cmd.env(k, v);
        }
        let status = cmd.status().context("spawning `restic snapshots`")?;
        if !status.success() {
            eprintln!(
                "{} restic snapshots failed for {svc}",
                style("warning:").yellow()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status() -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let Some(settings) = config.backup.as_ref() else {
        println!("Backup not configured. Run `ryra backup configure` first.");
        return Ok(());
    };

    println!(
        "  Repository: {}",
        style(settings.backend.restic_repo()).dim()
    );
    match read_schedule_state() {
        Some(ScheduleState { interval, next_run }) => {
            println!(
                "  Schedule:   {} (next: {})",
                style(interval).green(),
                style(next_run.unwrap_or_else(|| "?".into())).dim()
            );
        }
        None => {
            println!(
                "  Schedule:   {} ({} to enable)",
                style("none").yellow(),
                style("ryra backup schedule daily").cyan()
            );
        }
    }

    let enabled = list_backup_enabled()?;
    if enabled.is_empty() {
        println!("  No services have backups enabled.");
        return Ok(());
    }

    let status_db = load_status_db().unwrap_or_default();
    println!("\n  Enabled services:");
    for svc in &enabled {
        let line = match status_db.get(svc) {
            Some(entry) => match &entry.outcome {
                BackupOutcomeRecord::Success => {
                    format!(
                        "    {} {:<20} last run: {} {}",
                        style("✓").green(),
                        svc,
                        entry.timestamp,
                        style("(success)").green()
                    )
                }
                BackupOutcomeRecord::Failure(msg) => {
                    format!(
                        "    {} {:<20} last run: {} {} {}",
                        style("✗").red(),
                        svc,
                        entry.timestamp,
                        style("(failed)").red(),
                        style(msg).dim()
                    )
                }
            },
            None => format!(
                "    {} {:<20} {}",
                style("·").dim(),
                svc,
                style("never run").yellow()
            ),
        };
        println!("{line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Schedule (systemd --user timer)
// ---------------------------------------------------------------------------

/// Unit names + paths for the user-level backup timer. Kept as
/// constants so installing and removing reference the same files.
const TIMER_UNIT: &str = "ryra-backup.timer";
const SERVICE_UNIT: &str = "ryra-backup.service";

fn systemd_user_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine $XDG_CONFIG_HOME"))?;
    Ok(base.join("systemd").join("user"))
}

async fn schedule(interval: ScheduleInterval) -> Result<()> {
    let dir = systemd_user_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    let timer_path = dir.join(TIMER_UNIT);
    let service_path = dir.join(SERVICE_UNIT);

    if interval == ScheduleInterval::Disable {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "disable", "--now", TIMER_UNIT])
            .status();
        // Best-effort file removal; missing files mean "already gone."
        let _ = std::fs::remove_file(&timer_path);
        let _ = std::fs::remove_file(&service_path);
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("  {} timer removed.", style("ryra-backup").cyan());
        return Ok(());
    }

    // Find the installed ryra binary so the unit file points at the
    // same one the user just invoked. `current_exe()` gives the
    // absolute path, which is more robust than `ryra` in the unit's
    // PATH (especially for ~/.cargo/bin/ryra or release tarball
    // installs where $PATH at boot differs from the login shell).
    let exe = std::env::current_exe()
        .context("locating the current ryra binary")?
        .canonicalize()
        .context("resolving ryra binary path")?;

    std::fs::write(
        &service_path,
        format!(
            "[Unit]\n\
             Description=Ryra: push encrypted snapshots of every backup-enabled service\n\
             # Network is needed for S3-backed remotes; harmless for local repos.\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exe} backup run\n\
             # Don't keep restarting if a single backup fails; the\n\
             # next scheduled fire will try again. Status DB records\n\
             # the failure so `ryra backup status` shows it.\n\
             Restart=no\n",
            exe = exe.display(),
        ),
    )
    .with_context(|| format!("write {}", service_path.display()))?;

    std::fs::write(
        &timer_path,
        format!(
            "[Unit]\n\
             Description=Ryra backup timer ({label})\n\
             \n\
             [Timer]\n\
             OnCalendar={on_calendar}\n\
             # Run a missed schedule when the host comes back up\n\
             # (laptops, after a reboot, after suspend).\n\
             Persistent=true\n\
             Unit={service}\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            label = interval.label(),
            on_calendar = interval.on_calendar(),
            service = SERVICE_UNIT,
        ),
    )
    .with_context(|| format!("write {}", timer_path.display()))?;

    let reload = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("systemctl --user daemon-reload")?;
    if !reload.success() {
        bail!("systemctl --user daemon-reload failed");
    }
    let enable = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", TIMER_UNIT])
        .status()
        .context("systemctl --user enable --now ryra-backup.timer")?;
    if !enable.success() {
        bail!("could not enable {TIMER_UNIT}");
    }

    println!(
        "  {} scheduled: {}",
        style("ryra-backup").cyan(),
        style(interval.label()).green()
    );
    super::linger::warn_if_disabled().await?;
    Ok(())
}

/// State of the backup timer, if any. Used by `status` to show
/// whether scheduled runs are wired up and when the next one fires.
struct ScheduleState {
    interval: String,
    next_run: Option<String>,
}

fn read_schedule_state() -> Option<ScheduleState> {
    // If the timer unit file doesn't exist, the timer isn't
    // installed. Cheap check; avoids a `systemctl` fork on every
    // `ryra backup status` invocation when no timer is configured.
    let dir = systemd_user_dir().ok()?;
    if !dir.join(TIMER_UNIT).exists() {
        return None;
    }

    // Read the OnCalendar back from the unit so we don't have to
    // mirror the value in two places.
    let content = std::fs::read_to_string(dir.join(TIMER_UNIT)).ok()?;
    let interval = content
        .lines()
        .find_map(|l| l.strip_prefix("OnCalendar="))
        .unwrap_or("?")
        .to_string();

    // Next-run is best-effort: ask systemctl, parse `Next` row. Fail
    // open (None) on any error so a stale unit doesn't break status.
    let next_run = std::process::Command::new("systemctl")
        .args([
            "--user",
            "list-timers",
            "--no-pager",
            "--no-legend",
            TIMER_UNIT,
        ])
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            // Format: NEXT LEFT LAST PASSED UNIT ACTIVATES
            // Take everything before the first sequence of >=2 spaces
            // after the timestamp to capture the "next run" timestamp.
            let first_line = text.lines().next()?;
            let stripped = first_line.trim();
            if stripped.is_empty() {
                None
            } else {
                // The first two fields concatenated are the absolute
                // timestamp (e.g. "Thu 2026-05-22 03:00:00 CEST").
                Some(
                    stripped
                        .split_whitespace()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        });

    Some(ScheduleState { interval, next_run })
}

// ---------------------------------------------------------------------------
// Hook execution
// ---------------------------------------------------------------------------

fn run_hook(kind: &str, service: &str, script: &Path, service_home: &Path) -> Result<()> {
    if !script.exists() {
        return Err(ryra_core::error::Error::BackupHookFailed {
            service: service.to_string(),
            hook: kind.to_string(),
            message: format!("hook script not found: {}", script.display()),
        }
        .into());
    }
    println!("  {} {}", style("hook").dim(), kind);

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
        return Err(ryra_core::error::Error::BackupHookFailed {
            service: service.to_string(),
            hook: kind.to_string(),
            message: format!("hook script exited with {}", status.code().unwrap_or(-1)),
        }
        .into());
    }
    Ok(())
}

fn parse_env_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Status DB
// ---------------------------------------------------------------------------

enum BackupOutcome {
    Success,
    Failure(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
enum BackupOutcomeRecord {
    Success,
    Failure(String),
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct StatusDb {
    #[serde(default)]
    entries: BTreeMap<String, StatusEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct StatusEntry {
    timestamp: String,
    outcome: BackupOutcomeRecord,
}

impl StatusDb {
    fn get(&self, svc: &str) -> Option<&StatusEntry> {
        self.entries.get(svc)
    }
}

fn status_db_path() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .ok_or_else(|| anyhow!("could not determine state dir"))?;
    Ok(base.join("ryra").join("backup-status.toml"))
}

fn load_status_db() -> Result<StatusDb> {
    let path = status_db_path()?;
    if !path.exists() {
        return Ok(StatusDb::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let db: StatusDb =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(db)
}

fn record_status(service: &str, outcome: BackupOutcome) -> Result<()> {
    let path = status_db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let mut db = load_status_db().unwrap_or_default();
    let outcome_record = match outcome {
        BackupOutcome::Success => BackupOutcomeRecord::Success,
        BackupOutcome::Failure(msg) => BackupOutcomeRecord::Failure(msg),
    };
    db.entries.insert(
        service.to_string(),
        StatusEntry {
            timestamp: now_utc(),
            outcome: outcome_record,
        },
    );
    let text = toml::to_string_pretty(&db).context("serialize status db")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn now_utc() -> String {
    use chrono::Utc;
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
