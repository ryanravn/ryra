use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::scenario::{Outcome, ScenarioResult};

/// Root directory where test reports live: under the host-test sandbox
/// (`~/.local/share/services-test/reports/`), alongside the service data
/// and ledger, so the whole test footprint is one folder.
pub fn reports_dir() -> Result<PathBuf> {
    crate::test_sandbox_root()
        .map(|root| root.join("reports"))
        .context("cannot resolve test sandbox root ($HOME unset)")
}

/// Per-test result stored as `reports/<name>/result.json`.
#[derive(Clone, Debug)]
pub struct TestResult {
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
    pub timestamp: u64,
    pub has_playwright: bool,
}

/// Save one test's result to `reports/<name>/result.json` and its log to
/// `reports/<name>/run.log`. Previous results for other tests are untouched.
pub fn save_test_result(result: &ScenarioResult) -> Result<()> {
    let dir = reports_dir()?;
    let tdir = dir.join(&result.name);
    std::fs::create_dir_all(&tdir)?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let status = match &result.outcome {
        Outcome::Passed => "pass",
        Outcome::Failed(_) => "fail",
        Outcome::Skipped => "skip",
    };

    let json = format!(
        "{{\n  \"name\": \"{}\",\n  \"status\": \"{status}\",\n  \
         \"duration_ms\": {},\n  \"timestamp\": {timestamp}\n}}\n",
        escape_json(&result.name),
        result.duration.as_millis(),
    );
    std::fs::write(tdir.join("result.json"), json)?;
    std::fs::write(tdir.join("run.log"), format!("{result}"))?;

    Ok(())
}

/// Remove a single test's results (report dir and per-test sandbox).
pub fn delete_test_result(name: &str) -> Result<()> {
    let report = reports_dir()?.join(name);
    if report.is_dir() {
        std::fs::remove_dir_all(&report)
            .with_context(|| format!("failed to remove report dir: {}", report.display()))?;
    }

    if let Some(sandbox) = crate::test_sandbox_root() {
        let test_dir = sandbox.join("tests").join(name);
        if test_dir.is_dir() {
            std::fs::remove_dir_all(&test_dir)
                .with_context(|| format!("failed to remove sandbox dir: {}", test_dir.display()))?;
        }
    }

    Ok(())
}

/// Save results for a batch of tests.
pub fn save_run_results(results: &[ScenarioResult]) -> Result<()> {
    for r in results {
        save_test_result(r)?;
    }
    Ok(())
}

/// Scan `reports/*/result.json` to discover all stored test results,
/// the same way services are discovered by scanning directories.
pub fn scan_results() -> Vec<TestResult> {
    let dir = match reports_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut results: Vec<TestResult> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let tdir = e.path();
            let json = std::fs::read_to_string(tdir.join("result.json")).ok()?;
            parse_result_json(&json, &tdir)
        })
        .collect();

    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

fn parse_result_json(json: &str, tdir: &std::path::Path) -> Option<TestResult> {
    // Minimal JSON parsing without pulling in serde for this crate.
    let name = extract_json_str(json, "name")?;
    let status = extract_json_str(json, "status")?;
    let duration_ms = extract_json_u64(json, "duration_ms")?;
    let timestamp = extract_json_u64(json, "timestamp").unwrap_or(0);
    let has_playwright = tdir.join("playwright").join("index.html").exists();
    Some(TestResult {
        name,
        status,
        duration_ms,
        timestamp,
        has_playwright,
    })
}

fn extract_json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let pos = json.find(&needle)? + needle.len();
    let rest = &json[pos..];
    let start = rest.find('"')? + 1;
    let end = start + rest[start..].find('"')?;
    Some(rest[start..end].to_string())
}

fn extract_json_u64(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let pos = json.find(&needle)? + needle.len();
    let rest = &json[pos..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let end = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Format a duration as a compact human string, e.g. `1091s` -> `18m 11s`.
pub fn humanize_secs(total: u64) -> String {
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Print the end-of-run results summary and point the user at file locations.
pub fn print_results_paths(results: &[ScenarioResult], wall_clock: std::time::Duration) {
    let dir = match reports_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let display = tilde_path(&dir);

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
        .count();
    let total = results.len();

    let elapsed = humanize_secs(wall_clock.as_secs());
    println!("\nResults: {passed}/{total} passed ({failed} failed) in {elapsed}");
    println!("  dir: {display}/");

    if failed > 0 {
        println!("\n  Failed ({failed}):");
        for r in results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
        {
            println!("    x {} ({:.1}s)", r.name, r.duration.as_secs_f64());
            if let Some(why) = r.failure_summary() {
                println!("        {why}");
            }
        }
    }

    for r in results {
        let status = match &r.outcome {
            Outcome::Passed => "PASS",
            Outcome::Failed(_) => "FAIL",
            Outcome::Skipped => "SKIP",
        };
        println!(
            "\n  {}: {status} ({:.1}s)",
            r.name,
            r.duration.as_secs_f64()
        );
        println!("    log:     cat {display}/{}/run.log", r.name);
        let playwright_index = dir.join(&r.name).join("playwright").join("index.html");
        if playwright_index.exists() {
            println!(
                "    browser: cd registry/tests/browser && bunx playwright show-report {display}/{}/playwright",
                r.name
            );
        }
    }
}

fn tilde_path(path: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.to_str() {
        Some(s) if !home.is_empty() && s.starts_with(&home) => {
            format!("~{}", &s[home.len()..])
        }
        Some(s) => s.to_string(),
        None => path.display().to_string(),
    }
}

/// Minimal JSON string escaping.
fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
