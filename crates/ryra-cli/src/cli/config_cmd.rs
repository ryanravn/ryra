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
        Some("cloudflare") => {
            config.cloudflare = prompts::prompt_cloudflare().await?;
        }
        Some("tunnel") => {
            let cf = config.cloudflare.as_ref().ok_or_else(|| {
                anyhow::anyhow!("Configure cloudflare first: ryra config cloudflare")
            })?;
            let tunnel = prompts::prompt_tunnel(&cf.api_token, &cf.zone_id).await?;
            if let Some(ref mut cf) = config.cloudflare {
                cf.tunnel = tunnel;
            }
        }
        Some("ssl") => {
            config.ssl = prompts::prompt_ssl()?;
        }
        Some("smtp") => {
            config.smtp = prompts::prompt_smtp()?;
        }
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
            bail!("Unknown section: {other}. Options: cloudflare, tunnel, ssl, smtp, repo");
        }
    }

    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("Config saved to {}", paths.config_file.display());

    Ok(())
}

fn print_overview(config: &ryra_core::config::schema::Config) {
    println!("ryra configuration:\n");

    // Cloudflare
    match &config.cloudflare {
        Some(cf) => {
            println!("  cloudflare: {} (zone: {})", status_ok(), cf.zone_name);
            match &cf.tunnel {
                Some(t) => println!("  tunnel:     {} (id: {})", status_ok(), t.tunnel_id),
                None => println!("  tunnel:     {}", status_none()),
            }
        }
        None => {
            println!("  cloudflare: {}", status_none());
            println!("  tunnel:     {}", status_none());
        }
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
        Some(ryra_core::config::schema::AuthCredentials::Authentik { url, .. }) => {
            println!("  auth:       {} (authentik, {})", status_ok(), url);
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

    println!("\nEdit a section: ryra config <cloudflare|tunnel|ssl|smtp|repo>");
}

fn status_ok() -> &'static str {
    "configured"
}

fn status_none() -> &'static str {
    "not configured"
}
