//! `ryra status` — global state observation. Reads config + metadata
//! files, probes `systemctl --user` and `loginctl` for live service /
//! linger state. No health judgement — that's what `ryra doctor` is
//! for. The principle: every line is a fact, not an opinion.

use std::collections::HashSet;

use anyhow::Result;
use ryra_core::config::status::{
    BackupSummary, ProviderStatus, RyraStatus, StatusInfo, TailscaleSummary,
};

pub async fn run() -> Result<()> {
    match ryra_core::status() {
        RyraStatus::NotInitialized => {
            println!("ryra is not configured yet. Run `ryra add <service>` to get started.");
        }
        RyraStatus::Error(msg) => {
            eprintln!("{} {msg}", super::style::error_prefix("Error:"));
        }
        RyraStatus::Initialized(info) => print_overview(&info),
    }
    Ok(())
}

fn print_overview(info: &StatusInfo) {
    println!("Config:     {}", info.config_path.display());
    println!();
    println!("SMTP:       {}", format_provider(&info.smtp));
    println!("Auth:       {}", format_provider(&info.auth));
    println!("Backup:     {}", format_backup(info.backup.as_ref()));
    println!("Linger:     {}", format_linger(linger_enabled()));
    if let Some(ts) = &info.tailscale {
        println!("Tailscale:  {}", format_tailscale(ts));
    }
    println!();

    if info.services.is_empty() {
        println!("Services:   none installed — run `ryra add <service>` to install one");
    } else {
        let active = active_user_units();
        let breakdown = breakdown(&info.services, &active);
        println!(
            "Services:   {} installed — {}",
            info.services.len(),
            breakdown
        );
        println!("            run `ryra list` to list them");
    }
}

fn format_provider(status: &ProviderStatus) -> &str {
    match status {
        ProviderStatus::None => "not configured",
        ProviderStatus::Configured { name } => name,
    }
}

fn format_backup(b: Option<&BackupSummary>) -> String {
    match b {
        None => "not configured".to_string(),
        Some(s) => format!(
            "{} — {} service{} included",
            s.backend_label,
            s.included,
            plural(s.included)
        ),
    }
}

fn format_linger(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

fn format_tailscale(ts: &TailscaleSummary) -> String {
    if ts.advertised == 0 {
        "configured (no services advertised)".to_string()
    } else {
        format!(
            "{} service{} advertised",
            ts.advertised,
            plural(ts.advertised)
        )
    }
}

/// "10 running, 1 stopped, 1 failed (vikunja)". The failed-service
/// callout is the most actionable thing in the whole status output —
/// surface it by name so the next command is obvious.
fn breakdown(
    services: &[ryra_core::config::status::ServiceInfo],
    active: &HashSet<String>,
) -> String {
    let mut running = 0usize;
    let mut stopped = 0usize;
    let mut failed_names: Vec<&str> = Vec::new();
    let failed_set = failed_user_units();
    for svc in services {
        if !svc.installed {
            continue;
        }
        if active.contains(&svc.name) {
            running += 1;
        } else if failed_set.contains(&svc.name) {
            failed_names.push(svc.name.as_str());
        } else {
            stopped += 1;
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if stopped > 0 {
        parts.push(format!("{stopped} stopped"));
    }
    if !failed_names.is_empty() {
        parts.push(format!(
            "{} failed ({})",
            failed_names.len(),
            failed_names.join(", ")
        ));
    }
    if parts.is_empty() {
        "all stopped".to_string()
    } else {
        parts.join(", ")
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Best-effort linger probe via `loginctl`. Returns false on any error —
/// safer to under-report than to lie about a footgun the other way.
fn linger_enabled() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        return false;
    }
    let out = std::process::Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "yes",
        _ => false,
    }
}

/// Active user units — one `systemctl list-units` call, parsed for
/// service names. Cheaper than N `is-active` probes and matches what
/// `ryra list` already does.
fn active_user_units() -> HashSet<String> {
    units_in_state("active")
}

fn failed_user_units() -> HashSet<String> {
    units_in_state("failed")
}

fn units_in_state(state: &str) -> HashSet<String> {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            "--type=service",
            &format!("--state={state}"),
            "--no-legend",
            "--plain",
            "--no-pager",
        ])
        .output();
    let Ok(out) = out else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|unit| unit.strip_suffix(".service"))
        .map(|s| s.to_string())
        .collect()
}
