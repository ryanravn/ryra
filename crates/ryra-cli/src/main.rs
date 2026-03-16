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
    Init,
    /// Add and start a service
    Add {
        /// Service name from registry
        service: String,
    },
    /// Remove a service
    Remove {
        /// Service name to remove
        service: String,
    },
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
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init => cli::init::run().await?,
        Command::Add { ref service } => cli::add::run(service).await?,
        Command::Remove { ref service } => cli::remove::run(service).await?,
        Command::List => cli::list::run()?,
        Command::Registry { action } => match action {
            RegistryAction::Add { ref url } => cli::registry::run_add(url).await?,
        },
    }

    Ok(())
}
