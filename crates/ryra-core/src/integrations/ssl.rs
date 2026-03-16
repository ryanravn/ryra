use std::path::PathBuf;

use crate::config::schema::SslConfig;

/// Base directory for ryra-managed certs.
pub fn cert_dir() -> PathBuf {
    PathBuf::from("/etc/ryra/certs")
}

/// Cert paths for Cloudflare Proxy mode (self-signed origin cert).
pub fn origin_cert_paths(domain: &str) -> (PathBuf, PathBuf) {
    (
        cert_dir().join(domain).join("fullchain.pem"),
        cert_dir().join(domain).join("privkey.pem"),
    )
}

/// Cert paths for DnsOnly mode (Let's Encrypt).
pub fn letsencrypt_cert_paths(domain: &str) -> (PathBuf, PathBuf) {
    (
        cert_dir().join(domain).join("fullchain.pem"),
        cert_dir().join(domain).join("privkey.pem"),
    )
}

/// Cert paths for user-provided custom certs.
pub fn custom_cert_paths(custom_cert_dir: &str, domain: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(custom_cert_dir);
    (
        dir.join(domain).join("fullchain.pem"),
        dir.join(domain).join("privkey.pem"),
    )
}

/// Resolve cert + key paths for a domain based on SSL config.
/// Used for DnsOnly mode where SSL config determines cert type.
pub fn cert_paths_for_ssl(ssl: &SslConfig, domain: &str) -> (PathBuf, PathBuf) {
    match ssl {
        SslConfig::Letsencrypt { .. } => letsencrypt_cert_paths(domain),
        SslConfig::Custom { cert_dir } => custom_cert_paths(cert_dir, domain),
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
