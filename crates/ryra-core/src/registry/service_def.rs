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
    #[serde(default)]
    pub tests: Vec<TestDef>,
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

/// A test defined in a service's `[[tests]]` section or a multi-service test file.
///
/// Tests are shell commands — exit 0 = pass, anything else = fail.
/// Env vars from the service's `.env` are available in the command.
///
/// For single-service tests (inside `service.toml`), env vars are unprefixed.
/// For multi-service tests (in `tests/*.toml`), env vars are prefixed
/// with the service name: `WHOAMI__RYRA_PORT_HTTP`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestDef {
    pub name: String,
    /// Shell command to run inside the VM.
    pub run: String,
    /// Timeout in seconds (default: 30).
    #[serde(default = "default_test_timeout")]
    pub timeout: u64,
    /// Env var overrides for this test. Used to provide values for required
    /// env vars that have no default (e.g. `GITEA_DOMAIN = "localhost"`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_test_timeout() -> u64 {
    30
}

/// A multi-service test file from the `tests/` directory in a registry.
///
/// Deploys multiple services, then runs tests with env vars from all
/// services prefixed by service name (`WHOAMI__RYRA_PORT_HTTP`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiServiceTestDef {
    pub test: MultiServiceTestMeta,
    #[serde(default)]
    pub tests: Vec<TestDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiServiceTestMeta {
    pub name: String,
    pub services: Vec<String>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl ServiceDef {
    /// Returns env var names that are required (have a prompt but no default value).
    pub fn required_env_vars(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|e| e.prompt.is_some() && e.value.is_empty())
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Validate that all tests provide values for required env vars.
    ///
    /// Returns an error listing which tests are missing which vars.
    pub fn validate_tests(&self) -> std::result::Result<(), Vec<String>> {
        let required = self.required_env_vars();
        if required.is_empty() || self.tests.is_empty() {
            return Ok(());
        }

        let mut errors = Vec::new();
        for test in &self.tests {
            for var in &required {
                if !test.env.contains_key(*var) {
                    errors.push(format!(
                        "test '{}' missing required env var '{}'",
                        test.name, var
                    ));
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
