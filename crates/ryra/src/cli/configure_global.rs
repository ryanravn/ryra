//! `ryra configure` with no service argument: edit global preferences
//! (the SMTP relay, the admin email) and then propagate the change into
//! every installed service that renders an env var from it.
//!
//! The editing half writes `preferences.toml`; the propagation half is core's
//! [`ryra_core::reconcile_service`], which re-renders each installed service
//! and surfaces only the env keys driven by global config. This module owns
//! the user-facing flow: collect the edit (flags or prompts), save, show the
//! per-service env diff, let the user pick which services to apply, restart.

use anyhow::{Result, bail};
use console::style;
use dialoguer::{Input, MultiSelect, Select};

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::{Config, SmtpCredentials, SmtpSecurity};
use ryra_core::{EnvKeyChange, ServiceReconcile};

use super::apply;

/// Flags for the no-service (global) form of `ryra configure`. Each SMTP
/// field is individually settable so a script can change one value without
/// restating the rest.
#[derive(Debug, Default, Clone)]
pub struct GlobalFlags {
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub smtp_from: Option<String>,
    pub smtp_security: Option<String>,
    pub admin_email: Option<String>,
    pub yes: bool,
    pub dry_run: bool,
}

impl GlobalFlags {
    fn has_smtp_edit(&self) -> bool {
        self.smtp_host.is_some()
            || self.smtp_port.is_some()
            || self.smtp_username.is_some()
            || self.smtp_password.is_some()
            || self.smtp_from.is_some()
            || self.smtp_security.is_some()
    }

    fn has_any_edit(&self) -> bool {
        self.has_smtp_edit() || self.admin_email.is_some()
    }
}

pub async fn run(flags: GlobalFlags) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file)?;
    let had_secrets_before = config.has_secrets();

    let changed = if flags.has_any_edit() {
        apply_flag_edits(&mut config, &flags)?
    } else if super::is_interactive() {
        edit_interactive(&mut config)?
    } else {
        print_current(&config);
        println!();
        println!(
            "No changes specified. Pass --smtp-host / --smtp-password / --admin-email / … \
             (or run in a terminal to edit interactively)."
        );
        return Ok(());
    };

    if !changed {
        println!("No changes - global config left as-is.");
        return Ok(());
    }

    // Dry run must end fully non-mutating. But `reconcile_service` reads the
    // global config from disk, so to preview propagation we have to make the
    // new values visible: snapshot the file, write the edit, plan, then put
    // the original back. No service steps are executed (planning is pure), so
    // the only transient change is the config file, which we restore.
    if flags.dry_run {
        let existed = paths.config_file.exists();
        let snapshot = if existed {
            Some(std::fs::read(&paths.config_file).map_err(|e| {
                anyhow::anyhow!(
                    "reading {} for dry-run snapshot: {e}",
                    paths.config_file.display()
                )
            })?)
        } else {
            None
        };
        paths.ensure_dirs()?;
        ryra_core::config::save_config(&paths.config_file, &config)?;
        let plans = collect_plans().await;
        // Restore before surfacing any planning error, so a failure can't
        // leave the temporarily-written config behind.
        match &snapshot {
            Some(bytes) => std::fs::write(&paths.config_file, bytes).map_err(|e| {
                anyhow::anyhow!(
                    "restoring {} after dry run: {e}",
                    paths.config_file.display()
                )
            })?,
            None => std::fs::remove_file(&paths.config_file).map_err(|e| {
                anyhow::anyhow!(
                    "removing dry-run temp config {}: {e}",
                    paths.config_file.display()
                )
            })?,
        }
        let plans = plans?;

        println!();
        println!("Would set:");
        print_current(&config);
        if plans.is_empty() {
            println!();
            println!("Dry run: no installed service env vars would change. Nothing was written.");
        } else {
            println!();
            render_plans(&plans);
            println!();
            println!(
                "Dry run: nothing written. {} service(s) would be updated and restarted. \
                 Re-run without --dry-run to apply.",
                plans.len()
            );
        }
        return Ok(());
    }

    paths.ensure_dirs()?;
    ryra_core::config::save_config(&paths.config_file, &config)?;
    println!("  Saved to {}", paths.config_file.display());
    if !had_secrets_before && config.has_secrets() {
        println!(
            "  Note: credentials saved to {} (mode 0600 / do not commit or share).",
            paths.config_file.display()
        );
    }

    propagate(flags.yes).await
}

/// Apply `--smtp-*` / `--admin-email` flag edits onto `config`. Returns true
/// if anything changed. SMTP edits overlay onto the existing relay; creating
/// one from scratch needs at least `--smtp-host`.
fn apply_flag_edits(config: &mut Config, flags: &GlobalFlags) -> Result<bool> {
    let mut changed = false;

    if flags.has_smtp_edit() {
        let mut smtp = match &config.smtp {
            Some(s) => s.clone(),
            None => {
                let host = flags.smtp_host.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "no global SMTP relay configured yet - provide at least --smtp-host \
                         (and typically --smtp-from) to create one"
                    )
                })?;
                SmtpCredentials {
                    host,
                    port: 587,
                    username: String::new(),
                    password: String::new(),
                    from: String::new(),
                    security: SmtpSecurity::Starttls,
                }
            }
        };
        if let Some(h) = &flags.smtp_host {
            smtp.host = h.clone();
        }
        if let Some(p) = flags.smtp_port {
            smtp.port = p;
        }
        if let Some(u) = &flags.smtp_username {
            smtp.username = u.clone();
        }
        if let Some(pw) = &flags.smtp_password {
            smtp.password = pw.clone();
        }
        if let Some(f) = &flags.smtp_from {
            smtp.from = f.clone();
        }
        if let Some(sec) = &flags.smtp_security {
            smtp.security = parse_security(sec)?;
        }
        if smtp.from.is_empty() {
            smtp.from = format!("noreply@{}", smtp.host);
        }
        config.smtp = Some(smtp);
        changed = true;
    }

    if let Some(email) = &flags.admin_email
        && config.admin_email.as_deref() != Some(email.as_str())
    {
        config.admin_email = Some(email.clone());
        changed = true;
    }

    Ok(changed)
}

fn parse_security(s: &str) -> Result<SmtpSecurity> {
    match s.to_ascii_lowercase().as_str() {
        "starttls" => Ok(SmtpSecurity::Starttls),
        "force_tls" | "forcetls" | "tls" => Ok(SmtpSecurity::ForceTls),
        "off" | "none" | "plaintext" => Ok(SmtpSecurity::Off),
        other => bail!("invalid --smtp-security '{other}' (expected: starttls, force_tls, off)"),
    }
}

/// Interactive global editor: show current state, pick a setting, edit it.
fn edit_interactive(config: &mut Config) -> Result<bool> {
    println!();
    println!("{}", style("Global configuration").bold());
    print_current(config);
    println!();

    let sel = Select::new()
        .with_prompt("Edit which setting?")
        .items(&["SMTP relay", "Admin email", "Cancel"])
        .default(0)
        .interact()?;

    match sel {
        0 => match super::prompts::prompt_smtp()? {
            super::prompts::SmtpSetupChoice::Custom(smtp) => {
                config.smtp = Some(smtp);
                Ok(true)
            }
            super::prompts::SmtpSetupChoice::Inbucket => {
                config.smtp = Some(SmtpCredentials::inbucket());
                if !ryra_core::is_service_installed("inbucket") {
                    println!(
                        "  {} inbucket is not installed - run `ryra add inbucket` so mail has somewhere to land.",
                        style("note:").yellow()
                    );
                }
                Ok(true)
            }
            super::prompts::SmtpSetupChoice::Skip => Ok(false),
        },
        1 => {
            let current = config.admin_email.clone().unwrap_or_default();
            let email: String = Input::new()
                .with_prompt("Admin email")
                .with_initial_text(current)
                .interact_text()?;
            let email = email.trim();
            if email.is_empty() {
                Ok(false)
            } else {
                config.admin_email = Some(email.to_string());
                Ok(true)
            }
        }
        _ => Ok(false),
    }
}

fn print_current(config: &Config) {
    let smtp = match &config.smtp {
        Some(s) => format!(
            "{}:{} (from {}, {})",
            s.host,
            s.port,
            s.from,
            s.security.as_str()
        ),
        None => "(not configured)".to_string(),
    };
    println!("  SMTP:        {}", style(smtp).cyan());
    println!(
        "  Admin email: {}",
        style(config.admin_email.as_deref().unwrap_or("(not set)")).cyan()
    );
}

/// Reconcile every installed service against the current on-disk global
/// config, keeping only the ones whose env would actually change. A service
/// that fails to reconcile (e.g. an unresolvable registry) is warned about
/// and skipped, not fatal.
async fn collect_plans() -> Result<Vec<ServiceReconcile>> {
    let installed = ryra_core::list_installed()?;
    let mut plans: Vec<ServiceReconcile> = Vec::new();
    for svc in &installed {
        match ryra_core::reconcile_service(&svc.name).await {
            Ok(r) if !r.changes.is_empty() => plans.push(r),
            Ok(_) => {}
            Err(e) => eprintln!("  {} {}: {e}", style("warning:").yellow(), svc.name),
        }
    }
    Ok(plans)
}

/// Print the per-service env diff.
fn render_plans(plans: &[ServiceReconcile]) {
    for p in plans {
        println!("{}", style(&p.service).bold());
        for c in &p.changes {
            print_change(c);
        }
    }
}

/// Re-render every installed service against the just-saved global config,
/// show the per-service env diff, and apply to the ones the user selects.
async fn propagate(yes: bool) -> Result<()> {
    println!();
    println!("Checking installed services for affected env vars…");
    let plans = collect_plans().await?;

    if plans.is_empty() {
        println!("All services already match the global config - nothing to update.");
        return Ok(());
    }

    println!();
    render_plans(&plans);
    println!();

    let selected: Vec<&ServiceReconcile> = if yes {
        plans.iter().collect()
    } else if super::is_interactive() {
        let labels: Vec<String> = plans
            .iter()
            .map(|p| {
                let n = p.changes.len();
                format!(
                    "{} ({n} env var{})",
                    p.service,
                    if n == 1 { "" } else { "s" }
                )
            })
            .collect();
        let defaults = vec![true; plans.len()];
        let chosen = MultiSelect::new()
            .with_prompt("Update which services? (space toggles, enter confirms)")
            .items(&labels)
            .defaults(&defaults)
            .interact()?;
        chosen.into_iter().filter_map(|i| plans.get(i)).collect()
    } else {
        bail!(
            "non-interactive run without --yes; re-run with --yes to apply to all {} affected \
             service(s), or --dry-run to preview",
            plans.len()
        );
    };

    if selected.is_empty() {
        println!("Nothing selected - no services updated.");
        return Ok(());
    }

    for p in &selected {
        println!();
        println!("Updating {} (restart)…", style(&p.service).bold());
        apply::execute_all(&p.steps).await?;
    }
    println!();
    println!("Done. Updated {} service(s).", selected.len());
    Ok(())
}

fn print_change(c: &EnvKeyChange) {
    let show = |v: &str| {
        if c.secret {
            "••••••".to_string()
        } else {
            v.to_string()
        }
    };
    match &c.from {
        Some(old) => println!(
            "  {} {}: {} {} {}",
            style("~").yellow(),
            c.key,
            style(show(old)).dim(),
            style("→").dim(),
            style(show(&c.to)).cyan()
        ),
        None => println!(
            "  {} {}: {} (was: unset)",
            style("+").green().bold(),
            c.key,
            style(show(&c.to)).cyan()
        ),
    }
}
