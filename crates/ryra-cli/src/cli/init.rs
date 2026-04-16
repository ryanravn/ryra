use anyhow::Result;

use ryra_core::config::schema::*;

use super::apply;
use super::prompts;

pub async fn run(dry_run: bool) -> Result<()> {
    let interactive = super::is_interactive();

    // If config exists, ask to overwrite
    let has_existing = ryra_core::config::ConfigPaths::resolve()?
        .config_file
        .exists();

    if has_existing && interactive && !dry_run {
        let overwrite = dialoguer::Confirm::new()
            .with_prompt("ryra is already configured. Reconfigure?")
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Cancelled. Use `ryra config <section>` to edit individual settings.");
            return Ok(());
        }
    }

    // 1. SMTP
    let smtp = if interactive {
        match prompts::prompt_smtp()? {
            prompts::SmtpSetupChoice::Custom(smtp) => Some(smtp),
            prompts::SmtpSetupChoice::Inbucket => {
                println!("  Inbucket can be installed later with `ryra add inbucket`.");
                None
            }
            prompts::SmtpSetupChoice::Skip => None,
        }
    } else {
        None
    };

    let config = Config {
        smtp,
        auth: None,
        ..Config::default()
    };

    let result = ryra_core::init(config.clone()).await?;

    match dry_run {
        true => {
            println!("Config written to ~/.config/ryra/ryra.toml\n");
            super::print_dry_run(&result.steps);
        }
        false => {
            println!("Setting up...");
            apply::execute_all(&result.steps).await?;
            let config_path = ryra_core::config::ConfigPaths::resolve()
                .map(|p| p.config_file.display().to_string())
                .unwrap_or_else(|_| "~/.config/ryra/ryra.toml".into());
            println!();
            println!("ryra initialized!");
            println!("  Config: {config_path}");
            println!("  Run `ryra search` to browse available services.");
        }
    }

    Ok(())
}
