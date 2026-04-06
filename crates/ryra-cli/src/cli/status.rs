use anyhow::Result;
use ryra_core::config::status::{ProviderStatus, RyraStatus, StatusInfo};

pub async fn run(service: Option<&str>, repo: Option<&str>) -> Result<()> {
    match service {
        Some(name) => run_service(name, repo).await,
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
    println!("Host:       {}", info.domain);
    println!();
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
}

async fn run_service(service: &str, repo: Option<&str>) -> Result<()> {
    let (_repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;
    let detail = ryra_core::service_info(&repo_dir, service)?;

    println!("{}", detail.name);
    println!("  {}", detail.description);

    if let Some(url) = &detail.url {
        println!("  Docs: {url}");
    }

    if detail.has_sidecars {
        println!("  Deploy: multi-container (quadlet)");
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

    if let Some(exposure) = &detail.installed_exposure {
        let home_dir = ryra_core::service_home(service);
        let username = ryra_core::service_user(service);

        println!();
        match &detail.installed_domain {
            Some(domain) => println!("Installed: {domain} ({exposure})"),
            None => println!("Installed ({exposure})"),
        }
        println!("Config:   {}", home_dir.display());
        println!();
        println!("Useful commands:");
        println!("  sudo cat {}", home_dir.join(".env").display());
        println!("  sudo systemctl --machine={username}@ --user status {service}");
        println!("  sudo journalctl _SYSTEMD_USER_UNIT={service}.service -f");
        println!("  sudo systemctl --machine={username}@ --user restart {service}");
    } else {
        println!();
        println!("Not installed. Run `ryra add {service}` to install.");
    }

    Ok(())
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}
