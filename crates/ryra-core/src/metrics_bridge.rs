//! Wiring between metrics-store providers (prometheus), services that
//! expose a `[metrics]` endpoint, and metrics-dashboard providers
//! (grafana).
//!
//! Pure step builders — no filesystem writes happen here; callers emit
//! the returned [`Step`]s through the normal plan/apply pipeline.
//!
//! Two artifacts:
//! - **Scrape targets**: one `<store-home>/targets/<svc>.json` per
//!   metrics-declaring service. The store's config watches that dir via
//!   `file_sd_configs`, so adding/removing a file takes effect without a
//!   reload. Targets address the consumer's main container by name
//!   (registry convention: `ContainerName=<service>`) on the *container*
//!   port — the store reaches it over the shared podman network.
//! - **Datasources**: one provisioning yml per store in the dashboard's
//!   `provisioning-datasources/` dir (bind-mounted into grafana's
//!   provisioning path). Read at boot — pair with a restart when the
//!   dashboard is already running.

use std::path::PathBuf;

use crate::error::Result;
use crate::generate::GeneratedFile;
use crate::plan::Step;
use crate::registry::service_def::ServiceDef;

/// `<store-home>/targets/<consumer>.json`.
pub fn target_file_path(store_name: &str, consumer_name: &str) -> Result<PathBuf> {
    Ok(crate::service_home(store_name)?
        .join("targets")
        .join(format!("{consumer_name}.json")))
}

/// Step writing the file_sd scrape target for a `[metrics]`-declaring
/// service. `None` when the def declares no metrics endpoint, or when a
/// host-network service's resolved port isn't known.
///
/// `resolved_host_port` is the host port allocated for the `[metrics]`
/// port entry — only consulted for `host_network = true` services, whose
/// target is `host.containers.internal:<host_port>` (they can't join the
/// store's bridge network, so container DNS doesn't apply).
pub fn scrape_target_step(
    store_name: &str,
    consumer: &ServiceDef,
    resolved_host_port: Option<u16>,
) -> Result<Option<Step>> {
    let Some(metrics) = &consumer.metrics else {
        return Ok(None);
    };
    let name = &consumer.service.name;
    let target = if metrics.host_network {
        let Some(host_port) = resolved_host_port else {
            // Without the resolved port there is nothing valid to write.
            // Reached only when an installed service's .env is missing
            // its SERVICE_PORT_* line — a broken install that `ryra
            // doctor` flags; don't compound it with a bogus target.
            return Ok(None);
        };
        format!("host.containers.internal:{host_port}")
    } else {
        let Some(port) = consumer.ports.iter().find(|p| p.name == metrics.port) else {
            // validate() rejects this at load time; never reached for
            // defs that came through the normal parse path.
            return Ok(None);
        };
        format!("{name}:{}", port.container_port)
    };
    let content = format!(
        "[{{\"targets\": [\"{target}\"], \"labels\": {{\"service\": \"{name}\", \"__metrics_path__\": \"{path}\"}}}}]\n",
        path = metrics.path,
    );
    Ok(Some(Step::WriteFile(GeneratedFile {
        path: target_file_path(store_name, name)?,
        content,
    })))
}

/// `<dashboard-home>/provisioning-datasources/ryra-<store>.yml`.
pub fn datasource_file_path(dashboard_name: &str, store_name: &str) -> Result<PathBuf> {
    Ok(crate::service_home(dashboard_name)?
        .join("provisioning-datasources")
        .join(format!("ryra-{store_name}.yml")))
}

/// Step provisioning a datasource on a dashboard provider, pointing at
/// the store's container on the shared network. `store_container_port`
/// is the store's primary container port (e.g. 9090). The store speaks
/// the prometheus query API — that's what `metrics-store` means today.
pub fn datasource_step(
    dashboard_name: &str,
    store_name: &str,
    store_container_port: u16,
) -> Result<Step> {
    let content = format!(
        "# Managed by ryra - datasource for the installed metrics store.\n\
         apiVersion: 1\n\
         datasources:\n\
         \x20 - name: {store_name}\n\
         \x20   type: prometheus\n\
         \x20   access: proxy\n\
         \x20   url: http://{store_name}:{store_container_port}\n\
         \x20   isDefault: true\n"
    );
    Ok(Step::WriteFile(GeneratedFile {
        path: datasource_file_path(dashboard_name, store_name)?,
        content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::service_def::ServiceDef;

    fn def_with_metrics(name: &str, port_name: &str, container_port: u16) -> ServiceDef {
        toml::from_str(&format!(
            "[service]\nname = \"{name}\"\ndescription = \"x\"\n\n\
             [[ports]]\nname = \"{port_name}\"\ncontainer_port = {container_port}\n\n\
             [metrics]\nport = \"{port_name}\"\n"
        ))
        .unwrap_or_else(|e| unreachable!("minimal def must parse: {e}"))
    }

    #[test]
    fn scrape_target_uses_container_port_and_default_path() {
        let def = def_with_metrics("forgejo", "http", 3000);
        let step = scrape_target_step("prometheus", &def, Some(38123))
            .unwrap_or_else(|e| unreachable!("step build should not fail: {e}"));
        let Some(Step::WriteFile(file)) = step else {
            unreachable!("expected a WriteFile step")
        };
        // Bridge-network service: container DNS + container port; the
        // resolved host port is irrelevant.
        assert!(file.content.contains("\"forgejo:3000\""));
        assert!(file.content.contains("\"__metrics_path__\": \"/metrics\""));
        assert!(file.path.ends_with("prometheus/targets/forgejo.json"));
    }

    #[test]
    fn host_network_target_uses_host_gateway_and_host_port() {
        let mut def = def_with_metrics("node-exporter", "http", 9100);
        if let Some(m) = def.metrics.as_mut() {
            m.host_network = true;
        }
        let step = scrape_target_step("prometheus", &def, Some(9100))
            .unwrap_or_else(|e| unreachable!("step build should not fail: {e}"));
        let Some(Step::WriteFile(file)) = step else {
            unreachable!("expected a WriteFile step")
        };
        assert!(file.content.contains("\"host.containers.internal:9100\""));

        // Unknown host port → no target rather than a bogus one.
        let none = scrape_target_step("prometheus", &def, None)
            .unwrap_or_else(|e| unreachable!("step build should not fail: {e}"));
        assert!(none.is_none());
    }

    #[test]
    fn no_metrics_decl_no_step() {
        let mut def = def_with_metrics("plain", "http", 80);
        def.metrics = None;
        let step = scrape_target_step("prometheus", &def, None)
            .unwrap_or_else(|e| unreachable!("step build should not fail: {e}"));
        assert!(step.is_none());
    }

    #[test]
    fn datasource_points_at_store_container() {
        let step = datasource_step("grafana", "prometheus", 9090)
            .unwrap_or_else(|e| unreachable!("step build should not fail: {e}"));
        let Step::WriteFile(file) = step else {
            unreachable!("expected a WriteFile step")
        };
        assert!(file.content.contains("url: http://prometheus:9090"));
        assert!(file.content.contains("type: prometheus"));
        assert!(
            file.path
                .ends_with("grafana/provisioning-datasources/ryra-prometheus.yml")
        );
    }
}
