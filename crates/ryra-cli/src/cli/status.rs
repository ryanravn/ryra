use anyhow::Result;
use ryra_core::config::status::{ProviderStatus, RyraStatus, StatusInfo};

pub fn run() -> Result<()> {
    match ryra_core::status() {
        RyraStatus::NotInitialized => {
            println!("ryra is not initialized. Run `ryra init` to get started.");
        }
        RyraStatus::Initialized(info) => print_status(&info),
    }
    Ok(())
}

fn print_status(info: &StatusInfo) {
    println!("Host:       {}", info.domain);
    println!();
    println!("DNS:        {}", format_provider(&info.dns));
    println!("Tunnel:     {}", format_provider(&info.tunnel));
    println!("SSL:        {}", format_provider(&info.ssl));
    println!("SMTP:       {}", format_provider(&info.smtp));
    println!("Auth:       {}", format_provider(&info.auth));
    println!();
    println!(
        "Registries: {}",
        if info.registries.is_empty() {
            "none".into()
        } else {
            info.registries.join(", ")
        }
    );
    println!();

    if info.services.is_empty() {
        println!("Services:   none installed");
    } else {
        println!("Services:");
        for svc in &info.services {
            println!("  {:<20} {}", svc.name, svc.domain);
        }
    }

    println!();
    println!(
        "Ports:      {} allocated (next: {})",
        info.ports_allocated, info.next_port
    );
    println!("Secrets:    {} stored", info.secrets_count);
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}
