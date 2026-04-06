use std::io::IsTerminal;

use anyhow::Result;
use dialoguer::Input;

use ryra_core::config::schema::*;

use super::apply;
use super::prompts;

pub async fn run(
    repo: Option<String>,
    email: Option<String>,
    domain: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();

    // If config exists, ask to overwrite
    let has_existing = ryra_core::config::ConfigPaths::resolve()
        .ok()
        .map(|p| p.config_file.exists())
        .unwrap_or(false);

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

    // 1. Domain
    let domain = match domain {
        Some(d) => Some(d),
        None if interactive => {
            let d: String = Input::new()
                .with_prompt("Base domain (leave empty to skip)")
                .allow_empty(true)
                .interact_text()?;
            if d.is_empty() { None } else { Some(d) }
        }
        None => None,
    };

    // 2. Let's Encrypt email
    let ssl = match email {
        Some(e) => Some(SslConfig::Letsencrypt { email: e }),
        None if interactive => prompts::prompt_ssl()?,
        None => None,
    };

    // 3. SMTP
    let smtp = if interactive {
        prompts::prompt_smtp()?
    } else {
        None
    };

    // 4. Repo
    let default_repo = match repo {
        Some(r) => Some(r),
        None if interactive => {
            let url: String = Input::new()
                .with_prompt("Default repo")
                .default(ryra_core::DEFAULT_REPO.to_string())
                .interact_text()?;
            Some(url)
        }
        None => Some(ryra_core::DEFAULT_REPO.to_string()),
    };

    let config = Config {
        domain,
        ssl,
        smtp,
        auth: None,
        default_repo,
        ..Config::default()
    };

    let result = ryra_core::init(config.clone()).await?;

    match dry_run {
        true => {
            println!("Config written to /etc/ryra/ryra.toml\n");
            super::print_dry_run(&result.steps);
        }
        false => {
            println!("Setting up...");
            apply::execute_all(&result.steps).await?;
            let config_path = ryra_core::config::ConfigPaths::resolve()
                .map(|p| p.config_file.display().to_string())
                .unwrap_or_else(|_| "/etc/ryra/ryra.toml".into());
            println!();
            println!("ryra initialized!");
            println!("  Config: {config_path}");
            println!("  Run `ryra search` to browse available services.");
        }
    }

    Ok(())
}
