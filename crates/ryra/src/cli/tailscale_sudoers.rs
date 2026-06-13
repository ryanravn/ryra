//! One-time sudoers drop-in so `sudo -n tailscale` works without a
//! password. Required for ryra-api (no TTY to prompt) and convenient
//! for interactive installs (no repeated sudo prompts per port).
//!
//! Writes `/etc/sudoers.d/ryra-tailscale` via `sudo tee`, following
//! the same detect-describe-confirm-execute pattern as `linger.rs` and
//! `sysctl_low_ports.rs`.

use std::io::Write;
use std::process::Stdio;

use anyhow::Result;
use dialoguer::Confirm;

const SUDOERS_FILE: &str = "/etc/sudoers.d/ryra-tailscale";

/// True when the current user can already `sudo -n tailscale status`
/// without a password (either via our drop-in or any other sudoers rule).
fn passwordless_tailscale() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "tailscale", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Offer to install the sudoers drop-in. No-op when passwordless sudo
/// already works, when running non-interactively, or when the user
/// declines.
pub async fn offer_enable() -> Result<()> {
    if passwordless_tailscale() {
        return Ok(());
    }

    let user = std::env::var("USER").unwrap_or_else(|_| "<your-user>".into());

    println!();
    println!(
        "  Tailscale commands need sudo. To avoid repeated password prompts\n  \
         (and to let ryra-api work without a TTY), ryra can write:"
    );
    println!();
    println!("    {SUDOERS_FILE}");
    println!("    {user} ALL=(root) NOPASSWD: /usr/bin/tailscale");
    println!();

    if !super::is_interactive() {
        eprintln!("  (non-interactive; tailscale commands will prompt for sudo each time)");
        return Ok(());
    }

    let proceed = match Confirm::new()
        .with_prompt("  Install this sudoers rule? (one-time sudo)")
        .default(true)
        .interact()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Warning: could not read confirmation ({e}); skipping");
            return Ok(());
        }
    };
    if !proceed {
        return Ok(());
    }

    let tailscale_bin = which_tailscale().unwrap_or_else(|| "/usr/bin/tailscale".into());
    let content = format!(
        "# Installed by ryra so tailscale serve/status work without a password.\n\
         {user} ALL=(root) NOPASSWD: {tailscale_bin}\n"
    );

    let mut child = match std::process::Command::new("sudo")
        .args(["tee", SUDOERS_FILE])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  Failed to run sudo tee: {e}");
            return Ok(());
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
    }
    match child.wait() {
        Ok(s) if s.success() => {
            // sudoers files must be 0440, owned by root. `sudo tee`
            // creates them 0644; fix the mode.
            let _ = std::process::Command::new("sudo")
                .args(["chmod", "0440", SUDOERS_FILE])
                .status();
            println!("  Passwordless tailscale enabled.");
        }
        Ok(s) => eprintln!("  sudo tee exited with {s}; tailscale will prompt for sudo each time"),
        Err(e) => eprintln!("  Failed waiting for sudo tee: {e}"),
    }

    Ok(())
}

/// Remove the sudoers drop-in on `ryra reset`. Best-effort.
pub fn remove() {
    if !std::path::Path::new(SUDOERS_FILE).exists() {
        return;
    }
    let _ = std::process::Command::new("sudo")
        .args(["-n", "rm", "-f", SUDOERS_FILE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn which_tailscale() -> Option<String> {
    let output = std::process::Command::new("which")
        .arg("tailscale")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}
