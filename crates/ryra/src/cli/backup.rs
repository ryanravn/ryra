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
    BackupRestorePlan, list_backup_enabled, plan_backup_restore, plan_backup_run, plan_mode_prune,
    restic_forget, restic_restore, run_hook,
};
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{BackupBackend, BackupSettings, Config, ScheduleMode};
use ryra_core::metadata::load_metadata;
use ryra_core::registry::resolve::ServiceRef;

#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// Set up the encrypted backup repository (run once, then again
    /// to change the backend or rotate the password).
    #[command(name = "config", alias = "configure")]
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
    /// Push a snapshot of each backup-enabled install (or just the listed
    /// services). Hand runs are `manual` (kept forever); the daily/weekly
    /// timers pass `--mode` so their snapshots are capped by the schedule.
    Run {
        /// Service name(s). Omit to back up every enabled install.
        services: Vec<String>,
        /// Cadence tag for these snapshots. `manual` (default) is never pruned.
        #[arg(long, value_enum, default_value_t = BackupMode::Manual)]
        mode: BackupMode,
    },
    /// Restore from a snapshot. With a service name, restores just that
    /// install's folder. With no name, performs full disaster recovery:
    /// every service in the repo is restored, re-linked, and started
    /// (needs your `preferences.toml` in place for the repo creds).
    Restore {
        /// Service name. Omit to restore everything (disaster recovery).
        service: Option<String>,
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
    /// List backups grouped by mode (daily / weekly / manual), for one or all
    /// backup-enabled services.
    List {
        /// Service name(s). Omit to list snapshots for every enabled
        /// install.
        services: Vec<String>,
    },
    /// Show repository overview, per-service last-run timestamps,
    /// and total repo size.
    Status,
    /// Turn a scheduled cadence on or off. `daily`/`weekly` install a systemd
    /// --user timer that runs `ryra backup run --mode <cadence>` and keeps the
    /// last `--keep`; `--off` removes it. Manual backups are always available
    /// and unlimited.
    Schedule {
        /// `daily` or `weekly`.
        cadence: ScheduleCadence,
        /// How many of this cadence's snapshots to keep (default 7). Older ones
        /// are pruned automatically after each run.
        #[arg(long)]
        keep: Option<u32>,
        /// Time of day, 24h `HH:MM` (default 03:00).
        #[arg(long)]
        at: Option<String>,
        /// Remove this cadence's schedule (stop taking it).
        #[arg(long)]
        off: bool,
    },
    /// Prune scheduled (daily/weekly) snapshots to their keep-counts now. Manual
    /// snapshots are never pruned. Runs automatically after each scheduled
    /// backup; use this on demand or to preview with `--dry-run`.
    Forget {
        /// Service name(s). Omit to sweep every enrolled install.
        services: Vec<String>,
        /// Show what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Managed,
    S3,
    Local,
}

/// Which cadence a backup snapshot belongs to. `manual` is unlimited (never
/// pruned); `daily` and `weekly` are capped by the schedule.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupMode {
    Daily,
    Weekly,
    Manual,
}

impl BackupMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupMode::Daily => "daily",
            BackupMode::Weekly => "weekly",
            BackupMode::Manual => "manual",
        }
    }
}

/// A schedulable cadence -- [`BackupMode`] without `manual` (you can't schedule
/// manual backups; they're always available on demand).
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleCadence {
    Daily,
    Weekly,
}

impl ScheduleCadence {
    fn as_str(self) -> &'static str {
        match self {
            ScheduleCadence::Daily => "daily",
            ScheduleCadence::Weekly => "weekly",
        }
    }
}

/// The systemd `OnCalendar=` expression for a cadence at `at` (HH:MM). Weekly
/// fires Sunday; daily every day.
fn on_calendar(cadence: &str, at: &str) -> String {
    if cadence == "weekly" {
        format!("Sun *-*-* {at}:00")
    } else {
        format!("*-*-* {at}:00")
    }
}

/// Validate + normalize a 24h `HH:MM` time. `None` -> the 03:00 default
/// (the standard low-traffic window).
fn parse_schedule_time(at: Option<&str>) -> Result<String> {
    let raw = at.unwrap_or("03:00").trim();
    let (h, m) = raw
        .split_once(':')
        .ok_or_else(|| anyhow!("time must be HH:MM (24h), e.g. 03:00"))?;
    let h: u32 = h.parse().map_err(|_| anyhow!("invalid hour in '{raw}'"))?;
    let m: u32 = m.parse().map_err(|_| anyhow!("invalid minute in '{raw}'"))?;
    if h > 23 || m > 59 {
        bail!("time out of range: '{raw}' (00:00-23:59)");
    }
    Ok(format!("{h:02}:{m:02}"))
}

// ---------------------------------------------------------------------------
// Managed backups
// ---------------------------------------------------------------------------

/// Set up the Ryra-managed backend: confirm the account is logged in (offering
/// to log in right here if not) and has an active plan (opening the subscribe
/// page if not). Stores no credentials, they are vended per backup run, and the
/// restic password stays client-side.
async fn collect_managed(interactive: bool) -> Result<BackupBackend> {
    use ryra_core::system::account::{self, BackupState};
    // Make sure we have a token. If not, offer to log in inline rather than
    // dead-ending the user into a separate command.
    let src = match account::effective_token()? {
        Some(src) => src,
        None if interactive => {
            let want = Confirm::new()
                .with_prompt("Managed backups need a ryra account. Log in now?")
                .default(true)
                .interact()?;
            if !want {
                bail!(
                    "managed backups need a ryra account. Run `ryra account login` \
                     when you're ready, then re-run `ryra backup config`."
                );
            }
            super::account::device_login().await?;
            account::effective_token()?
                .ok_or_else(|| anyhow!("login completed but no credential was stored"))?
        }
        None => {
            let base = account::api_base_url();
            bail!(
                "managed backups need a ryra account. Set RYRA_TOKEN or run \
                 `ryra account login` (sign in at {base}), then re-run `ryra backup config`."
            );
        }
    };
    match account::backup_status(src.token())? {
        BackupState::Active { .. } => {
            println!("  Using your active ryra-managed backup plan.");
            Ok(BackupBackend::Managed)
        }
        BackupState::None | BackupState::Inactive(_) => {
            // Subscribing is a billing action the human completes in the
            // dashboard with their full session; a box's backups-only key
            // can't (and shouldn't) mint a checkout. Send them to the dashboard
            // backups page, where Stripe takes over. The printed URL is the
            // fallback for headless boxes (open is best-effort).
            let url = format!("{}/backups", account::api_base_url());
            if interactive {
                super::account::open_browser(&url);
            }
            bail!(
                "no active managed backup plan. Subscribe in your dashboard, then \
                 re-run `ryra backup config`:\n  {url}"
            );
        }
    }
}

/// Load config, resolving a `Managed` backup backend into concrete, short-lived
/// S3 credentials vended from the user's account. After this the rest of the
/// backup path only ever sees S3/Local, so the pure planner stays pure.
fn load_config_resolved(paths: &ConfigPaths) -> Result<Config> {
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if let Some(settings) = config.backup.as_mut()
        && matches!(settings.backend, BackupBackend::Managed)
    {
        settings.backend = ryra_core::system::account::resolve_managed_backend()?;
    }
    Ok(config)
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
        BackupAction::Run { services, mode } => run_backup(services, mode).await,
        BackupAction::Restore { service, at, force } => match service {
            Some(svc) => restore(svc, at, force).await,
            None => restore_all(at).await,
        },
        BackupAction::List { services } => list(services).await,
        BackupAction::Status => status().await,
        BackupAction::Schedule {
            cadence,
            keep,
            at,
            off,
        } => schedule(cadence, keep, at, off).await,
        BackupAction::Forget { services, dry_run } => forget(services, dry_run).await,
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
            // Bare `ryra backup config` after a prior failed init:
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
        ConfigureMode::Fresh => collect_new_settings(&args, interactive).await?,
    };

    // A managed backend stores `Managed` but needs concrete vended creds for
    // the restic-touching steps below (init + repo display). The STORED settings
    // keep `Managed`, so each run re-vends fresh short-lived creds.
    let resolved = if matches!(settings.backend, BackupBackend::Managed) {
        let mut r = settings.clone();
        r.backend = ryra_core::system::account::resolve_managed_backend()?;
        r
    } else {
        settings.clone()
    };

    // Init first, then save. Restic talks to the backend during init,
    // so failures here (auth, network, missing bucket) surface BEFORE
    // we touch preferences.toml. That means a failed configure leaves
    // no stale state behind to clean up.
    init_repo_if_needed(&resolved)?;

    if matches!(mode, ConfigureMode::Fresh) {
        config.backup = Some(settings.clone());
        // Persist the stable machine id alongside the backup config. collect_s3
        // mints + saves it when defaulting the S3 prefix; re-read it here (or
        // mint for local/managed) and carry it on THIS in-memory config so the
        // save below doesn't drop the `[machine]` block. Idempotent.
        config.machine = Some(ryra_core::config::schema::MachineConfig {
            id: ryra_core::config::machine_id(&paths)?,
        });
        paths.ensure_dirs()?;
        ryra_core::config::save_config(&paths.config_file, &config)?;
        println!(
            "\n  Backup repository saved to {}",
            paths.config_file.display()
        );
    }
    println!(
        "  Repository ready: {}",
        style(resolved.backend.restic_repo()).dim()
    );

    // Set up the schedule: daily and weekly are independent cadences, each
    // capped at `keep` snapshots (oldest dropped). Manual backups
    // (`ryra backup run`) are always available and never pruned. Always offered
    // -- the current values are the defaults, so on a retry you can leave them
    // untouched (enter through) or change them here.
    if interactive && !args.yes {
        println!(
            "\n  {} keep the last N of each. Manual backups (`ryra backup run`) \
             are always available and kept forever.",
            style("Scheduled backups:").bold()
        );
        let cur_daily = config.backup.as_ref().and_then(|b| b.daily.clone());
        let cur_weekly = config.backup.as_ref().and_then(|b| b.weekly.clone());
        let daily = prompt_cadence("daily", 7, cur_daily)?;
        let weekly = prompt_cadence("weekly", 4, cur_weekly)?;
        if let Some(b) = config.backup.as_mut() {
            b.daily = daily;
            b.weekly = weekly;
        }
        ryra_core::config::save_config(&paths.config_file, &config)?;
        apply_schedule(&config).await?;
    }

    Ok(())
}

/// Ask whether to take this cadence; if yes, collect keep-count + time of day.
/// `current` (the existing schedule, if any) seeds the defaults so a retry can
/// keep the present settings by pressing enter.
fn prompt_cadence(
    cadence: &str,
    fallback_keep: u32,
    current: Option<ScheduleMode>,
) -> Result<Option<ScheduleMode>> {
    let on = Confirm::new()
        .with_prompt(format!("  Take {cadence} backups?"))
        .default(current.is_some() || cadence == "daily")
        .interact()?;
    if !on {
        return Ok(None);
    }
    let keep: u32 = Input::new()
        .with_prompt(format!("    How many {cadence} backups to keep?"))
        .default(current.as_ref().map(|m| m.keep).unwrap_or(fallback_keep))
        .interact_text()?;
    let at: String = Input::new()
        .with_prompt("    Time of day (24h HH:MM)")
        .default(current.map(|m| m.at).unwrap_or_else(|| "03:00".to_string()))
        .interact_text()?;
    Ok(Some(ScheduleMode {
        keep,
        at: parse_schedule_time(Some(&at))?,
    }))
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

async fn collect_new_settings(args: &ConfigureArgs, interactive: bool) -> Result<BackupSettings> {
    let kind = match args.backend {
        Some(k) => k,
        None if interactive => prompt_backend()?,
        None => bail!("--backend is required in non-interactive mode (managed, s3, or local)"),
    };

    let backend = match kind {
        BackendKind::Managed => collect_managed(interactive).await?,
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

    // Schedule (daily/weekly) is set up after the backend, in `configure`.
    Ok(BackupSettings {
        password,
        backend,
        daily: None,
        weekly: None,
    })
}

fn prompt_backend() -> Result<BackendKind> {
    println!("\nWhich backup backend?");
    println!("  1. Ryra-managed   (encrypted off-site via your ryra account)");
    println!("  2. S3-compatible  (MinIO, AWS, Backblaze B2, R2, Wasabi)");
    println!("  3. Local path     (testing only, no off-machine protection)");
    let choice: u32 = Input::new()
        .with_prompt("Choose")
        .default(1)
        .interact_text()?;
    match choice {
        1 => Ok(BackendKind::Managed),
        2 => Ok(BackendKind::S3),
        3 => Ok(BackendKind::Local),
        n => bail!("expected 1, 2, or 3, got {n}"),
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
    // Default the prefix to this machine's stable id, so several machines can
    // share one bucket without colliding and the layout never keys off the
    // (mutable, non-unique) hostname. An explicit --prefix wins, e.g. to adopt
    // an existing machine's prefix when migrating to a new box.
    let prefix = match args.prefix.clone().filter(|p| !p.is_empty()) {
        Some(p) => Some(p),
        None => Some(ryra_core::config::machine_id(&ConfigPaths::resolve()?)?),
    };

    Ok(BackupBackend::S3 {
        endpoint,
        bucket,
        access_key_id,
        secret_access_key,
        session_token: None,
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

/// The keep-count configured for a scheduled cadence (`None` for manual, or a
/// cadence with no schedule set — those are never auto-pruned).
fn mode_keep(config: &Config, mode: BackupMode) -> Option<u32> {
    let b = config.backup.as_ref()?;
    match mode {
        BackupMode::Daily => b.daily.as_ref().map(|s| s.keep),
        BackupMode::Weekly => b.weekly.as_ref().map(|s| s.keep),
        BackupMode::Manual => None,
    }
}

/// Prune one service's snapshots in `mode` to `keep` (best-effort). Logged, not
/// fatal. Shared by the post-run auto-prune and `ryra backup forget`.
fn prune_one(config: &Config, svc: &str, mode: BackupMode, keep: u32, dry_run: bool) -> bool {
    match plan_mode_prune(svc, config, mode.as_str(), keep, dry_run) {
        Ok(Some(plan)) => match restic_forget(&plan) {
            Ok((kept, removed)) => {
                println!(
                    "  {} {svc} {}: {removed} removed, {kept} kept{}",
                    style("pruned:").dim(),
                    mode.as_str(),
                    if dry_run { " (dry run)" } else { "" }
                );
                true
            }
            Err(e) => {
                eprintln!("{} {svc} {}: {e:#}", style("prune failed:").yellow(), mode.as_str());
                false
            }
        },
        Ok(None) => true,
        Err(e) => {
            eprintln!("{} {svc} {}: {e:#}", style("prune failed:").yellow(), mode.as_str());
            false
        }
    }
}

pub(crate) async fn run_backup(services: Vec<String>, mode: BackupMode) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup config").cyan()
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
        match run_one(svc, &config, mode).await {
            Ok(()) => {
                record_status(svc, BackupOutcome::Success)?;
                // Cap this cadence to its keep. Manual is unlimited (mode_keep
                // returns None). Best-effort: a prune failure never fails the
                // backup that just succeeded.
                if let Some(keep) = mode_keep(&config, mode) {
                    prune_one(&config, svc, mode, keep, false);
                }
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

/// Prune the scheduled (daily + weekly) snapshots to their keep-counts. Manual
/// snapshots are never pruned. `--dry-run` previews removals.
async fn forget(services: Vec<String>, dry_run: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup config").cyan()
        );
    }
    let targets = if services.is_empty() {
        list_backup_enabled()?
    } else {
        services
    };
    if targets.is_empty() {
        println!("No services have backups enabled.");
        return Ok(());
    }
    // Nothing scheduled -> nothing to prune (manual is unlimited).
    if mode_keep(&config, BackupMode::Daily).is_none()
        && mode_keep(&config, BackupMode::Weekly).is_none()
    {
        println!(
            "{} no daily/weekly schedule set — manual backups are kept forever. \
             Set one with `{}`.",
            style("nothing to prune:").dim(),
            style("ryra backup schedule daily --keep N").cyan()
        );
        return Ok(());
    }
    let mut ok = true;
    for svc in &targets {
        for mode in [BackupMode::Daily, BackupMode::Weekly] {
            if let Some(keep) = mode_keep(&config, mode) {
                ok &= prune_one(&config, svc, mode, keep, dry_run);
            }
        }
    }
    if !ok {
        bail!("one or more prunes failed");
    }
    Ok(())
}

async fn run_one(service_name: &str, config: &Config, mode: BackupMode) -> Result<()> {
    let repo_dir = resolve_repo_dir_for_install(service_name).await?;
    let plan = plan_backup_run(service_name, config, &repo_dir, mode.as_str())?;
    println!(
        "\n{} {} ({}, {} path(s))",
        style("backing up:").cyan().bold(),
        plan.service_name,
        mode.as_str(),
        plan.paths.len()
    );

    ryra_core::backup::execute_backup_run(&plan)
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

async fn restore(service: String, at: Option<String>, force: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    let Some(settings) = config.backup.as_ref() else {
        bail!("no backup repository configured: run `ryra backup config` first");
    };

    // Allow a snapshot id in the service position: `ryra backup restore <id>`.
    // If the arg isn't an installed service, resolve it as a snapshot and
    // restore the service it belongs to, at that exact point.
    let (service, at) = if at.is_none() && load_metadata(&service)?.is_none() {
        match resolve_snapshot_service(settings, &service)? {
            Some((svc, id)) => {
                println!(
                    "{} snapshot {} belongs to {}",
                    style("restore:").cyan().bold(),
                    style(&id).cyan(),
                    style(&svc).cyan()
                );
                (svc, Some(id))
            }
            None => (service, at),
        }
    } else {
        (service, at)
    };

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

/// Infra/auth services come up before apps so the reverse proxy and
/// OIDC provider are ready when apps start. Lower sorts earlier.
fn restore_priority(service: &str) -> u8 {
    match service {
        "caddy" => 0,
        "authelia" | "minio" => 1,
        _ => 2,
    }
}

/// Distinct services with snapshots in the repo, read from the
/// `service:<name>` tags on `restic snapshots --json`.
fn list_repo_services(
    repo: &str,
    password: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("snapshots")
        .arg("--json")
        .arg("--repo")
        .arg(repo)
        .env("RESTIC_PASSWORD", password);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().context("spawning `restic snapshots`")?;
    if !output.status.success() {
        bail!(
            "restic snapshots failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[derive(Deserialize)]
    struct Snap {
        #[serde(default)]
        tags: Vec<String>,
    }
    let snaps: Vec<Snap> = serde_json::from_slice(&output.stdout)
        .with_context(|| "parsing `restic snapshots --json` output")?;
    let mut set = std::collections::BTreeSet::new();
    for s in snaps {
        for t in s.tags {
            if let Some(name) = t.strip_prefix("service:") {
                set.insert(name.to_string());
            }
        }
    }
    Ok(set.into_iter().collect())
}

/// Re-create quadlet symlinks for a restored service — every
/// `*.container`/`*.network`/`*.volume` in the service home gets linked
/// into `~/.config/containers/systemd/`, matching `ryra add`. Idempotent.
fn link_quadlets(home: &Path, quadlet_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(home)
        .with_context(|| format!("reading {}", home.display()))?
        .flatten()
    {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n.ends_with(".container") || n.ends_with(".network") || n.ends_with(".volume") {
            let link = quadlet_dir.join(&name);
            if std::fs::symlink_metadata(&link).is_ok() {
                std::fs::remove_file(&link).ok();
            }
            std::os::unix::fs::symlink(entry.path(), &link).with_context(|| {
                format!("symlink {} -> {}", link.display(), entry.path().display())
            })?;
        }
    }
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    let status = std::process::Command::new("systemctl")
        .args(&full)
        .status()
        .context("spawning systemctl")?;
    if !status.success() {
        bail!(
            "systemctl {} exited with {}",
            args.join(" "),
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

/// Full disaster recovery: restore every service folder in the repo,
/// re-link quadlets, bring the stack up, and import any DB dumps. The
/// only prerequisite is the user's kept `preferences.toml` (the repo
/// location + password) — everything else lives in the repo.
pub(crate) async fn restore_all(at: Option<String>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    let settings = config.backup.clone().ok_or_else(|| {
        anyhow!(
            "no backup repository configured. Put your saved `preferences.toml` at {} \
             (it carries the repo location + password), then re-run `ryra backup restore`.",
            paths.config_file.display()
        )
    })?;

    let repo = settings.backend.restic_repo();
    let env: std::collections::BTreeMap<String, String> = settings
        .backend
        .env()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let snapshot = at.unwrap_or_else(|| "latest".to_string());

    let mut services = list_repo_services(&repo, &settings.password, &env)?;
    if services.is_empty() {
        bail!("no service snapshots found in {repo}");
    }
    services.sort_by_key(|s| (restore_priority(s), s.clone()));

    println!(
        "{} {}",
        style("disaster recovery — restoring:").cyan().bold(),
        services.join(", ")
    );

    // 1. Lay every folder back (whole-folder snapshots: config + data).
    for svc in &services {
        let plan = BackupRestorePlan {
            service_name: svc.clone(),
            service_home: ryra_core::service_home(svc)?,
            repo: repo.clone(),
            password: settings.password.clone(),
            env: env.clone(),
            snapshot: snapshot.clone(),
            pre_restore_hook: None,
            post_restore_hook: None,
        };
        println!("\n{} {svc}", style("restoring folder:").cyan());
        restic_restore(&plan)?;
    }

    // 2. Re-link quadlets for all of them, then reload systemd once.
    let quadlet_dir = ryra_core::quadlet_dir()?;
    std::fs::create_dir_all(&quadlet_dir)
        .with_context(|| format!("creating {}", quadlet_dir.display()))?;
    for svc in &services {
        link_quadlets(&ryra_core::service_home(svc)?, &quadlet_dir)?;
    }
    run_systemctl(&["daemon-reload"])?;

    // 3. Start each service (infra first); run its restore-post hook
    //    against the now-running stack. Dump services import their SQL
    //    here; cold-stop services just re-sequence their startup.
    for svc in &services {
        let home = ryra_core::service_home(svc)?;
        println!("\n{} {svc}", style("starting:").cyan());
        run_systemctl(&["start", &format!("{svc}.service")])?;
        let hook = home.join("configs").join("scripts").join("restore-post.sh");
        if hook.exists() {
            run_hook("post_restore", svc, &hook, &home)?;
        }
    }

    println!(
        "\n{} {} service(s) restored and started.",
        style("done:").green().bold(),
        services.len()
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

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// Find the snapshot whose id matches `id` (short id, or a full-id prefix) and
/// return (service, short_id) so `ryra backup restore <id>` can restore the
/// right service at that exact point. None if nothing matches or the snapshot
/// has no `service:` tag.
fn resolve_snapshot_service(
    settings: &BackupSettings,
    id: &str,
) -> Result<Option<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Snap {
        id: String,
        short_id: String,
        #[serde(default)]
        tags: Vec<String>,
    }
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("snapshots")
        .arg("--json")
        .arg("--repo")
        .arg(settings.backend.restic_repo())
        .env("RESTIC_PASSWORD", &settings.password);
    for (k, v) in settings.backend.env() {
        cmd.env(k, v);
    }
    let out = cmd.output().context("spawning `restic snapshots`")?;
    if !out.status.success() {
        return Ok(None);
    }
    let snaps: Vec<Snap> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    for s in snaps {
        if s.short_id == id || s.id == id || s.id.starts_with(id) {
            if let Some(svc) = s.tags.iter().find_map(|t| t.strip_prefix("service:")) {
                return Ok(Some((svc.to_string(), s.short_id)));
            }
        }
    }
    Ok(None)
}

async fn list(services: Vec<String>) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
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

    // restic's own `snapshots` table is noisy (a "repository … opened" line)
    // and doesn't match `ryra list`. Parse `--json` (quiet) and render our own
    // clean, aligned table: newest restore point first, with the id you pass to
    // `ryra backup restore --at <id>`.
    #[derive(serde::Deserialize)]
    struct Snap {
        short_id: String,
        time: String,
        #[serde(default)]
        tags: Vec<String>,
    }
    for svc in &targets {
        let mut cmd = std::process::Command::new("restic");
        cmd.arg("snapshots")
            .arg("--json")
            .arg("--repo")
            .arg(settings.backend.restic_repo())
            .arg("--tag")
            .arg(format!("service:{svc}"))
            .env("RESTIC_PASSWORD", &settings.password);
        for (k, v) in settings.backend.env() {
            cmd.env(k, v);
        }
        let out = cmd.output().context("spawning `restic snapshots`")?;
        if !out.status.success() {
            eprintln!(
                "{} couldn't list backups for {svc}: {}",
                style("warning:").yellow(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            continue;
        }
        let mut snaps: Vec<Snap> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        snaps.reverse(); // restic lists oldest-first; show newest restore point first
        let count = snaps.len();
        println!(
            "\n{}  {}",
            style(svc).cyan().bold(),
            style(format!(
                "({count} restore point{})",
                if count == 1 { "" } else { "s" }
            ))
            .dim()
        );
        if snaps.is_empty() {
            println!("  {}", style("no backups yet").dim());
            continue;
        }
        println!(
            "  {:<19}  {:<7}  {}",
            style("WHEN").dim(),
            style("MODE").dim(),
            style("ID").dim()
        );
        for s in &snaps {
            let when = s.time.get(..19).unwrap_or(&s.time).replace('T', " ");
            let mode = s
                .tags
                .iter()
                .find_map(|t| t.strip_prefix("mode:"))
                .unwrap_or("manual");
            println!("  {:<19}  {:<7}  {}", when, mode, s.short_id);
        }
    }
    println!(
        "\n{} {}  ({})",
        style("restore a point:").dim(),
        style("ryra backup restore <id>").cyan(),
        style("or `ryra backup restore <service>` for the latest").dim()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status() -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    let Some(settings) = config.backup.as_ref() else {
        println!("Backup not configured. Run `ryra backup config` first.");
        return Ok(());
    };

    println!(
        "  Repository: {}",
        style(settings.backend.restic_repo()).dim()
    );
    println!("  Schedule:");
    for (label, mode) in [
        ("daily ", settings.daily.as_ref()),
        ("weekly", settings.weekly.as_ref()),
    ] {
        match mode {
            Some(m) => println!(
                "    {label}: {} (keep {})",
                style(&m.at).green(),
                style(m.keep).green()
            ),
            None => println!("    {label}: {}", style("off").dim()),
        }
    }
    println!("    manual: always available (kept forever)");

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

fn systemd_user_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow!("could not determine $XDG_CONFIG_HOME"))?;
    Ok(base.join("systemd").join("user"))
}

/// Timer + oneshot-service unit names for a cadence (`daily` / `weekly`).
fn unit_names(cadence: &str) -> (String, String) {
    (
        format!("ryra-backup-{cadence}.timer"),
        format!("ryra-backup-{cadence}.service"),
    )
}

fn cadence_mode(config: &Config, cadence: ScheduleCadence) -> Option<ScheduleMode> {
    let b = config.backup.as_ref()?;
    match cadence {
        ScheduleCadence::Daily => b.daily.clone(),
        ScheduleCadence::Weekly => b.weekly.clone(),
    }
}

fn set_cadence(config: &mut Config, cadence: ScheduleCadence, mode: Option<ScheduleMode>) {
    if let Some(b) = config.backup.as_mut() {
        match cadence {
            ScheduleCadence::Daily => b.daily = mode,
            ScheduleCadence::Weekly => b.weekly = mode,
        }
    }
}

/// `ryra backup schedule <daily|weekly> [--keep N] [--at HH:MM] | --off`:
/// turn a cadence on (persist its keep/time + install the timer) or off.
pub(crate) async fn schedule(
    cadence: ScheduleCadence,
    keep: Option<u32>,
    at: Option<String>,
    off: bool,
) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        bail!("no backup repository configured: run `ryra backup config` first");
    }
    let name = cadence.as_str();

    if off {
        set_cadence(&mut config, cadence, None);
        ryra_core::config::save_config(&paths.config_file, &config)?;
        apply_schedule(&config).await?;
        println!("  {} {name} backups off.", style("ryra-backup").cyan());
        return Ok(());
    }

    // Start from the current schedule for this cadence (or a default), then
    // apply whichever knobs were passed.
    let mut mode = cadence_mode(&config, cadence).unwrap_or_default();
    if let Some(k) = keep {
        mode.keep = k;
    }
    if let Some(a) = at {
        mode.at = parse_schedule_time(Some(&a))?;
    }
    set_cadence(&mut config, cadence, Some(mode.clone()));
    ryra_core::config::save_config(&paths.config_file, &config)?;
    apply_schedule(&config).await?;
    println!(
        "  {} {name} backups at {}, keeping the last {}.",
        style("ryra-backup").cyan(),
        style(&mode.at).green(),
        style(mode.keep).green()
    );
    super::linger::warn_if_disabled().await?;
    Ok(())
}

/// Reconcile the systemd --user timers to match `config`: write + enable a
/// timer for each enabled cadence, remove the rest, one daemon-reload. Shared
/// by `config`, `schedule`, and the rpc.
pub(crate) async fn apply_schedule(config: &Config) -> Result<()> {
    let dir = systemd_user_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    // Point unit files at the exact binary the user invoked, not bare `ryra`
    // (its $PATH at boot differs from the login shell).
    let exe = std::env::current_exe()
        .context("locating the current ryra binary")?
        .canonicalize()
        .context("resolving ryra binary path")?;

    let modes = [
        ("daily", config.backup.as_ref().and_then(|b| b.daily.clone())),
        ("weekly", config.backup.as_ref().and_then(|b| b.weekly.clone())),
    ];
    for (cadence, mode) in &modes {
        let (timer, service) = unit_names(cadence);
        match mode {
            Some(m) => write_timer(&dir, &exe, cadence, &m.at, &timer, &service)?,
            None => remove_timer(&dir, &timer, &service),
        }
    }

    // Enabling the timers needs a running `systemd --user` session. Where there
    // isn't one (containers, CI, a sandbox, sometimes a fresh login), the unit
    // files are still written + the config is saved -- a real session picks them
    // up on next login. So treat systemctl failures as a warning, not fatal:
    // the schedule is recorded either way.
    // Suppress systemctl's own "Failed to ..." chatter -- we report a single
    // clean note ourselves when there's no session.
    let reload = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let systemd_ok = matches!(reload, Ok(s) if s.success());
    if systemd_ok {
        let mut any_enable_failed = false;
        for (cadence, mode) in &modes {
            if mode.is_some() {
                let (timer, _) = unit_names(cadence);
                let st = std::process::Command::new("systemctl")
                    .args(["--user", "enable", "--now", &timer])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                any_enable_failed |= !matches!(st, Ok(s) if s.success());
            }
        }
        if any_enable_failed {
            eprintln!(
                "{} schedule saved, but the timer isn't active yet (no usable \
                 `systemd --user` session). It'll start on a normal login.",
                style("note:").yellow()
            );
        }
    } else {
        eprintln!(
            "{} no `systemd --user` session here, so the timer isn't active yet. \
             The schedule is saved and the unit files are written; they'll run \
             once you have a user session (a normal login box does).",
            style("note:").yellow()
        );
    }
    Ok(())
}

fn write_timer(
    dir: &Path,
    exe: &Path,
    cadence: &str,
    at: &str,
    timer: &str,
    service: &str,
) -> Result<()> {
    std::fs::write(
        dir.join(service),
        format!(
            "[Unit]\n\
             Description=Ryra {cadence} backup: snapshot every backup-enabled service\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={exe} backup run --mode {cadence}\n\
             Restart=no\n",
            exe = exe.display(),
        ),
    )
    .with_context(|| format!("write {service}"))?;
    std::fs::write(
        dir.join(timer),
        format!(
            "[Unit]\n\
             Description=Ryra {cadence} backup timer\n\
             \n\
             [Timer]\n\
             OnCalendar={oncal}\n\
             # Catch up a missed run after a reboot/suspend.\n\
             Persistent=true\n\
             Unit={service}\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            oncal = on_calendar(cadence, at),
        ),
    )
    .with_context(|| format!("write {timer}"))?;
    Ok(())
}

fn remove_timer(dir: &Path, timer: &str, service: &str) {
    // Best-effort + quiet: disabling a never-installed timer is fine, and
    // systemctl's "Failed to disable" chatter isn't useful here.
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", timer])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(dir.join(timer));
    let _ = std::fs::remove_file(dir.join(service));
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
