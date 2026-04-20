use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::image::Image;

/// Global registry of active VM PIDs and work dirs, for signal cleanup.
static ACTIVE_VMS: Mutex<Vec<ActiveVm>> = Mutex::new(Vec::new());

struct ActiveVm {
    pid: u32,
    work_dir: PathBuf,
}

/// Kill all active VMs and clean up their work dirs.
/// Called from the Ctrl-C handler to ensure no orphaned processes.
pub fn cleanup_all_vms() {
    let vms = {
        let mut guard = ACTIVE_VMS.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    };
    for vm in &vms {
        // Send SIGKILL to the QEMU process
        unsafe {
            libc::kill(vm.pid as i32, libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&vm.work_dir);
    }
    if !vms.is_empty() {
        eprintln!("\nCleaned up {} VM(s)", vms.len());
    }
}

fn register_vm(pid: u32, work_dir: &Path) {
    let mut guard = ACTIVE_VMS.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(ActiveVm {
        pid,
        work_dir: work_dir.to_path_buf(),
    });
}

fn deregister_vm(pid: u32) {
    let mut guard = ACTIVE_VMS.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|vm| vm.pid != pid);
}

/// A running QEMU VM for E2E testing.
pub struct Machine {
    pub name: String,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub work_dir: PathBuf,
    process: tokio::process::Child,
}

impl Drop for Machine {
    /// Kill the QEMU process if it's still alive. Needed because tokio's
    /// `Child::drop` does NOT reap the process by default, so any early
    /// return from `spawn_*` (e.g. SSH timeout during wait_for_ssh) would
    /// otherwise orphan a QEMU holding the forwarded SSH port. Async destroy
    /// paths that want to survive this (keep_alive) use `std::mem::forget`
    /// to skip Drop entirely.
    fn drop(&mut self) {
        if let Some(pid) = self.process.id() {
            // SIGKILL is sync and cheap. `Child::start_kill` would work too but
            // we're in Drop so we can't .await anything.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            deregister_vm(pid);
        }
    }
}

/// Options that affect how the VM is launched.
pub struct SpawnOpts {
    pub use_kvm: bool,
    pub memory_mb: u32,
    pub cpus: u32,
    /// Virtual disk size in GB. The disk is copy-on-write so actual host
    /// usage is only the delta from the base image.
    pub disk_gb: u32,
}

impl SpawnOpts {
    /// SSH + cloud-init timeout. Without KVM, everything is ~10x slower.
    pub fn boot_timeout(&self) -> std::time::Duration {
        if self.use_kvm {
            std::time::Duration::from_secs(300)
        } else {
            std::time::Duration::from_secs(900) // 15 minutes for TCG
        }
    }
}

impl Default for SpawnOpts {
    fn default() -> Self {
        Self {
            use_kvm: true,
            memory_mb: 2048,
            cpus: 2,
            disk_gb: 20,
        }
    }
}

pub fn random_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    format!("{:06x}", rng.random::<u32>() & 0xffffff)
}

impl Machine {
    pub async fn spawn(
        image: &Image,
        test_id: &str,
        ssh_port: u16,
        opts: &SpawnOpts,
    ) -> Result<Self> {
        if let Some(ref snapshot) = image.snapshot {
            return Self::spawn_from_snapshot(image, snapshot, test_id, ssh_port, opts).await;
        }
        Self::spawn_cold(image, test_id, ssh_port, opts).await
    }

    /// Instant boot: restore from a saved QEMU snapshot (<1s to SSH).
    async fn spawn_from_snapshot(
        image: &Image,
        snapshot: &crate::image::SnapshotFiles,
        test_id: &str,
        ssh_port: u16,
        opts: &SpawnOpts,
    ) -> Result<Self> {
        let name = format!("ryra-test-{test_id}");
        let work_dir = vm_work_base_dir()?.join(&name);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .context("failed to create VM work directory")?;

        // Reflink-copy snapshot files for per-VM isolation (instant on btrfs)
        let disk = work_dir.join("disk.qcow2");
        let efi_vars = work_dir.join("efivars.qcow2");
        let seed_iso = work_dir.join("seed.iso");

        run_cmd(
            "cp",
            &[
                "--reflink=auto",
                &snapshot.disk.to_string_lossy(),
                &disk.to_string_lossy(),
            ],
        )
        .await
        .context("failed to copy snapshot disk")?;
        run_cmd(
            "cp",
            &[
                "--reflink=auto",
                &snapshot.efivars.to_string_lossy(),
                &efi_vars.to_string_lossy(),
            ],
        )
        .await
        .context("failed to copy snapshot efivars")?;
        run_cmd(
            "cp",
            &[
                "--reflink=auto",
                &snapshot.seed_iso.to_string_lossy(),
                &seed_iso.to_string_lossy(),
            ],
        )
        .await
        .context("failed to copy snapshot seed ISO")?;

        // Copy shared SSH key to work dir (ssh_key_path() expects it there)
        let key_path = work_dir.join("id_ed25519");
        tokio::fs::copy(&snapshot.ssh_key, &key_path)
            .await
            .context("failed to copy SSH key")?;

        // Build QEMU args — memory must match the snapshot's size exactly
        let memory = snapshot.memory_mb.to_string();
        let cpus = opts.cpus.to_string();
        let efi_code_arg = format!(
            "if=pflash,format=raw,file={},readonly=on",
            image.efi_code.display()
        );
        let efi_vars_arg = format!("if=pflash,format=qcow2,file={}", efi_vars.display());
        let disk_arg = format!("if=virtio,file={},format=qcow2", disk.display());
        let seed_arg = format!(
            "if=virtio,file={},format=raw,readonly=on",
            seed_iso.display()
        );
        let nic_arg = format!("user,hostfwd=tcp::{ssh_port}-:22");
        let serial_log = work_dir.join("serial.log");
        let serial_arg = format!("file:{}", serial_log.display());
        let shared_store = image_shared_store_dir()?;
        tokio::fs::create_dir_all(&shared_store).await.ok();
        let virtfs_arg = format!(
            "local,path={},mount_tag=images,security_model=none,readonly=on",
            shared_store.display()
        );

        let mut args: Vec<&str> = vec![
            "-machine",
            "virt",
            "-cpu",
            if opts.use_kvm { "host" } else { "max" },
            "-m",
            &memory,
            "-smp",
            &cpus,
            "-drive",
            &efi_code_arg,
            "-drive",
            &efi_vars_arg,
            "-drive",
            &disk_arg,
            "-drive",
            &seed_arg,
            "-nic",
            &nic_arg,
            "-nographic",
            "-serial",
            &serial_arg,
            "-monitor",
            "none",
            "-virtfs",
            &virtfs_arg,
            "-loadvm",
            "ready",
        ];

        if opts.use_kvm {
            args.extend(crate::accel_args());
        }

        let process = Command::new("qemu-system-aarch64")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start QEMU — is qemu-system-aarch64 installed?")?;

        let machine = Machine {
            name,
            ssh_host: "127.0.0.1".to_string(),
            ssh_port,
            work_dir,
            process,
        };

        if let Some(pid) = machine.process.id() {
            register_vm(pid, &machine.work_dir);
        }

        // Wait for SSH — snapshot restore is sub-second once pages are warm,
        // but the very first restore in a session can take >30s on a cold page
        // cache (hundreds of MB of snapshot state faulted in from disk).
        machine
            .wait_for_ssh(std::time::Duration::from_secs(60))
            .await?;

        // Fix clock skew: the snapshot's system clock is frozen at save time,
        // which can be hours or days behind wall clock. Services that enforce
        // fresh time (authelia's NTP startup check, TLS cert validity, OIDC
        // token expiry, …) fail hard when the guest clock is off, so we
        // force-set it before any test starts.
        //
        // Expand the timestamp on the HOST side and send it as a literal.
        // The obvious-looking `date -s "$(date -u -R)"` evaluates the inner
        // `$()` inside the guest, which just reads the frozen clock and
        // rewrites it to itself — effectively a no-op.
        let host_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if host_epoch > 0
            && let Err(e) = machine
                .exec(&format!(
                    "sudo date -s @{host_epoch} >/dev/null 2>&1 && sudo hwclock --systohc >/dev/null 2>&1 || true"
                ))
                .await
        {
            eprintln!("  warning: failed to sync clock in snapshot-booted VM: {e:#}");
        }

        Ok(machine)
    }

    /// Cold boot: traditional boot with cloud-init (fallback when no snapshot).
    async fn spawn_cold(
        image: &Image,
        test_id: &str,
        ssh_port: u16,
        opts: &SpawnOpts,
    ) -> Result<Self> {
        let name = format!("ryra-test-{test_id}");
        let work_dir = vm_work_base_dir()?.join(&name);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .context("failed to create VM work directory")?;

        // Create a copy-on-write disk backed by the base image
        let disk = work_dir.join("disk.qcow2");
        let disk_size = format!("{}G", opts.disk_gb);
        run_cmd(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                "-b",
                &image.path.to_string_lossy(),
                "-F",
                "qcow2",
                &disk.to_string_lossy(),
                &disk_size,
            ],
        )
        .await
        .context("qemu-img create failed")?;

        // Copy EFI vars template (writable per-VM)
        let efi_vars = work_dir.join("efivars.fd");
        tokio::fs::copy(&image.efi_vars_template, &efi_vars)
            .await
            .context("failed to copy EFI vars template")?;

        // Generate SSH key pair
        let key_path = work_dir.join("id_ed25519");
        let _ = tokio::fs::remove_file(&key_path).await;
        run_cmd(
            "ssh-keygen",
            &[
                "-t",
                "ed25519",
                "-f",
                &key_path.to_string_lossy(),
                "-N",
                "",
                "-q",
            ],
        )
        .await
        .context("ssh-keygen failed")?;

        let pub_key = tokio::fs::read_to_string(format!("{}.pub", key_path.display()))
            .await
            .context("failed to read SSH public key")?;

        // Build cloud-init seed ISO
        let seed_iso = work_dir.join("seed.iso");
        build_seed_iso(&work_dir, &seed_iso, &name, pub_key.trim()).await?;

        // Build QEMU args
        let memory = opts.memory_mb.to_string();
        let cpus = opts.cpus.to_string();
        let efi_code_arg = format!(
            "if=pflash,format=raw,file={},readonly=on",
            image.efi_code.display()
        );
        let efi_vars_arg = format!("if=pflash,format=raw,file={}", efi_vars.display());
        let disk_arg = format!("if=virtio,file={},format=qcow2", disk.display());
        let seed_arg = format!("if=virtio,file={},format=raw", seed_iso.display());
        let nic_arg = format!("user,hostfwd=tcp::{ssh_port}-:22");
        let serial_log = work_dir.join("serial.log");
        let serial_arg = format!("file:{}", serial_log.display());

        let shared_store = image_shared_store_dir()?;
        tokio::fs::create_dir_all(&shared_store).await.ok();
        let virtfs_arg = format!(
            "local,path={},mount_tag=images,security_model=none,readonly=on",
            shared_store.display()
        );

        let mut args: Vec<&str> = vec![
            "-machine",
            "virt",
            "-cpu",
            if opts.use_kvm { "host" } else { "max" },
            "-m",
            &memory,
            "-smp",
            &cpus,
            "-drive",
            &efi_code_arg,
            "-drive",
            &efi_vars_arg,
            "-drive",
            &disk_arg,
            "-drive",
            &seed_arg,
            "-nic",
            &nic_arg,
            "-nographic",
            "-serial",
            &serial_arg,
            "-monitor",
            "none",
            "-virtfs",
            &virtfs_arg,
        ];

        if opts.use_kvm {
            args.extend(crate::accel_args());
        }

        let process = Command::new("qemu-system-aarch64")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start QEMU — is qemu-system-aarch64 installed?")?;

        let machine = Machine {
            name,
            ssh_host: "127.0.0.1".to_string(),
            ssh_port,
            work_dir,
            process,
        };

        if let Some(pid) = machine.process.id() {
            register_vm(pid, &machine.work_dir);
        }

        let boot_timeout = opts.boot_timeout();
        machine.wait_for_ssh(boot_timeout).await?;

        machine
            .exec("cloud-init status --wait")
            .await
            .context("cloud-init did not complete")?;

        Ok(machine)
    }

    /// Build common SSH arguments for connecting to this VM.
    fn ssh_args(&self) -> Vec<String> {
        let key = self.ssh_key_path();
        vec![
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-i".into(),
            key.to_string_lossy().into_owned(),
            "-p".into(),
            self.ssh_port.to_string(),
            format!("ryra@{}", self.ssh_host),
        ]
    }

    /// Run a command inside the VM via SSH.
    pub async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let mut args = self.ssh_args();
        args.push(cmd.to_string());
        let output = Command::new("ssh")
            .args(&args)
            .output()
            .await
            .with_context(|| format!("failed to SSH exec in {}: {cmd}", self.name))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            anyhow::bail!(
                "command failed in VM {} (exit {}): {cmd}\nstdout: {stdout}\nstderr: {stderr}",
                self.name,
                output.status,
            );
        }

        Ok(ExecOutput { stdout, stderr })
    }

    /// Run a command inside the VM via SSH, streaming stdout/stderr to the terminal.
    /// Returns the exit status (Ok if success, Err if non-zero).
    pub async fn exec_streaming(&self, cmd: &str, prefix: &str) -> Result<ExecOutput> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::process::Command as TokioCommand;

        let mut args = self.ssh_args();
        args.push(cmd.to_string());
        let mut child = TokioCommand::new("ssh")
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to SSH exec in {}: {cmd}", self.name))?;

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
        let stdout_buf = stdout_handle.await.unwrap_or_default();
        let stderr_buf = stderr_handle.await.unwrap_or_default();

        if !status.success() {
            anyhow::bail!(
                "command failed in VM {} (exit {}): {cmd}\nstdout: {stdout_buf}\nstderr: {stderr_buf}",
                self.name,
                status,
            );
        }

        Ok(ExecOutput {
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }

    /// Wait for the qemu process to exit on its own (e.g., after a clean
    /// `sudo poweroff`). Returns once the process is gone or `timeout` elapses.
    /// Used before snapshotting so the disk is fully released.
    pub async fn wait_for_exit(&mut self, timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if matches!(self.process.try_wait(), Ok(Some(_))) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Shut down the VM and clean up files.
    pub async fn destroy(mut self) -> Result<()> {
        if let Some(pid) = self.process.id() {
            deregister_vm(pid);
        }
        let _ = self.exec("sudo poweroff").await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let _ = self.process.kill().await;
        let _ = self.process.wait().await;
        let _ = tokio::fs::remove_dir_all(&self.work_dir).await;
        Ok(())
    }

    /// Print SSH connection info for debugging, then detach.
    /// VM keeps running until the user kills it or the process exits.
    pub fn keep_alive(self) {
        if let Some(pid) = self.process.id() {
            deregister_vm(pid);
        }
        println!(
            "  VM still running. Connect with:\n    \
             ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
             -i {}/id_ed25519 -p {} ryra@{}\n  \
             Serial log: {}/serial.log\n  \
             Kill with: kill {}",
            self.work_dir.display(),
            self.ssh_port,
            self.ssh_host,
            self.work_dir.display(),
            self.process
                .id()
                .map(|id| id.to_string())
                .unwrap_or_else(|| "?".to_string()),
        );
        // Intentionally leak — VM process stays alive, cleaned up when parent exits
        std::mem::forget(self);
    }

    fn ssh_key_path(&self) -> PathBuf {
        self.work_dir.join("id_ed25519")
    }

    async fn wait_for_ssh(&self, timeout: std::time::Duration) -> Result<()> {
        let start = std::time::Instant::now();
        let mut ssh_args = self.ssh_args();
        // Override ConnectTimeout to 3s for probing
        if let Some(pos) = ssh_args.iter().position(|a| a == "ConnectTimeout=10") {
            ssh_args[pos] = "ConnectTimeout=3".into();
        }
        ssh_args.push("true".into());
        let mut last_log = std::time::Instant::now();

        loop {
            // Log progress every 30 seconds
            if last_log.elapsed().as_secs() >= 30 {
                println!(
                    "  [{}] still waiting for SSH... ({:.0}s elapsed)",
                    self.name,
                    start.elapsed().as_secs_f64()
                );
                last_log = std::time::Instant::now();
            }

            // Try a real SSH command (not just TCP connect)
            let result = Command::new("ssh")
                .args(&ssh_args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;

            if let Ok(status) = result
                && status.success()
            {
                return Ok(());
            }

            if start.elapsed() > timeout {
                anyhow::bail!(
                    "timed out waiting for SSH on {}:{} after {}s\n  \
                     Check serial log: {}/serial.log",
                    self.ssh_host,
                    self.ssh_port,
                    timeout.as_secs(),
                    self.work_dir.display(),
                );
            }

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
}

#[allow(dead_code)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// Run a command and bail if it fails.
async fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        anyhow::bail!("{program} failed with exit status {status}");
    }
    Ok(())
}

/// Read the host's /etc/subuid entry for the invoking user so the VM can
/// mirror it. Matching subuid ranges ensures image store files (owned by host
/// UIDs in the subuid range) map to the correct container UIDs inside the VM.
fn host_subid_mapping() -> Result<(u32, u32)> {
    let user = std::env::var("USER").context("USER env var not set")?;
    let subuid = std::fs::read_to_string("/etc/subuid").context("failed to read /etc/subuid")?;
    for line in subuid.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() == 3 && parts[0] == user {
            let start: u32 = parts[1]
                .parse()
                .with_context(|| format!("invalid subuid start for {user}: {}", parts[1]))?;
            let size: u32 = parts[2]
                .parse()
                .with_context(|| format!("invalid subuid size for {user}: {}", parts[2]))?;
            return Ok((start, size));
        }
    }
    anyhow::bail!("no subuid entry found for user {user} in /etc/subuid")
}

/// Build a minimal cloud-init seed ISO — just SSH key, no package installs.
/// Used for test VMs backed by a prepared image that already has packages.
pub(crate) async fn build_seed_iso(
    work_dir: &Path,
    output: &Path,
    hostname: &str,
    pub_key: &str,
) -> Result<()> {
    // VMs run podman rootless as UID 1000 (matching host) and mirror the
    // host's subuid/subgid range so rootless user namespaces map image store
    // files correctly. The image store is shared from host via 9p, and files
    // inside are owned by host UIDs in the subuid range (e.g., postgres UID
    // 70 in the image → host UID subuid_start+69). Identical VM subuid
    // mapping makes these appear as the same container UIDs inside the VM.
    let (subid_start, subid_size) = host_subid_mapping()?;
    let user_data = format!(
        r#"#cloud-config
ssh_pwauth: false

users:
  - name: ryra
    uid: 1000
    shell: /bin/bash
    lock_passwd: true
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - {pub_key}

write_files:
  - path: /etc/subuid
    content: "ryra:{subid_start}:{subid_size}\n"
    permissions: '0644'
  - path: /etc/subgid
    content: "ryra:{subid_start}:{subid_size}\n"
    permissions: '0644'

runcmd:
  - loginctl enable-linger ryra
"#
    );
    write_seed_iso(work_dir, output, hostname, &user_data).await
}

/// Build a full cloud-init seed ISO — installs all packages.
/// Used once during image preparation.
pub async fn build_seed_iso_full(
    work_dir: &Path,
    output: &Path,
    hostname: &str,
    pub_key: &str,
    packages: &[&str],
) -> Result<()> {
    let package_list = packages
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (subid_start, subid_size) = host_subid_mapping()?;
    let user_data = format!(
        r#"#cloud-config
ssh_pwauth: false

users:
  - name: ryra
    uid: 1000
    shell: /bin/bash
    lock_passwd: true
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - {pub_key}

packages:
{package_list}

write_files:
  - path: /etc/subuid
    content: "ryra:{subid_start}:{subid_size}\n"
    permissions: '0644'
  - path: /etc/subgid
    content: "ryra:{subid_start}:{subid_size}\n"
    permissions: '0644'

runcmd:
  - loginctl enable-linger ryra
"#
    );
    write_seed_iso(work_dir, output, hostname, &user_data).await
}

/// Write a cloud-init seed ISO from given user-data content.
async fn write_seed_iso(
    work_dir: &Path,
    output: &Path,
    hostname: &str,
    user_data: &str,
) -> Result<()> {
    let seed_dir = work_dir.join("seed");
    tokio::fs::create_dir_all(&seed_dir)
        .await
        .context("failed to create seed dir")?;

    let meta_data = format!("instance-id: {hostname}\nlocal-hostname: {hostname}\n");
    tokio::fs::write(seed_dir.join("meta-data"), &meta_data)
        .await
        .context("failed to write meta-data")?;

    tokio::fs::write(seed_dir.join("user-data"), user_data)
        .await
        .context("failed to write user-data")?;

    // Build ISO with genisoimage or mkisofs
    let iso_tools = ["genisoimage", "mkisofs"];
    let mut created = false;
    for tool in &iso_tools {
        let result = Command::new(tool)
            .args([
                "-output",
                &output.to_string_lossy(),
                "-volid",
                "cidata",
                "-joliet",
                "-rock",
                &seed_dir.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        if let Ok(status) = result
            && status.success()
        {
            created = true;
            break;
        }
    }

    if !created {
        anyhow::bail!(
            "failed to create seed ISO — install genisoimage or mkisofs:\n  \
             sudo apt install genisoimage    # Debian/Ubuntu\n  \
             sudo dnf install genisoimage    # Fedora\n  \
             sudo pacman -S cdrtools         # Arch"
        );
    }

    // Clean up seed dir
    let _ = tokio::fs::remove_dir_all(&seed_dir).await;

    Ok(())
}

/// Directory for saved container image tars shared into VMs via 9p.
///
/// Tars are an intermediate step: the host's rootless podman store uses UID
/// shifting that other podman instances can't read. We save to tar first,
/// then load into the shared store (see [`image_shared_store_dir`]).
fn image_tar_cache_dir() -> Result<PathBuf> {
    Ok(cache_base_dir()?.join("image-tars"))
}

/// Dedicated overlay store populated from tars, shared into VMs via 9p.
///
/// VMs configure `additionalimagestores` pointing at this mount, so all
/// pre-cached images are available instantly — no per-image `podman load`.
///
/// This store is created with `podman --root <path> --storage-driver overlay`
/// which uses unprivileged overlayfs (kernel ≥5.11). The resulting files are
/// owned by the current user and readable by root podman in the VM.
pub fn image_shared_store_dir() -> Result<PathBuf> {
    Ok(cache_base_dir()?.join("image-store"))
}

/// Base directory for VM work dirs (disk images, keys, logs).
/// Uses ~/.cache/ryra-e2e/vms/ instead of /tmp so we don't fill
/// up a RAM-backed tmpfs with multi-GB qcow2 COW disks.
fn vm_work_base_dir() -> Result<PathBuf> {
    Ok(cache_base_dir()?.join("vms"))
}

/// Shared cache root for all ryra-e2e artifacts.
fn cache_base_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("could not determine cache directory (is $HOME set?)")?;
    Ok(base.join("ryra-e2e"))
}

/// Ensure a container image is cached in the shared store for VM sharing.
///
/// Flow: pull → save to tar (intermediate) → load into shared overlay store.
/// The shared store at ~/.cache/ryra-e2e/image-store/ is mounted into VMs
/// via 9p and used as an `additionalimagestores` entry, making images
/// available instantly without per-image `podman load`.
///
/// Tars are kept as an intermediate cache so that re-populating the shared
/// store (e.g., after clearing it) doesn't require re-pulling from registries.
pub async fn ensure_image_cached(image: &str) -> Result<()> {
    let store_dir = image_shared_store_dir()?;
    tokio::fs::create_dir_all(&store_dir).await.ok();

    // Check if the image is already in the shared store
    if image_exists_in_store(&store_dir, image).await {
        return Ok(());
    }

    // Ensure tar exists (pull + save if needed)
    let tar_dir = image_tar_cache_dir()?;
    tokio::fs::create_dir_all(&tar_dir).await.ok();
    let tar_name = sanitize_image_name(image);
    let tar_path = tar_dir.join(&tar_name);

    if !tar_path.exists() {
        // Podman sometimes doesn't recognize the docker.io/ prefix for local
        // lookups (image exists, save) even though pull writes it that way.
        // Try the full name first, then the short name without docker.io/.
        let short_name = strip_docker_io(image);
        let local_name = if Command::new("podman")
            .args(["image", "exists", image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
        {
            image
        } else if short_name != image
            && Command::new("podman")
                .args(["image", "exists", short_name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false)
        {
            short_name
        } else {
            // Not in local store — pull it
            println!("    pulling {image}...");
            let status = Command::new("podman")
                .args(["pull", image])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .context("failed to run podman pull")?;
            if !status.success() {
                anyhow::bail!("podman pull {image} failed");
            }
            image
        };

        println!("    saving {image}...");
        let status = Command::new("podman")
            .args(["save", "-o", &tar_path.display().to_string(), local_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .context("failed to run podman save")?;
        if !status.success() {
            let _ = tokio::fs::remove_file(&tar_path).await;
            anyhow::bail!("podman save {image} failed");
        }
    }

    // Load tar into the shared overlay store
    println!("    loading {image} into shared store...");
    let status = Command::new("podman")
        .args([
            "--root",
            &store_dir.display().to_string(),
            "--storage-driver",
            "overlay",
            "load",
            "-i",
            &tar_path.display().to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("failed to load image into shared store")?;
    if !status.success() {
        anyhow::bail!("podman load into shared store failed for {image}");
    }

    // Delete the tar — the shared store is the canonical cache now.
    // If the shared store is cleared, tars will be recreated on next run.
    let _ = tokio::fs::remove_file(&tar_path).await;

    Ok(())
}

/// Check if an image exists in the shared store.
///
/// Quadlets use fully qualified names (e.g. "docker.io/library/caddy:2-alpine"),
/// but older caches may hold short-name entries — try both forms so existing
/// shared stores keep hitting.
async fn image_exists_in_store(store_dir: &Path, image: &str) -> bool {
    let short = strip_docker_io(image);
    let expanded_library = format!("docker.io/library/{short}");
    let expanded_org = format!("docker.io/{short}");
    for name in [image, short, &expanded_library, &expanded_org] {
        let ok = Command::new("podman")
            .args([
                "--root",
                &store_dir.display().to_string(),
                "--storage-driver",
                "overlay",
                "image",
                "exists",
                name,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

/// Strip the `docker.io/` or `docker.io/library/` prefix from an image name.
/// Podman sometimes doesn't recognize the prefix for local operations even
/// though `podman pull` stores images with it.
fn strip_docker_io(image: &str) -> &str {
    image
        .strip_prefix("docker.io/library/")
        .or_else(|| image.strip_prefix("docker.io/"))
        .unwrap_or(image)
}

fn sanitize_image_name(image: &str) -> String {
    image.replace(['/', ':'], "_") + ".tar"
}

/// Make host container images available in the VM via shared store.
///
/// On snapshot-restored VMs, the config is already baked in. On cold boot,
/// this configures everything from scratch. Idempotent — safe to call either way.
pub async fn load_images_into_vm(machine: &Machine, _images: &[String]) -> Result<()> {
    // Mount the 9p store (already mounted from snapshot, or needs mounting on cold boot).
    // Uses sudo because mount is a privileged operation.
    machine
        .exec("sudo mkdir -p /mnt/images && (mountpoint -q /mnt/images 2>/dev/null || sudo mount -t 9p -o trans=virtio,version=9p2000.L,ro images /mnt/images)")
        .await
        .context("failed to mount 9p image store in VM")?;

    // Configure rootless podman storage/registries at the user level
    // (~/.config/containers/). These are used by the ryra user's rootless
    // podman and don't require sudo. The VM's /etc/subuid is set to match
    // the host's at cloud-init time, so default rootless UID mapping aligns
    // with the shared image store — no userns=auto needed.
    machine
        .exec(
            "mkdir -p ~/.config/containers && \
             (grep -q '/mnt/images' ~/.config/containers/storage.conf 2>/dev/null || \
              printf '[storage]\\ndriver = \"overlay\"\\n[storage.options]\\nadditionalimagestores = [\"/mnt/images\"]\\n' > ~/.config/containers/storage.conf) && \
             (grep -q 'docker.io' ~/.config/containers/registries.conf 2>/dev/null || \
              printf 'unqualified-search-registries = [\"docker.io\"]\\n' > ~/.config/containers/registries.conf)",
        )
        .await
        .context("failed to configure podman config in VM")?;

    Ok(())
}

/// Build common SCP arguments for copying files to a VM.
fn scp_base_args(machine: &Machine) -> Vec<String> {
    let key = machine.ssh_key_path();
    vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-i".into(),
        key.to_string_lossy().into_owned(),
        "-P".into(),
        machine.ssh_port.to_string(),
    ]
}

/// SCP a local file into the VM at the given remote path.
async fn scp_to_vm(machine: &Machine, local_path: &Path, remote_path: &str) -> Result<()> {
    let dest = format!("ryra@{}:{remote_path}", machine.ssh_host);
    let mut args = scp_base_args(machine);
    args.push(local_path.to_string_lossy().into_owned());
    args.push(dest);

    let status = Command::new("scp")
        .args(&args)
        .status()
        .await
        .with_context(|| format!("failed to SCP {} to VM", local_path.display()))?;
    if !status.success() {
        anyhow::bail!("SCP of {} failed", local_path.display());
    }
    Ok(())
}

/// SCP a local directory recursively into the VM at the given remote path.
async fn scp_dir_to_vm(machine: &Machine, local_path: &Path, remote_path: &str) -> Result<()> {
    let dest = format!("ryra@{}:{remote_path}", machine.ssh_host);
    let mut args = scp_base_args(machine);
    args.push("-r".into());
    args.push(local_path.to_string_lossy().into_owned());
    args.push(dest);

    let status = Command::new("scp")
        .args(&args)
        .status()
        .await
        .with_context(|| format!("failed to SCP {} to VM", local_path.display()))?;
    if !status.success() {
        anyhow::bail!("SCP of {} failed", local_path.display());
    }
    Ok(())
}

/// SCP a remote directory recursively from the VM to the given local path.
/// The local parent directory must exist; the remote directory contents are
/// written into `local_path`.
pub async fn scp_dir_from_vm(
    machine: &Machine,
    remote_path: &str,
    local_path: &Path,
) -> Result<()> {
    let source = format!("ryra@{}:{remote_path}", machine.ssh_host);
    let mut args = scp_base_args(machine);
    args.push("-r".into());
    args.push(source);
    args.push(local_path.to_string_lossy().into_owned());

    let status = Command::new("scp")
        .args(&args)
        .status()
        .await
        .with_context(|| format!("failed to SCP {remote_path} from VM"))?;
    if !status.success() {
        anyhow::bail!("SCP of {remote_path} from VM failed");
    }
    Ok(())
}

/// Copy the ryra binary into a running VM via SCP.
pub async fn copy_ryra_to_vm(machine: &Machine, ryra_bin: &Path) -> Result<()> {
    // SCP to user's home (writable), then sudo-move to system PATH.
    scp_to_vm(machine, ryra_bin, "/tmp/ryra").await?;
    machine
        .exec("sudo mv /tmp/ryra /usr/local/bin/ryra && sudo chmod +x /usr/local/bin/ryra")
        .await?;
    Ok(())
}

/// Copy test fixtures into a running VM via SCP.
///
/// The fixtures_dir contains service directories (e.g. whoami/, postgres/).
/// These get copied into /opt/ryra-test-registry/ so ryra can find them
/// at /opt/ryra-test-registry/whoami/service.toml etc.
pub async fn copy_fixtures_to_vm(machine: &Machine, fixtures_dir: &Path) -> Result<()> {
    if !fixtures_dir.exists() {
        return Ok(());
    }

    // Create destination dir (needs sudo — /opt is root-owned, chown to ryra user)
    machine
        .exec(
            "sudo mkdir -p /opt/ryra-test-registry && sudo chown ryra:ryra /opt/ryra-test-registry",
        )
        .await?;

    // SCP each service dir individually to avoid nesting issues
    // (scp -r dir/ remote:dest/ creates dest/dir/, not dest/contents/)
    let mut entries = tokio::fs::read_dir(fixtures_dir)
        .await
        .context("failed to read fixtures directory")?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        scp_dir_to_vm(machine, &path, "/opt/ryra-test-registry/").await?;
    }

    Ok(())
}

/// Copy a local project directory (quadlet files + test.toml) into a running VM.
///
/// Files are placed at /opt/ryra-test-project/ so the test runner can copy them
/// into the systemd quadlet directory.
pub async fn copy_project_to_vm(machine: &Machine, project_dir: &Path) -> Result<()> {
    if !project_dir.exists() {
        anyhow::bail!("project directory not found: {}", project_dir.display());
    }

    machine
        .exec("sudo mkdir -p /opt/ryra-test-project && sudo chown ryra:ryra /opt/ryra-test-project")
        .await?;

    // Copy individual files (not directories) — quadlet files and test.toml
    let quadlet_extensions = ["container", "volume", "network", "pod", "kube", "toml"];
    let mut entries = tokio::fs::read_dir(project_dir)
        .await
        .with_context(|| format!("failed to read project directory {}", project_dir.display()))?;

    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && quadlet_extensions.contains(&ext)
        {
            scp_to_vm(machine, &path, "/opt/ryra-test-project/").await?;
        }
    }

    Ok(())
}
