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

    // services.* — cross-service references from installed services
    for installed in &config.services {
        let name = &installed.name;

        // services.<name>.domain
        if let Some(ref domain) = installed.domain {
            ctx.insert(format!("services.{name}.domain"), domain.clone());
        }

        // services.<name>.port.<port_name> — from stored port mappings
        for (port_name, port) in &installed.ports {
            ctx.insert(
                format!("services.{name}.port.{port_name}"),
                port.to_string(),
            );
        }

        // services.<name>.env.<VAR> — read from the service's .env file
        let env_file = crate::service_home(name).join(".env");
        if let Ok(content) = std::fs::read_to_string(&env_file) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, val)) = line.split_once('=') {
                    ctx.insert(format!("services.{name}.env.{key}"), val.to_string());
                }
            }
        }
    }

    // secret.* — generate fresh values using the env var's format + length.
    // Scan both the service's own env vars and dependency env vars so that
    // shared secret references (e.g., {{secret.db_password}}) resolve to the
    // same generated value across the main service and its sidecars.
    let all_env_vars = service_def.env.iter().chain(
        service_def
            .dependencies
            .iter()
            .flat_map(|dep| dep.env.iter()),
    );

    for env in all_env_vars {
        for secret_name in crate::generate::extract_secret_refs(&env.value) {
            let key = format!("secret.{secret_name}");
            ctx.entry(key)
                .or_insert_with(|| secret::generate(&env.format, env.length));
        }
    }

    ctx
}
