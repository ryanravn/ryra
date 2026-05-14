//! Optional one-time sudo prompt that lowers
//! `net.ipv4.ip_unprivileged_port_start` to 80 so rootless Caddy can bind
//! ports 80 and 443 directly. Without this, ryra has to expose Caddy on
//! 8080/8443 and ask the user to NAT 80→8080 / 443→8443 at the router —
//! works, but every URL ends up with a `:8443` until they do, and the
//! mental overhead on first setup is real.
//!
//! Mirrors the `linger.rs` pattern: detect → describe what'll run →
//! confirm → execute sudo directly. Persistence is done by writing
//! `/etc/sysctl.d/50-ryra.conf` (via `sudo tee`) so the change survives
//! reboot. If anything fails or the user declines we just continue with
//! the high-port fallback — it's an optional ergonomics tweak, never a
//! blocker.

use std::io::Write;
use std::process::Stdio;

use anyhow::Result;
use dialoguer::Confirm;
use tokio::process::Command;

const SYSCTL_FILE: &str = "/etc/sysctl.d/50-ryra.conf";
const SYSCTL_KEY: &str = "net.ipv4.ip_unprivileged_port_start";
const SYSCTL_VALUE: &str = "80";

/// Offer to enable rootless low-port binding when it isn't already on.
/// No-op when the kernel is already configured, when running
/// non-interactively, or when the user declines. Soft-failures: any
/// sudo error prints a warning and returns Ok — the caller continues
/// with the default high-port mapping.
pub async fn offer_enable() -> Result<()> {
    if ryra_core::system::sysctl::rootless_can_bind_low_ports() {
        return Ok(());
    }

    println!();
    println!(
        "  Caddy will be exposed on host ports 8080/8443 by default — rootless\n  \
         podman can't bind <1024. To let Caddy listen on 80/443 directly\n  \
         (cleaner URLs, simpler router forwarding), ryra can run:"
    );
    println!();
    println!("    sudo sysctl -w {SYSCTL_KEY}={SYSCTL_VALUE}");
    println!("    (persisted in {SYSCTL_FILE})");
    println!();

    if !super::is_interactive() {
        eprintln!(
            "  (non-interactive; using 8080/8443 — run the sysctl command above when convenient)"
        );
        return Ok(());
    }

    let proceed = match Confirm::new()
        .with_prompt("  Run this now? (one-time sudo, persists across reboots)")
        .default(true)
        .interact()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Warning: could not read confirmation ({e}); using 8080/8443");
            return Ok(());
        }
    };
    if !proceed {
        return Ok(());
    }

    // Apply immediately.
    match Command::new("sudo")
        .args(["sysctl", "-w", &format!("{SYSCTL_KEY}={SYSCTL_VALUE}")])
        .status()
        .await
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("  sudo sysctl exited with {s}; falling back to 8080/8443");
            return Ok(());
        }
        Err(e) => {
            eprintln!("  Failed to run sudo sysctl: {e}; falling back to 8080/8443");
            return Ok(());
        }
    }

    // Persist for reboot. `sudo tee` writes the file with root permissions
    // and we discard its stdout (it echoes the input by default).
    let conf = format!(
        "# Written by ryra so rootless Caddy can bind 80/443.\n{SYSCTL_KEY} = {SYSCTL_VALUE}\n"
    );
    let mut child = match std::process::Command::new("sudo")
        .args(["tee", SYSCTL_FILE])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "  Note: applied for this session, but couldn't write {SYSCTL_FILE} ({e}).\n  \
                 The setting will revert on reboot — re-run if needed."
            );
            return Ok(());
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(conf.as_bytes());
    }
    match child.wait() {
        Ok(s) if s.success() => println!("  Privileged port binding enabled."),
        Ok(s) => eprintln!(
            "  Note: applied for this session, but `sudo tee` exited {s}; \
             setting will revert on reboot."
        ),
        Err(e) => eprintln!(
            "  Note: applied for this session, but persistence failed ({e}); \
             setting will revert on reboot."
        ),
    }
    Ok(())
}

/// Symmetric to [`offer_enable`]: when ryra previously wrote
/// `/etc/sysctl.d/50-ryra.conf` to lower the unprivileged port floor,
/// ask on `ryra reset` whether to also undo that. Removing the file
/// and reloading sysctl restores the kernel default (1024). Skips
/// silently when the file doesn't exist (we never wrote it, or the
/// user already removed it). For non-interactive resets we leave a
/// note instead of touching the system without consent — matches the
/// `linger::note_if_enabled` pattern for shared-state knobs ryra didn't
/// originally own.
pub async fn offer_disable() -> Result<()> {
    use std::path::Path;

    if !Path::new(SYSCTL_FILE).exists() {
        return Ok(());
    }

    if !super::is_interactive() {
        println!(
            "  Note: ryra previously persisted {SYSCTL_KEY}={SYSCTL_VALUE} in {SYSCTL_FILE}.\n  \
             Run `sudo rm {SYSCTL_FILE} && sudo sysctl --system` to revert."
        );
        return Ok(());
    }

    println!();
    println!(
        "  ryra previously persisted {SYSCTL_KEY}={SYSCTL_VALUE} in {SYSCTL_FILE} so\n  \
         rootless Caddy could bind 80/443. Reverting will restore the kernel default (1024)."
    );

    let revert = match Confirm::new()
        .with_prompt("  Revert that change too? (sudo)")
        .default(false)
        .interact()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Warning: could not read confirmation ({e}); leaving sysctl alone");
            return Ok(());
        }
    };
    if !revert {
        return Ok(());
    }

    match Command::new("sudo")
        .args(["rm", SYSCTL_FILE])
        .status()
        .await
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("  sudo rm {SYSCTL_FILE} exited with {s}; leaving sysctl alone");
            return Ok(());
        }
        Err(e) => {
            eprintln!("  Failed to run sudo rm: {e}; leaving sysctl alone");
            return Ok(());
        }
    }
    // `sysctl --system` re-applies all of /etc/sysctl.d/, /usr/lib/sysctl.d/,
    // /run/sysctl.d/, etc. With our file gone, the kernel default takes over.
    match Command::new("sudo")
        .args(["sysctl", "--system"])
        .status()
        .await
    {
        Ok(s) if s.success() => println!("  Reverted privileged port binding."),
        Ok(s) => eprintln!(
            "  Note: removed {SYSCTL_FILE}, but `sudo sysctl --system` exited {s}; \
             reboot to fully revert."
        ),
        Err(e) => eprintln!(
            "  Note: removed {SYSCTL_FILE}, but couldn't reload sysctl ({e}); \
             reboot to fully revert."
        ),
    }
    Ok(())
}
