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
    // -- Linux-only commands (require systemd, podman, sudo) --
    /// Initialize ryra on this host (optional — `ryra add` works without it)
    Init {
        /// Default repo (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
        /// Email for Let's Encrypt SSL certificates
        #[arg(long)]
        email: Option<String>,
        /// Cloudflare API token (optional, enables auto DNS)
        #[arg(long)]
        cf_token: Option<String>,
        /// Cloudflare zone ID (required if --cf-token is set)
        #[arg(long)]
        cf_zone_id: Option<String>,
        /// Cloudflare zone name (required if --cf-token is set)
        #[arg(long)]
        cf_zone_name: Option<String>,
        /// Cloudflare Tunnel token (optional, enables tunnel mode)
        #[arg(long)]
        tunnel_token: Option<String>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// Add and start a service
    Add {
        /// Service name from repo
        service: String,
        /// Domain for this service (defaults to <service>.<zone>)
        #[arg(long)]
        domain: Option<String>,
        /// Repo to install from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// Remove a service
    Remove {
        /// Service name to remove
        service: String,
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
        /// Section to configure (cloudflare, tunnel, ssl, smtp, repo)
        section: Option<String>,
    },
    /// Change how a service is exposed (local, tunnel, proxy, dns-only, host-port)
    Expose {
        /// Service name
        service: String,
        /// New domain (optional, for proxied modes)
        #[arg(long)]
        domain: Option<String>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
        /// Show file contents as they are written
        #[arg(long, short)]
        verbose: bool,
    },
    /// Show global config, or details about a specific service
    Status {
        /// Service name (omit for global overview)
        service: Option<String>,
        /// Repo to look up service from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
    },
    /// List installed services
    List,
    /// Re-scaffold a service with the latest registry definition (destructive)
    Update {
        /// Service name to update
        service: String,
        /// Repo to update from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
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
    // -- Cross-platform commands (read-only / VM-based) --

    /// Show what changed in a service's registry definition since install
    Diff {
        /// Service name
        service: String,
        /// Repo to compare against (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Search available services in a repo
    Search {
        /// Filter by name or description
        query: Option<String>,
        /// Repo to search (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
    },
    /// Run tests for a service
    Test {
        /// Test name filters
        names: Vec<String>,
        /// Run against live services instead of a fresh VM
        #[arg(long)]
        live: bool,
        /// Service to test (live mode)
        #[arg(long)]
        service: Option<String>,
        /// Run a multi-service test suite from the registry
        #[arg(long)]
        suite: Option<String>,
        /// Run only a specific test by name (live mode)
        #[arg(long)]
        test: Option<String>,
        /// Repo to load test definitions from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
        /// Keep VM alive after tests (or boot without tests for interactive use)
        #[arg(long)]
        keep_alive: bool,
        /// Skip confirmation prompts
        #[arg(long, short)]
        yes: bool,
        /// Show real-time output from VM commands
        #[arg(long, short)]
        verbose: bool,
        /// List available tests
        #[arg(long)]
        list: bool,
        /// Max concurrent VMs (default: 1)
        #[arg(long)]
        parallel: Option<usize>,
    },
}

impl Command {
    /// Whether this command works on non-Linux platforms.
    /// New commands default to Linux-only — add them here explicitly if cross-platform.
    fn is_cross_platform(&self) -> bool {
        matches!(
            self,
            Command::Search { .. } | Command::Diff { .. } | Command::Test { .. }
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cfg!(not(target_os = "linux")) && !cli.command.is_cross_platform() {
        anyhow::bail!(
            "this command requires Linux (systemd + podman).\n\
             \n\
             ryra manages services on a Linux host — commands like init, add, and remove\n\
             need systemd and rootless podman, which aren't available on macOS.\n\
             \n\
             On macOS you can:\n  \
             • ryra search / ryra diff  — browse and inspect the registry\n  \
             • ryra test --vm           — spin up a Linux VM and test services\n\
             \n\
             To manage a remote Linux server, SSH in and run ryra there."
        );
    }

    match cli.command {
        Command::Init {
            repo,
            email,
            cf_token,
            cf_zone_id,
            cf_zone_name,
            tunnel_token,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::init::run(
                repo,
                email,
                cf_token,
                cf_zone_id,
                cf_zone_name,
                tunnel_token,
                dry_run,
            )
            .await?
        }
        Command::Add {
            ref service,
            ref domain,
            ref repo,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::add::run(service, domain.as_deref(), repo.as_deref(), dry_run).await?
        }
        Command::Remove {
            ref service,
            yes,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::remove::run(service, yes, dry_run).await?
        }
        Command::Reset {
            yes,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::reset::run(yes, dry_run).await?
        }
        Command::Config { ref section } => cli::config_cmd::run(section.as_deref()).await?,
        Command::Expose {
            ref service,
            ref domain,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::expose::run(service, domain.as_deref(), dry_run).await?
        }
        Command::Status {
            ref service,
            ref repo,
        } => cli::status::run(service.as_deref(), repo.as_deref()).await?,
        Command::Update {
            ref service,
            ref repo,
            yes,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::update::run(service, repo.as_deref(), yes, dry_run).await?
        }
        Command::Diff {
            ref service,
            ref repo,
        } => cli::diff::run(service, repo.as_deref()).await?,
        Command::List => cli::list::run()?,
        Command::Search {
            ref query,
            ref repo,
        } => cli::search::run(query.as_deref(), repo.as_deref()).await?,
        Command::Test {
            ref names,
            live,
            ref service,
            ref suite,
            ref test,
            ref repo,
            keep_alive,
            yes,
            verbose,
            list,
            parallel,
        } => {
            cli::test::run(cli::test::TestRunParams {
                service: service.as_deref(),
                suite: suite.as_deref(),
                test_filter: test.as_deref(),
                repo: repo.as_deref(),
                vm: !live,
                keep_alive,
                yes,
                verbose,
                list,
                parallel,
                names,
            })
            .await?
        }
    }

    Ok(())
}
