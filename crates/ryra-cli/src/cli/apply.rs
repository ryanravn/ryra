use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use ryra_core::Step;

/// A concrete record of something we created in this run.
enum Created {
    File(PathBuf),
    StartedService(String),
}

/// Execute a list of steps with automatic rollback on failure.
pub async fn execute_all(steps: &[Step]) -> Result<()> {
    let mut created: Vec<Created> = Vec::new();

    let verbose = ryra_core::verbose::is_enabled();
    for step in steps {
        let start = std::time::Instant::now();
        match execute(step, &mut created).await {
            Ok(()) => {
                if verbose {
                    let elapsed = start.elapsed();
                    if elapsed.as_millis() > 500 {
                        println!("    ({:.1}s)", elapsed.as_secs_f64());
                    }
                }
            }
            Err(e) => {
                eprintln!("\nStep failed: {e}");
                prompt_rollback(&created).await;
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Prompt the user before rolling back. In non-interactive mode, skip rollback.
async fn prompt_rollback(created: &[Created]) {
    if created.is_empty() {
        return;
    }

    eprintln!("\n{} changes were made before the failure.", created.len());
    eprintln!("Rollback will attempt to undo them, but may not be perfect.\n");

    let should_rollback = if std::io::stdin().is_terminal() {
        dialoguer::Confirm::new()
            .with_prompt("Roll back changes?")
            .default(false)
            .interact()
            .unwrap_or(false)
    } else {
        eprintln!("Non-interactive mode — skipping rollback. Manual cleanup may be needed.");
        false
    };

    if !should_rollback {
        eprintln!("Skipping rollback. You may need to clean up manually.");
        return;
    }

    eprintln!("Rolling back...");
    for item in created.iter().rev() {
        let result = match item {
            Created::StartedService(unit) => {
                eprintln!("  Stopping {unit}...");
                run_quiet(&format!("systemctl --user stop {unit}"))
            }
            Created::File(path) => {
                eprintln!("  Removing {}", path.display());
                std::fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))
            }
        };
        if let Err(e) = result {
            eprintln!("  Rollback warning: {e}");
        }
    }
    eprintln!("Rollback complete.");
}

/// Execute a single step, recording what was created for rollback.
async fn execute(step: &Step, created: &mut Vec<Created>) -> Result<()> {
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
            created.push(Created::File(file.path.clone()));
            Ok(())
        }
        Step::DaemonReload => run("systemctl --user daemon-reload"),
        Step::StartService { unit } => {
            run(&format!("systemctl --user start --no-block {unit}"))?;
            created.push(Created::StartedService(unit.clone()));
            Ok(())
        }
        Step::StopService { unit } => {
            let _ = run(&format!("systemctl --user stop {unit}"));
            Ok(())
        }
        Step::RestartService { unit } => run(&format!("systemctl --user restart {unit}")),
        Step::ReloadCaddy => {
            println!("  Reloading Caddy config...");
            // Wait for Caddy container to be running before reload
            for _ in 0..10 {
                if Command::new("podman")
                    .args(["exec", "systemd-caddy", "true"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    return run(
                        "podman exec systemd-caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile",
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
        Step::CreateDir(path) => std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create directory {}", path.display())),
        Step::PostStartHook {
            name,
            service_name,
            run: hook_cmd,
            timeout,
        } => {
            println!("  Running post-start hook: {name}...");
            let home = ryra_core::service_home(service_name);
            let full_cmd = format!(
                "sh -c {cmd}",
                cmd = shell_escape(&format!(
                    "set -a && . {home}/.env && set +a && timeout {timeout} sh -c {inner}",
                    home = home.display(),
                    inner = shell_escape(hook_cmd),
                )),
            );
            run(&full_cmd)
        }
    }
}

/// Escape a string for use as a single sh -c argument.
fn shell_escape(s: &str) -> String {
    // Use single quotes, escaping any embedded single quotes
    let escaped = s.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
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

fn run_quiet(cmd: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run: {cmd}"))?;
    if !status.success() {
        bail!("command failed: {cmd}");
    }
    Ok(())
}
