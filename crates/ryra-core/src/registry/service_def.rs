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
    /// Sidecar containers for multi-container services.
    #[serde(default)]
    pub containers: Vec<ContainerDef>,
    #[serde(default)]
    pub mappings: Mappings,
    #[serde(default)]
    pub integrations: IntegrationFlags,
    /// Commands that run on the host before the service container starts.
    /// The service's .env is sourced and data dir exists at this point.
    #[serde(default)]
    pub pre_start: Vec<PostStartHookDef>,
    /// Commands that run on the host after the service is started and ports are reachable.
    /// Useful for services that need config injection into files created at runtime
    /// (e.g. Seafile writes OAuth config to seahub_settings.py after bootstrap).
    /// The service's .env is sourced before each hook runs.
    #[serde(default)]
    pub post_start: Vec<PostStartHookDef>,
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

/// A sidecar container that runs alongside the primary container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerDef {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub command: Option<String>,
    /// References to top-level [[volumes]] this sidecar also mounts.
    #[serde(default)]
    pub volumes: Vec<VolumeDef>,
    /// Whether this container reads the shared .env file.
    #[serde(default = "default_true")]
    pub env_file: bool,
    /// Names of other containers this one depends on (maps to After=/Requires=).
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub healthcheck: Option<HealthcheckDef>,
    /// If true, this is an init container — runs once, must exit 0 before
    /// dependent containers start. Maps to Type=oneshot + RemainAfterExit=yes.
    #[serde(default)]
    pub init: bool,
}

/// Healthcheck configuration for a container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckDef {
    pub command: String,
    /// Start period in seconds.
    #[serde(default)]
    pub start_period: Option<u32>,
    /// Interval in seconds.
    #[serde(default)]
    pub interval: Option<u32>,
    /// Number of retries.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Timeout in seconds.
    #[serde(default)]
    pub timeout: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMeta {
    pub name: String,
    pub description: String,
    /// Optional URL to documentation or project homepage.
    #[serde(default)]
    pub url: Option<String>,
    /// Container image for the primary container.
    pub image: String,
    /// Override the container CMD (maps to quadlet `Exec=`).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub kind: ServiceKind,
    /// Supported CPU architectures (e.g. ["amd64", "arm64"]).
    /// Empty means all architectures are supported.
    #[serde(default)]
    pub architecture: Vec<String>,
    /// Optional message shown after install (e.g. pairing instructions).
    #[serde(default)]
    pub note: Option<String>,
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
    /// Fixed host port (for privileged services like Caddy that need specific ports).
    /// If not set, ryra allocates a port dynamically.
    #[serde(default)]
    pub host_port: Option<u16>,
    #[serde(default)]
    pub protocol: PortProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeDef {
    pub name: String,
    pub mount_path: String,
    /// If set, this is a bind mount from a path relative to the service home dir.
    /// The volume name is used for identification only.
    #[serde(default)]
    pub host_path: Option<String>,
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
/// References separately-installed ryra services whose env vars
/// and ports can be referenced via `{{services.<name>.*}}` templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequirement {
    pub service: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mappings {
    #[serde(default)]
    pub smtp: BTreeMap<String, String>,
    #[serde(default)]
    pub auth: BTreeMap<String, String>,
}

/// What kind of auth integration a service supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    /// Service handles OIDC auth itself (e.g. affine, forgejo).
    Oidc,
    /// Auth is handled by the reverse proxy in front of the service (e.g. whoami).
    ForwardAuth,
}

impl std::fmt::Display for AuthKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthKind::Oidc => write!(f, "oidc"),
            AuthKind::ForwardAuth => write!(f, "forward-auth"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationFlags {
    /// Auth types this service supports. Empty = no auth support.
    #[serde(default)]
    pub auth: Vec<AuthKind>,
    #[serde(default = "default_true")]
    pub smtp: bool,
}

impl Default for IntegrationFlags {
    fn default() -> Self {
        Self {
            auth: vec![],
            smtp: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_hook_timeout() -> u32 {
    300
}

/// A command that runs on the host after the service is started.
///
/// The service's `.env` file is sourced before the command runs, so all
/// env vars (including auth mappings like `OAUTH_CLIENT_ID`) are available.
/// The command runs as root with the service's home dir as working directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostStartHookDef {
    pub name: String,
    /// Shell command to run on the host.
    pub run: String,
    /// Timeout in seconds (default: 300). Must be 1–3600.
    #[serde(default = "default_hook_timeout")]
    pub timeout: u32,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl ServiceDef {
    /// Check if this service supports the current system architecture.
    /// Returns None if supported (or no restriction), Some(error) if not.
    pub fn check_architecture(&self) -> Option<String> {
        if self.service.architecture.is_empty() {
            return None;
        }
        let current = current_architecture();
        if self.service.architecture.iter().any(|a| a == current) {
            None
        } else {
            Some(format!(
                "{} only supports {} — this system is {current}",
                self.service.name,
                self.service.architecture.join(", "),
            ))
        }
    }

    /// All container images needed by this service (primary + sidecars), deduplicated.
    pub fn all_images(&self) -> Vec<&str> {
        let mut images = vec![self.service.image.as_str()];
        for c in &self.containers {
            if !images.contains(&c.image.as_str()) {
                images.push(&c.image);
            }
        }
        images
    }

    /// Returns env var names that are required — must be provided during install.
    pub fn required_env_vars(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|e| e.kind == EnvKind::Required)
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Validate hook timeouts are within reasonable bounds (1–3600 seconds).
    pub fn validate_hooks(&self) -> Result<(), String> {
        for hook in self.pre_start.iter().chain(self.post_start.iter()) {
            if hook.timeout == 0 || hook.timeout > 3600 {
                return Err(format!(
                    "hook '{}' has timeout {} — must be 1–3600 seconds",
                    hook.name, hook.timeout,
                ));
            }
        }
        Ok(())
    }
}

/// Detect the current system architecture using OCI/Docker naming conventions.
pub fn current_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}
