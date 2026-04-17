pub mod add;
pub mod apply;
pub mod config_cmd;
pub mod diff;
pub mod init;
pub mod linger;
pub mod ls;
pub mod prompts;
pub mod registry_cmd;
pub mod rm;
pub mod reset;
pub mod search;
pub mod status;
pub mod test;

use std::io::IsTerminal;

use ryra_core::Step;

/// Whether stdin is connected to a terminal (shared check).
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// A system CA certificate to install or remove, with the distro-specific
/// trust store path and update command.
struct CaCertTarget {
    cert_path: &'static str,
    update_cmd: &'static str,
}

const CA_TARGETS: &[CaCertTarget] = &[
    // Fedora / RHEL
    CaCertTarget {
        cert_path: "/etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt",
        update_cmd: "update-ca-trust",
    },
    // Arch Linux
    CaCertTarget {
        cert_path: "/etc/ca-certificates/trust-source/anchors/ryra-caddy-ca.crt",
        update_cmd: "update-ca-trust",
    },
    // Debian / Ubuntu
    CaCertTarget {
        cert_path: "/usr/local/share/ca-certificates/ryra-caddy-ca.crt",
        update_cmd: "update-ca-certificates",
    },
];

/// Remove Caddy's CA certificate from system and browser trust stores.
/// Called during reset and when removing caddy.
pub fn remove_caddy_ca() {
    let installed: Vec<&CaCertTarget> = CA_TARGETS
        .iter()
        .filter(|t| std::path::Path::new(t.cert_path).exists())
        .collect();

    // Browser trust store (NSS — Chromium/Brave/Chrome)
    let nssdb = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".pki/nssdb"))
        .filter(|p| p.exists());
    if let Some(ref nssdb_path) = nssdb {
        let nss_arg = format!("sql:{}", nssdb_path.display());
        let in_nss = std::process::Command::new("certutil")
            .args(["-d", &nss_arg, "-L", "-n", "ryra-caddy-ca"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if in_nss {
            match std::process::Command::new("certutil")
                .args(["-d", &nss_arg, "-D", "-n", "ryra-caddy-ca"])
                .status()
            {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("  Warning: certutil exited with {s}"),
                Err(e) => eprintln!("  Warning: could not run certutil: {e}"),
            }
        }
    }

    if installed.is_empty() {
        return;
    }

    let interactive = is_interactive();
    println!("\n  Removing Caddy CA from system trust store:");
    for target in &installed {
        println!(
            "    sudo rm -f {} && sudo {}",
            target.cert_path, target.update_cmd
        );
    }
    if interactive {
        let run = dialoguer::Confirm::new()
            .with_prompt("  Run these commands now?")
            .default(true)
            .interact()
            .unwrap_or(false);
        if !run {
            return;
        }
    }

    for target in &installed {
        let rm_ok = std::process::Command::new("sudo")
            .args(["rm", "-f", target.cert_path])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if rm_ok {
            match std::process::Command::new("sudo")
                .arg(target.update_cmd)
                .status()
            {
                Ok(s) if s.success() => {}
                _ => eprintln!("  Warning: {} failed", target.update_cmd),
            }
        }
    }
}

/// Print a dry-run summary: files to write, then commands to run.
pub fn print_dry_run(steps: &[Step]) {
    let verbose = crate::verbose::is_enabled();

    let file_steps: Vec<_> = steps
        .iter()
        .filter_map(|s| match s {
            Step::WriteFile(f) => Some(f),
            _ => None,
        })
        .collect();

    let commands: Vec<_> = steps
        .iter()
        .filter(|s| !matches!(s, Step::WriteFile(_)))
        .collect();

    if !file_steps.is_empty() {
        println!("Files to write:\n");
        for file in &file_steps {
            println!("  {}", file.path.display());
            if verbose && !file.content.is_empty() {
                for line in file.content.lines() {
                    println!("    | {line}");
                }
                println!();
            }
        }
        if !verbose {
            println!();
        }
    }

    if !commands.is_empty() {
        println!("Commands to run:\n");
        for step in &commands {
            println!("  {}", step.to_command());
        }
        println!();
    }

    println!("Dry run — no changes made. Remove --dry-run to apply.\n");
}
