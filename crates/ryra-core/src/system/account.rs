//! The ryra account: talking to the control plane (app.ryra.dev) and
//! persisting the account API key locally.
//!
//! Auth is a bearer API key (`sk_ryra_orc_...`) minted in the dashboard,
//! not an OAuth flow, so "login" is really "store and validate a key" the
//! way `gh auth login --with-token` does. The key is the same credential
//! that unlocks ryra-managed backups (a later step vends short-lived R2
//! storage creds against it).
//!
//! System-touching (network + a 0600 credential file), so it lives under
//! `system` rather than in the pure planner. HTTP goes through `curl` to
//! match the rest of the codebase (ryra carries no HTTP-client crate).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::ConfigPaths;

/// Default control-plane base URL. `RYRA_API_URL` overrides it (local dev
/// and E2E point at a throwaway orchestrator), mirroring how `RYRA_DATA_DIR`
/// / `RYRA_CONFIG_DIR` redirect the rest of ryra in tests.
const DEFAULT_API_URL: &str = "https://app.ryra.dev";

/// The control-plane base URL, with no trailing slash.
pub fn api_base_url() -> String {
    match std::env::var("RYRA_API_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().trim_end_matches('/').to_string(),
        _ => DEFAULT_API_URL.to_string(),
    }
}

/// The stored account credential. Persisted to `credentials.toml` next to
/// `preferences.toml`, 0600 (it is a bearer secret).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Bearer API key (`sk_ryra_orc_...`).
    pub token: String,
}

fn credentials_path() -> Result<PathBuf> {
    Ok(ConfigPaths::resolve()?.config_dir.join("credentials.toml"))
}

/// Load the stored credentials, or `None` if the user has not logged in.
pub fn load_credentials() -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let creds =
                toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(creds))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
    }
}

/// Where the active token came from. A managed box is provisioned with
/// `RYRA_TOKEN` in its env; a self-hoster stores one via `ryra account login`.
pub enum TokenSource {
    /// From the `RYRA_TOKEN` environment variable (managed box / CI).
    Env(String),
    /// From the stored credentials file (`ryra account login`).
    Stored(String),
}

impl TokenSource {
    pub fn token(&self) -> &str {
        match self {
            TokenSource::Env(t) | TokenSource::Stored(t) => t,
        }
    }
}

/// The token ryra should authenticate with. `RYRA_TOKEN` in the environment
/// (how a managed box is provisioned) wins over the stored credentials file
/// (how a self-hoster logs in). `None` if neither is set.
pub fn effective_token() -> Result<Option<TokenSource>> {
    if let Ok(t) = std::env::var("RYRA_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(Some(TokenSource::Env(t)));
        }
    }
    Ok(load_credentials()?.map(|c| TokenSource::Stored(c.token)))
}

/// Persist credentials at 0600. The directory is created if missing.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;
    let path = paths.config_dir.join("credentials.toml");
    let body = toml::to_string(creds).context("serializing credentials")?;
    crate::system::atomic_write::atomic_write(&path, body.as_bytes(), 0o600)?;
    Ok(())
}

/// Delete the stored credentials. Returns whether a file was actually removed
/// (so `logout` can tell the user "nothing to do" vs "done").
pub fn delete_credentials() -> Result<bool> {
    let path = credentials_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::new(e).context(format!("removing {}", path.display()))),
    }
}

/// One HTTP response: status code + body. Body may be empty.
struct ApiResponse {
    status: u16,
    body: String,
}

/// `curl` to the control plane with the bearer key. Distinguishes a transport
/// failure (DNS/TLS/offline: curl exits non-zero) from an HTTP error code
/// (curl succeeds, we read the status off `-w`).
fn curl(method: &str, path: &str, token: &str, body: Option<&str>) -> Result<ApiResponse> {
    let url = format!("{}{}", api_base_url(), path);
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-X", method])
        .arg("-H")
        .arg(format!("Authorization: Bearer {token}"))
        .arg("-H")
        .arg("Accept: application/json")
        .arg("-w")
        .arg("\n%{http_code}");
    if let Some(b) = body {
        cmd.args(["-H", "Content-Type: application/json", "--data-binary", b]);
    }
    cmd.arg(&url);
    let out = cmd
        .output()
        .with_context(|| format!("curl {method} {url}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("could not reach {url}: {}", err.trim());
    }
    let combined = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = combined
        .rsplit_once('\n')
        .ok_or_else(|| anyhow::anyhow!("malformed curl response from {url} (no status code)"))?;
    let status: u16 = code
        .trim()
        .parse()
        .with_context(|| format!("parsing HTTP status from {code:?}"))?;
    Ok(ApiResponse {
        status,
        body: body.to_string(),
    })
}

/// Validate a key against the control plane. `Ok(())` means the key is live
/// and accepted; errors name the likely cause (rejected vs unreachable).
///
/// Uses `GET /api/v1/plans`, an authenticated endpoint with no parameters, so
/// a 200 proves the key works without depending on any account-specific shape.
pub fn verify_token(token: &str) -> Result<()> {
    let resp = curl("GET", "/api/v1/plans", token, None)?;
    match resp.status {
        200 => Ok(()),
        401 | 403 => bail!(
            "the control plane rejected this API key (HTTP {}). \
             Generate a fresh key at {}/account.",
            resp.status,
            api_base_url()
        ),
        other => {
            let detail = resp.body.trim();
            if detail.is_empty() {
                bail!("unexpected response from the control plane: HTTP {other}");
            }
            bail!("unexpected response from the control plane: HTTP {other}: {detail}");
        }
    }
}

/// The account's managed-backup state, as the CLI needs it to decide what to do.
pub enum BackupState {
    /// No backup plan yet (the control plane returned 404).
    None,
    /// An active, paid plan.
    Active { used_bytes: i64, quota_bytes: i64 },
    /// A plan row exists but isn't active (e.g. `canceled`, `past_due`).
    Inactive(String),
}

/// Fetch the calling account's managed-backup state (`GET /api/v1/backup`).
pub fn backup_status(token: &str) -> Result<BackupState> {
    let resp = curl("GET", "/api/v1/backup", token, None)?;
    match resp.status {
        200 => {
            #[derive(Deserialize)]
            struct Body {
                status: String,
                used_bytes: i64,
                quota_bytes: i64,
            }
            let b: Body = serde_json::from_str(&resp.body).context("parsing backup status")?;
            if b.status == "active" {
                Ok(BackupState::Active {
                    used_bytes: b.used_bytes,
                    quota_bytes: b.quota_bytes,
                })
            } else {
                Ok(BackupState::Inactive(b.status))
            }
        }
        404 => Ok(BackupState::None),
        401 | 403 => bail!(
            "the control plane rejected this key (HTTP {}). Re-run `ryra account login`.",
            resp.status
        ),
        other => bail!("unexpected response from the control plane: HTTP {other}"),
    }
}

/// Start a managed-backup checkout for the calling account. Returns the URL to
/// open to subscribe (`POST /api/v1/billing/backup-checkout`).
pub fn backup_checkout(token: &str) -> Result<String> {
    let resp = curl("POST", "/api/v1/billing/backup-checkout", token, None)?;
    match resp.status {
        200 => {
            #[derive(Deserialize)]
            struct Body {
                url: String,
            }
            let b: Body = serde_json::from_str(&resp.body).context("parsing checkout response")?;
            Ok(b.url)
        }
        401 | 403 => bail!(
            "this key can't start a backup checkout: it needs an account-scoped key with \
             the billing.write scope. Generate one in the dashboard."
        ),
        409 => bail!("backups can't be purchased right now: {}", resp.body.trim()),
        other => bail!(
            "unexpected response from the control plane: HTTP {other}: {}",
            resp.body.trim()
        ),
    }
}
