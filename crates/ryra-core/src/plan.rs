//! Typed steps the CLI executes, the warnings it surfaces, and the result
//! shapes returned from `add` / `remove` / `reset`. Pattern matching ensures
//! every step type is handled — no string parsing or if-chains.

use std::path::PathBuf;

use crate::generate::GeneratedFile;

/// One port served over a service's Tailscale vIP: TLS-terminated at
/// `https_port` on the service hostname, proxied to `http://127.0.0.1:<host_port>`.
/// The entry with `https_port == 443` answers at the bare hostname (web root).
#[derive(Debug, Clone)]
pub struct TailscalePort {
    pub https_port: u16,
    pub host_port: u16,
}

/// Resolve which ports a service exposes over its Tailscale vIP.
///
/// Ports declaring `tailscale_https` are each served on that HTTPS port,
/// mapped to their resolved host port. A service that declares none (every
/// single-port web app — seafile, authelia, …) falls back to serving its
/// primary port at the web root (`443`), preserving the original behaviour.
pub fn tailscale_ports(
    ports: &[crate::registry::service_def::PortDef],
    resolved: &[(String, u16)],
    primary_host_port: Option<u16>,
) -> Vec<TailscalePort> {
    let mapped: Vec<TailscalePort> = ports
        .iter()
        .filter_map(|p| {
            let https_port = p.tailscale_https?;
            let host_port = resolved
                .iter()
                .find(|(n, _)| n == &p.name)
                .map(|(_, hp)| *hp)
                .or(p.host_port)?;
            Some(TailscalePort {
                https_port,
                host_port,
            })
        })
        .collect();
    if !mapped.is_empty() {
        return mapped;
    }
    primary_host_port
        .map(|host_port| {
            vec![TailscalePort {
                https_port: 443,
                host_port,
            }]
        })
        .unwrap_or_default()
}

/// A discrete operation that the CLI executes.
pub enum Step {
    /// Write a file.
    WriteFile(GeneratedFile),
    /// Create a symlink at `link` pointing to `target`. Idempotent: if
    /// `link` already exists (whether as a file, dir, or symlink), it's
    /// removed first. Used to satisfy systemd's fixed quadlet path
    /// (`~/.config/containers/systemd/<svc>.container`) while keeping
    /// the real file alongside the rest of the service's data in
    /// `~/.local/share/services/<svc>/`.
    Symlink { link: PathBuf, target: PathBuf },
    /// Reload systemd for the current user.
    DaemonReload,
    /// Start a service under the current user's systemd.
    StartService { unit: String },
    /// Stop a service under the current user's systemd.
    StopService { unit: String },
    /// Restart a service under the current user's systemd.
    RestartService { unit: String },
    /// Reload Caddy's config without restarting the container.
    ReloadCaddy,
    /// Pull a container image.
    PullImage { image: String },
    /// Remove a file.
    RemoveFile(PathBuf),
    /// Remove a directory tree.
    RemoveDir(PathBuf),
    /// Remove a podman named volume.
    RemoveVolume { name: String },
    /// Remove a podman network. Best-effort: skipped when the network is
    /// still in use by another service (which is the correct outcome) or
    /// already gone. `ryra remove` emits this after stopping a service's
    /// `<svc>-network` unit, because stopping a `RemainAfterExit` network
    /// oneshot leaves the podman network behind — and that leak makes the
    /// next install fail (its regenerated network unit's `podman network
    /// create` hits the existing network).
    RemoveNetwork { name: String },
    /// Create a directory (with parents).
    CreateDir(PathBuf),
    /// Wait for a file to appear (with timeout).
    WaitForFile { path: PathBuf, timeout_secs: u32 },
    /// Poll an HTTP endpoint until it answers with `expect_status`, or time
    /// out. The readiness gate for a blue/green deploy: ryra won't swap the
    /// Caddy upstream onto a freshly started instance until its health endpoint
    /// says it's actually serving (DB up, migrations run). A timeout aborts the
    /// deploy with the old instance still live and serving.
    WaitForHttpHealthy {
        url: String,
        expect_status: u16,
        timeout_secs: u32,
    },
    /// Copy a file from the registry (or similar source) to a destination.
    /// Used for vendored binary files (e.g. Jellyfin's SSO plugin DLLs)
    /// that don't fit the templated `configs/` pipeline.
    CopyFile { src: PathBuf, dst: PathBuf },
    /// Run a build/prepare command in `dir` (e.g. `cargo build --release`,
    /// `bun install`) for a `runtime = "native"` service. Runs at apply time
    /// in the service's source dir, before the unit is (re)started.
    Build { dir: PathBuf, command: String },
    /// Mirror a source tree into `dst` (clearing `dst` first), skipping
    /// VCS/build/dependency dirs. The language-agnostic primitive behind native
    /// blue/green: each color slot gets its own isolated working copy, so a
    /// rebuild of the idle slot can't mutate source files the live slot is
    /// still reading (critical for interpreted runtimes like Python/Node).
    SyncDir { src: PathBuf, dst: PathBuf },
    /// First-time Tailscale Services setup on this tailnet: ensure ACL
    /// has `tag:ryra-host` + `tag:ryra-service` tagOwners and the
    /// services autoApprover entry, then apply `tag:ryra-host` to the
    /// local node so it's allowed to advertise services. Idempotent:
    /// reads current state via API and only writes diffs.
    TailscaleSetup,
    /// Define a Tailscale Service via the admin API and advertise it
    /// from the host: `sudo tailscale serve --service=svc:<svc_name>
    /// --https=443 http://127.0.0.1:<host_port>`. The service gets
    /// `tag:ryra-service` (matches the autoApprover) so the host's
    /// advertisement auto-approves with no manual UI clicks.
    ///
    /// `svc_name` is the part after `svc:` — already host-scoped at
    /// planning time (`<service>-<host>`) so two ryra hosts on the
    /// same tailnet can run independent copies of a service without
    /// colliding on the global Tailscale Service namespace.
    TailscaleEnable {
        svc_name: String,
        ports: Vec<TailscalePort>,
    },
    /// Stop advertising a Tailscale Service on this host and delete
    /// its definition via the admin API. Used in `ryra remove --purge`
    /// and `ryra reset` for tailscale-enabled services. `svc_name`
    /// matches the value used at install time (recovered from the
    /// stored Tailscale URL so a hostname change post-install doesn't
    /// break teardown).
    TailscaleDisable { svc_name: String },
}

impl Step {
    /// Render this step as a shell command (for dry-run display).
    pub fn to_command(&self) -> String {
        match self {
            Step::WriteFile(file) => format!("write {}", file.path.display()),
            Step::Symlink { link, target } => {
                format!("ln -sf {} {}", target.display(), link.display())
            }
            Step::DaemonReload => "systemctl --user daemon-reload".into(),
            Step::StartService { unit } => format!("systemctl --user start {unit}"),
            Step::StopService { unit } => format!("systemctl --user stop {unit}"),
            Step::RestartService { unit } => format!("systemctl --user restart {unit}"),
            Step::ReloadCaddy => {
                "podman exec caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile"
                    .into()
            }
            Step::PullImage { image } => format!("podman pull {image}"),
            Step::RemoveFile(path) => format!("rm -f {}", path.display()),
            Step::RemoveDir(path) => format!("rm -rf {}", path.display()),
            Step::CreateDir(path) => format!("mkdir -p {}", path.display()),
            Step::RemoveVolume { name } => format!("podman volume rm {name}"),
            Step::RemoveNetwork { name } => format!("podman network rm {name}"),
            Step::WaitForFile { path, timeout_secs } => {
                format!("wait for {} (up to {timeout_secs}s)", path.display())
            }
            Step::WaitForHttpHealthy {
                url,
                expect_status,
                timeout_secs,
            } => format!("wait for {url} -> {expect_status} (up to {timeout_secs}s)"),
            Step::CopyFile { src, dst } => format!("cp {} {}", src.display(), dst.display()),
            Step::Build { dir, command } => format!("(cd {} && {command})", dir.display()),
            Step::SyncDir { src, dst } => {
                format!("sync {} -> {} (skip build/VCS dirs)", src.display(), dst.display())
            }
            Step::TailscaleSetup => "tailscale: ensure ACL tags + auto-approval".to_string(),
            Step::TailscaleEnable { svc_name, ports } => ports
                .iter()
                .map(|p| {
                    format!(
                        "tailscale serve --service=svc:{svc_name} --https={} http://127.0.0.1:{}",
                        p.https_port, p.host_port
                    )
                })
                .collect::<Vec<_>>()
                .join(" && "),
            Step::TailscaleDisable { svc_name } => {
                format!("tailscale serve --service=svc:{svc_name} off + delete service")
            }
        }
    }
}

/// Warnings generated during service operations that the CLI should display.
pub enum Warning {
    /// System RAM is below the service's minimum requirement.
    RamBelowMinimum {
        service_name: String,
        min_mb: u64,
        available_mb: u64,
    },
    /// System RAM is below the service's recommended level (but above minimum).
    RamBelowRecommended {
        service_name: String,
        recommended_mb: u64,
        available_mb: u64,
    },
    /// A port was reassigned because the default was privileged or in use.
    PortReassigned {
        service_name: String,
        port_name: String,
        original_port: u16,
        assigned_port: u16,
        reason: String,
    },
    /// `--url` was passed but no ryra-managed reverse proxy (Caddy) is installed.
    /// Ryra still templates the URL into env vars and OIDC config, but routing
    /// is the user's responsibility (nginx, Cloudflare Tunnel, Tailscale Funnel,
    /// external load balancer, etc.).
    UrlWithoutReverseProxy {
        service_name: String,
        url: String,
        host_port: u16,
    },
}

pub struct AddResult {
    pub steps: Vec<Step>,
    pub warnings: Vec<Warning>,
    pub repo_url: String,
    /// Allocated ports for this service (port_name, host_port).
    pub allocated_ports: Vec<(String, u16)>,
    /// Names of auto-generated secrets (values are in .env).
    pub generated_secrets: Vec<String>,
    /// The generated .env content (for post-install processing).
    pub env_content: String,
    /// Public URL for this service (if --url was provided).
    pub url: Option<String>,
    /// Static env vars (key, default value, kind, optional human prompt
    /// label) the registry expects in `.env`. Populated whether or not
    /// the user is in interactive mode — `ryra upgrade` reads this back
    /// to decide which additions need to prompt the user (kind=Prompted
    /// / Required) versus which can be appended silently (kind=Default).
    pub tracked_envs: Vec<TrackedEnv>,
}

/// Per-env metadata the planner keeps alongside the rendered value, so
/// downstream callers (CLI prompts for `ryra upgrade`) can decide
/// whether a given env var needs user input.
#[derive(Debug, Clone)]
pub struct TrackedEnv {
    pub key: String,
    pub value: String,
    pub kind: crate::registry::service_def::EnvKind,
    pub prompt: Option<String>,
}

pub struct RemoveResult {
    pub steps: Vec<Step>,
    pub service_name: String,
    /// URL that was assigned to this service (if any).
    pub url: Option<String>,
}

pub struct ResetResult {
    pub steps: Vec<Step>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::service_def::{PortDef, PortProtocol};

    fn port(name: &str, container: u16, ts: Option<u16>) -> PortDef {
        PortDef {
            name: name.into(),
            container_port: container,
            host_port: None,
            protocol: PortProtocol::default(),
            tailscale_https: ts,
        }
    }

    #[test]
    fn single_port_service_falls_back_to_primary_on_443() {
        // No port declares tailscale_https → primary served at the web root.
        let ports = vec![port("http", 80, None)];
        let resolved = vec![("http".to_string(), 10001u16)];
        let out = tailscale_ports(&ports, &resolved, Some(10001));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].https_port, 443);
        assert_eq!(out[0].host_port, 10001);
    }

    #[test]
    fn multiport_maps_each_declared_port_to_its_resolved_host_port() {
        let ports = vec![
            port("http", 8080, Some(8080)),
            port("photos", 3000, Some(443)),
        ];
        let resolved = vec![
            ("http".to_string(), 8080u16),
            ("photos".to_string(), 10002u16),
        ];
        let mut out = tailscale_ports(&ports, &resolved, Some(8080));
        out.sort_by_key(|p| p.https_port);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].https_port, out[0].host_port), (443, 10002)); // photos root
        assert_eq!((out[1].https_port, out[1].host_port), (8080, 8080)); // museum api
    }

    #[test]
    fn no_ports_and_no_primary_yields_empty() {
        assert!(tailscale_ports(&[], &[], None).is_empty());
    }
}
