//! Typed steps the CLI executes, the warnings it surfaces, and the result
//! shapes returned from `add` / `remove` / `reset`. Pattern matching ensures
//! every step type is handled — no string parsing or if-chains.

use std::path::PathBuf;

use crate::generate::GeneratedFile;

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
    /// Create a directory (with parents).
    CreateDir(PathBuf),
    /// Wait for a file to appear (with timeout).
    WaitForFile { path: PathBuf, timeout_secs: u32 },
    /// Copy a file from the registry (or similar source) to a destination.
    /// Used for vendored binary files (e.g. Jellyfin's SSO plugin DLLs)
    /// that don't fit the templated `configs/` pipeline.
    CopyFile { src: PathBuf, dst: PathBuf },
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
    TailscaleEnable { svc_name: String, host_port: u16 },
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
            Step::WaitForFile { path, timeout_secs } => {
                format!("wait for {} (up to {timeout_secs}s)", path.display())
            }
            Step::CopyFile { src, dst } => format!("cp {} {}", src.display(), dst.display()),
            Step::TailscaleSetup => "tailscale: ensure ACL tags + auto-approval".to_string(),
            Step::TailscaleEnable {
                svc_name,
                host_port,
            } => format!(
                "tailscale serve --service=svc:{svc_name} --https=443 http://127.0.0.1:{host_port}"
            ),
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
    /// `--url` was passed but no bundled reverse proxy (Caddy) is installed.
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
