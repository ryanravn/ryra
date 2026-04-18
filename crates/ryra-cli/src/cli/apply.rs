use std::process::{Command, Stdio};

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
            println!("  Writing {}", file.path.display());
            if crate::verbose::is_enabled() && !file.content.is_empty() {
                for line in file.content.lines() {
                    println!("    | {line}");
                }
                println!();
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
            match run_cmd("systemctl", &["--user", "start", unit]) {
                Ok(()) => Ok(()),
                Err(_first_err) => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    run_cmd("systemctl", &["--user", "start", unit])
                }
            }
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
        Step::RestartService { unit } => run_cmd("systemctl", &["--user", "restart", unit]),
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
                println!("  {image} already available, skipping pull");
                return Ok(());
            }
            // Check additional image stores via podman images.
            // Match both the exact name and docker.io/ expanded forms since
            // quadlets use short names (e.g. "caddy:2-alpine") but podman
            // stores them with the full registry prefix.
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
                println!("  {image} available in image store, skipping pull");
                return Ok(());
            }
            println!("  Pulling {image}...");
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
    }
}

/// Run a command with explicit program and args (no shell interpretation).
fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let display = format!("{program} {}", args.join(" "));
    println!("  $ {display}");
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run: {display}"))?;
    if !status.success() {
        bail!("command failed: {display}");
    }
    Ok(())
}
