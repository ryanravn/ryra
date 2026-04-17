mod cli;
pub mod verbose;

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
    // -- Linux-only commands (require systemd, podman) --
    /// Initialize ryra on this host (optional — `ryra add` works without it)
    Init {
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// Add and start a service
    Add {
        /// Service name(s) from registry (e.g., "jellyfin" or "acme/jellyfin")
        #[arg(required = true, num_args = 1..)]
        services: Vec<String>,
        /// Public URL for this service (e.g., https://docs.example.com)
        #[arg(long)]
        url: Option<String>,
        /// Enable auth (forward auth via Caddy, or native OIDC if supported)
        #[arg(long)]
        auth: bool,
        /// Configure SMTP non-interactively. Currently the only choice is
        /// "inbucket", which auto-installs the inbucket SMTP test server and
        /// points this install (and any service added in the same batch)
        /// at it. Skipped if SMTP is already configured.
        #[arg(long, value_enum)]
        smtp: Option<cli::add::SmtpProvider>,
        /// Skip confirmation prompts (including untrusted registry warnings)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// Remove a service
    Rm {
        /// Service name(s) to remove
        #[arg(required_unless_present = "all", num_args = 1.., conflicts_with = "all")]
        services: Vec<String>,
        /// Remove every installed service (use `ryra reset` to also wipe ryra's
        /// config + CA + snapshots)
        #[arg(long, short = 'a')]
        all: bool,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Destructive: also delete data subdirs and podman named volumes.
        /// Without this flag, data is preserved and `ryra ls` will show
        /// the service as orphan afterwards. Also works on orphans to
        /// clean up their leftover data.
        #[arg(long)]
        purge: bool,
    },
    /// Tear down all services, containers, and config
    Reset {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// View or edit global configuration
    Config {
        /// Section to configure (smtp, auth)
        section: Option<String>,
    },
    /// Show global config, or details about a specific service
    Status {
        /// Service name (omit for global overview)
        service: Option<String>,
    },
    /// List installed services
    Ls {
        /// Also show orphan services (removed but data still on disk)
        #[arg(long, short = 'a')]
        all: bool,
        /// Long listing — include size + volume breakdown. Takes longer
        /// because each volume requires a podman subprocess to measure.
        #[arg(long, short = 'l')]
        long: bool,
    },
    // -- Read-only / VM-based commands --
    /// Show what changed in a service's registry definition since install
    Diff {
        /// Service name
        service: String,
    },
    /// Search available services in a registry
    Search {
        /// Filter by name or description
        query: Option<String>,
        /// Search a specific custom registry
        #[arg(long)]
        registry: Option<String>,
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
    /// Manage custom registries
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
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
        Command::Init { dry_run, verbose } => {
            crate::verbose::set(verbose);
            cli::init::run(dry_run).await?
        }
        Command::Add {
            ref services,
            ref url,
            auth,
            smtp,
            yes,
            dry_run,
            verbose,
        } => {
            crate::verbose::set(verbose);
            cli::add::run(services, url.as_deref(), auth, smtp, dry_run, yes).await?
        }
        Command::Rm {
            ref services,
            all,
            yes,
            dry_run,
            purge,
        } => cli::rm::run(services, all, yes, dry_run, purge).await?,
        Command::Reset {
            yes,
            dry_run,
            verbose,
        } => {
            crate::verbose::set(verbose);
            cli::reset::run(yes, dry_run).await?
        }
        Command::Config { ref section } => cli::config_cmd::run(section.as_deref()).await?,
        Command::Status { ref service } => cli::status::run(service.as_deref()).await?,
        Command::Diff { ref service } => cli::diff::run(service).await?,
        Command::Ls { all, long } => cli::ls::run(all, long)?,
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
