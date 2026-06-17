//! Filesystem paths ryra reads and writes.
//!
//! The directory name is `services/` (not `ryra/`) because the deployments
//! are the user's — ryra is just the scaffolding tool that puts them there.
//! Wiping `~/.local/share/services/`, `~/.config/services/`, and the
//! ryra-managed quadlets in `~/.config/containers/systemd/` removes ryra's
//! footprint completely.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::scope::Scope;

/// Root for all rootful (System-scope) ryra state. Fixed and tied to the
/// scope, not to whoever invokes ryra, so running as root never lands state in
/// `/root/...`. Mirrors the per-user layout one level down. The system config
/// dir (`/etc/ryra`) lives in [`crate::config::ConfigPaths::resolve_for`].
const SYSTEM_ROOT: &str = "/var/lib/ryra";
/// Where rootful podman/quadlet-generator looks for system quadlets.
const SYSTEM_QUADLET_DIR: &str = "/etc/containers/systemd";
/// Where the system systemd manager looks for native (non-quadlet) units.
const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// Sentinel value for `InstalledService.repo` meaning "came from the
/// default registry" (the project-managed git repo at
/// [`DEFAULT_REGISTRY_URL`]) rather than a user-added custom registry.
pub const REGISTRY_DEFAULT: &str = "default";

/// Git URL of the default service registry. Cloned on first
/// `ryra add`/`ryra search` into `<cache>/default/` and updated by
/// `ryra registry update`.
///
/// Tests and dev workflows can short-circuit the clone by setting
/// [`REGISTRY_DIR_ENV`] to a local directory; the resolver uses that
/// path verbatim instead.
pub const DEFAULT_REGISTRY_URL: &str = "https://github.com/ryanravn/ryra-registry.git";

/// Env var that, when set to an existing directory, replaces the git
/// fetch entirely — ryra uses that directory as the default registry
/// verbatim (no clone, no pull). The E2E test harness sets this to
/// `/opt/ryra-test-registry` inside the VM; dev workflows can point it
/// at a local checkout to iterate without committing/pushing.
pub const REGISTRY_DIR_ENV: &str = "RYRA_REGISTRY_DIR";

/// Env var that, when set, overrides the directory holding
/// `preferences.toml` (normally `~/.config/services/`). The E2E test
/// harness points this at a throwaway dir for host (bare-mode) runs so
/// tests never read or clobber the user's real SMTP/auth/backup
/// credentials. Only the preferences/config dir moves — service data
/// (`~/.local/share/services`) and quadlets (`~/.config/containers/systemd`)
/// stay put, because `systemctl --user` reads those from fixed locations.
pub const CONFIG_DIR_ENV: &str = "RYRA_CONFIG_DIR";

/// Env var that, when set, overrides the service-data root (normally
/// `~/.local/share/services/`). The host test harness points this at a
/// sandbox (`~/.local/share/services-test/services/`) so test deployments
/// never share a directory with the user's real services. Because ryra
/// stores each quadlet *inside* `service_home` and only symlinks it into
/// the systemd quadlet dir, moving this also moves the unit files; the
/// quadlet *symlink* still lands in the fixed `~/.config/containers/systemd`.
pub const DATA_DIR_ENV: &str = "RYRA_DATA_DIR";

/// The active `RYRA_DATA_DIR` override, if any. `None` for normal installs
/// (the common case) — callers use that to keep behaviour byte-identical
/// when no sandbox is requested.
pub(crate) fn data_dir_override() -> Option<PathBuf> {
    match std::env::var_os(DATA_DIR_ENV) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Resolve the user's home directory, falling back to $HOME.
pub(crate) fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
        .ok_or(Error::HomeDirNotFound)
}

/// Root directory holding every installed service's home dir:
/// `~/.local/share/services/`.
pub fn service_data_root() -> Result<PathBuf> {
    if let Some(dir) = data_dir_override() {
        return Ok(dir);
    }
    let base = match dirs::data_dir() {
        Some(d) => d,
        None => home_dir()?.join(".local").join("share"),
    };
    Ok(base.join("services"))
}

/// Data directory for a service: `~/.local/share/services/<name>`
///
/// Rejects path-like names before the join: `PathBuf::join` with an
/// absolute path REPLACES the base, so an unvalidated name like
/// `/home/user/project` would make this return that very directory,
/// and a purge would then delete it. A test-harness bug did exactly
/// that once; never again.
pub fn service_home(service_name: &str) -> Result<PathBuf> {
    if service_name.is_empty()
        || service_name == "."
        || service_name == ".."
        || service_name.contains('/')
        || service_name.contains('\\')
    {
        return Err(Error::ConfigValidation(format!(
            "invalid service name '{service_name}': names must not be paths"
        )));
    }
    Ok(service_data_root()?.join(service_name))
}

/// Per-install metadata file: `~/.local/share/services/<name>/metadata.toml`.
/// Stores the install-time decisions (registry, exposure, url, auth) so
/// later commands can reconstruct the install without scraping comments.
pub fn metadata_path(service_name: &str) -> Result<PathBuf> {
    Ok(service_home(service_name)?.join("metadata.toml"))
}

/// Scope-aware quadlet directory. User scope is the per-user
/// `~/.config/containers/systemd`; System scope is the host-wide
/// `/etc/containers/systemd`. See [`quadlet_dir`] for the user-scope default.
pub fn quadlet_dir_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => quadlet_dir(),
        Scope::System => Ok(PathBuf::from(SYSTEM_QUADLET_DIR)),
    }
}

/// Scope-aware systemd unit directory for native (non-quadlet) services.
/// User scope is `~/.config/systemd/user`; System scope is
/// `/etc/systemd/system`.
pub fn systemd_unit_dir_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => systemd_user_dir(),
        Scope::System => Ok(PathBuf::from(SYSTEM_UNIT_DIR)),
    }
}

/// Scope-aware service-data root. User scope is `~/.local/share/services`;
/// System scope is `/var/lib/ryra/services` (ignores the test data-dir
/// override, which is a user-scope-only sandboxing aid).
pub fn service_data_root_for(scope: Scope) -> Result<PathBuf> {
    match scope {
        Scope::User => service_data_root(),
        Scope::System => Ok(PathBuf::from(SYSTEM_ROOT).join("services")),
    }
}

/// Scope-aware per-service home dir. Rejects path-like names exactly like
/// [`service_home`].
pub fn service_home_for(scope: Scope, service_name: &str) -> Result<PathBuf> {
    match scope {
        Scope::User => service_home(service_name),
        Scope::System => {
            validate_service_name(service_name)?;
            Ok(service_data_root_for(scope)?.join(service_name))
        }
    }
}

/// Reject path-like service names before any `join` (see [`service_home`]).
fn validate_service_name(service_name: &str) -> Result<()> {
    if service_name.is_empty()
        || service_name == "."
        || service_name == ".."
        || service_name.contains('/')
        || service_name.contains('\\')
    {
        return Err(Error::ConfigValidation(format!(
            "invalid service name '{service_name}': names must not be paths"
        )));
    }
    Ok(())
}

/// Quadlet directory: ~/.config/containers/systemd
pub fn quadlet_dir() -> Result<PathBuf> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("containers").join("systemd"))
}

/// systemd `--user` unit directory: `~/.config/systemd/user`. Where native
/// (non-quadlet) service units are linked so `systemctl --user` finds them —
/// the analogue of [`quadlet_dir`] for `runtime = "native"` services.
pub fn systemd_user_dir() -> Result<PathBuf> {
    let base = match dirs::config_dir() {
        Some(d) => d,
        None => home_dir()?.join(".config"),
    };
    Ok(base.join("systemd").join("user"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_home_rejects_path_like_names() {
        // An absolute or traversing name must never escape the data root
        // (PathBuf::join with an absolute path replaces the base). A real
        // purge once deleted a whole repo this way.
        for bad in [
            "/home/user/code/ryra-api",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "",
        ] {
            assert!(
                service_home(bad).is_err(),
                "expected '{bad}' to be rejected as a service name"
            );
        }
    }

    #[test]
    fn service_home_accepts_plain_names() {
        // Plain registry-style names still resolve (under the data root).
        for good in ["forgejo", "ryra-api", "node-exporter", "caddy"] {
            let home = service_home(good).expect("plain name should resolve");
            assert!(home.ends_with(good));
        }
    }
}
