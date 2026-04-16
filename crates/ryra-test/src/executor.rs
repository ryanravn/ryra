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

    /// Wait for a systemd user service to become active.
    async fn wait_for_service(&self, unit: &str, timeout: Duration) -> Result<()>;

    /// Copy a directory from the execution environment to the host.
    /// No-op for local execution (files are already on the host).
    async fn fetch_dir(&self, remote_path: &str, local_path: &Path) -> Result<()>;
}

/// Executes commands inside a QEMU VM via SSH.
pub struct VmExecutor<'a> {
    pub vm: &'a Machine,
}

impl<'a> VmExecutor<'a> {
    pub fn new(vm: &'a Machine) -> Self {
        Self { vm }
    }
}

#[async_trait]
impl Executor for VmExecutor<'_> {
    async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        self.vm.exec(cmd).await
    }

    async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput> {
        self.vm.exec_streaming(cmd, prefix).await
    }

    async fn wait_for_service(&self, unit: &str, timeout: Duration) -> Result<()> {
        self.vm.wait_for_service(unit, timeout).await
    }

    async fn fetch_dir(&self, remote_path: &str, local_path: &Path) -> Result<()> {
        // Ensure the local parent dir exists so scp writes into local_path/
        if let Some(parent) = local_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        scp_dir_from_vm(self.vm, remote_path, local_path).await
    }
}

/// Executes commands directly on the host machine.
pub struct LocalExecutor;

#[async_trait]
impl Executor for LocalExecutor {
    async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let output = tokio::process::Command::new("bash")
            .args(["-c", cmd])
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

        let mut child = tokio::process::Command::new("bash")
            .args(["-c", cmd])
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

    async fn wait_for_service(&self, unit: &str, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            let cmd = format!(
                "a=$(systemctl --user is-active {unit} 2>/dev/null || true); \
                 f=$(systemctl --user is-failed {unit} 2>/dev/null || true); \
                 echo \"$a|$f\""
            );
            let out = self.exec(&cmd).await?;
            let line = out.stdout.trim();
            let mut parts = line.split('|');
            let active = parts.next().unwrap_or("");
            let failed = parts.next().unwrap_or("");

            if active == "active" {
                return Ok(());
            }
            if failed == "failed" {
                anyhow::bail!("service {unit} entered failed state");
            }
            if start.elapsed() > timeout {
                anyhow::bail!("service {unit} not active after {}s", timeout.as_secs());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn fetch_dir(&self, _remote_path: &str, _local_path: &Path) -> Result<()> {
        // No-op: in local mode the "remote" path is already on the host.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_executor_runs_command() {
        let exec = LocalExecutor;
        let out = exec.exec("echo hello").await.unwrap();
        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn local_executor_fails_on_bad_command() {
        let exec = LocalExecutor;
        let result = exec.exec("false").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn local_executor_streams_output() {
        let exec = LocalExecutor;
        let out = exec.exec_streaming("echo streamed", "test").await.unwrap();
        assert_eq!(out.stdout.trim(), "streamed");
    }

    #[tokio::test]
    async fn local_wait_for_nonexistent_service_times_out() {
        let exec = LocalExecutor;
        let result = exec
            .wait_for_service(
                "definitely-not-a-real-unit-xyz123.service",
                Duration::from_secs(2),
            )
            .await;
        assert!(result.is_err());
        // Should time out or report non-active, not hang
    }
}
