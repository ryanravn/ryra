//! Infrastructure services ryra knows about for cross-service integration
//! (joining networks, configuring OIDC, setting up TLS).
//!
//! Using an enum instead of string constants makes comparisons type-safe
//! and ensures the compiler catches typos or missing match arms.
//!
//! Capability data is no longer here — every provider declares
//! `[capabilities] provides = [...]` in its own `service.toml`, and
//! installed services carry the persisted snapshot on
//! [`crate::config::schema::InstalledService::provides`]. This enum
//! survives only as a typed handle to the default-registry providers' *names*,
//! used by code paths that emit network names (`<svc>.network`),
//! seed provider-specific config files (caddy's `tls.caddy`), or read
//! their on-disk artifacts (authelia's `.env`).

use crate::config::schema::Config;

/// Default Caddy HTTPS port, used when the caddy service record has no
/// "https" port entry (e.g., config was written by an older version).
const DEFAULT_CADDY_HTTPS_PORT: u16 = 8443;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WellKnownService {
    Caddy,
    Authelia,
    Inbucket,
}

impl WellKnownService {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caddy => "caddy",
            Self::Authelia => "authelia",
            Self::Inbucket => "inbucket",
        }
    }

    /// Try to match a service name to a well-known service.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "caddy" => Some(Self::Caddy),
            "authelia" => Some(Self::Authelia),
            "inbucket" => Some(Self::Inbucket),
            _ => None,
        }
    }

    /// Check if a string matches this well-known service name.
    pub fn matches(&self, name: &str) -> bool {
        self.as_str() == name
    }
}

impl std::fmt::Display for WellKnownService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Look up Caddy's HTTPS port. Reads from the quadlet scan (which
/// reconstructs ports from the service's `.env` file), falling back
/// to the default when caddy isn't installed yet.
pub(crate) fn caddy_https_port(_config: &Config) -> u16 {
    crate::list_installed()
        .unwrap_or_default()
        .into_iter()
        .find(|s| WellKnownService::Caddy.matches(&s.name))
        .and_then(|s| s.ports.get("https").copied())
        .unwrap_or(DEFAULT_CADDY_HTTPS_PORT)
}
