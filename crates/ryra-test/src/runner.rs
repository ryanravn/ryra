use std::time::{Duration, Instant};

use anyhow::Result;

use crate::registry::{DiscoveredTest, StepEntry, TestEntry};
use crate::scenario::{Event, EventKind, Outcome, ScenarioResult};
use ryra_vm::machine::Machine;

fn print_event_result(prefix: &str, event: &Event) {
    let elapsed = format!("{:.1}s", event.duration.as_secs_f64());
    match &event.outcome {
        Outcome::Passed => println!("{prefix}    ok ({elapsed})"),
        Outcome::Failed(msg) => println!("{prefix}    FAIL ({elapsed}) — {msg}"),
        Outcome::Skipped => println!("{prefix}    skip"),
    }
}

/// Execute a registry-defined test suite inside a VM.
///
/// 1. Runs `ryra init`
/// 2. Deploys each service with `ryra add`
/// 3. Sources `.env` files (unprefixed for single-service, prefixed for multi)
/// 4. Runs each test command via SSH, checks exit code
pub async fn run_registry_test(
    vm: &Machine,
    test: &DiscoveredTest,
    repo_path: &str,
) -> ScenarioResult {
    let start = Instant::now();
    let name = test.name();
    let mut events = Vec::new();
    let mut failed = false;

    // Init
    println!("[{name}]   ryra init...");
    let init_event = run_event(
        vm,
        EventKind::Init,
        &format!("ryra init --repo {repo_path}"),
        30,
    )
    .await;
    print_event_result(name, &init_event);
    if init_event.outcome.is_fail() {
        failed = true;
    }
    events.push(init_event);

    // Collect env overrides from all tests — these may include values for
    // required env vars that `ryra add` needs to succeed non-interactively.
    let mut add_env_prefix = String::new();
    {
        let mut combined: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for entry in test.tests() {
            for (key, val) in &entry.env {
                combined.entry(key.clone()).or_insert_with(|| val.clone());
            }
        }
        if !combined.is_empty() {
            let exports: Vec<String> = combined.iter().map(|(k, v)| format!("{k}={v}")).collect();
            add_env_prefix = exports.join(" ") + " ";
        }
    }

    // Deploy each service
    if !failed {
        for service in test.services() {
            println!("[{name}]   ryra add {service}...");
            let step_event = run_event(
                vm,
                EventKind::Step,
                &format!("{add_env_prefix}ryra add {service} --repo {repo_path}"),
                300,
            )
            .await;
            print_event_result(name, &step_event);

            if step_event.outcome.is_fail() {
                failed = true;
                events.push(step_event);
                break;
            }
            events.push(step_event);

            // Wait for service to be active
            println!("[{name}]   waiting for {service} to start...");
            let wait_event = wait_for_service(vm, service).await;
            print_event_result(name, &wait_event);
            if wait_event.outcome.is_fail() {
                failed = true;
                events.push(wait_event);
                break;
            }
            events.push(wait_event);
        }
    }

    // Wait for declared ports to be reachable (services may need startup time
    // beyond what systemd "active" indicates).
    if !failed {
        for service in test.services() {
            let port_cmd =
                format!("grep RYRA_PORT /var/lib/{service}/.env 2>/dev/null | cut -d= -f2");
            if let Ok(out) = vm.exec(&port_cmd).await {
                for port in out.stdout.trim().lines() {
                    let port = port.trim();
                    if port.is_empty() {
                        continue;
                    }
                    println!("[{name}]   waiting for port {port}...");
                    let port_event = wait_for_port(vm, name, port).await;
                    print_event_result(name, &port_event);
                    if port_event.outcome.is_fail() {
                        failed = true;
                        events.push(port_event);
                        break;
                    }
                    events.push(port_event);
                }
            }
            if failed {
                break;
            }
        }
    }

    // Build the env sourcing prefix for test commands
    let env_prefix = if !failed {
        match build_env_prefix(vm, test).await {
            Ok(prefix) => prefix,
            Err(e) => {
                failed = true;
                events.push(Event {
                    description: "source service env vars".to_string(),
                    kind: EventKind::Step,
                    outcome: Outcome::Failed(format!("{e:#}")),
                    duration: Duration::ZERO,
                });
                String::new()
            }
        }
    } else {
        String::new()
    };

    // Run each test command
    for test_entry in test.tests() {
        if failed {
            events.push(Event {
                description: format!("test: {}", test_entry.name),
                kind: EventKind::Assertion,
                outcome: Outcome::Skipped,
                duration: Duration::ZERO,
            });
            println!("[{name}]   skip  {}", test_entry.name);
            continue;
        }

        println!("[{name}]   test  {}...", test_entry.name);
        let event = run_test_entry(vm, test_entry, &env_prefix).await;
        print_event_result(name, &event);
        if event.outcome.is_fail() {
            failed = true;
        }
        events.push(event);
    }

    // Dump diagnostics on failure
    if failed {
        dump_diagnostics(vm, name, &test.services()).await;
    }

    let outcome = if failed {
        let first_failure = events
            .iter()
            .find_map(|e| match &e.outcome {
                Outcome::Failed(msg) => Some(msg.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown failure".to_string());
        Outcome::Failed(first_failure)
    } else {
        Outcome::Passed
    };

    ScenarioResult {
        name: test.name().to_string(),
        events,
        duration: start.elapsed(),
        outcome,
    }
}

/// Run a single test entry — wraps the `run` command with env sourcing and timeout.
async fn run_test_entry(vm: &Machine, entry: &TestEntry, env_prefix: &str) -> Event {
    let t = Instant::now();

    // Build the full command: env overrides + env sourcing + the test command
    let mut parts = Vec::new();

    // Inline env overrides from the test definition
    for (key, val) in &entry.env {
        parts.push(format!("export {key}={val}"));
    }

    // Source the env files
    if !env_prefix.is_empty() {
        parts.push(env_prefix.to_string());
    }

    // The actual test command
    parts.push(entry.run.clone());

    let full_cmd = parts.join(" && ");

    // Run with timeout
    let timeout = Duration::from_secs(entry.timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec(&full_cmd)).await;

    let outcome = match result {
        Ok(Ok(_)) => Outcome::Passed,
        Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
        Err(_) => Outcome::Failed(format!("timed out after {}s", entry.timeout_secs)),
    };

    Event {
        description: format!("test: {}", entry.name),
        kind: EventKind::Assertion,
        outcome,
        duration: t.elapsed(),
    }
}

/// Build a shell snippet that sources all relevant .env files.
///
/// Single-service: `. /var/lib/<service>/.env` (unprefixed)
/// Multi-service: reads each .env and exports with SERVICE__ prefix
async fn build_env_prefix(_vm: &Machine, test: &DiscoveredTest) -> Result<String> {
    match test {
        DiscoveredTest::SingleService { service_name, .. } => {
            Ok(format!(". /var/lib/{service_name}/.env"))
        }
        DiscoveredTest::MultiService { services, .. } => {
            // For multi-service, we generate a script that reads each .env
            // and re-exports vars with the service name prefix
            let mut lines = Vec::new();
            for service in services {
                let prefix = service.to_uppercase();
                // Read each line from the .env, prefix the var name, export it
                lines.push(format!(
                    "while IFS='=' read -r key val; do \
                     [ -n \"$key\" ] && export {prefix}__$key=\"$val\"; \
                     done < /var/lib/{service}/.env"
                ));
            }
            Ok(lines.join(" && "))
        }
        DiscoveredTest::Lifecycle { .. } => {
            // Lifecycle tests handle env sourcing within their step commands
            Ok(String::new())
        }
    }
}

/// Wait for a service's systemd unit to become active.
async fn wait_for_service(vm: &Machine, service: &str) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(300);

    let unit = format!("{service}.service");
    let result = vm.wait_for_service(service, &unit, timeout).await;

    let outcome = match result {
        Ok(()) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("service didn't start: {e:#}")),
    };

    Event {
        description: format!("wait for {service}"),
        kind: EventKind::Step,
        outcome,
        duration: t.elapsed(),
    }
}

/// Execute a lifecycle test — interleaved actions and assertions.
///
/// Unlike `run_registry_test`, this processes a sequence of typed steps
/// (add, remove, reset, wait, run, assert) rather than "add all then test".
pub async fn run_lifecycle_test(
    vm: &Machine,
    name: &str,
    steps: &[StepEntry],
    repo_path: &str,
    verbose: bool,
    single_test: bool,
) -> ScenarioResult {
    let start = Instant::now();
    let mut events = Vec::new();
    let mut failed = false;
    let p = if single_test {
        String::new()
    } else {
        format!("[{name}] ")
    };
    let stream_prefix = if single_test { "" } else { name };

    // Init first (all lifecycle tests start with ryra init)
    println!("{p}  ryra init...");
    let init_event = run_event(
        vm,
        EventKind::Init,
        &format!("ryra init --repo {repo_path}"),
        30,
    )
    .await;
    print_event_result(&p, &init_event);
    if init_event.outcome.is_fail() {
        failed = true;
    }
    events.push(init_event);

    for step in steps {
        if failed {
            let desc = lifecycle_step_description(step);
            let kind = lifecycle_step_kind(step);
            events.push(Event {
                description: desc.clone(),
                kind,
                outcome: Outcome::Skipped,
                duration: Duration::ZERO,
            });
            println!("{p}  skip  {desc}");
            continue;
        }

        match step {
            StepEntry::Add { service } => {
                println!("{p}  ryra add {service}...");
                let event = run_event(
                    vm,
                    EventKind::Step,
                    &format!("ryra add {service} --repo {repo_path}"),
                    300,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepEntry::Remove { service } => {
                println!("{p}  ryra remove {service}...");
                let event = run_event(
                    vm,
                    EventKind::Step,
                    &format!("ryra remove {service} -y"),
                    120,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepEntry::Reset => {
                println!("{p}  ryra reset...");
                let event = run_event(vm, EventKind::Step, "ryra reset -y", 120).await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepEntry::Wait { service } => {
                println!("{p}  waiting for {service}...");
                let event = wait_for_service(vm, service).await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepEntry::Run {
                name: step_name,
                run,
                timeout_secs,
            } => {
                println!("{p}  run: {step_name}...");
                let event = if verbose {
                    run_event_streaming(vm, stream_prefix, EventKind::Step, run, *timeout_secs)
                        .await
                } else {
                    run_event(vm, EventKind::Step, run, *timeout_secs).await
                };
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepEntry::Assert {
                name: step_name,
                run,
                timeout_secs,
            } => {
                println!("{p}  assert: {step_name}...");
                let t = Instant::now();
                let timeout = Duration::from_secs(*timeout_secs);
                let result = tokio::time::timeout(timeout, vm.exec(run)).await;

                let outcome = match result {
                    Ok(Ok(_)) => Outcome::Passed,
                    Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
                    Err(_) => Outcome::Failed(format!("timed out after {timeout_secs}s")),
                };

                let event = Event {
                    description: format!("assert: {step_name}"),
                    kind: EventKind::Assertion,
                    outcome,
                    duration: t.elapsed(),
                };
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
        }
    }

    let outcome = if failed {
        let first_failure = events
            .iter()
            .find_map(|e| match &e.outcome {
                Outcome::Failed(msg) => Some(msg.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown failure".to_string());
        Outcome::Failed(first_failure)
    } else {
        Outcome::Passed
    };

    ScenarioResult {
        name: name.to_string(),
        events,
        duration: start.elapsed(),
        outcome,
    }
}

fn lifecycle_step_description(step: &StepEntry) -> String {
    match step {
        StepEntry::Add { service } => format!("ryra add {service}"),
        StepEntry::Remove { service } => format!("ryra remove {service}"),
        StepEntry::Reset => "ryra reset".to_string(),
        StepEntry::Wait { service } => format!("wait for {service}"),
        StepEntry::Run { name, .. } => format!("run: {name}"),
        StepEntry::Assert { name, .. } => format!("assert: {name}"),
    }
}

fn lifecycle_step_kind(step: &StepEntry) -> EventKind {
    match step {
        StepEntry::Assert { .. } => EventKind::Assertion,
        _ => EventKind::Step,
    }
}

/// Wait for a port to accept TCP connections (not just be bound by rootlessport).
///
/// Uses bash `/dev/tcp` to test actual TCP connectivity through to the
/// container, not just whether rootlessport is listening on the host side.
async fn wait_for_port(vm: &Machine, test_name: &str, port: &str) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(300);
    let mut last_log = std::time::Instant::now();
    // First few seconds: rootlessport is listening but the container app
    // may not be ready yet. A successful bash /dev/tcp probe means the
    // connection made it all the way to the container.
    loop {
        let cmd = format!("bash -c 'echo > /dev/tcp/127.0.0.1/{port}' 2>/dev/null");
        if vm.exec(&cmd).await.is_ok() {
            return Event {
                description: format!("port {port} ready"),
                kind: EventKind::Step,
                outcome: Outcome::Passed,
                duration: t.elapsed(),
            };
        }

        if t.elapsed() > timeout {
            return Event {
                description: format!("port {port} ready"),
                kind: EventKind::Step,
                outcome: Outcome::Failed(format!(
                    "port {port} not responding after {}s",
                    timeout.as_secs()
                )),
                duration: t.elapsed(),
            };
        }

        if last_log.elapsed().as_secs() >= 30 {
            println!(
                "[{test_name}]     still waiting for port {port}... ({:.0}s)",
                t.elapsed().as_secs_f64()
            );
            last_log = std::time::Instant::now();
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Dump diagnostic info for each service when a test fails.
async fn dump_diagnostics(vm: &Machine, test_name: &str, services: &[&str]) {
    println!("[{test_name}] --- diagnostics ---");
    for svc in services {
        // Systemd service status
        let cmd = format!(
            "systemctl --machine={svc}@ --user status {svc}.service 2>&1 | head -20 || true"
        );
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] systemd status:");
                for line in trimmed.lines() {
                    println!("[{test_name}]     {line}");
                }
            }
        }

        // Container status
        let cmd = format!(
            "cd / && sudo -H -u {svc} podman ps -a --format '{{{{.Names}}}} {{{{.Status}}}} {{{{.Ports}}}}' 2>&1 || true"
        );
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] containers: {trimmed}");
            } else {
                println!("[{test_name}]   [{svc}] containers: (none)");
            }
        }

        // Container/journal logs
        let uid_cmd = format!("id -u {svc} 2>/dev/null || echo 0");
        let uid = vm
            .exec(&uid_cmd)
            .await
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_else(|_| "0".to_string());
        let cmd = format!(
            "sudo -u {svc} XDG_RUNTIME_DIR=/run/user/{uid} journalctl --user -u {svc}.service --no-pager -n 30 2>&1 || true"
        );
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] logs:");
                for line in trimmed.lines().take(30) {
                    println!("[{test_name}]     {line}");
                }
            }
        }

        // Env file
        let cmd = format!("cat /var/lib/{svc}/.env 2>&1 | grep RYRA_PORT || true");
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] ports: {trimmed}");
            }
        }

        // Check quadlet, container internals, and network
        let cmd = format!(
            "echo '=== quadlet ==='; grep -i exec /var/lib/{svc}/.config/containers/systemd/{svc}.container 2>/dev/null || true; \
             echo '=== container process ==='; cd / && sudo -H -u {svc} podman exec systemd-{svc} ps aux 2>&1 | head -10 || true; \
             echo '=== container listeners ==='; cd / && sudo -H -u {svc} podman exec systemd-{svc} cat /proc/net/tcp6 2>&1 | head -10 || true; \
             echo '=== host listeners ==='; ss -tlnp 2>/dev/null | head -20; \
             echo '=== curl ==='; curl -sv http://127.0.0.1:18789/ 2>&1 | head -10 || true"
        );
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            println!("[{test_name}]   [{svc}] network:");
            for line in trimmed.lines() {
                println!("[{test_name}]     {line}");
            }
        }
    }
    println!("[{test_name}] --- end diagnostics ---");
}

/// Run a command in the VM with real-time output streaming and return an Event.
async fn run_event_streaming(
    vm: &Machine,
    test_name: &str,
    kind: EventKind,
    cmd: &str,
    timeout_secs: u64,
) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec_streaming(cmd, test_name)).await;

    let outcome = match result {
        Ok(Ok(_)) => Outcome::Passed,
        Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
        Err(_) => Outcome::Failed(format!("timed out after {timeout_secs}s")),
    };

    Event {
        description: cmd.to_string(),
        kind,
        outcome,
        duration: t.elapsed(),
    }
}

/// Run a command in the VM and return an Event.
async fn run_event(vm: &Machine, kind: EventKind, cmd: &str, timeout_secs: u64) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec(cmd)).await;

    let outcome = match result {
        Ok(Ok(_)) => Outcome::Passed,
        Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
        Err(_) => Outcome::Failed(format!("timed out after {timeout_secs}s")),
    };

    Event {
        description: cmd.to_string(),
        kind,
        outcome,
        duration: t.elapsed(),
    }
}
