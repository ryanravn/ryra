use anyhow::{Result, bail};

use super::prompts;

pub async fn run(section: Option<&str>) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

    match section {
        None | Some("show") => {
            print_overview(&config);
            return Ok(());
        }
        Some("smtp") => match prompts::prompt_smtp()? {
            prompts::SmtpSetupChoice::Custom(smtp) => config.smtp = Some(smtp),
            prompts::SmtpSetupChoice::Inbucket => {
                println!("  Run `ryra add inbucket` first, then re-run `ryra config smtp`.");
                return Ok(());
            }
            prompts::SmtpSetupChoice::Skip => return Ok(()),
        },
        Some("auth") => match prompts::prompt_auth()? {
            prompts::AuthSetupChoice::External(auth) => config.auth = Some(auth),
            prompts::AuthSetupChoice::InstallAuthelia => {
                println!();
                println!(
                    "  Run `ryra add authelia` to install — auth will be configured automatically."
                );
                return Ok(());
            }
            prompts::AuthSetupChoice::Skip => return Ok(()),
        },
        Some(other) => {
            bail!("Unknown section: {other}. Options: smtp, auth");
        }
    }

    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("Config saved to {}", paths.config_file.display());

    Ok(())
}

fn print_overview(config: &ryra_core::config::schema::Config) {
    println!("ryra configuration:\n");

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

    if !config.services.is_empty() {
        println!("\n  {} installed service(s)", config.services.len());
    }

    println!("\nEdit a section: ryra config <smtp|auth>");
}

fn status_ok() -> &'static str {
    "configured"
}

fn status_none() -> &'static str {
    "not configured"
}
