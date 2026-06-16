//! `ryra rpc`: one-shot typed RPC over stdio.
//!
//! Reads a single [`Request`] as JSON on stdin, executes it against ryra-core,
//! writes a single [`Reply`] as JSON on stdout, and exits. This is the
//! programmatic seam: a client (ryra-api today) runs `ryra rpc` as the target
//! user and pipes one request in. Run-and-exit, like every other ryra command,
//! NOT a long-lived daemon. The shared [`ryra_core::protocol`] types give both
//! ends a compiler-checked contract; the same messages move to a network
//! transport unchanged when the client moves off-box.

use std::collections::HashMap;
use std::io::Read;

use anyhow::Result;
use ryra_core::config::schema::InstalledService;
use ryra_core::data::{ServiceStatus, enumerate_all};
use ryra_core::ops::{self, Operation, PlanContext, Planned};
use ryra_core::protocol::{
    ApplyOutcome, BackupSnapshotView, DiffEntry, DiffKind, DiffView, EnvAddition, ErrorCode, Reply,
    Request, Response, RevertOutcome, RpcError, ServiceState, ServiceView,
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
        // Mutations: plan via the one shared entry point, then execute the
        // typed Steps with the same executor every frontend uses.
        Request::Add(r) => run_mutation(Operation::Add(r)).await,
        Request::Remove(r) => run_mutation(Operation::Remove(r)).await,
        Request::Configure(r) => run_mutation(Operation::Configure(r)).await,
        Request::Lifecycle(r) => run_mutation(Operation::Lifecycle(r)).await,
        Request::Upgrade(r) => run_mutation(Operation::Upgrade(r)).await,
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
