//! `ryra rpc`: one-shot typed RPC over stdio.
//!
//! Reads a single [`Request`] as JSON on stdin, executes it against ryra-core,
//! writes a single [`Reply`] as JSON on stdout, and exits. This is the
//! programmatic seam: a client (ryra-api today) runs `ryra rpc` as the target
//! user and pipes one request in. Run-and-exit, like every other ryra command,
//! NOT a long-lived daemon. The shared [`ryra_protocol`] types give both ends a
//! compiler-checked contract; the same messages move to a network transport
//! unchanged when the client moves off-box.

use std::collections::HashMap;
use std::io::Read;

use anyhow::Result;
use ryra_core::config::schema::InstalledService;
use ryra_core::data::{ServiceStatus, enumerate_all};
use ryra_core::ops::{self, Operation, PlanContext, Planned};
use ryra_protocol::{
    ApplyOutcome, BackupOutcome, BackupSnapshotView, ChoiceOptionView, ChoiceView, ConfigureView,
    DiffEntry, DiffKind, DiffView, DoctorIssue, EnvAddition, EnvGroupView, EnvKindView, EnvVarView,
    EnvKeyChangeView, ErrorCode, ReconcileOutcome, ReconcilePlanView, RegistryInfo, Reply, Request,
    Response, RestoreOutcome, RevertOutcome, RpcError, SearchHit, ServiceDefView, ServiceState,
    ServiceView, Severity,
};

use super::apply;

type OpResult = std::result::Result<Response, RpcError>;

pub async fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let reply = match serde_json::from_str::<Request>(&input) {
        Ok(req) => match dispatch(req).await {
            Ok(resp) => Reply::Ok(resp),
            Err(e) => Reply::Error(e),
        },
        Err(e) => Reply::Error(RpcError::new(
            ErrorCode::BadRequest,
            format!("invalid request: {e}"),
        )),
    };

    println!("{}", serde_json::to_string(&reply)?);
    if matches!(reply, Reply::Error(_)) {
        std::process::exit(1);
    }
    Ok(())
}

async fn dispatch(req: Request) -> OpResult {
    match req {
        // Reads.
        Request::List => Ok(Response::Services(all_views()?)),
        Request::Get { service } => view_of(&service).map(Response::Service),
        Request::Diff { service } => diff_view(&service).await.map(Response::Diff),
        Request::Backups { service } => {
            let snaps = ryra_core::list_backups(&service).map_err(core_err)?;
            Ok(Response::Backups(
                snaps
                    .into_iter()
                    .map(|s| BackupSnapshotView {
                        timestamp: s.timestamp,
                    })
                    .collect(),
            ))
        }
        Request::Revert { service, at } => {
            revert(&service, at.as_deref()).await.map(Response::Revert)
        }
        Request::Search { query, registry } => search(query.as_deref(), registry.as_deref())
            .await
            .map(Response::SearchResults),
        Request::Registries => {
            let regs = ryra_core::registry::manage::list().map_err(core_err)?;
            Ok(Response::Registries(
                regs.into_iter()
                    .map(|r| RegistryInfo {
                        name: r.name,
                        url: r.url,
                        service_count: r.service_count,
                    })
                    .collect(),
            ))
        }
        Request::AddRegistry { name, url } => {
            ryra_core::registry::manage::add(&name, &url)
                .await
                .map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::RemoveRegistry { name } => {
            ryra_core::registry::manage::remove(&name).map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::Doctor => Ok(Response::Doctor(doctor())),
        Request::Backup { service } => {
            let plan = ops::plan_backup_run(&ryra_core::ops::BackupRunRequest {
                service: service.clone(),
            })
            .await
            .map_err(core_err)?;
            let paths = plan.paths.len();
            ryra_core::backup::execute_backup_run(&plan).map_err(core_err)?;
            Ok(Response::Backup(BackupOutcome { service, paths }))
        }
        Request::Restore { service, snapshot } => {
            restore(&service, &snapshot).await.map(Response::Restore)
        }
        Request::BackupEnrolled => {
            let services = ryra_core::backup::list_backup_enabled().map_err(core_err)?;
            Ok(Response::BackupEnrolled(services))
        }
        Request::SetBackupEnrolled { service, enabled } => {
            set_backup_enrolled(&service, enabled)?;
            Ok(Response::Done)
        }
        Request::ServiceDef { service, registry } => service_def_view(&service, registry.as_deref())
            .await
            .map(Response::ServiceDef),
        Request::ConfigureView { service } => {
            configure_view(&service).await.map(Response::ConfigureView)
        }
        Request::Reconcile { services, dry_run } => {
            reconcile(services, dry_run).await.map(Response::Reconcile)
        }
        // Mutations: plan via the one shared entry point, then execute the
        // typed Steps with the same executor every frontend uses.
        // Convert the protocol-native request payloads into the engine's ops
        // types at the boundary (ryra-core owns the From impls).
        Request::Add(r) => run_mutation(Operation::Add(r.into())).await,
        Request::Remove(r) => run_mutation(Operation::Remove(r.into())).await,
        Request::Configure(r) => run_mutation(Operation::Configure(r.into())).await,
        Request::Lifecycle(r) => run_mutation(Operation::Lifecycle(r.into())).await,
        Request::Upgrade(r) => run_mutation(Operation::Upgrade(r.into())).await,
    }
}

/// Plan + execute one mutating operation. Remove returns `Done`; the rest
/// return an [`ApplyOutcome`] (the fresh service view + how much applied +
/// whether the change was destructive), so callers don't lose the per-op
/// accounting the in-process plan exposed.
async fn run_mutation(op: Operation) -> OpResult {
    // The installed name to re-read afterwards. For Add the request `service`
    // may be a registry ref or path, so we take the resolved name from the plan.
    let target = match &op {
        Operation::Remove(r) => Some(r.service.clone()),
        Operation::Configure(r) => Some(r.service.clone()),
        Operation::Lifecycle(r) => Some(r.service.clone()),
        Operation::Upgrade(r) => Some(r.service.clone()),
        Operation::Add(_) | Operation::BackupRun(_) => None,
    };

    let ctx = PlanContext::new(&super::is_port_in_use);
    let planned = ops::plan(&op, ctx).await.map_err(core_err)?;

    // Remove has no post-op service view; handle and return early.
    if let Planned::Remove(r) = planned {
        apply::execute_all(&r.steps).await.map_err(core_err)?;
        ryra_core::finalize_remove(&r.service_name).map_err(core_err)?;
        return Ok(Response::Done);
    }

    // Capture the apply accounting BEFORE executing (steps are consumed below).
    let (name, applied, destructive) = match &planned {
        Planned::Add(p) => (p.service.clone(), p.result.steps.len(), false),
        Planned::Lifecycle(steps) => (target.clone().unwrap_or_default(), steps.len(), false),
        Planned::Upgrade(u) => (target.clone().unwrap_or_default(), u.steps.len(), false),
        Planned::Configure(c) => (
            target.clone().unwrap_or_default(),
            if c.is_noop() { 0 } else { c.changes.len() },
            c.has_destructive,
        ),
        Planned::Remove(_) => unreachable!("handled above"),
        // Not part of the service-management surface this seam exposes.
        Planned::BackupRun(_) => {
            return Err(RpcError::new(
                ErrorCode::BadRequest,
                "backup_run is not supported over rpc",
            ));
        }
    };

    match planned {
        Planned::Add(p) => {
            p.record_pending().map_err(core_err)?;
            apply::execute_all(&p.result.steps)
                .await
                .map_err(core_err)?;
        }
        Planned::Lifecycle(steps) => apply::execute_all(&steps).await.map_err(core_err)?,
        Planned::Upgrade(u) => apply::execute_all(&u.steps).await.map_err(core_err)?,
        Planned::Configure(c) => apply::execute_all(&c.steps).await.map_err(core_err)?,
        Planned::Remove(_) | Planned::BackupRun(_) => unreachable!("handled above"),
    }

    let service = view_of(&name)?;
    Ok(Response::Applied(ApplyOutcome {
        service,
        applied,
        destructive,
    }))
}

/// What an upgrade would change for a service (read-only).
async fn diff_view(service: &str) -> std::result::Result<DiffView, RpcError> {
    let d = ryra_core::diff_service(service).await.map_err(core_err)?;
    let blocked_by_drift = d
        .entries
        .iter()
        .any(|e| matches!(e.kind, ryra_core::DiffKind::Drift));
    let upgrade_available = !d.is_clean() || d.source_stale;
    Ok(DiffView {
        service: d.service,
        upgrade_available,
        blocked_by_drift,
        source_stale: d.source_stale,
        entries: d
            .entries
            .iter()
            .filter(|e| !matches!(e.kind, ryra_core::DiffKind::Unchanged))
            .map(|e| DiffEntry {
                path: e.path.display().to_string(),
                kind: map_diff_kind(&e.kind),
            })
            .collect(),
        env_additions: d
            .env_additions
            .iter()
            .map(|a| EnvAddition {
                key: a.key.clone(),
                kind: format!("{:?}", a.kind).to_lowercase(),
                prompt: a.prompt.clone(),
            })
            .collect(),
    })
}

fn map_diff_kind(k: &ryra_core::DiffKind) -> DiffKind {
    use ryra_core::DiffKind as Core;
    match k {
        Core::Unchanged => DiffKind::Unchanged,
        Core::Modified => DiffKind::Modified,
        Core::Drift => DiffKind::Drift,
        Core::Added => DiffKind::Added,
        Core::Removed => DiffKind::Removed,
    }
}

/// Restore a service from a pre-upgrade snapshot, then execute the restore.
async fn revert(service: &str, at: Option<&str>) -> std::result::Result<RevertOutcome, RpcError> {
    let r = ryra_core::revert_service(service, at).map_err(core_err)?;
    let outcome = RevertOutcome {
        service: r.service.clone(),
        timestamp: r.snapshot.timestamp.clone(),
        files_restored: r.files_to_restore.len(),
        files_deleted: r.files_to_delete.len(),
    };
    apply::execute_all(&r.steps).await.map_err(core_err)?;
    Ok(outcome)
}

/// Search a registry for installable services (default registry if unset).
async fn search(
    query: Option<&str>,
    registry: Option<&str>,
) -> std::result::Result<Vec<SearchHit>, RpcError> {
    use ryra_core::registry::resolve::ServiceRef;
    let service_ref = match registry {
        Some(name) => ServiceRef::Custom {
            registry: name.to_string(),
            service: String::new(),
        },
        None => ServiceRef::Default(String::new()),
    };
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref)
        .await
        .map_err(core_err)?;
    let results = ryra_core::search_services(&repo_dir, query).map_err(core_err)?;
    Ok(results
        .into_iter()
        .map(|r| SearchHit {
            name: r.name,
            description: r.description,
            installed: r.installed,
            supports: r.supports,
        })
        .collect())
}

/// Restore a service's data from a restic snapshot, running its pre/post
/// restore hooks around the restic restore (the engine half of
/// `ryra backup restore`).
async fn restore(
    service: &str,
    snapshot: &str,
) -> std::result::Result<RestoreOutcome, RpcError> {
    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let cfg = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    let installed = ryra_core::list_installed()
        .map_err(core_err)?
        .into_iter()
        .find(|s| s.name == service)
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::NotFound,
                format!("service '{service}' is not installed"),
            )
        })?;
    let service_ref = ryra_core::service_ref_from_installed(&installed);
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref)
        .await
        .map_err(core_err)?;
    let plan = ryra_core::backup::plan_backup_restore(service, snapshot, &cfg, &repo_dir)
        .map_err(core_err)?;

    // pre-hook -> restic restore -> post-hook, mirroring the CLI. Hooks let
    // database services import a dumped file after the filesystem restore.
    if let Some(hook) = &plan.pre_restore_hook {
        ryra_core::backup::run_hook("pre_restore", &plan.service_name, hook, &plan.service_home)
            .map_err(core_err)?;
    }
    ryra_core::backup::restic_restore(&plan).map_err(core_err)?;
    if let Some(hook) = &plan.post_restore_hook {
        ryra_core::backup::run_hook("post_restore", &plan.service_name, hook, &plan.service_home)
            .map_err(core_err)?;
    }
    Ok(RestoreOutcome {
        service: service.to_string(),
        snapshot: snapshot.to_string(),
    })
}

/// Propagate the global config into installed services (`configure --apply`).
/// Empty `services` reconciles every installed service; `dry_run` previews
/// without writing or restarting. A service that fails to reconcile (e.g. an
/// unresolvable registry) is skipped, not fatal.
async fn reconcile(
    services: Vec<String>,
    dry_run: bool,
) -> std::result::Result<ReconcileOutcome, RpcError> {
    let targets: Vec<String> = if services.is_empty() {
        ryra_core::list_installed()
            .map_err(core_err)?
            .into_iter()
            .map(|s| s.name)
            .collect()
    } else {
        services
    };

    let mut reconciles = Vec::new();
    for name in &targets {
        match ryra_core::reconcile_service(name).await {
            Ok(r) if !r.changes.is_empty() => reconciles.push(r),
            Ok(_) => {}
            Err(e) => eprintln!("reconcile skipped for {name}: {e}"),
        }
    }

    let plans: Vec<ReconcilePlanView> = reconciles
        .iter()
        .map(|r| ReconcilePlanView {
            service: r.service.clone(),
            changes: r
                .changes
                .iter()
                .map(|c| EnvKeyChangeView {
                    key: c.key.clone(),
                    from: c.from.clone(),
                    to: c.to.clone(),
                    secret: c.secret,
                })
                .collect(),
        })
        .collect();

    if dry_run {
        return Ok(ReconcileOutcome { plans, applied: 0 });
    }
    for r in &reconciles {
        apply::execute_all(&r.steps).await.map_err(core_err)?;
    }
    let applied = reconciles.len();
    Ok(ReconcileOutcome { plans, applied })
}

/// Set whether a service is enrolled in backups (`metadata.backup_enabled`).
/// Idempotent; a no-op for a service with no install metadata.
fn set_backup_enrolled(service: &str, enabled: bool) -> std::result::Result<(), RpcError> {
    let Some(mut meta) = ryra_core::load_metadata(service).map_err(core_err)? else {
        return Ok(());
    };
    if meta.backup_enabled == enabled {
        return Ok(());
    }
    meta.backup_enabled = enabled;
    let path = ryra_core::service_home(service)
        .map_err(core_err)?
        .join("metadata.toml");
    let toml = toml::to_string_pretty(&meta).map_err(core_err)?;
    std::fs::write(&path, toml).map_err(core_err)?;
    Ok(())
}

/// The installable schema for a registry service (default registry if unset).
async fn service_def_view(
    name: &str,
    registry: Option<&str>,
) -> std::result::Result<ServiceDefView, RpcError> {
    use ryra_core::registry::resolve::ServiceRef;
    let service_ref = match registry {
        Some(r) => ServiceRef::Custom {
            registry: r.to_string(),
            service: name.to_string(),
        },
        None => ServiceRef::Default(name.to_string()),
    };
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref)
        .await
        .map_err(core_err)?;
    let reg_service = ryra_core::registry::find_service(&repo_dir, name).map_err(|e| {
        RpcError::new(ErrorCode::NotFound, format!("service '{name}': {e}"))
    })?;
    Ok(def_view(&reg_service.def))
}

/// The configure view for an installed service: schema resolved from the
/// recorded registry, plus the current selections and `.env` values.
async fn configure_view(name: &str) -> std::result::Result<ConfigureView, RpcError> {
    use ryra_core::registry::resolve::{ServiceRef, is_path_like};
    let metadata = ryra_core::metadata::load_metadata(name)
        .map_err(core_err)?
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::NotFound,
                format!("service '{name}' is not installed"),
            )
        })?;
    let registry = &metadata.registry;
    let service_ref = if registry.is_empty() || registry == ryra_core::REGISTRY_DEFAULT {
        ServiceRef::Default(name.to_string())
    } else if is_path_like(registry) {
        ServiceRef::Path {
            dir: std::path::PathBuf::from(registry),
            name: name.to_string(),
        }
    } else {
        ServiceRef::Custom {
            registry: registry.to_string(),
            service: name.to_string(),
        }
    };
    let repo_dir = ryra_core::resolve_registry_dir(&service_ref)
        .await
        .map_err(core_err)?;
    let reg_service = ryra_core::registry::find_service(&repo_dir, name).map_err(core_err)?;
    let current_env = ryra_core::service_home(name)
        .ok()
        .and_then(|home| std::fs::read_to_string(home.join(".env")).ok())
        .map(|c| parse_env(&c))
        .unwrap_or_default();
    Ok(ConfigureView {
        name: name.to_string(),
        def: def_view(&reg_service.def),
        selected_choices: metadata.selected_choices,
        enabled_groups: metadata.enabled_groups,
        current_env,
    })
}

/// Project a core service definition onto the wire schema the forms render.
fn def_view(def: &ryra_core::registry::service_def::ServiceDef) -> ServiceDefView {
    ServiceDefView {
        name: def.service.name.clone(),
        env: def.env.iter().map(env_var_view).collect(),
        env_groups: def
            .env_groups
            .iter()
            .map(|g| EnvGroupView {
                name: g.name.clone(),
                prompt: g.prompt.clone(),
                env: g.env.iter().map(env_var_view).collect(),
            })
            .collect(),
        choices: def
            .choices
            .iter()
            .map(|c| ChoiceView {
                name: c.name.clone(),
                prompt: c.prompt.clone(),
                default: c.default.clone(),
                options: c
                    .options
                    .iter()
                    .map(|o| ChoiceOptionView {
                        name: o.name.clone(),
                        label: o.label.clone(),
                        env: o.env.iter().map(env_var_view).collect(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn env_var_view(e: &ryra_core::registry::service_def::EnvVar) -> EnvVarView {
    use ryra_core::registry::service_def::{EnvFormat, EnvKind};
    let kind = match e.kind {
        EnvKind::Default => EnvKindView::Default,
        EnvKind::Prompted => EnvKindView::Prompted,
        EnvKind::Required => EnvKindView::Required,
    };
    let format = match e.format {
        EnvFormat::String => "string",
        EnvFormat::Hex => "hex",
        EnvFormat::Base64 => "base64",
        EnvFormat::Base64Url => "base64_url",
        EnvFormat::Uuid => "uuid",
        EnvFormat::JwtHs256 => "jwt_hs256",
    };
    EnvVarView {
        name: e.name.clone(),
        kind,
        prompt: e.prompt.clone(),
        format: format.to_string(),
        generated: e.value.contains("{{secret."),
        value_empty: e.value.is_empty(),
    }
}

/// Parse a rendered `.env` into a key->value map for prefilling a form.
/// Skips blanks and comments; strips one layer of surrounding quotes.
fn parse_env(content: &str) -> std::collections::BTreeMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// The full doctor sweep (same checks as `ryra doctor`).
fn doctor() -> Vec<DoctorIssue> {
    use ryra_core::system::doctor;
    let issues = (|| -> anyhow::Result<Vec<doctor::Issue>> {
        let paths = ryra_core::config::ConfigPaths::resolve()?;
        let config = ryra_core::config::load_or_default(&paths.config_file)?;
        Ok(doctor::check_all(&config)
            .into_iter()
            .chain(doctor::check_auth_wiring())
            .chain(doctor::check_tailscale_services())
            .collect())
    })()
    .unwrap_or_default();
    issues
        .into_iter()
        .map(|i| DoctorIssue {
            code: i.code().to_string(),
            severity: map_severity(i.severity()),
            service: i.service(),
            message: i.to_string(),
        })
        .collect()
}

fn map_severity(s: ryra_core::system::doctor::Severity) -> Severity {
    use ryra_core::system::doctor::Severity as S;
    match s {
        S::Blocker => Severity::Blocker,
        S::Warning => Severity::Warning,
        S::Info => Severity::Info,
    }
}

/// Map any ryra-core error to a structured rpc error. Coarse for now (most
/// land as `internal`); refine to NotFound/Conflict as the need arises.
fn core_err(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e.to_string())
}

/// One service's view by name, or NotFound.
fn view_of(name: &str) -> std::result::Result<ServiceView, RpcError> {
    all_views()?
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("no service '{name}'")))
}

/// A [`ServiceView`] for every service (installed + orphan), mirroring the data
/// behind `ryra list`.
fn all_views() -> std::result::Result<Vec<ServiceView>, RpcError> {
    let svcs = enumerate_all().map_err(core_err)?;
    let installed = ryra_core::list_installed().unwrap_or_default();
    let by_name: HashMap<&str, &InstalledService> =
        installed.iter().map(|s| (s.name.as_str(), s)).collect();
    let active = super::list::active_user_units();

    Ok(svcs
        .iter()
        .map(|svc| {
            let inst = by_name.get(svc.service.as_str()).copied();
            let state = if matches!(svc.status, ServiceStatus::Orphan) {
                ServiceState::Removed
            } else if active.contains(&svc.service) {
                ServiceState::Running
            } else {
                ServiceState::Stopped
            };
            view_from(svc.service.clone(), state, inst)
        })
        .collect())
}

fn view_from(name: String, state: ServiceState, inst: Option<&InstalledService>) -> ServiceView {
    let Some(i) = inst else {
        return ServiceView {
            name,
            state,
            url: None,
            ports: Default::default(),
            registry: None,
            version: None,
            upgrade_available: false,
        };
    };
    ServiceView {
        name,
        state,
        url: i.exposure.url().map(|u| u.to_string()),
        ports: i.ports.clone(),
        registry: Some(i.repo.clone()).filter(|s| !s.is_empty()),
        version: Some(i.version.clone()).filter(|s| !s.is_empty()),
        upgrade_available: false,
    }
}
