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
use ryra_core::protocol::{
    ErrorCode, Reply, Request, Response, RpcError, ServiceState, ServiceView,
};

pub async fn run() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    let reply = match serde_json::from_str::<Request>(&input) {
        Ok(req) => dispatch(req).await,
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

async fn dispatch(req: Request) -> Reply {
    match req {
        Request::List => match all_views() {
            Ok(views) => Reply::Ok(Response::Services(views)),
            Err(e) => Reply::Error(e),
        },
        Request::Get { service } => match all_views() {
            Ok(views) => match views.into_iter().find(|v| v.name == service) {
                Some(v) => Reply::Ok(Response::Service(v)),
                None => Reply::Error(RpcError::new(
                    ErrorCode::NotFound,
                    format!("no service '{service}'"),
                )),
            },
            Err(e) => Reply::Error(e),
        },
        // Mutating ops land next, reusing the same ryra-core flows the CLI's
        // add/remove/configure/lifecycle/upgrade commands drive.
        Request::Add(_)
        | Request::Remove(_)
        | Request::Configure(_)
        | Request::Lifecycle(_)
        | Request::Upgrade(_) => Reply::Error(RpcError::new(
            ErrorCode::Internal,
            "mutating ops are not wired into rpc yet",
        )),
    }
}

/// A [`ServiceView`] for every service (installed + orphan), mirroring the data
/// behind `ryra list`.
fn all_views() -> std::result::Result<Vec<ServiceView>, RpcError> {
    let svcs = enumerate_all().map_err(|e| RpcError::new(ErrorCode::Internal, e.to_string()))?;
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
