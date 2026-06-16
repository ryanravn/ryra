mod cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ryra",
    version,
    about = "Self-hosted service manager using rootless Podman quadlets"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search available services in a registry
    Search {
        /// Filter by name or description
        query: Option<String>,
        /// Search a specific custom registry
        #[arg(long)]
        registry: Option<String>,
    },
    /// Add and start a service
    Add {
        /// Service(s): a registry name ("forgejo" / "acme/forgejo"), a local
        /// project path ("." / "./path"), or omit to use ./service.toml here.
        #[arg(num_args = 0..)]
        services: Vec<String>,
        /// Public URL for this service (e.g., https://docs.example.com)
        #[arg(long)]
        url: Option<String>,
        /// Wire this service to OIDC SSO via Authelia. Auto-installs Authelia
        /// at https://authelia.internal:<port> if it isn't already configured.
        /// Only works for services that declare native OIDC support — see the
        /// SUPPORTS column in `ryra search`.
        #[arg(long)]
        auth: bool,
        /// Configure SMTP non-interactively. Currently the only choice is
        /// "inbucket", which auto-installs the inbucket SMTP test server and
        /// points this install (and any service added in the same batch)
        /// at it. Skipped if SMTP is already configured.
        #[arg(long, value_enum)]
        smtp: Option<cli::add::SmtpProvider>,
        /// Enable a named `[[env_group]]` bundle on the service (repeatable).
        /// Required members of enabled groups are read from the process env
        /// in non-interactive mode, or prompted for interactively.
        #[arg(long = "enable", value_name = "GROUP")]
        enable: Vec<String>,
        /// Select a `[[choice]]` option non-interactively (repeatable).
        /// Format: CHOICE=OPTION, e.g. `--choose billing=mock`. Choices left
        /// unset use their declared default. Single-service installs only.
        #[arg(long = "choose", value_name = "CHOICE=OPTION")]
        choose: Vec<String>,
        /// Expose this service on your tailnet at
        /// `https://<service>.<tailnet>.ts.net` via Tailscale Services.
        /// Defines the service through the Tailscale admin API and runs
        /// `tailscale serve --service=svc:<name>` from the host (sudo
        /// required) — no sidecar containers, no port pool. Requires the
        /// `tailscale` CLI installed and a logged-in tailnet (configure
        /// the API token with TAILSCALE_API_KEY or interactively).
        /// Mutually exclusive with --url.
        #[arg(long, conflicts_with = "url")]
        tailscale: bool,
        /// Include this install in encrypted backups managed by
        /// `ryra backup run`. Only valid for services whose manifest
        /// sets `backup = true` under [integrations]. The actual
        /// backup repository (S3 endpoint, password) is configured
        /// once with `ryra backup configure`.
        #[arg(long)]
        backup: bool,
        /// Use Let's Encrypt for Caddy-managed routes. Pass `--acme you@example.com`
        /// to register with that email for renewal notices, or `--acme` alone to
        /// register anonymously. Without this flag, Caddy uses its internal CA
        /// (self-signed). After first install, edit
        /// `~/.local/share/services/caddy/config/tls.caddy` directly to switch to
        /// wildcards, Cloudflare DNS-01, BYO certs, etc.
        #[arg(long, value_name = "EMAIL", num_args = 0..=1, default_missing_value = "")]
        acme: Option<String>,
        /// Bring your own TLS cert for Caddy-managed routes instead of
        /// Let's Encrypt or the internal CA. Pass the cert (fullchain) path
        /// together with `--tls-key`. The right choice behind a proxy that
        /// terminates public TLS (e.g. a Cloudflare Origin CA cert with
        /// "Full (Strict)"). Mutually exclusive with `--acme`.
        #[arg(
            long,
            value_name = "CERT_PATH",
            requires = "tls_key",
            conflicts_with = "acme"
        )]
        tls_cert: Option<String>,
        /// Private key path that pairs with `--tls-cert`.
        #[arg(long, value_name = "KEY_PATH", requires = "tls_cert")]
        tls_key: Option<String>,
        /// Skip confirmation prompts (including untrusted registry warnings)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a service
    Remove {
        /// Service name(s) to remove
        #[arg(
            required_unless_present_any = ["all", "orphans"],
            num_args = 1..,
            conflicts_with_all = ["all", "orphans"]
        )]
        services: Vec<String>,
        /// Remove every installed service (use `ryra reset` to also wipe ryra's
        /// config + CA)
        #[arg(long, short = 'a', conflicts_with = "orphans")]
        all: bool,
        /// Purge every orphan service's data (leftover from prior `ryra remove`).
        /// Never touches installed services.
        #[arg(long)]
        orphans: bool,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Destructive: also delete data subdirs and podman named volumes.
        /// Without this flag, data is preserved and `ryra list` will show
        /// the service as orphan afterwards. Also works on orphans to
        /// clean up their leftover data.
        #[arg(long)]
        purge: bool,
    },
    /// List installed services
    List {
        /// Also show orphan services (removed but data still on disk)
        #[arg(long, short = 'a')]
        all: bool,
        /// Long listing — include size + volume breakdown. Takes longer
        /// because each volume requires a podman subprocess to measure.
        #[arg(long, short = 'l')]
        long: bool,
        /// Emit machine-readable JSON (name, status, url per service) instead
        /// of the human table. For programmatic callers such as ryra-api.
        #[arg(long)]
        json: bool,
    },
    /// Start an installed service (and its sidecars).
    Start {
        /// Service name. Omit and pass --all to start every installed service.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        service: Option<String>,
        /// Start every installed service.
        #[arg(long, short = 'a')]
        all: bool,
        /// Show what would happen without making changes.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop an installed service (and its sidecars). Data is untouched —
    /// `ryra start` brings it back. Use `ryra remove` to uninstall.
    Stop {
        /// Service name. Omit and pass --all to stop every installed service.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        service: Option<String>,
        /// Stop every installed service.
        #[arg(long, short = 'a')]
        all: bool,
        /// Show what would happen without making changes.
        #[arg(long)]
        dry_run: bool,
    },
    /// Global overview: config path, SMTP / auth providers, service count.
    Status,
    /// Manage custom registries
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
    /// Run tests for a service. Runs the full lifecycle on this host by
    /// default; pass --vm to run in an isolated throwaway VM instead.
    Test {
        /// Test name filters. Pick tests with names, or pass --all to run
        /// everything; bare `ryra test` just prints this help.
        names: Vec<String>,
        /// Run every test in the registry.
        #[arg(long, short = 'a', conflicts_with = "names")]
        all: bool,
        /// Run the full add/assert/remove lifecycle in a fresh, throwaway
        /// QEMU VM instead of on this host. Slower, needs KVM, but isolated.
        #[arg(long)]
        vm: bool,
        /// Run assertions against an already-installed service on this host
        /// (no add/remove). Requires --service. Non-mutating.
        #[arg(long)]
        live: bool,
        /// Deprecated: the host is now the default target, so this is a
        /// no-op kept for backward compatibility (CI, scripts). Use --vm
        /// to opt into a VM instead.
        #[arg(long, hide = true)]
        no_vm: bool,
        /// Service to test (live mode)
        #[arg(long)]
        service: Option<String>,
        /// Run only a specific test by name (live mode)
        #[arg(long)]
        test: Option<String>,
        /// Repo to load test definitions from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
        /// Test a local project directory with test.toml (+ optional quadlet files)
        #[arg(long)]
        project: Option<std::path::PathBuf>,
        /// Skip setup steps (add/wait) and only re-run shell/playwright steps
        #[arg(long)]
        retest: bool,
        /// Keep VM alive after tests (or boot without tests for interactive use)
        #[arg(long)]
        keep_alive: bool,
        /// Skip confirmation prompts
        #[arg(long, short)]
        yes: bool,
        /// Show real-time output from VM commands
        #[arg(long, short)]
        verbose: bool,
        /// Max concurrent VMs (default: 1)
        #[arg(long)]
        parallel: Option<usize>,
        /// Subcommand (e.g. `list`). Omit to run tests.
        #[command(subcommand)]
        action: Option<TestAction>,
    },
    /// Tear down all services, containers, and config
    Reset {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Diagnose environment + install state and report issues with fixes
    Doctor,
    /// Scaffold a ryra service.toml + test.toml in the current project.
    ///
    /// Additive (like `git init`): detects the project type (Cargo.toml / Bun
    /// package.json) to infer the run/build commands, prompts for name +
    /// description + port (defaults pre-filled), and writes the two files. Never
    /// touches your source. Then `ryra add` runs it.
    Init {
        /// Service name (default: current directory name)
        #[arg(long)]
        name: Option<String>,
        /// HTTP port the service listens on (skips the prompt)
        #[arg(long)]
        port: Option<u16>,
        /// Accept all defaults without prompting
        #[arg(short, long)]
        yes: bool,
    },
    /// Preview what `ryra upgrade` would change. Read-only.
    Diff {
        /// Service name(s). Omit to diff every installed service.
        services: Vec<String>,
    },
    /// Re-render an installed service against the current registry.
    ///
    /// Backs up displaced files to ~/.local/state/ryra/backups/<UTC-ts>/<service>/
    /// (last 5 snapshots kept) and restarts the unit. Refuses to clobber
    /// hand-edited files unless --force.
    Upgrade {
        /// Service name(s). Omit to upgrade every installed service.
        services: Vec<String>,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Overwrite hand-edited files. Backups still go to
        /// ~/.local/state/ryra/backups/<timestamp>/<service>/ first.
        #[arg(long)]
        force: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore an installed service from an upgrade backup.
    Revert {
        /// Service name(s) to revert. Required unless --list is set.
        services: Vec<String>,
        /// Specific snapshot timestamp (e.g. 2026-05-05T13-33-50Z). Omit
        /// to use the most recent. Only valid for a single service.
        #[arg(long)]
        at: Option<String>,
        /// Show available backup snapshots and exit. Combine with service
        /// names to filter, or run alone to list every service's backups.
        #[arg(long)]
        list: bool,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
    },
    /// Encrypted backups to an S3-compatible store (MinIO, AWS, R2, B2,
    /// Wasabi) via restic.
    Backup {
        #[command(subcommand)]
        action: cli::backup::BackupAction,
    },
    /// Reconfigure an installed service in place, or (with no service)
    /// edit global preferences and propagate them to installed services.
    Configure {
        /// Service name to reconfigure. Omit to edit global config (SMTP
        /// relay, admin email) and push changes into installed services.
        service: Option<String>,
        /// Global mode: set the SMTP relay host.
        #[arg(long = "smtp-host")]
        smtp_host: Option<String>,
        /// Global mode: set the SMTP relay port.
        #[arg(long = "smtp-port")]
        smtp_port: Option<u16>,
        /// Global mode: set the SMTP username.
        #[arg(long = "smtp-username")]
        smtp_username: Option<String>,
        /// Global mode: set the SMTP password.
        #[arg(long = "smtp-password")]
        smtp_password: Option<String>,
        /// Global mode: set the SMTP From address.
        #[arg(long = "smtp-from")]
        smtp_from: Option<String>,
        /// Global mode: set SMTP transport security.
        #[arg(long = "smtp-security", value_enum)]
        smtp_security: Option<cli::configure_global::SmtpSecurityArg>,
        /// Global mode: set the default admin email.
        #[arg(long = "admin-email")]
        admin_email: Option<String>,
        /// Global mode: reconcile installed services against the current
        /// preferences.toml without editing it (e.g. after hand-editing the
        /// file). Shows the per-service env diff and the same service chooser.
        #[arg(long)]
        apply: bool,
        /// Set or change the public URL.
        #[arg(long, conflicts_with_all = ["no_url", "tailscale"])]
        url: Option<String>,
        /// Remove the public URL (drops the Caddy route, switches to loopback).
        #[arg(long = "no-url", conflicts_with_all = ["url", "tailscale"])]
        no_url: bool,
        /// Switch this service to Tailscale Service exposure.
        #[arg(long, conflicts_with_all = ["url", "no_url"])]
        tailscale: bool,
        /// Wire this service to the global SMTP relay.
        #[arg(long, conflicts_with = "no_smtp")]
        smtp: bool,
        /// Stop wiring this service to the global SMTP relay.
        #[arg(long = "no-smtp", conflicts_with = "smtp")]
        no_smtp: bool,
        /// Include this service in encrypted backups.
        #[arg(long, conflicts_with = "no_backup")]
        backup: bool,
        /// Stop including this service in encrypted backups.
        #[arg(long = "no-backup", conflicts_with = "backup")]
        no_backup: bool,
        /// Register an OIDC client with the auth provider and enable SSO.
        /// Requires a `--url` if the service wasn't already URL-exposed.
        #[arg(long, conflicts_with = "no_auth")]
        auth: bool,
        /// Unregister the OIDC client and disable SSO. Destructive.
        #[arg(long = "no-auth", conflicts_with = "auth")]
        no_auth: bool,
        /// Re-register this service's OIDC client with the auth provider,
        /// reusing the credentials already in its .env (no rotation). Repairs
        /// a provider/consumer desync, e.g. after restoring the auth provider
        /// from a snapshot that predated this service. See `ryra doctor`.
        #[arg(long = "reassert-auth", conflicts_with = "no_auth")]
        reassert_auth: bool,
        /// Enable a named env_group bundle (repeatable).
        #[arg(long = "enable", value_name = "GROUP")]
        enable: Vec<String>,
        /// Disable a named env_group bundle (repeatable). Destructive —
        /// drops the group's env vars from `.env`.
        #[arg(long = "disable", value_name = "GROUP")]
        disable: Vec<String>,
        /// Reselect a `[[choice]]` option (repeatable). Format: CHOICE=OPTION,
        /// e.g. `--choose billing=live`. Switching to an option with required
        /// members needs their values via `--set` or the process env.
        #[arg(long = "choose", value_name = "CHOICE=OPTION")]
        choose: Vec<String>,
        /// Override an individual env var (repeatable). Format: KEY=VALUE.
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Skip confirmation prompts. Destructive changes still require
        /// typed confirmation in an interactive session unless `--yes`
        /// is combined with destructive flags (then they're auto-confirmed).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum TestAction {
    /// Search available tests from the registry
    Search {
        /// Test name filters
        names: Vec<String>,
        /// Show full step details
        #[arg(long, short)]
        verbose: bool,
    },
    /// Show local test sandbox state: installed services and last run results
    List,
    /// Remove stored results for one or more tests (report, log, playwright,
    /// per-test sandbox). Does not touch services or the ledger.
    Remove {
        /// Test names to remove
        names: Vec<String>,
    },
    /// Tear down the test sandbox: purge leftover test services and delete
    /// the sandbox dir (service data, preferences, ledger, run results).
    /// `ryra reset` for the test footprint; real services are untouched.
    Reset {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RegistryAction {
    /// Add a custom registry
    Add {
        /// Registry name (used as namespace, e.g., "acme")
        name: String,
        /// Git URL of the registry repo
        url: String,
    },
    /// Remove a custom registry
    Remove {
        /// Registry name
        name: String,
    },
    /// Update (git pull) custom registries
    Update {
        /// Specific registry to update (default: all)
        name: Option<String>,
    },
    /// List custom registries
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Add {
            services,
            url,
            auth,
            smtp,
            enable,
            choose,
            tailscale,
            backup,
            acme,
            tls_cert,
            tls_key,
            yes,
            dry_run,
        } => {
            // Map clap's raw flags to typed values at the CLI boundary so
            // every interior call site pattern-matches exhaustive enums
            // instead of poking at `Option<String>`s with empty-string
            // sentinels:
            //   --acme absent       → None (planner falls back to Internal)
            //   `--acme`            → Some(Anonymous)
            //   `--acme me@foo.bar` → Some(WithEmail(...))
            // --url / --tailscale fold into one ExposureRequest (clap's
            // conflicts_with already rejects passing both).
            //   --tls-cert P --tls-key K → Some(Byo { cert, key }) (clap's
            //   `conflicts_with`/`requires` guarantee they come as a pair and
            //   never alongside --acme).
            let acme_mode: Option<ryra_core::caddy::AcmeMode> = match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => Some(ryra_core::caddy::AcmeMode::Byo { cert, key }),
                _ => acme.as_deref().map(|s| {
                    if s.is_empty() {
                        ryra_core::caddy::AcmeMode::Anonymous
                    } else {
                        ryra_core::caddy::AcmeMode::WithEmail(s.to_string())
                    }
                }),
            };
            let exposure = match url {
                Some(u) => cli::add::ExposureRequest::Url(u),
                None if tailscale => cli::add::ExposureRequest::Tailscale,
                None => cli::add::ExposureRequest::Auto,
            };
            cli::add::run(cli::add::AddRequest {
                services,
                exposure,
                auth,
                smtp,
                enable,
                choose,
                backup,
                acme: acme_mode,
                dry_run,
                yes,
            })
            .await?
        }
        Command::Remove {
            ref services,
            all,
            orphans,
            yes,
            dry_run,
            purge,
        } => cli::remove::run(services, all, orphans, yes, dry_run, purge).await?,
        Command::Reset { yes, dry_run } => cli::reset::run(yes, dry_run).await?,
        Command::Doctor => cli::doctor::run()?,
        Command::Init {
            ref name,
            port,
            yes,
        } => cli::init::run(name.as_deref(), port, yes)?,
        Command::Diff { ref services } => cli::diff::run(services).await?,
        Command::Upgrade {
            ref services,
            yes,
            force,
            dry_run,
        } => cli::upgrade::run(services, yes, force, dry_run).await?,
        Command::Revert {
            ref services,
            ref at,
            list,
            yes,
            dry_run,
        } => cli::revert::run(services, at.as_deref(), yes, dry_run, list).await?,
        Command::Backup { action } => cli::backup::run(action).await?,
        Command::Configure {
            ref service,
            ref smtp_host,
            smtp_port,
            ref smtp_username,
            ref smtp_password,
            ref smtp_from,
            ref smtp_security,
            ref admin_email,
            apply,
            ref url,
            no_url,
            tailscale,
            smtp,
            no_smtp,
            backup,
            no_backup,
            auth,
            no_auth,
            reassert_auth,
            ref enable,
            ref disable,
            ref choose,
            ref set,
            yes,
            dry_run,
        } => match service {
            Some(service) => {
                if apply {
                    anyhow::bail!(
                        "--apply reconciles all services against the global config; \
                         run it without a service name (`ryra configure --apply`)"
                    );
                }
                let flags = cli::configure::ConfigureFlags {
                    url: url.clone(),
                    no_url,
                    tailscale,
                    smtp,
                    no_smtp,
                    backup,
                    no_backup,
                    auth,
                    no_auth,
                    reassert_auth,
                    enable: enable.clone(),
                    disable: disable.clone(),
                    choose: choose.clone(),
                    set: set.clone(),
                    yes,
                    dry_run,
                };
                cli::configure::run(service, flags).await?
            }
            None => {
                // Per-service-only flags make no sense without a service.
                let stray = [
                    (url.is_some() || no_url, "--url/--no-url"),
                    (tailscale, "--tailscale"),
                    (smtp || no_smtp, "--smtp/--no-smtp"),
                    (backup || no_backup, "--backup/--no-backup"),
                    (auth || no_auth, "--auth/--no-auth"),
                    (reassert_auth, "--reassert-auth"),
                    (!enable.is_empty(), "--enable"),
                    (!disable.is_empty(), "--disable"),
                    (!choose.is_empty(), "--choose"),
                    (!set.is_empty(), "--set"),
                ]
                .iter()
                .find(|(present, _)| *present)
                .map(|(_, name)| *name);
                if let Some(name) = stray {
                    anyhow::bail!(
                        "{name} needs a service (e.g. `ryra configure forgejo {name}`); \
                         with no service, `ryra configure` edits global config"
                    );
                }
                let flags = cli::configure_global::GlobalFlags {
                    smtp_host: smtp_host.clone(),
                    smtp_port,
                    smtp_username: smtp_username.clone(),
                    smtp_password: smtp_password.clone(),
                    smtp_from: smtp_from.clone(),
                    smtp_security: *smtp_security,
                    admin_email: admin_email.clone(),
                    apply,
                    yes,
                    dry_run,
                };
                cli::configure_global::run(flags).await?
            }
        },
        Command::Start {
            ref service,
            all,
            dry_run,
        } => {
            cli::lifecycle::run(
                service.as_deref(),
                all,
                ryra_core::Lifecycle::Start,
                dry_run,
            )
            .await?
        }
        Command::Stop {
            ref service,
            all,
            dry_run,
        } => {
            cli::lifecycle::run(service.as_deref(), all, ryra_core::Lifecycle::Stop, dry_run)
                .await?
        }
        Command::Status => cli::status::run().await?,
        Command::List { all, long, json } => cli::list::run(all, long, json)?,
        Command::Search {
            ref query,
            ref registry,
        } => cli::search::run(query.as_deref(), registry.as_deref()).await?,
        Command::Test {
            ref names,
            all,
            vm,
            live,
            no_vm: _,
            retest,
            ref service,
            ref test,
            repo: _,
            ref project,
            keep_alive,
            yes,
            verbose,
            parallel,
            ref action,
        } => {
            // `ryra test list [-v] [names…]` is a subcommand; everything
            // else is the normal run path.
            let (effective_list, effective_verbose, effective_names): (bool, bool, &[String]) =
                match action {
                    Some(TestAction::Search {
                        names: search_names,
                        verbose: search_verbose,
                    }) => (true, *search_verbose || verbose, search_names.as_slice()),
                    Some(TestAction::List) => {
                        cli::test::show_sandbox_state();
                        return Ok(());
                    }
                    Some(TestAction::Remove {
                        names: remove_names,
                    }) => {
                        cli::test::remove_tests(remove_names);
                        return Ok(());
                    }
                    Some(TestAction::Reset { yes: reset_yes }) => {
                        cli::test::reset_sandbox(*reset_yes || yes).await?;
                        return Ok(());
                    }
                    None => {
                        // Running everything must be asked for explicitly
                        // (--all), because the default mode installs and
                        // purges every service the registry declares on
                        // THIS host. Without --all, names, or another
                        // explicit target (--project, --live, --keep-alive),
                        // print help instead of running the world.
                        let has_target =
                            all || !names.is_empty() || project.is_some() || live || keep_alive;
                        if !has_target {
                            use clap::CommandFactory;
                            let mut sub = Cli::command()
                                .find_subcommand("test")
                                .cloned()
                                .unwrap_or_else(|| {
                                    unreachable!("'test' is declared in the Command enum")
                                })
                                // Printed outside a parse, so clap hasn't
                                // propagated the parent bin name into the
                                // usage line; set it explicitly.
                                .bin_name("ryra test");
                            sub.print_help()?;
                            // Exit non-zero (clap's missing-required-arg code)
                            // so scripts that relied on bare `ryra test`
                            // running everything fail loudly instead of
                            // silently passing.
                            std::process::exit(2);
                        }
                        (false, verbose, names.as_slice())
                    }
                };
            cli::test::run(cli::test::TestRunParams {
                service: service.as_deref(),
                test_filter: test.as_deref(),
                project: project.as_ref(),
                vm,
                live,
                retest,
                keep_alive,
                yes,
                verbose: effective_verbose,
                list: effective_list,
                parallel,
                names: effective_names,
            })
            .await?
        }
        Command::Registry { action } => match action {
            RegistryAction::Add { ref name, ref url } => cli::registry_cmd::add(name, url).await?,
            RegistryAction::Remove { ref name } => cli::registry_cmd::remove(name)?,
            RegistryAction::Update { ref name } => {
                cli::registry_cmd::update(name.as_deref()).await?
            }
            RegistryAction::List => cli::registry_cmd::list()?,
        },
    }

    Ok(())
}
