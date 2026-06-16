//! Typed wire protocol for driving ryra programmatically.
//!
//! Both the `ryra` agent (server) and any client compile against these types,
//! so the request/response contract is checked on both ends rather than passed
//! as stringly-typed CLI flags + parsed text. Transport-agnostic: the same
//! messages ride JSON-RPC over stdio now (a client spawns the agent as the
//! target user) and a network transport later (the agent on the box, the client
//! on a control plane), so moving off-box is a transport swap, not a rewrite.
//!
//! State-changing requests reuse the [`crate::ops`] request structs, which are
//! already the serde wire vocabulary ("Wire frontends can carry this enum
//! directly"). Read-only queries and the result/error shapes are added here,
//! because the on-disk types ([`crate::config::schema::InstalledService`]) are
//! deliberately not serde -- their on-disk artifacts are the source of truth,
//! so [`ServiceView`] is the stable wire projection of one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ops::{AddRequest, ConfigureRequest, LifecycleRequest, RemoveRequest, UpgradeRequest};

/// One request to the agent. Adjacently tagged so it maps straight onto a
/// JSON-RPC `method` + `params`: `{"method":"add","params":{...}}`,
/// `{"method":"list"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Install and start a service.
    Add(AddRequest),
    /// Remove a service (optionally purging its data).
    Remove(RemoveRequest),
    /// Re-render an installed service with a changed integration set.
    Configure(ConfigureRequest),
    /// Start or stop an installed service.
    Lifecycle(LifecycleRequest),
    /// Upgrade an installed service to the registry's current version.
    Upgrade(UpgradeRequest),
    /// List every service (installed + orphan) with live status.
    List,
    /// One service's current view.
    Get { service: String },
    /// What an upgrade would change for a service (read-only).
    Diff { service: String },
    /// The pre-upgrade snapshots available to revert to, newest first.
    Backups { service: String },
    /// Restore a service from a pre-upgrade snapshot (latest if `at` is None).
    Revert {
        service: String,
        #[serde(default)]
        at: Option<String>,
    },
    /// Search a registry for installable services (default registry if unset).
    Search {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        registry: Option<String>,
    },
    /// List the configured registries.
    Registries,
    /// Add a custom registry.
    AddRegistry { name: String, url: String },
    /// Remove a custom registry.
    RemoveRegistry { name: String },
    /// Run the diagnostics ryra-doctor runs.
    Doctor,
    /// Take a backup snapshot of a (backup-enabled) service.
    Backup { service: String },
}

/// The result of a backup run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupOutcome {
    pub service: String,
    /// Paths included in the snapshot.
    pub paths: usize,
}

/// One installable service from a registry search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub installed: bool,
    /// Integrations the service supports (e.g. "oidc", "smtp").
    pub supports: Vec<String>,
}

/// A configured registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryInfo {
    pub name: String,
    pub url: String,
    pub service_count: usize,
}

/// Severity of a doctor finding. Mirrors `system::doctor::Severity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Blocks installs outright.
    Blocker,
    /// Service runs but the user probably wants to fix it.
    Warning,
    /// Informational.
    Info,
}

/// One diagnostic finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorIssue {
    /// Stable machine-readable id for the issue variant.
    pub code: String,
    pub severity: Severity,
    /// Full human-readable message, including the suggested fix (byte-for-byte
    /// what `ryra doctor` prints).
    pub message: String,
    /// The service this issue is scoped to, when service-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// How one file differs between the registry render and disk. Mirrors
/// `upgrade::DiffKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Unchanged,
    Modified,
    /// Hand-edited; blocks a plain upgrade without force.
    Drift,
    Added,
    Removed,
}

/// One changed file in a [`DiffView`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
}

/// An env var the registry expects that the install is missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvAddition {
    pub key: String,
    /// Registry env kind (default / prompted / required), as a string.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// What an upgrade would change for a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffView {
    pub service: String,
    /// Anything (file or env or stale source) would change on upgrade.
    pub upgrade_available: bool,
    /// Hand-edited files would block a plain upgrade (needs force).
    pub blocked_by_drift: bool,
    /// Native source changed since the process started (rebuild would ship it).
    pub source_stale: bool,
    /// Per-file changes; omits unchanged files.
    pub entries: Vec<DiffEntry>,
    /// Env vars the registry expects but the `.env` is missing.
    pub env_additions: Vec<EnvAddition>,
}

/// One restorable pre-upgrade snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSnapshotView {
    /// `YYYY-MM-DDTHH-MM-SSZ`; pass back as `at` to revert to exactly this one.
    pub timestamp: String,
}

/// The result of a revert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertOutcome {
    pub service: String,
    /// The snapshot timestamp restored.
    pub timestamp: String,
    pub files_restored: usize,
    pub files_deleted: usize,
}

/// Live run state of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Running,
    Stopped,
    /// Removed, but its data is preserved on disk.
    Removed,
}

/// A service as seen over the wire: the stable, serde projection of an on-disk
/// `InstalledService` plus its live status. Mirrors the shape ryra-api already
/// returns to its dashboard, promoted here so server and client share it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceView {
    pub name: String,
    pub state: ServiceState,
    /// The URL a user reaches the service at, if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Allocated host ports (`port_name -> host_port`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Registry the service came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Installed version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A newer version is available in the registry.
    #[serde(default)]
    pub upgrade_available: bool,
}

/// The outcome of a mutating operation: the affected service's fresh view plus
/// what the apply actually did. `applied` is the number of steps/changes
/// executed (0 = nothing to do / already current); `destructive` is true when
/// the change set deletes data or is otherwise irreversible (the safety signal
/// `ryra configure` surfaces).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub service: ServiceView,
    pub applied: usize,
    #[serde(default)]
    pub destructive: bool,
}

/// The payload of a successful response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// `add` / `configure` / `lifecycle` / `upgrade`: the service view plus what
    /// the apply did.
    Applied(ApplyOutcome),
    /// `get`: a pure view, no apply.
    Service(ServiceView),
    /// `list`.
    Services(Vec<ServiceView>),
    /// `diff`.
    Diff(DiffView),
    /// `backups`.
    Backups(Vec<BackupSnapshotView>),
    /// `revert`.
    Revert(RevertOutcome),
    /// `search`.
    SearchResults(Vec<SearchHit>),
    /// `registries`.
    Registries(Vec<RegistryInfo>),
    /// `doctor`.
    Doctor(Vec<DoctorIssue>),
    /// `backup`.
    Backup(BackupOutcome),
    /// `remove` / `add_registry` / `remove_registry`.
    Done,
}

/// What `ryra rpc` writes to stdout: exactly one of these per request, then it
/// exits. Tagged so a client can branch without inspecting the exit code,
/// though `ryra rpc` also exits non-zero on `Error` for shell ergonomics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(Response),
    Error(RpcError),
}

/// A structured error, mappable to a JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

/// Coarse error categories, so a client can branch (e.g. 404 vs 409 vs 500)
/// without string-matching the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request, or it referenced something unknown/invalid.
    BadRequest,
    /// No such service.
    NotFound,
    /// Conflicting state (e.g. already installed, drift blocks the change).
    Conflict,
    /// Execution failed (podman / systemctl / the registry / the network).
    Internal,
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_to_method_and_params() {
        let req = Request::Add(AddRequest::new("forgejo"));
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["method"], "add");
        assert_eq!(v["params"]["service"], "forgejo");
    }

    #[test]
    fn unit_request_has_no_params() {
        let v = serde_json::to_value(Request::List).unwrap();
        assert_eq!(v["method"], "list");
        assert!(v.get("params").is_none());
    }

    #[test]
    fn get_carries_its_service() {
        let v = serde_json::to_value(Request::Get {
            service: "vaultwarden".to_string(),
        })
        .unwrap();
        assert_eq!(v["method"], "get");
        assert_eq!(v["params"]["service"], "vaultwarden");
    }

    #[test]
    fn service_view_round_trips_and_omits_empties() {
        let view = ServiceView {
            name: "forgejo".to_string(),
            state: ServiceState::Running,
            url: Some("https://forgejo.example.com".to_string()),
            ports: BTreeMap::new(),
            registry: None,
            version: None,
            upgrade_available: false,
        };
        let v = serde_json::to_value(&view).unwrap();
        // Empty/None fields are omitted from the wire.
        assert!(v.get("ports").is_none());
        assert!(v.get("registry").is_none());
        assert_eq!(v["state"], "running");
        let back: ServiceView = serde_json::from_value(v).unwrap();
        assert_eq!(back.name, "forgejo");
        assert_eq!(back.state, ServiceState::Running);
    }
}
