//! `ryra account`: log in to a ryra account on the control plane
//! (app.ryra.dev). Mirrors `gh auth`: `login` stores an API key,
//! `logout` drops it, `status` reports it. The key is what unlocks
//! ryra-managed backups.
//!
//! Headless by construction: `login` takes the key from `RYRA_TOKEN` or
//! `--with-token` (stdin) without a TTY. Interactive runs instead lead with a
//! browser device-authorization flow (no key pasting); the scripted token
//! paths stay as the fallback.

use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use console::style;
use ryra_core::system::account::{self, Credentials, DevicePoll};

#[derive(Subcommand)]
pub enum AccountAction {
    /// Log in to your ryra account.
    ///
    /// Interactive runs open a browser to approve this box (no key to paste).
    /// Scripts pass a key via RYRA_TOKEN or pipe it on stdin with --with-token.
    Login {
        /// Read the API key from stdin instead of the browser flow (for CI/scripts).
        #[arg(long)]
        with_token: bool,
    },
    /// Remove the stored API key.
    Logout,
    /// Show whether you're logged in, and to which control plane.
    Status,
}

pub async fn run(action: AccountAction) -> Result<()> {
    match action {
        AccountAction::Login { with_token } => login(with_token).await,
        AccountAction::Logout => logout(),
        AccountAction::Status => status(),
    }
}

async fn login(with_token: bool) -> Result<()> {
    // Scripted path (unchanged): an explicit key from RYRA_TOKEN or stdin wins.
    if let Some(token) = scripted_login_token(with_token)? {
        let token = token.trim().to_string();
        if token.is_empty() {
            bail!("no API key provided");
        }
        // Validate before persisting so we never store a dead key (and so the
        // user gets immediate feedback that it works).
        account::verify_token(&token).context("validating the API key with the control plane")?;
        account::save_credentials(&Credentials {
            token: token.clone(),
        })?;
        println!("Connected to {}.", account::api_base_url());
        return Ok(());
    }

    // No scripted key. The device flow needs a human at the browser, so it only
    // runs when both stdin and stdout are TTYs. Otherwise fail loudly, naming
    // the scripted inputs, exactly as before (never hang on stdin).
    if !super::is_interactive() {
        bail!(
            "no TTY and no key supplied: pipe the key on stdin with \
             `ryra account login --with-token`, or set RYRA_TOKEN. \
             Generate a key at {}/account.",
            account::api_base_url()
        );
    }

    device_login().await
}

/// The scripted key, if one is supplied: `RYRA_TOKEN` env, or `--with-token` on
/// stdin. `None` means "no scripted key" (the caller falls back to the device
/// flow). Never prompts and never blocks on stdin unless `--with-token`.
fn scripted_login_token(with_token: bool) -> Result<Option<String>> {
    if let Ok(t) = std::env::var("RYRA_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(Some(t));
    }
    if with_token {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading API key from stdin")?;
        return Ok(Some(buf));
    }
    Ok(None)
}

/// The browser device-authorization flow: start a request, show the user where
/// to approve it (with a best-effort auto-open), then poll until approved.
/// Reused by `ryra backup` to offer an inline login when picking managed backups.
pub(crate) async fn device_login() -> Result<()> {
    let base = account::api_base_url();
    let label = box_label();
    let start = account::device_start(&label).context("starting the device login")?;

    println!("To connect this machine to {base}, approve it in your browser:");
    println!();
    println!(
        "  {} {}",
        super::style::arrow(),
        style(&start.verification_uri_complete).cyan().bold()
    );
    println!(
        "  or go to {} and enter the code: {}",
        start.verification_uri,
        style(&start.user_code).bold()
    );
    println!();

    // Best-effort: open the one-click URL. The printed URL is the fallback for
    // headless SSH boxes, so a failure to open is never fatal.
    open_browser(&start.verification_uri_complete);

    // Poll until a terminal status, bounded by `expires_in` so we can never
    // loop forever even if the server keeps saying "pending".
    let interval = Duration::from_secs(start.interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(start.expires_in.max(1));
    print!("Waiting for approval");
    let _ = std::io::stdout().flush();
    loop {
        if Instant::now() >= deadline {
            println!();
            bail!("the login request expired before it was approved; re-run `ryra account login`");
        }
        match account::device_poll(&start.device_code).context("checking the device login")? {
            DevicePoll::Pending => {
                print!(".");
                let _ = std::io::stdout().flush();
                tokio::time::sleep(interval).await;
            }
            DevicePoll::Approved(key) => {
                println!();
                account::save_credentials(&Credentials { token: key })?;
                println!("Connected to {base}.");
                return Ok(());
            }
            DevicePoll::Denied => {
                println!();
                bail!("the login was denied in the browser");
            }
            DevicePoll::Expired => {
                println!();
                bail!("the login request expired; re-run `ryra account login`");
            }
        }
    }
}

/// A human-readable label for this box, shown in the approval UI. Prefers an
/// explicit `RYRA_BOX_LABEL`, then the system hostname (`HOSTNAME` env or the
/// `hostname` command, no new dependency), then a sane default.
fn box_label() -> String {
    if let Ok(v) = std::env::var("RYRA_BOX_LABEL")
        && !v.trim().is_empty()
    {
        return v.trim().to_string();
    }
    if let Ok(v) = std::env::var("HOSTNAME")
        && !v.trim().is_empty()
    {
        return v.trim().to_string();
    }
    if let Ok(out) = Command::new("hostname").output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    "a ryra machine".to_string()
}

/// Best-effort open `url` in the user's browser. No crate dependency: shell out
/// to the platform opener. Never fails the flow if it can't open (the printed
/// URL is the fallback for headless boxes).
pub(crate) fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // Detach stdio so the opener can't steal the terminal or block our poll.
    let _ = Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn logout() -> Result<()> {
    // Revoke server-side first so the machine disappears from "Connected machines" and
    // its key stops working. Best-effort: if the control plane is unreachable we
    // warn and still clear the local key, so logout works offline.
    let revoked = match account::revoke_stored_key() {
        Ok(sent) => sent,
        Err(e) => {
            eprintln!(
                "warning: couldn't revoke the key on the control plane ({e:#}); \
                 removing it locally anyway. Revoke it from Settings if needed."
            );
            false
        }
    };
    if account::delete_credentials()? {
        if revoked {
            println!(
                "Logged out; revoked the API key on the control plane and removed it locally."
            );
        } else {
            println!("Logged out; removed the stored API key.");
        }
    } else {
        println!("Not logged in; nothing to remove.");
    }
    Ok(())
}

fn status() -> Result<()> {
    let base = account::api_base_url();
    let Some(src) = account::effective_token()? else {
        println!("Not logged in.");
        println!("Run `ryra account login` to connect to {base}.");
        return Ok(());
    };
    // A managed box is authenticated via RYRA_TOKEN in its env; a self-hoster
    // via the stored credentials file. Name the source so the two are legible.
    let origin = match src {
        account::TokenSource::Env(_) => "RYRA_TOKEN (managed machine / env)",
        account::TokenSource::Stored(_) => "stored credentials",
    };
    // Live-check the key so status reflects reality, not just "a token exists".
    match account::verify_token(src.token()) {
        Ok(()) => {
            println!("Logged in to {base} via {origin}.");
            println!("  API key: {}", mask_token(src.token()));
        }
        Err(e) => {
            println!(
                "Logged in to {base} via {origin} (key {}), but it could not be verified:",
                mask_token(src.token())
            );
            println!("  {e:#}");
            if matches!(src, account::TokenSource::Stored(_)) {
                println!("  Run `ryra account login` to refresh it.");
            }
        }
    }
    Ok(())
}

/// Show enough of the key to identify which one it is without leaking it.
fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return "****".to_string();
    }
    let head: String = chars[..7].iter().collect();
    let tail: String = chars[n - 4..].iter().collect();
    format!("{head}...{tail}")
}
