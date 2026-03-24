mod assert;
pub mod image;
pub mod machine;
pub mod ports;

use std::process::Stdio;

use anyhow::Result;

/// Read current memory usage from /proc/meminfo. Returns (total_mb, used_mb).
pub fn read_host_memory() -> Option<(u64, u64)> {
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
             genisoimage openssh-clients curl                   # Fedora",
            missing.join(", ")
        );
    }

    if use_kvm {
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

    Ok(())
}
