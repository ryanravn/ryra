use crate::registry::service_def::EnvVar;

/// All the pieces needed to generate quadlet files for one container.
pub struct QuadletParams<'a> {
    pub service_name: &'a str,
    pub image: &'a str,
    pub env_vars: &'a [EnvVar],
    pub ports: &'a [PortMapping],
    pub volumes: &'a [VolumeMapping<'a>],
    pub network: &'a str,
    pub command: Option<&'a str>,
}

pub struct PortMapping {
    pub host_port: u16,
    pub container_port: u16,
}

pub struct VolumeMapping<'a> {
    pub volume_name: &'a str,
    pub mount_path: &'a str,
}

/// Render a .container quadlet unit file.
pub fn render_container(params: &QuadletParams) -> String {
    let mut lines = Vec::new();

    lines.push("[Unit]".to_string());
    lines.push(format!(
        "Description=Ryra service: {}",
        params.service_name
    ));
    lines.push(String::new());

    lines.push("[Container]".to_string());
    lines.push(format!("Image={}", params.image));
    lines.push(format!("Network={}.network", params.network));

    if let Some(cmd) = params.command {
        lines.push(format!("Exec={cmd}"));
    }

    for env in params.env_vars {
        lines.push(format!("Environment={}={}", env.name, env.value));
    }

    for port in params.ports {
        lines.push(format!(
            "PublishPort=127.0.0.1:{}:{}",
            port.host_port, port.container_port
        ));
    }

    for vol in params.volumes {
        lines.push(format!("Volume={}.volume:{}",vol.volume_name, vol.mount_path));
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
         Description=Ryra network: {name}\n\
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
         Description=Ryra volume: {name}\n\
         \n\
         [Volume]\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}
