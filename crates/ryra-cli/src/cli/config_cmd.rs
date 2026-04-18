use anyhow::{Result, bail};
use ryra_core::config::status::{ProviderStatus, RyraStatus, StatusInfo};

use super::prompts;

/// `ryra config` — one command for everything config-related.
///
/// - `ryra config`             → overview (config path, SMTP, auth, service count)
/// - `ryra config smtp|auth`   → interactive edit of that section
/// - `ryra config <service>`   → detail for that service (installed-or-not, URL, commands)
pub async fn run(section: Option<&str>) -> Result<()> {
    match section {
        None => return run_overview(),
        Some("smtp") => return edit_smtp().await,
        Some("auth") => return edit_auth().await,
        // Anything else is treated as a service name. Services are never
        // named `smtp` / `auth` in the registry (those are reserved config
        // sections), so there's no ambiguity.
        Some(name) => return show_service(name).await,
    }
}

// -- overview ---------------------------------------------------------------

fn run_overview() -> Result<()> {
    match ryra_core::status() {
        RyraStatus::NotInitialized => {
            println!("ryra is not configured yet. Run `ryra add <service>` to get started.");
        }
        RyraStatus::Error(msg) => {
            eprintln!("Error: {msg}");
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

    // Per-service detail (URL, ports, data paths) lives in `ryra list` —
    // keep a single canonical listing instead of duplicating it here.
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

// -- section editors --------------------------------------------------------

async fn edit_smtp() -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    match prompts::prompt_smtp()? {
        prompts::SmtpSetupChoice::Custom(smtp) => config.smtp = Some(smtp),
        prompts::SmtpSetupChoice::Inbucket => {
            println!("  Run `ryra add inbucket` first, then re-run `ryra config smtp`.");
            return Ok(());
        }
        prompts::SmtpSetupChoice::Skip => return Ok(()),
    }
    save(&paths, &config)
}

async fn edit_auth() -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    match prompts::prompt_auth()? {
        prompts::AuthSetupChoice::External(auth) => config.auth = Some(auth),
        prompts::AuthSetupChoice::InstallAuthelia => {
            println!();
            println!(
                "  Run `ryra add authelia` to install — auth will be configured automatically."
            );
            return Ok(());
        }
        prompts::AuthSetupChoice::Skip => return Ok(()),
    }
    save(&paths, &config)
}

fn save(paths: &ryra_core::config::ConfigPaths, config: &ryra_core::config::schema::Config) -> Result<()> {
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, config)?;
    println!("Config saved to {}", paths.config_file.display());
    Ok(())
}

// -- per-service detail -----------------------------------------------------

async fn show_service(service: &str) -> Result<()> {
    use ryra_core::registry::resolve::ServiceRef;

    let service_ref = ServiceRef::parse(service)
        .map_err(|_| anyhow::anyhow!("unknown section or service: {service}. Options: smtp, auth, or any installed service name"))?;
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
    let service_name = service_ref.service_name();

    // Catch the "typo that happens to parse as a service-ref" case early
    // with a more helpful error than the generic "not found in registry".
    let detail = match ryra_core::service_info(&repo_dir, service_name) {
        Ok(d) => d,
        Err(_) => bail!(
            "unknown section or service: {service_name}. Options: smtp, auth, or any installed service name"
        ),
    };

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

    let installed_service = ryra_core::list_installed()?
        .into_iter()
        .find(|s| s.name == service_name);

    if let Some(ref svc) = installed_service {
        let home_dir = ryra_core::service_home(service_name)?;
        println!();
        println!("Installed");
        if let Some(ref url) = svc.url {
            println!("URL:      {url}");
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
