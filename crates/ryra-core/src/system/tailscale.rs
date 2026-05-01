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

/// Strip the host portion from a MagicDNS name, leaving the tailnet
/// suffix that other devices on the same tailnet share.
///
/// `debian.cobbler-tuna.ts.net` → `Some("cobbler-tuna.ts.net")`. Used
/// to derive per-service hostnames: each ryra-managed service runs its
/// own tailscaled with hostname `<service>`, so its full URL is
/// `https://<service>.<tailnet_suffix>/`.
pub fn tailnet_suffix(node_dns_name: &str) -> Option<String> {
    let lower = node_dns_name.to_ascii_lowercase();
    if !lower.ends_with(".ts.net") {
        return None;
    }
    // Strip everything up to and including the first dot — the rest is
    // <tailnet>.ts.net. Reject single-label names (no tailnet portion).
    lower.split_once('.').map(|(_, rest)| rest.to_string())
}

/// The local node's short hostname as Tailscale knows it (e.g.
/// `debian` from `debian.cobbler-tuna.ts.net`). Used to scope per-host
/// Tailscale Service names — `svc:vikunja-debian` instead of bare
/// `svc:vikunja` — so two ryra hosts on the same tailnet can each run
/// their own copy of a service without colliding on the global svc
/// namespace, and `ryra reset` on one host doesn't tear down the
/// other's registration.
///
/// Tailscale already enforces uniqueness across the tailnet
/// (duplicates get `-1`/`-2` suffixes), so the resulting svc name is
/// guaranteed unique by construction.
pub fn self_short_hostname() -> Option<String> {
    self_dns_name().and_then(|name| name.split_once('.').map(|(host, _)| host.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_suffix_strips_host_from_three_label_name() {
        assert_eq!(
            tailnet_suffix("debian.cobbler-tuna.ts.net"),
            Some("cobbler-tuna.ts.net".into())
        );
    }

    #[test]
    fn tailnet_suffix_lowercases_input() {
        // tailscale's JSON sometimes returns the name with caps; we need
        // the suffix in canonical form for URL templating.
        assert_eq!(
            tailnet_suffix("HOST.COBBLER-TUNA.TS.NET"),
            Some("cobbler-tuna.ts.net".into())
        );
    }

    #[test]
    fn tailnet_suffix_rejects_non_ts_net() {
        assert_eq!(tailnet_suffix("debian.example.com"), None);
        assert_eq!(tailnet_suffix("not-a-dns-name"), None);
    }

    #[test]
    fn tailnet_suffix_rejects_bare_ts_net() {
        // `ts.net` itself doesn't end with `.ts.net` (no leading dot),
        // so our suffix check rejects it. tailscale never emits this
        // anyway — it's always a three-label MagicDNS name — but
        // documenting the boundary keeps the contract clear.
        assert_eq!(tailnet_suffix("ts.net"), None);
    }
}
