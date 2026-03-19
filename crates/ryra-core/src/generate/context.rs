use std::collections::BTreeMap;

use crate::config::schema::Config;
use crate::registry::service_def::ServiceDef;
use crate::system::secret;

/// Build the template context for rendering env var values.
/// Secrets are generated fresh using each env var's format + length.
pub fn build_context(
    config: &Config,
    service_def: &ServiceDef,
    domain: &str,
) -> BTreeMap<String, String> {
    let mut ctx = BTreeMap::new();

    // service.*
    ctx.insert("service.name".into(), service_def.service.name.clone());
    ctx.insert("service.domain".into(), domain.to_string());

    // host.*
    if let Some(base_domain) = config.base_domain() {
        ctx.insert("host.domain".into(), base_domain.to_string());
    }

    // smtp.*
    if let Some(smtp) = &config.smtp {
        ctx.insert("smtp.host".into(), smtp.host.clone());
        ctx.insert("smtp.port".into(), smtp.port.to_string());
        ctx.insert("smtp.username".into(), smtp.username.clone());
        ctx.insert("smtp.password".into(), smtp.password.clone());
        ctx.insert("smtp.from".into(), smtp.from.clone());
    }

    // secret.* — generate fresh values using the env var's format + length
    for env in &service_def.env {
        for secret_name in crate::generate::extract_secret_refs(&env.value) {
            let key = format!("secret.{secret_name}");
            if !ctx.contains_key(&key) {
                ctx.insert(key, secret::generate(&env.format, env.length));
            }
        }
    }

    ctx
}
