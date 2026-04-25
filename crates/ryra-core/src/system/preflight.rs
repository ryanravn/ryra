//! Host-environment checks run before `ryra add` does any work.
//!
//! Each variant carries the data needed to render an actionable fix message,
//! so users see the exact command to run instead of a cryptic podman error.

use std::fmt;
use std::fs;

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
        }
    }
}

/// Run all preflight checks. Returns the first failure, or `Ok(())` if all pass.
pub fn check() -> Result<(), PreflightError> {
    check_subid_range()?;
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
}
