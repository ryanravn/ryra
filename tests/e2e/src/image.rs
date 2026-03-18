use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Which distro/version to use as the base VM image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian13,
}

impl Distro {
    fn cloud_image_url(&self) -> &str {
        match self {
            Distro::Debian13 => {
                "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-arm64.qcow2"
            }
        }
    }

    fn image_filename(&self) -> &str {
        match self {
            Distro::Debian13 => "debian-13-generic-arm64.qcow2",
        }
    }
}

impl fmt::Display for Distro {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Distro::Debian13 => write!(f, "debian-13"),
        }
    }
}

impl std::str::FromStr for Distro {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "debian-13" => Ok(Distro::Debian13),
            other => Err(format!("unknown distro: {other}")),
        }
    }
}

/// Paths to the cached base image and EFI firmware.
pub struct Image {
    pub path: PathBuf,
    pub efi_code: PathBuf,
    pub efi_vars_template: PathBuf,
}

/// Cache directory for downloaded images.
fn cache_dir() -> PathBuf {
    let dir = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("ryra-e2e");
    dir
}

/// Ensure the base cloud image and EFI firmware are available.
pub async fn ensure_image(distro: &Distro, redownload: bool) -> Result<Image> {
    let cache = cache_dir();
    tokio::fs::create_dir_all(&cache)
        .await
        .context("failed to create image cache directory")?;

    let image_path = cache.join(distro.image_filename());

    // Download cloud image if needed
    if redownload || !image_path.exists() {
        download_image(distro, &image_path).await?;
    } else {
        println!("Using cached image: {}", image_path.display());
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

    Ok(Image {
        path: image_path,
        efi_code: efi.code,
        efi_vars_template: vars_template,
    })
}

struct EfiFirmware {
    code: PathBuf,
    vars: PathBuf,
}

async fn find_efi_firmware() -> Result<EfiFirmware> {
    // Standard locations on Debian/Ubuntu for aarch64 UEFI
    let candidates = [
        (
            "/usr/share/AAVMF/AAVMF_CODE.fd",
            "/usr/share/AAVMF/AAVMF_VARS.fd",
        ),
        (
            "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
            "/usr/share/qemu-efi-aarch64/vars-template-pflash.raw",
        ),
        // Fedora/RHEL paths
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
         sudo dnf install edk2-aarch64        # Fedora"
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

    // Atomic rename
    tokio::fs::rename(&partial, dest)
        .await
        .context("failed to move downloaded image into place")?;

    println!("Image cached at: {}", dest.display());
    Ok(())
}
