use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capability::Capability;

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
    /// Optional, user-toggled bundles of env vars. A group is either fully
    /// enabled (every member lands in `.env`) or fully disabled (none do) —
    /// makes "client_id without client_secret" unrepresentable.
    #[serde(default, rename = "env_group")]
    pub env_groups: Vec<EnvGroup>,
    #[serde(default)]
    pub requires: Vec<ServiceRequirement>,
    #[serde(default)]
    pub mappings: Mappings,
    #[serde(default)]
    pub integrations: IntegrationFlags,
    /// Roles this service can play for *other* services. The dual of
    /// [`IntegrationFlags`] (which describes what this service consumes).
    /// Drives capability-based dispatch — see [`crate::capability`].
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Backup configuration. Present only when the author has declared
    /// `backup = true` in `[integrations]` and the service needs more
    /// than the default "back up everything classified as data."
    /// Carries hooks (pre/post dump) and exclude lists.
    #[serde(default)]
    pub backup: Option<BackupConfig>,
    /// Prometheus-style metrics endpoint this service exposes. When set
    /// and a metrics-store provider is installed, ryra writes a file_sd
    /// scrape target and joins the service to the store's network.
    #[serde(default)]
    pub metrics: Option<MetricsDef>,
}

/// Where a service serves Prometheus-style metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsDef {
    /// Name of the `[[ports]]` entry the metrics endpoint listens on.
    /// The scrape target uses that entry's *container* port — the store
    /// reaches the service over the shared podman network, not the host.
    pub port: String,
    /// HTTP path of the endpoint.
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

/// Capability declarations on a service.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// Capabilities this service offers to other services.
    #[serde(default)]
    pub provides: Vec<Capability>,
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
    pub architecture: Vec<Arch>,
    /// Whether this service requires HTTPS to function.
    #[serde(default)]
    pub https: HttpsRequirement,
    /// How this service runs: a podman container (default) or a native process
    /// under systemd --user.
    #[serde(default)]
    pub runtime: Runtime,
    /// `runtime = "native"` only: the command ryra runs as the service (the
    /// unit's `ExecStart`), executed in the service's source dir. A binary
    /// (`target/release/app`), an interpreter (`bun run src/index.ts`), or a
    /// watcher (`bun --watch run …`) for save-and-reload. Required for native,
    /// forbidden for podman (enforced in `validate()`).
    #[serde(default)]
    pub run: Option<String>,
    /// `runtime = "native"` only: optional command run in the source dir before
    /// the service starts and on every `ryra upgrade` (e.g. `cargo build
    /// --release`, `bun install`). Omit when `run` needs no build step.
    #[serde(default)]
    pub build: Option<String>,
}

/// What role this service plays in the system.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    #[default]
    Application,
    Infrastructure,
}

/// How a service is realized on the host.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// A rootless podman container via a quadlet (`Image=`). The default, and
    /// what every catalog service uses.
    #[default]
    Podman,
    /// A process run directly under `systemd --user`, no container. ryra runs
    /// the service's `run` command in its source dir (after the optional
    /// `build` step), with the same port/data/env contract a container gets.
    Native,
}

impl Runtime {
    /// Whether this is the default podman runtime. Used as a serde
    /// `skip_serializing_if` so podman installs don't carry a redundant
    /// `runtime = "podman"` in their metadata.
    pub fn is_podman(&self) -> bool {
        matches!(self, Runtime::Podman)
    }
}

/// CPU architecture for container images.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    Amd64,
    Arm64,
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Arch::Amd64 => write!(f, "amd64"),
            Arch::Arm64 => write!(f, "arm64"),
        }
    }
}

/// Whether this service requires HTTPS to function.
///
/// Declarative, per-service. No magic derivation from other fields — a
/// service that needs HTTPS must say so explicitly.
///
/// - `Never` (default): HTTP is fine. Per RFC 8252 loopback redirect URIs
///   (`http://127.0.0.1`, `http://localhost`) are valid OIDC callbacks, so
///   most services work over plain HTTP even with `--auth`.
/// - `Auth`: HTTPS required when `--auth` is used. For services whose OIDC
///   implementation rejects plain-HTTP even on loopback (e.g. nextcloud's
///   `user_oidc` refuses to render the SSO button over HTTP).
/// - `Always`: HTTPS required regardless of flags. For services that
///   refuse HTTP outright (e.g. authelia, vaultwarden).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HttpsRequirement {
    #[default]
    Never,
    Auth,
    Always,
}

impl HttpsRequirement {
    /// Decide whether an install must be promoted to HTTPS.
    ///
    /// HTTPS is required when any of these hold:
    ///   1. The service declares `https = "always"`.
    ///   2. The service declares `https = "auth"` AND the user chose OIDC
    ///      auth (via `--auth` or the interactive prompt).
    ///   3. The user passed an `https://...` URL explicitly.
    pub fn needs_https(&self, auth_requested: bool, url: Option<&str>) -> bool {
        matches!(self, HttpsRequirement::Always)
            || (matches!(self, HttpsRequirement::Auth) && auth_requested)
            || url.is_some_and(|u| u.starts_with("https://"))
    }
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
    /// When set and the service is exposed with `--tailscale`, this port is
    /// served over the service's Tailscale vIP on the given HTTPS port (e.g.
    /// `443` for the web root, `8080` for an API). Tailnet-only `serve`
    /// accepts arbitrary ports, so the value is usually the port's own number
    /// (or `443` for the one port that should answer at the bare hostname).
    /// Ports without this stay loopback-only. Reachable in templates via
    /// `{{service.port_url.<name>}}`. Multi-port services (e.g. ente: a web
    /// UI plus a separate API) need this so each endpoint gets its own URL.
    #[serde(default)]
    pub tailscale_https: Option<u16>,
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
    /// Standard base64 encoding of N random bytes (`length` = byte count,
    /// default 32). Use for binary keys that the service base64-decodes to a
    /// fixed byte length — e.g. Ente's libsodium keys (32-byte encryption,
    /// 64-byte hash). A plain `string`/`hex` value decodes to the wrong length.
    Base64,
    /// URL-safe base64 (`-_` instead of `+/`) of N random bytes. Same use as
    /// `base64`, but for services that decode with URL-safe base64 — e.g.
    /// Ente's `jwt.secret` (Go `base64.URLEncoding`), which rejects `+`/`/`.
    Base64Url,
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

/// A user-toggled bundle of env vars. Enabling the group writes every
/// member into `.env`; disabling it writes none of them.
///
/// Members reuse the full [`EnvVar`] shape — `kind = "default"` members are
/// auto-included with their rendered template when the group is on,
/// `prompted` members get shown with a default, `required` members must be
/// supplied (interactively or via process env).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvGroup {
    /// Identifier used by the `--enable <name>` CLI flag. Lowercase
    /// snake_case by convention.
    pub name: String,
    /// Yes/no question shown during `ryra add` to toggle the group.
    pub prompt: String,
    #[serde(default)]
    pub env: Vec<EnvVar>,
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

/// OIDC token endpoint authentication method for authelia client registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenAuthMethod {
    #[default]
    ClientSecretPost,
    ClientSecretBasic,
    /// PKCE public client — no client_secret sent. Used by apps like Zammad
    /// that only support the public-client + PKCE OIDC flow.
    None,
}

impl TokenAuthMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenAuthMethod::ClientSecretPost => "client_secret_post",
            TokenAuthMethod::ClientSecretBasic => "client_secret_basic",
            TokenAuthMethod::None => "none",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationFlags {
    /// Auth types this service supports. Empty = no auth support.
    #[serde(default)]
    pub auth: Vec<AuthKind>,
    /// OIDC token endpoint auth method for authelia client registration.
    #[serde(default)]
    pub token_auth_method: TokenAuthMethod,
    /// OIDC callback path suffixes registered with the auth provider.
    /// Appended to the service's base URL(s) to form redirect_uris.
    #[serde(default)]
    pub oidc_callbacks: Vec<String>,
    #[serde(default = "default_true")]
    pub smtp: bool,
    /// True if the service author has certified this service can be
    /// backed up safely. The default is `false` (explicit opt-in)
    /// because the worst failure mode is a backup that takes cleanly
    /// but won't restore (e.g. forgot to write a pg_dump hook), so
    /// authors must consciously declare support.
    ///
    /// When `true`, an accompanying `[backup]` section MAY provide
    /// hooks and excludes; when absent, the default behaviour is to
    /// back up every top-level child of the service home dir that the
    /// classifier marks as data.
    #[serde(default)]
    pub backup: bool,
}

impl Default for IntegrationFlags {
    fn default() -> Self {
        Self {
            auth: vec![],
            token_auth_method: TokenAuthMethod::default(),
            oidc_callbacks: vec![],
            smtp: true,
            backup: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Per-service backup configuration. Present only when the service's
/// `[integrations]` section sets `backup = true` AND the service needs
/// non-default behaviour (excludes or hooks).
///
/// Hooks are filenames inside `configs/scripts/` (same convention as
/// the existing `ExecStartPost=` scripts). They run with the same env
/// as those scripts: `$SERVICE_HOME` plus everything in the service's
/// `.env` file.
///
/// Pre/post hooks form a pair around the operation:
///
/// ```text
/// backup:  [pre_backup]  -> restic snapshot   -> [post_backup]
/// restore: [pre_restore] -> restic restore    -> [post_restore]
/// ```
///
/// Hooks must dump to `$SERVICE_HOME/.backup/` (a sibling of `data/`)
/// so it's clear which files are user-owned data versus snapshot
/// artefacts. Listing `.backup/<file>` in `paths` is required if the
/// hook writes one; nothing is implicitly included.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupConfig {
    /// Explicit list of paths (relative to service home) to include in
    /// the snapshot. When empty, the default is "every top-level child
    /// of the service home dir that the classifier marks as data."
    #[serde(default)]
    pub paths: Vec<String>,
    /// Restic-style exclude patterns relative to service home.
    /// Useful for skipping caches, previews, transcoding artefacts.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Script filename (in `configs/scripts/`) run before the restic
    /// snapshot. Typically dumps a database to `$SERVICE_HOME/.backup/`.
    #[serde(default)]
    pub pre_backup: Option<String>,
    /// Script filename run after a successful restic snapshot.
    /// Typically cleans up `$SERVICE_HOME/.backup/`.
    #[serde(default)]
    pub post_backup: Option<String>,
    /// Script filename run before restoring (typically stops the
    /// service and wipes the live data dir).
    #[serde(default)]
    pub pre_restore: Option<String>,
    /// Script filename run after restoring (typically imports the
    /// dump back into the live database and restarts the service).
    #[serde(default)]
    pub post_restore: Option<String>,
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
        if self.service.architecture.contains(&current) {
            None
        } else {
            let supported: Vec<_> = self
                .service
                .architecture
                .iter()
                .map(|a| a.to_string())
                .collect();
            Some(format!(
                "{} only supports {} — this system is {current}",
                self.service.name,
                supported.join(", "),
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
        let mut seen_ts_https = std::collections::HashSet::new();
        for p in &self.ports {
            if !seen_ports.insert(&p.name) {
                errors.push(format!("duplicate port name '{}'", p.name));
            }
            // `container_port = 0` is the "fill in later" placeholder `ryra init`
            // writes for a blank port. Refuse to install until it's a real port.
            if p.container_port == 0 {
                errors.push(format!(
                    "port '{}' has container_port = 0 — fill in the port your service listens on",
                    p.name
                ));
            }
            // Two ports can't be served on the same Tailscale HTTPS port —
            // the second `tailscale serve --https=<p>` would clobber the first.
            if let Some(https) = p.tailscale_https
                && !seen_ts_https.insert(https)
            {
                errors.push(format!(
                    "two ports map to the same tailscale_https port {https}"
                ));
            }
        }
        // If any port opts into Tailscale exposure, exactly one must own 443 —
        // that's the web root answering at the bare `<svc>.<tailnet>.ts.net`.
        let ts_ports: Vec<&PortDef> = self
            .ports
            .iter()
            .filter(|p| p.tailscale_https.is_some())
            .collect();
        if !ts_ports.is_empty()
            && ts_ports
                .iter()
                .filter(|p| p.tailscale_https == Some(443))
                .count()
                != 1
        {
            errors.push(
                "services exposing ports over Tailscale must mark exactly one port \
                 tailscale_https = 443 (the web root)"
                    .to_string(),
            );
        }

        // [metrics] must reference a declared port — the scrape target is
        // built from that entry's container_port.
        if let Some(metrics) = &self.metrics
            && !self.ports.iter().any(|p| p.name == metrics.port)
        {
            errors.push(format!(
                "[metrics] references port '{}' but no [[ports]] entry has that name",
                metrics.port
            ));
        }

        // Every env var name (top-level + every group member) must be unique
        // across the whole service — podman's .env is a flat keyspace so two
        // FOO= lines would be ambiguous.
        let mut seen_envs: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for e in &self.env {
            if !seen_envs.insert(&e.name) {
                errors.push(format!("duplicate env var name '{}'", e.name));
            }
        }
        for g in &self.env_groups {
            for e in &g.env {
                if !seen_envs.insert(&e.name) {
                    errors.push(format!(
                        "env var '{}' in group '{}' collides with another env var",
                        e.name, g.name
                    ));
                }
            }
        }

        // --- Env var name format + kind consistency ---

        for e in &self.env {
            check_env_var(e, None, &mut errors);
        }

        // --- Env group names + members ---

        let mut seen_groups = std::collections::HashSet::new();
        for g in &self.env_groups {
            if !seen_groups.insert(&g.name) {
                errors.push(format!("duplicate env_group name '{}'", g.name));
            }
            if g.name.is_empty() {
                errors.push("env_group has empty name".to_string());
            } else if !g
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                errors.push(format!(
                    "env_group '{}' must be lowercase snake_case ([a-z0-9_])",
                    g.name
                ));
            }
            if g.prompt.is_empty() {
                errors.push(format!("env_group '{}' has empty prompt", g.name));
            }
            if g.env.is_empty() {
                errors.push(format!("env_group '{}' has no env vars", g.name));
            }
            for e in &g.env {
                check_env_var(e, Some(&g.name), &mut errors);
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

        // --- Backup consistency ---
        //
        // The `[backup]` section is only meaningful when the author has
        // certified the service is backup-safe via `backup = true`. If
        // they wrote hooks/excludes without flipping the flag we'd
        // silently ship a service whose backup support is half-declared,
        // so reject it loudly.
        if let Some(ref backup) = self.backup
            && !self.integrations.backup
        {
            errors.push("[backup] section requires `backup = true` in [integrations]".to_string());
            // No-op read so the binding isn't unused if all sub-checks
            // below get gated out by serde defaults.
            let _ = backup;
        }
        if let Some(ref backup) = self.backup {
            for (label, hook) in [
                ("pre_backup", &backup.pre_backup),
                ("post_backup", &backup.post_backup),
                ("pre_restore", &backup.pre_restore),
                ("post_restore", &backup.post_restore),
            ] {
                if let Some(script) = hook
                    && (script.is_empty() || script.contains('/') || script.contains(".."))
                {
                    errors.push(format!(
                        "backup hook '{label}' must be a bare filename under configs/scripts/ \
                         (got {script:?})"
                    ));
                }
            }
            for p in &backup.paths {
                if p.is_empty() || p.starts_with('/') || p.contains("..") {
                    errors.push(format!(
                        "backup path {p:?} must be a relative path within the service home"
                    ));
                }
            }
        }

        // --- Runtime / build consistency ---
        // Make "native without a build target" and "podman with a build
        // section" unrepresentable past load: a native service needs to know
        // which binary to run; a podman service has no business declaring one.
        match self.service.runtime {
            Runtime::Native => match &self.service.run {
                None => errors.push(
                    "runtime = \"native\" requires a `run` command under [service]".to_string(),
                ),
                Some(run) if run.trim().is_empty() => {
                    errors.push("[service].run must not be empty".to_string())
                }
                Some(_) => {}
            },
            Runtime::Podman => {
                if self.service.run.is_some() || self.service.build.is_some() {
                    errors.push(
                        "`run` / `build` are only valid for runtime = \"native\" services"
                            .to_string(),
                    );
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("{name}: {}", errors.join("; ")))
        }
    }
}

/// Shared name-format + kind-consistency check for a single `EnvVar`, used
/// for both top-level `[[env]]` entries and `[[env_group.env]]` members.
/// `group` is `Some(group_name)` for member vars — it's used to make error
/// messages locate the offending declaration.
fn check_env_var(e: &EnvVar, group: Option<&str>, errors: &mut Vec<String>) {
    let where_ = match group {
        Some(g) => format!(" in group '{g}'"),
        None => String::new(),
    };
    if e.name.is_empty() {
        errors.push(format!("env var has empty name{where_}"));
    } else if !e
        .name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        errors.push(format!(
            "env var '{}'{where_} must start with a letter or _",
            e.name
        ));
    } else if !e
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        errors.push(format!(
            "env var '{}'{where_} contains invalid characters — must match [A-Za-z0-9_]",
            e.name
        ));
    }
    if e.kind == EnvKind::Required && e.value.contains("{{secret.") {
        errors.push(format!(
            "env var '{}'{where_} is kind=required but has a secret template default — use kind=prompted or kind=default",
            e.name
        ));
    }
}

/// Detect the current system architecture using OCI/Docker naming conventions.
pub fn current_architecture() -> Arch {
    match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        // Fallback: default to amd64 for unknown architectures.
        // The service's check_architecture() will catch unsupported ones.
        _ => Arch::Amd64,
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;

    fn parse(toml_src: &str) -> ServiceDef {
        toml::from_str(toml_src).expect("parse")
    }

    #[test]
    fn tailscale_https_requires_exactly_one_root() {
        // Two tailscale-exposed ports but neither owns 443 → rejected.
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[[ports]]
name = "http"
container_port = 8080
tailscale_https = 8080

[[ports]]
name = "photos"
container_port = 3000
tailscale_https = 3000
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("tailscale_https = 443"), "got: {err}");
    }

    #[test]
    fn tailscale_https_duplicate_port_rejected() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[[ports]]
name = "a"
container_port = 1
tailscale_https = 443

[[ports]]
name = "b"
container_port = 2
tailscale_https = 443
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("same tailscale_https"), "got: {err}");
    }

    #[test]
    fn tailscale_https_one_root_plus_api_validates() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[[ports]]
name = "http"
container_port = 8080
tailscale_https = 8080

[[ports]]
name = "photos"
container_port = 3000
tailscale_https = 443
"#,
        );
        svc.validate()
            .expect("one 443 root + one api port is valid");
    }

    #[test]
    fn backup_defaults_to_false_when_omitted() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"
"#,
        );
        assert!(!svc.integrations.backup);
        assert!(svc.backup.is_none());
        svc.validate().expect("default is valid");
    }

    #[test]
    fn backup_section_alone_is_rejected_without_integration_flag() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[backup]
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(
            err.contains("backup = true"),
            "error mentions the required flag: {err}"
        );
    }

    #[test]
    fn backup_supported_without_hooks_validates() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true
"#,
        );
        assert!(svc.integrations.backup);
        assert!(svc.backup.is_none());
        svc.validate().expect("ok without [backup] table");
    }

    #[test]
    fn backup_with_full_hooks_validates() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true

[backup]
paths = [".backup/db.sql.gz", "data"]
exclude = ["data/cache"]
pre_backup = "backup-pre.sh"
post_backup = "backup-post.sh"
pre_restore = "restore-pre.sh"
post_restore = "restore-post.sh"
"#,
        );
        svc.validate().expect("ok");
        let backup = svc.backup.as_ref().expect("section present");
        assert_eq!(backup.paths, vec![".backup/db.sql.gz", "data"]);
        assert_eq!(backup.pre_backup.as_deref(), Some("backup-pre.sh"));
    }

    #[test]
    fn backup_hook_with_slash_is_rejected() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true

[backup]
pre_backup = "subdir/script.sh"
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("pre_backup"), "{err}");
    }

    #[test]
    fn backup_hook_with_dotdot_is_rejected() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true

[backup]
post_backup = "../escape.sh"
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("post_backup"), "{err}");
    }

    #[test]
    fn backup_absolute_path_is_rejected() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true

[backup]
paths = ["/etc/passwd"]
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("/etc/passwd"), "{err}");
    }

    #[test]
    fn backup_path_with_dotdot_is_rejected() {
        let svc = parse(
            r#"
[service]
name = "x"
description = "x"

[integrations]
backup = true

[backup]
paths = ["../../somewhere"]
"#,
        );
        let err = svc.validate().expect_err("must reject");
        assert!(err.contains("somewhere"), "{err}");
    }
}

#[cfg(test)]
mod https_requirement_tests {
    use super::*;

    #[test]
    fn never_service_stays_http() {
        assert!(!HttpsRequirement::Never.needs_https(false, None));
        // Even with --auth, a service that didn't opt into HTTPS stays HTTP.
        // This is the RFC 8252 loopback case: http://127.0.0.1 is a valid
        // OIDC redirect_uri and most services (forgejo, etc.) work fine
        // that way.
        assert!(!HttpsRequirement::Never.needs_https(true, None));
        // Explicit http:// URL also stays HTTP.
        assert!(!HttpsRequirement::Never.needs_https(true, Some("http://foo.example.com")));
    }

    #[test]
    fn always_service_always_promotes() {
        assert!(HttpsRequirement::Always.needs_https(false, None));
        assert!(HttpsRequirement::Always.needs_https(false, Some("http://foo.example.com")));
    }

    #[test]
    fn auth_service_promotes_only_with_auth() {
        // The regression this guards: `ryra add nextcloud --auth` without
        // --url used to quietly install over HTTP and the SSO button never
        // rendered (user_oidc refuses to show it without HTTPS).
        assert!(HttpsRequirement::Auth.needs_https(true, None));
        // Without --auth, even an `https = "auth"` service stays HTTP.
        assert!(!HttpsRequirement::Auth.needs_https(false, None));
    }

    #[test]
    fn explicit_https_url_promotes() {
        assert!(HttpsRequirement::Never.needs_https(false, Some("https://foo.example.com")));
    }
}
