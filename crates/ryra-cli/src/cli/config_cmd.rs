use anyhow::{Result, bail};

use super::prompts;

pub async fn run(section: Option<&str>) -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;

    let section = match section {
        Some(s) => s,
        // No section → the old behaviour printed a summary that duplicated
        // `ryra status`. Point the user at the right command instead and
        // keep `config` focused on mutation.
        None => {
            println!("Edit a section:");
            println!("  ryra config smtp");
            println!("  ryra config auth");
            println!();
            println!("For an overview of current config + installed services, run `ryra status`.");
            return Ok(());
        }
    };

    match section {
        "smtp" => match prompts::prompt_smtp()? {
            prompts::SmtpSetupChoice::Custom(smtp) => config.smtp = Some(smtp),
            prompts::SmtpSetupChoice::Inbucket => {
                println!("  Run `ryra add inbucket` first, then re-run `ryra config smtp`.");
                return Ok(());
            }
            prompts::SmtpSetupChoice::Skip => return Ok(()),
        },
        "auth" => match prompts::prompt_auth()? {
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
        other => {
            bail!("Unknown section: {other}. Options: smtp, auth");
        }
    }

    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("Config saved to {}", paths.config_file.display());

    Ok(())
}
