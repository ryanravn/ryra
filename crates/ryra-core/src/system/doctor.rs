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

/// Minimum podman version. Quadlet must pass `${...}` port/path values
/// through to the generated ExecStart (validation removed in 5.3.0) —
/// older quadlet rejects `PublishPort=${SERVICE_PORT_HTTP}:...` outright,
/// so every registry service fails to generate. Ubuntu 24.04 LTS ships
/// 4.9; this check turns that into one clear message instead of a
/// confusing unit failure.
const MIN_PODMAN: (u32, u32) = (5, 3);

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
    /// `podman` is missing or older than [`MIN_PODMAN`]. Registry
    /// quadlets rely on runtime env expansion that older quadlet
    /// versions reject at generation time.
    PodmanUnsupported {
        /// `podman --version` output, `None` when the binary isn't on
        /// PATH or didn't run.
        found: Option<String>,
    },
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
    /// A service's metadata says OIDC SSO is on, but the managed auth
    /// provider's config has no client registered for it: ryra's
    /// bookkeeping and the provider's actual state disagree, so SSO is
    /// silently broken. Usually a `ryra backup restore` of the provider
    /// from a snapshot predating this service's registration.
    AuthSsoDesync { service: String },
    /// A service is exposed via Tailscale (`svc:<svc_name>`) but the
    /// control plane hasn't approved this host to serve it, so the
    /// `*.ts.net` URL routes nowhere even though the container is healthy.
    /// `ryra add` verifies approval once at install time; this catches it
    /// drifting out of approval afterwards (ACL change, host de-approved,
    /// tailscaled losing the advertisement across a reboot).
    TailscaleServiceUnapproved { service: String, svc_name: String },
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
    /// A `runtime = "native"` service was installed from a local project dir
    /// that no longer exists (the user deleted or moved their repo). The unit
    /// runs from that dir, so it can't start or rebuild (a zombie install).
    NativeSourceMissing { service: String, source: PathBuf },
    /// A quadlet references an `EnvironmentFile=` that doesn't exist on
    /// disk. The unit fails to start, or starts with every `${SERVICE_*}`
    /// var expanding to an empty string. Usually means the service's data
    /// dir was moved or renamed (e.g. `mv grafana grafana-test`) or the
    /// `.env` was deleted by hand.
    BrokenEnvFileRef {
        service: String,
        quadlet: PathBuf,
        env_file: PathBuf,
    },
    /// `loginctl --user enable-linger` hasn't been run, so user-level
    /// services don't survive logout / reboot.
    LingerNotEnabled,
    /// Rootless podman fell back to the cgroupfs cgroup manager because there's
    /// no usable systemd user session (no user D-Bus). `systemctl --user`
    /// quadlets still run (systemd owns their cgroup), but direct `podman build`
    /// and `podman exec` fail to create containers ("sd-bus call: Interactive
    /// authentication required"). Caused by lingering off and/or a missing user
    /// D-Bus session (`dbus-user-session` on Debian/Ubuntu) and/or an unset
    /// XDG_RUNTIME_DIR. This is the wart that turns a clean `ryra add` of a
    /// container-built service into a cryptic crun failure.
    PodmanCgroupfsFallback,
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
            Issue::PodmanUnsupported { .. } => Severity::Blocker,
            Issue::SubidNotConfigured { .. } | Issue::SubidRangeTooSmall { .. } => {
                Severity::Blocker
            }
            Issue::TailscaleCliMissing | Issue::TailscaleNotLoggedIn => Severity::Warning,
            Issue::AuthSsoDesync { .. } => Severity::Warning,
            Issue::TailscaleServiceUnapproved { .. } => Severity::Warning,
            Issue::DanglingSymlink { .. } | Issue::OrphanQuadletFile { .. } => Severity::Warning,
            Issue::BrokenEnvFileRef { .. } => Severity::Warning,
            Issue::LingerNotEnabled => Severity::Warning,
            Issue::PodmanCgroupfsFallback => Severity::Warning,
            Issue::MissingMetadata { .. } => Severity::Info,
            Issue::NativeSourceMissing { .. } => Severity::Warning,
            Issue::IntegrityScanFailed { .. } => Severity::Warning,
        }
    }

    /// Stable machine-readable identifier for the issue variant, so a UI or
    /// rpc client can switch on it without parsing the message. Kept in one
    /// match so adding a variant surfaces here as a compile error rather than
    /// silently degrading to a blank code.
    pub fn code(&self) -> &'static str {
        match self {
            Issue::PodmanUnsupported { .. } => "podman_unsupported",
            Issue::SubidNotConfigured { .. } => "subid_not_configured",
            Issue::SubidRangeTooSmall { .. } => "subid_range_too_small",
            Issue::TailscaleCliMissing => "tailscale_cli_missing",
            Issue::TailscaleNotLoggedIn => "tailscale_not_logged_in",
            Issue::AuthSsoDesync { .. } => "auth_sso_desync",
            Issue::TailscaleServiceUnapproved { .. } => "tailscale_service_unapproved",
            Issue::DanglingSymlink { .. } => "dangling_symlink",
            Issue::OrphanQuadletFile { .. } => "orphan_quadlet_file",
            Issue::MissingMetadata { .. } => "missing_metadata",
            Issue::NativeSourceMissing { .. } => "native_source_missing",
            Issue::BrokenEnvFileRef { .. } => "broken_env_file_ref",
            Issue::LingerNotEnabled => "linger_not_enabled",
            Issue::PodmanCgroupfsFallback => "podman_cgroupfs_fallback",
            Issue::IntegrityScanFailed { .. } => "integrity_scan_failed",
        }
    }

    /// The installed service this issue is scoped to, when it's service-specific.
    pub fn service(&self) -> Option<String> {
        match self {
            Issue::AuthSsoDesync { service }
            | Issue::TailscaleServiceUnapproved { service, .. }
            | Issue::MissingMetadata { service }
            | Issue::NativeSourceMissing { service, .. }
            | Issue::BrokenEnvFileRef { service, .. } => Some(service.clone()),
            _ => None,
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::PodmanUnsupported { found } => match found {
                Some(version) => write!(
                    f,
                    "podman {version} is too old — ryra needs podman >= {}.{} \
                     (quadlet env expansion in PublishPort/Volume).\n\
                     \n\
                     Fix: upgrade podman — current Debian-based, Fedora, and Arch \
                     releases all ship a supported version.",
                    MIN_PODMAN.0, MIN_PODMAN.1,
                ),
                None => write!(
                    f,
                    "podman isn't on PATH — ryra runs every service as a rootless \
                     podman container.\n\
                     \n\
                     Fix:\n  \
                       sudo apt install podman      # Debian-based\n  \
                       sudo dnf install podman      # Fedora\n  \
                       sudo pacman -S podman        # Arch",
                ),
            },
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
            Issue::AuthSsoDesync { service } => {
                write!(
                    f,
                    "{service} is configured for OIDC SSO, but the auth provider has no client \
                     registered for it, so SSO is broken even though ryra's metadata says it's \
                     wired. Often follows a `ryra backup restore` of the provider from a \
                     snapshot taken before {service} was added with --auth.\n\
                     \n\
                     Fix (re-registers using the existing client credentials in {service}'s \
                     .env, no secret rotation):\n  \
                       ryra configure {service} --reassert-auth -y",
                )
            }
            Issue::TailscaleServiceUnapproved { service, svc_name } => {
                write!(
                    f,
                    "{service} is exposed on your tailnet (svc:{svc_name}) but the control \
                     plane hasn't approved this host to serve it, so its *.ts.net URL routes \
                     nowhere even though the container is healthy.\n\
                     \n\
                     Fix (most common: tailscaled didn't push the advertisement):\n  \
                       sudo systemctl restart tailscaled\n\
                     If it stays unapproved, your tailnet ACL isn't auto-approving the service. \
                     Confirm with:\n  \
                       sudo tailscale status --json | jq '.Self.CapMap[\"service-host\"]'\n\
                     and add the service to autoApprovers.services in the ACL (or approve the \
                     host in the admin console).",
                )
            }
            Issue::DanglingSymlink { link, target } => {
                write!(
                    f,
                    "{} is a dangling symlink → {} (target missing).\n\
                     The service's data dir was moved, renamed, or deleted, but the \
                     systemd unit pointer wasn't updated.\n\
                     \n\
                     Fix (restore the dir if it was moved, or drop the unit):\n  \
                       # put the data dir back so {} exists again\n  \
                       # or: rm {}",
                    link.display(),
                    target.display(),
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
            Issue::NativeSourceMissing { service, source } => {
                write!(
                    f,
                    "{service} (native) runs from {} but that directory is gone \
                     (deleted or moved). It can't start or rebuild.\n\
                     \n\
                     Fix (restore the source, then re-render):\n  \
                       # put the project back at {}, then: ryra upgrade {service}\n  \
                       # or drop the install: ryra remove --purge {service}",
                    source.display(),
                    source.display(),
                )
            }
            Issue::BrokenEnvFileRef {
                service,
                quadlet,
                env_file,
            } => {
                write!(
                    f,
                    "{} references EnvironmentFile={} but that file doesn't exist.\n\
                     The unit can't start — and ${{SERVICE_HOME}}/${{SERVICE_PORT_*}} in it \
                     would expand to empty strings.\n\
                     Usually the service's data dir was moved or renamed, or the .env was deleted.\n\
                     \n\
                     Fix (restore the path, or reinstall):\n  \
                       # put the data back at {}, then: systemctl --user restart {service}\n  \
                       # or: ryra remove --purge {service} && ryra add {service}",
                    quadlet.display(),
                    env_file.display(),
                    env_file
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("?"))
                        .display(),
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
            Issue::PodmanCgroupfsFallback => {
                write!(
                    f,
                    "rootless podman has no usable systemd user session and fell back to the\n\
                     cgroupfs cgroup manager. Quadlet services started via `systemctl --user`\n\
                     still run, but direct `podman build` / `podman exec` fail to create\n\
                     containers (\"sd-bus call: Interactive authentication required\"). This box\n\
                     can pull + run images, but it can't build one locally until the user\n\
                     session works.\n\
                     \n\
                     Fix (run all three, then log out and back in so the session starts):\n  \
                       sudo loginctl enable-linger $USER\n  \
                       sudo apt-get install -y dbus-user-session   # Debian/Ubuntu: provides the user D-Bus session\n  \
                       # confirm XDG_RUNTIME_DIR=/run/user/$(id -u) is set in your shell\n\
                     \n\
                     Verify afterwards:  podman info --format '{{{{.Host.CgroupManager}}}}'  (want: systemd)\n\
                     Or sidestep it entirely: build the image in CI and let the box pull it.",
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
    if let Err(e) = check_podman_version() {
        issues.push(e);
    }
    if let Err(e) = check_subid_range() {
        issues.push(e);
    }
    if !check_linger_enabled() {
        issues.push(Issue::LingerNotEnabled);
    }
    if !check_podman_user_session() {
        issues.push(Issue::PodmanCgroupfsFallback);
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

/// Verify ryra's auth bookkeeping matches the provider's actual state: for
/// every installed service whose metadata says SSO is on, the managed auth
/// provider should still have a client registered for it. Catches the
/// provider/consumer desync (e.g. a provider restore that rolled back past
/// a registration) that local install-state checks miss entirely.
///
/// Doctor-only (not in the `ryra add` gate) and silent unless the managed
/// provider is installed and a service claims auth; only a definite
/// mismatch is reported; undeterminable (provider config unreadable) stays
/// quiet.
pub fn check_auth_wiring() -> Vec<Issue> {
    // Only the managed provider exposes a config we can introspect; an
    // external OIDC provider is the user's to verify.
    if !crate::is_service_installed(crate::WellKnownService::Authelia.as_str()) {
        return Vec::new();
    }
    let Ok(installed) = crate::list_installed() else {
        return Vec::new();
    };
    let mut issues = Vec::new();
    for svc in &installed {
        if svc.auth_kind.is_none() {
            continue;
        }
        if crate::authelia::oidc_client_registered(&svc.name) == Some(false) {
            issues.push(Issue::AuthSsoDesync {
                service: svc.name.clone(),
            });
        }
    }
    issues
}

/// Verify every Tailscale-exposed installed service is still approved by
/// the tailnet to serve its `svc:<name>`. Deliberately *not* part of
/// [`check_all`]: it's a `ryra doctor`-only check (no point probing the
/// tailnet on the `ryra add` blocker gate), and it stays silent unless the
/// user actually has Tailscale-exposed services; no tailnet calls happen
/// otherwise. Only a definite "not approved" is reported; when approval
/// can't be determined (CLI missing, status unreadable) we say nothing
/// rather than nag.
pub fn check_tailscale_services() -> Vec<Issue> {
    let Ok(installed) = crate::list_installed() else {
        // Install-state errors are already surfaced by check_install_integrity;
        // don't double-report here.
        return Vec::new();
    };
    let mut issues = Vec::new();
    for svc in &installed {
        if !svc.exposure.is_tailscale() {
            continue;
        }
        let Some(svc_name) = svc.exposure.tailscale_svc_name() else {
            continue;
        };
        if tailscale::is_service_approved(&svc_name) == Some(false) {
            issues.push(Issue::TailscaleServiceUnapproved {
                service: svc.name.clone(),
                svc_name,
            });
        }
    }
    issues
}

/// Blocker when podman is missing or below [`MIN_PODMAN`].
fn check_podman_version() -> Result<(), Issue> {
    let Ok(output) = std::process::Command::new("podman")
        .arg("--version")
        .output()
    else {
        return Err(Issue::PodmanUnsupported { found: None });
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some((major, minor, patch)) = parse_podman_version(&text) else {
        // Ran but printed something unparseable — report what we saw
        // rather than guessing it's fine.
        return Err(Issue::PodmanUnsupported {
            found: Some(text.trim().to_string()),
        });
    };
    if (major, minor) < MIN_PODMAN {
        return Err(Issue::PodmanUnsupported {
            found: Some(format!("{major}.{minor}.{patch}")),
        });
    }
    Ok(())
}

/// Parse `podman --version` output ("podman version 5.8.2", tolerating
/// suffixes like "5.9.0-dev").
fn parse_podman_version(s: &str) -> Option<(u32, u32, u32)> {
    let nums = s.split_whitespace().last()?;
    let mut parts = nums.split('.');
    let digits = |p: &str| -> Option<u32> {
        let d: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
        d.parse().ok()
    };
    let major = digits(parts.next()?)?;
    let minor = digits(parts.next()?)?;
    let patch = parts.next().and_then(digits).unwrap_or(0);
    Some((major, minor, patch))
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

/// Whether rootless podman has a usable systemd user session, i.e. it isn't
/// falling back to the cgroupfs cgroup manager. We read the observable symptom
/// directly: `podman info`'s cgroup manager. `systemd` (or anything that isn't
/// `cgroupfs`) is healthy; `cgroupfs` means crun can't register systemd scopes,
/// so `podman build`/`exec` will hit "Interactive authentication required".
///
/// Conservative: if podman is absent or unreadable we return `true` (healthy)
/// so we don't false-positive on hosts where this check can't run (and the
/// separate podman-version check already flags a missing podman).
fn check_podman_user_session() -> bool {
    let output = std::process::Command::new("podman")
        .args(["info", "--format", "{{.Host.CgroupManager}}"])
        .output();
    match output {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout)
            .trim()
            .eq_ignore_ascii_case("cgroupfs"),
        _ => true,
    }
}

/// Detect drift between the quadlet symlink farm and the per-service
/// home dirs: dangling symlinks, orphan quadlet files, and installed
/// services missing `metadata.toml`. Returns the issues in the order
/// they're discovered.
/// Scan a generated `.container` file for `EnvironmentFile=` lines whose
/// target doesn't exist. `%h` is resolved like systemd would; a leading `-`
/// (systemd's ignore-missing marker) means absence is by design — skipped.
fn broken_env_file_refs(service: &str, quadlet_path: &std::path::Path) -> Vec<Issue> {
    let Ok(content) = std::fs::read_to_string(quadlet_path) else {
        return Vec::new();
    };
    let Ok(home) = crate::home_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let Some(value) = line.trim().strip_prefix("EnvironmentFile=") else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() || value.starts_with('-') {
            continue;
        }
        let resolved = PathBuf::from(value.replace("%h", &home.to_string_lossy()));
        if !resolved.exists()
            && !out.iter().any(
                |i| matches!(i, Issue::BrokenEnvFileRef { env_file, .. } if *env_file == resolved),
            )
        {
            out.push(Issue::BrokenEnvFileRef {
                service: service.to_string(),
                quadlet: quadlet_path.to_path_buf(),
                env_file: resolved,
            });
        }
    }
    out
}

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
                    out.push(Issue::OrphanQuadletFile { path: path.clone() });
                }
                if n.ends_with(".container") {
                    out.extend(broken_env_file_refs(svc, &path));
                }
            }
        }
    }

    // Native services run from their source dir (recorded in metadata as the
    // install's `registry`, which for a local-path install holds the project
    // path). Quadlet scans above never see them, so check separately that the
    // source still exists: a deleted/moved repo leaves a zombie install.
    if let Ok(root) = crate::paths::service_data_root()
        && let Ok(entries) = std::fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            let Some(svc) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(Some(meta)) = crate::metadata::load_metadata(&svc) else {
                continue;
            };
            if meta.runtime != crate::registry::service_def::Runtime::Native {
                continue;
            }
            // Only local-path installs record a filesystem path here; registry
            // installs record a registry name (ryra-managed, not user-deletable).
            if crate::registry::resolve::is_path_like(&meta.registry) {
                let source = PathBuf::from(&meta.registry);
                if !source.is_dir() {
                    out.push(Issue::NativeSourceMissing {
                        service: svc,
                        source,
                    });
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
    fn podman_version_parsing() {
        assert_eq!(
            parse_podman_version("podman version 5.8.2"),
            Some((5, 8, 2))
        );
        assert_eq!(
            parse_podman_version("podman version 4.9.3"),
            Some((4, 9, 3))
        );
        assert_eq!(
            parse_podman_version("podman version 5.9.0-dev"),
            Some((5, 9, 0))
        );
        assert_eq!(parse_podman_version("podman version 6.0"), Some((6, 0, 0)));
        assert_eq!(parse_podman_version("garbage"), None);
        // The floor itself: 5.3 passes, 5.2 / 4.x don't.
        assert!((5, 3) >= MIN_PODMAN);
        assert!((5, 2) < MIN_PODMAN);
        assert!((4, 9) < MIN_PODMAN);
    }

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
    fn podman_cgroupfs_fallback_display_has_the_session_fix() {
        let s = format!("{}", Issue::PodmanCgroupfsFallback);
        // The three commands a user needs, and the verify hint (with the braces
        // un-escaped from the format string).
        assert!(s.contains("enable-linger"), "{s}");
        assert!(s.contains("dbus-user-session"), "{s}");
        assert!(s.contains("XDG_RUNTIME_DIR"), "{s}");
        assert!(s.contains("{{.Host.CgroupManager}}"), "{s}");
        assert_eq!(Issue::PodmanCgroupfsFallback.severity(), Severity::Warning);
    }

    #[test]
    fn auth_sso_desync_display_names_service_and_nonrotating_fix() {
        let issue = Issue::AuthSsoDesync {
            service: "seafile".into(),
        };
        assert_eq!(issue.severity(), Severity::Warning);
        let s = format!("{issue}");
        assert!(s.contains("seafile"));
        // Points at the non-rotating repair command.
        assert!(s.contains("ryra configure seafile --reassert-auth"));
    }

    #[test]
    fn tailscale_unapproved_display_names_service_and_fix() {
        let issue = Issue::TailscaleServiceUnapproved {
            service: "vikunja".into(),
            svc_name: "vikunja-debian".into(),
        };
        assert_eq!(issue.severity(), Severity::Warning);
        let s = format!("{issue}");
        // Names the service and its svc:, and carries the one-line fix.
        assert!(s.contains("vikunja") && s.contains("svc:vikunja-debian"));
        assert!(s.contains("systemctl restart tailscaled"));
        assert!(s.contains("autoApprovers.services"));
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
