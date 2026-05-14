//! Unified environment + install-state checks for `ryra doctor` and the
//! preflight gate that runs before every `ryra add`.
//!
//! Each [`Issue`] variant carries the data needed to render an actionable
//! fix message via its `Display` impl, plus a [`Severity`] that decides
//! whether `ryra add` should bail (`Blocker`) or just warn (`Warning` /
//! `Info`). One source of truth for "what can go wrong with a ryra
//! setup" — adding a new check means adding one variant + one detection
//! function and both `ryra doctor` and the install gate pick it up.
//!
//! `--tailscale`-specific checks are kept separate ([`check_tailscale_runtime`])
//! because they're only relevant when the user explicitly opts into the
//! Tailscale path; surfacing them in the always-on `ryra doctor` view
//! would be noise for users who never touch tailscale.

use std::fmt;
use std::fs;
use std::path::PathBuf;

use crate::config::schema::Config;
use crate::system::tailscale;

/// Minimum subuid/subgid range required for rootless podman to map common
/// container UIDs/GIDs (e.g. nginx user 101, postgres 999, shadow group 42).
/// 65536 is the standard allocation size shipped by adduser/usermod.
const MIN_SUBID_RANGE: u32 = 65536;

/// How serious an [`Issue`] is. Drives both UI grouping in `ryra doctor`
/// output and the gate behaviour of `ryra add` (which bails on any
/// `Blocker` but otherwise prints warnings without stopping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Will cause installs to fail outright. `ryra add` refuses to proceed.
    Blocker,
    /// Service runs but is in a state the user probably wants to fix —
    /// stale symlinks, linger off, etc.
    Warning,
    /// Informational, doesn't affect anything currently. Old-format
    /// installs missing `metadata.toml` etc.
    Info,
}

/// A typed, renderable problem detected by [`check_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// User has no entry in /etc/subuid or /etc/subgid.
    SubidNotConfigured {
        user: String,
        missing_files: Vec<&'static str>,
    },
    /// Range is too small — common on Debian where adduser doesn't auto-allocate.
    SubidRangeTooSmall {
        user: String,
        current: u32,
        minimum: u32,
    },
    /// `--tailscale` was used but the `tailscale` CLI isn't on PATH.
    TailscaleCliMissing,
    /// CLI is present but `tailscale status --json` doesn't return a
    /// `*.ts.net` DNSName for this node.
    TailscaleNotLoggedIn,
    /// A symlink in `~/.config/containers/systemd/` points at a target
    /// that no longer exists. Usually means the user `rm -rf`d the
    /// service's home dir under `~/.local/share/services/<svc>/`.
    DanglingSymlink { link: PathBuf, target: PathBuf },
    /// A real quadlet file lives in `~/.local/share/services/<svc>/`
    /// but no matching symlink exists in the systemd quadlet path,
    /// so systemd doesn't know about it. Usually means the user
    /// deleted the symlink by hand.
    OrphanQuadletFile { path: PathBuf },
    /// A service is installed (has a marker'd quadlet) but lacks a
    /// `metadata.toml`. Pre-metadata.toml install — reinstall to migrate.
    MissingMetadata { service: String },
    /// `loginctl --user enable-linger` hasn't been run, so user-level
    /// services don't survive logout / reboot.
    LingerNotEnabled,
    /// Couldn't read the quadlet symlink farm or service data root to
    /// detect drift — usually a permissions problem on
    /// `~/.config/containers/systemd/` or `~/.local/share/services/`.
    /// Surfaced rather than swallowed so the user knows their install
    /// state isn't being checked.
    IntegrityScanFailed { error: String },
}

impl Issue {
    /// How `ryra add` and `ryra doctor` should treat this issue.
    pub fn severity(&self) -> Severity {
        match self {
            Issue::SubidNotConfigured { .. } | Issue::SubidRangeTooSmall { .. } => {
                Severity::Blocker
            }
            Issue::TailscaleCliMissing | Issue::TailscaleNotLoggedIn => Severity::Warning,
            Issue::DanglingSymlink { .. } | Issue::OrphanQuadletFile { .. } => Severity::Warning,
            Issue::LingerNotEnabled => Severity::Warning,
            Issue::MissingMetadata { .. } => Severity::Info,
            Issue::IntegrityScanFailed { .. } => Severity::Warning,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::SubidNotConfigured {
                user,
                missing_files,
            } => {
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
            Issue::SubidRangeTooSmall {
                user,
                current,
                minimum,
            } => {
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
            Issue::TailscaleCliMissing => {
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
            Issue::TailscaleNotLoggedIn => {
                write!(
                    f,
                    "this node isn't logged into a tailnet.\n\
                     `tailscale status` doesn't return a *.ts.net hostname.\n\
                     \n\
                     Fix:\n  \
                       sudo tailscale up",
                )
            }
            Issue::DanglingSymlink { link, target } => {
                write!(
                    f,
                    "{} is a dangling symlink → {} (target missing).\n\
                     The service's data dir was deleted but the systemd unit pointer wasn't.\n\
                     \n\
                     Fix:\n  \
                       rm {}",
                    link.display(),
                    target.display(),
                    link.display(),
                )
            }
            Issue::OrphanQuadletFile { path } => {
                write!(
                    f,
                    "{} exists but no matching symlink in ~/.config/containers/systemd/, so systemd doesn't see it.\n\
                     \n\
                     Fix (re-link):\n  \
                       ln -sf {} ~/.config/containers/systemd/{}\n  \
                       systemctl --user daemon-reload\n\
                     Or delete the orphan: ryra remove --purge <service>",
                    path.display(),
                    path.display(),
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                )
            }
            Issue::MissingMetadata { service } => {
                write!(
                    f,
                    "{service} is installed but has no metadata.toml — install record from a pre-metadata ryra version.\n\
                     `ryra list` and `ryra remove` will work but URL/exposure won't be reported.\n\
                     \n\
                     Fix (reinstall to migrate):\n  \
                       ryra remove --purge {service} && ryra add {service}",
                )
            }
            Issue::LingerNotEnabled => {
                write!(
                    f,
                    "loginctl linger isn't enabled, so your user services stop when you log out.\n\
                     \n\
                     Fix:\n  \
                       loginctl enable-linger",
                )
            }
            Issue::IntegrityScanFailed { error } => {
                write!(
                    f,
                    "couldn't scan installed services to check for drift: {error}\n\
                     Fix the underlying error (commonly a permissions problem on \
                     ~/.config/containers/systemd/ or ~/.local/share/services/) so \
                     `ryra doctor` can verify install state.",
                )
            }
        }
    }
}

/// Run every always-applicable check and return all detected issues
/// (any severity). Tailscale-only checks are conditional; see
/// [`check_tailscale_runtime`].
pub fn check_all(_config: &Config) -> Vec<Issue> {
    let mut issues = Vec::new();
    if let Err(e) = check_subid_range() {
        issues.push(e);
    }
    if !check_linger_enabled() {
        issues.push(Issue::LingerNotEnabled);
    }
    issues.extend(check_install_integrity());
    issues
}

/// Filtered view: only `Blocker`-severity issues. `ryra add` calls this
/// to decide whether to bail; warnings/info get printed but don't gate.
pub fn blockers(config: &Config) -> Vec<Issue> {
    check_all(config)
        .into_iter()
        .filter(|i| i.severity() == Severity::Blocker)
        .collect()
}

/// Verify the host can do `tailscale serve`: the CLI is on PATH and the
/// node is logged into a tailnet. Called from the `--tailscale` flag
/// handler and the "Tailscale" branch of the exposure prompt; failure is
/// fatal (we can't auto-derive a tailnet URL without a logged-in node).
pub fn check_tailscale_runtime() -> Result<(), Issue> {
    if !tailscale::cli_available() {
        return Err(Issue::TailscaleCliMissing);
    }
    if tailscale::self_dns_name().is_none() {
        return Err(Issue::TailscaleNotLoggedIn);
    }
    Ok(())
}

fn check_subid_range() -> Result<(), Issue> {
    let user = std::env::var("USER").unwrap_or_default();
    if user.is_empty() {
        // No $USER means we can't check — skip rather than false-positive.
        return Ok(());
    }

    let mut missing = Vec::new();
    let subuid_size = parse_subid_range("/etc/subuid", &user, &mut missing);
    let subgid_size = parse_subid_range("/etc/subgid", &user, &mut missing);

    if !missing.is_empty() {
        return Err(Issue::SubidNotConfigured {
            user,
            missing_files: missing,
        });
    }
    let min = subuid_size.min(subgid_size);
    if min < MIN_SUBID_RANGE {
        return Err(Issue::SubidRangeTooSmall {
            user,
            current: min,
            minimum: MIN_SUBID_RANGE,
        });
    }
    Ok(())
}

/// `loginctl --user enable-linger`: are user services allowed to keep
/// running after logout? Reads via `loginctl show-user`. Unable-to-read
/// is treated as "enabled" (don't false-positive on systems without
/// loginctl, e.g. some CI containers).
fn check_linger_enabled() -> bool {
    let user = match std::env::var("USER") {
        Ok(u) if !u.is_empty() => u,
        _ => return true,
    };
    let output = std::process::Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger"])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            !stdout.trim().eq_ignore_ascii_case("Linger=no")
        }
        _ => true,
    }
}

/// Detect drift between the quadlet symlink farm and the per-service
/// home dirs: dangling symlinks, orphan quadlet files, and installed
/// services missing `metadata.toml`. Returns the issues in the order
/// they're discovered.
fn check_install_integrity() -> Vec<Issue> {
    let mut out = Vec::new();
    let Ok(quadlet) = crate::quadlet_dir() else {
        return out;
    };
    let Ok(data_root) = crate::service_data_root() else {
        return out;
    };

    // Dangling symlinks in quadlet dir whose target sits under our data root.
    if let Ok(entries) = std::fs::read_dir(&quadlet) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_symlink() {
                continue;
            }
            let Ok(target) = std::fs::read_link(&path) else {
                continue;
            };
            let resolved = if target.is_absolute() {
                target.clone()
            } else {
                // `path` came from read_dir on `quadlet`, so it always has a
                // parent. The else-arm only fires if a future caller hands us
                // a rootless path — skip rather than join against an empty
                // base and report a phantom dangling symlink.
                let Some(parent) = path.parent() else {
                    continue;
                };
                parent.join(&target)
            };
            if !resolved.starts_with(&data_root) {
                continue;
            }
            if !resolved.exists() {
                out.push(Issue::DanglingSymlink {
                    link: path,
                    target: resolved,
                });
            }
        }
    }

    // Orphan quadlet files: real .container/.network/.volume in service home
    // with no matching symlink in quadlet dir, plus missing metadata.toml
    // for marker'd installs.
    let managed = match crate::scan_managed_services() {
        Ok(m) => m,
        Err(e) => {
            out.push(Issue::IntegrityScanFailed {
                error: e.to_string(),
            });
            return out;
        }
    };
    for svc in &managed {
        let Ok(home) = crate::service_home(svc) else {
            continue;
        };
        if !home.is_dir() {
            continue;
        }
        if let Ok(meta_path) = crate::metadata_path(svc)
            && !meta_path.exists()
        {
            out.push(Issue::MissingMetadata {
                service: svc.clone(),
            });
        }
        if let Ok(entries) = std::fs::read_dir(&home) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let n = name.to_string_lossy();
                if !(n.ends_with(".container") || n.ends_with(".network") || n.ends_with(".volume"))
                {
                    continue;
                }
                let symlink = quadlet.join(&name);
                let symlink_ok = std::fs::read_link(&symlink)
                    .ok()
                    .and_then(|t| {
                        if t.is_absolute() {
                            Some(t)
                        } else {
                            // symlink = quadlet.join(name) — always has a parent.
                            symlink.parent().map(|p| p.join(&t))
                        }
                    })
                    .is_some_and(|resolved| resolved == path);
                if !symlink_ok {
                    out.push(Issue::OrphanQuadletFile { path });
                }
            }
        }
    }

    out
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
        // A malformed count falls through as 0, which then trips
        // SubidRangeTooSmall — same actionable fix command as a missing range.
        let count = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
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
        let e = Issue::SubidRangeTooSmall {
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
        let e = Issue::SubidNotConfigured {
            user: "bob".into(),
            missing_files: vec!["/etc/subuid", "/etc/subgid"],
        };
        let s = format!("{e}");
        assert!(s.contains("/etc/subuid"));
        assert!(s.contains("/etc/subgid"));
    }

    #[test]
    fn tailscale_cli_missing_display_has_install_hint() {
        let s = format!("{}", Issue::TailscaleCliMissing);
        assert!(s.contains("tailscale.com/install"));
        assert!(s.contains("ryra add caddy") && s.contains("--url"));
    }

    #[test]
    fn tailscale_not_logged_in_display_has_up_command() {
        let s = format!("{}", Issue::TailscaleNotLoggedIn);
        assert!(s.contains("tailscale up"));
    }

    #[test]
    fn severity_split() {
        assert_eq!(
            Issue::SubidRangeTooSmall {
                user: "x".into(),
                current: 0,
                minimum: 1,
            }
            .severity(),
            Severity::Blocker
        );
        assert_eq!(
            Issue::DanglingSymlink {
                link: "/a".into(),
                target: "/b".into(),
            }
            .severity(),
            Severity::Warning
        );
        assert_eq!(
            Issue::MissingMetadata {
                service: "x".into(),
            }
            .severity(),
            Severity::Info
        );
    }

    #[test]
    fn dangling_symlink_display_has_rm_fix() {
        let s = format!(
            "{}",
            Issue::DanglingSymlink {
                link: "/x/foo.container".into(),
                target: "/y/foo.container".into(),
            }
        );
        assert!(s.contains("rm /x/foo.container"));
    }

    #[test]
    fn missing_metadata_display_suggests_reinstall() {
        let s = format!(
            "{}",
            Issue::MissingMetadata {
                service: "forgejo".into(),
            }
        );
        assert!(s.contains("ryra remove --purge forgejo"));
        assert!(s.contains("ryra add forgejo"));
    }
}
