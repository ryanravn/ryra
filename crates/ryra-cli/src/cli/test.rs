use std::process::Stdio;
use std::time::Instant;

use anyhow::Result;
use tokio::process::Command;

pub async fn run(service: &str, verbose: bool) -> Result<()> {
    let info = ryra_core::service_tests(service).await?;

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

    println!(
        "Testing {service} ({} tests)\n",
        info.tests.len()
    );

    let mut passed = 0;
    let mut failed = 0;
    let total_start = Instant::now();

    for test in &info.tests {
        let start = Instant::now();

        // Build command: source .env, apply test env overrides, then run
        let mut parts = Vec::new();
        parts.push(format!(". {}", info.env_file.display()));
        for (key, val) in &test.env {
            parts.push(format!("export {key}={val}"));
        }
        parts.push(test.run.clone());
        let full_cmd = parts.join(" && ");

        let timeout = std::time::Duration::from_secs(test.timeout);
        let result = tokio::time::timeout(timeout, async {
            Command::new("sh")
                .args(["-c", &full_cmd])
                .stdout(if verbose { Stdio::inherit() } else { Stdio::piped() })
                .stderr(if verbose { Stdio::inherit() } else { Stdio::piped() })
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
