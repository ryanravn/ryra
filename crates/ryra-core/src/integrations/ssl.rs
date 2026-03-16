use std::path::PathBuf;

use crate::config::schema::SslConfig;

/// Base directory for ryra-managed certs.
pub fn cert_dir() -> PathBuf {
    PathBuf::from("/etc/ryra/certs")
}

/// Resolve cert + key paths for a domain based on SSL config.
pub fn cert_paths(ssl: &SslConfig, domain: &str) -> (PathBuf, PathBuf) {
    match ssl {
        SslConfig::Letsencrypt { .. } | SslConfig::CloudflareOrigin => (
            cert_dir().join(domain).join("fullchain.pem"),
            cert_dir().join(domain).join("privkey.pem"),
        ),
        SslConfig::Custom { cert_dir } => {
            let dir = PathBuf::from(cert_dir);
            (
                dir.join(domain).join("fullchain.pem"),
                dir.join(domain).join("privkey.pem"),
            )
        }
    }
}

/// Generate a self-signed origin certificate (for Cloudflare proxy mode).
/// Returns the openssl command to run.
pub fn self_signed_cert_command(domain: &str) -> String {
    let dir = cert_dir().join(domain);
    format!(
        "sudo mkdir -p {dir} && \
         sudo openssl req -x509 -nodes -days 3650 \
         -newkey rsa:2048 \
         -keyout {dir}/privkey.pem \
         -out {dir}/fullchain.pem \
         -subj '/CN={domain}'",
        dir = dir.display(),
        domain = domain,
    )
}
