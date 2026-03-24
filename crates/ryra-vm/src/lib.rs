mod assert;
pub mod image;
pub mod machine;
pub mod ports;

use std::process::Stdio;

use anyhow::Result;

/// Read current memory usage. Returns (total_mb, used_mb).
pub fn read_host_memory() -> Option<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let parse_kb = |key: &str| -> Option<u64> {
            meminfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        };
        let total_kb = parse_kb("MemTotal:")?;
        let avail_kb = parse_kb("MemAvailable:")?;
        let total_mb = total_kb / 1024;
        let used_mb = total_mb.saturating_sub(avail_kb / 1024);
        Some((total_mb, used_mb))
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let total_bytes: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        let total_mb = total_bytes / (1024 * 1024);
        // Rough estimate via vm_stat
        let vm_out = std::process::Command::new("vm_stat").output().ok()?;
        let vm_str = String::from_utf8_lossy(&vm_out.stdout);
        let parse_pages = |key: &str| -> u64 {
            vm_str
                .lines()
                .find(|l| l.contains(key))
                .and_then(|l| l.split_whitespace().last())
                .and_then(|v| v.trim_end_matches('.').parse::<u64>().ok())
                .unwrap_or(0)
        };
        // macOS ARM uses 16K pages, Intel uses 4K
        let page_size: u64 = if cfg!(target_arch = "aarch64") {
            16384
        } else {
            4096
        };
        let free = parse_pages("Pages free");
        let inactive = parse_pages("Pages inactive");
        let avail_mb = (free + inactive) * page_size / (1024 * 1024);
        let used_mb = total_mb.saturating_sub(avail_mb);
        Some((total_mb, used_mb))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Check that QEMU, SSH, and related tools are installed.
pub fn check_prerequisites(use_kvm: bool) -> Result<()> {
    let required = [
        "qemu-system-aarch64",
        "qemu-img",
        "ssh",
        "scp",
        "ssh-keygen",
        "curl",
    ];
    let mut missing = Vec::new();

    for cmd in &required {
        let found = std::process::Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            missing.push(*cmd);
        }
    }

    // Need at least one ISO creation tool
    let has_iso_tool = ["genisoimage", "mkisofs"].iter().any(|cmd| {
        std::process::Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if !has_iso_tool {
        missing.push("genisoimage");
    }

    if !missing.is_empty() {
        anyhow::bail!(
            "missing required tools: {}\n\
             Install with:\n  \
             sudo apt install qemu-system-arm qemu-utils qemu-efi-aarch64 \\\n    \
             genisoimage openssh-client curl                    # Debian/Ubuntu\n  \
             sudo dnf install qemu-system-aarch64 qemu-img edk2-aarch64 \\\n    \
             genisoimage openssh-clients curl                   # Fedora\n  \
             sudo pacman -S qemu-system-aarch64 qemu-img edk2-aarch64 \\\n    \
             cdrtools openssh curl                              # Arch\n  \
             brew install qemu genisoimage                      # macOS",
            missing.join(", ")
        );
    }

    if use_kvm {
        check_hw_accel()?;
    }

    Ok(())
}

/// Check for hardware acceleration support (KVM on Linux, HVF on macOS).
fn check_hw_accel() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let kvm = std::path::Path::new("/dev/kvm");
        if !kvm.exists() {
            anyhow::bail!(
                "/dev/kvm not found — KVM is not available on this machine.\n\
                 Run with --no-kvm to use software emulation (slower), or \
                 run on a machine with KVM support."
            );
        }
        let accessible = std::fs::File::open(kvm).is_ok();
        if !accessible {
            anyhow::bail!(
                "/dev/kvm exists but is not accessible — permission denied.\n\
                 Add your user to the kvm group and re-login:\n  \
                 sudo usermod -aG kvm $USER\n  \
                 # then log out and back in, or run: newgrp kvm"
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "kern.hv_support"])
            .output();
        let supported = output
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        if !supported {
            anyhow::bail!(
                "Hypervisor.framework not available on this Mac.\n\
                 Run with --no-kvm to use software emulation (slower)."
            );
        }
    }
    Ok(())
}

/// Return QEMU acceleration arguments for the current platform.
pub fn accel_args() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["-accel", "hvf"]
    } else {
        &["-enable-kvm"]
    }
}

/// Whether 9p virtfs is supported (Linux only — macOS QEMU lacks 9p support).
pub fn supports_virtfs() -> bool {
    cfg!(target_os = "linux")
}
