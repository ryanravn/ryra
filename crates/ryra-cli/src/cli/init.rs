use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::Input;

use ryra_core::config::schema::*;

use super::apply;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    domain: Option<String>,
    registry: Option<String>,
    email: Option<String>,
    cf_token: Option<String>,
    cf_zone_id: Option<String>,
    cf_zone_name: Option<String>,
    tunnel_token: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal();

    // Load existing config if present (for re-init)
    let existing = ryra_core::config::ConfigPaths::resolve()
        .ok()
        .and_then(|p| ryra_core::config::load_config(&p.config_file).ok());

    let existing_cf = existing.as_ref().and_then(|c| match &c.cloudflare {
        CloudflareConfig::Configured {
            api_token,
            zone_id,
            zone_name,
            tunnel,
        } => Some((
            api_token.clone(),
            zone_id.clone(),
            zone_name.clone(),
            tunnel.clone(),
        )),
        CloudflareConfig::None => None,
    });

    // 1. Cloudflare credentials (no proxy/dns-only question — that's per-service)
    let (cloudflare, derived_domain) = match (cf_token, cf_zone_id, cf_zone_name) {
        // All provided via flags
        (Some(token), Some(zone_id), Some(zone_name)) => (
            CloudflareConfig::Configured {
                api_token: token,
                zone_id,
                zone_name: zone_name.clone(),
                tunnel: None,
            },
            Some(zone_name),
        ),
        // Interactive setup
        (None, None, None) if interactive => {
            match existing_cf {
                Some((token, zone_id, zone_name, tunnel)) => {
                    let masked = format!("{}...{}", &token[..4], &token[token.len() - 4..]);
                    println!();
                    println!(
                        "  Existing Cloudflare config found: {zone_name} (token: {masked})"
                    );

                    let keep = dialoguer::Confirm::new()
                        .with_prompt("  Keep existing Cloudflare configuration?")
                        .default(true)
                        .interact()?;

                    match keep {
                        true => {
                            let cf = CloudflareConfig::Configured {
                                api_token: token,
                                zone_id,
                                zone_name: zone_name.clone(),
                                tunnel,
                            };
                            (cf, Some(zone_name))
                        }
                        false => prompt_cloudflare_setup().await?,
                    }
                }
                None => prompt_cloudflare_setup().await?,
            }
        }
        // Non-interactive without Cloudflare
        (None, None, None) => (CloudflareConfig::None, None),
        // Partial flags
        _ => bail!("--cf-token, --cf-zone-id, and --cf-zone-name must all be provided together"),
    };

    // 2. Tunnel setup (only if CF is configured)
    let cloudflare = match (&cloudflare, tunnel_token) {
        (CloudflareConfig::Configured { tunnel: Some(_), .. }, _) => {
            // Already has tunnel from existing config reuse
            cloudflare
        }
        (CloudflareConfig::Configured { .. }, Some(_token)) => {
            // CLI flag for tunnel — need interactive for selecting tunnel
            bail!("Use interactive mode for tunnel setup (need to select tunnel)");
        }
        (CloudflareConfig::Configured { api_token, zone_id, zone_name, .. }, None)
            if interactive =>
        {
            let tunnel = prompt_tunnel_setup(api_token, zone_id).await?;
            CloudflareConfig::Configured {
                api_token: api_token.clone(),
                zone_id: zone_id.clone(),
                zone_name: zone_name.clone(),
                tunnel,
            }
        }
        _ => cloudflare,
    };

    // 3. Domain — use Cloudflare zone if available, otherwise ask
    let existing_domain = existing.as_ref().map(|c| c.host.domain.clone());
    let domain = match domain {
        Some(d) => d,
        None => match (derived_domain, existing_domain) {
            (Some(d), _) | (None, Some(d)) => match interactive {
                true => Input::new()
                    .with_prompt("Domain")
                    .default(d)
                    .interact_text()?,
                false => d,
            },
            (None, None) => match interactive {
                true => Input::new()
                    .with_prompt("Your domain (e.g., example.com)")
                    .interact_text()?,
                false => bail!("--domain is required in non-interactive mode"),
            },
        },
    };

    // 4. Let's Encrypt email (only ask if they might use DnsOnly mode — i.e., not tunnel-only)
    let has_tunnel = cloudflare.tunnel_info().is_some();
    let ssl = match email {
        Some(e) => Some(SslConfig::Letsencrypt { email: e }),
        None if interactive && !has_tunnel => {
            // They might use DnsOnly mode, so ask for LE email
            let existing_email = existing.as_ref().and_then(|c| match &c.ssl {
                Some(SslConfig::Letsencrypt { email }) => Some(email.clone()),
                _ => None,
            });
            println!();
            println!("  Let's Encrypt email is needed for DNS-only exposure mode.");
            println!("  Leave empty to skip (you can configure it later when adding a service).");
            let le_email: String = match existing_email {
                Some(e) => Input::new()
                    .with_prompt("Let's Encrypt email")
                    .default(e)
                    .allow_empty(true)
                    .interact_text()?,
                None => Input::new()
                    .with_prompt("Let's Encrypt email")
                    .allow_empty(true)
                    .interact_text()?,
            };
            match le_email.is_empty() {
                true => None,
                false => Some(SslConfig::Letsencrypt { email: le_email }),
            }
        }
        None => {
            // Preserve existing SSL config if present
            existing.as_ref().and_then(|c| c.ssl.clone())
        }
    };

    // 5. Registry
    let existing_registry = existing
        .as_ref()
        .and_then(|c| c.registries.first().map(|r| r.url.clone()));
    let registry_url = match registry {
        Some(r) => r,
        None if interactive => {
            let default = existing_registry
                .unwrap_or_else(|| "https://github.com/user/ryra-registry".into());
            Input::new()
                .with_prompt("Registry URL (git repo or local path)")
                .default(default)
                .interact_text()?
        }
        None => bail!("--registry is required in non-interactive mode"),
    };

    let config = Config {
        host: HostConfig { domain },
        cloudflare,
        ssl,
        smtp: existing
            .as_ref()
            .map(|c| c.smtp.clone())
            .unwrap_or_default(),
        auth: existing
            .as_ref()
            .map(|c| c.auth.clone())
            .unwrap_or_default(),
        registries: vec![RegistryEntry {
            name: "default".into(),
            url: registry_url,
        }],
        services: existing.map(|c| c.services).unwrap_or_default(),
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
            println!("\nryra initialized! Run `ryra list` to see available services.");
        }
    }

    // After everything succeeds, ask about persisting the API token
    if interactive && config.cloudflare.credentials().is_some() {
        let save_token = dialoguer::Confirm::new()
            .with_prompt(
                "Save Cloudflare API token in config? (needed for auto DNS on future services)",
            )
            .default(true)
            .interact()?;

        match save_token {
            true => {}
            false => {
                let mut saved_config = ryra_core::config::load_config(
                    &ryra_core::config::ConfigPaths::resolve()?.config_file,
                )?;
                saved_config.cloudflare = CloudflareConfig::None;
                ryra_core::config::save_config(
                    &ryra_core::config::ConfigPaths::resolve()?.config_file,
                    &saved_config,
                )?;
                println!(
                    "API token removed from config. DNS records will need to be created manually."
                );
            }
        }
    }

    Ok(())
}

/// Interactive Cloudflare setup flow — credentials only, no proxy/dns-only choice.
async fn prompt_cloudflare_setup() -> Result<(CloudflareConfig, Option<String>)> {
    println!();
    println!("  Cloudflare handles DNS, SSL, and tunnels for your services.");
    println!("  One token covers everything — DNS records, proxy, and tunnel management.");
    println!();
    println!("  Create an API token at: https://dash.cloudflare.com/profile/api-tokens");
    println!("  Permissions needed:");
    println!("    Zone > DNS > Edit");
    println!("    Account > Cloudflare Tunnel > Edit  (only if using tunnel)");
    println!();

    let api_token: String = Input::new()
        .with_prompt("Cloudflare API token (leave empty to skip)")
        .allow_empty(true)
        .interact_text()?;

    match api_token.is_empty() {
        true => Ok((CloudflareConfig::None, None)),
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
                CloudflareConfig::Configured {
                    api_token,
                    zone_id: zone.id.clone(),
                    zone_name: zone.name.clone(),
                    tunnel: None,
                },
                Some(zone.name.clone()),
            ))
        }
    }
}

/// Interactive tunnel setup. Uses CF API token to list/create tunnels.
async fn prompt_tunnel_setup(api_token: &str, zone_id: &str) -> Result<Option<TunnelInfo>> {
    println!();
    println!("  Cloudflare Tunnel exposes services without port forwarding.");
    println!("  No open ports needed — traffic flows through an outbound connection.");
    println!();

    let setup = dialoguer::Confirm::new()
        .with_prompt("  Set up a Cloudflare Tunnel?")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    // Get account ID from zone data
    let account_id =
        match ryra_core::integrations::tunnel::get_account_id(api_token, zone_id).await {
            Ok(id) => id,
            Err(e) => {
                eprintln!("  Failed to get account ID: {e}");
                eprintln!("  The token may not have access to the zone details.");
                return Ok(None);
            }
        };

    // List tunnels
    println!("  Fetching tunnels...");
    let tunnels =
        match ryra_core::integrations::tunnel::list_tunnels(api_token, &account_id).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  Failed to list tunnels: {e}");
                eprintln!("  The token needs Account > Cloudflare Tunnel > Edit permission.");
                eprintln!(
                    "  Update your token at: https://dash.cloudflare.com/profile/api-tokens"
                );
                return Ok(None);
            }
        };

    let (tunnel_id, tunnel_name) = if tunnels.is_empty() {
        // Offer to create one
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
    } else {
        // Select from existing
        let mut items: Vec<String> = tunnels.iter().map(|t| t.name.clone()).collect();
        items.push("Create new tunnel".into());

        let selection = dialoguer::FuzzySelect::new()
            .with_prompt("Select tunnel")
            .items(&items)
            .interact()?;

        if selection == tunnels.len() {
            // Create new
            let name: String = Input::new()
                .with_prompt("Tunnel name")
                .default("ryra".into())
                .interact_text()?;

            println!("  Creating tunnel '{name}'...");
            let created =
                ryra_core::integrations::tunnel::create_tunnel(api_token, &account_id, &name)
                    .await?;

            println!("  Tunnel created: {} ({})", created.name, created.id);

            return Ok(Some(TunnelInfo {
                tunnel_token: created.token,
                tunnel_id: created.id,
                account_id,
            }));
        }

        let tunnel = &tunnels[selection];
        (tunnel.id.clone(), tunnel.name.clone())
    };

    // Fetch token for existing tunnel
    println!("  Fetching token for '{tunnel_name}'...");
    let token =
        ryra_core::integrations::tunnel::get_tunnel_token(api_token, &account_id, &tunnel_id)
            .await?;

    Ok(Some(TunnelInfo {
        tunnel_token: token,
        tunnel_id,
        account_id,
    }))
}
