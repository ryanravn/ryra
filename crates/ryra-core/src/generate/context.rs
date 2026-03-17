use std::collections::BTreeMap;

use crate::config::schema::Config;
use crate::config::state::State;
use crate::registry::service_def::ServiceDef;
use crate::system::secret;

/// Build the template context for rendering env var values.
/// Secrets are generated fresh and returned separately (not stored in state).
pub fn build_context(
    config: &Config,
    state: &State,
    service_def: &ServiceDef,
    domain: &str,
    secret_refs: &[String],
) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();

    // service.*
    ctx.insert("service.name".into(), service_def.service.name.clone());
    ctx.insert("service.domain".into(), domain.to_string());

    // host.*
    ctx.insert("host.domain".into(), config.host.domain.clone());
    // smtp.* (only if configured)
    if let Some(smtp) = &config.smtp {
        ctx.insert("smtp.host".into(), smtp.host.clone());
        ctx.insert("smtp.port".into(), smtp.port.to_string());
        ctx.insert("smtp.username".into(), smtp.username.clone());
        ctx.insert("smtp.password".into(), smtp.password.clone());
        ctx.insert("smtp.from".into(), smtp.from.clone());
    }

    // secret.* — generate fresh values for each unique secret reference
    for secret_name in secret_refs {
        let key = format!("secret.{secret_name}");
        if !ctx.contains_key(&key) {
            ctx.insert(key, secret::generate_secret());
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
