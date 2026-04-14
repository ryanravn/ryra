use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use ryra_vm::machine::{ExecOutput, Machine};

/// Abstraction over where commands run — VM (SSH) or local host.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Run a command and return its output. Fails if exit code != 0.
    async fn exec(&self, cmd: &str) -> Result<ExecOutput>;

    /// Run a command, streaming stdout/stderr to the terminal.
    async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput>;

    /// Wait for a systemd user service to become active.
    async fn wait_for_service(&self, unit: &str, timeout: Duration) -> Result<()>;
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
}
