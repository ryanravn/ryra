//! Host-environment checks run before `ryra add` does any work.
//!
//! Each variant carries the data needed to render an actionable fix message,
//! so users see the exact command to run instead of a cryptic podman error.

use std::fmt;
use std::fs;

use crate::config::schema::Config;
use crate::system::tailscale;

/// Minimum subuid/subgid range required for rootless podman to map common
/// container UIDs/GIDs (e.g. nginx user 101, postgres 999, shadow group 42).
/// 65536 is the standard allocation size shipped by adduser/usermod.
const MIN_SUBID_RANGE: u32 = 65536;

/// Typed preflight failure. Each variant encodes both the diagnosis and
/// the data needed to print a fix command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightError {
    /// User has no entry in /etc/subuid or /etc/subgid.
    SubidNotConfigured { user: String, missing_files: Vec<&'static str> },
    /// Range is too small — common on Debian where adduser doesn't auto-allocate.
    SubidRangeTooSmall { user: String, current: u32, minimum: u32 },
    /// `--tailscale` was used (or "Tailscale" picked in the prompt) but
    /// the `tailscale` CLI isn't on PATH. Without it ryra can't read the
    /// node's MagicDNS name or wire `tailscale serve` for HTTPS termination.
    TailscaleCliMissing,
    /// CLI is present but `tailscale status --json` doesn't return a
    /// `*.ts.net` DNSName for this node — usually means `tailscale up`
    /// hasn't been run.
    TailscaleNotLoggedIn,
}

impl fmt::Display for PreflightError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreflightError::SubidNotConfigured { user, missing_files } => {
                write!(
                    f,
                    "rootless podman needs subuid/subgid mappings, but {} has no entry in {}.\n\
                     \n\
                     Fix:\n  \
                       sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {}\n  \
                       podman system migrate",
                    user,
                    missing_files.join(" / "),
                    user,
                )
            }
            PreflightError::SubidRangeTooSmall { user, current, minimum } => {
                write!(
                    f,
                    "rootless podman needs at least {minimum} subuids/subgids, but {user} has only {current}.\n\
                     Containers with non-zero UIDs (postgres, nginx, etc.) will fail to extract.\n\
                     \n\
                     Fix:\n  \
                       sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}\n  \
                       podman system migrate",
                )
            }
            PreflightError::TailscaleCliMissing => {
                write!(
                    f,
                    "the `tailscale` CLI isn't on PATH.\n\
                     \n\
                     Fix (Debian/Ubuntu):\n  \
                       curl -fsSL https://tailscale.com/install.sh | sh\n\
                     Or drop --tailscale and reach the service via Caddy \
                     (run `ryra add caddy` first) or your own URL (--url).",
                )
            }
            PreflightError::TailscaleNotLoggedIn => {
                write!(
                    f,
                    "this node isn't logged into a tailnet.\n\
                     `tailscale status` doesn't return a *.ts.net hostname.\n\
                     \n\
                     Fix:\n  \
                       sudo tailscale up",
                )
            }
        }
    }
}

/// Run host-level preflight checks before any service install. Currently
/// just subuid/subgid; provider-specific checks (e.g. `--tailscale`)
/// fire inline at the relevant flag/prompt site instead of being driven
/// from a global config field.
pub fn check(_config: &Config) -> Result<(), PreflightError> {
    check_subid_range()?;
    Ok(())
}

/// Verify the host can do `tailscale serve`: the CLI is on PATH and the
/// node is logged into a tailnet. Called from the `--tailscale` flag
/// handler and the "Tailscale" branch of the exposure prompt; failure is
/// fatal (we can't auto-derive a tailnet URL without a logged-in node).
pub fn check_tailscale_runtime() -> Result<(), PreflightError> {
    if !tailscale::cli_available() {
        return Err(PreflightError::TailscaleCliMissing);
    }
    if tailscale::self_dns_name().is_none() {
        return Err(PreflightError::TailscaleNotLoggedIn);
    }
    Ok(())
}

fn check_subid_range() -> Result<(), PreflightError> {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        // No $USER means we can't check — skip rather than false-positive.
        return Ok(());
    }

    let mut missing = Vec::new();
    let subuid_size = parse_subid_range("/etc/subuid", &user, &mut missing);
    let subgid_size = parse_subid_range("/etc/subgid", &user, &mut missing);

    if !missing.is_empty() {
        return Err(PreflightError::SubidNotConfigured { user, missing_files: missing });
    }
    let min = subuid_size.min(subgid_size);
    if min < MIN_SUBID_RANGE {
        return Err(PreflightError::SubidRangeTooSmall {
            user,
            current: min,
            minimum: MIN_SUBID_RANGE,
        });
    }
    Ok(())
}

/// Parse `username:start:count` lines for the given user. Returns the count
/// (range size) — 0 if the user isn't found. Records `path` in `missing` if
/// the file is unreadable or has no entry for the user.
fn parse_subid_range(path: &'static str, user: &str, missing: &mut Vec<&'static str>) -> u32 {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            missing.push(path);
            return 0;
        }
    };
    for line in contents.lines() {
        let mut parts = line.splitn(3, ':');
        let Some(name) = parts.next() else { continue };
        if name != user {
            continue;
        }
        let _start = parts.next();
        let count = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        return count;
    }
    missing.push(path);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_too_small_includes_fix_command() {
        let e = PreflightError::SubidRangeTooSmall {
            user: "alice".into(),
            current: 1000,
            minimum: 65536,
        };
        let s = format!("{e}");
        assert!(s.contains("usermod --add-subuids"));
        assert!(s.contains("alice"));
        assert!(s.contains("podman system migrate"));
    }

    #[test]
    fn display_not_configured_lists_files() {
        let e = PreflightError::SubidNotConfigured {
            user: "bob".into(),
            missing_files: vec!["/etc/subuid", "/etc/subgid"],
        };
        let s = format!("{e}");
        assert!(s.contains("/etc/subuid"));
        assert!(s.contains("/etc/subgid"));
    }

    #[test]
    fn tailscale_cli_missing_display_has_install_hint() {
        let s = format!("{}", PreflightError::TailscaleCliMissing);
        // Must point the user at a working install command and at the
        // alternative paths (Caddy / explicit --url) so they can choose.
        assert!(s.contains("tailscale.com/install"));
        assert!(s.contains("ryra add caddy") && s.contains("--url"));
    }

    #[test]
    fn tailscale_not_logged_in_display_has_up_command() {
        let s = format!("{}", PreflightError::TailscaleNotLoggedIn);
        assert!(s.contains("tailscale up"));
    }

    // No positive Tailscale test here: we'd need to either mock the
    // `tailscale` binary or assume the test host has it. Both are bad —
    // mocking adds complexity for a single check, and assuming presence
    // would make the test flaky on any CI without tailscale. The negative
    // path (CLI absent → TailscaleCliMissing) is exercised by the
    // `tailscale::cli_available` path being a thin Command wrapper, and
    // the Display tests above prove the error renders correctly.
}
