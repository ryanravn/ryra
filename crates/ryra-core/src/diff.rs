use std::path::Path;

use crate::config::{self, ConfigPaths};
use crate::error::Result;
use crate::registry;
use crate::registry::service_def::{AuthKind, ServiceDef};

/// A single change between the installed snapshot and the current registry.
#[derive(Debug)]
pub enum Change {
    ImageChanged {
        old: String,
        new: String,
    },
    PortAdded {
        name: String,
        port: u16,
    },
    PortRemoved {
        name: String,
        port: u16,
    },
    PortChanged {
        name: String,
        old: u16,
        new: u16,
    },
    VolumeAdded {
        name: String,
        mount_path: String,
    },
    VolumeRemoved {
        name: String,
        mount_path: String,
    },
    EnvAdded {
        name: String,
    },
    EnvRemoved {
        name: String,
    },
    EnvDefaultChanged {
        name: String,
        old: String,
        new: String,
    },
    SmtpIntegrationChanged {
        old: bool,
        new: bool,
    },
    AuthIntegrationChanged {
        old: Vec<AuthKind>,
        new: Vec<AuthKind>,
    },
    DescriptionChanged {
        old: String,
        new: String,
    },
    RequirementsChanged {
        description: String,
    },
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Change::ImageChanged { old, new } => write!(f, "image: {old} → {new}"),
            Change::PortAdded { name, port } => write!(f, "port added: {name} ({port})"),
            Change::PortRemoved { name, port } => write!(f, "port removed: {name} ({port})"),
            Change::PortChanged { name, old, new } => {
                write!(f, "port changed: {name} ({old} → {new})")
            }
            Change::VolumeAdded { name, mount_path } => {
                write!(f, "volume added: {name} → {mount_path}")
            }
            Change::VolumeRemoved { name, mount_path } => {
                write!(f, "volume removed: {name} → {mount_path}")
            }
            Change::EnvAdded { name } => write!(f, "env var added: {name}"),
            Change::EnvRemoved { name } => write!(f, "env var removed: {name}"),
            Change::EnvDefaultChanged { name, old, new } => {
                write!(f, "env default changed: {name} ({old} → {new})")
            }
            Change::SmtpIntegrationChanged { old, new } => {
                write!(f, "smtp integration: {old} → {new}")
            }
            Change::AuthIntegrationChanged { old, new } => {
                let fmt = |kinds: &[AuthKind]| {
                    if kinds.is_empty() {
                        "none".to_string()
                    } else {
                        kinds
                            .iter()
                            .map(|k| k.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                write!(f, "auth integration: {} → {}", fmt(old), fmt(new))
            }
            Change::DescriptionChanged { old, new } => {
                write!(f, "description: \"{old}\" → \"{new}\"")
            }
            Change::RequirementsChanged { description } => {
                write!(f, "requirements: {description}")
            }
        }
    }
}

/// Compare the installed snapshot of a service against the current registry version.
pub async fn diff_service(service_name: &str, repo: Option<&str>) -> Result<Vec<Change>> {
    let paths = ConfigPaths::resolve()?;

    // Load the snapshot from install time
    let snapshot_content = config::load_snapshot(&paths.snapshots_dir, service_name)?;
    let old: ServiceDef =
        toml::from_str(&snapshot_content).map_err(|source| crate::error::Error::TomlParse {
            path: paths.snapshots_dir.join(format!("{service_name}.toml")),
            source,
        })?;

    // Load the current version from the registry
    let (_repo_url, repo_dir) = crate::resolve_repo(repo).await?;
    let current = registry::find_service(&repo_dir, service_name)?;
    let new = &current.def;

    Ok(compute_changes(&old, new))
}

/// Load a snapshot from a specific path (for testing or custom paths).
pub fn diff_service_from_paths(
    snapshot_path: &Path,
    registry_path: &Path,
    service_name: &str,
) -> Result<Vec<Change>> {
    let snapshot_content =
        std::fs::read_to_string(snapshot_path).map_err(|source| crate::error::Error::FileRead {
            path: snapshot_path.to_path_buf(),
            source,
        })?;
    let old: ServiceDef =
        toml::from_str(&snapshot_content).map_err(|source| crate::error::Error::TomlParse {
            path: snapshot_path.to_path_buf(),
            source,
        })?;

    let current = registry::find_service(registry_path, service_name)?;
    Ok(compute_changes(&old, &current.def))
}

pub fn compute_changes(old: &ServiceDef, new: &ServiceDef) -> Vec<Change> {
    let mut changes = Vec::new();

    // Image
    if old.service.image != new.service.image {
        changes.push(Change::ImageChanged {
            old: old.service.image.clone(),
            new: new.service.image.clone(),
        });
    }

    // Description
    if old.service.description != new.service.description {
        changes.push(Change::DescriptionChanged {
            old: old.service.description.clone(),
            new: new.service.description.clone(),
        });
    }

    // Ports
    for new_port in &new.ports {
        match old.ports.iter().find(|p| p.name == new_port.name) {
            Some(old_port) if old_port.container_port != new_port.container_port => {
                changes.push(Change::PortChanged {
                    name: new_port.name.clone(),
                    old: old_port.container_port,
                    new: new_port.container_port,
                });
            }
            None => {
                changes.push(Change::PortAdded {
                    name: new_port.name.clone(),
                    port: new_port.container_port,
                });
            }
            _ => {}
        }
    }
    for old_port in &old.ports {
        if !new.ports.iter().any(|p| p.name == old_port.name) {
            changes.push(Change::PortRemoved {
                name: old_port.name.clone(),
                port: old_port.container_port,
            });
        }
    }

    // Volumes
    for new_vol in &new.volumes {
        if !old.volumes.iter().any(|v| v.name == new_vol.name) {
            changes.push(Change::VolumeAdded {
                name: new_vol.name.clone(),
                mount_path: new_vol.mount_path.clone(),
            });
        }
    }
    for old_vol in &old.volumes {
        if !new.volumes.iter().any(|v| v.name == old_vol.name) {
            changes.push(Change::VolumeRemoved {
                name: old_vol.name.clone(),
                mount_path: old_vol.mount_path.clone(),
            });
        }
    }

    // Env vars
    for new_env in &new.env {
        match old.env.iter().find(|e| e.name == new_env.name) {
            Some(old_env) if old_env.value != new_env.value => {
                changes.push(Change::EnvDefaultChanged {
                    name: new_env.name.clone(),
                    old: old_env.value.clone(),
                    new: new_env.value.clone(),
                });
            }
            None => {
                changes.push(Change::EnvAdded {
                    name: new_env.name.clone(),
                });
            }
            _ => {}
        }
    }
    for old_env in &old.env {
        if !new.env.iter().any(|e| e.name == old_env.name) {
            changes.push(Change::EnvRemoved {
                name: old_env.name.clone(),
            });
        }
    }

    // Integrations
    if old.integrations.smtp != new.integrations.smtp {
        changes.push(Change::SmtpIntegrationChanged {
            old: old.integrations.smtp,
            new: new.integrations.smtp,
        });
    }
    if old.integrations.auth != new.integrations.auth {
        changes.push(Change::AuthIntegrationChanged {
            old: old.integrations.auth.clone(),
            new: new.integrations.auth.clone(),
        });
    }

    // Requirements
    match (&old.requirements, &new.requirements) {
        (None, Some(req)) => {
            changes.push(Change::RequirementsChanged {
                description: format!("added (min {}MB RAM)", req.ram.min),
            });
        }
        (Some(_), None) => {
            changes.push(Change::RequirementsChanged {
                description: "removed".to_string(),
            });
        }
        (Some(old_req), Some(new_req)) => {
            if old_req.ram.min != new_req.ram.min
                || old_req.ram.recommended != new_req.ram.recommended
            {
                changes.push(Change::RequirementsChanged {
                    description: format!("RAM min {}MB → {}MB", old_req.ram.min, new_req.ram.min),
                });
            }
        }
        _ => {}
    }

    changes
}
