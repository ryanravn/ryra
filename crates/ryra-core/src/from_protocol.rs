//! Convert the wire protocol's request payloads ([`ryra_protocol`]) into the
//! engine's `ops` request types. The protocol crate is deliberately
//! engine-free, so the mapping lives here, where both types are in scope. Every
//! field is mirrored by name, so a shape change on either side surfaces as a
//! compile error.

use ryra_protocol as p;

use crate::Lifecycle;
use crate::RemoveMode;
use crate::configure::{ExposureChange, Overrides};
use crate::ops::{
    AddRequest, AuthRequested, ConfigureRequest, ExposureRequest, LifecycleRequest, RemoveRequest,
    UpgradeRequest,
};
use crate::registry::service_def::AuthKind;

impl From<p::ExposureRequest> for ExposureRequest {
    fn from(e: p::ExposureRequest) -> Self {
        match e {
            p::ExposureRequest::Loopback => ExposureRequest::Loopback,
            p::ExposureRequest::Url(u) => ExposureRequest::Url(u),
            p::ExposureRequest::Tailscale(u) => ExposureRequest::Tailscale(u),
        }
    }
}

impl From<p::AuthKind> for AuthKind {
    fn from(k: p::AuthKind) -> Self {
        match k {
            p::AuthKind::Oidc => AuthKind::Oidc,
        }
    }
}

impl From<p::AuthRequested> for AuthRequested {
    fn from(a: p::AuthRequested) -> Self {
        match a {
            p::AuthRequested::No => AuthRequested::No,
            p::AuthRequested::Yes => AuthRequested::Yes,
            p::AuthRequested::Kind(k) => AuthRequested::Kind(k.into()),
        }
    }
}

impl From<p::AddRequest> for AddRequest {
    fn from(r: p::AddRequest) -> Self {
        AddRequest {
            service: r.service,
            exposure: r.exposure.into(),
            auth: r.auth.into(),
            smtp: r.smtp,
            backup: r.backup,
            env: r.env,
            enable_groups: r.enable_groups,
            choose: r.choose,
        }
    }
}

impl From<p::RemoveMode> for RemoveMode {
    fn from(m: p::RemoveMode) -> Self {
        match m {
            p::RemoveMode::Preserve => RemoveMode::Preserve,
            p::RemoveMode::Purge => RemoveMode::Purge,
        }
    }
}

impl From<p::RemoveRequest> for RemoveRequest {
    fn from(r: p::RemoveRequest) -> Self {
        RemoveRequest {
            service: r.service,
            mode: r.mode.into(),
        }
    }
}

impl From<p::Lifecycle> for Lifecycle {
    fn from(l: p::Lifecycle) -> Self {
        match l {
            p::Lifecycle::Start => Lifecycle::Start,
            p::Lifecycle::Stop => Lifecycle::Stop,
        }
    }
}

impl From<p::LifecycleRequest> for LifecycleRequest {
    fn from(r: p::LifecycleRequest) -> Self {
        LifecycleRequest {
            service: r.service,
            action: r.action.into(),
        }
    }
}

impl From<p::UpgradeRequest> for UpgradeRequest {
    fn from(r: p::UpgradeRequest) -> Self {
        UpgradeRequest {
            service: r.service,
            force: r.force,
        }
    }
}

impl From<p::ExposureChange> for ExposureChange {
    fn from(e: p::ExposureChange) -> Self {
        match e {
            p::ExposureChange::Url(u) => ExposureChange::Url(u),
            p::ExposureChange::Tailscale(u) => ExposureChange::Tailscale(u),
            p::ExposureChange::Loopback => ExposureChange::Loopback,
        }
    }
}

impl From<p::Overrides> for Overrides {
    fn from(o: p::Overrides) -> Self {
        Overrides {
            exposure: o.exposure.map(Into::into),
            smtp: o.smtp,
            backup: o.backup,
            auth: o.auth,
            enable_groups: o.enable_groups,
            disable_groups: o.disable_groups,
            choose: o.choose,
            env_overrides: o.env_overrides,
            reassert_auth: o.reassert_auth,
        }
    }
}

impl From<p::ConfigureRequest> for ConfigureRequest {
    fn from(r: p::ConfigureRequest) -> Self {
        ConfigureRequest {
            service: r.service,
            changes: r.changes.into(),
        }
    }
}
