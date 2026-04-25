//! Thin wrapper around the `tailscale` CLI.
//!
//! Used by preflight to verify a node is logged in before any
//! `--tailscale` install, and by the install path to look up the local
//! node's MagicDNS name when generating service URLs.
//!
//! Kept tiny on purpose — we don't want a Tailscale SDK dependency for
//! the few facts we read from `tailscale status --json`. The JSON parse
//! is hand-rolled to keep the dependency surface flat.

use std::process::Command;

/// Whether the `tailscale` CLI is on PATH and runnable.
pub fn cli_available() -> bool {
    Command::new("tailscale")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The local node's Tailscale MagicDNS name (e.g.
/// `debian.cobbler-tuna.ts.net`) if `tailscale` is installed and the
/// node is logged in. Returns `None` for any failure mode — the caller
/// (preflight) decides whether that's fatal.
pub fn self_dns_name() -> Option<String> {
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Self comes before Peer in the JSON, so the first DNSName is ours.
    // Parse manually to avoid pulling in a JSON crate just for this hint;
    // `tailscale status --json` pretty-prints, so we tolerate whitespace
    // between the key and its string value.
    let body = std::str::from_utf8(&out.stdout).ok()?;
    let after_key = body.split_once("\"DNSName\"")?.1;
    let after_colon = after_key
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let (value, _) = after_colon.split_once('"')?;
    let name = value.trim_end_matches('.');
    name.ends_with(".ts.net").then(|| name.to_string())
}
