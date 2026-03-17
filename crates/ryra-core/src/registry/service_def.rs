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

/// How a service is deployed: single container via quadlet, or multi-container via compose.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum DeployMode {
    /// Single container managed via podman quadlet.
    Quadlet { image: String },
    /// Multi-service stack managed via podman compose.
    Compose {
        /// Path to compose file relative to the service directory in the registry.
        file: String,
        /// Named profiles: alternative compose files for different configurations.
        #[serde(default)]
        profiles: Vec<ComposeProfile>,
    },
}

impl DeployMode {
    pub fn is_compose(&self) -> bool {
        matches!(self, DeployMode::Compose { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeProfile {
    pub name: String,
    pub description: String,
    pub file: String,
}

/// Raw helper for deserializing ServiceMeta — defaults mode to "quadlet" when absent.
#[derive(Deserialize)]
struct ServiceMetaRaw {
    name: String,
    description: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default = "default_quadlet_mode")]
    mode: String,
    image: Option<String>,
    file: Option<String>,
    #[serde(default)]
    profiles: Vec<ComposeProfile>,
    #[serde(default)]
    kind: ServiceKind,
}

fn default_quadlet_mode() -> String {
    "quadlet".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceMeta {
    pub name: String,
    pub description: String,
    /// Optional URL to documentation or project homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(flatten)]
    pub deploy: DeployMode,
    #[serde(default)]
    pub kind: ServiceKind,
}

impl<'de> Deserialize<'de> for ServiceMeta {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = ServiceMetaRaw::deserialize(deserializer)?;
        let deploy = match raw.mode.as_str() {
            "quadlet" => {
                let image = raw.image.ok_or_else(|| {
                    serde::de::Error::missing_field("image")
                })?;
                DeployMode::Quadlet { image }
            }
            "compose" => {
                let file = raw.file.ok_or_else(|| {
                    serde::de::Error::missing_field("file")
                })?;
                DeployMode::Compose {
                    file,
                    profiles: raw.profiles,
                }
            }
            other => {
                return Err(serde::de::Error::unknown_variant(
                    other,
                    &["quadlet", "compose"],
                ));
            }
        };
        Ok(ServiceMeta {
            name: raw.name,
            description: raw.description,
            url: raw.url,
            deploy,
            kind: raw.kind,
        })
    }
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
