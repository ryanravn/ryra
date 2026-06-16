//! The reverse of [`crate::from_protocol`]: turn the engine's `ops` request
//! types into the wire protocol's payloads. A client that builds `ops` requests
//! (e.g. ryra-api, which constructs them to authorize) uses these to send the
//! request over rpc. Field-by-field, so a shape change is a compile error.

use ryra_protocol as p;

use crate::Lifecycle;
use crate::RemoveMode;
use crate::configure::{ExposureChange, Overrides};
use crate::ops::{
    AddRequest, AuthRequested, ConfigureRequest, ExposureRequest, LifecycleRequest, RemoveRequest,
    UpgradeRequest,
};
use crate::registry::service_def::AuthKind;

impl From<ExposureRequest> for p::ExposureRequest {
    fn from(e: ExposureRequest) -> Self {
        match e {
            ExposureRequest::Loopback => p::ExposureRequest::Loopback,
            ExposureRequest::Url(u) => p::ExposureRequest::Url(u),
            ExposureRequest::Tailscale(u) => p::ExposureRequest::Tailscale(u),
        }
    }
}

impl From<AuthKind> for p::AuthKind {
    fn from(k: AuthKind) -> Self {
        match k {
            AuthKind::Oidc => p::AuthKind::Oidc,
        }
    }
}

impl From<AuthRequested> for p::AuthRequested {
    fn from(a: AuthRequested) -> Self {
        match a {
            AuthRequested::No => p::AuthRequested::No,
            AuthRequested::Yes => p::AuthRequested::Yes,
            AuthRequested::Kind(k) => p::AuthRequested::Kind(k.into()),
        }
    }
}

impl From<AddRequest> for p::AddRequest {
    fn from(r: AddRequest) -> Self {
        p::AddRequest {
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

impl From<RemoveMode> for p::RemoveMode {
    fn from(m: RemoveMode) -> Self {
        match m {
            RemoveMode::Preserve => p::RemoveMode::Preserve,
            RemoveMode::Purge => p::RemoveMode::Purge,
        }
    }
}

impl From<RemoveRequest> for p::RemoveRequest {
    fn from(r: RemoveRequest) -> Self {
        p::RemoveRequest {
            service: r.service,
            mode: r.mode.into(),
        }
    }
}

impl From<Lifecycle> for p::Lifecycle {
    fn from(l: Lifecycle) -> Self {
        match l {
            Lifecycle::Start => p::Lifecycle::Start,
            Lifecycle::Stop => p::Lifecycle::Stop,
        }
    }
}

impl From<LifecycleRequest> for p::LifecycleRequest {
    fn from(r: LifecycleRequest) -> Self {
        p::LifecycleRequest {
            service: r.service,
            action: r.action.into(),
        }
    }
}

impl From<UpgradeRequest> for p::UpgradeRequest {
    fn from(r: UpgradeRequest) -> Self {
        p::UpgradeRequest {
            service: r.service,
            force: r.force,
        }
    }
}

impl From<ExposureChange> for p::ExposureChange {
    fn from(e: ExposureChange) -> Self {
        match e {
            ExposureChange::Url(u) => p::ExposureChange::Url(u),
            ExposureChange::Tailscale(u) => p::ExposureChange::Tailscale(u),
            ExposureChange::Loopback => p::ExposureChange::Loopback,
        }
    }
}

impl From<Overrides> for p::Overrides {
    fn from(o: Overrides) -> Self {
        p::Overrides {
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

impl From<ConfigureRequest> for p::ConfigureRequest {
    fn from(r: ConfigureRequest) -> Self {
        p::ConfigureRequest {
            service: r.service,
            changes: r.changes.into(),
        }
    }
}
