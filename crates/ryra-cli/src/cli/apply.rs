use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use ryra_core::Step;

/// A concrete record of something we created in this run.
enum Created {
    User(String),
    File(PathBuf),
    Linger(String),
    StartedService { username: String, unit: String },
    StartedSystemService(String),
    DnsRecord { api_token: String, zone_id: String, domain: String },
}

/// Execute a list of steps with automatic rollback on failure.
pub async fn execute_all(steps: &[Step]) -> Result<()> {
    let mut created: Vec<Created> = Vec::new();

    for step in steps {
        match execute(step, &mut created).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("\nStep failed: {e}");
                prompt_rollback(&created).await;
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Prompt the user before rolling back. In non-interactive mode, skip rollback.
async fn prompt_rollback(created: &[Created]) {
    if created.is_empty() {
        return;
    }

    eprintln!("\n{} changes were made before the failure.", created.len());
    eprintln!("Rollback will attempt to undo them, but may not be perfect.\n");

    let should_rollback = if std::io::stdin().is_terminal() {
        dialoguer::Confirm::new()
            .with_prompt("Roll back changes?")
            .default(true)
            .interact()
            .unwrap_or(false)
    } else {
        eprintln!("Non-interactive mode — skipping rollback. Manual cleanup may be needed.");
        false
    };

    if !should_rollback {
        eprintln!("Skipping rollback. You may need to clean up manually.");
        return;
    }

    eprintln!("Rolling back...");
    for item in created.iter().rev() {
        let result = match item {
            Created::StartedService { username, unit } => {
                eprintln!("  Stopping {unit}...");
                run_quiet(&format!(
                    "sudo systemctl --machine={username}@ --user stop {unit}"
                ))
            }
            Created::StartedSystemService(unit) => {
                eprintln!("  Stopping {unit}...");
                run_quiet(&format!("sudo systemctl stop {unit}"))
            }
            Created::File(path) => {
                eprintln!("  Removing {}", path.display());
                run_quiet(&format!("sudo rm -f {}", path.display()))
            }
            Created::Linger(username) => {
                eprintln!("  Disabling linger for {username}...");
                run_quiet(&format!("sudo loginctl disable-linger {username}"))
            }
            Created::DnsRecord { api_token, zone_id, domain } => {
                eprintln!("  Deleting DNS record for {domain}...");
                let r = ryra_core::integrations::dns::find_record(api_token, zone_id, domain).await;
                if let Ok(Some(record)) = r {
                    let _ = ryra_core::integrations::dns::delete_record(api_token, zone_id, &record.id).await;
                }
                Ok(())
            }
            Created::User(username) => {
                eprintln!("  Removing user {username}...");
                run_quiet(&format!("sudo userdel --remove {username}"))
            }
        };
        if let Err(e) = result {
            eprintln!("  Rollback warning: {e}");
        }
    }
    eprintln!("Rollback complete.");
}

/// Execute a single step, recording what was created for rollback.
async fn execute(step: &Step, created: &mut Vec<Created>) -> Result<()> {
    match step {
        Step::CreateUser { username, home_dir } => {
            let exists = Command::new("id")
                .arg(username)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

            if exists {
                println!("  User {username} already exists, skipping");
                return Ok(());
            }

            run(&format!(
                "sudo useradd --system --shell /usr/sbin/nologin --home-dir {} --create-home {username}",
                home_dir.display()
            ))?;
            created.push(Created::User(username.clone()));
            Ok(())
        }
        Step::EnableLinger { username } => {
            run(&format!("sudo loginctl enable-linger {username}"))?;
            created.push(Created::Linger(username.clone()));
            Ok(())
        }
        Step::DisableLinger { username } => {
            // Ignore errors — user may not exist (partial add failure)
            let _ = run(&format!("sudo loginctl disable-linger {username}"));
            Ok(())
        }
        Step::TerminateUserSession { username } => {
            // Ignore errors — user may not have an active session
            let _ = run(&format!("sudo loginctl terminate-user {username}"));
            // Give systemd a moment to clean up
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(())
        }
        Step::WriteFile(file) => {
            println!("  Writing {}", file.path.display());
            if ryra_core::verbose::is_enabled() && !file.content.is_empty() {
                for line in file.content.lines() {
                    println!("    | {line}");
                }
                println!();
            }
            if let Some(parent) = file.path.parent() {
                run(&format!("sudo mkdir -p {}", parent.display()))?;
            }
            let mut child = Command::new("sudo")
                .args(["tee", &file.path.to_string_lossy()])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .context("failed to run sudo tee")?;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(file.content.as_bytes())?;
            }
            let status = child.wait()?;
            if !status.success() {
                bail!("failed to write {}", file.path.display());
            }
            created.push(Created::File(file.path.clone()));
            Ok(())
        }
        Step::Chown { path, username } => {
            run(&format!(
                "sudo chown -R {username}:{username} {}",
                path.display()
            ))
        }
        Step::DaemonReload { username } => {
            run(&format!(
                "sudo systemctl --machine={username}@ --user daemon-reload"
            ))
        }
        Step::StartService { username, unit } => {
            run(&format!(
                "sudo systemctl --machine={username}@ --user start {unit}"
            ))?;
            created.push(Created::StartedService {
                username: username.clone(),
                unit: unit.clone(),
            });
            Ok(())
        }
        Step::StopService { username, unit } => {
            let _ = run(&format!(
                "sudo systemctl --machine={username}@ --user stop {unit}"
            ));
            Ok(())
        }
        Step::SystemDaemonReload => run("sudo systemctl daemon-reload"),
        Step::SystemStart { unit } => {
            run(&format!("sudo systemctl start {unit}"))?;
            created.push(Created::StartedSystemService(unit.clone()));
            Ok(())
        }
        Step::SystemRestart { unit } => run(&format!("sudo systemctl restart {unit}")),
        Step::SystemStop { unit } => {
            let _ = run(&format!("sudo systemctl stop {unit}"));
            Ok(())
        }
        Step::StartTunnel => {
            println!("  Starting Cloudflare Tunnel...");
            run("sudo systemctl start cloudflared")?;
            created.push(Created::StartedSystemService("cloudflared".into()));
            Ok(())
        }
        Step::StopTunnel => {
            let _ = run("sudo systemctl stop cloudflared");
            Ok(())
        }
        Step::AddTunnelRoute {
            api_token,
            account_id,
            tunnel_id,
            zone_id,
            domain,
        } => {
            println!("  Adding tunnel route for {domain}...");

            // Get current ingress rules
            let mut rules = ryra_core::integrations::tunnel::get_tunnel_config(
                api_token, account_id, tunnel_id,
            )
            .await
            .unwrap_or_default();

            // Add the new route (pointing to nginx HTTP on localhost —
            // tunnel mode means nginx only listens on port 80)
            rules.push(ryra_core::integrations::tunnel::IngressRule {
                hostname: domain.clone(),
                service: "http://localhost:80".into(),
                path: None,
            });

            // Update tunnel config
            ryra_core::integrations::tunnel::update_tunnel_config(
                api_token, account_id, tunnel_id, &rules,
            )
            .await
            .context("failed to update tunnel ingress")?;

            // Create CNAME record
            ryra_core::integrations::tunnel::create_tunnel_dns(
                api_token, zone_id, domain, tunnel_id,
            )
            .await
            .context("failed to create tunnel CNAME")?;

            println!("  Tunnel route added: {domain} -> http://localhost:80");
            created.push(Created::DnsRecord {
                api_token: api_token.clone(),
                zone_id: zone_id.clone(),
                domain: domain.clone(),
            });
            Ok(())
        }
        Step::RemoveTunnelRoute {
            api_token,
            account_id,
            tunnel_id,
            zone_id,
            domain,
        } => {
            println!("  Removing tunnel route for {domain}...");

            // Get current ingress rules and filter out this domain
            let rules = ryra_core::integrations::tunnel::get_tunnel_config(
                api_token, account_id, tunnel_id,
            )
            .await
            .unwrap_or_default();

            let filtered: Vec<_> = rules
                .into_iter()
                .filter(|r| r.hostname != *domain)
                .collect();

            // Update tunnel config
            let _ = ryra_core::integrations::tunnel::update_tunnel_config(
                api_token, account_id, tunnel_id, &filtered,
            )
            .await;

            // Delete CNAME record
            if let Ok(Some(record)) = ryra_core::integrations::dns::find_record(api_token, zone_id, domain).await {
                let _ = ryra_core::integrations::dns::delete_record(
                    api_token, zone_id, &record.id,
                )
                .await;
            }

            println!("  Tunnel route removed for {domain}");
            Ok(())
        }
        Step::CreateDnsRecord {
            api_token,
            zone_id,
            domain,
            proxied,
        } => {
            use ryra_core::integrations::dns::{CreateRecordAction, CreateRecordMachine};

            let mode = match proxied {
                true => "proxied",
                false => "DNS-only",
            };
            println!("  Setting up DNS A record for {domain} ({mode})...");

            let mut machine = CreateRecordMachine::new(
                api_token.clone(),
                zone_id.clone(),
                domain.clone(),
                *proxied,
            );

            loop {
                match machine.advance().await? {
                    Some(CreateRecordAction::ResolveConflict { existing_ip }) => {
                        eprintln!("  Warning: A record already exists for {domain} -> {existing_ip}");
                        let overwrite = match std::io::stdin().is_terminal() {
                            true => dialoguer::Confirm::new()
                                .with_prompt("  Overwrite existing record?")
                                .default(false)
                                .interact()
                                .unwrap_or(false),
                            false => false,
                        };
                        match overwrite {
                            true => {
                                machine.confirm_overwrite();
                                println!("  Replacing existing record...");
                            }
                            false => {
                                return machine.abort().map_err(Into::into);
                            }
                        }
                    }
                    Some(CreateRecordAction::Created { ip }) => {
                        println!("  DNS record created: {domain} -> {ip}");
                        created.push(Created::DnsRecord {
                            api_token: api_token.clone(),
                            zone_id: zone_id.clone(),
                            domain: domain.clone(),
                        });
                        break;
                    }
                    None => break,
                }
            }
            Ok(())
        }
        Step::DeleteDnsRecord {
            api_token,
            zone_id,
            domain,
        } => {
            println!("  Deleting DNS record for {domain}...");
            match ryra_core::integrations::dns::find_record(api_token, zone_id, domain).await {
                Ok(Some(record)) => {
                    ryra_core::integrations::dns::delete_record(api_token, zone_id, &record.id)
                        .await
                        .context("failed to delete DNS record")?;
                    println!("  DNS record deleted");
                }
                Ok(None) => println!("  No DNS record found, skipping"),
                Err(e) => println!("  Warning: could not look up DNS record: {e}"),
            }
            Ok(())
        }
        Step::ObtainCert {
            domain,
            email,
            cloudflare_api_token,
        } => {
            println!("  Obtaining SSL certificate for {domain}...");
            let cert_dir = ryra_core::integrations::ssl::cert_dir();
            run(&format!("sudo mkdir -p {}/{domain}", cert_dir.display()))?;

            match cloudflare_api_token {
                Some(token) => {
                    // DNS-01 challenge via Cloudflare — run certbot in a container
                    run(&format!(
                        "sudo podman run --rm \
                         -v {cert_dir}:/etc/letsencrypt/live:Z \
                         -e CF_DNS_API_TOKEN={token} \
                         docker.io/certbot/dns-cloudflare:latest certonly \
                         --dns-cloudflare \
                         --dns-cloudflare-credentials /dev/null \
                         --dns-cloudflare-propagation-seconds 30 \
                         -d {domain} \
                         --email {email} \
                         --agree-tos --non-interactive \
                         --cert-path /etc/letsencrypt/live/{domain}/fullchain.pem \
                         --key-path /etc/letsencrypt/live/{domain}/privkey.pem",
                        cert_dir = cert_dir.display(),
                        token = token,
                        domain = domain,
                        email = email,
                    ))?;
                }
                None => {
                    // HTTP-01 challenge — nginx must be running on port 80
                    run(&format!(
                        "sudo podman run --rm \
                         -v {cert_dir}:/etc/letsencrypt/live:Z \
                         -p 80:80 \
                         docker.io/certbot/certbot:latest certonly \
                         --standalone \
                         -d {domain} \
                         --email {email} \
                         --agree-tos --non-interactive \
                         --cert-path /etc/letsencrypt/live/{domain}/fullchain.pem \
                         --key-path /etc/letsencrypt/live/{domain}/privkey.pem",
                        cert_dir = cert_dir.display(),
                        domain = domain,
                        email = email,
                    ))?;
                }
            }
            println!("  Certificate obtained for {domain}");
            Ok(())
        }
        Step::GenerateOriginCert { domain } => {
            println!("  Generating self-signed origin cert for {domain}...");
            let cmd = ryra_core::integrations::ssl::self_signed_cert_command(domain);
            run(&cmd)?;
            println!("  Origin cert generated for {domain}");
            Ok(())
        }
        Step::PullImage { image } => {
            println!("  Pulling {image}...");
            run(&format!("sudo podman pull {image}"))
        }
        Step::RemoveFile(path) => run(&format!("sudo rm -f {}", path.display())),
        Step::RemoveDir(path) => run(&format!("sudo rm -rf {}", path.display())),
        Step::RemoveUser { username } => {
            // Ignore errors — user may not exist (partial add failure)
            let _ = run(&format!("sudo userdel --remove {username}"));
            Ok(())
        }
    }
}

fn run(cmd: &str) -> Result<()> {
    println!("  $ {cmd}");
    let status = Command::new("sh")
        .args(["-c", cmd])
        .status()
        .with_context(|| format!("failed to run: {cmd}"))?;
    if !status.success() {
        bail!("command failed: {cmd}");
    }
    Ok(())
}

fn run_quiet(cmd: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to run: {cmd}"))?;
    if !status.success() {
        bail!("command failed: {cmd}");
    }
    Ok(())
}
