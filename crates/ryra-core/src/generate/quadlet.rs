use crate::registry::service_def::PortProtocol;

/// All the pieces needed to generate quadlet files for one container.
pub struct QuadletParams<'a> {
    pub service_name: &'a str,
    pub image: &'a str,
    pub ports: &'a [PortMapping],
    pub volumes: &'a [VolumeMapping],
    pub network: &'a str,
    pub command: Option<&'a str>,
    /// Systemd units this container depends on (After=/Requires=).
    pub depends_on: &'a [String],
    /// Healthcheck configuration.
    pub healthcheck: Option<&'a crate::registry::service_def::HealthcheckDef>,
    /// Absolute path to the .env file for this service, or None to skip.
    pub env_file: Option<&'a str>,
    /// Override the container name (used for DNS on shared network).
    /// If None, podman uses the quadlet filename stem.
    pub container_name: Option<&'a str>,
    /// If true, this is an init container (Type=oneshot, RemainAfterExit=yes).
    pub init: bool,
    /// Extra host entries to add (e.g., auth domain → host IP).
    pub add_hosts: &'a [(String, String)],
    /// Extra volume mounts (e.g., Caddy root CA cert).
    pub extra_volumes: &'a [String],
    /// Additional networks to join (e.g., caddy's network for reverse proxy).
    /// Format: network name, with optional alias (e.g., "caddy" or "caddy:alias=auth.test.local").
    pub extra_networks: &'a [String],
}

pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

pub enum VolumeMapping {
    /// Named volume managed by podman (references a .volume quadlet unit).
    Named {
        volume_name: String,
        mount_path: String,
    },
    /// Bind mount from a host path into the container.
    Bind {
        host_path: String,
        mount_path: String,
    },
}

/// Render a .container quadlet unit file.
/// Env vars come from EnvironmentFile=%h/.env, not inline.
pub fn render_container(params: &QuadletParams) -> String {
    let mut lines = Vec::new();

    lines.push("[Unit]".to_string());
    lines.push(format!("Description={}", params.service_name));
    // Never give up restarting — daemon-reload from adding other services
    // can cause transient failures (e.g., postgres connection drops).
    lines.push("StartLimitIntervalSec=0".to_string());

    for dep in params.depends_on {
        lines.push(format!("After={dep}.service"));
        lines.push(format!("Requires={dep}.service"));
    }

    lines.push(String::new());

    lines.push("[Container]".to_string());
    lines.push(format!("Image={}", params.image));
    if let Some(name) = params.container_name {
        lines.push(format!("ContainerName={name}"));
    }
    lines.push(format!("Network={}.network", params.network));
    for net in params.extra_networks {
        // Support "caddy" (plain) or "caddy:alias=foo.local" (with options)
        if let Some((name, opts)) = net.split_once(':') {
            lines.push(format!("Network={name}.network:{opts}"));
        } else {
            lines.push(format!("Network={net}.network"));
        }
    }

    for (host, ip) in params.add_hosts {
        lines.push(format!("AddHost={host}:{ip}"));
    }

    for vol in params.extra_volumes {
        lines.push(format!("Volume={vol}"));
    }

    if let Some(env_path) = params.env_file {
        lines.push(format!("EnvironmentFile={env_path}"));
    }

    if let Some(cmd) = params.command {
        lines.push(format!("Exec={cmd}"));
    }

    for port in params.ports {
        let proto_suffix = match port.protocol {
            PortProtocol::Tcp => String::new(),
            PortProtocol::Udp => "/udp".to_string(),
        };
        lines.push(format!(
            "PublishPort={}:{}{proto_suffix}",
            port.host_port, port.container_port
        ));
    }

    for vol in params.volumes {
        match vol {
            VolumeMapping::Named {
                volume_name,
                mount_path,
            } => {
                lines.push(format!("Volume={volume_name}.volume:{mount_path}:U"));
            }
            VolumeMapping::Bind {
                host_path,
                mount_path,
            } => {
                // Bind mounts use :Z (SELinux relabel) not :U (user remap).
                // :U changes ownership which breaks when container UID != host UID.
                lines.push(format!("Volume={host_path}:{mount_path}:Z"));
            }
        }
    }

    if let Some(hc) = params.healthcheck {
        lines.push(format!("HealthCmd=CMD-SHELL {}", hc.command));
        if let Some(sp) = hc.start_period {
            lines.push(format!("HealthStartPeriod={sp}s"));
        }
        if let Some(iv) = hc.interval {
            lines.push(format!("HealthInterval={iv}s"));
        }
        if let Some(r) = hc.retries {
            lines.push(format!("HealthRetries={r}"));
        }
        if let Some(t) = hc.timeout {
            lines.push(format!("HealthTimeout={t}s"));
        }
    }

    // Skip redundant pulls — ryra pre-pulls images before starting services
    lines.push("Pull=never".to_string());

    lines.push(String::new());
    lines.push("[Service]".to_string());
    if params.init {
        lines.push("Type=oneshot".to_string());
        lines.push("RemainAfterExit=yes".to_string());
    } else {
        lines.push("Restart=always".to_string());
        lines.push("RestartSec=5".to_string());
    }
    // Image pulls are handled by ryra, but first boot may still be slow
    lines.push("TimeoutStartSec=300".to_string());

    lines.push(String::new());
    lines.push("[Install]".to_string());
    lines.push("WantedBy=default.target".to_string());

    lines.join("\n") + "\n"
}

/// Render a .network quadlet unit file.
/// No [Install] section — networks are pulled in by container dependencies.
pub fn render_network(name: &str) -> String {
    format!(
        "[Unit]\n\
         Description={name} network\n\
         \n\
         [Network]\n"
    )
}

/// Render a .volume quadlet unit file.
/// No [Install] section — volumes are pulled in by container dependencies.
pub fn render_volume(name: &str) -> String {
    format!(
        "[Unit]\n\
         Description={name} volume\n\
         \n\
         [Volume]\n"
    )
}
