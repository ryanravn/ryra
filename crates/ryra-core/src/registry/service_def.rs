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
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub requires: Vec<ServiceRequirement>,
    #[serde(default)]
    pub mappings: Mappings,
    #[serde(default)]
    pub integrations: IntegrationFlags,
}

/// System resource requirements for a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirements {
    /// RAM requirements in megabytes.
    pub ram: RamRequirement,
    /// Disk requirements in gigabytes.
    #[serde(default)]
    pub disk: Option<DiskRequirement>,
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

/// Disk requirement in gigabytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskRequirement {
    /// Minimum disk in GB — container images + data must fit.
    pub min: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMeta {
    pub name: String,
    pub description: String,
    /// Optional URL to documentation or project homepage.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub kind: ServiceKind,
    /// Supported CPU architectures (e.g. ["amd64", "arm64"]).
    /// Empty means all architectures are supported.
    #[serde(default)]
    pub architecture: Vec<String>,
    /// Whether this service requires HTTPS to function.
    #[serde(default)]
    pub https: HttpsRequirement,
}

/// What role this service plays in the system.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    #[default]
    Application,
    Infrastructure,
}

/// Whether this service requires HTTPS to function.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HttpsRequirement {
    #[default]
    None,
    Required,
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
#[serde(rename_all = "snake_case")]
pub enum EnvFormat {
    /// Free-form alphanumeric string (default).
    #[default]
    String,
    /// Hexadecimal characters only.
    Hex,
    /// UUID v4.
    Uuid,
    /// HS256-signed JWT. Requires `jwt_role` and `jwt_signing_key` on the env var.
    JwtHs256,
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
    /// Length for generated secrets. Ignored for `uuid` and `jwt_hs256` formats.
    /// Defaults to 32 for `string`, 64 for `hex`.
    #[serde(default)]
    pub length: Option<u32>,
    /// JSON payload claims for `jwt_hs256` format (e.g., `{"role": "anon", "iss": "supabase"}`).
    /// `iat` and `exp` are added automatically if not present.
    #[serde(default)]
    pub jwt_claims: Option<std::collections::BTreeMap<std::string::String, serde_json::Value>>,
    /// Secret name used as the HS256 signing key (e.g., "jwt_secret"). Required for `jwt_hs256` format.
    #[serde(default)]
    pub jwt_signing_key: Option<std::string::String>,
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
}

impl std::fmt::Display for AuthKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthKind::Oidc => write!(f, "oidc"),
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

    /// Returns env var names that are required — must be provided during install.
    pub fn required_env_vars(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|e| e.kind == EnvKind::Required)
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Validate structural invariants that serde can't enforce.
    /// Called once after deserialization — if this returns Ok, the definition
    /// is safe to use without further checks.
    pub fn validate(&self) -> Result<(), String> {
        let name = &self.service.name;
        let mut errors: Vec<String> = Vec::new();

        // --- Duplicate names ---

        let mut seen_ports = std::collections::HashSet::new();
        for p in &self.ports {
            if !seen_ports.insert(&p.name) {
                errors.push(format!("duplicate port name '{}'", p.name));
            }
        }

        let mut seen_envs = std::collections::HashSet::new();
        for e in &self.env {
            if !seen_envs.insert(&e.name) {
                errors.push(format!("duplicate env var name '{}'", e.name));
            }
        }

        // --- Env var name format ---
        // Must be a valid shell variable name: starts with letter or _, contains only [A-Za-z0-9_]

        for e in &self.env {
            if e.name.is_empty() {
                errors.push("env var has empty name".to_string());
            } else if !e
                .name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                errors.push(format!(
                    "env var '{}' must start with a letter or _",
                    e.name
                ));
            } else if !e
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                errors.push(format!(
                    "env var '{}' contains invalid characters — must match [A-Za-z0-9_]",
                    e.name
                ));
            }
        }

        // --- Env kind consistency ---

        for e in &self.env {
            if e.kind == EnvKind::Required && e.value.contains("{{secret.") {
                errors.push(format!(
                    "env var '{}' is kind=required but has a secret template default — use kind=prompted or kind=default",
                    e.name
                ));
            }
        }

        // --- RAM requirements consistency ---

        if let Some(ref req) = self.requirements
            && let Some(rec) = req.ram.recommended
            && rec < req.ram.min
        {
            errors.push(format!(
                "recommended RAM ({rec}MB) is less than minimum ({}MB)",
                req.ram.min
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("{name}: {}", errors.join("; ")))
        }
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
