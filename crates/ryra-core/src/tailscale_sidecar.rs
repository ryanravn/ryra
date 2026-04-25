//! Per-service Tailscale sidecar quadlet generator.
//!
//! When a service is installed with `--tailscale`, ryra generates a
//! companion `ts-<service>.container` quadlet running `tailscale/tailscale`
//! that joins the user's tailnet as its own device. The actual service
//! container shares the sidecar's network namespace
//! (`Network=container:ts-<service>`), so:
//!
//! - The service container has no public ports of its own (no
//!   `PublishPort=`), only its `container_port` listening in the shared
//!   netns.
//! - tailscaled in the sidecar handles all inbound traffic from the
//!   tailnet, terminating TLS via `tailscale serve` with a
//!   publicly-trusted Let's Encrypt cert.
//! - From any tailnet device, `https://<service>.<tailnet>.ts.net/` reaches
//!   tailscale serve → localhost:<container_port> → the service.
//! - Other ryra-managed services on the same tailnet reach this one
//!   natively via MagicDNS — no auth_bridge cert mounting or alias trick
//!   needed (system CAs trust tailscale's cert, and each container is
//!   itself a tailnet node so it can resolve sibling FQDNs).
//!
//! The sidecar's `tailscale serve` config is set up by an
//! `ExecStartPost=` shell loop that polls until tailscaled has finished
//! authenticating, then issues `tailscale serve --bg --https=443 …`
//! once. Persistent state is held in a per-service podman volume
//! (`ts-<service>-state.volume`) mounted at `/var/lib/tailscale`, so
//! restarts don't re-register the device.

use std::path::Path;

use crate::generate::GeneratedFile;

/// Generated quadlet artifacts for one tailscale sidecar.
pub struct SidecarBundle {
    /// `ts-<service>.container` — the sidecar tailscaled.
    pub container_quadlet: GeneratedFile,
    /// `ts-<service>-state.volume` — declares the named podman volume
    /// that holds tailscaled state. Empty `[Volume]` section is enough;
    /// quadlet's auto-generation creates the volume on first start.
    pub state_volume_quadlet: GeneratedFile,
}

/// Build the sidecar quadlet bundle for `service` listening on
/// `container_port` inside its container, joining the tailnet with
/// `auth_key`.
///
/// The quadlets get written to the same directory as the service's own
/// quadlets (typically `~/.config/containers/systemd/`). systemd's
/// generator notices the `[Install]` section and registers the units
/// on the next `daemon-reload`.
pub fn build(
    service: &str,
    container_port: u16,
    auth_key: &str,
    quadlet_dir: &Path,
) -> SidecarBundle {
    SidecarBundle {
        container_quadlet: GeneratedFile {
            path: quadlet_dir.join(format!("ts-{service}.container")),
            content: render_container_quadlet(service, container_port, auth_key),
        },
        state_volume_quadlet: GeneratedFile {
            path: quadlet_dir.join(format!("ts-{service}-state.volume")),
            content: render_state_volume_quadlet(),
        },
    }
}

fn render_container_quadlet(service: &str, container_port: u16, auth_key: &str) -> String {
    // `?ephemeral=false` overrides whatever ephemerality the auth key
    // (or OAuth client) was minted with — ryra-managed services should
    // persist across container restarts, not re-register on every boot.
    //
    // `TS_USERSPACE=true` runs tailscaled with userspace networking
    // (no TUN device), which is what rootless podman needs since
    // CAP_NET_ADMIN isn't typically available there.
    //
    // The `ExecStartPost` poll loop runs on the host (not inside the
    // container — it's a `[Service]` directive). It calls
    // `podman exec` to issue `tailscale serve` once tailscaled has
    // authenticated. ~2 minutes of patience covers slow auth on the
    // first run; subsequent restarts hit the cached state immediately.
    format!(
        "[Unit]\n\
         Description=Tailscale sidecar for {service}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Container]\n\
         ContainerName=ts-{service}\n\
         Image=docker.io/tailscale/tailscale:stable\n\
         Volume=ts-{service}-state.volume:/var/lib/tailscale:U\n\
         Environment=TS_AUTHKEY={auth_key}?ephemeral=false\n\
         Environment=TS_HOSTNAME={service}\n\
         Environment=TS_STATE_DIR=/var/lib/tailscale\n\
         Environment=TS_USERSPACE=true\n\
         Environment=TS_EXTRA_ARGS=--ssh=false\n\
         HealthCmd=tailscale status --peers=false --self=true\n\
         HealthStartPeriod=30s\n\
         HealthInterval=30s\n\
         HealthRetries=3\n\
         \n\
         [Service]\n\
         Restart=always\n\
         RestartSec=5\n\
         TimeoutStartSec=180\n\
         ExecStartPost=/bin/bash -c 'for i in $(seq 1 60); do \
            if podman exec ts-{service} tailscale status --peers=false >/dev/null 2>&1; then \
              podman exec ts-{service} tailscale serve --bg --https=443 http://localhost:{container_port}; \
              exit 0; \
            fi; \
            sleep 2; \
         done; exit 1'\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn render_state_volume_quadlet() -> String {
    // Empty `[Volume]` body is the canonical "create a podman named
    // volume" quadlet. Quadlet auto-prefixes the resulting podman
    // volume name with `systemd-`, matching how ryra's other named
    // volumes work.
    "[Volume]\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sidecar_quadlet_contains_required_directives() {
        let dir = PathBuf::from("/tmp/test");
        let bundle = build("seafile", 8000, "tskey-auth-XXX", &dir);

        let c = &bundle.container_quadlet.content;
        // Image, name, volume — the basics
        assert!(c.contains("Image=docker.io/tailscale/tailscale:stable"));
        assert!(c.contains("ContainerName=ts-seafile"));
        assert!(c.contains("Volume=ts-seafile-state.volume:/var/lib/tailscale:U"));
        // Auth + identity
        assert!(c.contains("TS_AUTHKEY=tskey-auth-XXX?ephemeral=false"));
        assert!(c.contains("TS_HOSTNAME=seafile"));
        // Userspace mode (rootless-compatible)
        assert!(c.contains("TS_USERSPACE=true"));
        // Serve config issued post-start to localhost:<container_port>
        assert!(c.contains("podman exec ts-seafile tailscale serve --bg --https=443 http://localhost:8000"));
        // systemd integration
        assert!(c.contains("[Install]"));
        assert!(c.contains("WantedBy=default.target"));
    }

    #[test]
    fn sidecar_quadlet_paths_use_service_name() {
        let dir = PathBuf::from("/etc/test/systemd");
        let bundle = build("forgejo", 3000, "tskey-auth-Y", &dir);
        assert_eq!(
            bundle.container_quadlet.path,
            PathBuf::from("/etc/test/systemd/ts-forgejo.container")
        );
        assert_eq!(
            bundle.state_volume_quadlet.path,
            PathBuf::from("/etc/test/systemd/ts-forgejo-state.volume")
        );
    }

    #[test]
    fn state_volume_is_minimal() {
        let bundle = build("svc", 80, "k", &PathBuf::from("/x"));
        // The empty `[Volume]` is the canonical "auto-create" form;
        // anything more would risk overriding podman's default name
        // mangling.
        assert_eq!(bundle.state_volume_quadlet.content, "[Volume]\n");
    }
}
