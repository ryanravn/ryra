pub mod config;
pub mod error;
pub mod generate;
pub mod integrations;
pub mod registry;
pub mod system;

use std::path::Path;

use config::schema::{Config, InstalledService, RegistryEntry};
use config::state::State;
use config::ConfigPaths;
use error::{Error, Result};

/// Initialize a new ryra project: write config, set up nginx quadlet, fetch registries.
pub async fn init(config: Config) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    paths.ensure_dirs()?;

    // Write config
    config::save_config(&paths.config_file, &config)?;
    config::save_state(&paths.state_file, &State::default())?;

    // Set up nginx directories and base config
    write_file_sudo("/etc/ryra/nginx/sites/.keep", "")?;
    write_file_sudo(
        "/etc/ryra/nginx/nginx.conf",
        &generate::nginx::render_nginx_base_conf(),
    )?;

    // Write the nginx quadlet (root-level systemd)
    write_file_sudo(
        "/etc/containers/systemd/ryra-nginx.container",
        &generate::nginx::render_nginx_quadlet(),
    )?;

    // Fetch registries
    for reg in &config.registries {
        registry::fetch::fetch_registry(&reg.url, &paths.cache_dir, &reg.name).await?;
    }

    // Start nginx
    system::nginx::daemon_reload_system().await?;
    system::nginx::start().await?;

    Ok(())
}

/// Add a service: generate quadlets + nginx config, start it.
pub async fn add_service(service_name: &str, domain: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;
    let mut state = config::load_state(&paths.state_file)?;

    // Check not already installed
    if config.services.iter().any(|s| s.name == service_name) {
        return Err(Error::ServiceAlreadyInstalled(service_name.to_string()));
    }

    // Find service definition
    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();
    let reg_service = registry::find_service(&paths.cache_dir, &reg_pairs, service_name)?;

    // Determine quadlet directory
    let quadlet_dir = dirs::config_dir()
        .expect("no config dir")
        .join("containers/systemd");
    std::fs::create_dir_all(&quadlet_dir).map_err(|source| Error::DirCreate {
        path: quadlet_dir.clone(),
        source,
    })?;

    let nginx_dir = Path::new("/etc/ryra/nginx/sites");

    // Generate all files
    let generated = generate::generate_service(
        &config,
        &mut state,
        &reg_service.def,
        domain,
        &quadlet_dir,
        nginx_dir,
    )?;

    // Write quadlet files
    for file in &generated.quadlet_files {
        std::fs::write(&file.path, &file.content).map_err(|source| Error::FileWrite {
            path: file.path.clone(),
            source,
        })?;
    }

    // Write nginx site config (requires sudo)
    if let Some(site) = &generated.nginx_site {
        write_file_sudo(&site.path.to_string_lossy(), &site.content)?;
    }

    // Save state
    config::save_state(&paths.state_file, &state)?;

    // Record in config
    config.services.push(InstalledService {
        name: service_name.to_string(),
        domain: domain.to_string(),
        version: "0.1.0".to_string(),
    });
    config::save_config(&paths.config_file, &config)?;

    // Start service
    system::podman::daemon_reload().await?;
    system::podman::start_service(service_name).await?;
    system::nginx::reload().await?;

    Ok(())
}

/// Remove a service: stop it, delete generated files, clean up state.
pub async fn remove_service(service_name: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;
    let mut state = config::load_state(&paths.state_file)?;

    // Check it's installed
    if !config.services.iter().any(|s| s.name == service_name) {
        return Err(Error::ServiceNotInstalled(service_name.to_string()));
    }

    // Stop service (ignore errors — may already be stopped)
    let _ = system::podman::stop_service(service_name).await;

    // Remove quadlet files
    let quadlet_dir = dirs::config_dir()
        .expect("no config dir")
        .join("containers/systemd");

    remove_matching_files(&quadlet_dir, service_name);

    // Remove nginx site config
    let nginx_conf = format!("/etc/ryra/nginx/sites/{service_name}.conf");
    let _ = std::process::Command::new("sudo")
        .args(["rm", "-f", &nginx_conf])
        .output();

    // Deallocate ports and secrets
    system::port::deallocate_ports(&mut state, service_name);
    system::secret::remove_secrets(&mut state, service_name);
    config::save_state(&paths.state_file, &state)?;

    // Remove from config
    config.services.retain(|s| s.name != service_name);
    config::save_config(&paths.config_file, &config)?;

    // Reload
    system::podman::daemon_reload().await?;
    system::nginx::reload().await?;

    Ok(())
}

/// List services: what's available vs installed.
pub fn list_services() -> Result<Vec<ServiceStatus>> {
    let paths = ConfigPaths::resolve()?;
    let config = config::load_config(&paths.config_file)?;

    let reg_pairs: Vec<(String, String)> = config
        .registries
        .iter()
        .map(|r| (r.name.clone(), r.url.clone()))
        .collect();

    let available = registry::list_available(&paths.cache_dir, &reg_pairs)?;

    let statuses = available
        .into_iter()
        .map(|reg_svc| {
            let name = &reg_svc.def.service.name;
            let installed = config.services.iter().find(|s| s.name == *name);
            match installed {
                Some(inst) => ServiceStatus::Installed {
                    name: name.clone(),
                    description: reg_svc.def.service.description,
                    domain: inst.domain.clone(),
                },
                None => ServiceStatus::Available {
                    name: name.clone(),
                    description: reg_svc.def.service.description,
                },
            }
        })
        .collect();

    Ok(statuses)
}

/// Add a registry to the config and fetch it.
pub async fn add_registry(name: &str, url: &str) -> Result<()> {
    let paths = ConfigPaths::resolve()?;
    let mut config = config::load_config(&paths.config_file)?;

    if config.registries.iter().any(|r| r.name == name) {
        return Err(Error::RegistryAlreadyExists {
            name: name.to_string(),
        });
    }

    // Determine if it's a local path or git URL
    let source_path = Path::new(url);
    if source_path.exists() && source_path.is_dir() {
        registry::fetch::add_local_registry(source_path, &paths.cache_dir, name)?;
    } else {
        registry::fetch::fetch_registry(url, &paths.cache_dir, name).await?;
    }

    config.registries.push(RegistryEntry {
        name: name.to_string(),
        url: url.to_string(),
    });
    config::save_config(&paths.config_file, &config)?;

    Ok(())
}

// --- Enum for service listing ---

#[derive(Debug)]
pub enum ServiceStatus {
    Available {
        name: String,
        description: String,
    },
    Installed {
        name: String,
        description: String,
        domain: String,
    },
}

// --- Helpers ---

fn write_file_sudo(path: &str, content: &str) -> Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::process::Command::new("sudo")
            .args(["mkdir", "-p", &parent.to_string_lossy()])
            .output();
    }
    let output = std::process::Command::new("sudo")
        .args(["tee", path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(content.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| Error::FileWrite {
            path: path.into(),
            source: e,
        })?;

    if !output.status.success() {
        return Err(Error::FileWrite {
            path: path.into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sudo tee failed"),
        });
    }
    Ok(())
}

fn remove_matching_files(dir: &Path, prefix: &str) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(prefix) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}
