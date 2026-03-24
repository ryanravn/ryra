use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A service definition from a registry's `services/<name>/service.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDef {
    pub service: ServiceMeta,
    #[serde(default)]
    pub requirements: Option<Requirements>,
    #[serde(default)]
    pub ports: Vec<PortDef>,
    #[serde(default)]
    pub volumes: Vec<VolumeDef>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub requires: Vec<ServiceRequirement>,
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

/// System resource requirements for a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirements {
    /// RAM requirements in megabytes.
    pub ram: RamRequirement,
}

/// RAM requirement with minimum and recommended thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RamRequirement {
    /// Minimum RAM in MB — service may fail below this.
    pub min: u64,
    /// Recommended RAM in MB — service will run well at this level.
    #[serde(default)]
    pub recommended: Option<u64>,
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
    Quadlet {
        image: String,
        /// Override the container CMD (maps to quadlet `Exec=`).
        #[serde(default)]
        command: Option<String>,
    },
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
    #[serde(default)]
    command: Option<String>,
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
                DeployMode::Quadlet { image, command: raw.command }
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

/// How an env var is presented to the user during `ryra add`.
///
/// - `default`: static value or template (e.g. `{{secret.password}}`),
///   not prompted — user can edit `.env` manually after install
/// - `prompted`: shown during `ryra add` with a default value — optional
///   but visible (e.g. API keys that can be left empty)
/// - `required`: must be provided during `ryra add` — no usable default,
///   blocks install if not provided. Tests must supply these via `env` overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvKind {
    /// Not prompted. Value is used as-is (may contain templates like `{{secret.*}}`).
    #[default]
    Default,
    /// Prompted during `ryra add` with a default. User can accept or change.
    Prompted,
    /// Must be provided. No usable default — fails in non-interactive mode
    /// unless supplied via env overrides.
    Required,
}

/// Format of an env var's value — used for secret generation and input validation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvFormat {
    /// Free-form alphanumeric string (default).
    #[default]
    String,
    /// Hexadecimal characters only.
    Hex,
    /// UUID v4.
    Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub kind: EnvKind,
    /// Prompt message shown during `ryra add` (for `prompted` and `required` kinds).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Value format — used to generate secrets and validate user input.
    #[serde(default)]
    pub format: EnvFormat,
    /// Length for generated secrets. Ignored for `uuid` format.
    /// Defaults to 32 for `string`, 64 for `hex`.
    #[serde(default)]
    pub length: Option<u32>,
}

/// A service that must already be installed on the system before this one.
///
/// Unlike `DependencyDef` (sidecar containers bundled with the service),
/// `requires` references separately-installed ryra services whose env vars
/// and ports can be referenced via `{{services.<name>.*}}` templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequirement {
    pub service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyDef {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub volumes: Vec<VolumeDef>,
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
    /// Returns env var names that are required — must be provided during install.
    pub fn required_env_vars(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|e| e.kind == EnvKind::Required)
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
