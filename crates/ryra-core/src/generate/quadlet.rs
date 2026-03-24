use crate::registry::service_def::PortProtocol;

/// Whether a container port binds to localhost or all interfaces.
pub enum BindAddress {
    /// 127.0.0.1 — only reachable from the same host.
    Localhost,
    /// 0.0.0.0 — reachable from the network.
    Any,
}

/// All the pieces needed to generate quadlet files for one container.
pub struct QuadletParams<'a> {
    pub service_name: &'a str,
    pub image: &'a str,
    pub ports: &'a [PortMapping],
    pub volumes: &'a [VolumeMapping<'a>],
    pub network: &'a str,
    pub command: Option<&'a str>,
    pub bind_address: &'a BindAddress,
    /// Dependency unit names that must start before this container.
    pub requires: &'a [String],
}

pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: PortProtocol,
}

pub struct VolumeMapping<'a> {
    pub volume_name: &'a str,
    pub mount_path: &'a str,
}

/// Render a .container quadlet unit file.
/// Env vars come from EnvironmentFile=%h/.env, not inline.
pub fn render_container(params: &QuadletParams) -> String {
    let mut lines = Vec::new();

    lines.push("[Unit]".to_string());
    lines.push(format!("Description={}", params.service_name));
    for dep_unit in params.requires {
        lines.push(format!("Requires={dep_unit}.service"));
        lines.push(format!("After={dep_unit}.service"));
    }
    lines.push(String::new());

    lines.push("[Container]".to_string());
    lines.push(format!("Image={}", params.image));
    lines.push(format!("Network={}.network", params.network));
    lines.push("EnvironmentFile=%h/.env".to_string());

    if let Some(cmd) = params.command {
        lines.push(format!("Exec={cmd}"));
    }

    let bind_ip = match params.bind_address {
        BindAddress::Localhost => "127.0.0.1",
        BindAddress::Any => "0.0.0.0",
    };

    for port in params.ports {
        let proto_suffix = match port.protocol {
            PortProtocol::Tcp => String::new(),
            PortProtocol::Udp => "/udp".to_string(),
        };
        lines.push(format!(
            "PublishPort={bind_ip}:{}:{}{proto_suffix}",
            port.host_port, port.container_port
        ));
    }

    for vol in params.volumes {
        lines.push(format!(
            "Volume={}.volume:{}",
            vol.volume_name, vol.mount_path
        ));
    }

    lines.push(String::new());
    lines.push("[Service]".to_string());
    lines.push("Restart=always".to_string());
    lines.push("TimeoutStartSec=300".to_string());

    lines.push(String::new());
    lines.push("[Install]".to_string());
    lines.push("WantedBy=default.target".to_string());

    lines.join("\n") + "\n"
}

/// Parameters for a dependency (sidecar) container — no published ports.
pub struct DependencyQuadletParams<'a> {
    /// The main service name (used for description and network).
    pub service_name: &'a str,
    /// The dependency name (e.g., "postgres").
    pub dep_name: &'a str,
    pub image: &'a str,
    pub volumes: &'a [VolumeMapping<'a>],
    pub network: &'a str,
}

/// Render a .container quadlet for a dependency sidecar.
/// No ports are published — the container is only reachable via the shared podman network.
/// Container name is `{service_name}-{dep_name}`, which becomes the DNS hostname on the network.
pub fn render_dependency_container(params: &DependencyQuadletParams) -> String {
    let container_name = format!("{}-{}", params.service_name, params.dep_name);
    let mut lines = Vec::new();

    lines.push("[Unit]".to_string());
    lines.push(format!(
        "Description={} dependency for {}",
        params.dep_name, params.service_name
    ));
    lines.push(String::new());

    lines.push("[Container]".to_string());
    lines.push(format!("Image={}", params.image));
    lines.push(format!("ContainerName={container_name}"));
    lines.push(format!("Network={}.network", params.network));
    lines.push(format!("EnvironmentFile=%h/.env.{}", params.dep_name));

    for vol in params.volumes {
        lines.push(format!(
            "Volume={}.volume:{}",
            vol.volume_name, vol.mount_path
        ));
    }

    lines.push(String::new());
    lines.push("[Service]".to_string());
    lines.push("Restart=always".to_string());
    lines.push("TimeoutStartSec=300".to_string());

    lines.push(String::new());
    lines.push("[Install]".to_string());
    lines.push("WantedBy=default.target".to_string());

    lines.join("\n") + "\n"
}

/// Render a .network quadlet unit file.
pub fn render_network(name: &str) -> String {
    format!(
        "[Unit]\n\
         Description={name} network\n\
         \n\
         [Network]\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Render a .volume quadlet unit file.
pub fn render_volume(name: &str) -> String {
    format!(
        "[Unit]\n\
         Description={name} volume\n\
         \n\
         [Volume]\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}
