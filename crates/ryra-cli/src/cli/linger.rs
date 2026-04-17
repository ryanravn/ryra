//! Detect whether systemd-logind lingering is enabled for the current user.
//!
//! Rootless podman services managed via quadlets run inside a user's
//! `systemd --user` manager. That manager exits when the user logs out,
//! taking every service with it — unless `loginctl enable-linger <user>`
//! has been run, which pins the user manager at boot regardless of login
//! state. Without lingering, `ryra` on a server looks like it's working
//! until the user's SSH session closes, then silently everything stops.

use anyhow::Result;
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
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim() == "yes"
        }
        _ => false,
    }
}

/// Print a warning (with the fix command) when lingering is off. Services
/// added by `ryra` won't survive a logout/reboot without this — so call
/// this anywhere the user is about to lean on services running in the
/// background (init, add).
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
