use std::collections::BTreeMap;

use crate::config::schema::{Config, SmtpConfig};
use crate::config::state::State;
use crate::registry::service_def::ServiceDef;

/// Build the template context for rendering env var values.
pub fn build_context(
    config: &Config,
    state: &State,
    service_def: &ServiceDef,
    domain: &str,
) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();

    // service.*
    ctx.insert("service.name".into(), service_def.service.name.clone());
    ctx.insert("service.domain".into(), domain.to_string());

    // host.*
    ctx.insert("host.domain".into(), config.host.domain.clone());
    // smtp.* (only if configured)
    if let SmtpConfig::Configured {
        host,
        port,
        username,
        password,
        from,
    } = &config.smtp
    {
        ctx.insert("smtp.host".into(), host.clone());
        ctx.insert("smtp.port".into(), port.to_string());
        ctx.insert("smtp.username".into(), username.clone());
        ctx.insert("smtp.password".into(), password.clone());
        ctx.insert("smtp.from".into(), from.clone());
    }

    // secret.* — all secrets for this service
    for secret in &state.secrets {
        if secret.service == service_def.service.name {
            ctx.insert(format!("secret.{}", secret.name), secret.value.clone());
        }
    }

    // dep.*.host_port — allocated ports for dependencies
    let prefix = format!("{}-", service_def.service.name);
    for alloc in &state.allocated {
        if let Some(dep_name) = alloc.service.strip_prefix(&prefix) {
            ctx.insert(
                format!("dep.{dep_name}.host_port"),
                alloc.host_port.to_string(),
            );
        }
    }

    ctx
}
