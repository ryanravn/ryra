pub mod add;
pub mod apply;
pub mod backup;
pub mod configure;
pub mod diff;
pub mod doctor;
pub mod init;
pub mod lifecycle;
pub mod linger;
pub mod list;
pub mod prompts;
pub mod registry_cmd;
pub mod remove;
pub mod reset;
pub mod revert;
pub mod search;
pub mod status;
pub mod style;
pub mod sysctl_low_ports;
pub mod test;
pub mod upgrade;

use std::io::IsTerminal;
use std::net::TcpListener;

use ryra_core::Step;

/// Whether we can safely run interactive dialoguer prompts.
///
/// Both stdin AND stdout must be TTYs: stdin because we need to read the
/// user's response, stdout because dialoguer writes the prompt there and
/// errors with "not a terminal" if it isn't one. Checking only stdin
/// misses the `ryra add | tee` / test-runner case where stdout is
/// captured but stdin happens to be inherited from the parent shell.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
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
        cert_path: "/etc/pki/ca-trust/source/anchors/services-caddy-ca.crt",
        update_cmd: "update-ca-trust",
    },
    // Arch Linux
    CaCertTarget {
        cert_path: "/etc/ca-certificates/trust-source/anchors/services-caddy-ca.crt",
        update_cmd: "update-ca-trust",
    },
    // Debian / Ubuntu
    CaCertTarget {
        cert_path: "/usr/local/share/ca-certificates/services-caddy-ca.crt",
        update_cmd: "update-ca-certificates",
    },
];

/// The nickname used for ryra's Caddy CA in every NSS database (user NSS for
/// Chromium and every Firefox profile). Keeping it symmetric on install and
/// uninstall means `remove` / `reset` can always locate and drop the cert.
pub const CADDY_CA_NICKNAME: &str = "services-caddy-ca";

/// `~/.pki/nssdb` — the Chromium-family (Chrome, Edge, Brave, Opera, Vivaldi)
/// per-user cert store. Returns `None` if `$HOME` is unset.
pub fn nssdb_dir() -> Option<std::path::PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".pki/nssdb"))
}

/// Every Firefox profile directory on the host that has a `cert9.db`. Covers
/// the native install, Flatpak, and Snap. Older `cert8.db`-only profiles
/// (Firefox <58) are skipped.
pub fn firefox_profile_dirs() -> Vec<std::path::PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    let home = std::path::PathBuf::from(home);
    let bases = [
        home.join(".mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
    ];
    let mut profiles = Vec::new();
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.join("cert9.db").is_file() {
                profiles.push(dir);
            }
        }
    }
    profiles
}

/// Remove `/etc/hosts` entries this service added on install. Only lines
/// whose trailing comment matches `# Service-Source: registry/<service>`
/// are removed — handwritten entries (no marker) are left alone, so a
/// pre-existing `127.0.0.1 seafile` from before ryra never got clobbered.
///
/// Best-effort with sudo: tries `sudo -n` first (passwordless / cached),
/// escalates to interactive prompt if stderr is a TTY, prints a warning
/// otherwise. Failure to remove is non-fatal — the entry is harmless;
/// we just can't clean it up automatically.
pub fn remove_hosts_entries(service: &str) {
    use std::io::Write;
    let marker = format!("# Service-Source: registry/{service}");
    let Ok(content) = std::fs::read_to_string("/etc/hosts") else {
        return;
    };
    let lines: Vec<&str> = content.lines().collect();
    let kept: Vec<&str> = lines
        .iter()
        .filter(|l| !l.contains(&marker))
        .copied()
        .collect();
    if kept.len() == lines.len() {
        return;
    }
    let removed_count = lines.len() - kept.len();
    let mut new_content = kept.join("\n");
    if content.ends_with('\n') && !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    let try_write = |interactive: bool| -> bool {
        let mut cmd = std::process::Command::new("sudo");
        if !interactive {
            cmd.arg("-n");
        }
        cmd.args(["sh", "-c", "cat > /etc/hosts"]);
        cmd.stdin(std::process::Stdio::piped());
        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(new_content.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    };

    if try_write(false) {
        println!("  Removed {removed_count} stale /etc/hosts entry(ies) (via sudo).");
    } else if std::io::stderr().is_terminal() {
        eprintln!("  Removing {removed_count} stale /etc/hosts entry(ies) (sudo required):");
        if try_write(true) {
            println!("  Removed.");
        } else {
            eprintln!(
                "  WARN: failed to remove /etc/hosts entries for {service}. They are harmless but persist."
            );
        }
    } else {
        eprintln!(
            "  WARN: {removed_count} stale /etc/hosts entry(ies) for {service} not removed (sudo required)."
        );
    }
}

/// Remove Caddy's CA certificate from every rootless trust store (user NSS
/// DB, Firefox profiles). For the system trust store — which ryra never
/// installed itself, only ever printed a hint for — we print the matching
/// removal hint if something is still there.
pub fn remove_caddy_ca() {
    // No certutil means the matching install path never ran either — skip
    // silently so reset/remove don't spew warnings on hosts without nss-tools.
    let have_certutil = std::process::Command::new("certutil")
        .arg("-V")
        .output()
        .is_ok();

    if have_certutil {
        // Rootless: user NSS DB (Chromium family)
        if let Some(nssdb_path) = nssdb_dir().filter(|p| p.exists()) {
            let nss_arg = format!("sql:{}", nssdb_path.display());
            delete_ca_from_nssdb(&nss_arg);
        }

        // Rootless: every Firefox profile we can find
        for profile in firefox_profile_dirs() {
            let nss_arg = format!("sql:{}", profile.display());
            delete_ca_from_nssdb(&nss_arg);
        }
    }

    // System trust (anything at /etc/pki/...) — ryra didn't install it, so
    // ryra doesn't remove it. Point the user at the one-liner instead.
    let installed: Vec<&CaCertTarget> = CA_TARGETS
        .iter()
        .filter(|t| std::path::Path::new(t.cert_path).exists())
        .collect();
    if !installed.is_empty() {
        println!();
        println!("  Caddy CA is still in the system trust store. To remove it:");
        for target in &installed {
            println!(
                "    sudo rm -f {} && sudo {}",
                target.cert_path, target.update_cmd
            );
        }
    }
}

/// Best-effort `certutil -D` against a NSS DB directory. Silent when the
/// cert isn't in the DB (common case after manual cleanup); warns on real
/// failures so a busted `certutil` doesn't swallow state we wanted gone.
fn delete_ca_from_nssdb(nss_arg: &str) {
    let present = std::process::Command::new("certutil")
        .args(["-d", nss_arg, "-L", "-n", CADDY_CA_NICKNAME])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !present {
        return;
    }
    match std::process::Command::new("certutil")
        .args(["-d", nss_arg, "-D", "-n", CADDY_CA_NICKNAME])
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("  Warning: certutil -D exited with {s} for {nss_arg}"),
        Err(e) => eprintln!("  Warning: could not run certutil for {nss_arg}: {e}"),
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

    let arrow = style::arrow();
    for image in &images {
        println!("{arrow} pulls {image}");
    }
    if let Some(p) = primary_quadlet {
        println!("{arrow} writes {}", tildify(p));
    }
    match primary_url {
        Some(url) => println!("{arrow} starts {service} on {url}"),
        None => println!("{arrow} starts {service}"),
    }
    println!();
}

/// Print a dry-run summary: files to write, then commands to run.
pub fn print_dry_run(steps: &[Step]) {
    enum FileEntry<'a> {
        Write(&'a ryra_core::generate::GeneratedFile),
        Copy {
            src: &'a std::path::Path,
            dst: &'a std::path::Path,
        },
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
                }
                FileEntry::Copy { src, dst } => {
                    println!("  {} (<- {})", dst.display(), src.display());
                }
            }
        }
        println!();
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
