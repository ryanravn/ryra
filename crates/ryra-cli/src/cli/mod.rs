pub mod add;
pub mod apply;
pub mod config_cmd;
pub mod diff;
pub mod linger;
pub mod list;
pub mod prompts;
pub mod registry_cmd;
pub mod remove;
pub mod reset;
pub mod search;
pub mod test;

use std::io::IsTerminal;
use std::net::TcpListener;

use ryra_core::Step;

/// Whether stdin is connected to a terminal (shared check).
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// Check if a port is already bound on the host.
///
/// A port is considered in use if binding IPv4 fails. IPv6 is only checked
/// when the system has a working IPv6 loopback — otherwise `bind(::1)` can
/// fail even on a free port, making every port look occupied.
///
/// Lives in the CLI (not core) so that `ryra-core` planning stays free of
/// real system-state probes: callers pass this as `&dyn Fn(u16) -> bool`.
pub fn is_port_in_use(port: u16) -> bool {
    if TcpListener::bind(("127.0.0.1", port)).is_err() {
        return true;
    }
    if TcpListener::bind(("::1", 0u16)).is_ok() {
        return TcpListener::bind(("::1", port)).is_err();
    }
    false
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

/// Collapse a `$HOME` prefix to `~/...` for friendlier paths.
fn tildify(path: &std::path::Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

/// Compact preamble for `ryra add`: three arrows — pulls, writes, starts.
/// Executed steps follow beneath; verbose adds detail to the same flow.
pub fn print_plan_header(steps: &[Step], service: &str, primary_url: Option<&str>) {
    use std::collections::BTreeSet;

    // Deduplicated image pulls — multi-container services need every image
    // listed so the user knows what's coming down the wire.
    let images: BTreeSet<&str> = steps
        .iter()
        .filter_map(|s| match s {
            Step::PullImage { image } => Some(image.as_str()),
            _ => None,
        })
        .collect();

    // Primary quadlet: `<service>.container`. Sidecars/env/configs are
    // implied — listing them defeats the point of a compact header.
    let quadlet_name = format!("{service}.container");
    let primary_quadlet = steps.iter().find_map(|s| match s {
        Step::WriteFile(f) => {
            let name = f.path.file_name().and_then(|n| n.to_str())?;
            (name == quadlet_name).then_some(f.path.as_path())
        }
        _ => None,
    });

    for image in &images {
        println!("→ pulls {image}");
    }
    if let Some(p) = primary_quadlet {
        println!("→ writes {}", tildify(p));
    }
    match primary_url {
        Some(url) => println!("→ starts {service} on {url}"),
        None => println!("→ starts {service}"),
    }
    println!();
}

/// Print a dry-run summary: files to write, then commands to run.
pub fn print_dry_run(steps: &[Step]) {
    let verbose = crate::verbose::is_enabled();

    enum FileEntry<'a> {
        Write(&'a ryra_core::generate::GeneratedFile),
        Copy { src: &'a std::path::Path, dst: &'a std::path::Path },
    }

    let file_steps: Vec<FileEntry> = steps
        .iter()
        .filter_map(|s| match s {
            Step::WriteFile(f) => Some(FileEntry::Write(f)),
            Step::CopyFile { src, dst } => Some(FileEntry::Copy { src, dst }),
            _ => None,
        })
        .collect();

    let commands: Vec<_> = steps
        .iter()
        .filter(|s| !matches!(s, Step::WriteFile(_) | Step::CopyFile { .. }))
        .collect();

    if !file_steps.is_empty() {
        println!("Files to write:\n");
        for entry in &file_steps {
            match entry {
                FileEntry::Write(file) => {
                    println!("  {}", file.path.display());
                    if verbose && !file.content.is_empty() {
                        for line in file.content.lines() {
                            println!("    | {line}");
                        }
                        println!();
                    }
                }
                FileEntry::Copy { src, dst } => {
                    println!("  {} (<- {})", dst.display(), src.display());
                }
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
