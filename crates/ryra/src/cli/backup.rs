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
use dialoguer::{Confirm, Input, Password, Select};
use serde::{Deserialize, Serialize};

use ryra_core::REGISTRY_DEFAULT;
use ryra_core::backup::{
    BackupRestorePlan, list_backup_enabled, plan_backup_restore, plan_backup_run, plan_mode_prune,
    restic_forget,
};
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{BackupBackend, BackupSettings, Config, ScheduleMode};
use ryra_core::metadata::load_metadata;
use ryra_core::registry::resolve::ServiceRef;

#[derive(Subcommand, Debug)]
pub enum BackupAction {
    /// Connect backups to an encrypted repository: pick a backend and set the
    /// encryption password. Run again to change the backend or rotate the
    /// password. Set the schedule with `ryra backup config`; tear it all down
    /// with `ryra backup disconnect`.
    #[command(name = "connect")]
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
    /// Configure how this box uses the connected repository: which machine it
    /// backs up as (its sub-folder in the bucket; point it at another machine's
    /// to recover that machine's backups) and the daily/weekly schedule.
    /// Requires a connected repo (`ryra backup connect`).
    Config,
    /// Start backing up one or more installed services (the backup twin of
    /// `ryra add`): add them to the daily/weekly schedule, then offer to take a
    /// first snapshot. Adding does NOT snapshot on its own.
    Add {
        /// Service name(s) to start backing up.
        services: Vec<String>,
        /// Take a snapshot immediately, skipping the prompt.
        #[arg(long)]
        now: bool,
    },
    /// Stop backing up one or more services (the twin of `ryra remove`). Drops
    /// them from the schedule; existing snapshots stay in the bucket.
    Remove {
        /// Service name(s) to stop backing up.
        services: Vec<String>,
    },
    /// Take a backup snapshot now. Manual snapshots are kept forever (never
    /// auto-pruned). Name one or more services, or omit names to snapshot every
    /// service in backups. A named service that isn't in the schedule still gets
    /// a one-off snapshot.
    Manual {
        /// Service name(s) to snapshot. Omit to snapshot every service in
        /// backups. A named service need not be in the schedule (one-off).
        services: Vec<String>,
    },
    /// Internal: snapshot every service in backups at the given cadence (capped
    /// by the schedule's keep count). Invoked by the systemd daily/weekly timers,
    /// not meant to be run by hand -- use `ryra backup manual`.
    #[command(hide = true)]
    Scheduled {
        /// `daily` or `weekly`.
        cadence: ScheduleCadence,
    },
    /// Restore a backup. Pass a snapshot id from `ryra backup list` (or a
    /// service name to restore its most recent snapshot). Confirms first since
    /// it overwrites the service's current data.
    Restore {
        /// Snapshot id (from `ryra backup list`), or a service name for its
        /// latest snapshot.
        target: String,
        /// Restore even if the snapshot was taken against a different version
        /// of the service manifest. May fail to start: expect to migrate by
        /// hand. Also skips the confirmation.
        #[arg(long)]
        force: bool,
        /// Also restore the global `preferences.toml` (SMTP, auth, backup
        /// config) bundled in the snapshot, OVERWRITING your current global
        /// config. For disaster recovery on a fresh box, not routine restores.
        #[arg(long)]
        config: bool,
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
    /// Permanently delete one backup by its id (the ID column in
    /// `ryra backup list`). Confirms first unless `-y`. Scheduled (daily/weekly)
    /// backups are auto-pruned to their keep-counts after each run, so this is
    /// for removing a specific snapshot on demand -- including manual ones.
    Delete {
        /// Snapshot id from `ryra backup list`.
        id: String,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Disconnect backups: stop backing up + remove the schedule. Existing
    /// snapshots stay in the bucket; reconnecting to the same backend + password
    /// picks them back up. Confirms unless `-y`.
    Disconnect {
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
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
    let m: u32 = m
        .parse()
        .map_err(|_| anyhow!("invalid minute in '{raw}'"))?;
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
                     when you're ready, then re-run `ryra backup connect`."
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
                 `ryra account login` (sign in at {base}), then re-run `ryra backup connect`."
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
                 re-run `ryra backup connect`:\n  {url}"
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
                path,
                password,
                yes,
            })
            .await
        }
        BackupAction::Config => configure_backups().await,
        BackupAction::Add { services, now } => backup_add(services, now).await,
        BackupAction::Remove { services } => backup_remove(services).await,
        BackupAction::Manual { services } => run_backup(services, BackupMode::Manual).await,
        BackupAction::Scheduled { cadence } => {
            let mode = match cadence {
                ScheduleCadence::Daily => BackupMode::Daily,
                ScheduleCadence::Weekly => BackupMode::Weekly,
            };
            run_backup(Vec::new(), mode).await
        }
        BackupAction::Restore {
            target,
            force,
            config,
        } => restore(target, force, config).await,
        BackupAction::List { services } => list(services).await,
        BackupAction::Status => status().await,
        BackupAction::Delete { id, yes } => delete(id, yes).await,
        BackupAction::Disconnect { yes } => disconnect(yes).await,
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
            // Bare `ryra backup connect` after a prior failed init:
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

    // Connecting only establishes the repository. The schedule is separate
    // (`ryra backup config`), and services are added separately too. Point the
    // way on a fresh connect so the next steps are obvious.
    if matches!(mode, ConfigureMode::Fresh) {
        println!(
            "\n  Next: pick a schedule with `{}`, then add services with `{}`.",
            style("ryra backup config").cyan(),
            style("ryra backup add <service>").cyan()
        );
    }

    Ok(())
}

/// `ryra backup config`: configure how THIS box uses the connected repository --
/// which machine it backs up as (its sub-folder in the bucket) and the
/// daily/weekly schedule. `connect` establishes the repository; this configures
/// how this box uses it. Interactive.
async fn configure_backups() -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository connected: run `{}` first",
            style("ryra backup connect").cyan()
        );
    }
    if !super::is_interactive() {
        bail!("`ryra backup config` is interactive; run it in a terminal");
    }

    // 1. Machine: which sub-folder of the bucket this box reads/writes. Default
    //    is its own stable id; point it at another machine's id to recover or
    //    adopt that machine's backups. Managed backends assign this server-side.
    let mut prefix_changed = false;
    match &mut config.backup.as_mut().unwrap().backend {
        BackupBackend::S3 { prefix, .. } => {
            let own = ryra_core::config::machine_id(&paths)?;
            let current = prefix.clone().unwrap_or_else(|| own.clone());
            println!(
                "  {} this box backs up into one machine's sub-folder of the bucket. \
                 Use its own id, or another machine's to work with that machine's backups.",
                style("Machine:").bold()
            );
            let chosen: String = Input::new()
                .with_prompt("  Back up as machine")
                .default(current.clone())
                .interact_text()?;
            let chosen = chosen.trim().to_string();
            if !chosen.is_empty() && chosen != current {
                *prefix = Some(chosen);
                prefix_changed = true;
            }
        }
        BackupBackend::Managed => {
            println!(
                "  {} your Ryra account assigns this machine's storage (not selectable here).",
                style("Machine:").bold()
            );
        }
        BackupBackend::Local { .. } => {}
    }
    if prefix_changed {
        // Make sure the selected machine's repo is reachable (init if it's new).
        init_repo_if_needed(config.backup.as_ref().unwrap())?;
        println!("  {} machine set.", style("ok:").green());
    }

    // 2. Schedule: daily/weekly cadences + keep counts.
    println!(
        "\n  {} keep the last N of each. Manual backups (`ryra backup manual`) are \
         always available and kept forever.",
        style("Scheduled backups:").bold()
    );
    let cur_daily = config.backup.as_ref().and_then(|b| b.daily.clone());
    let cur_weekly = config.backup.as_ref().and_then(|b| b.weekly.clone());
    let daily = prompt_cadence("daily", 2, cur_daily)?;
    let weekly = prompt_cadence("weekly", 4, cur_weekly)?;
    if let Some(b) = config.backup.as_mut() {
        b.daily = daily;
        b.weekly = weekly;
    }

    ryra_core::config::save_config(&paths.config_file, &config)?;
    apply_schedule(&config).await?;
    super::linger::warn_if_disabled().await?;
    println!("{} backup config updated.", style("done:").green().bold());
    Ok(())
}

/// Ask whether to take this cadence; if yes, collect keep-count + time of day.
/// `current` (the existing schedule, if any) is shown in the prompt and seeds
/// every default, so re-running `config` shows what's set and pressing enter
/// keeps it.
fn prompt_cadence(
    cadence: &str,
    fallback_keep: u32,
    current: Option<ScheduleMode>,
) -> Result<Option<ScheduleMode>> {
    // Surface the current setting right in the question so the user can see
    // what's configured before deciding. The keep/time defaults below echo it too.
    let state = match &current {
        Some(m) => format!("currently keep {} at {}", m.keep, m.at),
        None => "currently off".to_string(),
    };
    let on = Confirm::new()
        .with_prompt(format!("  Take {cadence} backups? ({state})"))
        // Default matches reality so the [Y/n] capitalisation isn't misleading.
        .default(current.is_some())
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
    let choice = Select::new()
        .with_prompt("A backup repository is already configured")
        .items(&[
            "Retry connection         (reuse saved settings)",
            "Reconfigure from scratch (replace saved settings)",
            "Cancel",
        ])
        .default(0)
        .interact()?;
    match choice {
        0 => Ok(ConfigureMode::Retry),
        1 => Ok(ConfigureMode::Fresh),
        _ => bail!("cancelled"),
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
    let choice = Select::new()
        .with_prompt("Which backup backend?")
        .items(&[
            "Ryra-managed   (encrypted off-site via your ryra account)",
            "S3-compatible  (MinIO, AWS, Backblaze B2, R2, Wasabi)",
            "Local path     (testing only, no off-machine protection)",
        ])
        .default(0)
        .interact()?;
    Ok(match choice {
        0 => BackendKind::Managed,
        1 => BackendKind::S3,
        _ => BackendKind::Local,
    })
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
    // Default the prefix (which machine's sub-folder in the bucket) to this
    // box's stable id, so several machines can share one bucket without
    // colliding and the layout never keys off the (mutable) hostname. Connect
    // never asks for it; selecting a different machine (e.g. to recover another
    // box's backups) is done afterward with `ryra backup config`.
    let prefix = Some(ryra_core::config::machine_id(&ConfigPaths::resolve()?)?);

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
/// fatal. Runs after each scheduled backup to cap that cadence.
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
                eprintln!(
                    "{} {svc} {}: {e:#}",
                    style("prune failed:").yellow(),
                    mode.as_str()
                );
                false
            }
        },
        Ok(None) => true,
        Err(e) => {
            eprintln!(
                "{} {svc} {}: {e:#}",
                style("prune failed:").yellow(),
                mode.as_str()
            );
            false
        }
    }
}

/// `ryra backup add <svc>...`: add installed services to backups (the backup
/// twin of `ryra add`), then offer a first snapshot. Adding only enrolls them in
/// the schedule -- taking the snapshot is a separate, prompted step.
async fn backup_add(services: Vec<String>, now: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup connect").cyan()
        );
    }
    if services.is_empty() {
        bail!("name at least one service to add to backups");
    }
    for svc in &services {
        if load_metadata(svc)?.is_none() {
            bail!(
                "'{svc}' isn't installed. Install it with `{}` first.",
                style(format!("ryra add {svc}")).cyan()
            );
        }
        if ryra_core::backup::set_backup_enabled(svc, true)? {
            println!("{} {svc}", style("added to backups:").green().bold());
        } else {
            println!("{} {svc}", style("already in backups:").dim());
        }
    }

    // A snapshot is never taken automatically: offer one (default yes, since
    // adding a service is usually "protect it now"), or `--now` to skip asking.
    let take = now
        || (super::is_interactive()
            && Confirm::new()
                .with_prompt("Take a snapshot now?")
                .default(true)
                .interact()?);
    if take {
        run_backup(services, BackupMode::Manual).await
    } else {
        println!(
            "Added. Snapshot when you're ready with `{}`.",
            style("ryra backup manual").cyan()
        );
        Ok(())
    }
}

/// `ryra backup remove <svc>...`: stop backing up services (the twin of
/// `ryra remove`). Drops them from the schedule; existing snapshots are kept.
async fn backup_remove(services: Vec<String>) -> Result<()> {
    if services.is_empty() {
        bail!("name at least one service to remove from backups");
    }
    for svc in &services {
        if ryra_core::backup::set_backup_enabled(svc, false)? {
            println!("{} {svc}", style("removed from backups:").green().bold());
        } else {
            println!("{} {svc}", style("not in backups:").dim());
        }
    }
    println!(
        "Existing snapshots stay in the bucket. Remove them with `{}`.",
        style("ryra backup delete <id>").cyan()
    );
    Ok(())
}

pub(crate) async fn run_backup(services: Vec<String>, mode: BackupMode) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    if config.backup.is_none() {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup connect").cyan()
        );
    }

    // Snapshot the named services or, with no names, every service in backups
    // (the scheduled-timer path). Naming a service takes a snapshot now whether
    // or not it's enrolled: a one-off backup of any backup-capable install is
    // fine. `run` never enrolls -- `ryra backup add` is the way into the
    // schedule.
    let enrolled: std::collections::HashSet<String> = list_backup_enabled()?.into_iter().collect();
    let targets = if !services.is_empty() {
        for svc in &services {
            if !enrolled.contains(svc.as_str()) {
                println!(
                    "  {} {svc} {}",
                    style("one-off:").dim(),
                    style("not in the schedule; `ryra backup add` to back it up daily/weekly")
                        .dim()
                );
            }
        }
        services
    } else {
        // A backup that captures nothing isn't a backup. Refuse loudly rather
        // than report a hollow success -- this error also reaches the dashboard
        // over rpc, so a scheduled run can't silently no-op.
        if enrolled.is_empty() {
            bail!(
                "no services are in backups yet: add one with `{}`, then run this again",
                style("ryra backup add <service>").cyan()
            );
        }
        let mut v: Vec<String> = enrolled.into_iter().collect();
        v.sort();
        v
    };

    println!(
        "{} {} service(s)",
        style("backing up").cyan().bold(),
        targets.len()
    );

    let mut any_failed = false;
    let mut succeeded = 0usize;
    for svc in &targets {
        match run_one(svc, &config, mode).await {
            Ok(()) => {
                succeeded += 1;
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
    println!(
        "\n{} {} service(s)",
        style("backed up").green().bold(),
        succeeded
    );
    Ok(())
}

/// Permanently delete one snapshot by id (`restic forget <id> --prune`). Asks
/// to confirm first unless `yes`. The id uniquely identifies the snapshot, so
/// no service/mode is needed.
async fn delete(id: String, yes: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    let Some(settings) = config.backup.as_ref() else {
        bail!(
            "no backup repository configured: run `{}` first",
            style("ryra backup connect").cyan()
        );
    };
    // Managed resolves to short-lived vended creds (as a run/restore does).
    let mut settings = settings.clone();
    if matches!(settings.backend, BackupBackend::Managed) {
        settings.backend = ryra_core::system::account::resolve_managed_backend()?;
    }

    if !yes
        && !Confirm::new()
            .with_prompt(format!(
                "Permanently delete backup {}? This can't be undone",
                style(&id).cyan()
            ))
            .default(false)
            .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    let mut cmd = std::process::Command::new("restic");
    cmd.arg("forget")
        .arg(&id)
        .arg("--prune")
        .arg("--repo")
        .arg(settings.backend.restic_repo())
        .env("RESTIC_PASSWORD", &settings.password);
    for (k, v) in settings.backend.env() {
        cmd.env(k, v);
    }
    let status = cmd.status().context("spawning `restic forget`")?;
    if !status.success() {
        bail!("restic couldn't delete {id}");
    }
    println!("{} deleted backup {id}.", style("done:").green().bold());
    Ok(())
}

/// Disconnect backups: clear `[backup]` + remove the schedule timers. Existing
/// snapshots in the bucket are left untouched.
async fn disconnect(yes: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    if config.backup.is_none() {
        println!("Backups aren't configured.");
        return Ok(());
    }
    if !yes {
        println!(
            "  {} reconnecting to these snapshots needs your CURRENT encryption \n  password (in preferences.toml). Save it first \u{2014} a different password \n  can't read them.",
            style("\u{26A0}").yellow()
        );
        if !Confirm::new()
            .with_prompt(
                "Disconnect backups? New backups stop; existing snapshots stay in the bucket",
            )
            .default(false)
            .interact()?
        {
            println!("Cancelled.");
            return Ok(());
        }
    }
    config.backup = None;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    apply_schedule(&config).await?; // no backup config -> removes the timers
    println!(
        "{} backups disconnected. Existing snapshots remain in the bucket; \
         run `{}` to reconnect.",
        style("done:").green().bold(),
        style("ryra backup connect").cyan()
    );
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
    if !plan.online {
        println!(
            "  {}",
            style("stops the service briefly for a consistent snapshot, then restarts it").dim()
        );
    }

    ryra_core::backup::execute_backup_run(&plan)
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

async fn restore(target: String, force: bool, include_config: bool) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let config = load_config_resolved(&paths)?;
    let Some(settings) = config.backup.as_ref() else {
        bail!("no backup repository configured: run `ryra backup connect` first");
    };

    // `target` is a snapshot id (the usual case) or a service name (restore its
    // latest). Installed service -> latest; otherwise resolve the snapshot id to
    // its service + that exact point; neither -> a clear error.
    let (service, snapshot) = if load_metadata(&target)?.is_some() {
        (target, "latest".to_string())
    } else if let Some((svc, id)) = resolve_snapshot_service(settings, &target)? {
        println!(
            "{} snapshot {} belongs to {}",
            style("restore:").cyan().bold(),
            style(&id).cyan(),
            style(&svc).cyan()
        );
        (svc, id)
    } else {
        bail!(
            "'{target}' is neither an installed service nor a known snapshot id. \
             Run `{}` to see snapshot ids.",
            style("ryra backup list").cyan()
        );
    };

    let repo_dir = resolve_repo_dir_for_install(&service).await?;
    let mut plan = plan_backup_restore(&service, &snapshot, &config, &repo_dir)?;
    plan.include_config = include_config;

    if !force {
        check_version_match(&plan, &repo_dir).await?;
        // Restoring replaces the service's live data, so confirm first. A cold
        // restore (the default) also stops the service while it's replaced.
        let mut warn = if plan.online {
            "This overwrites its current data.".to_string()
        } else {
            "This stops the service, replaces its data, and restarts it.".to_string()
        };
        if include_config {
            warn.push_str(
                " It ALSO overwrites your global preferences.toml (SMTP, auth, backup config).",
            );
        }
        if super::is_interactive()
            && !Confirm::new()
                .with_prompt(format!(
                    "Restore {} from snapshot {}? {warn}",
                    style(&plan.service_name).cyan(),
                    style(&plan.snapshot).cyan()
                ))
                .default(false)
                .interact()?
        {
            println!("Cancelled.");
            return Ok(());
        }
    }

    println!(
        "\n{} {} (snapshot {})",
        style("restoring:").cyan().bold(),
        plan.service_name,
        plan.snapshot
    );

    // ryra owns the stop/wipe/restore/restart for cold services; online ones
    // run only their own hooks. See `execute_backup_restore`.
    ryra_core::backup::execute_backup_restore(&plan)?;

    println!(
        "\n{} {} restored. Run `{}` if the service didn't restart cleanly.",
        style("done:").green().bold(),
        plan.service_name,
        style(format!("systemctl --user restart {}", plan.service_name)).cyan()
    );
    Ok(())
}

/// Distinct services with snapshots in the repo, read from the
/// `service:<name>` tags on `restic snapshots --json`. Used as the fallback for
/// `ryra backup list` on a box where nothing is enrolled yet (fresh-machine
/// recovery: "which services have snapshots here?" with only preferences.toml).
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
        if (s.short_id == id || s.id == id || s.id.starts_with(id))
            && let Some(svc) = s.tags.iter().find_map(|t| t.strip_prefix("service:"))
        {
            return Ok(Some((svc.to_string(), s.short_id)));
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

    let targets = if !services.is_empty() {
        services
    } else {
        let enabled = list_backup_enabled()?;
        if enabled.is_empty() {
            // Nothing enrolled (e.g. a fresh box with only preferences.toml in
            // hand): discover services straight from the repo so recovery can
            // see what's restorable, then `ryra backup restore <id>`.
            let env: std::collections::BTreeMap<String, String> = settings
                .backend
                .env()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
            list_repo_services(&settings.backend.restic_repo(), &settings.password, &env)?
        } else {
            enabled
        }
    };
    if targets.is_empty() {
        println!("No services with backups enabled, and none found in the repository.");
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
        println!("Backup not configured. Run `ryra backup connect` first.");
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

/// Reconcile the systemd --user timers to match `config`: write + enable a
/// timer for each enabled cadence, remove the rest, one daemon-reload. Shared
/// by `config` (which owns the schedule) and the rpc.
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
        (
            "daily",
            config.backup.as_ref().and_then(|b| b.daily.clone()),
        ),
        (
            "weekly",
            config.backup.as_ref().and_then(|b| b.weekly.clone()),
        ),
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
                "{} schedule saved; the timer file is written but your systemd \
                 manager didn't pick it up here (a sandboxed config dir, or a \
                 pre-login shell). On a normal user session it activates \
                 automatically.",
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
             ExecStart={exe} backup scheduled {cadence}\n\
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
