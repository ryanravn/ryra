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
    ErrorCode, Reply, Request, Response, RpcError, ServiceState, ServiceView,
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
        Request::Get { service } => all_views()?
            .into_iter()
            .find(|v| v.name == service)
            .map(Response::Service)
            .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("no service '{service}'"))),
        // Mutations: plan via the one shared entry point, then execute the
        // typed Steps with the same executor every frontend uses.
        Request::Add(r) => run_mutation(Operation::Add(r)).await,
        Request::Remove(r) => run_mutation(Operation::Remove(r)).await,
        Request::Configure(r) => run_mutation(Operation::Configure(r)).await,
        Request::Lifecycle(r) => run_mutation(Operation::Lifecycle(r)).await,
        Request::Upgrade(r) => run_mutation(Operation::Upgrade(r)).await,
    }
}

/// Plan + execute one mutating operation, then return the affected service's
/// fresh view (or `Done` for remove).
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

    match planned {
        Planned::Add(p) => {
            let name = p.service.clone();
            p.record_pending().map_err(core_err)?;
            apply::execute_all(&p.result.steps)
                .await
                .map_err(core_err)?;
            one_view(&name)
        }
        Planned::Remove(r) => {
            apply::execute_all(&r.steps).await.map_err(core_err)?;
            ryra_core::finalize_remove(&r.service_name).map_err(core_err)?;
            Ok(Response::Done)
        }
        Planned::Lifecycle(steps) => {
            apply::execute_all(&steps).await.map_err(core_err)?;
            one_view(target.as_deref().unwrap_or_default())
        }
        Planned::Upgrade(u) => {
            apply::execute_all(&u.steps).await.map_err(core_err)?;
            one_view(target.as_deref().unwrap_or_default())
        }
        Planned::Configure(c) => {
            apply::execute_all(&c.steps).await.map_err(core_err)?;
            one_view(target.as_deref().unwrap_or_default())
        }
        // Not part of the service-management surface this seam exposes.
        Planned::BackupRun(_) => Err(RpcError::new(
            ErrorCode::BadRequest,
            "backup_run is not supported over rpc",
        )),
    }
}

/// Map any ryra-core error to a structured rpc error. Coarse for now (most
/// land as `internal`); refine to NotFound/Conflict as the need arises.
fn core_err(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e.to_string())
}

/// One service's view by name, or NotFound.
fn one_view(name: &str) -> OpResult {
    all_views()?
        .into_iter()
        .find(|v| v.name == name)
        .map(Response::Service)
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::NotFound,
                format!("service '{name}' not found after the operation"),
            )
        })
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
