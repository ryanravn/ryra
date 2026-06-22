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
    ApplyOutcome, BackupBackendSpec, BackupOutcome, BackupSnapshotView, BackupStatusView,
    ChoiceOptionView, ChoiceView, ConfigureView, DiffEntry, DiffKind, DiffView, DoctorIssue,
    EnvAddition, EnvGroupView, EnvKeyChangeView, EnvKindView, EnvVarView, ErrorCode,
    ReconcileOutcome, ReconcilePlanView, RegistryInfo, RegistryTestView, Reply, Request, Response,
    RestoreOutcome, RevertOutcome, RpcError, SearchHit, ServiceDefView, ServiceState, ServiceView,
    Severity, SnapshotView, TestEventView, TestResultEntryView, TestRunView, TestStateView,
};

use super::apply;

type OpResult = std::result::Result<Response, RpcError>;

pub async fn run() -> Result<()> {
    // The rpc contract is "one Reply as JSON on stdout, nothing else". But the
    // apply path we share with the CLI prints human progress to stdout, and a
    // deploy lets podman's pull progress bars flow through the inherited
    // stdout too -- correct for an interactive `ryra add`, fatal here, because
    // it interleaves with (and corrupts) the JSON reply the client parses.
    //
    // So reserve the real stdout for the reply alone: dup it aside, then point
    // fd 1 at stderr for the dispatch. Every `println!` and every child's
    // inherited stdout now lands on stderr (which the client treats as
    // diagnostics), and we write the reply to the saved fd at the very end.
    let mut reply_out = hijack_stdout()?;

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

    // Flush any buffered stdout writes (now aimed at stderr) before the reply,
    // so progress can't trail in after it on a shared descriptor.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    writeln!(reply_out, "{}", serde_json::to_string(&reply)?)?;
    reply_out.flush()?;
    if matches!(reply, Reply::Error(_)) {
        std::process::exit(1);
    }
    Ok(())
}

/// Save the real stdout aside and redirect fd 1 to stderr, returning a handle
/// to the original stdout (where the single reply is written). See [`run`].
fn hijack_stdout() -> std::io::Result<std::fs::File> {
    use std::os::unix::io::FromRawFd;
    // SAFETY: dup/dup2 on the process's own standard descriptors; the returned
    // fd is wrapped in exactly one `File` that owns it.
    unsafe {
        let saved = libc::dup(libc::STDOUT_FILENO);
        if saved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
            let err = std::io::Error::last_os_error();
            libc::close(saved);
            return Err(err);
        }
        Ok(std::fs::File::from_raw_fd(saved))
    }
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
        Request::Snapshots { service } => snapshots(&service).map(Response::Snapshots),
        Request::BackupStatus => backup_status().map(Response::BackupStatus),
        Request::ConfigureBackup { backend, password } => {
            configure_backup(backend, password)?;
            backup_status().map(Response::BackupStatus)
        }
        Request::AccountLogin { token } => {
            ryra_core::system::account::save_credentials(
                &ryra_core::system::account::Credentials { token },
            )
            .map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::ForgetBackups { service, dry_run } => {
            forget_backups(service, dry_run).map(Response::Forget)
        }
        // Parity with `ryra backup run/restore(all)/schedule`: reuse the CLI
        // orchestration directly (their stdout is hijacked to stderr by the rpc
        // entry, so only the JSON reply reaches the client).
        Request::RunBackups { services } => {
            super::backup::run_backup(services).await.map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::RestoreAll { snapshot } => {
            // "latest" -> newest-per-service; otherwise the explicit snapshot id.
            let at = (snapshot != "latest").then_some(snapshot);
            super::backup::restore_all(at).await.map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::ScheduleBackup { interval } => {
            use super::backup::ScheduleInterval;
            let interval = match interval.as_str() {
                "hourly" => ScheduleInterval::Hourly,
                "daily" => ScheduleInterval::Daily,
                "weekly" => ScheduleInterval::Weekly,
                "disable" => ScheduleInterval::Disable,
                other => {
                    return Err(core_err(format!(
                        "invalid schedule interval '{other}' (hourly|daily|weekly|disable)"
                    )));
                }
            };
            super::backup::schedule(interval).await.map_err(core_err)?;
            Ok(Response::Done)
        }
        Request::SetBackupEnrolled { service, enabled } => {
            set_backup_enrolled(&service, enabled)?;
            Ok(Response::Done)
        }
        Request::ServiceDef { service, registry } => {
            service_def_view(&service, registry.as_deref())
                .await
                .map(Response::ServiceDef)
        }
        Request::ConfigureView { service } => {
            configure_view(&service).await.map(Response::ConfigureView)
        }
        Request::Reconcile { services, dry_run } => {
            reconcile(services, dry_run).await.map(Response::Reconcile)
        }
        Request::ListTests => list_tests().await.map(Response::Tests),
        Request::RunTest { name } => run_test(&name).await.map(Response::TestRun),
        Request::TestState => test_state().map(Response::TestState),
        Request::RemoveTestResults { name } => {
            remove_test_results(name.as_deref());
            Ok(Response::Done)
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
    // op_err: plan-time precondition failures (already installed, leftover
    // state, not installed) carry a real status, not a 500.
    let planned = ops::plan(&op, ctx).await.map_err(op_err)?;

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
            seed_tailscale_token(&p.result.steps)?;
            p.record_pending().map_err(core_err)?;
            apply::execute_all(&p.result.steps)
                .await
                .map_err(core_err)?;
        }
        Planned::Lifecycle(steps) => apply::execute_all(&steps).await.map_err(core_err)?,
        Planned::Upgrade(u) => {
            seed_tailscale_token(&u.steps)?;
            apply::execute_all(&u.steps).await.map_err(core_err)?
        }
        Planned::Configure(c) => {
            seed_tailscale_token(&c.steps)?;
            apply::execute_all(&c.steps).await.map_err(core_err)?
        }
        Planned::Remove(_) | Planned::BackupRun(_) => unreachable!("handled above"),
    }

    let service = view_of(&name)?;
    Ok(Response::Applied(ApplyOutcome {
        service,
        applied,
        destructive,
    }))
}

/// Ensure the Tailscale admin token is in *this user's* config before an apply
/// that registers a Tailscale Service (Setup/Enable). The rpc runs as the agent
/// user, so this writes the agent user's `preferences.toml`, which is the one
/// the apply's admin-API call reads. Seeds from `TAILSCALE_API_KEY` (forwarded
/// into the rpc env by the client); a no-op when the token is already set or
/// the plan touches no Tailscale Service. Quiet by design: the rpc owns stdout
/// for the single JSON reply, so unlike the CLI path this prints nothing.
fn seed_tailscale_token(steps: &[ryra_core::Step]) -> std::result::Result<(), RpcError> {
    let needs = steps.iter().any(|s| {
        matches!(
            s,
            ryra_core::Step::TailscaleSetup | ryra_core::Step::TailscaleEnable { .. }
        )
    });
    if !needs {
        return Ok(());
    }
    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let mut config = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    if config.tailscale.is_some() {
        return Ok(());
    }
    let admin_api_key = std::env::var("TAILSCALE_API_KEY").map_err(|_| {
        RpcError::new(
            ErrorCode::BadRequest,
            "tailscale exposure needs a Tailscale admin API token: set \
             TAILSCALE_API_KEY (tskey-api-...) for ryra-api, or add [tailscale] \
             admin_api_key to the agent user's config",
        )
    })?;
    config.tailscale = Some(ryra_core::config::schema::TailscaleConfig {
        admin_api_key,
        tailnet: None,
    });
    paths.ensure_dirs().map_err(core_err)?;
    ryra_core::config::save_config(&paths.config_file, &config).map_err(core_err)?;
    Ok(())
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
            recommended_ram_mb: r.recommended_ram_mb,
        })
        .collect())
}

// -- Tests -------------------------------------------------------------------
//
// Test discovery + execution run here, in the rpc (i.e. as the agent user),
// so the registry resolves in that user's home and test services deploy into
// their rootless podman -- not ryra-api's. Same `ryra_test` engine the
// `ryra test` CLI uses.

/// The default registry dir, cloned if absent. Safe here: rpc runs as the
/// agent user, so the clone lands in their home where the engine reads it.
async fn registry_dir() -> std::result::Result<std::path::PathBuf, RpcError> {
    use ryra_core::registry::resolve::ServiceRef;
    ryra_core::resolve_registry_dir(&ServiceRef::Default(String::new()))
        .await
        .map_err(core_err)
}

async fn list_tests() -> std::result::Result<Vec<RegistryTestView>, RpcError> {
    let dir = registry_dir().await?;
    let discovered = ryra_test::registry::discover(&dir).map_err(core_err)?;
    Ok(discovered
        .iter()
        .map(|t| RegistryTestView {
            name: t.name().to_string(),
            kind: if t.is_lifecycle() {
                "lifecycle".to_string()
            } else {
                "simple".to_string()
            },
            services: t.services().iter().map(|s| s.to_string()).collect(),
            step_count: t.test_count(),
            step_kinds: t.step_kinds().iter().map(|s| s.to_string()).collect(),
            needs_browser: t.needs_browser(),
            requires_sudo: t.requires_sudo(),
        })
        .collect())
}

fn test_state() -> std::result::Result<TestStateView, RpcError> {
    let sandbox = ryra_test::test_sandbox_root().ok_or_else(|| core_err("$HOME not set"))?;
    let tests = ryra_test::reports::scan_results()
        .into_iter()
        .map(|r| TestResultEntryView {
            name: r.name,
            status: r.status,
            duration_ms: r.duration_ms,
            timestamp: r.timestamp,
            has_playwright: r.has_playwright,
        })
        .collect();
    Ok(TestStateView {
        sandbox_path: sandbox.display().to_string(),
        tests,
    })
}

fn remove_test_results(name: Option<&str>) {
    match name {
        Some(n) => {
            let _ = ryra_test::reports::delete_test_result(n);
        }
        None => {
            for r in ryra_test::reports::scan_results() {
                let _ = ryra_test::reports::delete_test_result(&r.name);
            }
        }
    }
}

async fn run_test(name: &str) -> std::result::Result<TestRunView, RpcError> {
    let registry_path = registry_dir().await?;
    let discovered = ryra_test::registry::discover(&registry_path).map_err(core_err)?;
    let test = discovered
        .into_iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("no test named '{name}'")))?;

    // Each test gets an isolated config/data sandbox under the test root.
    let sandbox = ryra_test::test_sandbox_root().ok_or_else(|| core_err("$HOME not set"))?;
    let test_dir = sandbox.join("tests").join(name);
    let config_dir = test_dir.join("config");
    let data_dir = test_dir.join("services");
    let _ = std::fs::remove_dir_all(&config_dir);
    std::fs::create_dir_all(&config_dir).map_err(core_err)?;
    std::fs::create_dir_all(&data_dir).map_err(core_err)?;
    let executor = ryra_test::executor::LocalExecutor::with_registry(&registry_path)
        .with_config_dir(&config_dir)
        .with_data_dir(&data_dir);

    let svcs: Vec<String> = test.services().iter().map(|s| s.to_string()).collect();
    let pre_test: std::collections::BTreeSet<String> = ryra_core::scan_managed_services()
        .unwrap_or_default()
        .into_iter()
        .collect();
    ryra_test::purge_services(&executor, &svcs, "before test").await;

    let result = match &test {
        ryra_test::registry::DiscoveredTest::Lifecycle { steps, .. } => {
            ryra_test::runner::run_lifecycle_test(
                &executor,
                name,
                steps,
                false,
                false,
                &registry_path,
                false,
                None,
            )
            .await
        }
        ryra_test::registry::DiscoveredTest::Simple { .. } => {
            ryra_test::runner::run_registry_test(&executor, &test, false, None).await
        }
    };

    // Clean up the test's services, plus anything it pulled in as a side effect.
    ryra_test::purge_services(&executor, &svcs, "after test").await;
    let leaked: Vec<String> = ryra_core::scan_managed_services()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !pre_test.contains(s) && !svcs.contains(s))
        .collect();
    if !leaked.is_empty() {
        ryra_test::purge_services(&executor, &leaked, "after test (side-effect)").await;
    }

    let _ = ryra_test::reports::save_test_result(&result);
    Ok(scenario_to_view(result))
}

fn scenario_to_view(r: ryra_test::scenario::ScenarioResult) -> TestRunView {
    use ryra_test::scenario::{EventKind, Outcome};
    let passed = r.passed();
    let duration_secs = r.duration.as_secs_f64();
    TestRunView {
        name: r.name,
        passed,
        duration_secs,
        outcome: match &r.outcome {
            Outcome::Passed => "passed".to_string(),
            Outcome::Failed(msg) => msg.clone(),
            Outcome::Skipped => "skipped".to_string(),
        },
        events: r
            .events
            .into_iter()
            .map(|ev| TestEventView {
                description: ev.description,
                kind: match ev.kind {
                    EventKind::Step => "step".to_string(),
                    EventKind::Assertion => "assertion".to_string(),
                },
                passed: ev.outcome.is_pass(),
                skipped: matches!(ev.outcome, Outcome::Skipped),
                error: match ev.outcome {
                    Outcome::Failed(msg) => Some(msg),
                    _ => None,
                },
                duration_secs: ev.duration.as_secs_f64(),
                stdout: ev.stdout,
                stderr: ev.stderr,
            })
            .collect(),
    }
}

/// Restore a service's data from a restic snapshot, running its pre/post
/// restore hooks around the restic restore (the engine half of
/// `ryra backup restore`).
async fn restore(service: &str, snapshot: &str) -> std::result::Result<RestoreOutcome, RpcError> {
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

/// Human label for a backup backend, matching what `ryra backup status` shows.
fn backend_label(backend: &ryra_core::config::schema::BackupBackend) -> String {
    use ryra_core::config::schema::BackupBackend;
    match backend {
        BackupBackend::Local { path } => format!("Local: {}", path.display()),
        BackupBackend::S3 {
            bucket, endpoint, ..
        } => format!("S3: {bucket} ({endpoint})"),
        BackupBackend::Managed => "Ryra-managed".to_string(),
    }
}

/// Build a `restic` command pre-wired with the repo, password, and backend
/// credentials, kept per-invocation rather than polluting the process env.
fn restic_cmd(
    settings: &ryra_core::config::schema::BackupSettings,
    args: &[&str],
) -> std::process::Command {
    let mut cmd = std::process::Command::new("restic");
    cmd.arg("--repo").arg(settings.backend.restic_repo());
    cmd.env("RESTIC_PASSWORD", &settings.password);
    for (key, value) in settings.backend.env() {
        cmd.env(key, value);
    }
    cmd.args(args);
    cmd
}

#[derive(serde::Deserialize)]
struct ResticSnapshot {
    short_id: String,
    time: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// A service's restic data snapshots, newest first. Empty when backups aren't
/// configured (the engine half of `ryra backup list`).
fn snapshots(service: &str) -> std::result::Result<Vec<SnapshotView>, RpcError> {
    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let cfg = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    let Some(settings) = cfg.backup else {
        return Ok(Vec::new());
    };
    let tag = format!("service:{service}");
    let out = restic_cmd(&settings, &["snapshots", "--json", "--tag", &tag])
        .output()
        .map_err(core_err)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(core_err(format!(
            "restic snapshots failed: {}",
            stderr.trim()
        )));
    }
    let parsed: Vec<ResticSnapshot> = serde_json::from_slice(&out.stdout).map_err(core_err)?;
    let mut views: Vec<SnapshotView> = parsed
        .into_iter()
        .map(|s| SnapshotView {
            id: s.short_id,
            time: s.time,
            tags: s.tags,
        })
        .collect();
    views.reverse();
    Ok(views)
}

/// The effective backup configuration plus enrolled services
/// (`ryra backup status`).
fn backup_status() -> std::result::Result<BackupStatusView, RpcError> {
    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let cfg = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    let enrolled = ryra_core::backup::list_backup_enabled().map_err(core_err)?;
    Ok(BackupStatusView {
        configured: cfg.backup.is_some(),
        backend_label: cfg.backup.as_ref().map(|s| backend_label(&s.backend)),
        enrolled,
        retention: cfg
            .backup
            .as_ref()
            .and_then(|s| s.retention.as_ref())
            .map(|r| ryra_protocol::RetentionView {
                keep_last: r.keep_last,
                keep_daily: r.keep_daily,
                keep_weekly: r.keep_weekly,
                keep_monthly: r.keep_monthly,
            }),
    })
}

/// Prune snapshots to the configured retention ladder, per service. Resolves a
/// managed backend to vended S3 creds first (same as a backup run). Services
/// with no policy come back as a zero-effect entry. Returns `(kept, removed)`
/// counts per service.
fn forget_backups(
    service: Option<String>,
    dry_run: bool,
) -> std::result::Result<Vec<ryra_protocol::ForgetView>, RpcError> {
    use ryra_core::config::schema::BackupBackend;
    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let mut cfg = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    let Some(mut settings) = cfg.backup.clone() else {
        return Ok(Vec::new());
    };
    // Managed resolves to short-lived vended S3 creds (and verifies a logged-in
    // account with an active plan), exactly as a backup run does.
    if matches!(settings.backend, BackupBackend::Managed) {
        settings.backend = ryra_core::system::account::resolve_managed_backend().map_err(core_err)?;
    }
    cfg.backup = Some(settings);
    let targets = match service {
        Some(s) => vec![s],
        None => ryra_core::backup::list_backup_enabled().map_err(core_err)?,
    };
    let mut out = Vec::new();
    for svc in targets {
        match ryra_core::backup::plan_backup_forget(&svc, &cfg, dry_run).map_err(core_err)? {
            Some(plan) => {
                let (kept, removed) =
                    ryra_core::backup::restic_forget(&plan).map_err(core_err)?;
                out.push(ryra_protocol::ForgetView {
                    service: svc,
                    kept,
                    removed,
                    dry_run,
                });
            }
            None => out.push(ryra_protocol::ForgetView {
                service: svc,
                kept: 0,
                removed: 0,
                dry_run,
            }),
        }
    }
    Ok(out)
}

/// `restic init`, treating an already-initialised repo as success.
fn restic_init(
    settings: &ryra_core::config::schema::BackupSettings,
) -> std::result::Result<(), RpcError> {
    let out = restic_cmd(settings, &["init"]).output().map_err(core_err)?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("already initialized") || stderr.contains("already exists") {
        return Ok(());
    }
    Err(core_err(format!("restic init failed: {}", stderr.trim())))
}

/// Point backups at a backend: init the restic repo (surfacing auth/bucket
/// errors up front), then persist `[backup]`. `password` is used as-is when
/// given, else the existing repo key is reused (so re-pointing doesn't orphan
/// snapshots under the old key), else a fresh key is generated.
fn configure_backup(
    backend: BackupBackendSpec,
    password: Option<String>,
) -> std::result::Result<(), RpcError> {
    use ryra_core::config::schema::{BackupBackend, BackupSettings};
    // The backend we PERSIST. `Managed` stays `Managed` so each run re-vends
    // fresh short-lived creds; Local/S3 persist verbatim.
    let persist_backend = match backend {
        BackupBackendSpec::Local { path } => BackupBackend::Local {
            path: std::path::PathBuf::from(path),
        },
        BackupBackendSpec::S3 {
            endpoint,
            bucket,
            access_key_id,
            secret_access_key,
            prefix,
        } => BackupBackend::S3 {
            endpoint,
            bucket,
            access_key_id,
            secret_access_key,
            session_token: None,
            prefix,
        },
        BackupBackendSpec::Managed => BackupBackend::Managed,
    };
    // The backend we INIT the restic repo against. `Managed` resolves to vended
    // S3 -- which also verifies a logged-in account WITH an active plan, so
    // configuring managed without a plan fails here, up front. Others init as-is.
    let init_backend = match &persist_backend {
        BackupBackend::Managed => {
            ryra_core::system::account::resolve_managed_backend().map_err(core_err)?
        }
        other => other.clone(),
    };

    let paths = ryra_core::config::ConfigPaths::resolve().map_err(core_err)?;
    let mut cfg = ryra_core::config::load_or_default(&paths.config_file).map_err(core_err)?;
    let password = password
        .or_else(|| cfg.backup.as_ref().map(|b| b.password.clone()))
        .unwrap_or_else(ryra_core::system::secret::generate_secret);

    // Init before persisting, so we only record a [backup] that actually works.
    restic_init(&BackupSettings {
        password: password.clone(),
        backend: init_backend,
        // Retention is irrelevant to init; this value is never persisted.
        retention: None,
    })?;
    let retention = persist_backend.default_retention();
    cfg.backup = Some(BackupSettings {
        password,
        backend: persist_backend,
        retention,
    });
    paths.ensure_dirs().map_err(core_err)?;
    ryra_core::config::save_config(&paths.config_file, &cfg).map_err(core_err)?;
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
    let reg_service = ryra_core::registry::find_service(&repo_dir, name)
        .map_err(|e| RpcError::new(ErrorCode::NotFound, format!("service '{name}': {e}")))?;
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
            .chain(doctor::check_memory(&paths.cache_dir))
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

/// Map any displayable error (io, git, ad-hoc strings) to an internal rpc
/// error. For typed core errors whose *shape* implies a status, use [`op_err`].
fn core_err(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e.to_string())
}

/// Map a planning error from a service operation to the status its shape
/// deserves, so a state conflict (already installed, or leftover state from a
/// prior install) surfaces as a 409 and a missing service as a 404 rather than
/// a blanket 500. Anything unanticipated stays internal.
fn op_err(e: ryra_core::error::Error) -> RpcError {
    use ryra_core::error::Error as E;
    let code = match &e {
        E::ServiceAlreadyInstalled(_) | E::ServiceIncomplete(_) => ErrorCode::Conflict,
        E::ServiceNotInstalled(_) => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    RpcError::new(code, e.to_string())
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
    let activating = super::list::activating_user_units();

    Ok(svcs
        .iter()
        .map(|svc| {
            let inst = by_name.get(svc.service.as_str()).copied();
            let state = if matches!(svc.status, ServiceStatus::Orphan) {
                ServiceState::Removed
            } else if active.contains(&svc.service) {
                ServiceState::Running
            } else if activating.contains(&svc.service) {
                // Unit's start job is still running (image pull, container
                // create, health check): an install/start is in flight, not
                // a genuinely stopped service.
                ServiceState::Installing
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
