//! `ryra status` — global overview: config path, SMTP/auth providers,
//! installed service count. Read-only.

use anyhow::Result;
use ryra_core::config::status::{ProviderStatus, RyraStatus, StatusInfo};

pub async fn run() -> Result<()> {
    match ryra_core::status() {
        RyraStatus::NotInitialized => {
            println!("ryra is not configured yet. Run `ryra add <service>` to get started.");
        }
        RyraStatus::Error(msg) => {
            eprintln!("{} {msg}", super::style::error_prefix("Error:"));
        }
        RyraStatus::Initialized(info) => print_overview(&info),
    }
    Ok(())
}

fn print_overview(info: &StatusInfo) {
    println!("Config:     {}", info.config_path.display());
    println!();
    println!("SMTP:       {}", format_provider(&info.smtp));
    println!("Auth:       {}", format_provider(&info.auth));
    println!();

    if info.services.is_empty() {
        println!("Services:   none installed — run `ryra add <service>` to install one");
    } else {
        println!(
            "Services:   {} installed — run `ryra list` to list them",
            info.services.len()
        );
    }
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}
