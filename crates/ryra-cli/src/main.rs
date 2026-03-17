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
    /// Initialize ryra on this host
    Init {
        /// Domain name (e.g., example.com)
        #[arg(long)]
        domain: Option<String>,
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
        /// Domain for this service (defaults to <service>.<host domain>)
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
    /// Show ryra configuration and installation status
    Status,
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
    /// Show details about a service
    Info {
        /// Service name
        service: String,
        /// Repo to look up from (git URL or local path)
        #[arg(long)]
        repo: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init {
            domain,
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
                domain, repo, email, cf_token, cf_zone_id, cf_zone_name, tunnel_token, dry_run,
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
        Command::Status => cli::status::run()?,
        Command::List => cli::list::run()?,
        Command::Search { ref query, ref repo } => {
            cli::search::run(query.as_deref(), repo.as_deref()).await?
        }
        Command::Info { ref service, ref repo } => {
            cli::info::run(service, repo.as_deref()).await?
        }
    }

    Ok(())
}
