//! The typed wire protocol for driving ryra over rpc.
//!
//! This crate is the contract, and *only* the contract: pure serde data types,
//! no dependency on `ryra-core` (the engine). Any client - ryra-api, a control
//! plane, a third-party tool - can speak it without compiling the engine, which
//! is what makes ryra-api movable off the box later (it talks to the box's
//! `ryra rpc` over a transport, depending only on these types).
//!
//! The `ryra` binary owns the engine: it deserializes a [`Request`], converts
//! the protocol-native request payloads into `ryra_core::ops` types, runs them,
//! and serializes a [`Reply`]. The request payloads here mirror the ops request
//! structs by shape (not by import), so the engine's internal types stay
//! engine-private.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

// ---- Request payloads (protocol-native; the engine converts to ops::*) ----

/// How a service should be exposed when installed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureRequest {
    #[default]
    Loopback,
    /// A concrete URL, classified by hostname into internal/public.
    Url(String),
    /// A pre-derived `*.ts.net` URL (the caller resolved the tailnet identity).
    Tailscale(String),
}

/// The kind of auth a service can be wired to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Oidc,
}

/// Whether (and how) to wire a service to the auth provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRequested {
    #[default]
    No,
    /// The service's first declared auth kind (the `--auth` rule).
    Yes,
    /// A specific kind.
    Kind(AuthKind),
}

/// Install and start a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRequest {
    /// Registry ref ("forgejo", "acme/forgejo") or a local project path.
    pub service: String,
    #[serde(default)]
    pub exposure: ExposureRequest,
    #[serde(default)]
    pub auth: AuthRequested,
    /// `None` = wire SMTP iff a provider is configured; `Some(true)` errors
    /// when none exists rather than silently skipping.
    #[serde(default)]
    pub smtp: Option<bool>,
    #[serde(default)]
    pub backup: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub enable_groups: BTreeSet<String>,
    /// `[[choice]]` selections (`choice -> option`); unset choices use defaults.
    #[serde(default)]
    pub choose: BTreeMap<String, String>,
    /// Skip-setup: install even when a `Required` var has no value (left blank
    /// in `.env` for the operator to fill in later) rather than erroring.
    #[serde(default)]
    pub allow_unset_required: bool,
}

impl AddRequest {
    /// The simplest install: loopback, no integrations.
    pub fn new(service: impl Into<String>) -> Self {
        AddRequest {
            service: service.into(),
            exposure: ExposureRequest::default(),
            auth: AuthRequested::default(),
            smtp: None,
            backup: false,
            env: BTreeMap::new(),
            enable_groups: BTreeSet::new(),
            choose: BTreeMap::new(),
            allow_unset_required: false,
        }
    }
}

/// How much to remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveMode {
    /// Stop + remove quadlets/config but keep data dirs and volumes (orphan).
    #[default]
    Preserve,
    /// Also delete data subdirs and podman named volumes.
    Purge,
}

/// Remove a service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub service: String,
    #[serde(default)]
    pub mode: RemoveMode,
}

/// Start or stop an installed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Start,
    Stop,
}

/// Start/stop a service (and its sidecars).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleRequest {
    pub service: String,
    pub action: Lifecycle,
}

/// Upgrade a service to the registry's current version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeRequest {
    pub service: String,
    /// Re-render even when the diff is empty.
    #[serde(default)]
    pub force: bool,
}

/// An exposure transition for `configure`. `Loopback` means "no public route".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureChange {
    Url(String),
    Tailscale(String),
    Loopback,
}

/// The integration change-set for `configure`. `None`/empty fields leave the
/// current state untouched; provided fields are the new truth.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    pub exposure: Option<ExposureChange>,
    pub smtp: Option<bool>,
    pub backup: Option<bool>,
    pub auth: Option<bool>,
    pub enable_groups: BTreeSet<String>,
    pub disable_groups: BTreeSet<String>,
    pub choose: BTreeMap<String, String>,
    pub env_overrides: BTreeMap<String, String>,
    /// Re-register the OIDC client even when auth is already on and the URL is
    /// unchanged (repairs a provider/consumer desync).
    pub reassert_auth: bool,
}

/// Re-render an installed service with a changed integration set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigureRequest {
    pub service: String,
    pub changes: Overrides,
}

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
    /// Restore a service's data from a restic snapshot ("latest" for newest).
    Restore { service: String, snapshot: String },
    /// List a service's restic data snapshots, newest first (`ryra backup list`).
    Snapshots { service: String },
    /// The effective backup configuration + enrolled services
    /// (`ryra backup status`).
    BackupStatus,
    /// Point backups at a backend: init the restic repo and persist `[backup]`
    /// (`ryra backup config`). `password` is the restic key; when absent the
    /// engine reuses the existing key or generates a fresh one.
    ConfigureBackup {
        backend: BackupBackendSpec,
        #[serde(default)]
        password: Option<String>,
    },
    /// Opt a service in or out of backups.
    SetBackupEnrolled { service: String, enabled: bool },
    /// Store an account token in the box's credentials file -- the structured
    /// equivalent of `ryra account login --with-token`. The control plane uses
    /// this to sign a managed box into its account for backups, over rpc rather
    /// than an ad-hoc SSH file write. The engine owns the path/format/perms.
    AccountLogin { token: String },
    /// Prune snapshots to the configured retention ladder (`restic forget`,
    /// then prune). `None` service = every enrolled service; `dry_run` previews
    /// what would be removed without deleting. A no-op for a service with no
    /// retention policy.
    ForgetBackups {
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// Back up enrolled services (empty `services` = every enrolled install) --
    /// the rpc twin of `ryra backup run`, for control-plane/dashboard parity.
    RunBackups {
        #[serde(default)]
        services: Vec<String>,
        /// Cadence tag for the snapshots: `daily` | `weekly` | `manual`.
        /// Defaults to `manual` (hand-run, never pruned).
        #[serde(default)]
        mode: Option<String>,
    },
    /// Full disaster recovery: restore EVERY service in the repo at `snapshot`
    /// ("latest" or an id), in dependency order, re-linking + starting them.
    /// The rpc twin of `ryra backup restore` with no service.
    RestoreAll { snapshot: String },
    /// Set the full backup schedule (rpc twin of `ryra backup config`'s schedule
    /// step + `ryra backup schedule`). Each cadence: `Some` enables it (keep N
    /// at `HH:MM`), `None` disables it. Installs/removes the daily + weekly
    /// timers to match. Manual backups are always available and unaffected.
    SetSchedule {
        #[serde(default)]
        daily: Option<ScheduleSpec>,
        #[serde(default)]
        weekly: Option<ScheduleSpec>,
    },
    /// The installable env/group/choice schema for a registry service
    /// (default registry if `registry` is unset).
    ServiceDef {
        service: String,
        #[serde(default)]
        registry: Option<String>,
    },
    /// The configure view (schema + current selections + `.env`) for an
    /// installed service.
    ConfigureView { service: String },
    /// Propagate the current global config into installed services
    /// (`ryra config --apply`). Empty `services` = every installed service
    /// whose env would change; `dry_run` previews without writing/restarting.
    Reconcile {
        #[serde(default)]
        services: Vec<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// Discover the registry's test suites (`ryra test search`).
    ListTests,
    /// Run one registry test by name on the host (`ryra test <name>`).
    RunTest { name: String },
    /// Local test sandbox state: installed services + last results
    /// (`ryra test list`).
    TestState,
    /// Delete stored results for one test, or all tests when `name` is None
    /// (`ryra test remove`).
    RemoveTestResults {
        #[serde(default)]
        name: Option<String>,
    },
}

/// The result of a backup run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupOutcome {
    pub service: String,
    /// Paths included in the snapshot.
    pub paths: usize,
}

/// The result of a restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub service: String,
    /// The snapshot restored ("latest" when none was specified).
    pub snapshot: String,
}

/// Where backups are stored, as a client describes one when configuring.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupBackendSpec {
    /// A local restic repo path (no off-box protection; rarely what you want).
    Local { path: String },
    /// Any S3-compatible object store (MinIO, AWS S3, B2, R2, Wasabi).
    S3 {
        endpoint: String,
        bucket: String,
        access_key_id: String,
        secret_access_key: String,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Ryra-managed: the box holds no storage keys; it vends short-lived,
    /// account-scoped S3 credentials per backup run. Requires an active managed
    /// backup plan (configuring without one fails at credential-vend time).
    Managed,
}

/// One restic data snapshot (`ryra backup list`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotView {
    /// Short restic snapshot id; pass back as the restore snapshot.
    pub id: String,
    /// RFC3339 timestamp the snapshot was taken.
    pub time: String,
    /// Restic tags (e.g. `service:foo`, `manifest_sha:...`).
    pub tags: Vec<String>,
}

/// The effective backup configuration plus enrolled services
/// (`ryra backup status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupStatusView {
    /// `[backup]` is configured (env-seeded, CLI, or manual).
    pub configured: bool,
    /// Human label for the backend, e.g. "S3: my-bucket (...)". None when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_label: Option<String>,
    /// Services enrolled in backups (`metadata.backup_enabled`).
    pub enrolled: Vec<String>,
    /// Daily schedule (keep N at HH:MM), if enabled. `None` = no daily backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<ScheduleSpec>,
    /// Weekly schedule (Sunday), if enabled. `None` = no weekly backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<ScheduleSpec>,
}

/// A scheduled backup cadence: keep at most `keep` snapshots of this mode,
/// run at `at` (24h `HH:MM`; `None` => the 03:00 default). Used both to set
/// the schedule (`SetSchedule`) and to report it (`BackupStatusView`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSpec {
    pub keep: u32,
    #[serde(default)]
    pub at: Option<String>,
}

/// Per-service result of a retention sweep (`ForgetBackups`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetView {
    pub service: String,
    /// Snapshots kept after the sweep.
    pub kept: u32,
    /// Snapshots removed (in a dry run, the count that WOULD be removed).
    pub removed: u32,
    /// True when this was a preview (`--dry-run`); nothing was deleted.
    pub dry_run: bool,
}

/// One env key a reconcile would change in a service's `.env`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvKeyChangeView {
    pub key: String,
    /// On-disk value, or `None` when the key isn't present yet.
    pub from: Option<String>,
    pub to: String,
    /// True when the key name looks sensitive (a client masks it for display).
    pub secret: bool,
}

/// What a reconcile would (or did) do to one installed service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcilePlanView {
    pub service: String,
    pub changes: Vec<EnvKeyChangeView>,
}

/// The outcome of propagating the global config into installed services.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileOutcome {
    /// Affected services and their env diffs (the preview, or what was applied).
    pub plans: Vec<ReconcilePlanView>,
    /// How many services were updated and restarted (0 on a dry run).
    pub applied: usize,
}

/// One installable service from a registry search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    pub installed: bool,
    /// Integrations the service supports (e.g. "oidc", "smtp").
    pub supports: Vec<String>,
    /// Recommended RAM in MB from the manifest, when declared. Lets callers
    /// warn before an install would overcommit the machine's memory.
    #[serde(default)]
    pub recommended_ram_mb: Option<u64>,
}

/// A configured registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryInfo {
    pub name: String,
    pub url: String,
    pub service_count: usize,
}

/// Severity of a doctor finding.
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

/// How one file differs between the registry render and disk.
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
    /// Install/start is in flight: the unit's start job is still running
    /// (image pull, container create, health check) so it reports
    /// `activating`, not yet `active`. A transient state during `ryra add`.
    Installing,
    /// Removed, but its data is preserved on disk.
    Removed,
}

/// A service as seen over the wire: the stable, serde projection of an on-disk
/// installed service plus its live status.
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
/// what the apply did. `applied` is the number of steps/changes executed (0 =
/// nothing to do); `destructive` is true when the change deletes data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub service: ServiceView,
    pub applied: usize,
    #[serde(default)]
    pub destructive: bool,
}

// ---- Service-definition views (the install / configure forms) -------------

/// How a registry env var is treated: a `default` value, a `prompted` one the
/// user may override, or a `required` one they must supply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvKindView {
    Default,
    Prompted,
    Required,
}

/// One env var as a form renders it: enough to label it, decide whether it
/// needs input, and show whether the value is auto-generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVarView {
    pub name: String,
    pub kind: EnvKindView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Value format: "string", "hex", "base64", "base64_url", "uuid", "jwt_hs256".
    pub format: String,
    /// The value comes from a `{{secret.*}}` template, so it's auto-generated.
    pub generated: bool,
    /// The declared value is empty (a `prompted` var with no default needs input).
    pub value_empty: bool,
}

/// An optional, named group of env vars, enabled together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvGroupView {
    pub name: String,
    pub prompt: String,
    pub env: Vec<EnvVarView>,
}

/// One alternative within a [`ChoiceView`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceOptionView {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub env: Vec<EnvVarView>,
}

/// A single-select `[[choice]]`: pick exactly one option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChoiceView {
    pub name: String,
    pub prompt: String,
    pub default: String,
    pub options: Vec<ChoiceOptionView>,
}

/// A service definition's installable schema, as the install picker renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDefView {
    pub name: String,
    pub env: Vec<EnvVarView>,
    pub env_groups: Vec<EnvGroupView>,
    pub choices: Vec<ChoiceView>,
}

/// The configure view for an installed service: its rendered schema plus the
/// selections and `.env` values currently on disk, so a form can pre-fill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigureView {
    pub name: String,
    pub def: ServiceDefView,
    /// Currently selected option per `[[choice]]` (`choice -> option`).
    pub selected_choices: BTreeMap<String, String>,
    /// Currently enabled optional groups.
    pub enabled_groups: Vec<String>,
    /// Current `.env` values, so prompted/required fields show what's set.
    pub current_env: BTreeMap<String, String>,
}

/// One discoverable registry test (`ryra test search`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryTestView {
    pub name: String,
    /// `"simple"` (setup then assert) or `"lifecycle"` (interleaved steps).
    pub kind: String,
    pub services: Vec<String>,
    pub step_count: usize,
    pub step_kinds: Vec<String>,
    pub needs_browser: bool,
    pub requires_sudo: bool,
}

/// The outcome of running one test (`ryra test <name>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunView {
    pub name: String,
    pub passed: bool,
    pub duration_secs: f64,
    /// `"passed"` / `"skipped"` / a failure message.
    pub outcome: String,
    pub events: Vec<TestEventView>,
}

/// One step/assertion within a test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEventView {
    pub description: String,
    /// `"step"` or `"assertion"`.
    pub kind: String,
    pub passed: bool,
    pub skipped: bool,
    pub error: Option<String>,
    pub duration_secs: f64,
    pub stdout: String,
    pub stderr: String,
}

/// Local test sandbox state: where it lives + the last stored results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestStateView {
    pub sandbox_path: String,
    pub tests: Vec<TestResultEntryView>,
}

/// One stored test result (from a prior run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResultEntryView {
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
    pub timestamp: u64,
    pub has_playwright: bool,
}

/// The payload of a successful response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// `add` / `configure` / `lifecycle` / `upgrade`.
    Applied(ApplyOutcome),
    /// `get`.
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
    /// `restore`.
    Restore(RestoreOutcome),
    /// `snapshots`.
    Snapshots(Vec<SnapshotView>),
    /// `backup_status`.
    BackupStatus(BackupStatusView),
    /// `forget_backups` — per-service retention sweep results.
    Forget(Vec<ForgetView>),
    /// `service_def`.
    ServiceDef(ServiceDefView),
    /// `configure_view`.
    ConfigureView(ConfigureView),
    /// `reconcile`.
    Reconcile(ReconcileOutcome),
    /// `list_tests`.
    Tests(Vec<RegistryTestView>),
    /// `run_test`.
    TestRun(TestRunView),
    /// `test_state`.
    TestState(TestStateView),
    /// `remove` / `add_registry` / `remove_registry` / `remove_test_results`.
    Done,
}

/// What `ryra rpc` writes to stdout: exactly one of these per request, then it
/// exits.
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

/// Coarse error categories, so a client can branch without string-matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
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
        assert!(v.get("ports").is_none());
        assert_eq!(v["state"], "running");
        let back: ServiceView = serde_json::from_value(v).unwrap();
        assert_eq!(back.name, "forgejo");
        assert_eq!(back.state, ServiceState::Running);
    }
}
