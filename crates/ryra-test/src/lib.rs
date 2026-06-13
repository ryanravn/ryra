pub mod executor;
pub mod registry;
pub mod reports;
pub mod runner;
pub mod scenario;
pub mod test_toml;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Semaphore;

use ryra_vm::image::Distro;
use ryra_vm::machine::{self, Machine, SpawnOpts};
use ryra_vm::{image, ports};
use scenario::{Outcome, ScenarioResult};

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
    // Write to stderr manually (signal-safe). Stay mode-agnostic here —
    // cleanup_all_vms reports the VM count only when there's actually one.
    let msg = b"\nInterrupted\n";
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

    // Only the *failures* get their full step trace dumped here — that's the
    // bit you actually need to read inline. Passing tests would just spew
    // every step's captured stdout; their full logs are saved to
    // `reports/<test>/run.log` and pointed at by the path summary below.
    let any_failed = results.iter().any(|r| r.outcome.is_fail());
    for result in results.iter().filter(|r| r.outcome.is_fail()) {
        print!("{result}");
    }
    if any_failed {
        println!();
    }

    let passed = results.iter().filter(|r| r.passed()).count();
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Failed(_)))
        .count();
    let skipped = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Skipped))
        .count();

    println!("----------------------------------------");
    println!(
        "{passed} passed, {failed} failed, {skipped} skipped, {} total ({} wall clock)",
        results.len(),
        reports::humanize_secs(wall_clock.as_secs()),
    );
    println!("========================================");
}

fn save_results(results: &[ScenarioResult], wall_clock: std::time::Duration) -> Result<()> {
    reports::save_run_results(results)?;
    reports::print_results_paths(results, wall_clock);
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

    let candidates = [
        PathBuf::from("registry"),
        PathBuf::from("crates/ryra-core/registry"),
    ];
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
            image::ensure_browser_image(
                &base_image,
                &args.distro,
                args.redownload,
                use_kvm,
                max_memory,
            )
            .await?,
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
    // Start-order counter so each VM START line carries an [N/total] marker
    // too. Under --parallel this is the order tests *begin*, not finish, but
    // it still tells you how far into the run you are at a glance.
    let progress_started = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

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
        let progress_started = progress_started.clone();
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
            let started =
                progress_started.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            println!("[{name}] ---- VM START [{started}/{total_tests}] ryra-test-{id} (ssh port {ssh_port}, {test_memory}MB RAM) ----");

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
                        runner::run_lifecycle_test(&executor, &name, steps, verbose, !single_test, vm_registry, false, None).await
                    }
                    registry::DiscoveredTest::Simple { .. } => {
                        runner::run_registry_test(&executor, &test, !single_test, None).await
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

    let total_elapsed = wall_clock.elapsed();
    print_summary(&results, total_elapsed);
    save_results(&results, total_elapsed)?;

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

/// Root of the host-test sandbox. Everything a host run reads or writes that
/// isn't a quadlet symlink lives under here, on real disk: service data
/// (`services/`), the preferences sandbox (`config/`), the ledger, and run
/// reports (`reports/`). It's `~/.local/share/services-test/` (honouring
/// `XDG_DATA_HOME`), a sibling of the real `~/.local/share/services/`, so the
/// whole test footprint is one folder you can `rm -rf`. `None` if `$HOME` is
/// unset.
pub fn test_sandbox_root() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("services-test"))
}

/// Path to the host-managed-services ledger: the services this harness has
/// installed on the host but not yet torn down. Persisted across runs so a
/// later run can tell *its own* leftovers (from an aborted run — safe to
/// reclaim) apart from services the user installed for real (must never be
/// touched). Lives in the sandbox root (real disk — it must survive reboots,
/// so never `/tmp`). Returns `None` only if `$HOME` is unset.
fn host_ledger_path() -> Option<PathBuf> {
    Some(test_sandbox_root()?.join("ledger"))
}

/// Ledger entries still installed on the host: leftovers from a previous
/// aborted run. The ledger only ever records harness installs, so purging
/// these is always safe: user-installed services are never in it.
pub fn host_leftovers() -> Vec<String> {
    let ledger = ledger_load();
    let installed = scan_installed();
    ledger.intersection(&installed).cloned().collect()
}

/// Load the ledger (newline-separated service names). Missing file → empty.
pub fn ledger_load() -> BTreeSet<String> {
    let Some(path) = host_ledger_path() else {
        return BTreeSet::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Persist the ledger. Best-effort: a write failure only degrades the
/// next run to the *conservative* side (it would treat our leftovers as
/// user-owned and skip them rather than delete anything), so we warn but
/// don't abort the test run.
fn ledger_save(set: &BTreeSet<String>) {
    let Some(path) = host_ledger_path() else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("warning: could not create ledger dir: {e}");
        return;
    }
    let body = set.iter().cloned().collect::<Vec<_>>().join("\n");
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!("warning: could not write host-managed-services ledger: {e}");
    }
}

/// Purge a test's own services from the host, dependents before
/// dependencies (reverse install order). Failures are non-fatal: a
/// not-installed service is a no-op. Callers guarantee these services are
/// harness-owned (never user-installed), so purging is always safe.
pub async fn purge_services(
    executor: &crate::executor::LocalExecutor,
    svcs: &[String],
    when: &str,
) {
    use crate::executor::Executor;
    for svc in svcs.iter().rev() {
        println!("  cleaning up {svc} (purge) {when}");
        let _ = executor
            .exec(&format!("ryra remove --purge {svc} -y"))
            .await;
    }
}

/// Snapshot the ryra-managed services currently installed on the host.
/// A scan failure degrades to "none" so the caller never deletes blindly.
fn scan_installed() -> BTreeSet<String> {
    match ryra_core::scan_managed_services() {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            eprintln!("warning: could not scan installed services ({e}); assuming none");
            BTreeSet::new()
        }
    }
}

/// Collect every `<label>.internal` hostname appearing in `s` into `out`.
fn scan_internal_hosts(s: &str, out: &mut BTreeSet<String>) {
    const SUFFIX: &str = ".internal";
    let bytes = s.as_bytes();
    for (idx, _) in s.match_indices(SUFFIX) {
        let mut start = idx;
        while start > 0 {
            let c = bytes[start - 1];
            if c.is_ascii_alphanumeric() || c == b'-' {
                start -= 1;
            } else {
                break;
            }
        }
        if start < idx {
            out.insert(s[start..idx + SUFFIX.len()].to_ascii_lowercase());
        }
    }
}

/// The `*.internal` hostnames the selected tests will actually contact, so the
/// runner can prime sudo (for `/etc/hosts` writes) *only* when a needed host is
/// missing — never on a run whose hosts already resolve.
///
/// Walks parsed lifecycle steps (`add` args/env, shell bodies, http
/// url/body/headers, playwright env) and reads each referenced playwright spec
/// file — its `*.internal` URL default catches auto-promoted hosts that never
/// appear in the toml. Simple tests (basic 127.0.0.1 installs) are scanned too,
/// cheaply, for completeness.
fn referenced_internal_hosts(
    tests: &[&registry::DiscoveredTest],
    registry_path: &Path,
) -> BTreeSet<String> {
    use crate::test_toml::StepDef;
    let browser_dir = registry_path.join("tests").join("browser");
    let mut out = BTreeSet::new();
    for t in tests {
        match t {
            registry::DiscoveredTest::Lifecycle { steps, .. } => {
                for step in steps {
                    match step {
                        StepDef::Add { args, env, .. } => {
                            if let Some(a) = args {
                                scan_internal_hosts(a, &mut out);
                            }
                            env.values().for_each(|v| scan_internal_hosts(v, &mut out));
                        }
                        StepDef::Shell { run, .. } => scan_internal_hosts(run, &mut out),
                        StepDef::Http {
                            url, body, headers, ..
                        } => {
                            scan_internal_hosts(url, &mut out);
                            if let Some(b) = body {
                                scan_internal_hosts(b, &mut out);
                            }
                            headers
                                .values()
                                .for_each(|v| scan_internal_hosts(v, &mut out));
                        }
                        StepDef::Playwright { spec, env, .. } => {
                            env.values().for_each(|v| scan_internal_hosts(v, &mut out));
                            if let Ok(txt) = std::fs::read_to_string(browser_dir.join(spec)) {
                                scan_internal_hosts(&txt, &mut out);
                            }
                        }
                        _ => {}
                    }
                }
            }
            registry::DiscoveredTest::Simple { tests: entries, .. } => {
                for e in entries {
                    scan_internal_hosts(&e.run, &mut out);
                    e.env
                        .values()
                        .for_each(|v| scan_internal_hosts(v, &mut out));
                }
            }
        }
    }
    out
}

/// The `*.internal` hostnames the selected tests contact that don't already
/// resolve via `/etc/hosts` — the ones ryra will have to add (a privileged
/// write). Empty when every contacted host already resolves.
fn missing_internal_hosts(needed: &BTreeSet<String>) -> Vec<String> {
    let hosts = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
    let present = |h: &str| {
        hosts.lines().any(|l| {
            let l = l.trim();
            !l.starts_with('#') && l.split_whitespace().any(|w| w == h)
        })
    };
    needed.iter().filter(|h| !present(h)).cloned().collect()
}

/// Acquire sudo once, up front, for a run that has privileged steps — so the
/// `sudo -n` those steps issue (inside captured, non-TTY shells that can't
/// themselves prompt) succeed silently for the whole run.
///
/// "Privileged steps" is a general notion, not a hosts special-case: a run
/// qualifies if it must add `*.internal` hostnames to `/etc/hosts` (detected
/// automatically) *or* any selected test declares `requires_sudo` (the escape
/// hatch for tests that shell out to sudo for any other reason). `reasons` is
/// the human-readable list of why; empty means nothing privileged → no-op.
///
/// Returns a keep-alive task that refreshes the credential every 60s for the
/// run's duration (sudo's default `timestamp_timeout` is far shorter than a
/// full suite). Behaviour:
/// - No reasons → `None`; sudo is never touched.
/// - Passwordless sudo → `None`; per-step `sudo -n` already works.
/// - Password required + a TTY → one prompt here, listing the reasons.
/// - Password required + no TTY (CI capturing output) → `None`, degrade
///   gracefully. CI uses `--vm`, which provisions its own passwordless sudo.
async fn acquire_run_sudo(reasons: &[String]) -> Option<tokio::task::JoinHandle<()>> {
    use std::io::IsTerminal;
    use std::time::Duration;

    if reasons.is_empty() {
        return None;
    }

    let passwordless = tokio::process::Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if passwordless {
        return None;
    }
    if !std::io::stderr().is_terminal() {
        return None;
    }

    eprintln!("\n  This run needs sudo for:");
    for r in reasons {
        eprintln!("    - {r}");
    }
    eprintln!("  Caching sudo once so it doesn't prompt mid-test:");
    let primed = tokio::process::Command::new("sudo")
        .arg("-v")
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !primed {
        eprintln!("  (skipped — privileged steps may fail; they'll say which.)\n");
        return None;
    }

    Some(tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            // `-n`: a keep-alive must never block on a prompt. If the cache
            // ever lapses, the next privileged step re-warms it itself.
            let _ = tokio::process::Command::new("sudo")
                .args(["-n", "-v"])
                .status()
                .await;
        }
    }))
}

/// Run tests directly on the host without a VM.
///
/// Bare mode shares the *real* host's ryra state, so isolation is built
/// from three guarantees:
///   1. Preferences are redirected to a throwaway dir (`RYRA_CONFIG_DIR`),
///      so tests never read or clobber the user's SMTP/auth/backup creds.
///   2. Services the user already installed are detected up front and left
///      strictly untouched; any test that would install over one is skipped.
///   3. Every test purges its own services afterwards so they don't pile up
///      and exhaust RAM — and a ledger records harness-owned installs so a
///      later run can reclaim leftovers from an aborted run.
async fn run_bare(
    args: &Args,
    to_run: &[&registry::DiscoveredTest],
    registry_path: &Path,
) -> Result<()> {
    use crate::executor::Executor;
    let wall_clock = std::time::Instant::now();

    // Acquire sudo once, up front, if (and only if) this run has privileged
    // steps: `*.internal` hostnames the tests contact that aren't in /etc/hosts
    // yet (ryra adds them), or a test that declares `requires_sudo`. Held warm
    // for the run so captured, non-TTY steps' `sudo -n` succeed; aborted before
    // we return. A run with nothing privileged never touches sudo.
    let mut sudo_reasons: Vec<String> = Vec::new();
    let missing_hosts = missing_internal_hosts(&referenced_internal_hosts(to_run, registry_path));
    if !missing_hosts.is_empty() {
        sudo_reasons.push(format!(
            "adding {} to /etc/hosts (OIDC/HTTPS service URLs)",
            missing_hosts.join(", ")
        ));
    }
    let sudo_tests: Vec<&str> = to_run
        .iter()
        .filter(|t| t.requires_sudo())
        .map(|t| t.name())
        .collect();
    if !sudo_tests.is_empty() {
        sudo_reasons.push(format!(
            "test(s) that declare requires_sudo: {}",
            sudo_tests.join(", ")
        ));
    }
    let sudo_keepalive = acquire_run_sudo(&sudo_reasons).await;

    // 1. Sandbox the whole run under ~/.local/share/services-test/ (real disk,
    //    a sibling of the real services dir). Service data, preferences, the
    //    ledger, and reports all live here — one folder, one wipe. Only the
    //    quadlet *symlinks* land outside it, in the systemd-mandated dir. Tests
    //    resolve data paths through ${RYRA_DATA_DIR:-…}, so they find the
    //    sandbox here and fall back to the real dir under --vm / normal use.
    let sandbox = test_sandbox_root().context("cannot resolve test sandbox root ($HOME unset)")?;

    // Base executor for cleanup operations (no per-test sandbox needed).
    let base_executor = crate::executor::LocalExecutor::with_registry(registry_path);

    // 2. Anything installed that we didn't install is the user's — off-limits.
    let mut ledger = ledger_load();
    let installed = scan_installed();
    let user_owned: BTreeSet<String> = installed.difference(&ledger).cloned().collect();
    if !user_owned.is_empty() {
        let list = user_owned.iter().cloned().collect::<Vec<_>>().join(", ");
        println!(
            "Leaving {} already-installed service(s) untouched: {list}",
            user_owned.len()
        );
        println!("  Tests installing these are skipped. If they're leftovers from an aborted run,");
        println!("  purge them yourself with `ryra remove --purge <name> -y`.");
    }

    // 3. Reclaim our own leftovers from a previous aborted run (frees RAM).
    let leftovers: Vec<String> = ledger.intersection(&installed).cloned().collect();
    for svc in &leftovers {
        println!("  reclaiming leftover {svc} (purge) from a previous run");
        let _ = base_executor
            .exec(&format!("ryra remove --purge {svc} -y"))
            .await;
        ledger.remove(svc);
    }
    if !leftovers.is_empty() {
        ledger_save(&ledger);
    }

    let mut results = Vec::new();
    let total = to_run.len();
    println!("\nRunning {total} tests on host (bare mode)\n");

    for (idx, test) in to_run.iter().enumerate() {
        let n = idx + 1;
        let name = test.name().to_string();
        let svcs: Vec<String> = test.services().iter().map(|s| s.to_string()).collect();

        // Skip any test that would install over a user-owned service.
        if let Some(conflict) = svcs.iter().find(|s| user_owned.contains(*s)) {
            println!(
                "---- SKIP [{n}/{total}] {name}: '{conflict}' already installed (left untouched) ----"
            );
            results.push(ScenarioResult {
                name,
                events: Vec::new(),
                duration: Duration::ZERO,
                outcome: Outcome::Skipped,
            });
            continue;
        }

        println!("---- START [{n}/{total}] {name} (bare) ----");

        // Record intent before installing, so an abort mid-test still leaves a
        // breadcrumb the next run can reclaim.
        for svc in &svcs {
            ledger.insert(svc.clone());
        }
        ledger_save(&ledger);

        // Per-test sandbox: each test gets its own config and data dirs so
        // no state leaks between tests (same pattern as per-test results).
        let test_dir = sandbox.join("tests").join(&name);
        let config_dir = test_dir.join("config");
        let data_dir = test_dir.join("services");
        let _ = std::fs::remove_dir_all(&config_dir);
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("failed to create {}", config_dir.display()))?;
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let executor = crate::executor::LocalExecutor::with_registry(registry_path)
            .with_config_dir(&config_dir)
            .with_data_dir(&data_dir);

        purge_services(&executor, &svcs, "before test").await;
        let _ = executor
            .exec("rm -rf \"${XDG_CACHE_HOME:-$HOME/.cache}/services/default\"")
            .await;

        let start = std::time::Instant::now();
        let result = match test {
            registry::DiscoveredTest::Lifecycle { steps, .. } => {
                runner::run_lifecycle_test(
                    &executor,
                    &name,
                    steps,
                    args.verbose,
                    false,
                    registry_path,
                    args.retest,
                    None,
                )
                .await
            }
            registry::DiscoveredTest::Simple { .. } => {
                runner::run_registry_test(&executor, test, false, None).await
            }
        };

        let status = if result.passed() { "PASS" } else { "FAIL" };
        println!(
            "---- END [{n}/{total}] {name} ({status}, {:.1}s) ----",
            start.elapsed().as_secs_f64()
        );

        // Tear down everything this test put on the host so nothing
        // accumulates and eats RAM.
        purge_services(&executor, &svcs, "after test").await;
        let leaked: Vec<String> = scan_installed()
            .into_iter()
            .filter(|s| !user_owned.contains(s) && !svcs.contains(s))
            .collect();
        if !leaked.is_empty() {
            purge_services(&executor, &leaked, "after test (side-effect)").await;
        }
        for svc in svcs.iter().chain(leaked.iter()) {
            ledger.remove(svc);
        }
        ledger_save(&ledger);

        results.push(result);
    }

    if let Some(h) = sudo_keepalive {
        h.abort();
    }

    let total_elapsed = wall_clock.elapsed();
    print_summary(&results, total_elapsed);
    save_results(&results, total_elapsed)?;

    if results
        .iter()
        .any(|r| matches!(r.outcome, Outcome::Failed(_)))
    {
        std::process::exit(1);
    }

    Ok(())
}
