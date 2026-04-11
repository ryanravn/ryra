use anyhow::Result;
use ryra_core::config::status::{ProviderStatus, RyraStatus, StatusInfo};

pub async fn run(service: Option<&str>) -> Result<()> {
    match service {
        Some(name) => run_service(name).await,
        None => run_global(),
    }
}

fn run_global() -> Result<()> {
    match ryra_core::status() {
        RyraStatus::NotInitialized => {
            println!("ryra is not configured yet. Run `ryra add <service>` to get started.");
        }
        RyraStatus::Error(msg) => {
            eprintln!("Error: {msg}");
        }
        RyraStatus::Initialized(info) => print_global(&info),
    }
    Ok(())
}

fn print_global(info: &StatusInfo) {
    println!("Config:     {}", info.config_path.display());
    println!();
    println!("SMTP:       {}", format_provider(&info.smtp));
    println!("Auth:       {}", format_provider(&info.auth));
    println!();

    if info.services.is_empty() {
        println!("Services:   none installed");
    } else {
        println!("Services:");
        for svc in &info.services {
            let ports: Vec<String> = svc
                .ports
                .iter()
                .map(|(name, port)| format!("{name}={port}"))
                .collect();
            match (svc.domain.as_deref(), ports.is_empty()) {
                (Some(domain), true) => {
                    println!("  {} (https://{})", svc.name, domain);
                }
                (Some(domain), false) => {
                    println!("  {} (https://{}) [{}]", svc.name, domain, ports.join(", "));
                }
                (None, true) => {
                    println!("  {}", svc.name);
                }
                (None, false) => {
                    println!("  {} [{}]", svc.name, ports.join(", "));
                }
            }
        }
    }

    println!();
}

async fn run_service(service: &str) -> Result<()> {
    use ryra_core::registry::resolve::ServiceRef;

    let service_ref = ServiceRef::parse(service)?;
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
    let service_name = service_ref.service_name();
    let detail = ryra_core::service_info(&repo_dir, service_name)?;

    println!("{}", detail.name);
    println!("  {}", detail.description);

    if let Some(url) = &detail.url {
        println!("  Docs: {url}");
    }

    if !detail.ports.is_empty() {
        println!();
        println!("Ports:");
        for (port, proto, name) in &detail.ports {
            println!("  {name}: {port}/{proto}");
        }
    }

    let configurable: Vec<_> = detail
        .env_vars
        .iter()
        .filter(|(_, prompt)| prompt.is_some())
        .collect();
    if !configurable.is_empty() {
        println!();
        println!("Configuration (prompted during add):");
        for (name, prompt) in &configurable {
            println!("  {name}: {}", prompt.as_deref().unwrap_or(""));
        }
    }

    // Check if installed and get the domain
    let installed_service = ryra_core::list_installed()?
        .into_iter()
        .find(|s| s.name == service_name);

    if let Some(ref svc) = installed_service {
        let home_dir = ryra_core::service_home(service_name)?;

        println!();
        println!("Installed");
        if let Some(ref domain) = svc.domain {
            println!("Domain:   https://{domain}");
        }
        println!("Config:   {}", home_dir.display());
        println!();
        println!("Useful commands:");
        println!("  cat {}", home_dir.join(".env").display());
        println!("  systemctl --user status {service_name}");
        println!("  journalctl --user-unit {service_name}.service -f");
        println!("  systemctl --user restart {service_name}");
    } else {
        println!();
        println!("Not installed. Run `ryra add {service_name}` to install.");
    }

    Ok(())
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}
