//! `ryra account` — connect this machine to a ryra account on the control
//! plane (app.ryra.dev). Mirrors `gh auth`: `login` stores an API key,
//! `logout` drops it, `status` reports it. The key is what unlocks
//! ryra-managed backups.
//!
//! Headless by construction: `login` takes the key from `RYRA_TOKEN` or
//! `--with-token` (stdin) without a TTY, and only falls back to an
//! interactive hidden prompt when both stdin and stdout are terminals.

use std::io::{IsTerminal, Read};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ryra_core::system::account::{self, Credentials};

#[derive(Subcommand)]
pub enum AccountAction {
    /// Log in by storing a ryra API key (sk_ryra_orc_...).
    ///
    /// Generate a key in the dashboard, then paste it here. Interactive runs
    /// prompt for it (hidden); scripts pass it via RYRA_TOKEN or pipe it on
    /// stdin with --with-token.
    Login {
        /// Read the API key from stdin instead of prompting (for CI/scripts).
        #[arg(long)]
        with_token: bool,
    },
    /// Remove the stored API key from this machine.
    Logout,
    /// Show whether this machine is logged in, and to which control plane.
    Status,
}

pub async fn run(action: AccountAction) -> Result<()> {
    match action {
        AccountAction::Login { with_token } => login(with_token),
        AccountAction::Logout => logout(),
        AccountAction::Status => status(),
    }
}

fn login(with_token: bool) -> Result<()> {
    let token = resolve_login_token(with_token)?;
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
    println!("Logged in to {}.", account::api_base_url());
    Ok(())
}

/// Resolve the key to store. Priority: `RYRA_TOKEN` env, then `--with-token`
/// on stdin, then an interactive hidden prompt. A non-interactive shell with
/// none of the above fails loudly naming the flag, never hangs on stdin.
fn resolve_login_token(with_token: bool) -> Result<String> {
    if let Ok(t) = std::env::var("RYRA_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t);
    }
    if with_token {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading API key from stdin")?;
        return Ok(buf);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "no TTY and no key supplied: pipe the key on stdin with \
             `ryra account login --with-token`, or set RYRA_TOKEN. \
             Generate a key at {}/account.",
            account::api_base_url()
        );
    }
    let token = dialoguer::Password::new()
        .with_prompt(format!(
            "Paste your ryra API key (create one at {}/account)",
            account::api_base_url()
        ))
        .interact()
        .context("reading API key")?;
    Ok(token)
}

fn logout() -> Result<()> {
    if account::delete_credentials()? {
        println!("Logged out; removed the stored API key.");
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
        account::TokenSource::Env(_) => "RYRA_TOKEN (managed box / env)",
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
