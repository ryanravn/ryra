use anyhow::Result;
use ryra_core::config::status::{CloudflareStatus, ProviderStatus, RyraStatus, StatusInfo};

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
    println!("Config:     {}", info.config_path.display());
    println!("Host:       {}", info.domain);
    println!();
    println!(
        "Cloudflare: {}",
        match &info.cloudflare {
            CloudflareStatus::None => "not configured".into(),
            CloudflareStatus::Configured { zone_name, tunnel } => {
                let tunnel_str = if *tunnel { " + tunnel" } else { "" };
                format!("{zone_name}{tunnel_str}")
            }
        }
    );
    println!("SSL:        {}", format_provider(&info.ssl));
    println!("SMTP:       {}", format_provider(&info.smtp));
    println!("Auth:       {}", format_provider(&info.auth));
    println!();
    println!(
        "Repo:       {}",
        info.default_repo.as_deref().unwrap_or("not configured")
    );
    println!();

    if info.services.is_empty() {
        println!("Services:   none installed");
    } else {
        println!("Services:");
        for svc in &info.services {
            let location = match &svc.domain {
                Some(d) => d.clone(),
                None => format!("[no domain — {}]", svc.exposure),
            };
            println!("  {:<20} {:<30} ({})", svc.name, location, svc.exposure);
        }
    }

    println!();
    println!("Ports:      {} allocated", info.ports_allocated);
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}
