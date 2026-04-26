use std::path::Path;

use crate::error::{Error, Result};
use crate::generate::GeneratedFile;

/// Parameters for [`process_quadlet_bundle`].
pub struct ProcessBundleParams<'a> {
    pub service_dir: &'a Path,
    pub service_name: &'a str,
    pub quadlet_dir: &'a Path,
    pub extra_networks: &'a [String],
    pub extra_volumes: &'a [String],
    /// Extra args passed via `PodmanArgs=` in the quadlet.
    pub podman_args: &'a [String],
    /// Extra ExecStartPre commands to inject into [Service] section.
    pub extra_exec_start_pre: &'a [String],
    /// Port variable expansions (e.g., `RYRA_PORT_HTTP` → `8080`).
    /// Quadlet `PublishPort=${VAR}:container_port` directives need literal
    /// values because systemd doesn't expand EnvironmentFile vars in directives.
    pub port_vars: &'a [(String, String)],
}

/// Result of processing a quadlet bundle from the registry.
#[derive(Debug)]
pub struct ProcessedBundle {
    pub quadlet_files: Vec<GeneratedFile>,
    pub config_files: Vec<GeneratedFile>,
    pub images: Vec<String>,
    /// Host directories that must exist before containers start (bind mount sources).
    pub bind_mount_dirs: Vec<std::path::PathBuf>,
    /// Vendored files (src, dst) to copy raw from the registry into
    /// service_home. Kept separate from `config_files` because the config
    /// pipeline is UTF-8 only (template rendering) — DLLs, archives and
    /// other binaries don't fit there. The `files/` subtree mirrors
    /// service_home paths.
    pub files: Vec<(std::path::PathBuf, std::path::PathBuf)>,
}

/// Scan processed `.container` files for `Image=` lines. Deduplicate.
pub fn extract_images(files: &[GeneratedFile]) -> Vec<String> {
    let mut images = Vec::new();
    for file in files {
        let path_str = file.path.to_string_lossy();
        if !path_str.ends_with(".container") {
            continue;
        }
        for line in file.content.lines() {
            let trimmed = line.trim();
            if let Some(image) = trimmed.strip_prefix("Image=") {
                let image = image.trim().to_string();
                if !image.is_empty() && !images.contains(&image) {
                    images.push(image);
                }
            }
        }
    }
    images
}

/// Extract host directories from bind mount `Volume=` lines in `.container` files.
/// Bind mounts are `Volume=/host/path:/container/path:flags` (NOT `.volume:` references).
/// These directories must exist before the container starts.
///
/// Expands `%h` to the user's home directory (systemd specifier).
/// File bind mounts (host path has a file extension like `.crt`, `.yml`) are skipped —
/// only directory mounts need pre-creation.
pub fn extract_bind_mount_dirs(
    files: &[GeneratedFile],
) -> crate::error::Result<Vec<std::path::PathBuf>> {
    let home = crate::home_dir()?;
    let mut dirs = Vec::new();
    for file in files {
        let path_str = file.path.to_string_lossy();
        if !path_str.ends_with(".container") {
            continue;
        }
        for line in file.content.lines() {
            let trimmed = line.trim();
            if let Some(vol) = trimmed.strip_prefix("Volume=") {
                // Skip named volume references (e.g., "myvolume.volume:/path:U")
                if vol.contains(".volume:") {
                    continue;
                }
                // Bind mount format: /host/path:/container/path[:flags]
                if let Some(colon_pos) = vol.find(':') {
                    let host_path = &vol[..colon_pos];
                    if host_path.is_empty() {
                        continue;
                    }
                    // Expand %h systemd specifier to actual home directory
                    let expanded = host_path.replace("%h", &home.to_string_lossy());
                    // Skip file bind mounts — only directories need pre-creation.
                    let path = std::path::Path::new(&expanded);
                    if path.extension().is_some() {
                        continue;
                    }
                    dirs.push(std::path::PathBuf::from(expanded));
                }
            }
        }
    }
    Ok(dirs)
}

/// Append `Network=<name>.network` lines to the `[Container]` section of a quadlet file.
/// Inserts just before `[Service]` section if it exists, otherwise appends at end.
///
/// Each entry in `networks` is a network name optionally followed by `:` and
/// extra options (e.g., `"authelia:alias=auth.test.local"` →
/// `Network=authelia.network:alias=auth.test.local`).
pub fn inject_networks(content: &str, networks: &[String]) -> String {
    if networks.is_empty() {
        return content.to_string();
    }
    let extra_lines: String = networks
        .iter()
        .map(|n| {
            if let Some((name, opts)) = n.split_once(':') {
                format!("Network={name}.network:{opts}")
            } else {
                format!("Network={n}.network")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    inject_before_section(content, &extra_lines, "[Service]")
}

/// Append `PodmanArgs=` to the `[Container]` section of a quadlet file.
pub fn inject_podman_args(content: &str, args: &[String]) -> String {
    if args.is_empty() {
        return content.to_string();
    }
    let line = format!("PodmanArgs={}", args.join(" "));
    inject_before_section(content, &line, "[Service]")
}

/// Append `Volume=` lines to the `[Container]` section of a quadlet file.
/// Inserts just before `[Service]` section if it exists, otherwise appends at end.
pub fn inject_extra_volumes(content: &str, volumes: &[String]) -> String {
    if volumes.is_empty() {
        return content.to_string();
    }
    let extra_lines: String = volumes
        .iter()
        .map(|v| format!("Volume={v}"))
        .collect::<Vec<_>>()
        .join("\n");

    inject_before_section(content, &extra_lines, "[Service]")
}

/// Insert `extra_lines` just before the line matching `section_header`, or append at end.
fn inject_before_section(content: &str, extra_lines: &str, section_header: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    let insert_pos = lines.iter().position(|l| l.trim() == section_header);

    match insert_pos {
        Some(pos) => {
            // Insert extra lines before the section header, with a blank line separator if needed
            let needs_blank = pos > 0 && !lines[pos - 1].trim().is_empty();
            let mut insert = Vec::new();
            if needs_blank {
                insert.push("");
            }
            for line in extra_lines.lines() {
                insert.push(line);
            }
            // Splice in the extra lines
            for (i, line) in insert.iter().enumerate() {
                lines.insert(pos + i, line);
            }
            let mut result = lines.join("\n");
            // Preserve trailing newline if original had one
            if content.ends_with('\n') {
                result.push('\n');
            }
            result
        }
        None => {
            // No section header found — append at end
            let mut result = content.to_string();
            if !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(extra_lines);
            result.push('\n');
            result
        }
    }
}

/// Main entry point: read all quadlet files from `<service_dir>/quadlets/`,
/// apply substitutions and injections, extract images, process config files.
pub fn process_quadlet_bundle(params: &ProcessBundleParams<'_>) -> Result<ProcessedBundle> {
    let quadlets_dir = params.service_dir.join("quadlets");

    if !quadlets_dir.is_dir() {
        return Err(Error::Bundle(format!(
            "quadlets/ directory not found for service '{}'",
            params.service_name
        )));
    }

    let mut quadlet_files = Vec::new();

    let entries = std::fs::read_dir(&quadlets_dir).map_err(|source| Error::FileRead {
        path: quadlets_dir.clone(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: quadlets_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let mut content = std::fs::read_to_string(&path).map_err(|source| Error::FileRead {
            path: path.clone(),
            source,
        })?;

        let file_name = path
            .file_name()
            .ok_or_else(|| Error::Bundle(format!("invalid file path: {}", path.display())))?
            .to_string_lossy();

        // Only inject networks/volumes into .container files
        if file_name.ends_with(".container") {
            content = inject_networks(&content, params.extra_networks);
            content = inject_extra_volumes(&content, params.extra_volumes);
            content = inject_podman_args(&content, params.podman_args);
            // Inject ExecStartPre into the main service container
            // (the one named <service>.container, not sidecars)
            let is_main_container = file_name == format!("{}.container", params.service_name);
            if is_main_container {
                for cmd in params.extra_exec_start_pre {
                    content = inject_before_section(
                        &content,
                        &format!("ExecStartPre={cmd}"),
                        "[Install]",
                    );
                }
            }
            // Expand ${RYRA_PORT_*} in PublishPort lines — systemd doesn't
            // expand EnvironmentFile vars in quadlet directives.
            for (var, val) in params.port_vars {
                content = content.replace(&format!("${{{var}}}"), val);
            }
        }

        quadlet_files.push(GeneratedFile {
            path: params.quadlet_dir.join(file_name.as_ref()),
            content,
        });
    }

    if quadlet_files.is_empty() {
        return Err(Error::Bundle(format!(
            "no quadlet files found in quadlets/ for service '{}'",
            params.service_name
        )));
    }

    // Sort for deterministic ordering
    quadlet_files.sort_by(|a, b| a.path.cmp(&b.path));

    let images = extract_images(&quadlet_files);
    let bind_mount_dirs = extract_bind_mount_dirs(&quadlet_files)?;

    let service_home = crate::service_home(params.service_name)?;
    let config_files = process_configs(params.service_dir, &service_home)?;
    let files = collect_files(params.service_dir, &service_home)?;

    Ok(ProcessedBundle {
        quadlet_files,
        config_files,
        images,
        bind_mount_dirs,
        files,
    })
}

/// Collect vendored files from `<service_dir>/files/` recursively. Returns
/// (src, dst) pairs where `src` is the registry path and `dst` is the
/// corresponding path under `service_home`. The CLI copies them with
/// `std::fs::copy` at apply time (binary-safe, no UTF-8 assumption).
pub fn collect_files(
    service_dir: &Path,
    service_home: &Path,
) -> Result<Vec<(std::path::PathBuf, std::path::PathBuf)>> {
    let files_dir = service_dir.join("files");
    if !files_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    collect_files_recursive(&files_dir, &files_dir, service_home, &mut out)?;
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

fn collect_files_recursive(
    base_dir: &Path,
    current_dir: &Path,
    service_home: &Path,
    out: &mut Vec<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<()> {
    let entries = std::fs::read_dir(current_dir).map_err(|source| Error::FileRead {
        path: current_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: current_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(base_dir, &path, service_home, out)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(base_dir)
                .map_err(|e| Error::Bundle(format!("failed to compute relative path: {e}")))?;
            out.push((path.clone(), service_home.join(relative)));
        }
    }
    Ok(())
}

/// Read files from `<service_dir>/configs/` recursively,
/// map them to `<service_home>/configs/<relative_path>`.
pub fn process_configs(service_dir: &Path, service_home: &Path) -> Result<Vec<GeneratedFile>> {
    let configs_dir = service_dir.join("configs");
    if !configs_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_configs_recursive(&configs_dir, &configs_dir, service_home, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn collect_configs_recursive(
    base_dir: &Path,
    current_dir: &Path,
    service_home: &Path,
    files: &mut Vec<GeneratedFile>,
) -> Result<()> {
    let entries = std::fs::read_dir(current_dir).map_err(|source| Error::FileRead {
        path: current_dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: current_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();

        if path.is_dir() {
            collect_configs_recursive(base_dir, &path, service_home, files)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(base_dir)
                .map_err(|e| Error::Bundle(format!("failed to compute relative path: {e}")))?;

            let content = std::fs::read_to_string(&path).map_err(|source| Error::FileRead {
                path: path.clone(),
                source,
            })?;

            files.push(GeneratedFile {
                path: service_home.join("configs").join(relative),
                content,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_images_from_container_files_only() {
        let files = vec![
            GeneratedFile {
                path: PathBuf::from("/q/myapp.container"),
                content: "[Container]\nImage=docker.io/library/nginx:latest\n".to_string(),
            },
            GeneratedFile {
                path: PathBuf::from("/q/myapp.network"),
                content: "[Network]\nImage=should-be-ignored\n".to_string(),
            },
            GeneratedFile {
                path: PathBuf::from("/q/myapp-db.container"),
                content: "[Container]\nImage=docker.io/library/postgres:16\n".to_string(),
            },
        ];
        let images = extract_images(&files);
        assert_eq!(
            images,
            vec![
                "docker.io/library/nginx:latest".to_string(),
                "docker.io/library/postgres:16".to_string(),
            ]
        );
    }

    #[test]
    fn extract_images_deduplicates() {
        let files = vec![
            GeneratedFile {
                path: PathBuf::from("/q/a.container"),
                content: "Image=docker.io/img:1\n".to_string(),
            },
            GeneratedFile {
                path: PathBuf::from("/q/b.container"),
                content: "Image=docker.io/img:1\nImage=docker.io/img:2\n".to_string(),
            },
        ];
        let images = extract_images(&files);
        assert_eq!(
            images,
            vec!["docker.io/img:1".to_string(), "docker.io/img:2".to_string(),]
        );
    }

    #[test]
    fn inject_networks_before_service_section() {
        let content = "[Container]\nImage=nginx\n\n[Service]\nRestart=always\n";
        let result = inject_networks(content, &["caddy".to_string(), "auth".to_string()]);
        assert_eq!(
            result,
            "[Container]\nImage=nginx\n\nNetwork=caddy.network\nNetwork=auth.network\n[Service]\nRestart=always\n"
        );
    }

    #[test]
    fn inject_networks_no_service_section_appends() {
        let content = "[Container]\nImage=nginx\n";
        let result = inject_networks(content, &["caddy".to_string()]);
        assert_eq!(result, "[Container]\nImage=nginx\nNetwork=caddy.network\n");
    }

    #[test]
    fn inject_extra_volumes_before_service_section() {
        let content = "[Container]\nImage=nginx\n\n[Service]\nRestart=always\n";
        let result =
            inject_extra_volumes(content, &["/host/ca.crt:/etc/ssl/ca.crt:ro".to_string()]);
        assert_eq!(
            result,
            "[Container]\nImage=nginx\n\nVolume=/host/ca.crt:/etc/ssl/ca.crt:ro\n[Service]\nRestart=always\n"
        );
    }

    #[test]
    fn inject_extra_volumes_no_service_section_appends() {
        let content = "[Container]\nImage=nginx";
        let result = inject_extra_volumes(content, &["/a:/b".to_string()]);
        assert_eq!(result, "[Container]\nImage=nginx\nVolume=/a:/b\n");
    }

    #[test]
    fn inject_networks_adds_blank_line_when_needed() {
        let content =
            "[Container]\nImage=nginx\nNetwork=mynet.network\n[Service]\nRestart=always\n";
        let result = inject_networks(content, &["caddy".to_string()]);
        // Should insert blank line before injected content when previous line is not blank
        assert_eq!(
            result,
            "[Container]\nImage=nginx\nNetwork=mynet.network\n\nNetwork=caddy.network\n[Service]\nRestart=always\n"
        );
    }

    #[test]
    fn process_quadlet_bundle_errors_on_missing_dir() {
        let params = ProcessBundleParams {
            service_dir: Path::new("/nonexistent"),
            service_name: "test",
            quadlet_dir: Path::new("/home/user/.config/containers/systemd"),
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_vars: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("quadlets/ directory not found"));
    }

    #[test]
    fn process_quadlet_bundle_reads_and_processes_files() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("myservice");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));

        std::fs::write(
            quadlets_dir.join("app.container"),
            "[Container]\nImage=nginx:latest\nVolume=%h/.local/share/ryra/myservice/data:/data\n\n[Service]\nRestart=always\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        std::fs::write(
            quadlets_dir.join("app.network"),
            "[Network]\nDriver=bridge\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let quadlet_dir = Path::new("/home/user/.config/containers/systemd");

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "myservice",
            quadlet_dir,
            extra_networks: &["caddy".to_string()],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_vars: &[],
        };

        let bundle = process_quadlet_bundle(&params)
            .unwrap_or_else(|e| unreachable!("process_quadlet_bundle should not fail: {e}"));

        assert_eq!(bundle.quadlet_files.len(), 2);
        assert_eq!(bundle.images, vec!["nginx:latest".to_string()]);

        // Check content is preserved as-is (no placeholder substitution)
        let container_file = bundle
            .quadlet_files
            .iter()
            .find(|f| f.path.to_string_lossy().ends_with(".container"))
            .unwrap_or_else(|| unreachable!("container file must exist"));
        assert!(
            container_file
                .content
                .contains("%h/.local/share/ryra/myservice/data:/data")
        );
        // Check network injection happened
        assert!(container_file.content.contains("Network=caddy.network"));

        // Network file should NOT have network injection
        let network_file = bundle
            .quadlet_files
            .iter()
            .find(|f| f.path.to_string_lossy().ends_with(".network"))
            .unwrap_or_else(|| unreachable!("network file must exist"));
        assert!(!network_file.content.contains("Network=caddy.network"));
    }

    #[test]
    fn process_quadlet_bundle_errors_on_empty_dir() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("empty");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "empty",
            quadlet_dir: Path::new("/home/user/.config/containers/systemd"),
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_vars: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("no quadlet files found"));
    }

    #[test]
    fn process_configs_reads_recursively() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        let configs_dir = service_dir.join("configs");
        let sub_dir = configs_dir.join("subdir");
        std::fs::create_dir_all(&sub_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));

        std::fs::write(configs_dir.join("main.conf"), "data_dir=/some/path\n")
            .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        std::fs::write(sub_dir.join("nested.conf"), "no placeholders\n")
            .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let service_home = Path::new("/home/user/.local/share/ryra/svc");

        let files = process_configs(&service_dir, service_home)
            .unwrap_or_else(|e| unreachable!("process_configs should not fail: {e}"));

        assert_eq!(files.len(), 2);

        let main_conf = files
            .iter()
            .find(|f| f.path.ends_with("main.conf"))
            .unwrap_or_else(|| unreachable!("main.conf must exist"));
        assert_eq!(
            main_conf.path,
            PathBuf::from("/home/user/.local/share/ryra/svc/configs/main.conf")
        );
        assert!(main_conf.content.contains("/some/path"));

        let nested_conf = files
            .iter()
            .find(|f| f.path.ends_with("nested.conf"))
            .unwrap_or_else(|| unreachable!("nested.conf must exist"));
        assert_eq!(
            nested_conf.path,
            PathBuf::from("/home/user/.local/share/ryra/svc/configs/subdir/nested.conf")
        );
        assert_eq!(nested_conf.content, "no placeholders\n");
    }

    #[test]
    fn extract_bind_mount_dirs_finds_host_paths() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let files = vec![
            GeneratedFile {
                path: PathBuf::from("/q/immich.container"),
                content: "Volume=%h/.local/share/ryra/immich/upload:/data:Z\nVolume=immich-db-data.volume:/var/lib/postgresql/data:U\n".to_string(),
            },
            GeneratedFile {
                path: PathBuf::from("/q/immich.network"),
                content: "[Network]\n".to_string(),
            },
        ];
        let dirs = extract_bind_mount_dirs(&files).unwrap();
        assert_eq!(
            dirs,
            vec![PathBuf::from(format!(
                "{home}/.local/share/ryra/immich/upload"
            ))]
        );
    }

    #[test]
    fn extract_bind_mount_dirs_skips_named_volumes() {
        let files = vec![GeneratedFile {
            path: PathBuf::from("/q/svc.container"),
            content: "Volume=svc-data.volume:/data:U\n".to_string(),
        }];
        let dirs = extract_bind_mount_dirs(&files).unwrap();
        assert!(dirs.is_empty());
    }

    #[test]
    fn extract_bind_mount_dirs_skips_file_mounts() {
        let files = vec![GeneratedFile {
            path: PathBuf::from("/q/svc.container"),
            content: "Volume=/path/to/ca.crt:/etc/ssl/certs/ca.crt:ro,Z\nVolume=/path/to/config:/config:Z\n".to_string(),
        }];
        let dirs = extract_bind_mount_dirs(&files).unwrap();
        // Only the directory mount, not the .crt file mount
        assert_eq!(dirs, vec![PathBuf::from("/path/to/config")]);
    }

    #[test]
    fn process_configs_returns_empty_when_no_configs_dir() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        std::fs::create_dir_all(&service_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));

        let files = process_configs(&service_dir, Path::new("/home/user/.local/share/ryra/svc"))
            .unwrap_or_else(|e| unreachable!("process_configs should not fail: {e}"));

        assert!(files.is_empty());
    }
}
