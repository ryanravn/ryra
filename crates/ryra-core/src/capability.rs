//! Capabilities a service provides to other services.
//!
//! Dispatch sites that ask "is service X installed?" almost always actually
//! mean "is there an installed service that *plays role Y*?" — modeling
//! that question as a typed [`Capability`] lookup decouples integration
//! glue from hardcoded service names. New providers (a different reverse
//! proxy, a different OIDC IdP, an external SMTP relay) drop in without
//! the auth bridge / Caddy patcher / network-join logic having to learn
//! their names.
//!
//! Today the provider→capability mapping comes from
//! [`crate::WellKnownService::capabilities`] (a static map). Step 2 of
//! the migration moves the declaration into each service's `service.toml`
//! and persists it through `metadata.toml` so [`InstalledService`] can
//! report capabilities without core knowing the service name.

use crate::config::schema::InstalledService;

/// A role a service can play for other services. Pattern-match exhaustively
/// — adding a new variant forces every dispatch site to think about it.
///
/// Serializes as a kebab-case string so it round-trips cleanly through
/// `service.toml` (`provides = ["reverse-proxy", …]`) and through
/// `metadata.toml` (per-install snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// Terminates TLS and routes external hostnames to service containers.
    /// Today: Caddy. Future: nginx, traefik, etc.
    ReverseProxy,
    /// Issues OIDC tokens; ryra registers clients against it.
    /// Today: Authelia. Future: Pocket-ID, Authentik, Keycloak, …
    OidcProvider,
    /// Sits in front of services as Caddy `forward_auth` (cookie-based
    /// gate, no native OIDC in the protected service).
    ForwardAuthProvider,
    /// Accepts mail from services. Today: Inbucket (dev). Future: real
    /// MTA configurations.
    SmtpRelay,
    /// Scrapes and stores metrics; ryra drops file_sd target files into
    /// its `targets/` dir. Today: Prometheus. Future: VictoriaMetrics, …
    MetricsStore,
    /// Visualizes metrics from a [`Capability::MetricsStore`]; ryra
    /// provisions a datasource pointing at the installed store.
    /// Today: Grafana.
    MetricsDashboard,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReverseProxy => "reverse-proxy",
            Self::OidcProvider => "oidc-provider",
            Self::ForwardAuthProvider => "forward-auth-provider",
            Self::SmtpRelay => "smtp-relay",
            Self::MetricsStore => "metrics-store",
            Self::MetricsDashboard => "metrics-dashboard",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a service named `name` provides the given capability,
/// resolved by reading its `[capabilities] provides` declaration from
/// the cached default registry on disk.
///
/// Returns `false` if the default registry hasn't been cloned yet, the
/// service isn't in the default registry, or the file fails to parse —
/// capability dispatch on uninstalled, unknown names is not a query we
/// answer. Call sites that already hold a
/// [`crate::registry::service_def::ServiceDef`] should call
/// [`def_provides`] instead to skip the round-trip.
pub fn service_provides(name: &str, cap: Capability) -> bool {
    lookup_provides_from_registry(name)
        .map(|provides| provides.contains(&cap))
        .unwrap_or(false)
}

/// Capability list declared by a [`ServiceDef`]. Use this when the def
/// is already in scope (e.g. in `add_service` after `find_service`) —
/// it avoids the registry round-trip that [`service_provides`] does.
pub fn def_provides(def: &crate::registry::service_def::ServiceDef, cap: Capability) -> bool {
    def.capabilities.provides.contains(&cap)
}

/// Whether an `InstalledService` provides the given capability. Reads
/// from the persisted snapshot in `metadata.toml` (hydrated into
/// [`InstalledService::provides`] at [`crate::list_installed`] time).
pub fn installed_provides(svc: &InstalledService, cap: Capability) -> bool {
    svc.provides.contains(&cap)
}

/// Read `[capabilities] provides` for a service in the default registry.
///
/// Reads from the on-disk cache at `<cache>/default/<name>/service.toml`
/// (populated by the first `ryra add`/`ryra search`) or from the
/// `RYRA_REGISTRY_DIR` override directory. Returns `None` if the
/// registry hasn't been cloned yet, the service isn't in the default
/// registry, or the file fails to parse — capability dispatch on
/// uninstalled, unknown names is not a query we answer.
///
/// Intentionally sync: callers (e.g. `retroactive_network_joins`) run in
/// sync contexts and only need the cached snapshot, not a fresh git
/// clone. The first `ryra add` populates the cache, so by the time any
/// installed-services workflow asks "does X provide Y," the on-disk
/// registry directory is already there.
fn lookup_provides_from_registry(name: &str) -> Option<Vec<Capability>> {
    Some(lookup_registry_def(name)?.capabilities.provides)
}

/// Read a service's full definition from the cached default registry (or
/// the `RYRA_REGISTRY_DIR` override). Same availability caveats as
/// [`service_provides`]: `None` until the first `ryra add`/`ryra search`
/// has populated the cache. Used by dispatch sites that need more than
/// `provides` — e.g. the metrics bridge reading `[metrics]` and port
/// declarations of already-installed services.
pub(crate) fn lookup_registry_def(name: &str) -> Option<crate::registry::service_def::ServiceDef> {
    let paths = crate::config::ConfigPaths::resolve().ok()?;

    // Mirror resolve_default_registry_dir's env-override logic so an
    // RYRA_REGISTRY_DIR=/path/to/registry can serve capability lookups
    // before any clone has happened.
    let registry_dir = if let Ok(override_path) = std::env::var(crate::paths::REGISTRY_DIR_ENV)
        && let Some(p) = Some(std::path::PathBuf::from(override_path)).filter(|p| p.is_dir())
    {
        p
    } else {
        paths.cache_dir.join("default")
    };

    if !registry_dir.exists() {
        return None;
    }

    Some(crate::registry::find_service(&registry_dir, name).ok()?.def)
}

/// Find an installed service that provides the given capability. Returns
/// the first match — capabilities like [`Capability::ReverseProxy`] are
/// expected to have at most one provider installed at a time, but we
/// don't enforce that yet (a future "multiple OIDC providers" world is
/// the caller's problem to resolve).
pub fn find_installed_provider(
    installed: &[InstalledService],
    cap: Capability,
) -> Option<&InstalledService> {
    installed.iter().find(|s| installed_provides(s, cap))
}

/// Convenience: check live install state via [`crate::list_installed`]
/// for whether *any* provider of `cap` is currently installed. Use this
/// at planning sites that don't already have an `installed: &[…]` slice
/// in scope — anything inside [`crate::auth_bridge`] takes the slice as
/// a parameter and should call [`find_installed_provider`] instead.
pub fn any_installed_provider(cap: Capability) -> bool {
    crate::list_installed()
        .ok()
        .map(|installed| find_installed_provider(&installed, cap).is_some())
        .unwrap_or(false)
}
