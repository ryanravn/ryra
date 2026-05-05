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
        /// Service name(s) from registry (e.g., "forgejo" or "acme/forgejo")
        #[arg(required = true, num_args = 1..)]
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
        /// Use Let's Encrypt for Caddy-managed routes. Pass `--acme you@example.com`
        /// to register with that email for renewal notices, or `--acme` alone to
        /// register anonymously. Without this flag, Caddy uses its internal CA
        /// (self-signed). After first install, edit
        /// `~/.local/share/services/caddy/config/tls.caddy` directly to switch to
        /// wildcards, Cloudflare DNS-01, BYO certs, etc.
        #[arg(long, value_name = "EMAIL", num_args = 0..=1, default_missing_value = "")]
        acme: Option<String>,
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
    },
    /// View or edit global configuration (no args = overview)
    Config {
        /// A config section (`smtp`, `auth`) to edit, or a service name
        /// to show details for. Omit to print the global overview.
        section: Option<String>,
    },
    /// Manage custom registries
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
    /// Run tests for a service
    Test {
        /// Test name filters
        names: Vec<String>,
        /// Run against live services instead of a fresh VM
        #[arg(long)]
        live: bool,
        /// Run lifecycle tests directly on the host without a VM
        #[arg(long)]
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
    /// Show what would change if the current registry were re-rendered
    /// against an installed service. Read-only; flags hand-edited files
    /// so you know what `ryra upgrade` would refuse to overwrite.
    Diff {
        /// Service name(s). Omit to diff every installed service.
        services: Vec<String>,
    },
    /// Re-render an installed service against the current registry, backing
    /// up displaced files and restarting the unit. Refuses to clobber
    /// hand-edited files unless --force.
    ///
    /// Backups land in ~/.local/state/ryra/backups/<UTC-ts>/<service>/.
    /// The most recent 5 snapshots per service are kept; older ones are
    /// auto-pruned after each successful upgrade.
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
    /// Restore an installed service from its most recent (or `--at <timestamp>`)
    /// upgrade backup. Inverts a `ryra upgrade`: brings back overwritten files,
    /// removes upgrade-added files, restarts the unit.
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
}

#[derive(Subcommand)]
enum TestAction {
    /// List available tests (optionally filtered by name substrings)
    List {
        /// Test name filters
        names: Vec<String>,
        /// Show full step details (commands, URLs, poll config, …)
        #[arg(long, short)]
        verbose: bool,
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
            ref services,
            ref url,
            auth,
            smtp,
            ref enable,
            tailscale,
            ref acme,
            yes,
            dry_run,
        } => {
            // Map clap's raw flag to a typed AcmeMode at the CLI boundary so
            // every interior call site pattern-matches an exhaustive enum
            // instead of poking at a `Option<String>` with empty-string
            // sentinels:
            //   absent              → None (planner falls back to Internal)
            //   `--acme`            → Some(Anonymous)
            //   `--acme me@foo.bar` → Some(WithEmail(...))
            let acme_mode: Option<ryra_core::caddy::AcmeMode> = acme.as_deref().map(|s| {
                if s.is_empty() {
                    ryra_core::caddy::AcmeMode::Anonymous
                } else {
                    ryra_core::caddy::AcmeMode::WithEmail(s.to_string())
                }
            });
            cli::add::run(
                services,
                url.as_deref(),
                auth,
                smtp,
                enable,
                tailscale,
                acme_mode.as_ref(),
                dry_run,
                yes,
            )
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
        Command::Config { ref section } => cli::config_cmd::run(section.as_deref()).await?,
        Command::List { all, long } => cli::list::run(all, long)?,
        Command::Search {
            ref query,
            ref registry,
        } => cli::search::run(query.as_deref(), registry.as_deref()).await?,
        Command::Test {
            ref names,
            live,
            no_vm,
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
                    Some(TestAction::List {
                        names: list_names,
                        verbose: list_verbose,
                    }) => (true, *list_verbose || verbose, list_names.as_slice()),
                    None => (false, verbose, names.as_slice()),
                };
            cli::test::run(cli::test::TestRunParams {
                service: service.as_deref(),
                test_filter: test.as_deref(),
                project: project.as_ref(),
                vm: !live && !no_vm,
                no_vm,
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
