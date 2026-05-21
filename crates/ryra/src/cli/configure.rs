//! `ryra configure <service>` — re-configure an installed service. The
//! render itself happens in `ryra-core::configure_service`; this module
//! owns the user-facing flow: collect overrides (from flags or
//! interactive prompts), print the typed change summary + file diff,
//! gate destructive transitions, apply.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use console::style;
use dialoguer::{Confirm, Input};

use ryra_core::{
    ConfigureChange, ConfigureOverrides, ConfigureResult, DiffKind, ExposureChange,
    configure_service, is_service_installed, load_metadata,
};

use super::apply;

/// Flag bundle assembled in main.rs and passed in. Mirrors the clap
/// flags 1:1 so the dispatcher doesn't need to translate.
#[derive(Debug, Default, Clone)]
pub struct ConfigureFlags {
    pub url: Option<String>,
    pub no_url: bool,
    pub tailscale: bool,
    pub smtp: bool,
    pub no_smtp: bool,
    pub backup: bool,
    pub no_backup: bool,
    pub auth: bool,
    pub no_auth: bool,
    pub enable: Vec<String>,
    pub disable: Vec<String>,
    pub set: Vec<String>,
    pub yes: bool,
    pub dry_run: bool,
}

pub async fn run(service: &str, flags: ConfigureFlags) -> Result<()> {
    if !is_service_installed(service) {
        bail!("service '{service}' is not installed");
    }

    // Mutual-exclusion checks the clap-level attribute can't easily
    // express across multiple flag pairs.
    if flags.url.is_some() && flags.no_url {
        bail!("--url and --no-url are mutually exclusive");
    }
    if flags.url.is_some() && flags.tailscale {
        bail!("--url and --tailscale are mutually exclusive");
    }
    if flags.no_url && flags.tailscale {
        bail!("--no-url and --tailscale are mutually exclusive");
    }
    if flags.smtp && flags.no_smtp {
        bail!("--smtp and --no-smtp are mutually exclusive");
    }
    if flags.backup && flags.no_backup {
        bail!("--backup and --no-backup are mutually exclusive");
    }
    if flags.auth && flags.no_auth {
        bail!("--auth and --no-auth are mutually exclusive");
    }

    let has_any_flag = flags.url.is_some()
        || flags.no_url
        || flags.tailscale
        || flags.smtp
        || flags.no_smtp
        || flags.backup
        || flags.no_backup
        || flags.auth
        || flags.no_auth
        || !flags.enable.is_empty()
        || !flags.disable.is_empty()
        || !flags.set.is_empty();

    let overrides = if has_any_flag {
        build_overrides_from_flags(service, &flags)?
    } else if super::is_interactive() {
        build_overrides_interactive(service).await?
    } else {
        // No flags, no TTY — print the current state and exit so a
        // script that calls `ryra configure foo` accidentally doesn't
        // hang or apply phantom changes. The user gets a usable
        // overview either way.
        print_current_state(service).await?;
        println!();
        println!(
            "No changes specified. Pass flags (--smtp, --backup, --enable <group>, --set KEY=VAL …) to reconfigure."
        );
        return Ok(());
    };

    let result = configure_service(service, &overrides).await?;

    // Tailscale lifecycle steps (Enable on entering, Disable on
    // leaving) need the admin token in preferences.toml. Prompt now if
    // the plan contains any — without this we'd write the new
    // quadlet + .env and then crash mid-apply.
    super::add::ensure_tailscale_token_for_steps(&result.steps, super::is_interactive()).await?;

    if result.is_noop() {
        println!("No changes — service '{service}' is already configured that way.");
        return Ok(());
    }

    print_summary(&result);

    if flags.dry_run {
        println!("Dry run — no changes made. Remove --dry-run to apply.\n");
        return Ok(());
    }

    if !flags.yes {
        if !super::is_interactive() {
            bail!(
                "non-interactive run without --yes — re-run with `ryra configure {service} --yes` (or --dry-run to preview)"
            );
        }
        if result.has_destructive {
            // Destructive transitions get a typed confirmation: the user
            // has to type the service name to proceed. The simple Y/N
            // prompt is too easy to fly through on autopilot when the
            // wrong service is in scope.
            let typed: String = Input::<String>::new()
                .with_prompt(format!(
                    "Destructive changes detected. Type {} to confirm",
                    style(service).bold()
                ))
                .interact_text()?;
            if typed.trim() != service {
                println!("Cancelled (input did not match service name).");
                return Ok(());
            }
        } else {
            let proceed = Confirm::new()
                .with_prompt("Apply configuration changes?")
                .default(true)
                .interact()?;
            if !proceed {
                println!("Cancelled.");
                return Ok(());
            }
        }
    }

    println!();
    println!("Configuring {}…", style(service).bold());
    apply::execute_all(&result.steps).await?;
    println!();
    println!("Done.");
    Ok(())
}

fn build_overrides_from_flags(service: &str, flags: &ConfigureFlags) -> Result<ConfigureOverrides> {
    let mut overrides = ConfigureOverrides::default();
    if let Some(url) = &flags.url {
        overrides.exposure = Some(ExposureChange::Url(url.clone()));
    } else if flags.no_url {
        overrides.exposure = Some(ExposureChange::Loopback);
    } else if flags.tailscale {
        // Derive the tailscale Service URL the same way `ryra add
        // --tailscale` does, so the configure transition lands at the
        // same hostname the install path would have produced.
        overrides.exposure = Some(ExposureChange::Tailscale(derive_tailscale_url(service)?));
    }
    if flags.smtp {
        overrides.smtp = Some(true);
    } else if flags.no_smtp {
        overrides.smtp = Some(false);
    }
    if flags.backup {
        overrides.backup = Some(true);
    } else if flags.no_backup {
        overrides.backup = Some(false);
    }
    if flags.auth {
        overrides.auth = Some(true);
    } else if flags.no_auth {
        overrides.auth = Some(false);
    }
    overrides.enable_groups = flags.enable.iter().cloned().collect();
    overrides.disable_groups = flags.disable.iter().cloned().collect();
    for kv in &flags.set {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--set must be KEY=VALUE, got: {kv}"))?;
        let key = k.trim().to_string();
        if key.is_empty() {
            bail!("--set KEY is empty in: {kv}");
        }
        overrides.env_overrides.insert(key, v.to_string());
    }
    Ok(overrides)
}

/// Build the Tailscale Service URL for `service` from the local
/// tailnet identity. Mirrors `ryra add --tailscale`'s derivation so
/// `configure --tailscale` and `add --tailscale` produce the same
/// hostname for the same (service, host) pair.
fn derive_tailscale_url(service: &str) -> Result<String> {
    let node = ryra_core::system::tailscale::self_dns_name().ok_or_else(|| {
        anyhow::anyhow!("--tailscale: no logged-in tailnet — run `tailscale up` first")
    })?;
    let host = ryra_core::system::tailscale::self_short_hostname().ok_or_else(|| {
        anyhow::anyhow!(
            "--tailscale: couldn't extract host label from MagicDNS name '{node}' \
             (expected `<host>.<tailnet>.ts.net`)"
        )
    })?;
    let tailnet = ryra_core::system::tailscale::tailnet_suffix(&node).ok_or_else(|| {
        anyhow::anyhow!(
            "--tailscale: couldn't extract tailnet from MagicDNS name '{node}' \
             (expected `<host>.<tailnet>.ts.net`)"
        )
    })?;
    Ok(format!("https://{service}-{host}.{tailnet}"))
}

/// Interactive prompt walk-through. Loads the current state and offers
/// to flip each toggleable knob. Returns the assembled overrides.
async fn build_overrides_interactive(service: &str) -> Result<ConfigureOverrides> {
    let metadata = load_metadata(service)?.ok_or_else(|| {
        anyhow::anyhow!("metadata.toml missing for installed service '{service}'")
    })?;
    let mut overrides = ConfigureOverrides::default();
    let reg_groups = load_registry_group_names(service, &metadata.registry)
        .await
        .unwrap_or_default();

    println!();
    println!("Current configuration for {}:", style(service).bold());
    print_status_block(&metadata, &reg_groups);
    println!();

    // Exposure: offer the four target states.
    let exposure_choice = dialoguer::Select::new()
        .with_prompt("Change exposure?")
        .items(&[
            "Keep current",
            "Set to new URL",
            "Switch to Tailscale",
            "Remove (loopback only)",
        ])
        .default(0)
        .interact()?;
    match exposure_choice {
        1 => {
            let new_url: String = Input::<String>::new()
                .with_prompt("New URL (e.g. https://docs.example.com)")
                .interact_text()?;
            if !new_url.trim().is_empty() {
                overrides.exposure = Some(ExposureChange::Url(new_url.trim().to_string()));
            }
        }
        2 => {
            overrides.exposure = Some(ExposureChange::Tailscale(derive_tailscale_url(service)?));
        }
        3 => {
            overrides.exposure = Some(ExposureChange::Loopback);
        }
        _ => {}
    }

    // Auth toggle.
    let auth_on = metadata.auth.is_some();
    let new_auth = Confirm::new()
        .with_prompt(format!(
            "Enable OIDC SSO for this service? (currently {})",
            if auth_on { "on" } else { "off" }
        ))
        .default(auth_on)
        .interact()?;
    if new_auth != auth_on {
        overrides.auth = Some(new_auth);
    }

    // SMTP toggle.
    let new_smtp = Confirm::new()
        .with_prompt(format!(
            "Enable SMTP for this service? (currently {})",
            if metadata.smtp_enabled { "on" } else { "off" }
        ))
        .default(metadata.smtp_enabled)
        .interact()?;
    if new_smtp != metadata.smtp_enabled {
        overrides.smtp = Some(new_smtp);
    }

    // Backup toggle.
    let new_backup = Confirm::new()
        .with_prompt(format!(
            "Include this service in encrypted backups? (currently {})",
            if metadata.backup_enabled { "on" } else { "off" }
        ))
        .default(metadata.backup_enabled)
        .interact()?;
    if new_backup != metadata.backup_enabled {
        overrides.backup = Some(new_backup);
    }

    // Env groups — reuse the registry-known list loaded above so we
    // don't resolve the registry twice.
    let current_groups: BTreeSet<String> = metadata.enabled_groups.iter().cloned().collect();
    for (group_name, group_prompt) in reg_groups {
        let is_on = current_groups.contains(&group_name);
        let label = if group_prompt.is_empty() {
            format!("Enable env_group '{group_name}'?")
        } else {
            format!(
                "{group_prompt} (currently {})",
                if is_on { "on" } else { "off" }
            )
        };
        let new_state = Confirm::new()
            .with_prompt(label)
            .default(is_on)
            .interact()?;
        if new_state && !is_on {
            overrides.enable_groups.insert(group_name);
        } else if !new_state && is_on {
            overrides.disable_groups.insert(group_name);
        }
    }

    Ok(overrides)
}

/// Load a service's `[[env_group]]` names + prompts from its registry.
/// Returns `(group_name, group_prompt)` pairs. Async because the
/// registry resolver is — we're already inside `#[tokio::main]`, so
/// awaiting is fine and (importantly) we must not spin a nested runtime
/// here.
async fn load_registry_group_names(
    service: &str,
    registry: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    use ryra_core::registry::resolve::ServiceRef;
    let service_ref = if registry.is_empty() || registry == ryra_core::REGISTRY_BUNDLED {
        ServiceRef::Bundled(service.to_string())
    } else {
        ServiceRef::Custom {
            registry: registry.to_string(),
            service: service.to_string(),
        }
    };
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref).await?;
    let reg_service = ryra_core::registry::find_service(&repo_dir, service)?;
    Ok(reg_service
        .def
        .env_groups
        .iter()
        .map(|g| (g.name.clone(), g.prompt.clone()))
        .collect())
}

async fn print_current_state(service: &str) -> Result<()> {
    let meta = load_metadata(service)?.ok_or_else(|| {
        anyhow::anyhow!("metadata.toml missing for installed service '{service}'")
    })?;
    let reg_groups = load_registry_group_names(service, &meta.registry)
        .await
        .unwrap_or_default();
    println!("{}", style(service).bold());
    print_status_block(&meta, &reg_groups);
    Ok(())
}

/// Print the `url / auth / smtp / backup / groups` block. The `groups`
/// line is suppressed entirely when the registry defines no
/// `[[env_group]]` blocks — saying `(none enabled)` for a service that
/// has nothing to enable is just noise.
fn print_status_block(meta: &ryra_core::Metadata, reg_groups: &[(String, String)]) {
    if let Some(url) = &meta.url {
        println!("  url:    {}", style(url).cyan());
    } else {
        println!("  url:    {}", style("(none)").dim());
    }
    println!(
        "  auth:   {}",
        if meta.auth.is_some() {
            style("on").green().to_string()
        } else {
            style("off").red().to_string()
        }
    );
    println!(
        "  smtp:   {}",
        if meta.smtp_enabled {
            style("on").green().to_string()
        } else {
            style("off").red().to_string()
        }
    );
    println!(
        "  backup: {}",
        if meta.backup_enabled {
            style("on").green().to_string()
        } else {
            style("off").red().to_string()
        }
    );
    if !reg_groups.is_empty() {
        if meta.enabled_groups.is_empty() {
            println!("  groups: {}", style("(none enabled)").dim());
        } else {
            println!("  groups: {}", meta.enabled_groups.join(", "));
        }
    }
}

fn print_summary(result: &ConfigureResult) {
    println!();
    println!("{}", style(&result.service).bold());

    // High-level changes — colour-coded by destructive / additive.
    for change in &result.changes {
        print_change_line(change);
    }

    // File-level diff — same shape as `ryra upgrade` so the visual
    // language stays consistent. `service.manifest` is included in
    // `entries` for planning correctness but is internal bookkeeping
    // (sha256s of every other file), so we filter it out at display
    // time — surfacing it would just be noise.
    let manifest_file = ryra_core::manifest_path(&result.service).ok();
    let is_display = |entry: &ryra_core::DiffEntry| {
        !matches!(entry.kind, DiffKind::Unchanged) && Some(&entry.path) != manifest_file.as_ref()
    };
    let any_file_change =
        result.diff.entries.iter().any(is_display) || !result.diff.env_additions.is_empty();
    if any_file_change {
        println!();
        println!("  {}", style("Files:").dim());
        for entry in result.diff.entries.iter().filter(|e| is_display(e)) {
            match entry.kind {
                DiffKind::Unchanged => {}
                DiffKind::Added => println!(
                    "    {} {}  {}",
                    style("+").green().bold(),
                    entry.path.display(),
                    style("added").green()
                ),
                DiffKind::Modified => println!(
                    "    {} {}  {}",
                    style("~").yellow(),
                    entry.path.display(),
                    style("modified").yellow()
                ),
                DiffKind::Removed => println!(
                    "    {} {}  {}",
                    style("-").red(),
                    entry.path.display(),
                    style("removed").red()
                ),
                DiffKind::Drift => println!(
                    "    {} {}  {}",
                    style("!").red().bold(),
                    entry.path.display(),
                    style("drift (overwriting)").red().bold()
                ),
            }
        }
        for add in &result.diff.env_additions {
            println!(
                "    {} env: {}={}  {}",
                style("+").green().bold(),
                add.key,
                add.value,
                style("appended to .env").green()
            );
        }
    }

    let will_restart = result
        .steps
        .iter()
        .any(|s| matches!(s, ryra_core::Step::RestartService { .. }));
    if will_restart {
        println!();
        println!(
            "  {} systemctl --user daemon-reload + restart {} (brief downtime)",
            style("→").cyan(),
            result.service
        );
    }
    if result.has_destructive {
        println!(
            "  {} {}",
            style("!").red().bold(),
            style("Destructive changes — typed confirmation required").red()
        );
    }
    println!();
}

fn print_change_line(change: &ConfigureChange) {
    match change {
        ConfigureChange::Url { from, to } => match (from.as_deref(), to.as_deref()) {
            (None, Some(new)) => println!(
                "  {} url: {} (was: none)",
                style("+").green().bold(),
                style(new).cyan()
            ),
            (Some(old), None) => println!(
                "  {} url: removed (was: {})",
                style("-").red().bold(),
                style(old).dim()
            ),
            (Some(old), Some(new)) => println!(
                "  {} url: {} {} {}",
                style("~").yellow(),
                style(old).dim(),
                style("→").dim(),
                style(new).cyan()
            ),
            (None, None) => {}
        },
        ConfigureChange::Smtp { from, to } => println!(
            "  {} smtp: {} {} {}",
            toggle_arrow(*from, *to),
            on_off(*from),
            style("→").dim(),
            on_off(*to)
        ),
        ConfigureChange::Backup { from, to } => println!(
            "  {} backup: {} {} {}",
            toggle_arrow(*from, *to),
            on_off(*from),
            style("→").dim(),
            on_off(*to)
        ),
        ConfigureChange::Auth { from, to } => println!(
            "  {} auth:   {} {} {}",
            toggle_arrow(*from, *to),
            on_off(*from),
            style("→").dim(),
            on_off(*to)
        ),
        ConfigureChange::GroupEnabled(g) => println!(
            "  {} env_group: {} {}",
            style("+").green().bold(),
            style(g).cyan(),
            style("enabled").green()
        ),
        ConfigureChange::GroupDisabled(g) => println!(
            "  {} env_group: {} {}",
            style("-").red().bold(),
            style(g).dim(),
            style("disabled").red()
        ),
        ConfigureChange::EnvOverride { key, from, to } => {
            let from_display = from.as_deref().unwrap_or("(unset)");
            println!(
                "  {} env: {}: {} {} {}",
                style("~").yellow(),
                style(key).cyan(),
                style(from_display).dim(),
                style("→").dim(),
                style(to).cyan()
            );
        }
    }
}

fn on_off(value: bool) -> console::StyledObject<&'static str> {
    if value {
        style("on").green()
    } else {
        style("off").red()
    }
}

fn toggle_arrow(from: bool, to: bool) -> console::StyledObject<&'static str> {
    match (from, to) {
        (false, true) => style("+").green().bold(),
        (true, false) => style("-").red().bold(),
        _ => style("~").yellow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--set KEY=VAL` is the only field in `ConfigureFlags` with real
    /// parsing — the other flags are bool/Option<String> mappings clap
    /// already validates. The tricky cases: missing `=`, empty key,
    /// and `=` appearing inside the value (base64 padding).
    #[test]
    fn set_flag_parsing() {
        let ok = build_overrides_from_flags(
            "test",
            &ConfigureFlags {
                set: vec![
                    "ADMIN_EMAIL=admin@example.com".into(),
                    "OAUTH_KEY=abc==".into(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            ok.env_overrides.get("ADMIN_EMAIL").map(String::as_str),
            Some("admin@example.com")
        );
        assert_eq!(
            ok.env_overrides.get("OAUTH_KEY").map(String::as_str),
            Some("abc=="),
            "value should keep trailing '=' characters"
        );

        let no_equals = build_overrides_from_flags(
            "test",
            &ConfigureFlags {
                set: vec!["NO_EQUALS".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(no_equals.to_string().contains("KEY=VALUE"));

        let empty_key = build_overrides_from_flags(
            "test",
            &ConfigureFlags {
                set: vec!["=value".into()],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(empty_key.to_string().contains("KEY is empty"));
    }

    #[allow(dead_code)]
    fn _btreeset_used_in_signatures(_: BTreeSet<String>) {}
}
