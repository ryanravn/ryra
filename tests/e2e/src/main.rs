mod assert;
mod image;
mod machine;
mod ports;
mod registry;
mod runner;
mod scenario;
mod tests;

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Semaphore;

use image::Distro;
use machine::{Machine, SpawnOpts};
use scenario::ScenarioResult;

#[derive(Parser, Debug)]
#[command(
    name = "ryra-e2e",
    about = "E2E test runner for ryra — spins up QEMU VMs"
)]
pub struct Args {
    /// Max concurrent VMs
    #[arg(long, default_value_t = 1)]
    pub parallel: usize,

    /// Base image distro
    #[arg(long, default_value_t = Distro::Debian13)]
    pub distro: Distro,

    /// Re-download the base cloud image
    #[arg(long)]
    pub redownload: bool,

    /// Path to ryra binary
    #[arg(long)]
    pub ryra_bin: Option<PathBuf>,

    /// Don't destroy VMs for failed tests (for debugging via SSH)
    #[arg(long)]
    pub keep_failed: bool,

    /// Disable KVM acceleration (use software emulation — slower)
    #[arg(long)]
    pub no_kvm: bool,

    /// VM memory in MB
    #[arg(long, default_value_t = 2048)]
    pub memory: u32,

    /// VM CPU count
    #[arg(long, default_value_t = 2)]
    pub cpus: u32,

    /// Show serial log output on failure
    #[arg(long, short)]
    pub verbose: bool,

    /// Run registry-defined tests instead of built-in scenarios.
    /// Pass a path to a registry directory, or omit to use the bundled fixtures.
    #[arg(long)]
    pub registry: Option<Option<PathBuf>>,

    /// List available scenarios/tests
    #[arg(long)]
    pub list: bool,

    /// Scenario/test names to run (runs all if empty, supports substring match)
    pub tests: Vec<String>,
}

fn find_ryra_binary() -> Result<PathBuf> {
    for profile in &["release", "debug"] {
        let path = PathBuf::from(format!("../../target/{profile}/ryra-cli"));
        if path.exists() {
            return Ok(std::fs::canonicalize(&path)?);
        }
    }
    anyhow::bail!(
        "ryra binary not found in target/release or target/debug. \
         Build with: cargo build --release -p ryra-cli"
    )
}

fn print_summary(results: &[ScenarioResult]) {
    println!("\n========================================");
    println!("  Results");
    println!("========================================\n");

    for result in results {
        print!("{result}");
    }

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results.len() - passed;
    let total_duration: std::time::Duration = results.iter().map(|r| r.duration).sum();

    println!("----------------------------------------");
    println!(
        "{passed} passed, {failed} failed, {} total ({:.1}s)",
        results.len(),
        total_duration.as_secs_f64()
    );
    println!("========================================");
}

fn check_prerequisites(use_kvm: bool) -> Result<()> {
    let required = [
        "qemu-system-aarch64",
        "qemu-img",
        "ssh",
        "scp",
        "ssh-keygen",
        "curl",
    ];
    let mut missing = Vec::new();

    for cmd in &required {
        let found = std::process::Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            missing.push(*cmd);
        }
    }

    // Need at least one ISO creation tool
    let has_iso_tool = ["genisoimage", "mkisofs"].iter().any(|cmd| {
        std::process::Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if !has_iso_tool {
        missing.push("genisoimage");
    }

    if !missing.is_empty() {
        anyhow::bail!(
            "missing required tools: {}\n\
             Install with:\n  \
             sudo apt install qemu-system-arm qemu-utils qemu-efi-aarch64 \\\n    \
             genisoimage openssh-client curl",
            missing.join(", ")
        );
    }

    if use_kvm && !std::path::Path::new("/dev/kvm").exists() {
        anyhow::bail!(
            "/dev/kvm not found — KVM is not available on this machine.\n\
             Run with --no-kvm to use software emulation (slower), or \
             run on a machine with KVM support."
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Determine if we're running registry tests or built-in scenarios
    if args.registry.is_some() {
        run_registry_mode(&args).await
    } else {
        run_scenario_mode(&args).await
    }
}

/// Find the registry path — explicit arg, or auto-detect from fixtures.
fn resolve_registry_path(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return std::fs::canonicalize(p)
            .with_context(|| format!("registry path not found: {}", p.display()));
    }

    let candidates = [
        PathBuf::from("tests/e2e/fixtures/registry"),
        PathBuf::from("fixtures/registry"),
    ];
    for c in &candidates {
        if c.exists() {
            return std::fs::canonicalize(c)
                .with_context(|| format!("failed to resolve {}", c.display()));
        }
    }

    anyhow::bail!("no registry found. Pass --registry <path> or run from the repo root")
}

/// Run registry-defined tests: discover [[tests]] from service.toml files and tests/*.toml.
async fn run_registry_mode(args: &Args) -> Result<()> {
    let registry_arg = match &args.registry {
        Some(inner) => inner.as_ref(),
        None => None,
    };
    let registry_path = resolve_registry_path(registry_arg)?;
    let discovered = registry::discover(&registry_path)?;

    if args.list {
        println!("Registry tests from {}:", registry_path.display());
        for test in &discovered {
            let svc_list = test.services().join(" + ");
            println!(
                "  {:<30} ({} tests, services: {})",
                test.name(),
                test.test_count(),
                svc_list
            );
        }
        return Ok(());
    }

    if discovered.is_empty() {
        anyhow::bail!("no tests found in registry at {}", registry_path.display());
    }

    let use_kvm = !args.no_kvm;
    check_prerequisites(use_kvm)?;

    let spawn_opts = std::sync::Arc::new(SpawnOpts {
        use_kvm,
        memory_mb: args.memory,
        cpus: args.cpus,
    });

    let ryra_bin = match &args.ryra_bin {
        Some(p) => std::fs::canonicalize(p)?,
        None => find_ryra_binary()?,
    };

    let base_image = image::ensure_image(&args.distro, args.redownload, use_kvm).await?;
    let base_image = std::sync::Arc::new(base_image);
    let registry_path = std::sync::Arc::new(registry_path);

    // Filter tests
    let to_run: Vec<_> = if args.tests.is_empty() {
        discovered.iter().collect()
    } else {
        discovered
            .iter()
            .filter(|t| args.tests.iter().any(|f| t.name().contains(f.as_str())))
            .collect()
    };

    if to_run.is_empty() {
        anyhow::bail!("no tests matched the given filters");
    }

    println!(
        "Running {} registry tests (parallel={})\n",
        to_run.len(),
        args.parallel
    );

    let semaphore = std::sync::Arc::new(Semaphore::new(args.parallel));
    let mut handles = vec![];

    for test in to_run {
        let permit = semaphore.clone().acquire_owned().await?;
        let base_image = base_image.clone();
        let spawn_opts = spawn_opts.clone();
        let ryra_bin = ryra_bin.clone();
        let registry_path = registry_path.clone();
        let keep_failed = args.keep_failed;
        let verbose = args.verbose;
        let name = test.name().to_string();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let id = machine::random_id();
            let ssh_port = ports::allocate_ssh_port();
            let start = std::time::Instant::now();
            println!("[{name}] spawning VM ryra-test-{id} (ssh port {ssh_port})");

            let fail_result = |msg: String| ScenarioResult {
                name: name.clone(),
                events: vec![],
                duration: start.elapsed(),
                outcome: scenario::Outcome::Failed(msg),
            };

            // Re-discover tests inside task (DiscoveredTest isn't Send due to lifetime)
            let discovered = match registry::discover(&registry_path) {
                Ok(d) => d,
                Err(e) => return fail_result(format!("registry discovery failed: {e:#}")),
            };
            let test = match discovered.iter().find(|t| t.name() == name) {
                Some(t) => t,
                None => return fail_result("test not found (internal error)".into()),
            };

            // Spawn VM
            let vm = match Machine::spawn(&base_image, &id, ssh_port, &spawn_opts).await {
                Ok(vm) => vm,
                Err(e) => return fail_result(format!("failed to spawn VM: {e:#}")),
            };

            // Copy ryra binary into VM
            if let Err(e) = machine::copy_ryra_to_vm(&vm, &ryra_bin).await {
                let _ = vm.destroy().await;
                return fail_result(format!("failed to copy ryra to VM: {e:#}"));
            }

            // Copy registry into VM
            if let Err(e) = machine::copy_fixtures_to_vm(&vm, &registry_path).await {
                let _ = vm.destroy().await;
                return fail_result(format!("failed to copy registry to VM: {e:#}"));
            }

            let result = runner::run_registry_test(&vm, test, "/opt/ryra-test-registry").await;

            // On failure with --verbose, dump serial log
            if !result.passed() && verbose {
                let serial_log = vm.work_dir.join("serial.log");
                if let Ok(content) = tokio::fs::read_to_string(&serial_log).await {
                    let lines: Vec<&str> = content.lines().collect();
                    let start_idx = lines.len().saturating_sub(50);
                    eprintln!("[{name}] --- serial log (last 50 lines) ---");
                    for line in &lines[start_idx..] {
                        eprintln!("  {line}");
                    }
                    eprintln!("[{name}] --- end serial log ---");
                }
            }

            if !keep_failed || result.passed() {
                if let Err(e) = vm.destroy().await {
                    eprintln!("[{name}] warning: failed to destroy VM: {e}");
                }
            } else {
                println!("[{name}] FAILED — keeping VM alive for debugging:");
                vm.keep_alive();
            }

            println!(
                "[{name}] {}",
                if result.passed() { "passed" } else { "FAILED" }
            );
            result
        }));
    }

    let mut results = vec![];
    for handle in handles {
        results.push(handle.await?);
    }

    print_summary(&results);

    if results.iter().any(|r| !r.passed()) {
        std::process::exit(1);
    }

    Ok(())
}

/// Run built-in scenarios (the original mode).
async fn run_scenario_mode(args: &Args) -> Result<()> {
    let scenarios = tests::all();

    if args.list {
        println!("Available scenarios:");
        for s in &scenarios {
            println!(
                "  {:<30} ({} steps, {} assertions)",
                s.summary(),
                s.step_count(),
                s.assertion_count()
            );
        }
        return Ok(());
    }

    let use_kvm = !args.no_kvm;
    check_prerequisites(use_kvm)?;

    let spawn_opts = std::sync::Arc::new(SpawnOpts {
        use_kvm,
        memory_mb: args.memory,
        cpus: args.cpus,
    });

    let ryra_bin = match &args.ryra_bin {
        Some(p) => std::fs::canonicalize(p)?,
        None => find_ryra_binary()?,
    };

    let base_image = image::ensure_image(&args.distro, args.redownload, use_kvm).await?;
    let base_image = std::sync::Arc::new(base_image);

    // Find fixtures directory
    let fixtures_candidates = [
        PathBuf::from("tests/e2e/fixtures/registry"),
        PathBuf::from("fixtures/registry"),
    ];
    let fixtures_dir = fixtures_candidates
        .iter()
        .find(|p| p.exists())
        .map(|p| std::fs::canonicalize(p))
        .transpose()?;

    if fixtures_dir.is_none() {
        eprintln!("warning: fixtures directory not found, tests may fail");
    }

    let to_run: Vec<_> = if args.tests.is_empty() {
        scenarios.iter().collect()
    } else {
        scenarios
            .iter()
            .filter(|s| args.tests.iter().any(|f| s.name.contains(f.as_str())))
            .collect()
    };

    if to_run.is_empty() {
        anyhow::bail!("no scenarios matched the given filters");
    }

    println!(
        "Running {} scenarios (parallel={})\n",
        to_run.len(),
        args.parallel
    );

    let semaphore = std::sync::Arc::new(Semaphore::new(args.parallel));
    let mut handles = vec![];

    for scenario in to_run {
        let permit = semaphore.clone().acquire_owned().await?;
        let base_image = base_image.clone();
        let spawn_opts = spawn_opts.clone();
        let ryra_bin = ryra_bin.clone();
        let fixtures_dir = fixtures_dir.clone();
        let keep_failed = args.keep_failed;
        let verbose = args.verbose;
        let name = scenario.name.to_string();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let id = machine::random_id();
            let ssh_port = ports::allocate_ssh_port();
            let start = std::time::Instant::now();
            println!("[{name}] spawning VM ryra-test-{id} (ssh port {ssh_port})");

            let fail_result = |msg: String| ScenarioResult {
                name: name.clone(),
                events: vec![],
                duration: start.elapsed(),
                outcome: scenario::Outcome::Failed(msg),
            };

            let all = tests::all();
            let scenario = match all.iter().find(|s| s.name == name) {
                Some(s) => s,
                None => return fail_result("scenario not found (internal error)".into()),
            };

            let vm = match Machine::spawn(&base_image, &id, ssh_port, &spawn_opts).await {
                Ok(vm) => vm,
                Err(e) => return fail_result(format!("failed to spawn VM: {e:#}")),
            };

            if let Err(e) = machine::copy_ryra_to_vm(&vm, &ryra_bin).await {
                let _ = vm.destroy().await;
                return fail_result(format!("failed to copy ryra to VM: {e:#}"));
            }

            if let Some(fixtures) = &fixtures_dir {
                if let Err(e) = machine::copy_fixtures_to_vm(&vm, fixtures).await {
                    let _ = vm.destroy().await;
                    return fail_result(format!("failed to copy fixtures to VM: {e:#}"));
                }
            }

            let result = scenario.run(&vm).await;

            if !result.passed() && verbose {
                let serial_log = vm.work_dir.join("serial.log");
                if let Ok(content) = tokio::fs::read_to_string(&serial_log).await {
                    let lines: Vec<&str> = content.lines().collect();
                    let start_idx = lines.len().saturating_sub(50);
                    eprintln!("[{name}] --- serial log (last 50 lines) ---");
                    for line in &lines[start_idx..] {
                        eprintln!("  {line}");
                    }
                    eprintln!("[{name}] --- end serial log ---");
                }
            }

            if !keep_failed || result.passed() {
                if let Err(e) = vm.destroy().await {
                    eprintln!("[{name}] warning: failed to destroy VM: {e}");
                }
            } else {
                println!("[{name}] FAILED — keeping VM alive for debugging:");
                vm.keep_alive();
            }

            println!(
                "[{name}] {}",
                if result.passed() { "passed" } else { "FAILED" }
            );
            result
        }));
    }

    let mut results = vec![];
    for handle in handles {
        results.push(handle.await?);
    }

    print_summary(&results);

    if results.iter().any(|r| !r.passed()) {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parse_default_args() {
        let args = Args::parse_from(["ryra-e2e"]);
        assert_eq!(args.parallel, 1);
        assert_eq!(args.distro, Distro::Debian13);
        assert!(!args.redownload);
        assert!(!args.keep_failed);
        assert!(!args.no_kvm);
        assert!(!args.verbose);
        assert_eq!(args.memory, 2048);
        assert_eq!(args.cpus, 2);
        assert!(!args.list);
        assert!(args.tests.is_empty());
    }

    #[test]
    fn parse_no_kvm_and_verbose() {
        let args = Args::parse_from(["ryra-e2e", "--no-kvm", "-v"]);
        assert!(args.no_kvm);
        assert!(args.verbose);
    }

    #[test]
    fn parse_parallel_flag() {
        let args = Args::parse_from(["ryra-e2e", "--parallel=4"]);
        assert_eq!(args.parallel, 4);
    }

    #[test]
    fn parse_distro_flag() {
        let args = Args::parse_from(["ryra-e2e", "--distro", "debian-13"]);
        assert_eq!(args.distro, Distro::Debian13);
    }

    #[test]
    fn parse_test_filters() {
        let args = Args::parse_from(["ryra-e2e", "add_service", "remove"]);
        assert_eq!(args.tests, vec!["add_service", "remove"]);
    }

    #[test]
    fn parse_list_flag() {
        let args = Args::parse_from(["ryra-e2e", "--list"]);
        assert!(args.list);
    }

    #[test]
    fn parse_ryra_bin_flag() {
        let args = Args::parse_from(["ryra-e2e", "--ryra-bin", "/usr/local/bin/ryra"]);
        assert_eq!(args.ryra_bin, Some("/usr/local/bin/ryra".into()));
    }

    #[test]
    fn all_scenarios_are_registered() {
        let names: Vec<&str> = tests::all().iter().map(|s| s.name).collect();
        assert!(names.contains(&"init"));
        assert!(names.contains(&"idempotent-init"));
        assert!(names.contains(&"add-whoami"));
        assert!(names.contains(&"remove-whoami"));
        assert!(names.contains(&"reset"));
        assert!(names.contains(&"add-postgres"));
        assert!(names.contains(&"whoami-plus-postgres"));
        assert!(names.contains(&"re-add-after-remove"));
    }

    #[test]
    fn filter_scenarios_by_substring() {
        let all = tests::all();
        let filters = vec!["whoami".to_string()];
        let filtered: Vec<&str> = all
            .iter()
            .filter(|s| filters.iter().any(|f| s.name.contains(f.as_str())))
            .map(|s| s.name)
            .collect();
        assert!(filtered.contains(&"add-whoami"));
        assert!(filtered.contains(&"remove-whoami"));
        assert!(!filtered.contains(&"init"));
    }

    #[test]
    fn empty_filter_matches_all() {
        let all = tests::all();
        let filters: Vec<String> = vec![];
        let filtered: Vec<&str> = if filters.is_empty() {
            all.iter().map(|s| s.name).collect()
        } else {
            all.iter()
                .filter(|s| filters.iter().any(|f| s.name.contains(f.as_str())))
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(filtered.len(), all.len());
    }
}
