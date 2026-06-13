use std::process::Stdio;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Parser;
use tokio::process::Command;

use ryra_core::registry::test_def::TestDef;

/// Parameters for [`run`].
pub struct TestRunParams<'a> {
    pub service: Option<&'a str>,
    pub test_filter: Option<&'a str>,
    pub project: Option<&'a std::path::PathBuf>,
    pub vm: bool,
    pub live: bool,
    pub retest: bool,
    pub keep_alive: bool,
    pub yes: bool,
    pub verbose: bool,
    pub list: bool,
    pub parallel: Option<usize>,
    pub names: &'a [String],
}

pub async fn run(params: TestRunParams<'_>) -> Result<()> {
    // Host runs (default bare mode and `--live`) mutate THIS machine and run
    // arbitrary registry commands — require consent. `--vm`/`--keep-alive`
    // run in a throwaway VM, and `--list` only prints, so neither needs it.
    let host_run = !(params.vm || params.keep_alive || params.list);
    if host_run {
        confirm_host_run(params.yes)?;
    }

    // VM is opt-in (`--vm`). `--keep-alive` is inherently a VM operation
    // (boot a VM and hold it for interactive debugging), so it implies one.
    if params.vm || params.keep_alive {
        return run_vm(
            params.names,
            params.keep_alive,
            params.verbose,
            params.list,
            params.parallel,
            params.project,
        )
        .await;
    }

    // `--live`: run only the assertion commands against a service that's
    // already installed on this host — no add/remove.
    if params.live {
        return match params.service {
            Some(service) => {
                run_live_service(service, params.test_filter, params.yes, params.verbose).await
            }
            None => anyhow::bail!("--live requires --service <name>"),
        };
    }

    // Default: run the full add/assert/remove lifecycle on this host.
    run_no_vm(
        params.names,
        params.verbose,
        params.list,
        params.retest,
        params.project,
    )
    .await
}

/// Require explicit consent before running tests on the real host. Unlike
/// `--vm` (a throwaway VM), host tests mutate THIS machine: they install,
/// purge, and reinstall the services each test declares, and run arbitrary
/// shell/HTTP commands from the registry. Unrelated services and their data
/// are left untouched, but the user should still opt in. Mirrors
/// `warn_untrusted_repo`: `-y` skips, interactive prompts, non-interactive
/// without `-y` refuses.
fn confirm_host_run(yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }

    if !super::is_interactive() {
        anyhow::bail!(
            "refusing to run tests on this host without confirmation.\n\
             Host tests install/purge services and run arbitrary commands from the registry.\n\
             Re-run with -y to confirm, or use --vm to run in a throwaway VM."
        );
    }

    eprintln!(
        "About to run tests on THIS host (not a VM).\n\n\
         These tests run arbitrary shell and HTTP commands from the registry\n\
         against your real machine, and they install, start, stop, purge, and\n\
         reinstall the services each test declares. Services you already\n\
         installed are detected and left untouched (those tests are skipped),\n\
         but anything a test installs is removed again when it finishes.\n"
    );

    // Running the registry's arbitrary commands as root hands the whole machine
    // to whatever code the test executes. Warn hard — louder still under sudo,
    // which we can detect from the env sudo sets.
    if std::env::var_os("SUDO_USER").is_some() || std::env::var_os("SUDO_UID").is_some() {
        eprintln!(
            "  !! You appear to be running under sudo. Do NOT do this unless you\n     \
             have audited the exact registry code you're about to run and trust\n     \
             it completely — as root these commands own your entire machine.\n"
        );
    } else {
        eprintln!(
            "  Do not run this under sudo / as root unless you have audited the\n  \
             registry you're using: the test commands run with your privileges.\n"
        );
    }

    let confirm = dialoguer::Confirm::new()
        .with_prompt("Continue?")
        .default(false)
        .interact()?;

    if !confirm {
        anyhow::bail!("aborted");
    }

    Ok(())
}

/// Warn if tests are being loaded from a custom registry.
fn warn_untrusted_repo(registry_name: &str, yes: bool) -> Result<()> {
    // The default registry is always trusted
    if registry_name == ryra_core::REGISTRY_DEFAULT || registry_name.is_empty() {
        return Ok(());
    }

    if yes {
        eprintln!("warning: running test commands from custom registry: {registry_name}");
        return Ok(());
    }

    if !super::is_interactive() {
        anyhow::bail!(
            "refusing to run test commands from custom registry in non-interactive mode.\n\
             Registry: {registry_name}\n\
             Pass -y to skip this check."
        );
    }

    eprintln!("warning: about to run test commands from custom registry:\n  {registry_name}\n");
    let confirm = dialoguer::Confirm::new()
        .with_prompt("Continue?")
        .default(false)
        .interact()?;

    if !confirm {
        anyhow::bail!("aborted");
    }

    Ok(())
}

async fn run_live_service(
    service: &str,
    test_filter: Option<&str>,
    yes: bool,
    verbose: bool,
) -> Result<()> {
    let info = ryra_core::service_tests(service).await?;

    warn_untrusted_repo(&info.registry_name, yes)?;

    if info.tests.is_empty() {
        println!("No tests defined for {service}.");
        println!("Add [[tests]] sections to the service.toml in the registry.");
        return Ok(());
    }

    if !info.env_file.exists() {
        anyhow::bail!(
            "env file not found at {} — is {service} installed and running?",
            info.env_file.display()
        );
    }

    let tests: Vec<&TestDef> = match test_filter {
        Some(filter) => {
            let filtered: Vec<_> = info.tests.iter().filter(|t| t.name == filter).collect();
            if filtered.is_empty() {
                let available: Vec<&str> = info.tests.iter().map(|t| t.name.as_str()).collect();
                anyhow::bail!(
                    "no test named '{filter}' for {service}. Available: {}",
                    available.join(", ")
                );
            }
            filtered
        }
        None => info.tests.iter().collect(),
    };

    println!("Testing {service} ({} tests)\n", tests.len());

    let env_sources = vec![format!(". {}", info.env_file.display())];
    run_tests(&tests, &env_sources, verbose).await
}

async fn run_tests(tests: &[&TestDef], env_sources: &[String], verbose: bool) -> Result<()> {
    let mut passed = 0;
    let mut failed = 0;
    let total_start = Instant::now();

    for test in tests {
        let start = Instant::now();

        let mut parts = Vec::new();
        parts.extend(env_sources.iter().cloned());
        for (key, val) in &test.env {
            parts.push(format!("export {key}={val}"));
        }
        parts.push(test.run.clone());
        let full_cmd = parts.join(" && ");

        let timeout = std::time::Duration::from_secs(test.timeout);
        let result = tokio::time::timeout(timeout, async {
            Command::new("sh")
                .args(["-c", &full_cmd])
                .stdout(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::piped()
                })
                .stderr(if verbose {
                    Stdio::inherit()
                } else {
                    Stdio::piped()
                })
                .status()
                .await
        })
        .await;

        let elapsed = start.elapsed();
        let elapsed_str = format!("{:.1}s", elapsed.as_secs_f64());

        match result {
            Ok(Ok(status)) if status.success() => {
                passed += 1;
                println!("  PASS  {} ({elapsed_str})", test.name);
            }
            Ok(Ok(status)) => {
                failed += 1;
                let code = status.code().map(|c| c.to_string()).unwrap_or("?".into());
                println!("  FAIL  {} ({elapsed_str}) — exit code {code}", test.name);
                if !verbose {
                    println!("         re-run with -v to see output");
                }
            }
            Ok(Err(e)) => {
                failed += 1;
                println!("  FAIL  {} ({elapsed_str}) — {e}", test.name);
            }
            Err(_) => {
                failed += 1;
                println!(
                    "  FAIL  {} ({elapsed_str}) — timed out after {}s",
                    test.name, test.timeout
                );
            }
        }
    }

    let total = total_start.elapsed();
    println!(
        "\n{passed} passed, {failed} failed ({:.1}s)",
        total.as_secs_f64()
    );

    if failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}

async fn run_vm(
    names: &[String],
    keep_alive: bool,
    verbose: bool,
    list: bool,
    parallel: Option<usize>,
    project: Option<&std::path::PathBuf>,
) -> Result<()> {
    let mut args = ryra_test::Args::parse_from(std::iter::once("ryra-test"));
    args.verbose = verbose;
    args.keep_alive = keep_alive;
    args.list = list;
    if let Some(n) = parallel {
        args.parallel = n;
    }
    args.project = project.cloned();
    args.tests = names.to_vec();
    ryra_test::run(args).await
}

async fn run_no_vm(
    names: &[String],
    verbose: bool,
    list: bool,
    retest: bool,
    project: Option<&std::path::PathBuf>,
) -> Result<()> {
    let mut args = ryra_test::Args::parse_from(std::iter::once("ryra-test"));
    args.no_vm = true;
    args.retest = retest;
    args.verbose = verbose;
    args.list = list;
    args.project = project.cloned();
    args.tests = names.to_vec();
    ryra_test::run(args).await
}

/// Display a path with `$HOME` abbreviated to `~`.
fn tilde(path: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.to_str() {
        Some(s) if !home.is_empty() && s.starts_with(&home) => {
            format!("~{}", &s[home.len()..])
        }
        Some(s) => s.to_string(),
        None => path.display().to_string(),
    }
}

/// Tear down the host-test sandbox: purge leftover test services recorded in
/// the ledger, then delete the sandbox dir (service data, preferences,
/// ledger, run results). The test analogue of `ryra reset`; services the
/// user installed for real are never touched.
pub async fn reset_sandbox(yes: bool) -> Result<()> {
    let Some(root) = ryra_test::test_sandbox_root() else {
        anyhow::bail!("cannot resolve test sandbox root ($HOME unset)");
    };

    let leftovers = ryra_test::host_leftovers();
    if !root.exists() && leftovers.is_empty() {
        println!("Nothing to reset: no test sandbox found.");
        return Ok(());
    }

    if !yes {
        if super::is_interactive() {
            println!("This will:");
            if !leftovers.is_empty() {
                println!(
                    "  - Purge {} leftover test service(s): {}",
                    leftovers.len(),
                    leftovers.join(", ")
                );
            }
            println!(
                "  - Delete {}/  (test service data, preferences, ledger, run results)",
                tilde(&root)
            );
            println!();

            let confirm = dialoguer::Confirm::new()
                .with_prompt("Continue?")
                .default(false)
                .interact()?;
            if !confirm {
                println!("Cancelled.");
                return Ok(());
            }
        } else {
            anyhow::bail!("use --yes (-y) to confirm test reset in non-interactive mode");
        }
    }

    if !leftovers.is_empty() {
        let executor = ryra_test::executor::LocalExecutor::new()
            .with_config_dir(&root.join("config"))
            .with_data_dir(&root.join("services"));
        ryra_test::purge_services(&executor, &leftovers, "during reset").await;

        // purge_services is best-effort; verify before deleting the sandbox.
        // The ledger lives inside it, and wiping the ledger while a service
        // is still installed would make the next run treat that service as
        // user-owned and refuse to touch it.
        let remaining = ryra_test::host_leftovers();
        if !remaining.is_empty() {
            anyhow::bail!(
                "could not purge: {}. Remove manually with `ryra remove --purge <name> -y`, \
                 then re-run `ryra test reset`.",
                remaining.join(", ")
            );
        }
    }

    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|e| anyhow::anyhow!("failed to delete {}: {e}", root.display()))?;
    }

    println!("Test sandbox reset.");
    Ok(())
}

/// Remove stored results for the named tests.
pub fn remove_tests(names: &[String]) {
    if names.is_empty() {
        println!("Usage: ryra test remove <name> [<name> ...]");
        return;
    }
    let existing: Vec<String> = ryra_test::reports::scan_results()
        .into_iter()
        .map(|r| r.name)
        .collect();
    for name in names {
        if !existing.contains(name) {
            println!("  skip  {name} (no stored results)");
            continue;
        }
        match ryra_test::reports::delete_test_result(name) {
            Ok(()) => println!("  removed  {name}"),
            Err(e) => eprintln!("  error    {name}: {e}"),
        }
    }
}

/// Print local test sandbox state: per-test results discovered from
/// `reports/*/result.json`, plus any leftover ledger entries.
pub fn show_sandbox_state() {
    let Some(root) = ryra_test::test_sandbox_root() else {
        println!("No test sandbox found. Run `ryra test <name>` to create one.");
        return;
    };

    if !root.exists() {
        println!("No test sandbox found. Run `ryra test <name>` to create one.");
        return;
    }

    let display = tilde(&root);
    println!("Test sandbox ({display}/)\n");

    // Ledger (leftover services from aborted runs)
    let ledger = ryra_test::ledger_load();
    if !ledger.is_empty() {
        println!("Leftovers (from aborted runs):");
        for name in &ledger {
            println!("  {name}");
        }
        println!();
    }

    // Test results: scan reports/*/result.json
    let results = ryra_test::reports::scan_results();
    if results.is_empty() {
        println!("\nResults:\n  (no results)");
        return;
    }

    let passed = results.iter().filter(|r| r.status == "pass").count();
    let failed = results.iter().filter(|r| r.status == "fail").count();

    let latest_ts = results.iter().map(|r| r.timestamp).max().unwrap_or(0);
    let date_str = if latest_ts > 0 {
        let dt = DateTime::<Utc>::from_timestamp(latest_ts as i64, 0);
        match dt {
            Some(dt) => dt.format("%-d %b %Y").to_string(),
            None => "unknown date".to_string(),
        }
    } else {
        "unknown date".to_string()
    };

    println!("\nResults ({date_str}):");
    println!("  {passed} passed, {failed} failed");

    let max_name = results.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in &results {
        let label = match r.status.as_str() {
            "pass" => "PASS",
            "fail" => "FAIL",
            "skip" => "SKIP",
            other => other,
        };
        let duration_str = format!("{:.1}s", r.duration_ms as f64 / 1000.0);
        println!("    {label}  {:<max_name$}  {duration_str}", r.name);
    }
}
