use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

use ryra_core::Step;

/// Execute a list of steps, stopping on first failure.
pub async fn execute_all(steps: &[Step]) -> Result<()> {
    let verbose = crate::verbose::is_enabled();
    for step in steps {
        let start = std::time::Instant::now();
        execute(step).await?;
        if verbose {
            let elapsed = start.elapsed();
            if elapsed.as_millis() > 500 {
                println!("    ({:.1}s)", elapsed.as_secs_f64());
            }
        }
    }

    Ok(())
}

/// Execute a single step.
async fn execute(step: &Step) -> Result<()> {
    match step {
        Step::WriteFile(file) => {
            if crate::verbose::is_enabled() {
                println!("  Writing {}", file.path.display());
                if !file.content.is_empty() {
                    for line in file.content.lines() {
                        println!("    | {line}");
                    }
                    println!();
                }
            }
            // Pick the permission mode by file kind:
            // - `.env` / `ryra.toml`  — contain credentials, owner-only (0o600)
            // - `.sh`                  — executable scripts (0o755)
            // - everything else        — conventional world-readable (0o644)
            // Using atomic write across the board so a crash mid-write can't
            // leave a half-written quadlet/config behind.
            let name = file.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = file.path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let mode = if name == ".env" || name == "ryra.toml" {
                0o600
            } else if ext == "sh" {
                0o755
            } else {
                0o644
            };
            ryra_core::system::atomic_write::atomic_write(
                &file.path,
                file.content.as_bytes(),
                mode,
            )
            .with_context(|| format!("failed to write {}", file.path.display()))?;
            Ok(())
        }
        Step::DaemonReload => run_cmd("systemctl", &["--user", "daemon-reload"]),
        Step::StartService { unit } => {
            // Retry once on failure — the first start after daemon-reload can
            // fail on some podman versions due to a race in the quadlet
            // generator's dependency resolution.
            //
            // Services with heavy ExecStartPost (zammad autowizard, etc.) can
            // block `systemctl start` for a minute or more. Print an elapsed
            // counter on stderr so the user sees we're alive.
            with_spinner("starting", unit, || {
                match run_cmd("systemctl", &["--user", "start", unit]) {
                    Ok(()) => Ok(()),
                    Err(_first_err) => {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        run_cmd("systemctl", &["--user", "start", unit])
                    }
                }
            })
        }
        Step::StopService { unit } => {
            // Stop failures are non-fatal (service may already be stopped)
            if let Err(e) = run_cmd("systemctl", &["--user", "stop", unit])
                && crate::verbose::is_enabled()
            {
                eprintln!("  Note: stopping {unit} failed (may already be stopped): {e}");
            }
            Ok(())
        }
        Step::RestartService { unit } => with_spinner("restarting", unit, || {
            run_cmd("systemctl", &["--user", "restart", unit])
        }),
        Step::ReloadCaddy => {
            println!("  Reloading Caddy config...");
            // Wait for Caddy container to be running before reload
            for _ in 0..10 {
                if Command::new("podman")
                    .args(["exec", "caddy", "true"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    return run_cmd(
                        "podman",
                        &[
                            "exec",
                            "caddy",
                            "caddy",
                            "reload",
                            "--config",
                            "/etc/caddy/Caddyfile",
                            "--adapter",
                            "caddyfile",
                        ],
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            // Caddy not running — skip reload (will pick up config on next start)
            println!("    Caddy not running, skipping reload");
            Ok(())
        }
        Step::PullImage { image } => {
            // Skip if already available — check both the local store and
            // additionalimagestores (read-only mounts). `podman image exists`
            // only checks the local store, so fall back to listing images.
            let exists_local = Command::new("podman")
                .args(["image", "exists", image])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if exists_local {
                if crate::verbose::is_enabled() {
                    println!("  {image} already available, skipping pull");
                }
                return Ok(());
            }
            // Check additional image stores via podman images.
            // Quadlets use fully qualified names (e.g. "docker.io/library/caddy:2-alpine"),
            // but older caches may still hold short-name entries — check both forms
            // so existing stores continue to hit.
            let in_additional = Command::new("podman")
                .args(["images", "--format", "{{.Repository}}:{{.Tag}}"])
                .output()
                .map(|o| {
                    let expanded_library = format!("docker.io/library/{image}");
                    let expanded_org = format!("docker.io/{image}");
                    String::from_utf8_lossy(&o.stdout).lines().any(|line| {
                        line == image || line == expanded_library || line == expanded_org
                    })
                })
                .unwrap_or(false);
            if in_additional {
                if crate::verbose::is_enabled() {
                    println!("  {image} available in image store, skipping pull");
                }
                return Ok(());
            }
            if crate::verbose::is_enabled() {
                println!("  Pulling {image}...");
            }
            run_cmd("podman", &["pull", image])
        }
        Step::RemoveFile(path) => std::fs::remove_file(path)
            .with_context(|| format!("failed to remove {}", path.display())),
        Step::RemoveDir(path) => {
            // Service data dirs can contain files owned by podman subuids
            // (from rootless user-namespace mappings). Plain `rm -rf` as the
            // host user gets EPERM on those. `podman unshare rm -rf` runs
            // inside the user namespace where our UID maps to root, so it
            // nukes anything regardless of subuid ownership. Fall back to
            // std::fs on any podman failure (e.g. plain-user-owned paths
            // like ~/.config/ryra) so non-podman dirs still work.
            let path_str = path.display().to_string();
            let unshare = Command::new("podman")
                .args(["unshare", "rm", "-rf", "--", &path_str])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match unshare {
                Ok(status) if status.success() => Ok(()),
                _ => std::fs::remove_dir_all(path)
                    .with_context(|| format!("failed to remove directory {}", path.display())),
            }
        }
        Step::RemoveVolume { name } => {
            // Volume removal is best-effort — the volume may not exist if the
            // container never started, or podman may need the container gone first.
            let status = Command::new("podman")
                .args(["volume", "rm", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if crate::verbose::is_enabled() && !status.map(|s| s.success()).unwrap_or(false) {
                eprintln!("  Note: volume {name} not removed (may not exist)");
            }
            Ok(())
        }
        Step::CreateDir(path) => std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create directory {}", path.display())),
        Step::WaitForFile { path, timeout_secs } => {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs as u64);
            while !path.exists() {
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("timed out waiting for {}", path.display());
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Ok(())
        }
        Step::CopyFile { src, dst } => {
            if crate::verbose::is_enabled() {
                println!("  Copying {} -> {}", src.display(), dst.display());
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent dir {}", parent.display())
                })?;
            }
            std::fs::copy(src, dst).with_context(|| {
                format!("failed to copy {} -> {}", src.display(), dst.display())
            })?;
            Ok(())
        }
    }
}

/// Run `f` with a stderr status line that starts after a 2s grace period.
///
/// The label rotates between:
///   `{verb} {unit}: activating {subunit}… {elapsed}s`
/// when a dependency is mid-start, and
///   `{verb} {unit}: running ExecStartPost… {elapsed}s`
/// when the unit itself is active but a post-start script is still running.
/// Falls back to `{verb} {unit}… {elapsed}s` when systemctl is quiet.
///
/// This keeps systemd-blocking operations legible instead of opaque — the
/// user sees which subunit is currently running (e.g. `zammad-elasticsearch`)
/// rather than a bare elapsed counter.
fn with_spinner<T>(verb: &str, unit: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = Arc::clone(&done);
    let verb_owned = verb.to_string();
    let unit_owned = unit.to_string();
    let handle = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        // 2s grace period — fast operations (most of them) stay quiet.
        for _ in 0..20 {
            if done_clone.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Family pattern for glob matches: `zammad.service` → `zammad*`.
        // Covers sidecars like `zammad-postgres.service`.
        let family_glob = format!(
            "{}*",
            unit_owned.trim_end_matches(".service")
        );
        let mut last_detail = String::new();
        while !done_clone.load(Ordering::Relaxed) {
            let secs = start.elapsed().as_secs();
            let detail = describe_wait(&unit_owned, &family_glob).unwrap_or_default();
            // Clear the line fully before redrawing — detail changes size.
            if detail != last_detail {
                eprint!("\r\x1b[2K");
                last_detail = detail.clone();
            }
            if detail.is_empty() {
                eprint!("\r  {verb_owned} {unit_owned}… {secs}s  ");
            } else {
                eprint!("\r  {verb_owned} {unit_owned}: {detail} ({secs}s)  ");
            }
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_millis(1_000));
        }
    });
    let result = f();
    done.store(true, Ordering::Relaxed);
    let _ = handle.join();
    // Erase the status line.
    eprint!("\r\x1b[2K");
    let _ = std::io::stderr().flush();
    result
}

/// Tail the journal for `family_glob` and return the most recent line
/// matching one of our script-progress prefixes (`autowizard:`, `oidc:`,
/// `smtp:`, etc.). Filters out the firehose of Rails/podman info logs so the
/// spinner stays legible.
fn last_script_progress(family_glob: &str) -> Option<String> {
    let output = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            family_glob,
            "-n",
            "50",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Walk lines in reverse; first match wins. Recognise lines whose first
    // token is a known progress-tag followed by `:`.
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (tag, _) = trimmed.split_once(':')?;
        let tag = tag.trim();
        // Known progress prefixes emitted by registry service scripts plus
        // a couple of systemd standards. Keep this list short and explicit.
        const TAGS: &[&str] = &[
            "autowizard",
            "oidc",
            "smtp",
            "Starting",
            "Started",
            "Stopping",
            "Stopped",
        ];
        if TAGS.iter().any(|t| t == &tag) {
            let out = trimmed.to_string();
            return Some(if out.len() > 80 {
                format!("{}…", &out[..79])
            } else {
                out
            });
        }
    }
    None
}

/// Produce a short description of what we're currently waiting on for `unit`.
/// Returns `None` when the spinner should just show the verb + elapsed time.
fn describe_wait(unit: &str, family_glob: &str) -> Option<String> {
    // First: any subunit in the family currently `activating`?
    let output = Command::new("systemctl")
        .args([
            "--user",
            "list-units",
            family_glob,
            "--state=activating",
            "--no-legend",
            "--no-pager",
            "--all",
            "--plain",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let activating: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.ends_with(".service"))
        .collect();

    // The target unit is in `activating` as long as ExecStartPost scripts are
    // still running. If the ONLY activating unit is the target itself, we're
    // past deps and running our post-start hooks — tail the journal for the
    // last meaningful progress line so the user can see what the scripts say.
    if activating.len() == 1 && activating[0] == unit {
        if let Some(line) = last_script_progress(family_glob) {
            return Some(format!("post-start · {line}"));
        }
        return Some("running post-start scripts".to_string());
    }
    // Otherwise report the non-target subunit(s) that are still starting.
    let others: Vec<&str> = activating.iter().copied().filter(|u| *u != unit).collect();
    if !others.is_empty() {
        let list = others
            .iter()
            .take(3)
            .map(|u| u.trim_end_matches(".service"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = others.len().saturating_sub(3);
        if more > 0 {
            return Some(format!("activating {list} (+{more} more)"));
        }
        return Some(format!("activating {list}"));
    }
    None
}

/// Run a command with explicit program and args (no shell interpretation).
fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let display = format!("{program} {}", args.join(" "));
    if crate::verbose::is_enabled() {
        println!("  $ {display}");
    }
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run: {display}"))?;
    if !status.success() {
        bail!("command failed: {display}");
    }
    Ok(())
}
