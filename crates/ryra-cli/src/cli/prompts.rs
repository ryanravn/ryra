use anyhow::Result;
use dialoguer::Input;

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::*;

/// Interactive Cloudflare setup — credentials only.
pub async fn prompt_cloudflare() -> Result<Option<CloudflareCredentials>> {
    println!();
    println!("  Cloudflare handles DNS, SSL, and tunnels for your services.");
    println!("  Create an API token at: https://dash.cloudflare.com/profile/api-tokens");
    println!("  Permissions: Zone > DNS > Edit, Account > Cloudflare Tunnel > Edit");
    println!();

    let api_token: String = Input::new()
        .with_prompt("Cloudflare API token (leave empty to skip)")
        .allow_empty(true)
        .interact_text()?;

    if api_token.is_empty() {
        return Ok(None);
    }

    println!("  Fetching zones...");
    let zones = ryra_core::integrations::dns::list_zones(&api_token).await?;

    if zones.is_empty() {
        anyhow::bail!("No zones found for this API token");
    }

    let zone_names: Vec<String> = zones.iter().map(|z| z.name.clone()).collect();
    let selection = dialoguer::FuzzySelect::new()
        .with_prompt("Select your zone")
        .items(&zone_names)
        .interact()?;

    let zone = &zones[selection];

    Ok(Some(CloudflareCredentials {
        api_token,
        zone_id: zone.id.clone(),
        zone_name: zone.name.clone(),
        tunnel: None,
    }))
}

/// Interactive tunnel setup.
pub async fn prompt_tunnel(api_token: &str, zone_id: &str) -> Result<Option<TunnelInfo>> {
    println!();
    let setup = dialoguer::Confirm::new()
        .with_prompt("  Set up a Cloudflare Tunnel?")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    let account_id = match ryra_core::integrations::tunnel::get_account_id(api_token, zone_id).await
    {
        Ok(id) => id,
        Err(e) => {
            eprintln!("  Failed to get account ID: {e}");
            return Ok(None);
        }
    };

    println!("  Fetching tunnels...");
    let tunnels = match ryra_core::integrations::tunnel::list_tunnels(api_token, &account_id).await
    {
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

/// Interactive SSL setup — Let's Encrypt or custom certs.
pub fn prompt_ssl() -> Result<Option<SslConfig>> {
    println!();

    let items = vec![
        "Let's Encrypt (automatic, needs port 80 open)",
        "Custom certificates (provide your own)",
        "Skip",
    ];
    let selection = dialoguer::Select::new()
        .with_prompt("SSL provider")
        .items(&items)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            let email: String = Input::new()
                .with_prompt("Let's Encrypt email")
                .interact_text()?;
            Ok(Some(SslConfig::Letsencrypt { email }))
        }
        1 => {
            println!("  Certs should be at <cert_dir>/<domain>/fullchain.pem and privkey.pem");
            let cert_dir: String = Input::new()
                .with_prompt("Certificate directory")
                .default("/etc/ryra/certs".into())
                .interact_text()?;
            Ok(Some(SslConfig::Custom { cert_dir }))
        }
        _ => Ok(None),
    }
}

/// Interactive SMTP setup.
pub fn prompt_smtp() -> Result<Option<SmtpCredentials>> {
    println!();
    let setup = dialoguer::Confirm::new()
        .with_prompt("  Configure SMTP? (for email notifications, password resets)")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    let host: String = Input::new().with_prompt("SMTP host").interact_text()?;
    let port: u16 = Input::new()
        .with_prompt("SMTP port")
        .default(587)
        .interact_text()?;
    let username: String = Input::new().with_prompt("SMTP username").interact_text()?;
    let password: String = Input::new().with_prompt("SMTP password").interact_text()?;
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

/// Prompt for any missing config sections required by the chosen exposure mode.
/// Mutates config in-place and saves globally. Returns false if user cancelled.
pub async fn ensure_config_for_mode(
    config: &mut Config,
    paths: &ConfigPaths,
    exposure: &ExposureMode,
) -> Result<bool> {
    let missing = exposure.missing_config(config);
    if missing.is_empty() {
        return Ok(true);
    }

    println!();
    println!("  {} mode requires additional setup:", exposure.label());

    for req in &missing {
        match req {
            ConfigRequirement::Cloudflare => {
                println!();
                println!("  This will be saved globally and reused for future services.");
                match prompt_cloudflare().await? {
                    Some(cf) => config.cloudflare = Some(cf),
                    None => return Ok(false),
                }
            }
            ConfigRequirement::CloudflareTunnel => {
                // Safe: CloudflareTunnel always follows Cloudflare in missing_config(),
                // so cloudflare is guaranteed to be Some at this point.
                let (api_token, zone_id) = match config.cloudflare.as_ref() {
                    Some(cf) => (cf.api_token.clone(), cf.zone_id.clone()),
                    None => return Ok(false),
                };
                match prompt_tunnel(&api_token, &zone_id).await? {
                    Some(tunnel) => {
                        if let Some(ref mut cf) = config.cloudflare {
                            cf.tunnel = Some(tunnel);
                        }
                    }
                    None => return Ok(false),
                }
            }
            ConfigRequirement::Ssl => {
                println!();
                println!("  This will be saved globally and reused for future services.");
                match prompt_ssl()? {
                    Some(ssl) => config.ssl = Some(ssl),
                    None => return Ok(false),
                }
            }
        }
    }

    // Save updated config
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, config)?;
    println!("  Config saved to {}", paths.config_file.display());
    Ok(true)
}
