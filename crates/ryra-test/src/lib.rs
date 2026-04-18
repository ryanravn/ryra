pub mod executor;
pub mod registry;
mod reports;
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

/// Render `--list` output. Two sections:
///  1. **Service tests** — grouped under the owning service name
///     (derived from `registry/<svc>/test.toml`).
///  2. **Service-agnostic tests** — flat list from `registry/tests/*.toml`.
///
/// Each line shows the test name, step count, `[browser]` flag, and
/// distinct step kinds so `playwright`/`shell`/`http` tell you what
/// the test does at a glance.
///
/// When `verbose` is set, each test also gets a breakdown of every step
/// (commands, URLs, polls, heredoc bodies) so the caller can see exactly
/// what the test runs without opening the `.toml`.
fn render_list(discovered: &[registry::DiscoveredTest], registry_path: &Path, verbose: bool) {
    if discovered.is_empty() {
        println!("No tests discovered.");
        return;
    }

    let tests_dir = registry_path.join("tests");
    let is_cross_cutting = |p: &Path| p.starts_with(&tests_dir);

    // Group service tests by owning directory name; keep cross-cutting
    // tests flat since each file already contains a single test.
    let mut service_groups: Vec<(String, Vec<&registry::DiscoveredTest>)> = Vec::new();
    let mut cross_cutting: Vec<&registry::DiscoveredTest> = Vec::new();
    for test in discovered {
        let src = test.source();
        if is_cross_cutting(src) {
            cross_cutting.push(test);
            continue;
        }
        let svc = src
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        if let Some((_, bucket)) = service_groups.iter_mut().find(|(s, _)| s == &svc) {
            bucket.push(test);
        } else {
            service_groups.push((svc, vec![test]));
        }
    }
    service_groups.sort_by(|a, b| a.0.cmp(&b.0));
    cross_cutting.sort_by(|a, b| a.name().cmp(b.name()));

    let total_tests: usize = discovered.len();
    let file_count = service_groups.len() + cross_cutting.len();
    println!("{total_tests} tests across {file_count} files");

    let line = |t: &registry::DiscoveredTest, indent: &str| {
        let kinds = t.step_kinds().join(" → ");
        let browser = if t.needs_browser() { " [browser]" } else { "" };
        let step_count = t.test_count();
        println!(
            "{indent}{:<34} {} step{}{browser}  · {kinds}",
            t.name(),
            step_count,
            if step_count == 1 { "" } else { "s" },
        );
        if !verbose {
            return;
        }
        // Verbose: print each step's details. Use a deeper indent so the
        // hierarchy (group → test → step lines) stays readable.
        let step_indent = format!("{indent}    ");
        if let registry::DiscoveredTest::Lifecycle { steps, .. } = t {
            for (i, step) in steps.iter().enumerate() {
                let described = step.describe();
                if let Some((head, rest)) = described.split_first() {
                    println!("{step_indent}{:>2}. {head}", i + 1);
                    for l in rest {
                        println!("{step_indent}    {l}");
                    }
                }
            }
        } else if let registry::DiscoveredTest::Simple { tests, .. } = t {
            for (i, entry) in tests.iter().enumerate() {
                println!(
                    "{step_indent}{:>2}. shell '{}'  (timeout={}s)",
                    i + 1,
                    entry.name,
                    entry.timeout_secs
                );
                for l in entry.run.trim().lines() {
                    println!("{step_indent}    | {l}");
                }
            }
        }
    };

    if !service_groups.is_empty() {
        println!("─── Service tests  (registry/<service>/test.toml) ───");
        for (svc, tests) in &service_groups {
            println!("{svc}");
            for t in tests {
                line(t, "  ");
            }
        }
    }

    if !cross_cutting.is_empty() {
        println!("─── Service-agnostic tests  (registry/tests/*.toml) ───");
        for t in &cross_cutting {
            line(t, "");
        }
    }
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

    /// Run tests directly on the host without a VM
    #[arg(long)]
    pub no_vm: bool,

    /// Skip setup steps (add/wait/remove/reset) and only run shell/playwright
    /// steps. Use to re-run tests quickly when services are already installed.
    #[arg(long)]
    pub retest: bool,

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
    // The currently running binary is the one being tested — `ryra test` is a
    // subcommand of `ryra` itself, so whichever binary the user launched is by
    // definition the one we want to copy into VMs. Using current_exe avoids the
    // old footgun where we'd silently prefer target/release/ryra even when the
    // user had just rebuilt debug.
    let exe = std::env::current_exe()
        .context("failed to resolve current executable path for ryra binary")?;
    std::fs::canonicalize(&exe).context("failed to canonicalize current executable path")
}

/// Walk `crates/` looking for any `.rs` or `Cargo.toml` newer than `binary`.
/// Returns the newest offending source file, if any. Cheap (~few ms for <1000
/// files) because we only stat metadata, not read contents.
fn newest_source_newer_than(binary: &Path) -> Result<Option<(PathBuf, std::time::SystemTime)>> {
    let bin_mtime = std::fs::metadata(binary)
        .with_context(|| format!("stat binary {}", binary.display()))?
        .modified()
        .context("binary modified-time")?;
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crates_dir = match std::fs::canonicalize(workspace_root.join("crates")) {
        Ok(p) => p,
        // Running outside the workspace (e.g. an installed binary) — no check.
        Err(_) => return Ok(None),
    };

    fn is_source(path: &Path) -> bool {
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            return true;
        }
        matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("Cargo.toml")
        )
    }

    fn walk(
        dir: &Path,
        bin_mtime: std::time::SystemTime,
        newest: &mut Option<(PathBuf, std::time::SystemTime)>,
    ) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                // Skip build output dirs — they contain generated files we don't care about.
                if matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("target") | Some(".git") | Some("node_modules")
                ) {
                    continue;
                }
                walk(&path, bin_mtime, newest)?;
            } else if ft.is_file() && is_source(&path) {
                let mtime = entry.metadata()?.modified()?;
                if mtime > bin_mtime && newest.as_ref().is_none_or(|(_, t)| mtime > *t) {
                    *newest = Some((path, mtime));
                }
            }
        }
        Ok(())
    }

    let mut newest = None;
    walk(&crates_dir, bin_mtime, &mut newest)?;
    Ok(newest)
}

/// Error out if the `ryra` binary we're about to ship into VMs is older than
/// any workspace source file. This is the stale-binary footgun: `cargo build -p
/// ryra-test` rebuilds the lib but leaves `target/release/ryra` untouched, so
/// tests silently run against old behavior.
fn ensure_binary_fresh(binary: &Path) -> Result<()> {
    let Some((src, _)) = newest_source_newer_than(binary)? else {
        return Ok(());
    };
    anyhow::bail!(
        "ryra binary is older than source {}.\n  \
         Binary:  {}\n  \
         Rebuild: cargo build --release --bin ryra\n  \
         (or pass --ryra-bin <path> to skip this check)",
        src.display(),
        binary.display(),
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
    reports::save_run_results(results)?;
    reports::print_results_paths(results);
    Ok(())
}

/// Safety margin (MB) kept free beyond the VMs' own needs — for host processes,
/// QEMU overhead, the kernel page cache, and the GPU compositor. Running this
/// tight causes kernel-level thrashing and on Asahi can freeze the display.
const HOST_RESERVE_MB: u64 = 1024;

/// Decide how many VMs can safely run in parallel given current host memory.
/// Returns the clamped parallel count (never more than `requested`, never below 1),
/// and prints a report. Uses `sorted_mems_desc` so we pack the largest VMs first.
fn plan_parallelism(requested: usize, sorted_mems_desc: &[u32]) -> usize {
    let mem = match ryra_vm::read_host_memory() {
        Some(m) => m,
        None => {
            let total_mb: u64 = sorted_mems_desc
                .iter()
                .take(requested)
                .map(|m| *m as u64)
                .sum();
            println!("\nMax concurrent VM RAM: {total_mb}MB (host memory unknown)");
            return requested.max(1);
        }
    };

    let used_mb = mem.total_mb.saturating_sub(mem.available_mb);
    println!(
        "\nHost RAM: {}MB used / {}MB total ({}MB available, {}MB in swap)",
        used_mb, mem.total_mb, mem.available_mb, mem.swap_used_mb
    );

    let budget = mem.available_mb.saturating_sub(HOST_RESERVE_MB);
    let mut fit = 0usize;
    let mut total = 0u64;
    for m in sorted_mems_desc.iter().take(requested) {
        let next = total + *m as u64;
        if next > budget {
            break;
        }
        total = next;
        fit += 1;
    }

    let first_vm_mb = sorted_mems_desc.first().copied().unwrap_or(0) as u64;
    if fit == 0 && first_vm_mb > 0 {
        // Even one VM doesn't fit in budget — warn loudly but still let it run at
        // parallel=1 so the user can choose to override with --memory.
        eprintln!(
            "WARNING: largest VM needs {}MB but only {}MB free after {}MB host reserve. \
             Running anyway at --parallel=1 — expect swap pressure. Close apps or lower \
             VM size with --memory.",
            first_vm_mb, budget, HOST_RESERVE_MB
        );
        fit = 1;
    }

    let clamped = fit.min(requested).max(1);
    if clamped < requested {
        eprintln!(
            "Reducing --parallel from {requested} to {clamped} to fit in {budget}MB RAM budget \
             (total host RAM {}MB, {}MB reserved for host)",
            mem.total_mb, HOST_RESERVE_MB
        );
    }
    println!("Max concurrent VM RAM: {total}MB (parallel={clamped})");
    clamped
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
        // Respect positional filters: `ryra test --list whoami` shows only
        // whoami tests. Same substring-contains semantics as the run path.
        let filtered: Vec<registry::DiscoveredTest> = if args.tests.is_empty() {
            discovered
        } else {
            discovered
                .into_iter()
                .filter(|t| args.tests.iter().any(|f| t.name().contains(f.as_str())))
                .collect()
        };
        render_list(&filtered, registry_path.as_path(), args.verbose);
        return Ok(());
    }

    // --keep-alive with no tests: boot a VM and block until Ctrl-C.
    // This path needs VM prerequisites, so handle it after the no-vm branch below.
    let keep_alive_interactive = args.keep_alive && args.tests.is_empty();

    if discovered.is_empty() && !keep_alive_interactive {
        anyhow::bail!("no tests found in registry at {}", registry_path.display());
    }

    // Filter tests (independent of VM prep — safe to do first)
    let to_run: Vec<_> = if args.tests.is_empty() {
        discovered.iter().collect()
    } else {
        discovered
            .iter()
            .filter(|t| args.tests.iter().any(|f| t.name().contains(f.as_str())))
            .collect()
    };

    if to_run.is_empty() && !keep_alive_interactive {
        anyhow::bail!("no tests matched the given filters");
    }

    // Fresh report directory for this run. Previous run's output is discarded.
    reports::wipe_reports_dir()?;

    // --no-vm: run entirely on the host. Skip all VM prerequisites, binary
    // lookup, and image preparation since none of it is needed in bare mode.
    if args.no_vm {
        return run_bare(&args, &to_run, &registry_path).await;
    }

    let use_kvm = !args.no_kvm;
    ryra_vm::check_prerequisites(use_kvm)?;

    let memory_override = args.memory;
    let spawn_opts = std::sync::Arc::new(SpawnOpts {
        use_kvm,
        memory_mb: memory_override.unwrap_or(2048),
        cpus: args.cpus,
        disk_gb: 20,
    });

    let ryra_bin = match &args.ryra_bin {
        // Explicit --ryra-bin: trust the user, don't check freshness (the path
        // may be from a different tree, CI artefact, etc.).
        Some(p) => std::fs::canonicalize(p)?,
        None => {
            let bin = find_ryra_binary()?;
            ensure_binary_fresh(&bin)?;
            bin
        }
    };

    // Compute max RAM needed across the tests we're actually running.
    // The snapshot must be created at this size so all VMs can restore from it.
    let max_memory: u32 = to_run
        .iter()
        .map(|t| memory_override.unwrap_or_else(|| registry::vm_memory_for_test(&registry_path, t)))
        .max()
        .unwrap_or(1024);

    let base_image =
        image::ensure_image(&args.distro, args.redownload, use_kvm, max_memory).await?;

    if keep_alive_interactive {
        return run_interactive_vm(&base_image, &spawn_opts, &ryra_bin, &registry_path).await;
    }

    let base_image = std::sync::Arc::new(base_image);
    let registry_path = std::sync::Arc::new(registry_path);

    // Prepare browser image only if a filtered test actually needs it
    let any_needs_browser = to_run.iter().any(|t| t.needs_browser());
    let browser_image = if any_needs_browser {
        Some(std::sync::Arc::new(
            image::ensure_browser_image(&args.distro, args.redownload, use_kvm, max_memory).await?,
        ))
    } else {
        None
    };

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
    let effective_parallel = plan_parallelism(args.parallel, &sorted_mems);
    for (name, mem) in &test_memories {
        println!("  {name}: {mem}MB");
    }
    println!(
        "\nRunning {} tests (parallel={})\n",
        to_run.len(),
        effective_parallel
    );

    let wall_clock = std::time::Instant::now();
    let semaphore = std::sync::Arc::new(Semaphore::new(effective_parallel));
    let mut handles = vec![];
    let total_tests = to_run.len();
    // Shared progress counters — each task increments these when its VM
    // ends so the tail of the output doubles as a live progress ticker
    // (works under --parallel, order-independent).
    let progress_done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let progress_passed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

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
        let test_disk = registry::vm_disk_for_test(&registry_path, test);
        let spawn_opts = std::sync::Arc::new(SpawnOpts {
            use_kvm,
            memory_mb: test_memory,
            cpus: args.cpus,
            disk_gb: test_disk,
        });
        let ryra_bin = ryra_bin.clone();
        let registry_path = registry_path.clone();
        let keep_failed = args.keep_failed;
        let keep_alive = args.keep_alive;
        let verbose = args.verbose;
        let single_test = total_tests == 1;
        let name = test.name().to_string();
        let has_quadlets = test.has_quadlets();
        let progress_done = progress_done.clone();
        let progress_passed = progress_passed.clone();
        // Extract quadlet_dir before spawning task (DiscoveredTest isn't Send)
        let quadlet_dir = match test {
            registry::DiscoveredTest::Simple { setup, .. } => setup.quadlet_dir.clone(),
            registry::DiscoveredTest::Lifecycle { .. } => None,
        };

        handles.push(tokio::spawn(async move {
            // `permit` holds a slot in the `--parallel` semaphore; must be
            // alive until the task finishes. Kept as an explicit local so
            // Drop order is obvious to readers (and to the compiler —
            // `let _x = ...` used to be load-bearing here; drop at end
            // via explicit bind + final drop avoids any NLL surprises).
            let permit_guard = permit;
            let id = machine::random_id();
            let ssh_port = ports::allocate_ssh_port();
            let start = std::time::Instant::now();
            println!("[{name}] ---- VM START ryra-test-{id} (ssh port {ssh_port}, {test_memory}MB RAM) ----");

            // All fallible work lives in an inner async block so every exit
            // path — including early returns for VM-boot or file-copy failures —
            // flows through the single VM END reporting block below. Without
            // this, a `return fail_result(...)` would skip the VM END print and
            // the user would see back-to-back VM STARTs with no indication of
            // what went wrong on the previous test.
            let result: ScenarioResult = async {
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
                let executor = crate::executor::VmExecutor::new(&vm);
                let vm_registry = std::path::Path::new("/opt/ryra-test-registry");
                let result = match &test {
                    registry::DiscoveredTest::Lifecycle { steps, .. } => {
                        runner::run_lifecycle_test(&executor, &name, steps, verbose, single_test, vm_registry, false).await
                    }
                    registry::DiscoveredTest::Simple { .. } => {
                        runner::run_registry_test(&executor, &test).await
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
                if should_keep {
                    println!("[{name}] keeping VM alive:");
                    vm.keep_alive();
                } else if let Err(e) = vm.destroy().await {
                    eprintln!("[{name}] warning: failed to destroy VM: {e}");
                }

                result
            }
            .await;

            // Single end-of-task reporting path — runs for every outcome above,
            // so the user always sees a VM END line (with the failure reason
            // for fails) before the next test's VM START prints.
            use std::sync::atomic::Ordering;
            let done = progress_done.fetch_add(1, Ordering::SeqCst) + 1;
            if result.passed() {
                progress_passed.fetch_add(1, Ordering::SeqCst);
            }
            let passed_so_far = progress_passed.load(Ordering::SeqCst);
            let failed_so_far = done - passed_so_far;
            let wall = wall_clock.elapsed().as_secs();
            let (mins, secs) = (wall / 60, wall % 60);
            let status = match &result.outcome {
                scenario::Outcome::Passed => "PASS".to_string(),
                scenario::Outcome::Skipped => "SKIP".to_string(),
                scenario::Outcome::Failed(msg) => {
                    let first = msg.lines().next().unwrap_or("");
                    let trimmed: String = first.chars().take(140).collect();
                    if first.chars().count() > 140 {
                        format!("FAIL: {trimmed}…")
                    } else {
                        format!("FAIL: {trimmed}")
                    }
                }
            };
            println!(
                "[{name}] ---- VM END ({status}, test {:.1}s) ---- \
                 [{done}/{total_tests} · {passed_so_far} pass · {failed_so_far} fail · \
                 total {mins}:{secs:02}]",
                start.elapsed().as_secs_f64()
            );
            drop(permit_guard); // release the --parallel slot AFTER reporting
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
         -i {}/id_ed25519 -p {} ryra@{}",
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

/// Clean host state between bare-mode tests: uninstall all ryra-managed
/// services and remove the bundled registry cache (which can be polluted by
/// tests like diff-whoami that intentionally mutate it).
async fn reset_bare_state(executor: &crate::executor::LocalExecutor) {
    use crate::executor::Executor;
    let _ = executor.exec("ryra reset -y").await;
    let _ = executor
        .exec("rm -rf \"${XDG_CACHE_HOME:-$HOME/.cache}/ryra/bundled\"")
        .await;
}

/// Run tests directly on the host without a VM.
async fn run_bare(
    args: &Args,
    to_run: &[&registry::DiscoveredTest],
    registry_path: &Path,
) -> Result<()> {
    let wall_clock = std::time::Instant::now();
    let executor = crate::executor::LocalExecutor;
    let mut results = Vec::new();
    let single_test = to_run.len() == 1;

    println!("\nRunning {} tests on host (bare mode)\n", to_run.len());

    for test in to_run {
        let name = test.name().to_string();
        println!("---- START {name} (bare) ----");

        // Reset host state between tests so one test's leftover services or
        // cache pollution doesn't cascade into the next. Bare mode shares the
        // host's ryra config/cache across all tests — unlike VM mode where
        // each test gets a fresh VM. Failures here are non-fatal: the first
        // reset may find nothing to clean, and test assertions will surface
        // any real setup failures.
        reset_bare_state(&executor).await;

        let start = std::time::Instant::now();
        let result = match test {
            registry::DiscoveredTest::Lifecycle { steps, .. } => {
                runner::run_lifecycle_test(
                    &executor,
                    &name,
                    steps,
                    args.verbose,
                    single_test,
                    registry_path,
                    args.retest,
                )
                .await
            }
            registry::DiscoveredTest::Simple { .. } => {
                runner::run_registry_test(&executor, test).await
            }
        };

        let status = if result.passed() { "PASS" } else { "FAIL" };
        println!(
            "---- END {name} ({status}, {:.1}s) ----",
            start.elapsed().as_secs_f64()
        );
        results.push(result);
    }

    print_summary(&results, wall_clock.elapsed());
    save_results(&results)?;

    if results.iter().any(|r| !r.passed()) {
        std::process::exit(1);
    }

    Ok(())
}
