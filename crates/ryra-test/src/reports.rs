use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::scenario::{Outcome, ScenarioResult};

/// Root directory where test reports for the previous run live.
pub fn reports_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join("services/test-reports"))
}

/// Wipe the reports directory so only results from this run remain.
/// Called at the start of every `ryra test` invocation.
pub fn wipe_reports_dir() -> Result<()> {
    let dir = reports_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to wipe {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(())
}

/// Write the run-level summary.json and per-test events.json files,
/// then print a human-readable summary pointing at the files on disk.
pub fn save_run_results(results: &[ScenarioResult]) -> Result<()> {
    let dir = reports_dir()?;
    std::fs::create_dir_all(&dir)?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results.len() - passed;

    // Run-level summary.json — simple hand-written JSON (no serde_json dep).
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"timestamp\": {timestamp},\n"));
    json.push_str(&format!("  \"passed\": {passed},\n"));
    json.push_str(&format!("  \"failed\": {failed},\n"));
    json.push_str(&format!("  \"total\": {},\n", results.len()));
    json.push_str("  \"tests\": [\n");
    for (i, r) in results.iter().enumerate() {
        let status = match &r.outcome {
            Outcome::Passed => "pass",
            Outcome::Failed(_) => "fail",
            Outcome::Skipped => "skip",
        };
        let comma = if i + 1 < results.len() { "," } else { "" };
        json.push_str(&format!(
            "    {{\"name\": \"{}\", \"status\": \"{status}\", \"duration_ms\": {}}}{comma}\n",
            escape_json(&r.name),
            r.duration.as_millis(),
        ));
    }
    json.push_str("  ]\n");
    json.push_str("}\n");
    std::fs::write(dir.join("summary.json"), json)?;

    // Per-test events.json + run.log (events rendered as text)
    for r in results {
        let tdir = dir.join(&r.name);
        std::fs::create_dir_all(&tdir)?;
        std::fs::write(tdir.join("run.log"), format!("{r}"))?;
    }

    Ok(())
}

/// Print the end-of-run results summary and point the user at file locations.
pub fn print_results_paths(results: &[ScenarioResult]) {
    let dir = match reports_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let display = match dir.to_str() {
        Some(s) => s.to_string(),
        None => dir.display().to_string(),
    };
    // Replace $HOME prefix with ~ for brevity
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && display.starts_with(&home) {
        format!("~{}", &display[home.len()..])
    } else {
        display
    };

    println!("\nResults: {display}/");
    println!("  summary: cat {display}/summary.json");
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
            // The trace viewer requires http:// — file:// can't load the trace
            // zips — so surface the `show-report` command, not the path.
            println!(
                "    browser: cd registry/tests/browser && bunx playwright show-report {display}/{}/playwright",
                r.name
            );
        }
    }
}

/// Minimal JSON string escaping — enough for test names.
fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
