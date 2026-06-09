mod assert;
pub mod image;
pub mod machine;
pub mod ports;
pub mod progress;

use std::process::Stdio;

use anyhow::Result;

/// Host memory snapshot in MB.
pub struct HostMemory {
    pub total_mb: u64,
    pub available_mb: u64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
}

/// Read current memory usage.
pub fn read_host_memory() -> Option<HostMemory> {
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
    let swap_total_kb = parse_kb("SwapTotal:").unwrap_or(0);
    let swap_free_kb = parse_kb("SwapFree:").unwrap_or(0);
    Some(HostMemory {
        total_mb: total_kb / 1024,
        available_mb: avail_kb / 1024,
        swap_total_mb: swap_total_kb / 1024,
        swap_used_mb: swap_total_kb.saturating_sub(swap_free_kb) / 1024,
    })
}

/// Check that required tools are installed.
pub fn check_prerequisites(use_kvm: bool) -> Result<()> {
    let required = [
        "qemu-system-aarch64",
        "qemu-img",
        "ssh",
        "scp",
        "ssh-keygen",
        "curl",
    ];
    let mut missing: Vec<&str> = required
        .iter()
        .filter(|c| !has_command(c))
        .copied()
        .collect();

    if !has_any_iso_tool() {
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
             cdrtools openssh curl                              # Arch",
            missing.join(", ")
        );
    }

    if use_kvm {
        check_kvm()?;
    }

    Ok(())
}

fn has_command(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn has_any_iso_tool() -> bool {
    ["genisoimage", "mkisofs"]
        .iter()
        .any(|cmd| has_command(cmd))
}

/// Check for KVM acceleration support.
fn check_kvm() -> Result<()> {
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
    Ok(())
}

/// Return QEMU KVM acceleration arguments.
pub fn accel_args() -> &'static [&'static str] {
    &["-enable-kvm"]
}
