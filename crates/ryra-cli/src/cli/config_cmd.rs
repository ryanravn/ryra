use anyhow::{Result, bail};
use dialoguer::Input;

use super::prompts;

pub async fn run(section: Option<&str>) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

    match section {
        None | Some("show") => {
            print_overview(&config);
            return Ok(());
        }
        Some("domain") => {
            let d: String = Input::new()
                .with_prompt("Base domain")
                .default(config.domain.clone().unwrap_or_default())
                .interact_text()?;
            config.domain = if d.is_empty() { None } else { Some(d) };
        }
        Some("ssl") => {
            config.ssl = prompts::prompt_ssl()?;
        }
        Some("smtp") => {
            config.smtp = prompts::prompt_smtp()?;
        }
        Some("auth") => match prompts::prompt_auth()? {
            prompts::AuthSetupChoice::External(auth) => config.auth = Some(auth),
            prompts::AuthSetupChoice::InstallAuthentik => {
                println!();
                println!(
                    "  Run `ryra add authentik` to install — auth will be configured automatically."
                );
                return Ok(());
            }
            prompts::AuthSetupChoice::Skip => return Ok(()),
        },
        Some("repo") => {
            let url: String = Input::new()
                .with_prompt("Default repo")
                .default(
                    config
                        .default_repo
                        .clone()
                        .unwrap_or_else(|| ryra_core::DEFAULT_REPO.to_string()),
                )
                .interact_text()?;
            config.default_repo = Some(url);
        }
        Some(other) => {
            bail!("Unknown section: {other}. Options: domain, ssl, smtp, auth, repo");
        }
    }

    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("Config saved to {}", paths.config_file.display());

    Ok(())
}

fn print_overview(config: &ryra_core::config::schema::Config) {
    println!("ryra configuration:\n");

    // Domain
    match &config.domain {
        Some(domain) => println!("  domain:     {domain}"),
        None => println!("  domain:     {}", status_none()),
    }

    // SSL
    match &config.ssl {
        Some(ryra_core::config::schema::SslConfig::Letsencrypt { email }) => {
            println!("  ssl:        {} (letsencrypt, {})", status_ok(), email);
        }
        Some(ryra_core::config::schema::SslConfig::Custom { cert_dir }) => {
            println!("  ssl:        {} (custom, {})", status_ok(), cert_dir);
        }
        None => println!("  ssl:        {}", status_none()),
    }

    // SMTP
    match &config.smtp {
        Some(smtp) => println!("  smtp:       {} ({})", status_ok(), smtp.host),
        None => println!("  smtp:       {}", status_none()),
    }

    // Auth
    match &config.auth {
        Some(auth) => {
            println!(
                "  auth:       {} ({}, {})",
                status_ok(),
                auth.provider_name(),
                auth.url()
            );
        }
        None => println!("  auth:       {}", status_none()),
    }

    // Repo
    match &config.default_repo {
        Some(repo) => println!("  repo:       {repo}"),
        None => println!("  repo:       (default)"),
    }

    if !config.services.is_empty() {
        println!("\n  {} installed service(s)", config.services.len());
    }

    println!("\nEdit a section: ryra config <domain|ssl|smtp|auth|repo>");
}

fn status_ok() -> &'static str {
    "configured"
}

fn status_none() -> &'static str {
    "not configured"
}
