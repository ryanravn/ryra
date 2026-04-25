//! Prometheus scrape-target registration via file-based service discovery.
//!
//! Prometheus watches a directory of JSON target files (`file_sd_configs`) and
//! reloads them automatically — no SIGHUP, no marker parsing, no string edits.
//! Each service that declares `prometheus = true` drops a single JSON file on
//! install and removes it on uninstall.

use std::path::PathBuf;

use crate::error::Result;
use crate::generate::GeneratedFile;
use crate::registry::service_def::ServiceDef;
use crate::{Step, WellKnownService, service_home};

/// Directory prometheus watches for target files, inside the prometheus
/// service's data dir. Bind-mounted into the container at `/etc/prometheus/targets`.
pub fn targets_dir() -> Result<PathBuf> {
    Ok(service_home(WellKnownService::Prometheus.as_str())?.join("targets"))
}

/// Path to a specific service's scrape target file.
pub fn target_file(service_name: &str) -> Result<PathBuf> {
    Ok(targets_dir()?.join(format!("{service_name}.json")))
}

/// Build the scrape-target JSON for a service. Prometheus reaches it via
/// container DNS on the shared `prometheus` network — no host port needed.
fn render_target(service_name: &str, container_port: u16) -> String {
    // Prometheus file_sd format: an array of target groups. We emit one group
    // per service with a single target so remove is a trivial file delete.
    format!(
        "[{{\"targets\":[\"{service_name}:{container_port}\"],\"labels\":{{\"service\":\"{service_name}\"}}}}]\n"
    )
}

/// Emit steps to register a service as a Prometheus scrape target.
///
/// No-op if prometheus isn't installed or the service doesn't declare
/// `[integrations].prometheus`. The scraped port is the first `[[ports]]`
/// entry's `container_port` — services expose metrics on their primary port.
pub fn register_scrape_target(
    service_name: &str,
    service_def: &ServiceDef,
    prometheus_installed: bool,
) -> Result<Vec<Step>> {
    if !service_def.integrations.prometheus || !prometheus_installed {
        return Ok(Vec::new());
    }
    let container_port = match service_def.ports.first() {
        Some(p) => p.container_port,
        None => return Ok(Vec::new()),
    };
    Ok(vec![Step::WriteFile(GeneratedFile {
        path: target_file(service_name)?,
        content: render_target(service_name, container_port),
    })])
}

/// Emit steps to remove a service's scrape target.
///
/// No-op if the target file doesn't exist. Safe to call when prometheus
/// was never installed — `Step::RemoveFile` is `rm -f` under the hood.
pub fn unregister_scrape_target(service_name: &str) -> Result<Vec<Step>> {
    let path = target_file(service_name)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(vec![Step::RemoveFile(path)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_json_uses_container_dns() {
        let out = render_target("forgejo", 3000);
        assert!(out.contains("\"forgejo:3000\""));
        assert!(out.contains("\"service\":\"forgejo\""));
    }
}
