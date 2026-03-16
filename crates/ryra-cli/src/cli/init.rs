use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::Input;

use ryra_core::config::schema::*;

use super::apply;

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

    let existing_cf = existing
        .as_ref()
        .and_then(|c| c.dns.cloudflare_credentials().map(|(t, z, n)| {
            (t.to_string(), z.to_string(), n.to_string(), c.dns.is_proxied())
        }));

    // 1. Cloudflare first — if configured, we derive the domain from it
    let (dns, derived_domain) = match (cf_token, cf_zone_id, cf_zone_name) {
        // All provided via flags — defaults to proxy mode
        (Some(token), Some(zone_id), Some(zone_name)) => (
            DnsConfig::CloudflareProxy {
                api_token: token,
                zone_id,
                zone_name: zone_name.clone(),
            },
            Some(zone_name),
        ),
        // Interactive setup
        (None, None, None) if interactive => {
            // If we have existing Cloudflare config, offer to reuse it
            match existing_cf {
                Some((token, zone_id, zone_name, is_proxied)) => {
                    let masked = format!("{}...{}", &token[..4], &token[token.len()-4..]);
                    println!();
                    println!("  Existing Cloudflare config found: {zone_name} (token: {masked})");

                    let keep = dialoguer::Confirm::new()
                        .with_prompt("  Keep existing Cloudflare configuration?")
                        .default(true)
                        .interact()?;

                    match keep {
                        true => {
                            let dns = match is_proxied {
                                true => DnsConfig::CloudflareProxy {
                                    api_token: token,
                                    zone_id,
                                    zone_name: zone_name.clone(),
                                },
                                false => DnsConfig::CloudflareDns {
                                    api_token: token,
                                    zone_id,
                                    zone_name: zone_name.clone(),
                                },
                            };
                            (dns, Some(zone_name))
                        }
                        false => prompt_cloudflare_setup().await?,
                    }
                }
                None => prompt_cloudflare_setup().await?,
            }
        }
        // Non-interactive without Cloudflare
        (None, None, None) => (DnsConfig::None, None),
        // Partial flags
        _ => bail!("--cf-token, --cf-zone-id, and --cf-zone-name must all be provided together"),
    };

    // 2. Domain — use Cloudflare zone if available, otherwise ask
    let existing_domain = existing.as_ref().map(|c| c.host.domain.clone());
    let domain = match domain {
        Some(d) => d,
        None => match (derived_domain, existing_domain) {
            (Some(d), _) | (None, Some(d)) => {
                match interactive {
                    true => Input::new()
                        .with_prompt("Domain")
                        .default(d)
                        .interact_text()?,
                    false => d,
                }
            }
            (None, None) => match interactive {
                true => Input::new()
                    .with_prompt("Your domain (e.g., example.com)")
                    .interact_text()?,
                false => bail!("--domain is required in non-interactive mode"),
            },
        },
    };

    // 3. SSL — depends on DNS mode
    let ssl = match dns.is_proxied() {
        true => {
            println!("  SSL: Cloudflare proxy handles public SSL. A self-signed origin cert will be generated.");
            SslConfig::CloudflareOrigin
        }
        false => {
            let existing_email = existing.as_ref().and_then(|c| match &c.ssl {
                SslConfig::Letsencrypt { email } => Some(email.clone()),
                _ => None,
            });
            let ssl_email = match email {
                Some(e) => e,
                None if interactive => match existing_email {
                    Some(e) => Input::new()
                        .with_prompt("Email for Let's Encrypt SSL certificates")
                        .default(e)
                        .interact_text()?,
                    None => Input::new()
                        .with_prompt("Email for Let's Encrypt SSL certificates")
                        .interact_text()?,
                },
                None => bail!("--email is required in non-interactive mode"),
            };
            SslConfig::Letsencrypt { email: ssl_email }
        }
    };

    // 4. Cloudflare Tunnel (optional)
    let existing_tunnel = existing.as_ref().and_then(|c| match &c.tunnel {
        TunnelConfig::Cloudflare { .. } => Some(c.tunnel.clone()),
        TunnelConfig::None => None,
    });

    let tunnel = match tunnel_token {
        // CLI flag — need tunnel_id and account_id too (not ideal for non-interactive, but works)
        Some(_token) => {
            // For non-interactive with just a token, we'd need more flags.
            // For now, require interactive setup for tunnel.
            bail!("Use interactive mode for tunnel setup (need to select tunnel)")
        }
        None if interactive => {
            match existing_tunnel {
                Some(existing_tc) => {
                    println!("  Existing tunnel configuration found.");
                    let keep = dialoguer::Confirm::new()
                        .with_prompt("  Keep existing tunnel configuration?")
                        .default(true)
                        .interact()?;
                    match keep {
                        true => existing_tc,
                        false => prompt_tunnel_setup(&dns).await?,
                    }
                }
                None => prompt_tunnel_setup(&dns).await?,
            }
        }
        None => TunnelConfig::None,
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
        dns,
        tunnel,
        ssl,
        smtp: existing.as_ref().map(|c| c.smtp.clone()).unwrap_or_default(),
        auth: existing.as_ref().map(|c| c.auth.clone()).unwrap_or_default(),
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
    if interactive && config.dns.cloudflare_credentials().is_some() {
        let save_token = dialoguer::Confirm::new()
            .with_prompt("Save Cloudflare API token in config? (needed for auto DNS on future services)")
            .default(true)
            .interact()?;

        match save_token {
            true => {}
            false => {
                let mut saved_config = ryra_core::config::load_config(
                    &ryra_core::config::ConfigPaths::resolve()?.config_file,
                )?;
                saved_config.dns = DnsConfig::None;
                ryra_core::config::save_config(
                    &ryra_core::config::ConfigPaths::resolve()?.config_file,
                    &saved_config,
                )?;
                println!("API token removed from config. DNS records will need to be created manually.");
            }
        }
    }

    Ok(())
}

/// Interactive Cloudflare setup flow — extracted to avoid duplication.
async fn prompt_cloudflare_setup() -> Result<(DnsConfig, Option<String>)> {
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
        true => Ok((DnsConfig::None, None)),
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

            let mode_items = [
                "Proxy (recommended) — CF handles SSL, DDoS protection, caching",
                "DNS only — CF manages records, Let's Encrypt for SSL",
            ];
            let mode = dialoguer::Select::new()
                .with_prompt("Cloudflare mode")
                .items(&mode_items)
                .default(0)
                .interact()?;

            let dns = match mode {
                0 => DnsConfig::CloudflareProxy {
                    api_token,
                    zone_id: zone.id.clone(),
                    zone_name: zone.name.clone(),
                },
                _ => DnsConfig::CloudflareDns {
                    api_token,
                    zone_id: zone.id.clone(),
                    zone_name: zone.name.clone(),
                },
            };

            Ok((dns, Some(zone.name.clone())))
        }
    }
}

/// Interactive tunnel setup. Uses CF API token (from DNS config if available) to list tunnels.
async fn prompt_tunnel_setup(dns: &DnsConfig) -> Result<TunnelConfig> {
    println!();
    println!("  Cloudflare Tunnel exposes services without port forwarding.");
    println!("  No open ports needed — traffic flows through an outbound connection.");
    println!();

    // Tunnel requires the Cloudflare API token from DNS setup
    let (api_token, zone_id_from_dns) = match dns.cloudflare_credentials() {
        Some((token, zone_id, _)) => (token.to_string(), Some(zone_id.to_string())),
        None => {
            println!("  Tunnel requires Cloudflare DNS to be configured first.");
            println!("  Re-run `ryra init` and enter a Cloudflare API token in the first step.");
            return Ok(TunnelConfig::None);
        }
    };

    // Step 1: Get a zone_id to look up the account
    let zone_id = match zone_id_from_dns {
        Some(zid) => zid,
        None => {
            match ryra_core::integrations::dns::list_zones(&api_token).await {
                Ok(zones) => match zones.first() {
                    Some(z) => z.id.clone(),
                    None => {
                        eprintln!("  No zones found. The token needs Zone > DNS > Edit permission");
                        eprintln!("  to access at least one zone (needed to look up your account).");
                        return Ok(TunnelConfig::None);
                    }
                },
                Err(e) => {
                    eprintln!("  Failed to list zones: {e}");
                    eprintln!("  The token may be invalid or missing Zone permissions.");
                    return Ok(TunnelConfig::None);
                }
            }
        }
    };

    // Step 2: Get account ID from zone data
    let account_id = match ryra_core::integrations::tunnel::get_account_id(&api_token, &zone_id).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("  Failed to get account ID: {e}");
            eprintln!("  The token may not have access to the zone details.");
            return Ok(TunnelConfig::None);
        }
    };

    // Step 3: List tunnels
    println!("  Fetching tunnels...");
    let tunnels = match ryra_core::integrations::tunnel::list_tunnels(&api_token, &account_id).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  Failed to list tunnels: {e}");
            eprintln!("  The token needs Account > Cloudflare Tunnel > Read permission.");
            eprintln!("  Update your token at: https://dash.cloudflare.com/profile/api-tokens");
            return Ok(TunnelConfig::None);
        }
    };

    match tunnels.is_empty() {
        true => {
            println!("  No tunnels found.");
            println!("  Create one at: https://one.dash.cloudflare.com/ > Networks > Tunnels");
            println!("  Then re-run `ryra init` to configure it.");
            Ok(TunnelConfig::None)
        }
        false => {
            let tunnel_names: Vec<String> = tunnels.iter().map(|t| t.name.clone()).collect();
            let selection = dialoguer::FuzzySelect::new()
                .with_prompt("Select your tunnel")
                .items(&tunnel_names)
                .interact()?;

            let tunnel = &tunnels[selection];

            println!();
            println!("  Copy the tunnel token from: Tunnel > {} > Install", tunnel.name);
            let token: String = Input::new()
                .with_prompt("Tunnel token")
                .interact_text()?;

            Ok(TunnelConfig::Cloudflare {
                tunnel_token: token,
                tunnel_id: tunnel.id.clone(),
                account_id,
            })
        }
    }
}

