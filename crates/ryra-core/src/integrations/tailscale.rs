use std::path::PathBuf;

use crate::integrations::ssl;

/// Detect the Tailscale FQDN by parsing `tailscale status --json`.
/// Returns `None` if Tailscale is not running or not configured.
pub fn detect_fqdn() -> Option<String> {
    let output = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    // Prefer CertDomains[0] — the domain tailscale cert will issue for.
    if let Some(domains) = json.get("CertDomains").and_then(|d| d.as_array())
        && let Some(first) = domains.first().and_then(|d| d.as_str())
        && !first.is_empty()
    {
        return Some(first.to_string());
    }

    // Fallback: Self.DNSName (has trailing dot)
    if let Some(self_obj) = json.get("Self")
        && let Some(dns_name) = self_obj.get("DNSName").and_then(|d| d.as_str())
    {
        let name = dns_name.trim_end_matches('.');
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }

    None
}

/// Cert paths for Tailscale mode. Uses the standard ryra cert directory.
pub fn cert_paths(fqdn: &str) -> (PathBuf, PathBuf) {
    (
        ssl::cert_dir().join(fqdn).join("fullchain.pem"),
        ssl::cert_dir().join(fqdn).join("privkey.pem"),
    )
}

/// Build the `tailscale cert` command that writes certs to ryra's cert dir.
pub fn cert_command(fqdn: &str) -> String {
    let dir = ssl::cert_dir().join(fqdn);
    format!(
        "sudo mkdir -p {dir} && \
         sudo tailscale cert \
         --cert-file={dir}/fullchain.pem \
         --key-file={dir}/privkey.pem \
         {fqdn}",
        dir = dir.display(),
        fqdn = fqdn,
    )
}
