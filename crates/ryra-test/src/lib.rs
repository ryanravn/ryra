pub mod registry;
mod runner;
mod scenario;
pub mod test_toml;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Semaphore;

use ryra_vm::image::Distro;
use ryra_vm::machine::{self, Machine, SpawnOpts};
use ryra_vm::{image, ports};
use scenario::ScenarioResult;

/// Install a Ctrl-C handler that kills all active VMs and exits.
fn install_signal_handler() {
    // We use the raw libc handler (not tokio::signal) so it works even if
    // the tokio runtime is blocked or mid-shutdown.
    unsafe {
        libc::signal(
            libc::SIGINT,
            signal_handler as *const () as libc::sighandler_t,
        );
    }
}

extern "C" fn signal_handler(_sig: libc::c_int) {
    // Write to stderr manually (signal-safe)
    let msg = b"\nInterrupted - shutting down VMs...\n";
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
    }
    machine::cleanup_all_vms();
    std::process::exit(130); // 128 + SIGINT
}

#[derive(Parser, Debug)]
#[command(
    name = "ryra-e2e",
    about = "E2E test runner for ryra — spins up QEMU VMs for integration testing"
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

    /// Keep VM alive after tests complete (or boot without running tests).
    /// Prints SSH connection command for interactive use.
    #[arg(long)]
    pub keep_alive: bool,

    /// Disable KVM acceleration (use software emulation — slower)
    #[arg(long)]
    pub no_kvm: bool,

    /// VM memory in MB (overrides auto-detection from service requirements)
    #[arg(long)]
    pub memory: Option<u32>,

    /// VM CPU count
    #[arg(long, default_value_t = 2)]
    pub cpus: u32,

    /// Show serial log output on failure
    #[arg(long, short)]
    pub verbose: bool,

    /// Path to registry directory (auto-detected if omitted)
    #[arg(long)]
    pub registry: Option<PathBuf>,

    /// Path to a local project directory with test.toml (+ optional quadlet files)
    #[arg(long)]
    pub project: Option<PathBuf>,

    /// List available tests
    #[arg(long)]
    pub list: bool,

    /// Test names to run (runs all if empty, supports substring match)
    pub tests: Vec<String>,
}

fn find_ryra_binary() -> Result<PathBuf> {
    // Workspace root is two levels up from crates/ryra-test/
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");

    let search_paths = &["target/release/ryra", "target/debug/ryra"];

    for rel_path in search_paths {
        let path = workspace_root.join(rel_path);
        if path.exists() {
            return Ok(std::fs::canonicalize(&path)?);
        }
    }

    anyhow::bail!(
        "ryra binary not found in target/release or target/debug. \
         Build with: cargo build -p ryra-cli --release"
    )
}

fn print_summary(results: &[ScenarioResult], wall_clock: std::time::Duration) {
    println!("\n========================================");
    println!("  Results");
    println!("========================================\n");

    for result in results {
        print!("{result}");
    }

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results.len() - passed;

    println!("----------------------------------------");
    println!(
        "{passed} passed, {failed} failed, {} total ({:.0}s wall clock)",
        results.len(),
        wall_clock.as_secs_f64()
    );
    println!("========================================");
}

fn save_results(results: &[ScenarioResult]) -> Result<()> {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let log_dir = workspace_root.join("crates/ryra-test/logs");
    std::fs::create_dir_all(&log_dir)?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_path = log_dir.join(format!("e2e-{timestamp}.log"));

    let mut output = String::new();
    for result in results {
        output.push_str(&format!("{result}"));
    }

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results.len() - passed;
    let total_duration: std::time::Duration = results.iter().map(|r| r.duration).sum();
    output.push_str(&format!(
        "\n{passed} passed, {failed} failed, {} total ({:.1}s)\n",
        results.len(),
        total_duration.as_secs_f64()
    ));

    std::fs::write(&log_path, &output)?;

    let latest = log_dir.join("latest.log");
    let _ = std::fs::remove_file(&latest);
    std::fs::write(&latest, &output)?;

    println!("Results saved to: {}", log_path.display());

    Ok(())
}

fn print_memory_summary(max_concurrent_mb: u64) {
    if let Some((total_mb, used_mb)) = ryra_vm::read_host_memory() {
        let avail_mb = total_mb.saturating_sub(used_mb);
        println!("\nHost RAM: {used_mb}MB used / {total_mb}MB total ({avail_mb}MB available)");
        println!("Max concurrent VM RAM: {max_concurrent_mb}MB");
        if max_concurrent_mb > avail_mb {
            eprintln!(
                "WARNING: VMs may need up to {max_concurrent_mb}MB but only {avail_mb}MB available — \
                 reduce --parallel or --memory"
            );
        }
    } else {
        println!("\nMax concurrent VM RAM: {max_concurrent_mb}MB");
    }
}

/// Find the registry path — explicit arg, or auto-detect.
fn resolve_registry_path(explicit: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return std::fs::canonicalize(p)
            .with_context(|| format!("registry path not found: {}", p.display()));
    }

    let candidates = [PathBuf::from("registry")];
    for c in &candidates {
        if c.exists() {
            return std::fs::canonicalize(c)
                .with_context(|| format!("failed to resolve {}", c.display()));
        }
    }

    anyhow::bail!("no registry found. Pass --registry <path> or run from the repo root")
}

/// Run the E2E test suite with the given arguments.
pub async fn run(args: Args) -> Result<()> {
    install_signal_handler();

    // Check for local project first, then fall back to registry
    let registry_path = resolve_registry_path(args.registry.as_ref());

    let mut discovered = Vec::new();

    // Discover local project tests (--project flag)
    if let Some(ref project_dir) = args.project {
        match registry::discover_local_project(project_dir)? {
            Some(test) => discovered.push(test),
            None => {
                anyhow::bail!(
                    "no test.toml found in project directory: {}",
                    project_dir.display()
                );
            }
        }
    }

    // Discover registry tests (only if no explicit --project or if registry is also available)
    if let Ok(ref reg_path) = registry_path
        && let Ok(reg_tests) = registry::discover(reg_path)
    {
        // If --project was explicitly passed, skip registry tests
        if args.project.is_none() {
            discovered.extend(reg_tests);
        }
    }

    // Need a registry path for dependency resolution even with local projects
    let registry_path = registry_path.unwrap_or_else(|_| PathBuf::from("registry"));

    if args.list {
        if !discovered.is_empty() {
            println!("Available tests:");
        }
        for test in &discovered {
            if test.is_lifecycle() {
                println!(
                    "  {:<30} ({} steps, lifecycle)",
                    test.name(),
                    test.test_count(),
                );
            } else if test.has_quadlets() {
                println!(
                    "  {:<30} ({} tests, quadlets)",
                    test.name(),
                    test.test_count(),
                );
            } else {
                let svc_list = test.services().join(" + ");
                println!(
                    "  {:<30} ({} tests, services: {})",
                    test.name(),
                    test.test_count(),
                    svc_list
                );
            }
        }
        return Ok(());
    }

    let use_kvm = !args.no_kvm;
    ryra_vm::check_prerequisites(use_kvm)?;

    let memory_override = args.memory;
    let spawn_opts = std::sync::Arc::new(SpawnOpts {
        use_kvm,
        memory_mb: memory_override.unwrap_or(2048),
        cpus: args.cpus,
    });

    let ryra_bin = match &args.ryra_bin {
        Some(p) => std::fs::canonicalize(p)?,
        None => find_ryra_binary()?,
    };

    let base_image = image::ensure_image(&args.distro, args.redownload, use_kvm).await?;

    // --keep-alive with no tests: boot a VM and block until Ctrl-C
    if args.keep_alive && args.tests.is_empty() {
        return run_interactive_vm(&base_image, &spawn_opts, &ryra_bin, &registry_path).await;
    }

    if discovered.is_empty() {
        anyhow::bail!("no tests found in registry at {}", registry_path.display());
    }

    // Prepare browser image if any test needs it
    let any_needs_browser = discovered.iter().any(|t| t.needs_browser());
    let browser_image = if any_needs_browser {
        Some(image::ensure_browser_image(&args.distro, args.redownload, use_kvm).await?)
    } else {
        None
    };

    let base_image = std::sync::Arc::new(base_image);
    let browser_image = browser_image.map(std::sync::Arc::new);
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

    // Pre-pull all container images before spawning VMs.
    let mut all_images: Vec<String> = to_run
        .iter()
        .flat_map(|t| registry::images_for_test(&registry_path, t))
        .collect();
    all_images.sort();
    all_images.dedup();

    println!("Pre-caching {} container images...", all_images.len());
    for img in &all_images {
        machine::ensure_image_cached(img).await?;
    }

    // Compute per-test memory first (needed for accurate parallelism calculation)
    let test_memories: Vec<(&str, u32)> = to_run
        .iter()
        .map(|t| {
            let mem =
                memory_override.unwrap_or_else(|| registry::vm_memory_for_test(&registry_path, t));
            (t.name(), mem)
        })
        .collect();

    let mut sorted_mems: Vec<u32> = test_memories.iter().map(|(_, m)| *m).collect();
    sorted_mems.sort_unstable_by(|a, b| b.cmp(a));
    let max_concurrent_mb: u64 = sorted_mems
        .iter()
        .take(args.parallel)
        .map(|m| *m as u64)
        .sum();
    print_memory_summary(max_concurrent_mb);
    for (name, mem) in &test_memories {
        println!("  {name}: {mem}MB");
    }
    println!(
        "\nRunning {} tests (parallel={})\n",
        to_run.len(),
        args.parallel
    );

    let wall_clock = std::time::Instant::now();
    let semaphore = std::sync::Arc::new(Semaphore::new(args.parallel));
    let mut handles = vec![];
    let total_tests = to_run.len();

    for test in to_run {
        let permit = semaphore.clone().acquire_owned().await?;
        let test_image: std::sync::Arc<image::Image> = if test.needs_browser() {
            match browser_image.as_ref() {
                Some(img) => img.clone(),
                None => {
                    anyhow::bail!(
                        "test '{}' requires a browser image but none was prepared",
                        test.name()
                    );
                }
            }
        } else {
            base_image.clone()
        };
        let test_memory =
            memory_override.unwrap_or_else(|| registry::vm_memory_for_test(&registry_path, test));
        let spawn_opts = std::sync::Arc::new(SpawnOpts {
            use_kvm,
            memory_mb: test_memory,
            cpus: args.cpus,
        });
        let ryra_bin = ryra_bin.clone();
        let registry_path = registry_path.clone();
        let keep_failed = args.keep_failed;
        let keep_alive = args.keep_alive;
        let verbose = args.verbose;
        let single_test = total_tests == 1;
        let name = test.name().to_string();
        let has_quadlets = test.has_quadlets();
        // Extract quadlet_dir before spawning task (DiscoveredTest isn't Send)
        let quadlet_dir = match test {
            registry::DiscoveredTest::Simple { setup, .. } => setup.quadlet_dir.clone(),
            registry::DiscoveredTest::Lifecycle { .. } => None,
        };

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let id = machine::random_id();
            let ssh_port = ports::allocate_ssh_port();
            let start = std::time::Instant::now();
            println!("[{name}] ---- VM START ryra-test-{id} (ssh port {ssh_port}, {test_memory}MB RAM) ----");

            let fail_result = |msg: String| ScenarioResult {
                name: name.clone(),
                events: vec![],
                duration: start.elapsed(),
                outcome: scenario::Outcome::Failed(msg),
            };

            // Re-discover tests inside task (DiscoveredTest isn't Send due to lifetime)
            let test = if has_quadlets {
                let qdir = match quadlet_dir.as_ref() {
                    Some(d) => d,
                    None => return fail_result("quadlet_dir must be set for quadlet tests".into()),
                };
                match registry::discover_local_project(qdir) {
                    Ok(Some(t)) => t,
                    Ok(None) => return fail_result("local project not found (internal error)".into()),
                    Err(e) => return fail_result(format!("local project discovery failed: {e:#}")),
                }
            } else {
                let discovered = match registry::discover(&registry_path) {
                    Ok(d) => d,
                    Err(e) => return fail_result(format!("registry discovery failed: {e:#}")),
                };
                match discovered.into_iter().find(|t| t.name() == name) {
                    Some(t) => t,
                    None => return fail_result("test not found (internal error)".into()),
                }
            };

            // Spawn VM
            let phase = std::time::Instant::now();
            println!("[{name}] booting VM...");
            let vm = match Machine::spawn(&test_image, &id, ssh_port, &spawn_opts).await {
                Ok(vm) => vm,
                Err(e) => return fail_result(format!("failed to spawn VM: {e:#}")),
            };
            println!("[{name}] VM ready ({:.1}s)", phase.elapsed().as_secs_f64());

            // Copy ryra binary into VM
            let phase = std::time::Instant::now();
            if let Err(e) = machine::copy_ryra_to_vm(&vm, &ryra_bin).await {
                let _ = vm.destroy().await;
                return fail_result(format!("failed to copy ryra to VM: {e:#}"));
            }

            // Copy registry into VM (needed for dependency resolution)
            if registry_path.exists()
                && let Err(e) = machine::copy_fixtures_to_vm(&vm, &registry_path).await {
                    let _ = vm.destroy().await;
                    return fail_result(format!("failed to copy registry to VM: {e:#}"));
                }

            // Copy quadlet project files into VM
            if let Some(ref qdir) = quadlet_dir
                && let Err(e) = machine::copy_project_to_vm(&vm, qdir).await {
                    let _ = vm.destroy().await;
                    return fail_result(format!("failed to copy project to VM: {e:#}"));
                }
            println!("[{name}] files copied ({:.1}s)", phase.elapsed().as_secs_f64());

            // Load cached container images into VM
            let images = registry::images_for_test(&registry_path, &test);
            if !images.is_empty() {
                let phase = std::time::Instant::now();
                if let Err(e) = machine::load_images_into_vm(&vm, &images).await {
                    let _ = vm.destroy().await;
                    return fail_result(format!("failed to load container images: {e:#}"));
                }
                println!("[{name}] images loaded ({:.1}s, {} images)", phase.elapsed().as_secs_f64(), images.len());
            }

            let setup_time = start.elapsed();
            println!("[{name}] running tests (setup took {:.1}s)...", setup_time.as_secs_f64());
            let result = match &test {
                registry::DiscoveredTest::Lifecycle { steps, .. } => {
                    runner::run_lifecycle_test(&vm, &name, steps, "/opt/ryra-test-registry", verbose, single_test).await
                }
                registry::DiscoveredTest::Simple { .. } => {
                    runner::run_registry_test(&vm, &test, "/opt/ryra-test-registry").await
                }
            };

            // On failure, save serial log to logs dir
            if !result.passed() {
                let serial_log = vm.work_dir.join("serial.log");
                if let Ok(content) = tokio::fs::read_to_string(&serial_log).await {
                    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
                    let fail_log_dir = workspace_root.join("crates/ryra-test/logs");
                    let _ = tokio::fs::create_dir_all(&fail_log_dir).await;
                    let dest = fail_log_dir.join(format!("{name}-serial.log"));
                    let _ = tokio::fs::write(&dest, &content).await;
                    eprintln!("[{name}] serial log saved to: {}", dest.display());

                    if verbose {
                        let lines: Vec<&str> = content.lines().collect();
                        let start_idx = lines.len().saturating_sub(50);
                        eprintln!("[{name}] --- serial log (last 50 lines) ---");
                        for line in &lines[start_idx..] {
                            eprintln!("  {line}");
                        }
                        eprintln!("[{name}] --- end serial log ---");
                    }
                }
            }

            // Decide whether to keep the VM alive
            let should_keep = keep_alive || (keep_failed && !result.passed());
            let status = if result.passed() { "PASS" } else { "FAIL" };
            let elapsed = start.elapsed();
            if should_keep {
                println!("[{name}] keeping VM alive:");
                vm.keep_alive();
            } else {
                if let Err(e) = vm.destroy().await {
                    eprintln!("[{name}] warning: failed to destroy VM: {e}");
                }
            }

            println!("[{name}] ---- VM END ({status}, {:.1}s) ----", elapsed.as_secs_f64());
            result
        }));
    }

    let mut results = vec![];
    for handle in handles {
        results.push(handle.await?);
    }

    print_summary(&results, wall_clock.elapsed());
    save_results(&results)?;

    if results.iter().any(|r| !r.passed()) {
        std::process::exit(1);
    }

    Ok(())
}

/// Boot a VM with ryra + registry installed, print SSH command, block until Ctrl-C.
async fn run_interactive_vm(
    base_image: &image::Image,
    spawn_opts: &SpawnOpts,
    ryra_bin: &Path,
    registry_path: &Path,
) -> Result<()> {
    let id = machine::random_id();
    let ssh_port = ports::allocate_ssh_port();

    println!("Booting interactive VM ryra-test-{id} (ssh port {ssh_port})...");
    let vm = Machine::spawn(base_image, &id, ssh_port, spawn_opts).await?;
    println!("VM ready.");

    println!("Copying ryra binary...");
    machine::copy_ryra_to_vm(&vm, ryra_bin).await?;

    println!("Copying registry...");
    machine::copy_fixtures_to_vm(&vm, registry_path).await?;

    println!("\nVM is ready. Connect with:\n");
    println!(
        "  ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -i {}/id_ed25519 -p {} root@{}",
        vm.work_dir.display(),
        vm.ssh_port,
        vm.ssh_host,
    );
    println!("\nRegistry is at /opt/ryra-test-registry in the VM.");
    println!("Press Ctrl-C to stop the VM.\n");

    tokio::signal::ctrl_c().await?;

    println!("\nShutting down VM...");
    vm.destroy().await?;
    Ok(())
}
