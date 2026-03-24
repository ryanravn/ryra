use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::Input;

use ryra_core::config::schema::*;

use super::apply;
use super::prompts;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    repo: Option<String>,
    email: Option<String>,
    cf_token: Option<String>,
    cf_zone_id: Option<String>,
    cf_zone_name: Option<String>,
    tunnel_token: Option<String>,
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

    // 1. Cloudflare
    let cloudflare = match (cf_token, cf_zone_id, cf_zone_name) {
        (Some(token), Some(zone_id), Some(zone_name)) => Some(CloudflareCredentials {
            api_token: token,
            zone_id,
            zone_name,
            tunnel: None,
        }),
        (None, None, None) if interactive => prompts::prompt_cloudflare().await?,
        (None, None, None) => None,
        _ => bail!("--cf-token, --cf-zone-id, and --cf-zone-name must all be provided together"),
    };

    // 2. Tunnel (only if CF is configured)
    let cloudflare = match (&cloudflare, tunnel_token) {
        (Some(_), Some(_token)) => {
            bail!("Use interactive mode for tunnel setup (need to select tunnel)");
        }
        (Some(cf), None) if interactive => {
            let tunnel = prompts::prompt_tunnel(&cf.api_token, &cf.zone_id).await?;
            Some(CloudflareCredentials {
                api_token: cf.api_token.clone(),
                zone_id: cf.zone_id.clone(),
                zone_name: cf.zone_name.clone(),
                tunnel,
            })
        }
        _ => cloudflare,
    };

    // 3. Let's Encrypt email (skip if tunnel-only)
    let has_tunnel = cloudflare
        .as_ref()
        .and_then(|cf| cf.tunnel.as_ref())
        .is_some();
    let ssl = match email {
        Some(e) => Some(SslConfig::Letsencrypt { email: e }),
        None if interactive && !has_tunnel => prompts::prompt_ssl()?,
        None => None,
    };

    // 4. SMTP
    let smtp = if interactive {
        prompts::prompt_smtp()?
    } else {
        None
    };

    // 5. Repo
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
        cloudflare,
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
