pub mod add;
pub mod apply;
pub mod config_cmd;
pub mod diff;
pub mod init;
pub mod list;
pub mod prompts;
pub mod registry_cmd;
pub mod remove;
pub mod reset;
pub mod search;
pub mod status;
pub mod test;

use std::io::IsTerminal;

use ryra_core::Step;

/// Remove Caddy's CA certificate from system and browser trust stores.
/// Called during reset and when removing caddy.
pub fn remove_caddy_ca() {
    let mut sudo_commands = Vec::new();

    // System trust store (Fedora)
    if std::path::Path::new("/etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt").exists() {
        sudo_commands.push(
            "sudo rm -f /etc/pki/ca-trust/source/anchors/ryra-caddy-ca.crt && sudo update-ca-trust"
                .to_string(),
        );
    }
    // System trust store (Debian/Ubuntu)
    if std::path::Path::new("/usr/local/share/ca-certificates/ryra-caddy-ca.crt").exists() {
        sudo_commands.push(
            "sudo rm -f /usr/local/share/ca-certificates/ryra-caddy-ca.crt && sudo update-ca-certificates"
                .to_string(),
        );
    }

    // Browser trust store (NSS — Chromium/Brave/Chrome)
    let nssdb = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".pki/nssdb"))
        .filter(|p| p.exists());
    if let Some(ref nssdb_path) = nssdb {
        let in_nss = std::process::Command::new("certutil")
            .args(["-d", &format!("sql:{}", nssdb_path.display()), "-L", "-n", "ryra-caddy-ca"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if in_nss {
            let _ = std::process::Command::new("certutil")
                .args(["-d", &format!("sql:{}", nssdb_path.display()), "-D", "-n", "ryra-caddy-ca"])
                .status();
        }
    }

    if sudo_commands.is_empty() {
        return;
    }

    let interactive = std::io::stdin().is_terminal();
    println!("\n  Removing Caddy CA from system trust store:");
    for cmd in &sudo_commands {
        println!("    {cmd}");
    }
    if interactive {
        let run = dialoguer::Confirm::new()
            .with_prompt("  Run these commands now?")
            .default(true)
            .interact()
            .unwrap_or(false);
        if run {
            for cmd in &sudo_commands {
                let _ = std::process::Command::new("sh").args(["-c", cmd]).status();
            }
        }
    } else {
        for cmd in &sudo_commands {
            let _ = std::process::Command::new("sh").args(["-c", cmd]).status();
        }
    }
}

/// Print a dry-run summary: files to write, then commands to run.
pub fn print_dry_run(steps: &[Step]) {
    let verbose = ryra_core::verbose::is_enabled();

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
