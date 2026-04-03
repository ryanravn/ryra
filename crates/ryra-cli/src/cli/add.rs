use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Result, bail};
use dialoguer::{Confirm, Input};

use std::path::Path;

use ryra_core::Warning;
use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{Config, ExposureMode};
use ryra_core::registry::service_def::AuthKind;

use super::apply;
use super::prompts;

pub async fn run(
    services: &[String],
    domain: Option<&str>,
    repo: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if domain.is_some() && services.len() > 1 {
        bail!("--domain can only be used when adding a single service");
    }

    let (repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;

    for service in services {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    let interactive = std::io::stdin().is_terminal();

    // Look up the service definition
    let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;

    // Check architecture compatibility before any prompts
    if let Some(msg) = reg_service.def.check_architecture() {
        bail!("{msg}");
    }

    let has_nginx = reg_service.def.nginx.is_some();

    // Show ALL modes the service supports, annotate which need setup
    let supported = ExposureMode::supported_modes(has_nginx);

    let exposure = if supported.len() == 1 {
        let mode = supported[0].clone();
        println!("Exposure mode: {} — {}", mode.label(), mode.description());
        mode
    } else if interactive {
        let items: Vec<String> = supported
            .iter()
            .map(|m| {
                let missing = m.missing_config(&config);
                if missing.is_empty() {
                    format!("{} — {}", m.label(), m.description())
                } else {
                    format!("{} — {} (setup required)", m.label(), m.description())
                }
            })
            .collect();
        let selection = dialoguer::Select::new()
            .with_prompt("Exposure mode")
            .items(&items)
            .default(0)
            .interact()?;
        supported[selection].clone()
    } else {
        // Non-interactive: pick first mode that needs no setup
        supported
            .iter()
            .find(|m| m.missing_config(&config).is_empty())
            .cloned()
            .unwrap_or(ExposureMode::Local)
    };

    // Just-in-time: prompt for missing config sections
    if !exposure.missing_config(&config).is_empty() {
        if interactive {
            if !prompts::ensure_config_for_mode(&mut config, &paths, &exposure).await? {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            bail!(
                "{} exposure requires additional config. Run interactively or use `ryra config`.",
                exposure.label()
            );
        }
    }

    // Domain — only for proxied modes (tunnel/proxy/dns-only/tailscale)
    let domain = if exposure.needs_domain() {
        let default_domain = if exposure == ExposureMode::Tailscale {
            match ryra_core::integrations::tailscale::detect_fqdn() {
                Some(fqdn) => fqdn,
                None => {
                    bail!("Tailscale is not running or has no FQDN. Is tailscaled active?");
                }
            }
        } else {
            match config.base_domain() {
                Some(d) => format!("{service}.{d}"),
                None => format!("{service}.localhost"),
            }
        };
        Some(match domain {
            Some(d) => d.to_string(),
            None if interactive => Input::new()
                .with_prompt(format!("Domain for {service}"))
                .default(default_domain.clone())
                .interact_text()?,
            None => default_domain,
        })
    } else {
        None
    };

    // Auth — ask user if they want to enable auth (if the service supports it)
    let auth_kind: Option<AuthKind> = if reg_service.def.integrations.auth.is_empty() {
        None
    } else if reg_service.def.integrations.auth.len() == 1 {
        let kind = &reg_service.def.integrations.auth[0];
        if interactive {
            let enable = Confirm::new()
                .with_prompt(format!("Enable {kind} auth?"))
                .default(true)
                .interact()?;
            if enable {
                // Ensure auth is configured
                if config.auth.is_none() {
                    match ensure_auth_for_add(&mut config, &paths, &repo_url, &repo_dir, dry_run)
                        .await?
                    {
                        true => {}
                        false => return Ok(()),
                    }
                }
                Some(kind.clone())
            } else {
                None
            }
        } else {
            // Non-interactive: enable auth if configured
            if config.auth.is_some() {
                Some(kind.clone())
            } else {
                None
            }
        }
    } else if interactive {
        let items: Vec<String> = std::iter::once("None".to_string())
            .chain(
                reg_service
                    .def
                    .integrations
                    .auth
                    .iter()
                    .map(|k| k.to_string()),
            )
            .collect();
        let selection = dialoguer::Select::new()
            .with_prompt("Auth mode")
            .items(&items)
            .default(1)
            .interact()?;
        if selection == 0 {
            None
        } else {
            let kind = reg_service.def.integrations.auth[selection - 1].clone();
            if config.auth.is_none() {
                match ensure_auth_for_add(&mut config, &paths, &repo_url, &repo_dir, dry_run)
                    .await?
                {
                    true => {}
                    false => return Ok(()),
                }
            }
            Some(kind)
        }
    } else {
        None
    };

    // Prompt for env vars based on their kind
    use ryra_core::registry::service_def::EnvKind;

    let mut env_overrides = BTreeMap::new();
    let promptable: Vec<_> = reg_service
        .def
        .env
        .iter()
        .filter(|e| matches!(e.kind, EnvKind::Prompted | EnvKind::Required))
        .collect();

    if !promptable.is_empty() && interactive {
        println!("\nConfigure {service}:");
        for env in &promptable {
            let prompt_text = env.prompt.as_deref().unwrap_or(&env.name);
            let is_required = env.kind == EnvKind::Required;

            if is_required {
                // Required: must provide a value, no default
                let value: String = Input::new()
                    .with_prompt(format!("  {prompt_text} (required)"))
                    .interact_text()?;
                env_overrides.insert(env.name.clone(), value);
            } else {
                // Prompted: has a default, user can accept or change
                let value: String = Input::new()
                    .with_prompt(format!("  {prompt_text}"))
                    .default(env.value.clone())
                    .interact_text()?;
                if value != env.value {
                    env_overrides.insert(env.name.clone(), value);
                }
            }
        }
        println!();
    } else if !interactive {
        // Non-interactive: read required env vars from the process environment,
        // fail if any are still missing.
        let mut missing_required = Vec::new();
        for env in &promptable {
            if env.kind == EnvKind::Required {
                if let Ok(val) = std::env::var(&env.name) {
                    env_overrides.insert(env.name.clone(), val);
                } else {
                    missing_required.push(env.name.as_str());
                }
            }
        }
        if !missing_required.is_empty() {
            bail!(
                "required env vars not provided (run interactively or set via env): {}",
                missing_required.join(", ")
            );
        }
    }

    let result = ryra_core::add_service(
        service,
        domain.as_deref(),
        exposure.clone(),
        auth_kind.clone(),
        &env_overrides,
        &repo_url,
        &repo_dir,
    )?;

    // Show warnings and confirm
    if !result.warnings.is_empty() {
        println!();
        for warning in &result.warnings {
            match warning {
                Warning::NoAuthPublicExposure {
                    service_name,
                    exposure,
                } => {
                    println!(
                        "  WARNING: {service_name} has auth disabled and will be publicly exposed via {exposure}"
                    );
                }
                Warning::HostPortExposure {
                    service_name,
                    ports,
                } => {
                    let port_list: Vec<String> = ports
                        .iter()
                        .map(|(p, proto)| format!("{p}/{proto}"))
                        .collect();
                    println!(
                        "  WARNING: {service_name} will bind to 0.0.0.0 on ports: {}",
                        port_list.join(", ")
                    );
                }
                Warning::OidcLocalExposure {
                    service_name,
                    exposure,
                } => {
                    println!(
                        "  NOTE: {service_name} has OIDC auth enabled with {exposure} exposure."
                    );
                    println!(
                        "        OIDC login via browser (e.g. SSH tunnel) will likely fail because"
                    );
                    println!(
                        "        the browser and server disagree on redirect URLs."
                    );
                    println!(
                        "        Consider using 'tailscale' exposure mode for OIDC to work."
                    );
                }
                Warning::RamBelowMinimum {
                    service_name,
                    min_mb,
                    available_mb,
                } => {
                    println!(
                        "  WARNING: {service_name} requires at least {min_mb} MB RAM, \
                         but this system has {available_mb} MB — service may fail to start"
                    );
                }
                Warning::RamBelowRecommended {
                    service_name,
                    recommended_mb,
                    available_mb,
                } => {
                    println!(
                        "  NOTE: {service_name} recommends {recommended_mb} MB RAM, \
                         but this system has {available_mb} MB — performance may be degraded"
                    );
                }
            }
        }
        println!();

        if interactive && !dry_run {
            let confirmed = Confirm::new()
                .with_prompt("Continue with these warnings?")
                .default(true)
                .interact()?;
            if !confirmed {
                println!("Cancelled.");
                return Ok(());
            }
        }
    }

    if dry_run {
        super::print_dry_run(&result.steps);
        if let Some(ref domain) = result.domain {
            if domain.ends_with(".ts.net") {
                if let Some(port) = result.host_port {
                    println!("{service} will be available at https://{domain}:{port}");
                } else {
                    println!("{service} will be available at https://{domain}");
                }
            } else {
                println!("{service} will be available at https://{domain}");
            }
        } else {
            println!("{service} will be started (no domain — non-web service)");
        }
    } else {
        println!("Setting up {service} as user {}...", result.username);
        apply::execute_all(&result.steps).await?;
        ryra_core::finalize_add(ryra_core::FinalizeAddParams {
            service_name: service,
            domain: domain.as_deref(),
            exposure,
            auth_kind,
            repo: &result.repo_url,
            host_port: result.host_port,
            allocated_ports: &result.allocated_ports,
            repo_dir: &repo_dir,
            env_content: &result.env_content,
        })?;
        let home_dir = ryra_core::service_home(service);
        if let Some(ref domain) = result.domain {
            if domain.ends_with(".ts.net") {
                if let Some(port) = result.host_port {
                    println!("\n{service} is running at https://{domain}:{port}");
                } else {
                    println!("\n{service} is running at https://{domain}");
                }
            } else {
                println!("\n{service} is running at https://{domain}");
            }
        } else {
            println!("\n{service} is running.");
        }

        // Connection info
        if !result.allocated_ports.is_empty() {
            for (port_name, host_port) in &result.allocated_ports {
                println!("  Port ({port_name}): 127.0.0.1:{host_port}");
            }
        }
        if !result.generated_secrets.is_empty() {
            println!(
                "  Secrets: {} (auto-generated)",
                result.generated_secrets.join(", ")
            );
        }
        println!("  Config:  {}", home_dir.display());

        let u = &result.username;
        println!();
        println!("Useful commands:");
        println!("  sudo cat {}", home_dir.join(".env").display());
        println!("  sudo systemctl --machine={u}@ --user status {service}");
        println!("  sudo journalctl _SYSTEMD_USER_UNIT={service}.service -f");
        println!("  sudo systemctl --machine={u}@ --user restart {service}");
    }

    } // end for service in services

    Ok(())
}

/// Ensure auth is configured, possibly installing authentik inline.
/// Returns true if auth is ready, false if user cancelled.
async fn ensure_auth_for_add(
    config: &mut Config,
    paths: &ConfigPaths,
    repo_url: &str,
    _repo_dir: &Path,
    dry_run: bool,
) -> Result<bool> {
    match prompts::ensure_auth_configured(config, paths).await? {
        prompts::AuthSetupChoice::External(_) => Ok(true),
        prompts::AuthSetupChoice::InstallAuthentik => {
            // Check if authentik is already installed but auth wasn't configured
            let authentik_installed = config.services.iter().any(|s| s.name == "authentik");
            if authentik_installed {
                println!();
                println!("Authentik is already installed — configuring auth...");
                if try_configure_auth_from_installed(config, paths)? {
                    return Ok(true);
                }
                println!("Could not auto-configure auth from installed authentik.");
                return Ok(false);
            }

            println!();
            println!("Installing authentik first...");
            println!();
            // Recursively install authentik, then reload config
            Box::pin(run(&["authentik".to_string()], None, Some(repo_url), dry_run)).await?;
            // Reload config — authentik's finalize_add auto-configures [auth]
            *config = ryra_core::config::load_or_default(&paths.config_file)?;
            if config.auth.is_some() {
                println!();
                Ok(true)
            } else {
                println!("Auth was not configured after installing authentik.");
                Ok(false)
            }
        }
        prompts::AuthSetupChoice::Skip => {
            println!("Skipped auth setup.");
            Ok(false)
        }
    }
}

/// Try to configure auth from an already-installed authentik instance.
/// Reads the .env via sudo since it's owned by the authentik user.
fn try_configure_auth_from_installed(config: &mut Config, paths: &ConfigPaths) -> Result<bool> {
    let env_path = ryra_core::service_home("authentik").join(".env");
    let output = std::process::Command::new("sudo")
        .args(["cat", &env_path.to_string_lossy()])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let env_content = String::from_utf8_lossy(&output.stdout);

    // Find the bootstrap token
    let token = env_content
        .lines()
        .find_map(|line| line.strip_prefix("AUTHENTIK_BOOTSTRAP_TOKEN="))
        .map(|v| v.to_string());

    let Some(token) = token else {
        return Ok(false);
    };

    // Find the URL from the installed service record
    let service = config.services.iter().find(|s| s.name == "authentik");
    let url = match service.and_then(|s| s.domain.as_deref()) {
        Some(domain) => format!("https://{domain}"),
        None => {
            let port = service.and_then(|s| s.host_port).unwrap_or(9000);
            format!("http://localhost:{port}")
        }
    };

    config.auth = Some(ryra_core::config::schema::AuthCredentials::Authentik {
        url,
        api_token: token,
    });
    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, config)?;
    println!(
        "  Auth configured. Saved to {}",
        paths.config_file.display()
    );
    Ok(true)
}
