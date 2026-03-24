use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Instant;

use anyhow::Result;
#[cfg(feature = "vm")]
use clap::Parser;
use tokio::process::Command;

use ryra_core::registry::service_def::TestDef;

/// Parameters for [`run`].
pub struct TestRunParams<'a> {
    pub service: Option<&'a str>,
    pub suite: Option<&'a str>,
    pub test_filter: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub vm: bool,
    pub keep_alive: bool,
    pub yes: bool,
    pub verbose: bool,
    pub list: bool,
    pub parallel: Option<usize>,
    pub names: &'a [String],
}

pub async fn run(params: TestRunParams<'_>) -> Result<()> {
    if params.vm || params.keep_alive || params.list {
        return run_vm(
            params.names,
            params.keep_alive,
            params.verbose,
            params.list,
            params.parallel,
        )
        .await;
    }

    match (params.service, params.suite) {
        (Some(service), None) => {
            run_live_service(
                service,
                params.test_filter,
                params.repo,
                params.yes,
                params.verbose,
            )
            .await
        }
        (None, Some(suite)) => {
            run_live_suite(
                suite,
                params.test_filter,
                params.repo,
                params.yes,
                params.verbose,
            )
            .await
        }
        (Some(_), Some(_)) => anyhow::bail!("cannot specify both a service and --suite"),
        (None, None) => anyhow::bail!("specify a service name or --suite <name>"),
    }
}

/// Warn if tests are being loaded from a non-default repo.
fn warn_untrusted_repo(repo_url: &str, yes: bool) -> Result<()> {
    let default = ryra_core::DEFAULT_REPO;

    // Check configured default too
    let configured_default = ryra_core::config::ConfigPaths::resolve()
        .ok()
        .and_then(|p| ryra_core::config::load_or_default(&p.config_file).ok())
        .and_then(|c| c.default_repo);

    let is_trusted = repo_url == default || configured_default.as_deref() == Some(repo_url);

    if is_trusted {
        return Ok(());
    }

    if yes {
        eprintln!("warning: running test commands from non-default repo: {repo_url}");
        return Ok(());
    }

    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "refusing to run test commands from non-default repo in non-interactive mode.\n\
             Repo: {repo_url}\n\
             Pass -y to skip this check."
        );
    }

    eprintln!("warning: about to run test commands from non-default repo:\n  {repo_url}\n");
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
    repo: Option<&str>,
    yes: bool,
    verbose: bool,
) -> Result<()> {
    let info = ryra_core::service_tests(service, repo).await?;

    warn_untrusted_repo(&info.repo_url, yes)?;

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

async fn run_live_suite(
    suite: &str,
    test_filter: Option<&str>,
    repo: Option<&str>,
    yes: bool,
    verbose: bool,
) -> Result<()> {
    let info = ryra_core::suite_tests(suite, repo).await?;

    warn_untrusted_repo(&info.repo_url, yes)?;

    if info.tests.is_empty() {
        println!("No tests defined in suite {suite}.");
        return Ok(());
    }

    // Check all services are installed
    for svc in &info.services {
        let env_file = ryra_core::service_home(svc).join(".env");
        if !env_file.exists() {
            anyhow::bail!(
                "env file not found for {svc} — is it installed and running?\n\
                 Suite {suite} requires: {}",
                info.services.join(", ")
            );
        }
    }

    let tests: Vec<&TestDef> = match test_filter {
        Some(filter) => {
            let filtered: Vec<_> = info.tests.iter().filter(|t| t.name == filter).collect();
            if filtered.is_empty() {
                let available: Vec<&str> = info.tests.iter().map(|t| t.name.as_str()).collect();
                anyhow::bail!(
                    "no test named '{filter}' in suite {suite}. Available: {}",
                    available.join(", ")
                );
            }
            filtered
        }
        None => info.tests.iter().collect(),
    };

    println!(
        "Testing suite {suite} [{}] ({} tests)\n",
        info.services.join(" + "),
        tests.len()
    );

    // Build env sourcing: prefix each service's vars with SERVICE__
    let mut env_sources = Vec::new();
    for svc in &info.services {
        let env_file = ryra_core::service_home(svc).join(".env");
        let prefix = svc.to_uppercase();
        env_sources.push(format!(
            "while IFS='=' read -r key val; do \
             [ -n \"$key\" ] && export {prefix}__$key=\"$val\"; \
             done < {}",
            env_file.display()
        ));
    }

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

#[cfg(feature = "vm")]
async fn run_vm(
    names: &[String],
    keep_alive: bool,
    verbose: bool,
    list: bool,
    parallel: Option<usize>,
) -> Result<()> {
    let mut args = ryra_test::Args::parse_from(std::iter::once("ryra-test"));
    args.verbose = verbose;
    args.keep_alive = keep_alive;
    args.list = list;
    if let Some(n) = parallel {
        args.parallel = n;
    }
    args.tests = names.to_vec();
    ryra_test::run(args).await
}

#[cfg(not(feature = "vm"))]
async fn run_vm(
    _names: &[String],
    _keep_alive: bool,
    _verbose: bool,
    _list: bool,
    _parallel: Option<usize>,
) -> Result<()> {
    anyhow::bail!(
        "VM support not compiled in.\n\
         Rebuild with: cargo build --features vm"
    )
}
