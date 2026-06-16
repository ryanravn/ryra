//! End-to-end test of the blue/green *upgrade swap* plan, run entirely inside
//! a tempdir — no podman, no systemd. It fakes an installed blue/green service
//! (metadata + `.env` + a path registry), calls `blue_green_swap`, and asserts
//! the emitted plan does the right zero-downtime choreography.
//!
//! The runtime swap itself (Caddy graceful reload = zero dropped connections)
//! is proven separately against live containers; this pins the *planning*: the
//! right slot is started, the health gate hits the idle port, the old slot is
//! stopped, and `active_color` flips.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use ryra_core::Step;

/// These tests mutate process-global env vars, so they can't run in parallel.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("env lock poisoned")
}

/// A faked blue/green install in a tempdir: a path registry holding the
/// service.toml + main quadlet, plus the on-disk install state (metadata.toml
/// + .env with the allocated color port pair).
struct Sandbox {
    _tmp: tempfile::TempDir,
}

impl Sandbox {
    fn new(service: &str, active_color: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let home: PathBuf = tmp.path().to_path_buf();
        // SAFETY: guarded by env_lock() in each test; set before any path
        // helper reads them.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            std::env::set_var("XDG_STATE_HOME", home.join(".local/state"));
            std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        }

        // Path registry: <registry>/<svc>/{service.toml, quadlets/<svc>.container}
        let registry_dir = home.join("fake-registry");
        let service_dir = registry_dir.join(service);
        std::fs::create_dir_all(service_dir.join("quadlets")).expect("svc registry");
        std::fs::write(
            service_dir.join("service.toml"),
            format!(
                r#"
[service]
name = "{service}"
description = "test service"
runtime = "podman"
deploy = "blue-green"
health_check = "/healthz"

[[ports]]
name = "http"
container_port = 8080
"#,
            ),
        )
        .expect("write service.toml");
        std::fs::write(
            service_dir.join("quadlets").join(format!("{service}.container")),
            format!(
                "[Container]\n\
                 Image=docker.io/traefik/whoami:latest\n\
                 ContainerName={service}\n\
                 PublishPort=${{SERVICE_PORT_HTTP}}:8080\n\
                 EnvironmentFile=%h/.local/share/services/{service}/.env\n\
                 \n\
                 [Service]\n\
                 EnvironmentFile=%h/.local/share/services/{service}/.env\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n",
            ),
        )
        .expect("write quadlet");

        // On-disk install state. registry points at our path registry so the
        // swap resolves the def without a network. The .env carries the color
        // port pair the add path would have allocated.
        let service_home = home.join(".local/share/services").join(service);
        std::fs::create_dir_all(&service_home).expect("svc home");
        std::fs::write(
            service_home.join("metadata.toml"),
            format!(
                "registry = \"{}\"\nactive_color = \"{active_color}\"\n",
                registry_dir.display()
            ),
        )
        .expect("write metadata");
        std::fs::write(
            service_home.join(".env"),
            "SERVICE_HOME=/tmp\n\
             SERVICE_PORT_HTTP=19001\n\
             SERVICE_PORT_HTTP_BLUE=19001\n\
             SERVICE_PORT_HTTP_GREEN=19002\n",
        )
        .expect("write env");

        // Installed quadlets: both color slots in the quadlet dir, each carrying
        // the `# Service-Source: registry/<svc>` marker that scan_managed_services
        // (and hence is_service_installed) keys on.
        let quadlet_dir = home.join(".config/containers/systemd");
        std::fs::create_dir_all(&quadlet_dir).expect("quadlet dir");
        for color in ["blue", "green"] {
            std::fs::write(
                quadlet_dir.join(format!("{service}-{color}.container")),
                format!("# Service-Source: registry/{service}\n[Container]\nContainerName={service}-{color}\n"),
            )
            .expect("write installed quadlet");
        }

        Sandbox { _tmp: tmp }
    }
}

fn started(steps: &[Step]) -> Vec<String> {
    steps.iter().filter_map(|s| match s {
        Step::StartService { unit } => Some(unit.clone()),
        _ => None,
    }).collect()
}

fn stopped(steps: &[Step]) -> Vec<String> {
    steps.iter().filter_map(|s| match s {
        Step::StopService { unit } => Some(unit.clone()),
        _ => None,
    }).collect()
}

#[tokio::test]
async fn swap_from_blue_rolls_onto_green_and_flips_color() {
    let _guard = env_lock();
    let _sb = Sandbox::new("demo", "blue");

    let plan = ryra_core::upgrade::blue_green_swap("demo")
        .await
        .expect("swap plans")
        .expect("service is blue/green");

    // Starts green, stops blue.
    assert!(started(&plan.steps).contains(&"demo-green".to_string()), "started: {:?}", started(&plan.steps));
    assert!(stopped(&plan.steps).contains(&"demo-blue".to_string()), "stopped: {:?}", stopped(&plan.steps));
    assert!(!started(&plan.steps).contains(&"demo-blue".to_string()));

    // Health gate hits the *idle* (green) port + the declared path.
    let health = plan.steps.iter().find_map(|s| match s {
        Step::WaitForHttpHealthy { url, .. } => Some(url.clone()),
        _ => None,
    }).expect("has a health gate");
    assert_eq!(health, "http://127.0.0.1:19002/healthz", "got {health}");

    // Ordering: start green BEFORE stop blue (overlap = zero downtime).
    let start_idx = plan.steps.iter().position(|s| matches!(s, Step::StartService { unit } if unit == "demo-green")).unwrap();
    let health_idx = plan.steps.iter().position(|s| matches!(s, Step::WaitForHttpHealthy { .. })).unwrap();
    let stop_idx = plan.steps.iter().position(|s| matches!(s, Step::StopService { unit } if unit == "demo-blue")).unwrap();
    assert!(start_idx < health_idx, "start must precede health gate");
    assert!(health_idx < stop_idx, "health gate must precede stopping the old slot");

    // active_color flips to green in the metadata write.
    let meta = plan.steps.iter().rev().find_map(|s| match s {
        Step::WriteFile(f) if f.path.ends_with("metadata.toml") => Some(f.content.clone()),
        _ => None,
    }).expect("metadata write");
    assert!(meta.contains("active_color = \"green\""), "metadata: {meta}");

    // It's a swap, so force the apply even on a clean config diff.
    assert!(plan.force_apply);
}

#[tokio::test]
async fn swap_from_green_rolls_back_onto_blue() {
    let _guard = env_lock();
    let _sb = Sandbox::new("demo", "green");

    let plan = ryra_core::upgrade::blue_green_swap("demo")
        .await
        .expect("swap plans")
        .expect("service is blue/green");

    assert!(started(&plan.steps).contains(&"demo-blue".to_string()));
    assert!(stopped(&plan.steps).contains(&"demo-green".to_string()));
    let health = plan.steps.iter().find_map(|s| match s {
        Step::WaitForHttpHealthy { url, .. } => Some(url.clone()),
        _ => None,
    }).unwrap();
    // Rolling back onto blue -> health-checks the blue port.
    assert_eq!(health, "http://127.0.0.1:19001/healthz");
}
