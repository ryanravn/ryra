mod cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ryra", version, about = "Self-hosted service manager using rootless Podman quadlets")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// Service name
        service: Option<String>,
        /// Run only a specific test by name
        test: Option<String>,
        /// Run a multi-service test suite from the registry
        #[arg(long)]
        suite: Option<String>,
        /// Repo to load test definitions from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
        /// Run in a fresh VM instead of against live services
        #[arg(long)]
        vm: bool,
        /// Skip confirmation prompts
        #[arg(long, short)]
        yes: bool,
        /// Show test command output
        #[arg(long, short)]
        verbose: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

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
            cli::init::run(repo, email, cf_token, cf_zone_id, cf_zone_name, tunnel_token, dry_run)
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
        Command::Config { ref section } => {
            cli::config_cmd::run(section.as_deref()).await?
        }
        Command::Expose {
            ref service,
            ref domain,
            dry_run,
            verbose,
        } => {
            ryra_core::verbose::set(verbose);
            cli::expose::run(service, domain.as_deref(), dry_run).await?
        }
        Command::Status { ref service, ref repo } => {
            cli::status::run(service.as_deref(), repo.as_deref()).await?
        }
        Command::List => cli::list::run()?,
        Command::Search { ref query, ref repo } => {
            cli::search::run(query.as_deref(), repo.as_deref()).await?
        }
        Command::Test {
            ref service,
            ref test,
            ref suite,
            ref repo,
            vm,
            yes,
            verbose,
        } => {
            cli::test::run(
                service.as_deref(),
                suite.as_deref(),
                test.as_deref(),
                repo.as_deref(),
                vm,
                yes,
                verbose,
            )
            .await?
        }
    }

    Ok(())
}
