//! Deployment scope: rootless per-user vs rootful host-wide.
//!
//! Every ryra service is installed in exactly one scope:
//!
//! - [`Scope::User`] -- rootless. Quadlets in `~/.config/containers/systemd`,
//!   managed with `systemctl --user`, podman as the (non-root) user. The
//!   default, and what customer workloads always use.
//! - [`Scope::System`] -- rootful. Quadlets in `/etc/containers/systemd`,
//!   managed with the system `systemctl`, podman as root. For operator
//!   infrastructure (a host metrics agent, eventually ryra-api itself) that has
//!   to be host-wide and privileged. Opt-in, and only for services whose
//!   registry definition declares `scope = "system"`.
//!
//! Safety invariants (enforced by callers, documented here):
//! - A System-scope operation requires the process to run as root; a User-scope
//!   operation requires it NOT to. The two never silently cross.
//! - A service name may exist in only one scope at a time, so the two on-disk
//!   trees can never be confused for the same install.

use serde::{Deserialize, Serialize};

/// Where a service's units, quadlets and data live, and how systemd/podman
/// manage them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Rootless, per-user. The default for everything customer-facing.
    #[default]
    User,
    /// Rootful, host-wide. Operator infrastructure only.
    System,
}

impl Scope {
    /// `true` for the rootful, host-wide scope.
    pub fn is_system(self) -> bool {
        matches!(self, Scope::System)
    }

    /// Lowercase wire/display name (`"user"` / `"system"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }

    /// The systemctl scope flag this scope runs under: `Some("--user")` for a
    /// per-user manager, `None` for the system manager. Callers prepend it to
    /// every `systemctl` invocation so one code path serves both scopes.
    pub fn systemctl_user_flag(self) -> Option<&'static str> {
        match self {
            Scope::User => Some("--user"),
            Scope::System => None,
        }
    }
}

/// Whether this process is running as root (EUID 0). System-scope operations
/// require it; User-scope operations require its absence.
pub fn is_root() -> bool {
    // SAFETY: geteuid is always safe; it only reads the caller's effective uid.
    unsafe { libc::geteuid() == 0 }
}
