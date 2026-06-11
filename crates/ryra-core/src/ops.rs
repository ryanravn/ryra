//! Frontend-neutral operation vocabulary.
//!
//! Every way of driving ryra (CLI today, HTTP API, and whatever comes
//! later) expresses state changes as an [`Operation`] and plans it
//! through [`plan`]. The request types are plain serde data: required
//! fields are required by construction, optional knobs are `Option` or
//! defaulted, and mutually exclusive choices are enums, so a frontend
//! cannot build an invalid request and cannot silently support less
//! than the vocabulary (exhaustive `match` breaks the build when a
//! variant is added).
//!
//! Frontends keep their sugar (interactive prompts, auto-installing
//! authelia/inbucket, batching): sugar resolves user input *into* these
//! requests. Business rules live here, never in a frontend.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capability::Capability;
use crate::error::{Error, Result};
use crate::registry::resolve::ServiceRef;
use crate::registry::service_def::AuthKind;
use crate::{
    AddResult, AddServiceParams, AuthChoice, Exposure, Lifecycle, PlanMode, RemoveMode,
    RemoveResult, Step, config,
};

/// How the service should be reachable. The frontends resolve fuzzier
/// intent (prompts, `--tailscale` tailnet lookup) into one of these.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureRequest {
    /// `http://127.0.0.1:<port>` only. If the service requires HTTPS and
    /// Caddy is installed, planning auto-promotes to a `*.internal` URL
    /// (the same non-interactive default the CLI uses).
    #[default]
    Loopback,
    /// A concrete URL; classified by hostname into Internal / Public.
    Url(String),
    /// A pre-derived `*.ts.net` URL. Deriving it needs the host's
    /// tailnet identity, which is frontend territory (sudo, tailscale
    /// CLI), so it arrives here already resolved.
    Tailscale(String),
}

/// Whether (and how) to wire the service to the auth provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRequested {
    #[default]
    No,
    /// Use the service's first declared auth kind (the `--auth` rule).
    Yes,
    /// A specific kind, e.g. chosen at an interactive prompt.
    Kind(AuthKind),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRequest {
    /// Registry ref: "forgejo", "acme/forgejo", or a local project path.
    pub service: String,
    #[serde(default)]
    pub exposure: ExposureRequest,
    #[serde(default)]
    pub auth: AuthRequested,
    /// `None` = wire SMTP iff a provider is configured (the CLI's
    /// non-interactive default). `Some(true)` errors loudly when no
    /// provider exists instead of silently skipping.
    #[serde(default)]
    pub smtp: Option<bool>,
    #[serde(default)]
    pub backup: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub enable_groups: BTreeSet<String>,
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveRequest {
    pub service: String,
    #[serde(default)]
    pub mode: RemoveMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleRequest {
    pub service: String,
    pub action: Lifecycle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeRequest {
    pub service: String,
    /// Re-render even when the diff is empty (native services rebuild
    /// from source regardless).
    #[serde(default)]
    pub force: bool,
}

/// Re-render an installed service with a changed integration set. The
/// change set is core's [`crate::configure::Overrides`]: `None` fields
/// stay untouched, provided fields are the new truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigureRequest {
    pub service: String,
    pub changes: crate::configure::Overrides,
}

/// The complete state-changing vocabulary. Wire frontends can carry
/// this enum directly; the CLI constructs the inner requests from
/// flags and prompts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Add(AddRequest),
    Remove(RemoveRequest),
    Lifecycle(LifecycleRequest),
    Upgrade(UpgradeRequest),
    Configure(ConfigureRequest),
    BackupRun(BackupRunRequest),
}

/// Frontend-supplied capabilities and plan mechanics. Everything here
/// is either a system probe core refuses to own (`port_in_use`) or
/// internal plumbing for retries/upgrades; none of it is user intent.
pub struct PlanContext<'a> {
    /// `+ Sync` so plans can be held across awaits inside async (Send)
    /// handlers; the CLI's plain fn pointer satisfies it for free.
    pub port_in_use: &'a (dyn Fn(u16) -> bool + Sync),
    /// Already-resolved (ref, repo dir) when the frontend resolved the
    /// registry earlier (the CLI does, once per batch). `None` resolves
    /// from `AddRequest::service`.
    pub resolved: Option<(&'a ServiceRef, &'a Path)>,
    /// Secrets minted during an interactive prompt phase, reused so the
    /// values the user saw match what gets written.
    pub pre_built_ctx: Option<BTreeMap<String, String>>,
    /// Pin port assignments (upgrade re-renders).
    pub port_overrides: BTreeMap<String, u16>,
    pub mode: PlanMode,
    /// ACME mode for the reverse proxy's own install. Lives here rather
    /// than in [`AddRequest`] until `AcmeMode` grows serde support;
    /// only the CLI exposes it today.
    pub acme: Option<&'a crate::caddy::AcmeMode>,
}

impl<'a> PlanContext<'a> {
    pub fn new(port_in_use: &'a (dyn Fn(u16) -> bool + Sync)) -> Self {
        PlanContext {
            port_in_use,
            resolved: None,
            pre_built_ctx: None,
            port_overrides: BTreeMap::new(),
            mode: PlanMode::Add,
            acme: None,
        }
    }
}

/// A planned add, carrying everything a frontend needs to record and
/// execute it without re-deriving anything.
pub struct PlannedAdd {
    /// Plain service name (ref resolved).
    pub service: String,
    pub result: AddResult,
    pub exposure: Exposure,
    pub auth_kind: Option<AuthKind>,
    pub registry_name: String,
    pub repo_dir: PathBuf,
    /// Informational decisions made during planning (e.g. auto-derived
    /// URL). Frontends surface these; silence would hide behavior.
    pub notes: Vec<String>,
}

impl PlannedAdd {
    /// Record the install as pending before executing steps, so a
    /// failed run is visible to cleanup. Same data in every frontend.
    pub fn record_pending(&self) -> Result<()> {
        crate::record_pending(crate::RecordPendingParams {
            service_name: &self.service,
            auth_kind: self.auth_kind.clone(),
            registry_name: &self.registry_name,
            allocated_ports: &self.result.allocated_ports,
            repo_dir: &self.repo_dir,
            exposure: &self.exposure,
        })
    }
}

/// The planned outcome of any [`Operation`]. Execution stays with the
/// frontend (it owns the Step executor).
pub enum Planned {
    Add(Box<PlannedAdd>),
    Remove(RemoveResult),
    Lifecycle(Vec<Step>),
    Upgrade(Box<crate::upgrade::UpgradeResult>),
    Configure(Box<crate::configure::ConfigureResult>),
    BackupRun(Box<crate::backup::BackupRunPlan>),
}

/// Plan one operation. The single entry point shared by all frontends.
pub async fn plan(op: &Operation, ctx: PlanContext<'_>) -> Result<Planned> {
    match op {
        Operation::Add(req) => Ok(Planned::Add(Box::new(plan_add(req, ctx).await?))),
        Operation::Remove(req) => Ok(Planned::Remove(plan_remove(req)?)),
        Operation::Lifecycle(req) => Ok(Planned::Lifecycle(plan_lifecycle(req)?)),
        Operation::Upgrade(req) => Ok(Planned::Upgrade(Box::new(plan_upgrade(req).await?))),
        Operation::Configure(req) => Ok(Planned::Configure(Box::new(plan_configure(req).await?))),
        Operation::BackupRun(req) => Ok(Planned::BackupRun(Box::new(plan_backup_run(req).await?))),
    }
}

pub async fn plan_upgrade(req: &UpgradeRequest) -> Result<crate::upgrade::UpgradeResult> {
    crate::upgrade::upgrade_service(&req.service, req.force).await
}

pub async fn plan_configure(req: &ConfigureRequest) -> Result<crate::configure::ConfigureResult> {
    crate::configure::configure_service(&req.service, &req.changes).await
}

pub async fn plan_add(req: &AddRequest, ctx: PlanContext<'_>) -> Result<PlannedAdd> {
    let mut notes = Vec::new();

    // Resolve the registry ref unless the frontend already did.
    let (service_ref, repo_dir) = match ctx.resolved {
        Some((r, d)) => (r.clone(), d.to_path_buf()),
        None => {
            let r = ServiceRef::parse(&req.service)?;
            let d = crate::resolve_registry_dir(&r).await?;
            (r, d)
        }
    };
    let service = service_ref.service_name().to_string();
    let reg_service = crate::registry::find_service(&repo_dir, &service)?;
    let paths = config::ConfigPaths::resolve()?;
    let cfg = config::load_or_default(&paths.config_file)?;

    // Auth: the `--auth` rule (first declared kind), or a specific kind
    // which must actually be declared. The auth provider itself is the
    // exception: it isn't a client of itself.
    let supported = &reg_service.def.integrations.auth;
    let auth_kind: Option<AuthKind> = match &req.auth {
        AuthRequested::No => None,
        AuthRequested::Yes => match supported.first() {
            Some(kind) => Some(kind.clone()),
            None if reg_service
                .def
                .capabilities
                .provides
                .contains(&Capability::OidcProvider) =>
            {
                notes.push(format!(
                    "{service} is the auth provider itself; auth has no effect"
                ));
                None
            }
            None => return Err(Error::NoOidcSupport(service)),
        },
        AuthRequested::Kind(kind) => {
            if !supported.contains(kind) {
                return Err(Error::NoOidcSupport(service));
            }
            Some(kind.clone())
        }
    };
    if auth_kind.is_some() && cfg.auth.is_none() {
        return Err(Error::AuthNotConfigured);
    }

    // SMTP: explicit request must not silently degrade; the default
    // wires mail exactly when a provider exists.
    let enable_smtp = req.smtp.unwrap_or(cfg.smtp.is_some());
    if enable_smtp && cfg.smtp.is_none() {
        return Err(Error::ConfigValidation(format!(
            "SMTP requested for '{service}' but no SMTP provider is configured \
             (add inbucket, or configure SMTP first)"
        )));
    }

    // Exposure: concrete requests pass through classification; Loopback
    // on an HTTPS-requiring service auto-promotes through Caddy when
    // possible (the CLI's non-interactive default) and errors loudly
    // when it can't.
    let requested_url = match &req.exposure {
        ExposureRequest::Url(u) => Some(u.as_str()),
        _ => None,
    };
    let needs_https = reg_service
        .def
        .service
        .https
        .needs_https(auth_kind.is_some(), requested_url);
    let exposure = match &req.exposure {
        ExposureRequest::Url(u) => Exposure::from_url(u),
        ExposureRequest::Tailscale(u) => Exposure::Tailscale { url: u.clone() },
        ExposureRequest::Loopback if needs_https => {
            if crate::is_service_installed("caddy") {
                let https_port = crate::well_known::caddy_https_port(&cfg);
                let url = format!(
                    "https://{service}.{}:{https_port}",
                    config::schema::CADDY_LOCAL_DOMAIN
                );
                notes.push(format!("{service} requires HTTPS; exposing at {url}"));
                Exposure::from_url(&url)
            } else {
                return Err(Error::ConfigValidation(format!(
                    "service '{service}' requires HTTPS but no exposure was given: \
                     pass a URL or tailscale exposure, or add caddy first"
                )));
            }
        }
        ExposureRequest::Loopback => Exposure::Loopback,
    };

    let auth_choice = match &auth_kind {
        Some(kind) => AuthChoice::Native(kind.clone()),
        None => AuthChoice::None,
    };
    let result = crate::add_service(AddServiceParams {
        service_name: &service,
        exposure: &exposure,
        auth: auth_choice,
        enable_smtp,
        enable_backup: req.backup,
        env_overrides: &req.env,
        enabled_groups: &req.enable_groups,
        registry_name: service_ref.registry_name(),
        repo_dir: &repo_dir,
        pre_built_ctx: ctx.pre_built_ctx,
        port_in_use: ctx.port_in_use,
        acme_mode: ctx.acme,
        mode: ctx.mode,
        port_overrides: &ctx.port_overrides,
    })?;

    Ok(PlannedAdd {
        registry_name: service_ref.registry_name().to_string(),
        service,
        result,
        exposure,
        auth_kind,
        repo_dir,
        notes,
    })
}

pub fn plan_remove(req: &RemoveRequest) -> Result<RemoveResult> {
    crate::remove_service(&req.service, req.mode)
}

pub fn plan_lifecycle(req: &LifecycleRequest) -> Result<Vec<Step>> {
    crate::lifecycle_steps(&req.service, req.action)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRunRequest {
    pub service: String,
}

/// Plan a backup of one service: resolves the install's registry dir and
/// the configured repository. Execution is
/// [`crate::backup::execute_backup_run`].
pub async fn plan_backup_run(req: &BackupRunRequest) -> Result<crate::backup::BackupRunPlan> {
    let paths = config::ConfigPaths::resolve()?;
    let cfg = config::load_or_default(&paths.config_file)?;
    let installed = crate::list_installed()?
        .into_iter()
        .find(|s| s.name == req.service)
        .ok_or_else(|| Error::ServiceNotInstalled(req.service.clone()))?;
    let service_ref = crate::service_ref_from_installed(&installed);
    let repo_dir = crate::resolve_registry_dir(&service_ref).await?;
    crate::backup::plan_backup_run(&req.service, &cfg, &repo_dir)
}
