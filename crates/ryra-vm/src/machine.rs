use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::VmBackend;
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

/// A running VM for E2E testing (QEMU or vfkit).
pub struct Machine {
    pub name: String,
    /// SSH target host — "127.0.0.1" for QEMU (port-forwarded), VM IP for AppleVz.
    pub ssh_host: String,
    pub ssh_port: u16,
    pub work_dir: PathBuf,
    process: tokio::process::Child,
}

/// Options that affect how the VM is launched.
pub struct SpawnOpts {
    pub backend: VmBackend,
    pub use_kvm: bool,
    pub memory_mb: u32,
    pub cpus: u32,
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
            backend: VmBackend::default_for_platform(),
            use_kvm: true,
            memory_mb: 2048,
            cpus: 2,
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
        match opts.backend {
            VmBackend::Qemu => Self::spawn_qemu(image, test_id, ssh_port, opts).await,
            VmBackend::AppleVz => Self::spawn_apple_vz(image, test_id, opts).await,
        }
    }

    async fn spawn_qemu(
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
                "20G",
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

        // Share the image cache dir with the VM via 9p virtfs — lets the VM
        // read host-side image tars directly without SCP/network transfer.
        // Only available on Linux — macOS QEMU lacks 9p support.
        let image_cache = image_cache_dir()?;
        std::fs::create_dir_all(&image_cache)
            .context("failed to create image cache directory")?;
        let virtfs_arg = format!(
            "local,path={},mount_tag=images,security_model=none,readonly=on",
            image_cache.display()
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
        ];

        if crate::supports_virtfs() {
            args.extend(["-virtfs", &virtfs_arg]);
        }

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

        // Register for signal cleanup
        if let Some(pid) = machine.process.id() {
            register_vm(pid, &machine.work_dir);
        }

        // Wait for SSH to come up (cloud-init installs sshd + packages first)
        let boot_timeout = opts.boot_timeout();
        machine.wait_for_ssh(boot_timeout).await?;

        // Wait for cloud-init to finish (packages installed, files written)
        // cloud-init status --wait blocks until all stages complete
        machine
            .exec("cloud-init status --wait")
            .await
            .context("cloud-init did not complete")?;

        Ok(machine)
    }

    /// Spawn a VM using Apple Virtualization.framework via vfkit.
    ///
    /// Uses NAT networking with IP discovery via a virtio-fs shared directory.
    /// The VM writes its IP to a file in the shared dir during cloud-init,
    /// and the host polls for it. Uses vfkit's built-in `--cloud-init` flag
    /// to inject user-data/meta-data without needing genisoimage.
    async fn spawn_apple_vz(
        image: &Image,
        test_id: &str,
        opts: &SpawnOpts,
    ) -> Result<Self> {
        let raw_image = image.raw_path.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Apple Virtualization backend requires a raw disk image")
        })?;

        let name = format!("ryra-test-{test_id}");
        let work_dir = vm_work_base_dir()?.join(&name);
        tokio::fs::create_dir_all(&work_dir)
            .await
            .context("failed to create VM work directory")?;

        // Shared directory for VM → host communication (IP discovery)
        let vminfo_dir = work_dir.join("vminfo");
        tokio::fs::create_dir_all(&vminfo_dir)
            .await
            .context("failed to create vminfo directory")?;

        // APFS clone the raw image (instant, near-zero disk cost on APFS)
        let disk = work_dir.join("disk.raw");
        if run_cmd("cp", &["-c", &raw_image.to_string_lossy(), &disk.to_string_lossy()])
            .await
            .is_err()
        {
            // Fallback to regular copy if APFS clone fails (non-APFS filesystem)
            run_cmd("cp", &[&raw_image.to_string_lossy(), &disk.to_string_lossy()])
                .await
                .context("failed to copy raw disk image")?;
        }

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

        // Write cloud-init files — vfkit's --cloud-init flag builds the ISO
        // internally, so no genisoimage/mkisofs needed on macOS.
        let cloud_init_dir = work_dir.join("cloud-init");
        tokio::fs::create_dir_all(&cloud_init_dir).await?;
        write_cloud_init_vz(&cloud_init_dir, &name, pub_key.trim()).await?;

        let user_data_path = cloud_init_dir.join("user-data");
        let meta_data_path = cloud_init_dir.join("meta-data");
        let cloud_init_arg = format!(
            "{},{}",
            user_data_path.display(),
            meta_data_path.display()
        );

        // Image cache directory for virtio-fs sharing
        let image_cache = image_cache_dir()?;
        std::fs::create_dir_all(&image_cache)
            .context("failed to create image cache directory")?;

        // Serial log
        let serial_log = work_dir.join("serial.log");

        // vfkit EFI variable store (created by vfkit on first boot)
        let nvram_path = work_dir.join("nvram.bin");

        // Build vfkit command
        let memory_str = opts.memory_mb.to_string();
        let cpus_str = opts.cpus.to_string();
        let bootloader_arg = format!(
            "efi,variable-store={},create",
            nvram_path.display()
        );
        let disk_arg = format!("virtio-blk,path={}", disk.display());
        let net_arg = "virtio-net,nat";
        let vminfo_fs_arg = format!(
            "virtio-fs,sharedDir={},mountTag=vminfo",
            vminfo_dir.display()
        );
        let images_fs_arg = format!(
            "virtio-fs,sharedDir={},mountTag=images",
            image_cache.display()
        );
        let serial_arg = format!(
            "virtio-serial,logFilePath={}",
            serial_log.display()
        );

        let process = Command::new("vfkit")
            .args([
                "--cpus", &cpus_str,
                "--memory", &memory_str,
                "--bootloader", &bootloader_arg,
                "--cloud-init", &cloud_init_arg,
                "--device", &disk_arg,
                "--device", net_arg,
                "--device", &vminfo_fs_arg,
                "--device", &images_fs_arg,
                "--device", &serial_arg,
                "--device", "virtio-rng",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start vfkit — install with: brew install vfkit")?;

        let machine = Machine {
            name,
            ssh_host: String::new(), // will be discovered
            ssh_port: 22,
            work_dir,
            process,
        };

        // Register for signal cleanup
        if let Some(pid) = machine.process.id() {
            register_vm(pid, &machine.work_dir);
        }

        // Wait for the VM to write its IP to the shared directory
        let boot_timeout = opts.boot_timeout();
        let ip = wait_for_vm_ip(&vminfo_dir, boot_timeout).await?;
        let machine = Machine {
            ssh_host: ip,
            ..machine
        };

        // Wait for SSH to come up
        machine.wait_for_ssh(boot_timeout).await?;

        // Wait for cloud-init to finish
        machine
            .exec("cloud-init status --wait")
            .await
            .context("cloud-init did not complete")?;

        Ok(machine)
    }

    /// Run a command inside the VM via SSH.
    pub async fn exec(&self, cmd: &str) -> Result<ExecOutput> {
        let key = self.ssh_key_path();
        let port = self.ssh_port.to_string();
        let target = format!("root@{}", self.ssh_host);
        let output = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "BatchMode=yes",
                "-i",
                &key.to_string_lossy(),
                "-p",
                &port,
                &target,
                cmd,
            ])
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

    /// Shut down the VM and clean up files.
    pub async fn destroy(mut self) -> Result<()> {
        if let Some(pid) = self.process.id() {
            deregister_vm(pid);
        }
        let _ = self.exec("poweroff").await;
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
             -i {}/id_ed25519 -p {} root@{}\n  \
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
        let key = self.ssh_key_path();
        let port = self.ssh_port.to_string();
        let target = format!("root@{}", self.ssh_host);
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
                .args([
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-o",
                    "UserKnownHostsFile=/dev/null",
                    "-o",
                    "LogLevel=ERROR",
                    "-o",
                    "ConnectTimeout=3",
                    "-o",
                    "BatchMode=yes",
                    "-i",
                    &key.to_string_lossy(),
                    "-p",
                    &port,
                    &target,
                    "true",
                ])
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

/// Build a minimal cloud-init seed ISO — just SSH key, no package installs.
/// Used for test VMs backed by a prepared image that already has packages.
async fn build_seed_iso(
    work_dir: &Path,
    output: &Path,
    hostname: &str,
    pub_key: &str,
) -> Result<()> {
    let user_data = format!(
        r#"#cloud-config
disable_root: false
ssh_pwauth: false

users:
  - name: root
    lock_passwd: true
    ssh_authorized_keys:
      - {pub_key}

runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd
  - systemctl enable podman.socket
  - loginctl enable-linger root
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
    let user_data = format!(
        r#"#cloud-config
disable_root: false
ssh_pwauth: false

users:
  - name: root
    lock_passwd: true
    ssh_authorized_keys:
      - {pub_key}

packages:
{package_list}

runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd
  - systemctl enable podman.socket
  - loginctl enable-linger root
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
             sudo pacman -S cdrtools         # Arch\n  \
             brew install cdrtools            # macOS"
        );
    }

    // Clean up seed dir
    let _ = tokio::fs::remove_dir_all(&seed_dir).await;

    Ok(())
}

/// Write cloud-init user-data and meta-data files for Apple Virtualization VMs.
///
/// vfkit's `--cloud-init` flag builds the ISO internally from these files,
/// so no genisoimage/mkisofs needed. Adds extra runcmd to mount virtio-fs
/// and write the VM's IP for host-side discovery.
async fn write_cloud_init_vz(
    cloud_init_dir: &Path,
    hostname: &str,
    pub_key: &str,
) -> Result<()> {
    let user_data = format!(
        r#"#cloud-config
disable_root: false
ssh_pwauth: false

users:
  - name: root
    lock_passwd: true
    ssh_authorized_keys:
      - {pub_key}

runcmd:
  - sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
  - systemctl restart sshd
  - systemctl enable podman.socket
  - loginctl enable-linger root
  - mkdir -p /mnt/vminfo
  - |
    for i in $(seq 1 60); do
      mount -t virtiofs vminfo /mnt/vminfo 2>/dev/null && break
      sleep 1
    done
  - |
    for i in $(seq 1 30); do
      IP=$(hostname -I | awk '{{print $1}}')
      [ -n "$IP" ] && echo "$IP" > /mnt/vminfo/ip && break
      sleep 1
    done
"#
    );

    let meta_data = format!("instance-id: {hostname}\nlocal-hostname: {hostname}\n");

    tokio::fs::write(cloud_init_dir.join("user-data"), &user_data)
        .await
        .context("failed to write cloud-init user-data")?;
    tokio::fs::write(cloud_init_dir.join("meta-data"), &meta_data)
        .await
        .context("failed to write cloud-init meta-data")?;

    Ok(())
}

/// Poll a shared directory for the VM's IP address file.
///
/// The VM's cloud-init writes its IP to `<dir>/ip` via virtio-fs.
async fn wait_for_vm_ip(vminfo_dir: &Path, timeout: std::time::Duration) -> Result<String> {
    let ip_file = vminfo_dir.join("ip");
    let start = std::time::Instant::now();

    loop {
        if ip_file.exists() {
            let ip = tokio::fs::read_to_string(&ip_file)
                .await
                .context("failed to read VM IP file")?;
            let ip = ip.trim().to_string();
            if !ip.is_empty() {
                return Ok(ip);
            }
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "timed out waiting for VM to report its IP after {}s\n  \
                 The VM may have failed to boot or cloud-init did not run.\n  \
                 Check serial log in the VM work directory.",
                timeout.as_secs()
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Cache directory for saved container images on the host.
fn image_cache_dir() -> Result<PathBuf> {
    Ok(cache_base_dir()?.join("images"))
}

/// Base directory for VM work dirs (disk images, keys, logs).
/// Uses ~/.cache/ryra-e2e/vms/ instead of /tmp so we don't fill
/// up a RAM-backed tmpfs with multi-GB qcow2 COW disks.
fn vm_work_base_dir() -> Result<PathBuf> {
    Ok(cache_base_dir()?.join("vms"))
}

/// Shared cache root for all ryra-e2e artifacts.
fn cache_base_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir()
        .context("could not determine cache directory (is $HOME set?)")?;
    Ok(base.join("ryra-e2e"))
}

/// Ensure a container image is pulled and saved as a tar in the cache.
///
/// Uses the host's normal podman store — images pulled for testing are
/// the same ones ryra deploys, so sharing the store avoids duplicate pulls.
/// The tar cache (`~/.cache/ryra-e2e/images/`) is what gets shared into VMs.
pub async fn ensure_image_cached(image: &str) -> Result<PathBuf> {
    let cache = image_cache_dir()?;
    tokio::fs::create_dir_all(&cache)
        .await
        .context("failed to create image cache dir")?;

    // Use a safe filename: replace / and : with -
    let safe_name = image.replace(['/', ':'], "-");
    let tar_path = cache.join(format!("{safe_name}.tar"));

    if tar_path.exists() {
        return Ok(tar_path);
    }

    // Pull using the host's normal podman store
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

    // Save to tar for 9p sharing into VMs
    let partial = tar_path.with_extension("tar.partial");
    let status = Command::new("podman")
        .args(["save", "-o", &partial.to_string_lossy(), image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("failed to run podman save")?;
    if !status.success() {
        let _ = tokio::fs::remove_file(&partial).await;
        anyhow::bail!("podman save {image} failed");
    }

    tokio::fs::rename(&partial, &tar_path)
        .await
        .context("failed to move saved image into cache")?;

    Ok(tar_path)
}

/// Load cached container images into a VM.
///
/// Strategy depends on backend:
/// - QEMU on Linux: 9p virtfs mount (direct file access, no transfer)
/// - Apple Virtualization: virtio-fs mount (direct file access, no transfer)
/// - QEMU on macOS: SCP fallback (no 9p/virtio-fs in QEMU on macOS)
pub async fn load_images_into_vm(
    machine: &Machine,
    images: &[String],
    backend: VmBackend,
) -> Result<()> {
    if images.is_empty() {
        return Ok(());
    }

    // Configure podman so rootless users can see root's image store.
    machine
        .exec(
            "mkdir -p /etc/containers/storage.conf.d && \
             printf '[storage.options]\\nadditionalimagestores = [\"/var/lib/containers/storage\"]\\n' \
             > /etc/containers/storage.conf.d/shared-cache.conf"
        )
        .await
        .ok(); // best-effort

    let use_shared_fs = match backend {
        VmBackend::AppleVz => {
            // virtio-fs is already available — mount the images share
            machine
                .exec("mkdir -p /mnt/images && mount -t virtiofs images /mnt/images")
                .await
                .context("failed to mount virtio-fs image cache in VM")?;
            true
        }
        VmBackend::Qemu if crate::supports_virtfs() => {
            // 9p virtfs on Linux QEMU
            machine
                .exec("mkdir -p /mnt/images && mount -t 9p -o trans=virtio,ro images /mnt/images")
                .await
                .context("failed to mount 9p image cache in VM")?;
            true
        }
        _ => false,
    };

    for image in images {
        let tar_path = ensure_image_cached(image).await?;
        let safe_name = image.replace(['/', ':'], "-");
        println!("    loading {image} into VM...");

        if use_shared_fs {
            let remote_path = format!("/mnt/images/{safe_name}.tar");
            machine
                .exec(&format!("podman load -i {remote_path}"))
                .await
                .context(format!("failed to load {image} in VM"))?;
        } else {
            // No shared filesystem — SCP the tar into the VM and load it
            scp_to_vm(machine, &tar_path, &format!("/tmp/{safe_name}.tar")).await?;
            machine
                .exec(&format!(
                    "podman load -i /tmp/{safe_name}.tar && rm -f /tmp/{safe_name}.tar"
                ))
                .await
                .context(format!("failed to load {image} in VM"))?;
        }
    }

    Ok(())
}

/// SCP a local file into the VM at the given remote path.
async fn scp_to_vm(machine: &Machine, local_path: &Path, remote_path: &str) -> Result<()> {
    let key = machine.ssh_key_path();
    let port = machine.ssh_port.to_string();
    let dest = format!("root@{}:{remote_path}", machine.ssh_host);

    let status = Command::new("scp")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-i",
            &key.to_string_lossy(),
            "-P",
            &port,
            &local_path.to_string_lossy(),
            &dest,
        ])
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
    let key = machine.ssh_key_path();
    let port = machine.ssh_port.to_string();
    let dest = format!("root@{}:{remote_path}", machine.ssh_host);

    let status = Command::new("scp")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-i",
            &key.to_string_lossy(),
            "-P",
            &port,
            "-r",
            &local_path.to_string_lossy(),
            &dest,
        ])
        .status()
        .await
        .with_context(|| format!("failed to SCP {} to VM", local_path.display()))?;
    if !status.success() {
        anyhow::bail!("SCP of {} failed", local_path.display());
    }
    Ok(())
}

/// Copy the ryra binary into a running VM via SCP.
pub async fn copy_ryra_to_vm(machine: &Machine, ryra_bin: &Path) -> Result<()> {
    scp_to_vm(machine, ryra_bin, "/usr/local/bin/ryra").await?;
    machine.exec("chmod +x /usr/local/bin/ryra").await?;
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

    // Create destination dir first
    machine.exec("mkdir -p /opt/ryra-test-registry").await?;

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
