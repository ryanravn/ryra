mod cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ryra", version, about = "Self-hosted service manager using rootless Podman quadlets")]
struct Cli {
    /// Show file contents as they are written
    #[arg(long, short, global = true)]
    verbose: bool,
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
        /// Registry URL (git repo or local path)
        #[arg(long)]
        registry: Option<String>,
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
    },
    /// Add and start a service
    Add {
        /// Service name from registry
        service: String,
        /// Domain for this service (defaults to <service>.<host domain>)
        #[arg(long)]
        domain: Option<String>,
        /// Show what would happen without making changes
        #[arg(long)]
        dry_run: bool,
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
    /// Show ryra configuration and installation status
    Status,
    /// List available and installed services
    List,
    /// Manage registries
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
}

#[derive(Subcommand)]
enum RegistryAction {
    /// Add a new registry
    Add {
        /// Git URL or local path
        url: String,
        /// Registry name
        #[arg(long)]
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    ryra_core::verbose::set(cli.verbose);

    match cli.command {
        Command::Init {
            domain,
            registry,
            email,
            cf_token,
            cf_zone_id,
            cf_zone_name,
            tunnel_token,
            dry_run,
        } => {
            cli::init::run(
                domain, registry, email, cf_token, cf_zone_id, cf_zone_name, tunnel_token, dry_run,
            )
            .await?
        }
        Command::Add {
            ref service,
            ref domain,
            dry_run,
        } => cli::add::run(service, domain.as_deref(), dry_run).await?,
        Command::Remove {
            ref service,
            yes,
            dry_run,
        } => cli::remove::run(service, yes, dry_run).await?,
        Command::Reset { yes, dry_run } => cli::reset::run(yes, dry_run).await?,
        Command::Status => cli::status::run()?,
        Command::List => cli::list::run()?,
        Command::Registry { action } => match action {
            RegistryAction::Add { ref url, ref name } => {
                cli::registry::run_add(url, name.as_deref()).await?
            }
        },
    }

    Ok(())
}
