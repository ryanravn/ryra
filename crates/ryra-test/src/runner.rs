use std::time::{Duration, Instant};

use anyhow::Result;

use crate::executor::Executor;
use crate::registry::{DiscoveredTest, TestEntry};
use crate::scenario::{Event, EventKind, Outcome, ScenarioResult};
use crate::test_toml::StepDef;

fn print_event_result(prefix: &str, event: &Event) {
    let elapsed = format!("{:.1}s", event.duration.as_secs_f64());
    match &event.outcome {
        Outcome::Passed => println!("{prefix}    ok ({elapsed})"),
        Outcome::Failed(msg) => println!("{prefix}    FAIL ({elapsed}) — {msg}"),
        Outcome::Skipped => println!("{prefix}    skip"),
    }
}

/// Execute a simple (non-lifecycle) test suite inside a VM.
///
/// 1. Runs `ryra init` and deploys registry services with `ryra add`
/// 2. Waits for declared ports
/// 3. If quadlets are present, copies them to systemd dir, reloads, starts them
/// 4. Sources `.env` files
/// 5. Runs each test command via SSH, checks exit code
pub async fn run_registry_test(vm: &dyn Executor, test: &DiscoveredTest) -> ScenarioResult {
    let start = Instant::now();
    let name = test.name();
    let mut events = Vec::new();
    let mut failed = false;

    let (services, quadlets) = match test {
        DiscoveredTest::Simple { setup, .. } => (&setup.services, &setup.quadlets),
        DiscoveredTest::Lifecycle { .. } => {
            // Lifecycle tests should use run_lifecycle_test instead
            return ScenarioResult {
                name: name.to_string(),
                events: vec![],
                duration: start.elapsed(),
                outcome: Outcome::Failed("run_registry_test called for lifecycle test".to_string()),
            };
        }
    };

    // Init
    if !services.is_empty() || !quadlets.is_empty() {
        println!("[{name}]   ryra init...");
        let init_event = run_event(vm, EventKind::Init, "ryra init", 30).await;
        print_event_result(name, &init_event);
        if init_event.outcome.is_fail() {
            failed = true;
        }
        events.push(init_event);
    }

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

    // Deploy each registry service
    if !failed {
        for service in services {
            println!("[{name}]   ryra add {service}...");
            let step_event = run_event(
                vm,
                EventKind::Step,
                &format!("{add_env_prefix}ryra add {service}"),
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
        for service in services {
            let port_cmd = format!(
                "grep RYRA_PORT $HOME/.local/share/ryra/{service}/.env 2>/dev/null | cut -d= -f2"
            );
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

    // Deploy quadlet files if present
    if !failed && !quadlets.is_empty() {
        println!("[{name}]   deploying quadlet files...");
        let deploy_cmd = "\
            mkdir -p $HOME/.config/containers/systemd && \
            cp /opt/ryra-test-project/*.container $HOME/.config/containers/systemd/ 2>/dev/null; \
            cp /opt/ryra-test-project/*.volume $HOME/.config/containers/systemd/ 2>/dev/null; \
            cp /opt/ryra-test-project/*.network $HOME/.config/containers/systemd/ 2>/dev/null; \
            cp /opt/ryra-test-project/*.pod $HOME/.config/containers/systemd/ 2>/dev/null; \
            systemctl --user daemon-reload";
        let deploy_event = run_event(vm, EventKind::Step, deploy_cmd, 30).await;
        print_event_result(name, &deploy_event);
        if deploy_event.outcome.is_fail() {
            failed = true;
        }
        events.push(deploy_event);

        // Derive service names from .container file stems, start each
        if !failed {
            let quadlet_services: Vec<&str> = quadlets
                .iter()
                .filter(|q| q.ends_with(".container"))
                .filter_map(|q| q.strip_suffix(".container"))
                .collect();

            for svc in &quadlet_services {
                println!("[{name}]   starting {svc}...");
                let start_cmd = format!("systemctl --user start {svc}.service");
                let start_event = run_event(vm, EventKind::Step, &start_cmd, 120).await;
                print_event_result(name, &start_event);
                if start_event.outcome.is_fail() {
                    failed = true;
                    events.push(start_event);
                    break;
                }
                events.push(start_event);

                println!("[{name}]   waiting for {svc}...");
                let wait_event = wait_for_service(vm, svc).await;
                print_event_result(name, &wait_event);
                if wait_event.outcome.is_fail() {
                    failed = true;
                    events.push(wait_event);
                    break;
                }
                events.push(wait_event);
            }
        }
    }

    // Build the env sourcing prefix for test commands
    let env_prefix = if !failed {
        match build_env_prefix(vm, test).await {
            Ok(prefix) => prefix,
            Err(e) => {
                failed = true;
                events.push(Event::bare(
                    "source service env vars".to_string(),
                    EventKind::Step,
                    Outcome::Failed(format!("{e:#}")),
                    Duration::ZERO,
                ));
                String::new()
            }
        }
    } else {
        String::new()
    };

    // Run each test command
    for test_entry in test.tests() {
        if failed {
            events.push(Event::bare(
                format!("test: {}", test_entry.name),
                EventKind::Assertion,
                Outcome::Skipped,
                Duration::ZERO,
            ));
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
async fn run_test_entry(vm: &dyn Executor, entry: &TestEntry, env_prefix: &str) -> Event {
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

    Event::bare(
        format!("test: {}", entry.name),
        EventKind::Assertion,
        outcome,
        t.elapsed(),
    )
}

/// Shell snippet that loads an .env file into the current shell's exported
/// environment **safely** — `read`-per-line + single split on `=`, so values
/// containing whitespace or shell metacharacters (e.g. supabase's
/// `DB_AFTER_CONNECT_QUERY=SET search_path TO …` or `ERL_AFLAGS=-proto_dist
/// inet_tcp`) don't get parsed as inline commands like `. file` would.
///
/// The .env format ryra writes intentionally avoids quoting so podman's
/// --env-file can consume it verbatim (podman does NOT strip quotes), which
/// means bash `source` / `.` is unsafe against any value with whitespace.
fn load_env_shell(path: &str) -> String {
    format!(
        "while IFS='=' read -r __k __v; do \
         case \"$__k\" in \"\"|\\#*) continue ;; esac; \
         export \"$__k=$__v\"; \
         done < {path}"
    )
}

/// Build a shell snippet that sources all relevant .env files.
///
/// Single-service: loads `<service>/.env` into the current shell unprefixed.
/// Multi-service: reads each .env and exports with SERVICE__ prefix.
async fn build_env_prefix(_vm: &dyn Executor, test: &DiscoveredTest) -> Result<String> {
    match test {
        DiscoveredTest::Simple { setup, .. } => {
            if setup.services.len() == 1 {
                Ok(load_env_shell(&format!(
                    "$HOME/.local/share/ryra/{}/.env",
                    setup.services[0]
                )))
            } else if setup.services.len() > 1 {
                // For multi-service, we generate a script that reads each .env
                // and re-exports vars with the service name prefix
                let mut lines = Vec::new();
                for service in &setup.services {
                    let prefix = service.to_uppercase();
                    lines.push(format!(
                        "while IFS='=' read -r key val; do \
                         case \"$key\" in \"\"|\\#*) continue ;; esac; \
                         export {prefix}__$key=\"$val\"; \
                         done < $HOME/.local/share/ryra/{service}/.env"
                    ));
                }
                Ok(lines.join(" && "))
            } else {
                Ok(String::new())
            }
        }
        DiscoveredTest::Lifecycle { .. } => {
            // Lifecycle tests handle env sourcing within their step commands
            Ok(String::new())
        }
    }
}

/// Wait for a service's systemd unit to become active (default 60s timeout).
async fn wait_for_service(vm: &dyn Executor, service: &str) -> Event {
    wait_for_service_with_timeout(vm, service, 60).await
}

/// Wait for a service's systemd unit to become active with a custom timeout.
async fn wait_for_service_with_timeout(
    vm: &dyn Executor,
    service: &str,
    timeout_secs: u64,
) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    let unit = format!("{service}.service");
    let result = vm.wait_for_service(&unit, timeout).await;

    let outcome = match result {
        Ok(()) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("service didn't start: {e:#}")),
    };

    Event::bare(
        format!("wait for {service}"),
        EventKind::Step,
        outcome,
        t.elapsed(),
    )
}

/// Escape a value for embedding inside a single-quoted shell string.
fn shell_escape(s: &str) -> String {
    s.replace('\'', r"'\''")
}

/// Run a Playwright spec and save the HTML report.
async fn run_browser_step(
    vm: &dyn Executor,
    test_name: &str,
    step_name: &str,
    spec: &str,
    env: &std::collections::BTreeMap<String, String>,
    timeout_secs: u64,
    registry_path: &std::path::Path,
) -> Event {
    let t = Instant::now();

    // Build env exports from the step's env map. Values are shell-quoted.
    let mut env_exports = String::new();
    for (key, val) in env {
        let quoted = shell_escape(val);
        env_exports.push_str(&format!("export {key}='{quoted}' && "));
    }

    let browser_dir = format!("{}/tests/browser", registry_path.display());
    let browser_dir_esc = shell_escape(&browser_dir);
    let spec_esc = shell_escape(spec);
    let test_name_esc = shell_escape(test_name);

    // Shell command:
    // 1. Source all service .env files so port vars (RYRA_PORT_HTTP etc.) are
    //    available to the spec. Static env from the toml step overrides these.
    // 2. Pre-create the canonical report directory and tell playwright to
    //    emit the HTML report directly there (no intermediate copy step).
    // 3. cd into the browser test dir.
    // 4. Ensure node_modules exists — symlink /opt/playwright/node_modules
    //    in the VM image, or `bun install` on a bare host.
    // 5. Export env vars from the step (overrides sourced vars).
    // 6. Run playwright with the html reporter pointed at the canonical path.
    //    Also use the list reporter so the user sees live progress.
    // 7. Exit with playwright's own exit code.
    // Per-file safe .env load — same rationale as load_env_shell(): raw
    // `.` would choke on values with whitespace (supabase `DB_AFTER_*`, etc.)
    let env_loop = format!(
        "for __f in $HOME/.local/share/ryra/*/.env; do \
           [ -f \"$__f\" ] && {loader}; \
         done",
        loader = load_env_shell("\"$__f\"")
    );
    let cmd = format!(
        "{env_loop} && \
         DEST=\"$HOME/.local/share/ryra/test-reports/{test_name_esc}/playwright\" && \
         mkdir -p \"$DEST\" && \
         cd '{browser_dir_esc}' && \
         if [ ! -d node_modules ]; then \
           if [ -d /opt/playwright/node_modules ]; then \
             ln -sf /opt/playwright/node_modules .; \
           else \
             bun install playwright @playwright/test 2>&1; \
           fi; \
         fi && \
         export PATH=\"$HOME/.bun/bin:$PATH\" && \
         export PLAYWRIGHT_HTML_REPORT=\"$DEST\" && \
         export PLAYWRIGHT_HTML_OPEN=never && \
         {env_exports}\
         bunx playwright test '{spec_esc}' --reporter=list,html"
    );

    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec_streaming(&cmd, test_name)).await;

    let outcome = match result {
        Ok(Ok(_)) => Outcome::Passed,
        Ok(Err(e)) => Outcome::Failed(format!("{e:#}")),
        Err(_) => Outcome::Failed(format!("timed out after {timeout_secs}s")),
    };

    // Pull the playwright report out of the execution environment so it lives
    // at the canonical host path regardless of VM or bare mode. No-op on bare.
    // We always try — even on failure, because traces on failure are the
    // most valuable ones to inspect.
    if let Ok(home) = std::env::var("HOME") {
        let local_dir = std::path::PathBuf::from(&home)
            .join(".local/share/ryra/test-reports")
            .join(test_name)
            .join("playwright");
        let remote_dir =
            format!("/home/ryra/.local/share/ryra/test-reports/{test_name}/playwright");
        if let Err(e) = vm.fetch_dir(&remote_dir, &local_dir).await {
            eprintln!("warning: failed to fetch playwright report: {e:#}");
        }
    }

    Event::bare(
        format!("browser: {step_name}"),
        EventKind::Assertion,
        outcome,
        t.elapsed(),
    )
}

/// Execute a lifecycle test — interleaved actions and assertions.
///
/// Unlike `run_registry_test`, this processes a sequence of typed steps
/// (add, remove, reset, wait, run, assert) rather than "add all then test".
pub async fn run_lifecycle_test(
    vm: &dyn Executor,
    name: &str,
    steps: &[StepDef],
    verbose: bool,
    single_test: bool,
    registry_path: &std::path::Path,
    retest: bool,
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
    if !retest {
        println!("{p}  ryra init...");
        let init_event = run_event(vm, EventKind::Init, "ryra init", 30).await;
        print_event_result(&p, &init_event);
        if init_event.outcome.is_fail() {
            failed = true;
        }
        events.push(init_event);
    }

    for step in steps {
        // In retest mode, skip setup steps and only run test/assertion steps.
        if retest && step.is_setup() {
            let desc = step.step_name();
            println!("{p}  skip  {desc} (retest)");
            continue;
        }

        if failed {
            let desc = step.step_name();
            events.push(Event::bare(
                desc.clone(),
                EventKind::Step,
                Outcome::Skipped,
                Duration::ZERO,
            ));
            println!("{p}  skip  {desc}");
            continue;
        }

        match step {
            StepDef::Add {
                service,
                args,
                env,
                timeout,
            } => {
                println!("{p}  ryra add {service}...");
                let mut cmd = String::new();
                for (key, val) in env {
                    let escaped = shell_escape(val);
                    cmd.push_str(&format!("{key}='{escaped}' "));
                }
                cmd.push_str(&format!("ryra add {service}"));
                if let Some(a) = args.as_deref()
                    && !a.is_empty()
                {
                    cmd.push_str(&format!(" {a}"));
                }
                let event = run_event(vm, EventKind::Step, &cmd, *timeout).await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Remove { service } => {
                println!("{p}  ryra rm --purge {service}...");
                let event = run_event(
                    vm,
                    EventKind::Step,
                    &format!("ryra rm --purge {service} -y"),
                    120,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Reset => {
                println!("{p}  ryra reset...");
                let event = run_event(vm, EventKind::Step, "ryra reset -y", 120).await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Wait { service, timeout } => {
                println!("{p}  waiting for {service}...");
                let event = wait_for_service_with_timeout(vm, service, *timeout).await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Shell {
                name: step_name,
                run,
                timeout,
                poll,
            } => {
                println!("{p}  run: {step_name}...");
                let event = run_step_with_poll(
                    vm,
                    step_name,
                    run,
                    *timeout,
                    poll.as_ref(),
                    verbose,
                    stream_prefix,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Http {
                name: http_name,
                url,
                method,
                body,
                content_type,
                headers,
                status,
                service,
                poll,
                timeout,
            } => {
                let step_name = http_name.as_deref().unwrap_or(url);
                println!("{p}  http: {step_name}...");
                // Source service .env files for variable expansion ($RYRA_PORT_HTTP etc.),
                // follow redirects (-L), skip TLS verification (-k) for self-signed certs.
                // URL uses double quotes so shell variables expand.
                let url_esc = url.replace('"', r#"\""#);
                let env_source = match service {
                    Some(svc) => load_env_shell(&format!("$HOME/.local/share/ryra/{svc}/.env")),
                    None => format!(
                        "for __f in $HOME/.local/share/ryra/*/.env; do [ -f \"$__f\" ] && {}; done",
                        load_env_shell("\"$__f\"")
                    ),
                };
                // Assemble curl. For non-GET methods we prepend a heredoc so
                // the body flows verbatim into a $RYRA_BODY variable — this
                // dodges all the shell-quoting edge cases of embedding
                // arbitrary JSON/form bodies directly in the command string.
                let verb = method.as_curl_arg();
                let ct_esc = content_type.replace('"', r#"\""#);
                // Extra headers — rendered as a sequence of `-H "K: V"` args.
                // Values go through double-quotes so $VAR expansion against
                // the sourced .env files works (useful for apikey, tokens).
                let header_args = headers
                    .iter()
                    .map(|(k, v)| {
                        let k = k.replace('"', r#"\""#);
                        let v = v.replace('"', r#"\""#);
                        format!(r#" -H "{k}: {v}""#)
                    })
                    .collect::<String>();
                let curl = match body {
                    Some(b) => format!(
                        "RYRA_BODY=$(cat <<'RYRA_HTTP_BODY_EOF'\n{b}\nRYRA_HTTP_BODY_EOF\n) && \
                         HTTP_CODE=$(curl -skL -o /dev/null -w '%{{http_code}}' \
                            -X {verb} \
                            -H \"Content-Type: {ct_esc}\"{header_args} \
                            --data-raw \"$RYRA_BODY\" \
                            \"{url_esc}\")"
                    ),
                    None => format!(
                        "HTTP_CODE=$(curl -skL -o /dev/null -w '%{{http_code}}' -X {verb}{header_args} \"{url_esc}\")"
                    ),
                };
                let cmd = format!(
                    "set -a && {env_source} && set +a && {curl} && \
                     [ \"$HTTP_CODE\" = \"{status}\" ]"
                );
                let event = run_step_with_poll(
                    vm,
                    step_name,
                    &cmd,
                    *timeout,
                    poll.as_ref(),
                    verbose,
                    stream_prefix,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Playwright {
                name: browser_name,
                spec,
                env,
                timeout,
            } => {
                let step_name = browser_name.as_deref().unwrap_or(spec);
                println!("{p}  browser: {step_name}...");
                let event = run_browser_step(
                    vm,
                    name,      // test name (for report paths)
                    step_name, // step name (for event description)
                    spec,
                    env,
                    *timeout,
                    registry_path,
                )
                .await;
                print_event_result(&p, &event);
                if event.outcome.is_fail() {
                    failed = true;
                }
                events.push(event);
            }
            StepDef::Mail {
                name: mail_name,
                mailbox,
                contains,
                poll,
                timeout,
            } => {
                let step_name = mail_name.as_deref().unwrap_or(mailbox);
                println!("{p}  mail: {step_name}...");
                // Single-shot probe: discover inbucket's port, fetch the
                // mailbox JSON, check non-empty + (optional) substring.
                // run_step_with_poll retries this until the mail lands.
                let mailbox_esc = shell_escape(mailbox);
                let contains_check = match contains {
                    Some(c) => format!(
                        " && echo \"$RYRA_BODY\" | grep -q -- '{}'",
                        shell_escape(c),
                    ),
                    None => String::new(),
                };
                let cmd = format!(
                    "INBUCKET_PORT=$(grep RYRA_PORT_HTTP $HOME/.local/share/ryra/inbucket/.env 2>/dev/null | cut -d= -f2); \
                     [ -n \"$INBUCKET_PORT\" ] || {{ echo 'inbucket not installed — no ~/.local/share/ryra/inbucket/.env'; exit 2; }}; \
                     RYRA_BODY=$(curl -sf \"http://127.0.0.1:$INBUCKET_PORT/api/v1/mailbox/{mailbox_esc}\" 2>/dev/null); \
                     [ -n \"$RYRA_BODY\" ] && [ \"$RYRA_BODY\" != '[]' ]{contains_check}"
                );
                let event = run_step_with_poll(
                    vm,
                    step_name,
                    &cmd,
                    *timeout,
                    Some(poll),
                    verbose,
                    stream_prefix,
                )
                .await;
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

/// Execute a run step, optionally retrying on failure via poll config.
async fn run_step_with_poll(
    vm: &dyn Executor,
    step_name: &str,
    cmd: &str,
    timeout_secs: u64,
    poll: Option<&crate::test_toml::PollConfig>,
    verbose: bool,
    stream_prefix: &str,
) -> Event {
    let t = Instant::now();

    match poll {
        None => {
            // Single execution — same as before
            if verbose {
                run_event_streaming(vm, stream_prefix, EventKind::Step, cmd, timeout_secs).await
            } else {
                run_event(vm, EventKind::Step, cmd, timeout_secs).await
            }
        }
        Some(poll_cfg) => {
            // Retry loop
            let mut last_err = String::new();
            for attempt in 1..=poll_cfg.attempts {
                let timeout = Duration::from_secs(timeout_secs);
                let result = tokio::time::timeout(timeout, vm.exec(cmd)).await;

                match result {
                    Ok(Ok(out)) => {
                        return Event {
                            description: format!("run: {step_name}"),
                            kind: EventKind::Step,
                            outcome: Outcome::Passed,
                            duration: t.elapsed(),
                            stdout: out.stdout,
                            stderr: out.stderr,
                        };
                    }
                    Ok(Err(e)) => {
                        last_err = format!("{e:#}");
                    }
                    Err(_) => {
                        last_err = format!("timed out after {timeout_secs}s");
                    }
                }

                if attempt < poll_cfg.attempts {
                    tokio::time::sleep(Duration::from_secs(poll_cfg.interval)).await;
                }
            }

            Event::bare(
                format!("run: {step_name}"),
                EventKind::Step,
                Outcome::Failed(format!(
                    "failed after {} attempts (interval={}s): {last_err}",
                    poll_cfg.attempts, poll_cfg.interval
                )),
                t.elapsed(),
            )
        }
    }
}

/// Wait for a port to accept TCP connections (not just be bound by rootlessport).
///
/// Uses bash `/dev/tcp` to test actual TCP connectivity through to the
/// container, not just whether rootlessport is listening on the host side.
async fn wait_for_port(vm: &dyn Executor, test_name: &str, port: &str) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(60);
    let mut last_log = std::time::Instant::now();
    // First few seconds: rootlessport is listening but the container app
    // may not be ready yet. A successful bash /dev/tcp probe means the
    // connection made it all the way to the container.
    loop {
        let cmd = format!("bash -c 'echo > /dev/tcp/127.0.0.1/{port}' 2>/dev/null");
        if vm.exec(&cmd).await.is_ok() {
            return Event::bare(
                format!("port {port} ready"),
                EventKind::Step,
                Outcome::Passed,
                t.elapsed(),
            );
        }

        if t.elapsed() > timeout {
            return Event::bare(
                format!("port {port} ready"),
                EventKind::Step,
                Outcome::Failed(format!(
                    "port {port} not responding after {}s",
                    timeout.as_secs()
                )),
                t.elapsed(),
            );
        }

        if last_log.elapsed().as_secs() >= 10 {
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
async fn dump_diagnostics(vm: &dyn Executor, test_name: &str, services: &[&str]) {
    println!("[{test_name}] --- diagnostics ---");
    for svc in services {
        // Systemd service status
        let cmd = format!("systemctl --user status {svc}.service 2>&1 | head -20 || true");
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
        let cmd = "podman ps -a --format '{{.Names}} {{.Status}} {{.Ports}}' 2>&1 || true";
        if let Ok(out) = vm.exec(cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] containers: {trimmed}");
            } else {
                println!("[{test_name}]   [{svc}] containers: (none)");
            }
        }

        // Journal logs
        let cmd = format!("journalctl --user -u {svc}.service --no-pager -n 30 2>&1 || true");
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
        let cmd = format!("cat $HOME/.local/share/ryra/{svc}/.env 2>&1 | grep RYRA_PORT || true");
        if let Ok(out) = vm.exec(&cmd).await {
            let trimmed = out.stdout.trim();
            if !trimmed.is_empty() {
                println!("[{test_name}]   [{svc}] ports: {trimmed}");
            }
        }

        // Check quadlet, container internals, and network
        let cmd = format!(
            "echo '=== quadlet ==='; grep -i exec $HOME/.config/containers/systemd/{svc}.container 2>/dev/null || true; \
             echo '=== container process ==='; podman exec {svc} ps aux 2>&1 | head -10 || true; \
             echo '=== container listeners ==='; podman exec {svc} cat /proc/net/tcp6 2>&1 | head -10 || true; \
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
    vm: &dyn Executor,
    test_name: &str,
    kind: EventKind,
    cmd: &str,
    timeout_secs: u64,
) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec_streaming(cmd, test_name)).await;

    let (outcome, stdout, stderr) = match result {
        Ok(Ok(out)) => (Outcome::Passed, out.stdout, out.stderr),
        Ok(Err(e)) => (Outcome::Failed(format!("{e:#}")), String::new(), String::new()),
        Err(_) => (
            Outcome::Failed(format!("timed out after {timeout_secs}s")),
            String::new(),
            String::new(),
        ),
    };

    Event {
        description: cmd.to_string(),
        kind,
        outcome,
        duration: t.elapsed(),
        stdout,
        stderr,
    }
}

/// Run a command in the VM and return an Event.
async fn run_event(vm: &dyn Executor, kind: EventKind, cmd: &str, timeout_secs: u64) -> Event {
    let t = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let result = tokio::time::timeout(timeout, vm.exec(cmd)).await;

    let (outcome, stdout, stderr) = match result {
        Ok(Ok(out)) => (Outcome::Passed, out.stdout, out.stderr),
        Ok(Err(e)) => (Outcome::Failed(format!("{e:#}")), String::new(), String::new()),
        Err(_) => (
            Outcome::Failed(format!("timed out after {timeout_secs}s")),
            String::new(),
            String::new(),
        ),
    };

    Event {
        description: cmd.to_string(),
        kind,
        outcome,
        duration: t.elapsed(),
        stdout,
        stderr,
    }
}
