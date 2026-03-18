use std::time::{Duration, Instant};

use anyhow::Result;

use crate::machine::Machine;
use crate::registry::{DiscoveredTest, TestEntry};
use crate::scenario::{Event, EventKind, Outcome, ScenarioResult};

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
    let mut events = Vec::new();
    let mut failed = false;

    // Init
    let init_event = run_event(
        vm,
        EventKind::Init,
        &format!("ryra init --repo {repo_path}"),
        30,
    )
    .await;
    if init_event.outcome.is_fail() {
        failed = true;
    }
    events.push(init_event);

    // Deploy each service
    if !failed {
        for service in test.services() {
            let step_event = run_event(
                vm,
                EventKind::Step,
                &format!("ryra add {service} --repo {repo_path}"),
                300,
            )
            .await;

            if step_event.outcome.is_fail() {
                failed = true;
                events.push(step_event);
                break;
            }
            events.push(step_event);

            // Wait for service to be active
            let wait_event = wait_for_service(vm, service).await;
            if wait_event.outcome.is_fail() {
                failed = true;
                events.push(wait_event);
                break;
            }
            events.push(wait_event);
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
            continue;
        }

        let event = run_test_entry(vm, test_entry, &env_prefix).await;
        if event.outcome.is_fail() {
            failed = true;
        }
        events.push(event);
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
async fn build_env_prefix(vm: &Machine, test: &DiscoveredTest) -> Result<String> {
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
    }
}

/// Wait for a service's systemd unit to become active.
async fn wait_for_service(vm: &Machine, service: &str) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(300);
    let result = vm
        .wait_for_service(service, &format!("{service}.service"), timeout)
        .await;

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
