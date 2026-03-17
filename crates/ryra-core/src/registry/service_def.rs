use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A service definition from a registry's `services/<name>/service.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDef {
    pub service: ServiceMeta,
    #[serde(default)]
    pub ports: Vec<PortDef>,
    #[serde(default)]
    pub volumes: Vec<VolumeDef>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub dependencies: Vec<DependencyDef>,
    #[serde(default)]
    pub containers: Vec<ContainerDef>,
    pub nginx: Option<NginxDef>,
    #[serde(default)]
    pub mappings: Mappings,
    #[serde(default)]
    pub integrations: IntegrationFlags,
}

impl ServiceDef {
    /// Whether this service uses multiple containers (e.g., server + worker).
    pub fn is_multi_container(&self) -> bool {
        !self.containers.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMeta {
    pub name: String,
    pub description: String,
    pub image: String,
    #[serde(default)]
    pub kind: ServiceKind,
}

/// What role this service plays in the system.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    #[default]
    Application,
    Infrastructure,
}

/// Whether a port uses TCP or UDP.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
}

impl std::fmt::Display for PortProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortProtocol::Tcp => write!(f, "tcp"),
            PortProtocol::Udp => write!(f, "udp"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortDef {
    pub name: String,
    pub container_port: u16,
    #[serde(default)]
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeDef {
    pub name: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    /// If set, the CLI prompts the user with this message during `ryra add`.
    /// The `value` field serves as the default. User input replaces `value`
    /// before template rendering.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyDef {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub volumes: Vec<VolumeDef>,
    pub port: u16,
    pub url_template: Option<String>,
}

/// For multi-container services (e.g., authentik server + worker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerDef {
    pub name: String,
    pub command: Option<String>,
    #[serde(default)]
    pub ports: Vec<PortDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NginxDef {
    pub upstream_port: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mappings {
    #[serde(default)]
    pub smtp: BTreeMap<String, String>,
    #[serde(default)]
    pub auth: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationFlags {
    #[serde(default = "default_true")]
    pub auth: bool,
    #[serde(default = "default_true")]
    pub smtp: bool,
}

impl Default for IntegrationFlags {
    fn default() -> Self {
        Self {
            auth: true,
            smtp: true,
        }
    }
}

fn default_true() -> bool {
    true
}
