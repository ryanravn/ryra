use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ryra_vm::machine::{ExecOutput, Machine, scp_dir_from_vm};

/// Abstraction over where commands run — VM (SSH) or local host.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Run a command and return its output. Fails if exit code != 0.
    async fn exec(&self, cmd: &str) -> Result<ExecOutput>;

    /// Run a command, streaming stdout/stderr to the terminal.
    async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput>;

    /// Wait for a systemd user service to become active. `prefix` is the line
    /// prefix for the wait's progress heartbeat, so it aligns with surrounding
    /// test output (e.g. `"[my-test]     "`).
    async fn wait_for_service(&self, unit: &str, timeout: Duration, prefix: &str) -> Result<()>;

    /// Copy a directory from the execution environment to the host.
    /// No-op for local execution (files are already on the host).
    async fn fetch_dir(&self, remote_path: &str, local_path: &Path) -> Result<()>;

    /// Directory, *in this executor's environment*, where a browser step's
    /// Playwright HTML report should be written. The runner points Playwright
    /// here and then [`fetch_dir`]s it into the host's canonical reports dir.
    /// For local execution the two are the same path (so the fetch is a no-op);
    /// for a VM it's a VM-internal staging path that gets copied out.
    fn playwright_out_dir(&self, test_name: &str) -> String;
}

/// Path inside the VM where the test runner scps the registry fixtures.
/// Set as `RYRA_REGISTRY_DIR` so every `ryra` invocation resolves the
/// default registry to this local copy instead of cloning from GitHub.
pub const VM_REGISTRY_PATH: &str = "/opt/ryra-test-registry";

/// `podman inspect --format` template: prints `yes` when the container
/// declares a healthcheck, `no` otherwise. The wait loop uses this to decide
/// whether to actively probe health (`podman healthcheck run`, which forces an
/// immediate check rather than waiting for the scheduled interval) or fall
/// back to unit-active. Interpolated as a value, so its Go-template `{{…}}`
/// braces aren't reprocessed by Rust's `format!`.
const HEALTH_DEFINED_FMT: &str = "{{if .State.Health}}yes{{else}}no{{end}}";

/// Build the env-var prefix the executors prepend to every command so
/// `ryra` resolves the default registry against the test fixtures dir
/// instead of cloning from the internet on each test.
fn registry_env_prefix(path: &str) -> String {
    format!("export {}={}; ", ryra_core::REGISTRY_DIR_ENV, path)
}

/// Executes commands inside a QEMU VM via SSH.
pub struct VmExecutor<'a> {
    pub vm: &'a Machine,
    env_prefix: String,
}

impl<'a> VmExecutor<'a> {
    pub fn new(vm: &'a Machine) -> Self {
        Self {
            vm,
            env_prefix: registry_env_prefix(VM_REGISTRY_PATH),
        }
    }
}

#[async_trait]
impl Executor for VmExecutor<'_> {
    async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let wrapped = format!("{}{cmd}", self.env_prefix);
        self.vm.exec(&wrapped).await
    }

    async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput> {
        let wrapped = format!("{}{cmd}", self.env_prefix);
        self.vm.exec_streaming(&wrapped, prefix).await
    }

    async fn wait_for_service(&self, unit: &str, timeout: Duration, prefix: &str) -> Result<()> {
        self.vm.wait_for_service(unit, timeout, prefix).await
    }

    async fn fetch_dir(&self, remote_path: &str, local_path: &Path) -> Result<()> {
        // Ensure the local parent dir exists so scp writes into local_path/
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        scp_dir_from_vm(self.vm, remote_path, local_path).await
    }

    fn playwright_out_dir(&self, test_name: &str) -> String {
        // VM-internal staging path; the runner copies it to the host's
        // reports dir afterwards. The VM user is always `ryra`.
        format!("/home/ryra/.local/share/services-test/reports/{test_name}/playwright")
    }
}

/// Executes commands directly on the host machine.
///
/// Carries an optional `RYRA_REGISTRY_DIR` override prepended to every
/// command — bare-mode tests use this to point ryra at the in-repo
/// `registry/` checkout instead of cloning from GitHub on each run.
pub struct LocalExecutor {
    env_prefix: String,
}

impl LocalExecutor {
    /// Construct a LocalExecutor with no registry override. Used by commands
    /// that run `ryra` but never resolve the registry (e.g. `ryra remove`
    /// during `ryra test reset`); most callers want
    /// [`LocalExecutor::with_registry`].
    pub fn new() -> Self {
        let mut env_prefix = String::new();
        // Run the *same* ryra we're part of, not whatever (possibly stale)
        // `ryra` is on the user's PATH. `ryra test` is a subcommand of the ryra
        // binary, so the running executable IS the up-to-date ryra — put its
        // directory first on PATH so `ryra add/remove/...` in tests resolve to
        // it. Without this, `cargo run test` would drive an old installed ryra
        // that doesn't honour RYRA_DATA_DIR/RYRA_CONFIG_DIR and the sandbox
        // would silently diverge from where data actually lands.
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            env_prefix.push_str(&format!("export PATH=\"{}:$PATH\"; ", dir.display()));
        }
        Self { env_prefix }
    }

    /// Construct a LocalExecutor that prepends `RYRA_REGISTRY_DIR=<path>`
    /// to every command, so `ryra` resolves the default registry to the
    /// local checkout under test.
    pub fn with_registry(registry_path: &Path) -> Self {
        let mut s = Self::new();
        s.env_prefix
            .push_str(&registry_env_prefix(&registry_path.display().to_string()));
        s
    }

    /// Also export `RYRA_CONFIG_DIR=<path>` on every command, isolating
    /// `preferences.toml` into a throwaway dir so host tests never read or
    /// clobber the user's real SMTP/auth/backup credentials.
    pub fn with_config_dir(mut self, config_dir: &Path) -> Self {
        self.env_prefix.push_str(&format!(
            "export {}={}; ",
            ryra_core::CONFIG_DIR_ENV,
            config_dir.display()
        ));
        self
    }

    /// Also export `RYRA_DATA_DIR=<path>` on every command, so test service
    /// deployments (data, `.env`, configs, and the quadlet files ryra writes
    /// into `service_home`) land in the sandbox instead of the user's real
    /// `~/.local/share/services/`.
    pub fn with_data_dir(mut self, data_dir: &Path) -> Self {
        self.env_prefix.push_str(&format!(
            "export {}={}; ",
            ryra_core::DATA_DIR_ENV,
            data_dir.display()
        ));
        self
    }
}

impl Default for LocalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let wrapped = format!("{}{cmd}", self.env_prefix);
        let output = tokio::process::Command::new("bash")
            .args(["-c", &wrapped])
            .output()
            .await
            .with_context(|| format!("failed to exec locally: {cmd}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!(
                "command failed locally (exit {}): {cmd}\nstdout: {stdout}\nstderr: {stderr}",
                output.status,
            );
        }

        Ok(ExecOutput { stdout, stderr })
    }

    async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let wrapped = format!("{}{cmd}", self.env_prefix);
        let mut child = tokio::process::Command::new("bash")
            .args(["-c", &wrapped])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to exec locally: {cmd}"))?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let prefix_out = prefix.to_string();
        let prefix_err = prefix.to_string();

        let stdout_handle = tokio::spawn(async move {
            let mut lines = String::new();
            if let Some(pipe) = stdout_pipe {
                let mut reader = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if prefix_out.is_empty() {
                        println!("    {line}");
                    } else {
                        println!("[{prefix_out}]     {line}");
                    }
                    lines.push_str(&line);
                    lines.push('\n');
                }
            }
            lines
        });

        let stderr_handle = tokio::spawn(async move {
            let mut lines = String::new();
            if let Some(pipe) = stderr_pipe {
                let mut reader = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if prefix_err.is_empty() {
                        eprintln!("    {line}");
                    } else {
                        eprintln!("[{prefix_err}]     {line}");
                    }
                    lines.push_str(&line);
                    lines.push('\n');
                }
            }
            lines
        });

        let status = child.wait().await?;
        let stdout_buf = stdout_handle.await.context("stdout reader task panicked")?;
        let stderr_buf = stderr_handle.await.context("stderr reader task panicked")?;

        if !status.success() {
            anyhow::bail!(
                "command failed locally (exit {}): {cmd}\nstdout: {stdout_buf}\nstderr: {stderr_buf}",
                status,
            );
        }

        Ok(ExecOutput {
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }

    async fn wait_for_service(&self, unit: &str, timeout: Duration, prefix: &str) -> Result<()> {
        // The container is named after the unit (ContainerName=<svc>).
        let container = unit.trim_end_matches(".service");
        let mut progress =
            ryra_vm::progress::WaitProgress::new(unit, "systemctl + healthcheck", timeout)
                .with_prefix(prefix);
        loop {
            // One round-trip: unit active/failed + an *active* health probe.
            // When the container declares a healthcheck we run it immediately
            // (`podman healthcheck run`) instead of reading the passively-
            // scheduled status — otherwise we'd wait up to the health interval
            // (often 30s) for the first check. No healthcheck → "none".
            let cmd = format!(
                "a=$(systemctl --user is-active {unit} 2>/dev/null || true); \
                 f=$(systemctl --user is-failed {unit} 2>/dev/null || true); \
                 if [ \"$(podman inspect --format '{HEALTH_DEFINED_FMT}' {container} 2>/dev/null)\" = yes ]; then \
                   podman healthcheck run {container} >/dev/null 2>&1 && h=healthy || h=unhealthy; \
                 else h=none; fi; \
                 echo \"$a|$f|$h\""
            );
            let out = self.exec(&cmd).await?;
            let line = out.stdout.trim();
            let mut parts = line.split('|');
            let active = parts.next().unwrap_or("");
            let failed = parts.next().unwrap_or("");
            let health = parts.next().unwrap_or("");

            if failed == "failed" {
                anyhow::bail!("service {unit} entered failed state");
            }
            if active == "active" {
                // Health-aware readiness: when the container declares a
                // HealthCmd, wait for `healthy` rather than just "unit
                // started" — that's what makes the next step reliable on slow
                // machines and catches services that start then die (the unit
                // flips active for a beat before a fatal startup check). No
                // healthcheck → unit-active is the best signal we have.
                match health {
                    "healthy" | "none" | "" => return Ok(()),
                    // "starting" / "unhealthy" — keep polling until timeout.
                    _ => {}
                }
            }
            if progress.timed_out() {
                anyhow::bail!(
                    "service {unit} not ready after {}s (active={active}, health={health})",
                    timeout.as_secs()
                );
            }
            progress.tick();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn fetch_dir(&self, _remote_path: &str, _local_path: &Path) -> Result<()> {
        // No-op: in local mode the "remote" path is already on the host.
        Ok(())
    }

    fn playwright_out_dir(&self, test_name: &str) -> String {
        // Local mode: write straight to the host's canonical reports dir, so
        // the subsequent no-op fetch finds it already in place.
        crate::reports::reports_dir()
            .map(|d| d.join(test_name).join("playwright").display().to_string())
            .unwrap_or_else(|_| format!("/tmp/ryra-test-reports/{test_name}/playwright"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_executor_runs_command() {
        let exec = LocalExecutor::default();
        let out = exec.exec("echo hello").await.unwrap();
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn local_executor_fails_on_bad_command() {
        let exec = LocalExecutor::default();
        let result = exec.exec("false").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn local_executor_streams_output() {
        let exec = LocalExecutor::default();
        let out = exec.exec_streaming("echo streamed", "test").await.unwrap();
        assert_eq!(out.stdout.trim(), "streamed");
    }

    #[tokio::test]
    async fn local_wait_for_nonexistent_service_times_out() {
        let exec = LocalExecutor::default();
        let result = exec
            .wait_for_service(
                "definitely-not-a-real-unit-xyz123.service",
                Duration::from_secs(2),
                "  ",
            )
            .await;
        assert!(result.is_err());
        // Should time out or report non-active, not hang
    }
}
