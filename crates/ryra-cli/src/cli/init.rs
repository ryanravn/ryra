use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::Input;

use ryra_core::config::schema::*;

use super::apply;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    domain: Option<String>,
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
            .with_prompt("ryra is already initialized. Overwrite config?")
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Cancelled. Edit config directly or run `ryra reset` first.");
            return Ok(());
        }
    }

    // 1. Cloudflare
    let (cloudflare, derived_domain) = match (cf_token, cf_zone_id, cf_zone_name) {
        (Some(token), Some(zone_id), Some(zone_name)) => (
            Some(CloudflareCredentials {
                api_token: token,
                zone_id,
                zone_name: zone_name.clone(),
                tunnel: None,
            }),
            Some(zone_name),
        ),
        (None, None, None) if interactive => prompt_cloudflare_setup().await?,
        (None, None, None) => (None, None),
        _ => bail!("--cf-token, --cf-zone-id, and --cf-zone-name must all be provided together"),
    };

    // 2. Tunnel (only if CF is configured)
    let cloudflare = match (&cloudflare, tunnel_token) {
        (Some(_), Some(_token)) => {
            bail!("Use interactive mode for tunnel setup (need to select tunnel)");
        }
        (Some(cf), None) if interactive => {
            let tunnel = prompt_tunnel_setup(&cf.api_token, &cf.zone_id).await?;
            Some(CloudflareCredentials {
                api_token: cf.api_token.clone(),
                zone_id: cf.zone_id.clone(),
                zone_name: cf.zone_name.clone(),
                tunnel,
            })
        }
        _ => cloudflare,
    };

    // 3. Domain (optional — services default to <name>.<domain>)
    let domain = match domain {
        Some(d) => Some(d),
        None if interactive => {
            println!();
            println!("  Base domain for services (e.g. example.com).");
            println!("  Services will default to <name>.example.com.");
            println!("  Leave empty if only running local services.");
            println!();
            let default = derived_domain.unwrap_or_default();
            let input: String = Input::new()
                .with_prompt("Base domain")
                .default(default)
                .allow_empty(true)
                .interact_text()?;
            if input.is_empty() { None } else { Some(input) }
        }
        None => None,
    };

    // 4. Let's Encrypt email (skip if tunnel-only)
    let has_tunnel = cloudflare
        .as_ref()
        .and_then(|cf| cf.tunnel.as_ref())
        .is_some();
    let ssl = match email {
        Some(e) => Some(SslConfig::Letsencrypt { email: e }),
        None if interactive && !has_tunnel => {
            println!();
            println!("  Let's Encrypt email is needed for DNS-only exposure mode.");
            println!("  Leave empty to skip.");
            let le_email: String = Input::new()
                .with_prompt("Let's Encrypt email")
                .allow_empty(true)
                .interact_text()?;
            match le_email.is_empty() {
                true => None,
                false => Some(SslConfig::Letsencrypt { email: le_email }),
            }
        }
        None => None,
    };

    // 5. SMTP
    let smtp = if interactive {
        prompt_smtp_setup()?
    } else {
        None
    };

    // 6. Repo
    let default_repo = match repo {
        Some(r) => Some(r),
        None if interactive => {
            let url: String = Input::new()
                .with_prompt("Default repo (git URL or local path, empty to skip)")
                .allow_empty(true)
                .interact_text()?;
            if url.is_empty() { None } else { Some(url) }
        }
        None => None,
    };

    let config = Config {
        host: HostConfig { domain },
        cloudflare,
        ssl,
        smtp,
        auth: None,
        default_repo,
        registries: vec![],
        services: vec![],
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

/// Interactive Cloudflare setup — credentials only.
async fn prompt_cloudflare_setup() -> Result<(Option<CloudflareCredentials>, Option<String>)> {
    println!();
    println!("  Cloudflare handles DNS, SSL, and tunnels for your services.");
    println!("  Create an API token at: https://dash.cloudflare.com/profile/api-tokens");
    println!("  Permissions: Zone > DNS > Edit, Account > Cloudflare Tunnel > Edit");
    println!();

    let api_token: String = Input::new()
        .with_prompt("Cloudflare API token (leave empty to skip)")
        .allow_empty(true)
        .interact_text()?;

    match api_token.is_empty() {
        true => Ok((None, None)),
        false => {
            println!("  Fetching zones...");
            let zones = ryra_core::integrations::dns::list_zones(&api_token).await?;

            if zones.is_empty() {
                bail!("No zones found for this API token");
            }

            let zone_names: Vec<String> = zones.iter().map(|z| z.name.clone()).collect();
            let selection = dialoguer::FuzzySelect::new()
                .with_prompt("Select your zone")
                .items(&zone_names)
                .interact()?;

            let zone = &zones[selection];

            Ok((
                Some(CloudflareCredentials {
                    api_token,
                    zone_id: zone.id.clone(),
                    zone_name: zone.name.clone(),
                    tunnel: None,
                }),
                Some(zone.name.clone()),
            ))
        }
    }
}

/// Interactive tunnel setup.
async fn prompt_tunnel_setup(api_token: &str, zone_id: &str) -> Result<Option<TunnelInfo>> {
    println!();
    let setup = dialoguer::Confirm::new()
        .with_prompt("  Set up a Cloudflare Tunnel?")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    let account_id =
        match ryra_core::integrations::tunnel::get_account_id(api_token, zone_id).await {
            Ok(id) => id,
            Err(e) => {
                eprintln!("  Failed to get account ID: {e}");
                return Ok(None);
            }
        };

    println!("  Fetching tunnels...");
    let tunnels =
        match ryra_core::integrations::tunnel::list_tunnels(api_token, &account_id).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  Failed to list tunnels: {e}");
                eprintln!("  Token needs Account > Cloudflare Tunnel > Edit permission.");
                return Ok(None);
            }
        };

    if tunnels.is_empty() {
        println!("  No tunnels found.");
        let create = dialoguer::Confirm::new()
            .with_prompt("  Create a new tunnel?")
            .default(true)
            .interact()?;

        if !create {
            return Ok(None);
        }

        let name: String = Input::new()
            .with_prompt("Tunnel name")
            .default("ryra".into())
            .interact_text()?;

        println!("  Creating tunnel '{name}'...");
        let created =
            ryra_core::integrations::tunnel::create_tunnel(api_token, &account_id, &name).await?;
        println!("  Tunnel created: {} ({})", created.name, created.id);

        return Ok(Some(TunnelInfo {
            tunnel_token: created.token,
            tunnel_id: created.id,
            account_id,
        }));
    }

    let mut items: Vec<String> = tunnels.iter().map(|t| t.name.clone()).collect();
    items.push("Create new tunnel".into());

    let selection = dialoguer::FuzzySelect::new()
        .with_prompt("Select tunnel")
        .items(&items)
        .interact()?;

    if selection == tunnels.len() {
        let name: String = Input::new()
            .with_prompt("Tunnel name")
            .default("ryra".into())
            .interact_text()?;

        println!("  Creating tunnel '{name}'...");
        let created =
            ryra_core::integrations::tunnel::create_tunnel(api_token, &account_id, &name).await?;
        println!("  Tunnel created: {} ({})", created.name, created.id);

        return Ok(Some(TunnelInfo {
            tunnel_token: created.token,
            tunnel_id: created.id,
            account_id,
        }));
    }

    let tunnel = &tunnels[selection];
    println!("  Fetching token for '{}'...", tunnel.name);
    let token =
        ryra_core::integrations::tunnel::get_tunnel_token(api_token, &account_id, &tunnel.id)
            .await?;

    Ok(Some(TunnelInfo {
        tunnel_token: token,
        tunnel_id: tunnel.id.clone(),
        account_id,
    }))
}

/// Interactive SMTP setup.
fn prompt_smtp_setup() -> Result<Option<SmtpCredentials>> {
    println!();
    let setup = dialoguer::Confirm::new()
        .with_prompt("  Configure SMTP? (for email notifications, password resets)")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    let host: String = Input::new()
        .with_prompt("SMTP host")
        .interact_text()?;
    let port: u16 = Input::new()
        .with_prompt("SMTP port")
        .default(587)
        .interact_text()?;
    let username: String = Input::new()
        .with_prompt("SMTP username")
        .interact_text()?;
    let password: String = Input::new()
        .with_prompt("SMTP password")
        .interact_text()?;
    let from: String = Input::new()
        .with_prompt("From address")
        .default(format!("noreply@{host}"))
        .interact_text()?;

    Ok(Some(SmtpCredentials {
        host,
        port,
        username,
        password,
        from,
    }))
}
