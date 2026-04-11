use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use ryra_core::Step;

/// Execute a list of steps, stopping on first failure.
pub async fn execute_all(steps: &[Step]) -> Result<()> {
    let verbose = ryra_core::verbose::is_enabled();
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
            if ryra_core::verbose::is_enabled() && !file.content.is_empty() {
                for line in file.content.lines() {
                    println!("    | {line}");
                }
                println!();
            }
            if let Some(parent) = file.path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create directory {}", parent.display()))?;
            }
            std::fs::write(&file.path, &file.content)
                .with_context(|| format!("failed to write {}", file.path.display()))?;
            // Preserve executable permission for script files
            #[cfg(unix)]
            if file.path.extension().map(|e| e == "sh").unwrap_or(false) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file.path, std::fs::Permissions::from_mode(0o755))
                    .with_context(|| {
                        format!("failed to set permissions on {}", file.path.display())
                    })?;
            }
            Ok(())
        }
        Step::DaemonReload => run("systemctl --user daemon-reload"),
        Step::StartService { unit } => run(&format!("systemctl --user start {unit}")),
        Step::StopService { unit } => {
            // Stop failures are non-fatal (service may already be stopped)
            if let Err(e) = run(&format!("systemctl --user stop {unit}"))
                && ryra_core::verbose::is_enabled()
            {
                eprintln!("  Note: stopping {unit} failed (may already be stopped): {e}");
            }
            Ok(())
        }
        Step::RestartService { unit } => run(&format!("systemctl --user restart {unit}")),
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
                    return run(
                        "podman exec caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile",
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            // Caddy not running — skip reload (will pick up config on next start)
            println!("    Caddy not running, skipping reload");
            Ok(())
        }
        Step::PullImage { image } => {
            // Skip if already available
            let check = format!("podman image exists {image}");
            if Command::new("sh")
                .args(["-c", &check])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                println!("  {image} already available, skipping pull");
                return Ok(());
            }
            println!("  Pulling {image}...");
            run(&format!("podman pull {image}"))
        }
        Step::RemoveFile(path) => std::fs::remove_file(path)
            .with_context(|| format!("failed to remove {}", path.display())),
        Step::RemoveDir(path) => std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display())),
        Step::RemoveVolume { name } => {
            // Volume removal is best-effort — the volume may not exist if the
            // container never started, or podman may need the container gone first.
            let status = Command::new("podman")
                .args(["volume", "rm", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if ryra_core::verbose::is_enabled() && !status.map(|s| s.success()).unwrap_or(false) {
                eprintln!("  Note: volume {name} not removed (may not exist)");
            }
            Ok(())
        }
        Step::CreateDir(path) => std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create directory {}", path.display())),
    }
}

fn run(cmd: &str) -> Result<()> {
    println!("  $ {cmd}");
    let status = Command::new("sh")
        .args(["-c", cmd])
        .status()
        .with_context(|| format!("failed to run: {cmd}"))?;
    if !status.success() {
        bail!("command failed: {cmd}");
    }
    Ok(())
}
