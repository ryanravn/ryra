use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

/// Generate index.json from a registry directory of service.toml files.
///
/// Usage: generate-registry [registry_dir] [output_path]
///
/// Defaults: registry_dir = ./registry, output_path = stdout
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let registry_dir = match args.get(1) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("registry"),
    };

    if !registry_dir.is_dir() {
        bail!("registry directory not found: {}", registry_dir.display());
    }

    let services = ryra_core::registry::list_available(&registry_dir)?;

    let map: BTreeMap<String, &ryra_core::registry::service_def::ServiceDef> = services
        .iter()
        .map(|s| (s.def.service.name.clone(), &s.def))
        .collect();

    #[derive(serde::Serialize)]
    struct RegistryJson<'a> {
        services: BTreeMap<String, &'a ryra_core::registry::service_def::ServiceDef>,
    }

    let json = serde_json::to_string_pretty(&RegistryJson { services: map })?;

    match args.get(2) {
        Some(output) => {
            std::fs::write(output, &json)?;
            eprintln!("Generated {} with {} services", output, services.len());
        }
        None => print!("{json}"),
    }

    Ok(())
}
