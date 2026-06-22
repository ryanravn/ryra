//! `ryra config <service>` — re-plan an installed service with a
//! caller-supplied set of [`Overrides`] applied on top of its current
//! recorded state.
//!
//! The render path is shared with [`crate::add_service`] (driven via
//! [`crate::PlanMode::Upgrade`] so the install-time rejects don't fire and
//! the install-only side effects stay quiet). What's new here is the
//! *state recovery*, *change classification*, and *cross-service
//! lifecycle handling*:
//!
//! 1. Load `metadata.toml` + the existing `.env` and compute the current
//!    configuration (`enable_auth`, `enable_smtp`, `enable_backup`,
//!    `enabled_groups`, exposure, per-secret values).
//! 2. Apply [`Overrides`] to produce the *target* configuration.
//! 3. Re-call `add_service` in upgrade mode with the target values, passing
//!    the existing secrets through `pre_built_ctx` so `secret.*` and
//!    `auth.*` are preserved verbatim (a freshly-rotated JWT key would
//!    invalidate every active session). When auth is *being enabled*,
//!    fresh `auth.client_id` / `auth.client_secret` are minted here and
//!    seeded into `pre_built_ctx` so the same values flow into both the
//!    rendered `.env` and the `register_oidc_client` step.
//! 4. Diff the plan vs. on-disk state (reusing the upgrade diff machinery)
//!    and classify the high-level changes via [`ConfigureChange`] so the
//!    CLI can render them with the right colour and gate destructive
//!    transitions behind explicit confirmation.
//! 5. Emit lifecycle side-steps the install path normally handles only at
//!    `PlanMode::Add` time:
//!    - Authelia OIDC client `register` / `unregister` when `--auth` is
//!      flipped or the URL changes on an auth-enabled service (the
//!      `redirect_uri` is pinned to the URL at registration time).
//!    - `TailscaleEnable` / `TailscaleDisable` when the exposure crosses
//!      the loopback / URL / tailscale boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::exposure::Exposure;
use crate::generate::GeneratedFile;
use crate::metadata::load_metadata;
use crate::registry::resolve::ServiceRef;
use crate::registry::service_def::{AuthKind, EnvFormat, EnvKind};
use crate::system::secret;
use crate::upgrade::{DiffEntry, DiffKind, DiffResult, EnvAddition};
use crate::{
    AddResult, PlanMode, REGISTRY_DEFAULT, Step, WellKnownService, add_service, authelia, caddy,
    is_service_installed, list_installed, manifest, quadlet_dir, registry, resolve_registry_dir,
    service_home,
};

/// What the caller wants to change. Every field is "leave alone" by default;
/// `Some(_)` means "set to this value." Two-sided enums (e.g.
/// [`ExposureChange`]) make "remove" representable without overloading
/// `Some("")` with a sentinel meaning.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Overrides {
    /// Change the service's exposure: a public URL, a Tailscale Service,
    /// or loopback-only. `None` leaves the current exposure alone.
    pub exposure: Option<ExposureChange>,
    /// Flip per-service SMTP wiring. The global SMTP config is unchanged
    /// either way.
    pub smtp: Option<bool>,
    /// Flip the backup inclusion flag for this install.
    pub backup: Option<bool>,
    /// Flip OIDC auth wiring. `Some(true)` registers an OIDC client with
    /// the installed auth provider and adds OIDC env vars; `Some(false)`
    /// unregisters and strips them.
    pub auth: Option<bool>,
    /// Env-group names to turn ON (members land in `.env`).
    pub enable_groups: BTreeSet<String>,
    /// Env-group names to turn OFF (members drop out of `.env`).
    pub disable_groups: BTreeSet<String>,
    /// `[[choice]]` selections to change (`choice name -> option name`).
    /// Choices not listed keep their recorded selection.
    pub choose: BTreeMap<String, String>,
    /// Raw per-env overrides applied during render. Useful for changing
    /// the value of a `prompted` env var (e.g. an admin email) without
    /// touching anything else.
    pub env_overrides: BTreeMap<String, String>,
    /// Re-register this service's OIDC client with the auth provider even
    /// though auth is already on and the URL hasn't changed. Reuses the
    /// `client_id`/`client_secret` already in the service's `.env` (no
    /// rotation). Used to repair a provider/consumer desync, e.g. after a
    /// `ryra backup restore` of authelia dropped the client.
    pub reassert_auth: bool,
}

/// Exposure transition. `Loopback` means "no public route" (the install's
/// equivalent of dropping `--url` and `--tailscale`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureChange {
    /// Caddy-routed public URL. Internal vs. Public is auto-classified
    /// from the hostname by [`Exposure::from_url`].
    Url(String),
    /// Tailscale Service exposure. The caller must pre-derive the URL
    /// from the system's tailnet identity (`<svc>-<host>.<tailnet>`).
    Tailscale(String),
    /// Loopback-only (no Caddy route, no Tailscale Service).
    Loopback,
}

/// A single high-level change the configure run will apply. The CLI uses
/// this for the summary banner; `is_destructive` decides whether the
/// change requires the user to type the service name to confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigureChange {
    /// URL changed (or added, or removed). Covers all exposure
    /// transitions: tailscale-on shows as `Url { to: Some(ts_url) }`.
    Url {
        from: Option<String>,
        to: Option<String>,
    },
    /// Per-service SMTP wiring toggled.
    Smtp { from: bool, to: bool },
    /// Backup inclusion flag flipped.
    Backup { from: bool, to: bool },
    /// OIDC auth wiring toggled.
    Auth { from: bool, to: bool },
    /// An env-group bundle was switched on (members appended to `.env`).
    GroupEnabled(String),
    /// An env-group bundle was switched off (members removed from `.env`).
    GroupDisabled(String),
    /// A single env var's value was overridden by the user.
    EnvOverride {
        key: String,
        from: Option<String>,
        to: String,
    },
}

impl ConfigureChange {
    /// True when applying the change would invalidate state the user might
    /// depend on. The CLI gates these behind explicit confirmation.
    ///
    /// - Removing or changing the URL detaches the existing Caddy route
    ///   (and breaks any OAuth callback configured at the old hostname).
    /// - Disabling auth removes the OIDC client and SSO env vars; users
    ///   who logged in via SSO can no longer reach the service that way.
    /// - Disabling SMTP cuts off outbound mail for the service.
    /// - Disabling backup means future `ryra backup run` calls skip this
    ///   install — historical snapshots are kept on the restic repo.
    /// - Disabling a group drops env vars the service had access to;
    ///   features depending on them stop working until re-enabled.
    pub fn is_destructive(&self) -> bool {
        match self {
            ConfigureChange::Url { from, to } => from.is_some() && from != to,
            ConfigureChange::Smtp {
                from: true,
                to: false,
            } => true,
            ConfigureChange::Backup {
                from: true,
                to: false,
            } => true,
            ConfigureChange::Auth {
                from: true,
                to: false,
            } => true,
            ConfigureChange::GroupDisabled(_) => true,
            ConfigureChange::Smtp { .. } => false,
            ConfigureChange::Backup { .. } => false,
            ConfigureChange::Auth { .. } => false,
            ConfigureChange::GroupEnabled(_) => false,
            ConfigureChange::EnvOverride { .. } => false,
        }
    }
}

/// Output of [`configure_service`].
pub struct ConfigureResult {
    pub service: String,
    /// High-level transitions, in a stable order — the CLI walks this for
    /// the human-readable summary and the destructive-change gate.
    pub changes: Vec<ConfigureChange>,
    /// File-level diff from the upgrade machinery. Empty `entries` means
    /// no files differ from the current install (only metadata-level
    /// changes like `--backup` might still be in `changes`).
    pub diff: DiffResult,
    /// Steps to execute. Empty when neither files nor metadata would
    /// change (no-op configure).
    pub steps: Vec<Step>,
    /// True if at least one change in `changes` is destructive.
    pub has_destructive: bool,
}

impl ConfigureResult {
    /// True when nothing would change at all — neither files, env, nor
    /// metadata. The CLI uses this to short-circuit "already configured
    /// that way" without printing a confusing empty summary.
    /// `steps` is the source of truth: [`build_configure_steps`]
    /// returns `Vec::new()` whenever no step would run.
    pub fn is_noop(&self) -> bool {
        self.steps.is_empty()
    }
}

/// One env key whose value would change when the current global config is
/// re-rendered into an installed service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvKeyChange {
    pub key: String,
    /// On-disk value, or `None` when the key isn't present yet (the registry
    /// or global config newly produces it).
    pub from: Option<String>,
    pub to: String,
    /// True when the key name looks sensitive (password / secret / token), so
    /// the CLI masks the value when printing the diff. Display-only.
    pub secret: bool,
}

/// Plan for propagating the current global config into one installed
/// service's `.env`. Pure: reads the install's state, emits steps, touches
/// nothing.
pub struct ServiceReconcile {
    pub service: String,
    /// Keys whose re-rendered value differs from what's on disk, sorted by
    /// key. Empty when the service is already current.
    pub changes: Vec<EnvKeyChange>,
    /// Steps to apply: a merged `.env` write (changed keys only, every other
    /// line preserved byte-for-byte) plus a restart. Empty when `changes` is.
    pub steps: Vec<Step>,
}

/// Re-render an installed service against the *current* global config and
/// surface the env keys that come out different. Unlike [`configure_service`]
/// (which applies a user's per-service change) and
/// [`crate::upgrade::upgrade_service`] (which never touches `.env`), this is
/// the propagation path for "the global SMTP relay / admin email / auth
/// provider changed, push it into the services that consume it."
///
/// The whole `.env` is re-rendered, but everything that's *the user's* is
/// preserved first, so the only values that can move are driven by global
/// config (or a registry update): generated secrets and auth credentials come
/// back through the template context, interactively-supplied values (kind
/// `prompted`/`required`) are recovered straight from the live `.env`, and
/// ports are pinned. The diff is then taken over every key, and applied as a
/// line-level merge so any key the user hand-added to `.env` survives. No
/// hardcoded list of "which fields are global" is needed — the renderer is
/// the single source of truth.
pub async fn reconcile_service(service_name: &str) -> Result<ServiceReconcile> {
    let empty = ServiceReconcile {
        service: service_name.to_string(),
        changes: Vec::new(),
        steps: Vec::new(),
    };
    if !is_service_installed(service_name) {
        return Err(Error::ServiceNotInstalled(service_name.to_string()));
    }
    let metadata = load_metadata(service_name)?
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let service_ref = if metadata.registry.is_empty() || metadata.registry == REGISTRY_DEFAULT {
        ServiceRef::Default(service_name.to_string())
    } else if crate::registry::resolve::is_path_like(&metadata.registry) {
        ServiceRef::Path {
            dir: PathBuf::from(&metadata.registry),
            name: service_name.to_string(),
        }
    } else {
        ServiceRef::Custom {
            registry: metadata.registry.clone(),
            service: service_name.to_string(),
        }
    };
    let repo_dir = resolve_registry_dir(&service_ref).await?;
    let reg_service = registry::find_service(&repo_dir, service_name)?;
    let def = &reg_service.def;

    let enabled_groups: BTreeSet<String> = metadata.enabled_groups.iter().cloned().collect();
    let selected_choices = metadata.selected_choices.clone();

    let env_path = service_home(service_name)?.join(".env");
    let on_disk_text = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::FileRead {
                path: env_path,
                source,
            });
        }
    };
    let on_disk = parse_env_content(&on_disk_text);

    // Preserve everything that's the user's, so the only diffs left are
    // global-config (or registry) driven:
    //   - secrets / auth credentials → recovered into the template context;
    //   - prompted/required values → recovered straight from the live `.env`
    //     (their value came from the user at install, not from a template);
    //   - ports → pinned.
    let pre_built_ctx = recover_template_ctx(service_name, def)?;
    let mut env_overrides: BTreeMap<String, String> = BTreeMap::new();
    let mut recover_user_input = |e: &registry::service_def::EnvVar| {
        if matches!(e.kind, EnvKind::Prompted | EnvKind::Required)
            && let Some(v) = on_disk.get(&e.name)
        {
            env_overrides.insert(e.name.clone(), v.clone());
        }
    };
    for e in &def.env {
        recover_user_input(e);
    }
    for g in &def.env_groups {
        if enabled_groups.contains(&g.name) {
            for e in &g.env {
                recover_user_input(e);
            }
        }
    }
    for c in &def.choices {
        let selected = selected_choices.get(&c.name).unwrap_or(&c.default);
        if let Some(o) = c.options.iter().find(|o| &o.name == selected) {
            for e in &o.env {
                recover_user_input(e);
            }
        }
    }

    let exposure: Exposure = match metadata.url.as_deref() {
        Some(u) => Exposure::from_url(u),
        None => Exposure::Loopback,
    };
    let port_overrides = read_existing_ports(service_name)?;
    let port_in_use = |_p: u16| false;
    let result = add_service(crate::AddServiceParams {
        service_name,
        exposure: &exposure,
        auth: match metadata.auth.clone() {
            Some(kind) => crate::AuthChoice::Native(kind),
            None => crate::AuthChoice::None,
        },
        enable_smtp: metadata.smtp_enabled,
        enable_backup: metadata.backup_enabled,
        env_overrides: &env_overrides,
        enabled_groups: &enabled_groups,
        selected_choices: &selected_choices,
        registry_name: &metadata.registry,
        repo_dir: &repo_dir,
        pre_built_ctx: Some(pre_built_ctx),
        port_in_use: &port_in_use,
        acme_mode: None,
        mode: PlanMode::Upgrade,
        port_overrides: &port_overrides,
        // `ryra config` re-renders the full `.env` and reconciles it against
        // what's on disk itself, so it doesn't use the planner's merge.
        existing_env_file: None,
        allow_unset_required: false,
    })?;

    let rendered_content = result
        .steps
        .iter()
        .find_map(|s| match s {
            Step::WriteFile(f) if f.path == env_path => Some(f.content.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            Error::Template(format!(
                "{service_name}: re-render produced no .env to reconcile"
            ))
        })?;
    let rendered = parse_env_content(&rendered_content);

    // Diff every rendered key against disk. A key present in the render but
    // not on disk is an addition; keys only on disk (user-added, or dropped
    // by the registry) are never touched — the merge is append/update only.
    let mut changes: Vec<EnvKeyChange> = Vec::new();
    for (key, new_val) in &rendered {
        let old = on_disk.get(key);
        if old.map(String::as_str) != Some(new_val.as_str()) {
            changes.push(EnvKeyChange {
                key: key.clone(),
                from: old.cloned(),
                to: new_val.clone(),
                secret: is_sensitive_key(key),
            });
        }
    }
    changes.sort_by(|a, b| a.key.cmp(&b.key));

    if changes.is_empty() {
        return Ok(empty);
    }

    let merged = merge_env_changes(&on_disk_text, &changes);
    let steps = vec![
        Step::WriteFile(GeneratedFile {
            path: env_path,
            content: merged,
        }),
        Step::RestartService {
            unit: service_name.to_string(),
        },
    ];
    Ok(ServiceReconcile {
        service: service_name.to_string(),
        changes,
        steps,
    })
}

/// Whether an env key's *name* looks sensitive, for display masking only.
/// Used to decide whether to print `••••••` instead of the value in the
/// reconcile diff. Over-masking is harmless; this never affects what's
/// written.
fn is_sensitive_key(key: &str) -> bool {
    let up = key.to_ascii_uppercase();
    ["PASSWORD", "PASSWD", "SECRET", "TOKEN", "API_KEY", "APIKEY"]
        .iter()
        .any(|needle| up.contains(needle))
}

/// Parse `.env` text into a key→raw-value map. Comments and blanks skipped;
/// value keeps everything after the first `=` (values may contain `=`).
fn parse_env_content(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    out
}

/// Apply `changes` to the existing `.env` text line-by-line: rewrite the
/// value of any changed key in place (preserving file order and comments),
/// and append keys that weren't present. Every untouched line — secrets,
/// ports, prompted values, user-added keys — survives verbatim.
fn merge_env_changes(existing: &str, changes: &[EnvKeyChange]) -> String {
    let by_key: BTreeMap<&str, &str> = changes
        .iter()
        .map(|c| (c.key.as_str(), c.to.as_str()))
        .collect();
    let mut applied: BTreeSet<&str> = BTreeSet::new();
    let mut lines: Vec<String> = Vec::new();
    for line in existing.lines() {
        if let Some((k, _)) = line.trim().split_once('=') {
            let key = k.trim();
            if let Some(new_val) = by_key.get(key) {
                lines.push(format!("{key}={new_val}"));
                applied.insert(key);
                continue;
            }
        }
        lines.push(line.to_string());
    }
    for c in changes {
        if !applied.contains(c.key.as_str()) {
            lines.push(format!("{}={}", c.key, c.to));
        }
    }
    let mut content = lines.join("\n");
    content.push('\n');
    content
}

/// Re-plan an installed service against `overrides`. Pure: emits steps but
/// performs no I/O beyond reading the current install's state from disk.
pub async fn configure_service(
    service_name: &str,
    overrides: &Overrides,
) -> Result<ConfigureResult> {
    if !is_service_installed(service_name) {
        return Err(Error::ServiceNotInstalled(service_name.to_string()));
    }

    let metadata = load_metadata(service_name)?
        .ok_or_else(|| Error::ServiceNotInstalled(service_name.to_string()))?;

    let current_url: Option<String> = metadata.url.clone();
    let current_smtp: bool = metadata.smtp_enabled;
    let current_backup: bool = metadata.backup_enabled;
    let current_auth: bool = metadata.auth.is_some();
    let current_groups: BTreeSet<String> = metadata.enabled_groups.iter().cloned().collect();
    let current_choices = metadata.selected_choices.clone();

    // Compute target values.
    let target_url: Option<String> = match &overrides.exposure {
        None => current_url.clone(),
        Some(ExposureChange::Loopback) => None,
        Some(ExposureChange::Url(u)) => Some(u.clone()),
        Some(ExposureChange::Tailscale(u)) => Some(u.clone()),
    };
    let target_smtp: bool = overrides.smtp.unwrap_or(current_smtp);
    let target_backup: bool = overrides.backup.unwrap_or(current_backup);
    let target_auth: bool = overrides.auth.unwrap_or(current_auth);

    let service_ref = if metadata.registry.is_empty() || metadata.registry == REGISTRY_DEFAULT {
        ServiceRef::Default(service_name.to_string())
    } else {
        ServiceRef::Custom {
            registry: metadata.registry.clone(),
            service: service_name.to_string(),
        }
    };
    let repo_dir = resolve_registry_dir(&service_ref).await?;
    let reg_service = registry::find_service(&repo_dir, service_name)?;

    // Validate env_group flags before we touch any state, mirroring
    // `add_service`'s unknown-group check.
    let known_groups: BTreeSet<&str> = reg_service
        .def
        .env_groups
        .iter()
        .map(|g| g.name.as_str())
        .collect();
    for g in overrides
        .enable_groups
        .iter()
        .chain(overrides.disable_groups.iter())
    {
        if !known_groups.contains(g.as_str()) {
            let known: Vec<String> = known_groups.iter().map(|s| (*s).to_string()).collect();
            let hint = if known.is_empty() {
                " (service defines no env_groups)".to_string()
            } else {
                format!(" (known: {})", known.join(", "))
            };
            return Err(Error::UnknownEnvGroup {
                service: service_name.to_string(),
                group: g.clone(),
                hint,
            });
        }
    }
    for g in &overrides.enable_groups {
        if overrides.disable_groups.contains(g) {
            return Err(Error::ConfigureUnsupported {
                service: service_name.to_string(),
                field: format!("env_group '{g}'"),
                workaround:
                    "group can't appear in both --enable and --disable in one configure run"
                        .to_string(),
            });
        }
    }
    // Validate --choose against the registry: known choice, known option.
    for (cname, oname) in &overrides.choose {
        let Some(choice) = reg_service.def.choices.iter().find(|c| &c.name == cname) else {
            let known: Vec<&str> = reg_service
                .def
                .choices
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            let hint = if known.is_empty() {
                " (service defines no choices)".to_string()
            } else {
                format!(" (known: {})", known.join(", "))
            };
            return Err(Error::ConfigureUnsupported {
                service: service_name.to_string(),
                field: format!("choice '{cname}'"),
                workaround: format!("no such choice{hint}"),
            });
        };
        if !choice.options.iter().any(|o| &o.name == oname) {
            let known: Vec<&str> = choice.options.iter().map(|o| o.name.as_str()).collect();
            return Err(Error::ConfigureUnsupported {
                service: service_name.to_string(),
                field: format!("choice '{cname}' option '{oname}'"),
                workaround: format!("no such option (known: {})", known.join(", ")),
            });
        }
    }
    if target_backup && !reg_service.def.integrations.backup {
        return Err(Error::BackupNotSupported(service_name.to_string()));
    }
    // Enabling SMTP requires the service to actually consume it: an
    // `integrations.smtp` flag *and* a `[mappings.smtp]` block to render.
    // Mirrors the backup/auth guards (and the interactive prompt's
    // capability gate) so `configure <svc> --smtp` on a service that can't
    // send mail is rejected up front rather than recording a phantom
    // `smtp_enabled = true` that renders nothing.
    let smtp_supported =
        reg_service.def.integrations.smtp && !reg_service.def.mappings.smtp.is_empty();
    if !current_smtp && target_smtp && !smtp_supported {
        return Err(Error::ConfigureUnsupported {
            service: service_name.to_string(),
            field: "smtp".to_string(),
            workaround: "this service declares no SMTP support (no [mappings.smtp]); \
                 it can't be wired to the mail relay"
                .to_string(),
        });
    }
    // Enabling auth requires the service to support OIDC natively.
    // (`add_service` checks this too, but failing here gives a cleaner
    // error than a half-built plan.)
    if !current_auth
        && target_auth
        && reg_service.def.integrations.auth.is_empty()
        && !crate::capability::def_provides(&reg_service.def, crate::Capability::OidcProvider)
    {
        return Err(Error::NoOidcSupport(service_name.to_string()));
    }
    // OIDC client registration needs a base URL to write into the
    // `redirect_uris`. Covers both turn-on (need URL up front) and
    // URL-change-while-on (the re-register would have no target).
    let url_changed_pre = current_url != target_url;
    let needs_register_pre = target_auth && (!current_auth || url_changed_pre);
    if needs_register_pre && target_url.is_none() {
        return Err(Error::ConfigureUnsupported {
            service: service_name.to_string(),
            field: "auth without url".to_string(),
            workaround: "auth needs a public URL for the OIDC redirect_uri; pass `--url <URL>` \
                 alongside `--auth`, or use `--no-auth` to disable auth"
                .to_string(),
        });
    }

    let mut target_groups = current_groups.clone();
    for g in &overrides.enable_groups {
        target_groups.insert(g.clone());
    }
    for g in &overrides.disable_groups {
        target_groups.remove(g);
    }

    let mut target_choices = current_choices.clone();
    for (cname, oname) in &overrides.choose {
        target_choices.insert(cname.clone(), oname.clone());
    }

    // Recover existing secrets from the live `.env` so re-render doesn't
    // mint fresh ones. When auth is being *enabled* for the first time,
    // mint client_id / client_secret here (so we can pass the same pair
    // to the OIDC registration step below).
    let mut pre_built_ctx = recover_template_ctx(service_name, &reg_service.def)?;
    let mut minted_oidc: Option<(String, String)> = None;
    if !current_auth && target_auth {
        let client_id = secret::generate(&EnvFormat::Uuid, None);
        let client_secret = secret::generate(&EnvFormat::String, Some(64));
        pre_built_ctx.insert("auth.client_id".into(), client_id.clone());
        pre_built_ctx.insert("auth.client_secret".into(), client_secret.clone());
        minted_oidc = Some((client_id, client_secret));
    }

    // Pin existing host ports across re-renders — same rule as upgrade.
    let port_overrides = read_existing_ports(service_name)?;
    let port_in_use = |_p: u16| false;

    let target_exposure: Exposure = match &target_url {
        None => Exposure::Loopback,
        Some(u) => Exposure::from_url(u),
    };
    let prior_kind = current_url
        .as_deref()
        .map(Exposure::from_url)
        .unwrap_or(Exposure::Loopback);

    let result = add_service(crate::AddServiceParams {
        service_name,
        exposure: &target_exposure,
        auth: if target_auth {
            crate::AuthChoice::Native(AuthKind::Oidc)
        } else {
            crate::AuthChoice::None
        },
        enable_smtp: target_smtp,
        enable_backup: target_backup,
        env_overrides: &overrides.env_overrides,
        enabled_groups: &target_groups,
        selected_choices: &target_choices,
        registry_name: &metadata.registry,
        repo_dir: &repo_dir,
        pre_built_ctx: Some(pre_built_ctx),
        port_in_use: &port_in_use,
        // ACME is only consumed when seeding caddy on first install.
        acme_mode: None,
        mode: PlanMode::Upgrade,
        port_overrides: &port_overrides,
        // `ryra config` re-renders the full `.env` and reconciles it against
        // what's on disk itself, so it doesn't use the planner's merge.
        existing_env_file: None,
        allow_unset_required: false,
    })?;

    let diff = build_diff(service_name, &result)?;

    // High-level changes — order reflects how a user mentally categorises
    // the transitions (routing first, then per-service features, then
    // env scope, then individual vars).
    let mut changes: Vec<ConfigureChange> = Vec::new();
    if current_url != target_url {
        changes.push(ConfigureChange::Url {
            from: current_url.clone(),
            to: target_url.clone(),
        });
    }
    if current_auth != target_auth {
        changes.push(ConfigureChange::Auth {
            from: current_auth,
            to: target_auth,
        });
    }
    if current_smtp != target_smtp {
        changes.push(ConfigureChange::Smtp {
            from: current_smtp,
            to: target_smtp,
        });
    }
    if current_backup != target_backup {
        changes.push(ConfigureChange::Backup {
            from: current_backup,
            to: target_backup,
        });
    }
    for g in target_groups.difference(&current_groups) {
        changes.push(ConfigureChange::GroupEnabled(g.clone()));
    }
    for g in current_groups.difference(&target_groups) {
        changes.push(ConfigureChange::GroupDisabled(g.clone()));
    }
    let existing_env = read_existing_env_keys(service_name)?;
    for (key, val) in &overrides.env_overrides {
        let prior = existing_env.get(key).cloned();
        if prior.as_deref() != Some(val.as_str()) {
            changes.push(ConfigureChange::EnvOverride {
                key: key.clone(),
                from: prior,
                to: val.clone(),
            });
        }
    }
    let has_destructive = changes.iter().any(|c| c.is_destructive());

    // Cross-service lifecycle: classify what side-effects this configure
    // needs beyond writing the service's own files.
    //
    // OIDC: re-register whenever (a) auth is being turned on, or (b)
    // auth was already on but the URL changed (Authelia pins the
    // redirect_uri at registration time, so the old entry would now
    // point at the wrong hostname).
    let url_changed = current_url != target_url;
    let needs_unregister = current_auth && (!target_auth || url_changed);
    // `reassert_auth` forces a re-register (reusing the existing `.env` creds
    // via the same path a URL change takes) without a URL change. The outer
    // `target_auth` gate means it only fires for services that actually have
    // auth on; a no-op otherwise.
    let needs_register = target_auth && (!current_auth || url_changed || overrides.reassert_auth);
    // Tailscale: enable when entering, disable when leaving. The two
    // sides are independent — going `tailscale → url` runs both.
    let prior_is_ts = matches!(prior_kind, Exposure::Tailscale { .. });
    let target_is_ts = matches!(target_exposure, Exposure::Tailscale { .. });
    let needs_tailscale_disable = prior_is_ts && !target_is_ts;
    let needs_tailscale_enable = target_is_ts && !prior_is_ts;

    // Configure is a *user-requested-change applicator*, not a
    // drift-corrector. If the user asked for nothing (no
    // `ConfigureChange` entries) and no cross-service lifecycle step is
    // needed, we return zero steps — even if a `.env` re-render would
    // produce slightly different bytes (e.g. an `{{auth.*}}` template
    // resolving differently because caddy's port shifted since
    // install). Drift correction is what `ryra upgrade` is for; making
    // configure also chase drift produces confusing "nothing changed
    // but I'm restarting your service" runs.
    let no_user_request = changes.is_empty()
        && !needs_unregister
        && !needs_register
        && !needs_tailscale_disable
        && !needs_tailscale_enable;
    let steps = if no_user_request {
        Vec::new()
    } else {
        build_configure_steps(
            service_name,
            &result,
            &reg_service.def,
            &diff,
            current_url.as_deref(),
            target_url.as_deref(),
            needs_unregister,
            needs_register,
            needs_tailscale_disable,
            needs_tailscale_enable,
            minted_oidc.as_ref(),
        )?
    };

    Ok(ConfigureResult {
        service: service_name.to_string(),
        changes,
        diff,
        steps,
        has_destructive,
    })
}

/// Build the upgrade-style file diff from the freshly-planned `WriteFile`
/// steps. Mirrors `upgrade::diff_service` but operates on the already-
/// computed `AddResult` so we don't re-run the planner.
fn build_diff(service_name: &str, result: &AddResult) -> Result<DiffResult> {
    let manifest_file = manifest::manifest_path(service_name)?;
    let (manifest_entries, _) = manifest::load(service_name)?.unwrap_or_default();
    let manifest_by_path: BTreeMap<PathBuf, String> = manifest_entries
        .into_iter()
        .map(|e| (e.path, e.sha256))
        .collect();

    let planned: BTreeMap<PathBuf, String> = result
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::WriteFile(f) => Some((f.path.clone(), f.content.clone())),
            _ => None,
        })
        .collect();

    let existing_env = read_existing_env_keys(service_name)?;
    let env_additions: Vec<EnvAddition> = result
        .tracked_envs
        .iter()
        .filter(|p| !existing_env.contains_key(&p.key))
        .map(|p| EnvAddition {
            key: p.key.clone(),
            value: p.value.clone(),
            kind: p.kind.clone(),
            prompt: p.prompt.clone(),
        })
        .collect();

    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let env_filename = std::ffi::OsStr::new(".env");

    for (path, content) in &planned {
        seen.insert(path.clone());
        let planned_hash = manifest::hash_bytes(content.as_bytes());
        let on_disk_hash = if path.exists() {
            Some(manifest::hash_file(path)?)
        } else {
            None
        };
        let manifest_hash = manifest_by_path.get(path);
        let is_env = path.file_name() == Some(env_filename);
        let is_manifest = path == &manifest_file;
        let kind = match (on_disk_hash.as_deref(), manifest_hash.map(String::as_str)) {
            (None, _) => match manifest_hash {
                Some(_) => DiffKind::Modified,
                None => DiffKind::Added,
            },
            (Some(d), _) if d == planned_hash => DiffKind::Unchanged,
            // `.env` and the manifest itself have no manifest entry by
            // design (`.env` because of rotating secrets, the manifest
            // because of self-reference). For both, "no manifest entry"
            // does NOT mean drift — treat them as ryra-owned and safe
            // to overwrite. Without this carve-out they'd always read
            // as Drift on the first configure of a legacy install.
            (Some(_), None) if is_env || is_manifest => DiffKind::Modified,
            (Some(_), None) => DiffKind::Drift,
            (Some(d), Some(l)) if d == l => DiffKind::Modified,
            (Some(_), Some(_)) => DiffKind::Drift,
        };
        entries.push(DiffEntry {
            path: path.clone(),
            kind,
        });
    }
    for path in manifest_by_path.keys() {
        if seen.contains(path) {
            continue;
        }
        entries.push(DiffEntry {
            path: path.clone(),
            kind: DiffKind::Removed,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffResult {
        service: service_name.to_string(),
        entries,
        env_additions,
        // Configure diffs are about changed integration config, not native
        // source freshness — that signal belongs to the upgrade path.
        source_stale: false,
    })
}

/// Assemble the final step list:
///
/// ```text
///   writes → copies → removals
///   → caddy route teardown (url leaving)
///   → OIDC unregister (auth off / url changed with auth)
///   → Tailscale disable (leaving tailscale)
///   → daemon-reload (if any quadlet changed)
///   → caddy reload (if Caddyfile changed)
///   → Tailscale setup + enable (entering tailscale)
///   → OIDC register (auth on / url changed with auth)
///   → restart
/// ```
///
/// `Reload/restart` steps are gated on at least one file actually changing
/// **or** a cross-service lifecycle step needing to run. Without the gate,
/// configure would always restart the unit (phantom downtime) and prompt
/// the user to confirm even when there was literally nothing to apply.
#[allow(clippy::too_many_arguments)]
fn build_configure_steps(
    service_name: &str,
    result: &AddResult,
    service_def: &registry::service_def::ServiceDef,
    diff: &DiffResult,
    current_url: Option<&str>,
    target_url: Option<&str>,
    needs_unregister: bool,
    needs_register: bool,
    needs_tailscale_disable: bool,
    needs_tailscale_enable: bool,
    minted_oidc: Option<&(String, String)>,
) -> Result<Vec<Step>> {
    let unchanged: BTreeSet<PathBuf> = diff
        .entries
        .iter()
        .filter(|e| matches!(e.kind, DiffKind::Unchanged))
        .map(|e| e.path.clone())
        .collect();

    let mut writes: Vec<Step> = Vec::new();
    let mut copies: Vec<Step> = Vec::new();
    let mut kept_caddyfile = false;
    let mut kept_quadlet = false;
    let caddyfile_path = caddy::caddyfile_path().ok();

    let home_dir = service_home(service_name)?;
    for step in &result.steps {
        match step {
            // Install-only — configure issues a Restart at the very end if needed.
            Step::StartService { .. } => continue,
            // Home dir already exists.
            Step::CreateDir(p) if p == &home_dir => continue,
            // Image pulls are idempotent and rare to need on configure.
            Step::PullImage { .. } => continue,
            // Defer until we know whether any write happened.
            Step::DaemonReload | Step::ReloadCaddy | Step::Symlink { .. } => continue,
            // Install-only Tailscale steps — configure decides via the
            // explicit lifecycle flags below.
            Step::TailscaleSetup | Step::TailscaleEnable { .. } | Step::TailscaleDisable { .. } => {
                continue;
            }
            Step::WriteFile(file) => {
                if unchanged.contains(&file.path) {
                    continue;
                }
                if Some(&file.path) == caddyfile_path.as_ref() {
                    kept_caddyfile = true;
                }
                // Quadlet files (`.container`, `.network`, `.volume`, …)
                // live in `service_home/<name>.<ext>` and are *symlinked*
                // into the quadlet dir. Detect by extension: the symlink
                // in quadlet_dir is what `systemctl --user daemon-reload`
                // picks up, but the target it points at is the write
                // path we see here.
                if is_quadlet_filename(&file.path) {
                    kept_quadlet = true;
                }
                writes.push(Step::WriteFile(GeneratedFile {
                    path: file.path.clone(),
                    content: file.content.clone(),
                }));
            }
            Step::CopyFile { src, dst } => {
                copies.push(Step::CopyFile {
                    src: src.clone(),
                    dst: dst.clone(),
                });
            }
            other => copies.push(clone_step(other)),
        }
    }

    // Removed files: planner didn't emit them; rebuild the delete steps.
    let mut removals: Vec<Step> = Vec::new();
    for entry in &diff.entries {
        if matches!(entry.kind, DiffKind::Removed) && entry.path.exists() {
            removals.push(Step::RemoveFile(entry.path.clone()));
        }
    }

    // Caddy route teardown: emit when configure removes the URL *or*
    // when changing to a non-Caddy exposure (Loopback / Tailscale). The
    // add path strips and re-adds the block atomically when the URL
    // changes from one Caddy-routed value to another, so we only need a
    // teardown here for the *leaving Caddy* case.
    let prior_exp = current_url
        .map(Exposure::from_url)
        .unwrap_or(Exposure::Loopback);
    let target_exp = target_url
        .map(Exposure::from_url)
        .unwrap_or(Exposure::Loopback);
    let prior_caddy = matches!(
        prior_exp,
        Exposure::Internal { .. } | Exposure::Public { .. }
    );
    let target_caddy = matches!(
        target_exp,
        Exposure::Internal { .. } | Exposure::Public { .. }
    );
    let mut url_teardown: Vec<Step> = Vec::new();
    if prior_caddy
        && !target_caddy
        && let Some(prev) = current_url
        && let Some(s) = caddy_remove_route_steps(service_name, prev)?
    {
        url_teardown = s;
        kept_caddyfile = true;
    }

    // OIDC unregister + Tailscale disable steps run on the *old* state.
    let mut unregister_steps: Vec<Step> = Vec::new();
    if needs_unregister {
        unregister_steps = authelia::unregister_oidc_client(service_name)?;
    }
    let mut tailscale_disable_steps: Vec<Step> = Vec::new();
    if needs_tailscale_disable
        && let Some(svc_name) = current_url
            .map(Exposure::from_url)
            .as_ref()
            .and_then(|e| e.tailscale_svc_name())
    {
        tailscale_disable_steps.push(Step::TailscaleDisable { svc_name });
    }

    // OIDC register + Tailscale enable steps run on the *new* state.
    let mut register_steps: Vec<Step> = Vec::new();
    if needs_register {
        let (client_id, client_secret) = match minted_oidc {
            Some((id, secret)) => (id.clone(), secret.clone()),
            None => {
                // URL change on a service that was already auth-enabled.
                // Reuse the existing credentials so authelia's new entry
                // matches whatever the service's `.env` already holds.
                let env = read_existing_env_keys(service_name)?;
                let id = service_def
                    .mappings
                    .auth
                    .iter()
                    .find(|(_, v)| v.trim() == "{{auth.client_id}}")
                    .and_then(|(k, _)| env.get(k).map(|v| trim_env_value(v)))
                    .ok_or_else(|| {
                        Error::AuthContext(format!(
                            "service '{service_name}' has auth=oidc in metadata but no \
                             OAUTH_CLIENT_ID-shaped env var found — cannot re-register OIDC \
                             client at the new URL"
                        ))
                    })?;
                let secret = service_def
                    .mappings
                    .auth
                    .iter()
                    .find(|(_, v)| v.trim() == "{{auth.client_secret}}")
                    .and_then(|(k, _)| env.get(k).map(|v| trim_env_value(v)))
                    .unwrap_or_default();
                (id, secret)
            }
        };
        let mut ctx: BTreeMap<String, String> = BTreeMap::new();
        ctx.insert("auth.client_id".into(), client_id);
        ctx.insert("auth.client_secret".into(), client_secret);
        if let Some(u) = target_url {
            ctx.insert("service.url".into(), u.to_string());
        }
        let qdir = quadlet_dir()?;
        register_steps =
            authelia::register_oidc_client(service_name, service_def, target_url, &ctx, &qdir)?;
    }
    let mut tailscale_enable_steps: Vec<Step> = Vec::new();
    if needs_tailscale_enable
        && let Some(svc_name) = target_url
            .map(Exposure::from_url)
            .as_ref()
            .and_then(|e| e.tailscale_svc_name())
    {
        let primary = result
            .allocated_ports
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("http"))
            .or_else(|| result.allocated_ports.first())
            .map(|(_, p)| *p);
        let ts_ports =
            crate::plan::tailscale_ports(&service_def.ports, &result.allocated_ports, primary);
        if !ts_ports.is_empty() {
            tailscale_enable_steps.push(Step::TailscaleSetup);
            tailscale_enable_steps.push(Step::TailscaleEnable {
                svc_name,
                ports: ts_ports,
            });
        }
    }

    let any_file_change = !writes.is_empty() || !removals.is_empty() || !url_teardown.is_empty();
    let any_lifecycle = !unregister_steps.is_empty()
        || !register_steps.is_empty()
        || !tailscale_disable_steps.is_empty()
        || !tailscale_enable_steps.is_empty();
    if !any_file_change && !any_lifecycle {
        return Ok(Vec::new());
    }
    // Restart only when something the *container* actually sees has
    // changed: a quadlet rewrite, a `.env` rewrite, a script/cert
    // appearing or disappearing under service_home, a Caddyfile rewrite
    // that fronts this service, or an OIDC / Tailscale lifecycle step.
    // Metadata-only changes (backup_enabled, smtp_enabled flag) live in
    // `metadata.toml` and don't touch the running unit — restarting on
    // those eats systemd's `StartLimitBurst` budget for no reason.
    // ryra's own bookkeeping files (metadata.toml + service.manifest)
    // never reach the container — they're pure state for `ryra list`,
    // `ryra upgrade`, etc. A write that only touches these doesn't
    // warrant a restart.
    let manifest_file = manifest::manifest_path(service_name).ok();
    let metadata_file = manifest_file
        .as_ref()
        .and_then(|p| p.parent().map(|p| p.join("metadata.toml")));
    let writes_affect_runtime = writes.iter().any(|s| match s {
        Step::WriteFile(f) => {
            Some(&f.path) != metadata_file.as_ref() && Some(&f.path) != manifest_file.as_ref()
        }
        _ => false,
    });
    let needs_restart =
        writes_affect_runtime || !removals.is_empty() || !url_teardown.is_empty() || any_lifecycle;

    let mut steps: Vec<Step> = Vec::new();
    // Forward Symlinks alongside their WriteFile pairs.
    for step in &result.steps {
        if let Step::Symlink { link, target } = step
            && writes
                .iter()
                .any(|s| matches!(s, Step::WriteFile(f) if &f.path == target))
        {
            steps.push(Step::Symlink {
                link: link.clone(),
                target: target.clone(),
            });
        }
    }
    steps.splice(0..0, writes);
    steps.extend(copies);
    steps.extend(removals);
    steps.extend(url_teardown);
    steps.extend(unregister_steps);
    steps.extend(tailscale_disable_steps);
    if kept_quadlet {
        steps.push(Step::DaemonReload);
    }
    if kept_caddyfile {
        steps.push(Step::ReloadCaddy);
    }
    steps.extend(tailscale_enable_steps);
    steps.extend(register_steps);
    if needs_restart {
        steps.push(Step::RestartService {
            unit: service_name.to_string(),
        });
    }
    Ok(steps)
}

/// When configure is dropping a URL, emit the Caddyfile mutation that
/// strips the matching `# Service-Source: registry/<svc>` block, plus a
/// `ReloadCaddy` step. Returns `None` if Caddy isn't installed or no
/// block matches — both legitimate states for a non-Caddy-routed URL.
fn caddy_remove_route_steps(service_name: &str, prior_url: &str) -> Result<Option<Vec<Step>>> {
    use crate::{Capability, find_installed_provider};
    let installed = list_installed().unwrap_or_default();
    if find_installed_provider(&installed, Capability::ReverseProxy).is_none() {
        return Ok(None);
    }
    // Loopback / Tailscale never had a Caddy route — skip the rewrite.
    let prior_exp = Exposure::from_url(prior_url);
    if matches!(prior_exp, Exposure::Loopback | Exposure::Tailscale { .. }) {
        return Ok(None);
    }
    if WellKnownService::Caddy.matches(service_name) {
        return Ok(None);
    }
    let caddyfile_path = caddy::caddyfile_path()?;
    if !caddyfile_path.exists() {
        return Ok(None);
    }
    let existing = std::fs::read_to_string(&caddyfile_path).map_err(|source| Error::FileRead {
        path: caddyfile_path.clone(),
        source,
    })?;
    let updated = caddy::remove_route(&existing, service_name);
    if updated == existing {
        return Ok(None);
    }
    let mut out: Vec<Step> = Vec::new();
    out.push(Step::WriteFile(GeneratedFile {
        path: caddyfile_path,
        content: updated.clone(),
    }));
    if !updated.trim().is_empty() {
        out.push(Step::ReloadCaddy);
    }
    Ok(Some(out))
}

/// Read `.env` and reconstruct the template context entries the planner
/// would otherwise have to regenerate. Every `KEY=VALUE` line whose `KEY`
/// matches one of the service's `{{secret.<name>}}` or `{{auth.<name>}}`
/// references seeds the context with the on-disk value, so `add_service`
/// (called in upgrade mode) reuses the existing credentials verbatim
/// instead of minting fresh ones.
fn recover_template_ctx(
    service_name: &str,
    def: &registry::service_def::ServiceDef,
) -> Result<BTreeMap<String, String>> {
    let existing_env = read_existing_env_keys(service_name)?;
    if existing_env.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut ctx = BTreeMap::new();

    let collect_secrets = |value: &str, out: &mut Vec<String>| {
        let mut rest = value;
        while let Some(start) = rest.find("{{secret.") {
            let after = &rest[start + 9..];
            if let Some(end) = after.find("}}") {
                out.push(after[..end].to_string());
                rest = &after[end + 2..];
            } else {
                break;
            }
        }
    };
    let collect_auth = |value: &str, out: &mut Vec<String>| {
        for needle in ["{{auth.client_id", "{{auth.client_secret"] {
            if value.contains(needle) {
                let stripped = needle.trim_start_matches("{{auth.");
                out.push(stripped.to_string());
            }
        }
    };

    let mut secret_pairs: Vec<(String, String)> = Vec::new();
    let mut auth_keys: Vec<String> = Vec::new();

    let mut consider = |env: &registry::service_def::EnvVar| {
        let trimmed = env.value.trim();
        if let Some(name) = trimmed
            .strip_prefix("{{secret.")
            .and_then(|s| s.strip_suffix("}}"))
            && let Some(live) = existing_env.get(&env.name)
        {
            secret_pairs.push((name.to_string(), trim_env_value(live)));
        }
        let mut extras: Vec<String> = Vec::new();
        collect_secrets(&env.value, &mut extras);
        for n in extras {
            if !secret_pairs.iter().any(|(k, _)| k == &n) {
                secret_pairs.push((n, String::new()));
            }
        }
        let mut auth_refs: Vec<String> = Vec::new();
        collect_auth(&env.value, &mut auth_refs);
        for n in auth_refs {
            if !auth_keys.contains(&n) {
                auth_keys.push(n);
            }
        }
    };

    for e in &def.env {
        consider(e);
    }
    for g in &def.env_groups {
        for e in &g.env {
            consider(e);
        }
    }
    for (env_name, value_template) in &def.mappings.auth {
        let env = registry::service_def::EnvVar {
            name: env_name.clone(),
            value: value_template.clone(),
            kind: Default::default(),
            prompt: None,
            format: Default::default(),
            length: None,
            jwt_claims: None,
            jwt_signing_key: None,
        };
        consider(&env);
    }

    for (name, value) in &secret_pairs {
        if !value.is_empty() {
            ctx.insert(format!("secret.{name}"), value.clone());
        }
    }
    for (env_name, value_template) in &def.mappings.auth {
        let trimmed = value_template.trim();
        if let Some(rest) = trimmed
            .strip_prefix("{{auth.")
            .and_then(|s| s.strip_suffix("}}"))
            && let Some(live) = existing_env.get(env_name)
        {
            ctx.insert(format!("auth.{rest}"), trim_env_value(live));
        }
    }

    Ok(ctx)
}

fn trim_env_value(raw: &str) -> String {
    raw.trim_matches(|c: char| c == '"' || c == '\'')
        .to_string()
}

/// True when `path`'s filename ends in a podman-quadlet extension. Quadlet
/// regenerates a `.service` per matching file on every
/// `systemctl --user daemon-reload`, so a write to any of these means we
/// need to emit a reload before restarting.
fn is_quadlet_filename(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("container" | "volume" | "network" | "kube" | "image" | "pod" | "build")
    )
}

/// Parse the on-disk `.env` for a service into a key→value map.
fn read_existing_env_keys(service_name: &str) -> Result<BTreeMap<String, String>> {
    let env_path = service_home(service_name)?.join(".env");
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => {
            return Err(Error::FileRead {
                path: env_path,
                source,
            });
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    Ok(out)
}

/// Pin existing host ports across re-renders.
fn read_existing_ports(service_name: &str) -> Result<BTreeMap<String, u16>> {
    let env_path = service_home(service_name)?.join(".env");
    let mut overrides = BTreeMap::new();
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(overrides),
        Err(source) => {
            return Err(Error::FileRead {
                path: env_path,
                source,
            });
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(name) = key.strip_prefix("SERVICE_PORT_") else {
            continue;
        };
        if let Ok(port) = value.trim().parse::<u16>() {
            overrides.insert(name.to_ascii_lowercase(), port);
        }
    }
    Ok(overrides)
}

/// Clone a `Step` explicitly. `Step` carries non-`Clone` payloads in
/// places; we list each variant so a new one forces a compile error
/// here rather than silently being dropped.
fn clone_step(step: &Step) -> Step {
    match step {
        Step::WriteFile(f) => Step::WriteFile(GeneratedFile {
            path: f.path.clone(),
            content: f.content.clone(),
        }),
        Step::Symlink { link, target } => Step::Symlink {
            link: link.clone(),
            target: target.clone(),
        },
        Step::DaemonReload => Step::DaemonReload,
        Step::StartService { unit } => Step::StartService { unit: unit.clone() },
        Step::EnableService { unit } => Step::EnableService { unit: unit.clone() },
        Step::DisableService { unit } => Step::DisableService { unit: unit.clone() },
        Step::StopService { unit } => Step::StopService { unit: unit.clone() },
        Step::RestartService { unit } => Step::RestartService { unit: unit.clone() },
        Step::ReloadCaddy => Step::ReloadCaddy,
        Step::PullImage { image } => Step::PullImage {
            image: image.clone(),
        },
        Step::RemoveFile(p) => Step::RemoveFile(p.clone()),
        Step::RemoveDir(p) => Step::RemoveDir(p.clone()),
        Step::RemoveVolume { name } => Step::RemoveVolume { name: name.clone() },
        Step::RemoveNetwork { name } => Step::RemoveNetwork { name: name.clone() },
        Step::CreateDir(p) => Step::CreateDir(p.clone()),
        Step::WaitForFile { path, timeout_secs } => Step::WaitForFile {
            path: path.clone(),
            timeout_secs: *timeout_secs,
        },
        Step::WaitForHttpHealthy {
            url,
            expect_status,
            timeout_secs,
        } => Step::WaitForHttpHealthy {
            url: url.clone(),
            expect_status: *expect_status,
            timeout_secs: *timeout_secs,
        },
        Step::CopyFile { src, dst } => Step::CopyFile {
            src: src.clone(),
            dst: dst.clone(),
        },
        Step::Build { dir, command } => Step::Build {
            dir: dir.clone(),
            command: command.clone(),
        },
        Step::SyncDir { src, dst } => Step::SyncDir {
            src: src.clone(),
            dst: dst.clone(),
        },
        Step::TailscaleSetup => Step::TailscaleSetup,
        Step::TailscaleEnable { svc_name, ports } => Step::TailscaleEnable {
            svc_name: svc_name.clone(),
            ports: ports.clone(),
        },
        Step::TailscaleDisable { svc_name } => Step::TailscaleDisable {
            svc_name: svc_name.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line-level merge is the safety contract for reconcile: it must
    /// rewrite only the changed keys and leave everything else — comments,
    /// secrets, ports, user-added keys, file order — byte-for-byte intact.
    #[test]
    fn merge_rewrites_only_changed_keys() {
        let existing = "\
# generated by ryra
SMTP_HOST=old.example.com
SMTP_PORT=587
POSTGRES_PASSWORD=s3cret-unchanged
ADMIN_EMAIL=me@example.com
SERVICE_PORT_HTTP=8080
USER_ADDED=keep-me
";
        let changes = vec![
            EnvKeyChange {
                key: "SMTP_HOST".into(),
                from: Some("old.example.com".into()),
                to: "new.example.com".into(),
                secret: false,
            },
            // A key not yet present — appended, never inserted mid-file.
            EnvKeyChange {
                key: "SMTP_FROM".into(),
                from: None,
                to: "noreply@new.example.com".into(),
                secret: false,
            },
        ];
        let merged = merge_env_changes(existing, &changes);
        let parsed = parse_env_content(&merged);
        assert_eq!(
            parsed.get("SMTP_HOST").map(String::as_str),
            Some("new.example.com")
        );
        assert_eq!(
            parsed.get("SMTP_FROM").map(String::as_str),
            Some("noreply@new.example.com")
        );
        // Untouched lines survive verbatim.
        assert_eq!(
            parsed.get("POSTGRES_PASSWORD").map(String::as_str),
            Some("s3cret-unchanged")
        );
        assert_eq!(
            parsed.get("USER_ADDED").map(String::as_str),
            Some("keep-me")
        );
        assert_eq!(
            parsed.get("SERVICE_PORT_HTTP").map(String::as_str),
            Some("8080")
        );
        // The comment header is preserved.
        assert!(merged.starts_with("# generated by ryra\n"));
        // No duplicate SMTP_HOST line was appended.
        assert_eq!(merged.matches("SMTP_HOST=").count(), 1);
    }

    /// The is_destructive matrix is the safety contract: it decides
    /// whether the CLI demands typed confirmation. One table-driven test
    /// makes it cheap to spot a regression in any cell.
    #[test]
    fn destructive_classification() {
        let url = |from: Option<&str>, to: Option<&str>| ConfigureChange::Url {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        };
        let cases: &[(ConfigureChange, bool)] = &[
            // URL: changing or removing destroys old routes / OAuth callbacks.
            (url(Some("https://old"), Some("https://new")), true),
            (url(Some("https://old"), None), true),
            (url(None, Some("https://new")), false),
            (url(Some("https://x"), Some("https://x")), false),
            // Toggles: only the off direction is destructive.
            (
                ConfigureChange::Smtp {
                    from: true,
                    to: false,
                },
                true,
            ),
            (
                ConfigureChange::Smtp {
                    from: false,
                    to: true,
                },
                false,
            ),
            (
                ConfigureChange::Backup {
                    from: true,
                    to: false,
                },
                true,
            ),
            (
                ConfigureChange::Backup {
                    from: false,
                    to: true,
                },
                false,
            ),
            (
                ConfigureChange::Auth {
                    from: true,
                    to: false,
                },
                true,
            ),
            (
                ConfigureChange::Auth {
                    from: false,
                    to: true,
                },
                false,
            ),
            // Group disable drops env vars; enable just adds them.
            (ConfigureChange::GroupDisabled("oauth".into()), true),
            (ConfigureChange::GroupEnabled("oauth".into()), false),
            // Explicit user override: never a surprise.
            (
                ConfigureChange::EnvOverride {
                    key: "ADMIN_EMAIL".into(),
                    from: Some("a".into()),
                    to: "b".into(),
                },
                false,
            ),
        ];
        for (change, expected) in cases {
            assert_eq!(
                change.is_destructive(),
                *expected,
                "wrong classification for {change:?}"
            );
        }
    }
}
