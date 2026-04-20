//! Detect whether systemd-logind lingering is enabled for the current user.
//!
//! Rootless podman services managed via quadlets run inside a user's
//! `systemd --user` manager. That manager exits when the user logs out,
//! taking every service with it — unless `loginctl enable-linger <user>`
//! has been run, which pins the user manager at boot regardless of login
//! state. Without lingering, `ryra` on a server looks like it's working
//! until the user's SSH session closes, then silently everything stops.

use anyhow::Result;
use dialoguer::Confirm;
use tokio::process::Command;

/// Returns `true` if the current user has lingering enabled. Returns `false`
/// if not, or if the check can't be run (e.g. no systemd, not Linux). We
/// fail "not enabled" rather than erroring so the warning fires in all the
/// places where it should.
pub async fn is_enabled() -> bool {
    let Ok(user) = std::env::var("USER") else {
        return false;
    };
    let output = Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger", "--value"])
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == "yes",
        _ => false,
    }
}

/// Print a warning (with the fix command) when lingering is off. Services
/// added by `ryra` won't survive a logout/reboot without this — use this
/// from call sites where prompting would be too noisy (e.g. every `ryra add`).
pub async fn warn_if_disabled() -> Result<()> {
    if is_enabled().await {
        return Ok(());
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "<your-user>".into());
    eprintln!();
    eprintln!(
        "  ! systemd lingering is NOT enabled for {user}. Services added by ryra\n    \
           will stop when you log out. To keep them running across logouts and reboots:\n\n    \
           sudo loginctl enable-linger {user}"
    );
    eprintln!();
    Ok(())
}

/// Offer to run `sudo loginctl enable-linger <user>` interactively. Mirrors
/// the cert-setup prompt in `cli/add.rs`: print exactly what will run,
/// confirm, then execute sudo directly (no shell). Falls back to the
/// warning when stdin isn't a TTY.
pub async fn offer_enable() -> Result<()> {
    if is_enabled().await {
        return Ok(());
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "<your-user>".into());

    println!();
    println!("  systemd lingering is not enabled for {user}. Without it, services");
    println!("  added by ryra stop when you log out. To enable it:");
    println!();
    println!("    sudo loginctl enable-linger {user}");
    println!();

    if !super::is_interactive() {
        eprintln!("  (non-interactive; run the command above when convenient)");
        return Ok(());
    }

    let run = match Confirm::new()
        .with_prompt("  Run this now?")
        .default(true)
        .interact()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Warning: could not read confirmation ({e}); skipping");
            return Ok(());
        }
    };
    if !run {
        return Ok(());
    }

    match Command::new("sudo")
        .args(["loginctl", "enable-linger", &user])
        .status()
        .await
    {
        Ok(s) if s.success() => println!("  Lingering enabled."),
        Ok(s) => eprintln!("  sudo loginctl enable-linger exited with {s}"),
        Err(e) => eprintln!("  Failed to run sudo: {e}"),
    }

    Ok(())
}

/// On `ryra reset`, leave a one-line note if lingering is still on. Ryra
/// doesn't own the linger bit — it may predate ryra and serve other
/// `systemd --user` services — so we never touch it, just point to the
/// command the user can run themselves. No-op if lingering is already off.
pub async fn note_if_enabled() {
    if !is_enabled().await {
        return;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "<your-user>".into());
    println!(
        "  Note: systemd lingering is still enabled for {user}. \
         Run `sudo loginctl disable-linger {user}` to disable."
    );
}
