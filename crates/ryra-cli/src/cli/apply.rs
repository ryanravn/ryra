use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use ryra_core::Step;

/// A concrete record of something we created in this run.
enum Created {
    File(PathBuf),
    StartedService(String),
}

/// Returns true if a path is under /etc or /var (i.e. requires sudo).
fn is_system_path(path: &Path) -> bool {
    path.starts_with("/etc") || path.starts_with("/var")
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
                if is_system_path(path) {
                    run_quiet(&format!("sudo rm -f {}", path.display()))
                } else {
                    std::fs::remove_file(path)
                        .with_context(|| format!("failed to remove {}", path.display()))
                }
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
            if is_system_path(&file.path) {
                if let Some(parent) = file.path.parent() {
                    run(&format!("sudo mkdir -p {}", parent.display()))?;
                }
                let mut child = Command::new("sudo")
                    .args(["tee", &file.path.to_string_lossy()])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .context("failed to run sudo tee")?;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(file.content.as_bytes())?;
                }
                let status = child.wait()?;
                if !status.success() {
                    bail!("failed to write {}", file.path.display());
                }
            } else {
                if let Some(parent) = file.path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create directory {}", parent.display())
                    })?;
                }
                std::fs::write(&file.path, &file.content)
                    .with_context(|| format!("failed to write {}", file.path.display()))?;
            }
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
        Step::SystemDaemonReload => run("sudo systemctl daemon-reload"),
        Step::SystemStart { unit } => {
            run(&format!("sudo systemctl start {unit}"))?;
            Ok(())
        }
        Step::SystemStop { unit } => {
            let _ = run(&format!("sudo systemctl stop {unit}"));
            Ok(())
        }
        Step::SystemRestart { unit } => {
            let _ = run_quiet(&format!("sudo systemctl reset-failed {unit}"));
            run(&format!("sudo systemctl restart {unit}"))
        }
        Step::ReloadCaddy => {
            println!("  Reloading Caddy config...");
            run(
                "sudo podman exec systemd-caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile",
            )
        }
        Step::PullImage { image, system } => {
            let prefix = if *system { "sudo " } else { "" };
            // Skip if already available
            let check = format!("{prefix}podman image exists {image}");
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
            run(&format!("{prefix}podman pull {image}"))
        }
        Step::RemoveFile(path) => {
            if is_system_path(path) {
                run(&format!("sudo rm -f {}", path.display()))
            } else {
                std::fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))
            }
        }
        Step::RemoveDir(path) => {
            if is_system_path(path) {
                run(&format!("sudo rm -rf {}", path.display()))
            } else {
                std::fs::remove_dir_all(path)
                    .with_context(|| format!("failed to remove directory {}", path.display()))
            }
        }
        Step::CreateDir(path) => std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create directory {}", path.display())),
        Step::RegisterAuthProvider {
            service_name,
            api_url,
            api_token,
            client_id,
            client_secret,
            redirect_uri,
            launch_url,
        } => {
            println!("  Registering OAuth provider in authentik for {service_name}...");
            register_auth_provider(
                api_url,
                api_token,
                service_name,
                client_id,
                client_secret,
                redirect_uri,
                launch_url,
            )
            .await
        }
        Step::RemoveAuthProvider {
            service_name,
            api_url,
            api_token,
        } => {
            // Best-effort: authentik might already be stopped during reset
            let _ = remove_auth_provider(api_url, api_token, service_name).await;
            Ok(())
        }
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

/// Register an OAuth2 provider + application in authentik via its REST API.
async fn register_auth_provider(
    api_url: &str,
    api_token: &str,
    service_name: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    launch_url: &str,
) -> Result<()> {
    let api = format!("{api_url}/api/v3");
    let auth = format!("Bearer {api_token}");

    // Wait for authentik API to be ready (it takes a while after container start)
    let max_wait = 600;
    println!("    waiting for authentik API (up to {max_wait}s)...");
    for i in 1..=(max_wait / 10) {
        let probe = Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "-H",
                &format!("Authorization: {auth}"),
                &format!("{api}/core/users/me/"),
            ])
            .output();
        if let Ok(out) = probe {
            let code = String::from_utf8_lossy(&out.stdout);
            if code.trim() == "200" {
                println!("    authentik ready ({}s)", i * 10);
                break;
            }
        }
        if i == max_wait / 10 {
            bail!(
                "authentik API not ready after {max_wait}s — check: sudo journalctl _SYSTEMD_USER_UNIT=authentik.service"
            );
        }
        println!("    not yet — retrying in 10s ({}s elapsed)", i * 10);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }

    // Helper: GET a flow PK by slug
    let flow_pk = |slug: &str| -> Result<String> {
        let output = Command::new("curl")
            .args([
                "-sS",
                "-H",
                &format!("Authorization: {auth}"),
                &format!("{api}/flows/instances/?slug={slug}"),
            ])
            .output()
            .context("curl failed")?;
        let body: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("failed to parse flows response")?;
        body["results"][0]["pk"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("flow '{slug}' not found in authentik"))
    };

    let authz_flow = flow_pk("default-provider-authorization-implicit-consent")?;
    let authn_flow = flow_pk("default-authentication-flow")?;
    let inval_flow = flow_pk("default-provider-invalidation-flow")?;

    // Get scope mapping PKs
    let scope_output = Command::new("curl")
        .args([
            "-sS",
            "-H",
            &format!("Authorization: {auth}"),
            &format!("{api}/propertymappings/provider/scope/"),
        ])
        .output()
        .context("failed to fetch scope mappings")?;
    let scope_body: serde_json::Value =
        serde_json::from_slice(&scope_output.stdout).context("failed to parse scope mappings")?;
    let scope_pks: Vec<String> = scope_body["results"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|r| {
            let managed = r["managed"].as_str().unwrap_or("");
            managed.ends_with("/scope-openid")
                || managed.ends_with("/scope-email")
                || managed.ends_with("/scope-profile")
        })
        .filter_map(|r| r["pk"].as_str().map(|s| format!("\"{s}\"")))
        .collect();

    // Create OAuth2 provider
    let provider_json = format!(
        r#"{{"name":"{service_name}","authorization_flow":"{authz_flow}","authentication_flow":"{authn_flow}","invalidation_flow":"{inval_flow}","client_type":"confidential","client_id":"{client_id}","client_secret":"{client_secret}","redirect_uris":[{{"matching_mode":"regex","url":"{redirect_uri}"}}],"property_mappings":[{scopes}]}}"#,
        scopes = scope_pks.join(","),
    );
    let provider_output = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            &format!("Authorization: {auth}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &provider_json,
            &format!("{api}/providers/oauth2/"),
        ])
        .output()
        .context("failed to create OAuth2 provider")?;
    if !provider_output.status.success() {
        let err = String::from_utf8_lossy(&provider_output.stderr);
        bail!("failed to create OAuth2 provider in authentik: {err}");
    }
    let provider_body: serde_json::Value = serde_json::from_slice(&provider_output.stdout)
        .context("failed to parse provider response")?;
    let provider_pk = provider_body["pk"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("no pk in provider response: {provider_body}"))?;

    // Create application
    let app_json = format!(
        r#"{{"name":"{service_name}","slug":"{service_name}","provider":{provider_pk},"meta_launch_url":"{launch_url}"}}"#,
    );
    let app_output = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            &format!("Authorization: {auth}"),
            "-H",
            "Content-Type: application/json",
            "-d",
            &app_json,
            &format!("{api}/core/applications/"),
        ])
        .output()
        .context("failed to create application")?;
    if !app_output.status.success() {
        let err = String::from_utf8_lossy(&app_output.stderr);
        bail!("failed to create application in authentik: {err}");
    }

    Ok(())
}

/// Remove an OAuth2 application + provider from authentik via API.
async fn remove_auth_provider(api_url: &str, api_token: &str, service_name: &str) -> Result<()> {
    let api = format!("{api_url}/api/v3");
    let auth = format!("Bearer {api_token}");

    // Delete application (by slug)
    let _ = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "DELETE",
            "-H",
            &format!("Authorization: {auth}"),
            &format!("{api}/core/applications/{service_name}/"),
        ])
        .output();

    // Find and delete provider (by name)
    let output = Command::new("curl")
        .args([
            "-sS",
            "-H",
            &format!("Authorization: {auth}"),
            &format!("{api}/providers/oauth2/?name={service_name}"),
        ])
        .output()
        .ok();
    if let Some(output) = output
        && let Ok(body) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(pk) = body["results"][0]["pk"].as_i64()
    {
        let _ = Command::new("curl")
            .args([
                "-sS",
                "-X",
                "DELETE",
                "-H",
                &format!("Authorization: {auth}"),
                &format!("{api}/providers/oauth2/{pk}/"),
            ])
            .output();
    }

    Ok(())
}
