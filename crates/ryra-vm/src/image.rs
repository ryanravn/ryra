use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Which distro/version to use as the base VM image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian13,
    Fedora43,
}

impl Distro {
    fn cloud_image_url(&self) -> &str {
        match self {
            Distro::Debian13 => {
                "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-arm64.qcow2"
            }
            Distro::Fedora43 => {
                "https://download.fedoraproject.org/pub/fedora/linux/releases/43/Cloud/aarch64/images/Fedora-Cloud-Base-Generic-43-1.1.aarch64.qcow2"
            }
        }
    }

    fn image_filename(&self) -> &str {
        match self {
            Distro::Debian13 => "debian-13-generic-arm64.qcow2",
            Distro::Fedora43 => "fedora-43-cloud-arm64.qcow2",
        }
    }

    fn prepared_filename(&self) -> &str {
        match self {
            Distro::Debian13 => "debian-13-prepared-arm64.qcow2",
            Distro::Fedora43 => "fedora-43-prepared-arm64.qcow2",
        }
    }

    fn browser_prepared_filename(&self) -> &str {
        match self {
            Distro::Debian13 => "debian-13-browser-arm64.qcow2",
            Distro::Fedora43 => "fedora-43-browser-arm64.qcow2",
        }
    }

    /// Packages to install via cloud-init during image preparation.
    pub fn cloud_init_packages(&self) -> &[&str] {
        match self {
            // Runtime: podman, podman-compose (compose services), uidmap (rootless
            // namespaces), systemd-container (machined), git (registry fetch).
            // Test-only: curl (HTTP assertions), postgresql-client (postgres tests).
            Distro::Debian13 => &[
                "podman",
                "podman-compose",
                "uidmap",
                "git",
                "systemd-container",
                "curl",
                "postgresql-client",
            ],
            // Fedora: uidmap is part of shadow-utils (already installed).
            Distro::Fedora43 => &[
                "podman",
                "podman-compose",
                "git",
                "systemd-container",
                "curl",
            ],
        }
    }
}

impl fmt::Display for Distro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Distro::Debian13 => write!(f, "debian-13"),
            Distro::Fedora43 => write!(f, "fedora-43"),
        }
    }
}

impl std::str::FromStr for Distro {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "debian-13" => Ok(Distro::Debian13),
            "fedora-43" => Ok(Distro::Fedora43),
            other => Err(format!(
                "unknown distro: {other} (available: debian-13, fedora-43)"
            )),
        }
    }
}

/// Paths to the cached base image and EFI firmware.
pub struct Image {
    /// Prepared qcow2 image.
    pub path: PathBuf,
    pub efi_code: PathBuf,
    pub efi_vars_template: PathBuf,
    /// If true, cloud-init packages are already installed — skip package install.
    pub prepared: bool,
}

/// Cache directory for downloaded images.
fn cache_dir() -> Result<PathBuf> {
    let base = dirs::cache_dir().context("could not determine cache directory (is $HOME set?)")?;
    Ok(base.join("ryra-e2e"))
}

/// Ensure the base cloud image, prepared image, and EFI firmware are available.
///
/// The "prepared" image has all packages pre-installed (podman, nginx, git, etc.)
/// so VMs boot in ~30s instead of ~6 minutes. It's created by booting the raw
/// cloud image once with cloud-init, then snapshotting.
pub async fn ensure_image(distro: &Distro, redownload: bool, use_kvm: bool) -> Result<Image> {
    let cache = cache_dir()?;
    tokio::fs::create_dir_all(&cache)
        .await
        .context("failed to create image cache directory")?;

    let raw_path = cache.join(distro.image_filename());
    let prepared_path = cache.join(distro.prepared_filename());

    // Download raw cloud image if needed
    if redownload || !raw_path.exists() {
        download_image(distro, &raw_path).await?;
        // Force re-prepare if raw image changed
        let _ = tokio::fs::remove_file(&prepared_path).await;
    }

    // Find EFI firmware
    let efi = find_efi_firmware().await?;

    // Create a vars template if we don't have one
    let vars_template = cache.join("efivars.fd");
    if !vars_template.exists() {
        tokio::fs::copy(&efi.vars, &vars_template)
            .await
            .context("failed to copy EFI vars template")?;
    }

    // Build prepared image if it doesn't exist
    if !prepared_path.exists() {
        println!("Preparing base image (installing packages — this is a one-time operation)...");
        println!("  Serial log: /tmp/ryra-prepare-base/serial.log");
        prepare_image(
            distro,
            &raw_path,
            &prepared_path,
            &efi.code,
            &vars_template,
            use_kvm,
        )
        .await?;
        println!("Prepared image cached at: {}", prepared_path.display());
    } else {
        println!("Using prepared image: {}", prepared_path.display());
    }

    Ok(Image {
        path: prepared_path,
        efi_code: efi.code,
        efi_vars_template: vars_template,
        prepared: true,
    })
}

/// Ensure a browser-ready image exists (base image + bun + playwright + chromium).
/// Built on top of the base prepared image — one-time operation.
pub async fn ensure_browser_image(
    distro: &Distro,
    redownload: bool,
    use_kvm: bool,
) -> Result<Image> {
    // Ensure base image first
    let base = ensure_image(distro, redownload, use_kvm).await?;
    let cache = cache_dir()?;
    let browser_path = cache.join(distro.browser_prepared_filename());

    if redownload {
        let _ = tokio::fs::remove_file(&browser_path).await;
    }

    if !browser_path.exists() {
        println!("Preparing browser image (installing bun + playwright + chromium)...");
        println!("  This is a one-time operation.");
        prepare_browser_image(&base, &browser_path, use_kvm).await?;
        println!("Browser image cached at: {}", browser_path.display());
    } else {
        println!("Using browser image: {}", browser_path.display());
    }

    Ok(Image {
        path: browser_path,
        efi_code: base.efi_code,
        efi_vars_template: base.efi_vars_template,
        prepared: true,
    })
}

/// Boot the base prepared image, install bun + playwright + chromium, then snapshot.
async fn prepare_browser_image(base: &Image, browser_path: &Path, use_kvm: bool) -> Result<()> {
    use crate::machine::{Machine, SpawnOpts};
    use crate::ports;

    let id = crate::machine::random_id();
    let ssh_port = ports::allocate_ssh_port();
    let opts = SpawnOpts {
        use_kvm,
        memory_mb: 4096, // chromium install needs decent RAM
        cpus: 2,
    };

    let vm = Machine::spawn(base, &id, ssh_port, &opts).await?;

    // Install unzip (needed by bun installer), bun, playwright + chromium
    let install_script = r#"
set -e
apt-get update -qq && apt-get install -y -qq unzip >/dev/null 2>&1
curl -fsSL https://bun.sh/install | bash
export BUN_INSTALL="$HOME/.bun"
export PATH="$BUN_INSTALL/bin:$PATH"

# Create a global playwright project so chromium is cached system-wide
mkdir -p /opt/playwright
cd /opt/playwright
bun init -y >/dev/null 2>&1
bun add playwright @playwright/test
bunx playwright install chromium --with-deps

# Add bun to PATH for all future SSH sessions
echo 'export BUN_INSTALL="$HOME/.bun"' >> /root/.bashrc
echo 'export PATH="$BUN_INSTALL/bin:$PATH"' >> /root/.bashrc
"#;

    println!("  Installing bun + playwright + chromium in VM...");
    let result = vm.exec(install_script).await;
    if let Err(e) = &result {
        let _ = vm.destroy().await;
        anyhow::bail!("failed to install browser tools: {e:#}");
    }

    // Shut down and snapshot
    let _ = vm.exec("sync && poweroff").await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Copy the COW disk as the browser image (flatten the overlay)
    let disk = vm.work_dir.join("disk.qcow2");
    let status = Command::new("qemu-img")
        .args([
            "convert",
            "-f",
            "qcow2",
            "-O",
            "qcow2",
            &disk.to_string_lossy(),
            &browser_path.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("qemu-img convert failed")?;
    if !status.success() {
        anyhow::bail!("qemu-img convert failed for browser image");
    }

    let _ = vm.destroy().await;
    Ok(())
}

struct EfiFirmware {
    code: PathBuf,
    vars: PathBuf,
}

async fn find_efi_firmware() -> Result<EfiFirmware> {
    let candidates = [
        // Debian/Ubuntu
        (
            "/usr/share/AAVMF/AAVMF_CODE.fd",
            "/usr/share/AAVMF/AAVMF_VARS.fd",
        ),
        (
            "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
            "/usr/share/qemu-efi-aarch64/vars-template-pflash.raw",
        ),
        // Fedora / Arch
        (
            "/usr/share/edk2/aarch64/QEMU_EFI-pflash.raw",
            "/usr/share/edk2/aarch64/vars-template-pflash.raw",
        ),
    ];

    for (code, vars) in &candidates {
        let code_path = PathBuf::from(code);
        let vars_path = PathBuf::from(vars);
        if code_path.exists() && vars_path.exists() {
            return Ok(EfiFirmware {
                code: code_path,
                vars: vars_path,
            });
        }
    }

    anyhow::bail!(
        "EFI firmware not found. Install it with:\n  \
         sudo apt install qemu-efi-aarch64    # Debian/Ubuntu\n  \
         sudo dnf install edk2-aarch64        # Fedora\n  \
         sudo pacman -S edk2-aarch64          # Arch"
    )
}

async fn download_image(distro: &Distro, dest: &PathBuf) -> Result<()> {
    let url = distro.cloud_image_url();
    println!("Downloading {distro} cloud image...");
    println!("  {url}");

    let partial = dest.with_extension("qcow2.partial");

    let status = Command::new("curl")
        .args([
            "-L",
            "--progress-bar",
            "-o",
            &partial.to_string_lossy(),
            url,
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to run curl — is it installed?")?;

    if !status.success() {
        let _ = tokio::fs::remove_file(&partial).await;
        anyhow::bail!("failed to download cloud image from {url}");
    }

    tokio::fs::rename(&partial, dest)
        .await
        .context("failed to move downloaded image into place")?;

    println!("Image cached at: {}", dest.display());
    Ok(())
}

/// Boot the raw cloud image, let cloud-init install packages, then snapshot it.
///
/// This is a one-time operation. The resulting image has podman, nginx, git, etc.
/// already installed, so subsequent VMs skip the slow package install step.
async fn prepare_image(
    distro: &Distro,
    raw_image: &Path,
    prepared_path: &Path,
    efi_code: &Path,
    efi_vars_template: &Path,
    use_kvm: bool,
) -> Result<()> {
    let work_dir = cache_dir()?.join("prepare-base");
    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    tokio::fs::create_dir_all(&work_dir)
        .await
        .context("failed to create prepare work dir")?;

    // Create a working copy of the raw image (not COW — we want a standalone result)
    let disk = work_dir.join("disk.qcow2");
    let status = Command::new("qemu-img")
        .args([
            "create",
            "-f",
            "qcow2",
            "-b",
            &raw_image.to_string_lossy(),
            "-F",
            "qcow2",
            &disk.to_string_lossy(),
            "20G",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("qemu-img create failed")?;
    if !status.success() {
        anyhow::bail!("qemu-img create failed for prepare step");
    }

    // Copy EFI vars
    let efi_vars = work_dir.join("efivars.fd");
    tokio::fs::copy(efi_vars_template, &efi_vars)
        .await
        .context("failed to copy EFI vars")?;

    // Generate temp SSH key
    let key_path = work_dir.join("id_ed25519");
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &key_path.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("ssh-keygen failed")?;
    if !status.success() {
        anyhow::bail!("ssh-keygen failed");
    }
    let pub_key = tokio::fs::read_to_string(format!("{}.pub", key_path.display()))
        .await
        .context("failed to read public key")?;

    // Build seed ISO with full package install
    let seed_iso = work_dir.join("seed.iso");
    crate::machine::build_seed_iso_full(
        &work_dir,
        &seed_iso,
        "ryra-prepare",
        pub_key.trim(),
        distro.cloud_init_packages(),
    )
    .await?;

    // Boot VM
    let ssh_port: u16 = 10099;
    let serial_log = work_dir.join("serial.log");
    let memory = "2048";
    let cpus = "2";
    let efi_code_arg = format!(
        "if=pflash,format=raw,file={},readonly=on",
        efi_code.display()
    );
    let efi_vars_arg = format!("if=pflash,format=raw,file={}", efi_vars.display());
    let disk_arg = format!("if=virtio,file={},format=qcow2", disk.display());
    let seed_arg = format!("if=virtio,file={},format=raw", seed_iso.display());
    let nic_arg = format!("user,hostfwd=tcp::{ssh_port}-:22");
    let serial_arg = format!("file:{}", serial_log.display());

    let mut args: Vec<&str> = vec![
        "-machine",
        "virt",
        "-cpu",
        if use_kvm { "host" } else { "max" },
        "-m",
        memory,
        "-smp",
        cpus,
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
    if use_kvm {
        args.extend(crate::accel_args().iter().copied());
    }

    let mut qemu = Command::new("qemu-system-aarch64")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start QEMU for image preparation")?;

    // Wait for SSH
    let timeout = if use_kvm {
        std::time::Duration::from_secs(300)
    } else {
        std::time::Duration::from_secs(900)
    };
    let start = std::time::Instant::now();
    let port_str = ssh_port.to_string();
    loop {
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
                &key_path.to_string_lossy(),
                "-p",
                &port_str,
                "root@127.0.0.1",
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;

        if let Ok(s) = result
            && s.success()
        {
            break;
        }

        if start.elapsed() > timeout {
            let _ = qemu.kill().await;
            anyhow::bail!(
                "timed out waiting for SSH during image preparation after {}s\n  \
                 Serial log: {}",
                timeout.as_secs(),
                serial_log.display()
            );
        }

        if start.elapsed().as_secs().is_multiple_of(30) && start.elapsed().as_secs() > 0 {
            println!(
                "  preparing image... ({:.0}s elapsed)",
                start.elapsed().as_secs_f64()
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    // Wait for cloud-init to finish
    println!("  SSH ready, waiting for cloud-init to finish installing packages...");
    let ci_result = Command::new("ssh")
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
            &key_path.to_string_lossy(),
            "-p",
            &port_str,
            "root@127.0.0.1",
            "cloud-init status --wait",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("cloud-init wait failed")?;

    if !ci_result.success() {
        let _ = qemu.kill().await;
        anyhow::bail!("cloud-init failed during image preparation");
    }

    // Clean up cloud-init state so it runs again on next boot (for per-VM SSH keys)
    let _ = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "BatchMode=yes",
            "-i",
            &key_path.to_string_lossy(),
            "-p",
            &port_str,
            "root@127.0.0.1",
            "cloud-init clean --logs && rm -f /etc/ssh/ssh_host_*_key*",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    // Shut down gracefully
    let _ = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "BatchMode=yes",
            "-i",
            &key_path.to_string_lossy(),
            "-p",
            &port_str,
            "root@127.0.0.1",
            "poweroff",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let _ = qemu.kill().await;
    let _ = qemu.wait().await;

    // Compact the image — squash the COW layer into a standalone file
    let status = Command::new("qemu-img")
        .args([
            "convert",
            "-O",
            "qcow2",
            "-c",
            &disk.to_string_lossy(),
            &prepared_path.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("qemu-img convert failed")?;
    if !status.success() {
        anyhow::bail!("failed to compact prepared image");
    }

    // Clean up work dir
    let _ = tokio::fs::remove_dir_all(&work_dir).await;

    Ok(())
}
