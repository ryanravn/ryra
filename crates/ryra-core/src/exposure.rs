//! How a service is exposed to clients. Decided once at install time
//! (`--url`, `--tailscale`, the interactive prompt, or auto-derived) and
//! threaded through the planner so every code path that needs to know
//! "where does this service live?" pattern-matches a single typed value
//! instead of juggling a parallel `(Option<url>, bool)` pair where some
//! combinations were silently invalid (e.g. a `*.ts.net` URL with
//! `tailscale_enabled = false`).

/// True if the URL's host is a Tailscale MagicDNS name (`*.ts.net`). When
/// this matches, ryra skips the dances it does for `.internal` (Caddy route,
/// `/etc/hosts` entry, local CA trust) — Tailscale's tunnel already provides
/// routing, DNS, and encryption. Templates still populate normally so
/// service-specific config (trusted_domains, OIDC callbacks) picks up the
/// Tailscale hostname.
pub fn is_tailscale_url(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h.ends_with(".ts.net"))
}

/// Serialized form on disk uses an internal `kind` tag so the variant
/// is explicit in the TOML — no guessing whether `url = "foo.ts.net"`
/// implies Tailscale-mode or not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Exposure {
    /// No public-facing URL. Service runs on `http://127.0.0.1:<port>`
    /// only — reachable from the host but nothing routes external
    /// traffic to it. Used for services that don't need a domain
    /// (e.g. inbucket when the user only hits it via localhost).
    Loopback,
    /// LAN-only via Caddy at a `*.internal` hostname. Self-signed
    /// certs from Caddy's internal CA — useful on a single machine
    /// where the user has imported ryra's CA into their browser.
    Internal { url: String },
    /// Exposed on the user's tailnet at `<service>.<tailnet>.ts.net`
    /// via `tailscale serve` on the host. Real cert from the
    /// Tailscale-managed CA, no Caddyfile entry needed.
    Tailscale { url: String },
    /// A public hostname. Caddy is the reverse proxy when installed
    /// (LE or self-signed depending on `tls.caddy`); without Caddy,
    /// the user is fronting with their own proxy (Cloudflare Tunnel,
    /// nginx, etc.) and ryra leaves routing alone.
    Public { url: String },
}

impl Exposure {
    /// Browser-visible URL, if any. Convenient when something
    /// downstream is OK with `Option<&str>` (template context, OIDC
    /// redirects) and doesn't care about the routing variant.
    pub fn url(&self) -> Option<&str> {
        match self {
            Exposure::Loopback => None,
            Exposure::Internal { url }
            | Exposure::Tailscale { url }
            | Exposure::Public { url } => Some(url),
        }
    }

    /// True when this exposure is reached via `tailscale serve` instead
    /// of Caddy. Used to skip Caddyfile routes for `*.ts.net` URLs.
    pub fn is_tailscale(&self) -> bool {
        matches!(self, Exposure::Tailscale { .. })
    }

    /// For Tailscale exposures, the Tailscale Service name (the part
    /// after `svc:` — i.e. the first DNS label of the URL host). With
    /// per-host scoping this is `<service>-<host>` (e.g.
    /// `vikunja-debian`). Used by remove/reset paths to address the
    /// admin-API service definition without re-deriving the host from
    /// `tailscale status` (the URL was captured at install time, so
    /// renaming the host post-install doesn't break teardown).
    pub fn tailscale_svc_name(&self) -> Option<String> {
        let url = match self {
            Exposure::Tailscale { url } => url,
            _ => return None,
        };
        url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .and_then(|h| h.split_once('.').map(|(label, _)| label.to_string()))
    }

    /// Stable string form of the variant, for the `exposure` field in
    /// `metadata.toml` and reading it back. Mirrors the snake_case names
    /// used by serde's `tag = "kind"` representation so the two stay in
    /// lockstep.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Exposure::Loopback => "loopback",
            Exposure::Internal { .. } => "internal",
            Exposure::Tailscale { .. } => "tailscale",
            Exposure::Public { .. } => "public",
        }
    }

    /// Classify a user-supplied URL string into the corresponding
    /// Exposure variant. `*.internal` → `Internal`, `*.ts.net` →
    /// `Tailscale`, anything else → `Public`. Used by the CLI when a
    /// raw `--url <X>` flag is passed.
    pub fn from_url(url: &str) -> Self {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
        match host.as_deref() {
            Some(h) if h.ends_with(".internal") => Exposure::Internal {
                url: url.to_string(),
            },
            Some(h) if h.ends_with(".ts.net") => Exposure::Tailscale {
                url: url.to_string(),
            },
            _ => Exposure::Public {
                url: url.to_string(),
            },
        }
    }
}

/// True when the URL's host is publicly resolvable — i.e. something a
/// browser on the open internet would expect to reach. Used by the CLI
/// to decide whether to surface the Let's Encrypt prompt.
///
/// False for hosts that are LAN/loopback/tailnet by construction:
/// `*.internal`, `*.localhost`, `*.local`, the bare `localhost`,
/// `*.ts.net`, and any literal IP address.
pub fn is_public_url(url: &str) -> bool {
    let Some(host) = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
    else {
        return false;
    };
    // url::Url wraps IPv6 hosts in `[ ]`; strip them before the IpAddr parse.
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&host);
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    if host == "localhost" {
        return false;
    }
    !(host.ends_with(".internal")
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".ts.net"))
}
