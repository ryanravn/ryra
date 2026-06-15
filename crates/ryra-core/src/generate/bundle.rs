use std::path::Path;

use crate::error::{Error, Result};
use crate::generate::GeneratedFile;

/// Parameters for [`process_quadlet_bundle`].
///
/// Registry quadlets are plain podman files, used exactly as authored.
/// `${SERVICE_PORT_*}` / `${SERVICE_HOME}` are runtime env expansions:
/// quadlet passes them through to the generated `ExecStart=podman run ...`
/// line (podman >= 5.3), where systemd expands them from the `[Service]`
/// section's `EnvironmentFile=` — the `.env` ryra writes. The quadlet
/// therefore also works without ryra: copy it, write the `.env` by hand.
pub struct ProcessBundleParams<'a> {
    pub service_dir: &'a Path,
    pub service_name: &'a str,
    pub extra_networks: &'a [String],
    pub extra_volumes: &'a [String],
    /// Extra args passed via `PodmanArgs=` in the quadlet.
    pub podman_args: &'a [String],
    /// Extra ExecStartPre commands to inject into [Service] section.
    pub extra_exec_start_pre: &'a [String],
    /// Declared `[[ports]]` names from service.toml. Every
    /// `${SERVICE_PORT_<NAME>}` in a quadlet must match one — runtime env
    /// expansion can't catch typos (an undefined var expands to ""), so
    /// this is validated here, at the boundary.
    pub port_names: &'a [String],
    /// Quadlet filenames to skip: those claimed by a `[[choice.option]]`
    /// whose option was not selected. Skipped files are neither processed
    /// nor symlinked, and their `Image=` is never collected, so an
    /// unselected sidecar costs no pull.
    pub excluded_quadlets: &'a [String],
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

/// Validate the runtime env references a quadlet relies on.
///
/// Rejects leftover `{{...}}` template syntax (quadlets are plain podman
/// files, not templates) and any `${SERVICE_PORT_<NAME>}` that doesn't match
/// a declared `[[ports]]` entry. systemd expands an undefined var to an empty
/// string at runtime, which produces a confusingly broken unit — so the typo
/// is caught here, at add time, instead.
fn validate_quadlet_env_refs(
    content: &str,
    file_name: &str,
    params: &ProcessBundleParams<'_>,
) -> Result<()> {
    // Skip comment lines: a comment may legitimately mention `{{...}}` when it
    // explains the `.env` value a directive reads. Only a directive using
    // template syntax is the mistake (ryra renders `.env`, not quadlets).
    if content
        .lines()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("{{"))
    {
        return Err(Error::Bundle(format!(
            "quadlet '{}' for service '{}' contains '{{{{...}}}}' template syntax — quadlets \
             are plain podman files; use runtime env vars (${{SERVICE_PORT_<NAME>}}, \
             ${{SERVICE_HOME}}) instead",
            file_name, params.service_name
        )));
    }
    let mut rest = content;
    while let Some(idx) = rest.find("${SERVICE_PORT_") {
        let tail = &rest[idx + "${SERVICE_PORT_".len()..];
        let Some(end) = tail.find('}') else { break };
        let var_name = &tail[..end];
        if !params
            .port_names
            .iter()
            .any(|p| p.eq_ignore_ascii_case(var_name))
        {
            return Err(Error::Bundle(format!(
                "quadlet '{}' for service '{}' references ${{SERVICE_PORT_{}}} but \
                 service.toml declares no [[ports]] entry named '{}'",
                file_name,
                params.service_name,
                var_name,
                var_name.to_lowercase()
            )));
        }
        rest = &tail[end..];
    }

    // ${SERVICE_*} vars expand in the generated ExecStart from *service-level*
    // env — a `[Container]` EnvironmentFile only feeds the container. A unit
    // that uses the vars without a `[Service]` EnvironmentFile would expand
    // them to empty strings at runtime, silently mounting wrong paths.
    let uses_service_vars =
        content.contains("${SERVICE_HOME}") || content.contains("${SERVICE_PORT_");
    if uses_service_vars && !section_has_line(content, "[Service]", "EnvironmentFile=") {
        return Err(Error::Bundle(format!(
            "quadlet '{}' for service '{}' uses ${{SERVICE_*}} vars but has no \
             EnvironmentFile= in its [Service] section — systemd would expand them \
             to empty strings",
            file_name, params.service_name
        )));
    }
    Ok(())
}

/// Whether `section` (e.g. `[Service]`) contains a line starting with `prefix`.
fn section_has_line(content: &str, section: &str, prefix: &str) -> bool {
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == section;
        } else if in_section && trimmed.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// Extract host directories from bind mount `Volume=` lines in `.container` files.
/// Bind mounts are `Volume=/host/path:/container/path:flags` (NOT `.volume:` references).
/// These directories must exist before the container starts.
///
/// Expands `%h` (systemd specifier) and `${SERVICE_HOME}` (runtime env from
/// the service `.env`) — ryra knows both values at install time, and the
/// dirs must exist before the unit first starts.
pub fn extract_bind_mount_dirs(
    files: &[GeneratedFile],
    service_home: &Path,
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
                    // Expand %h systemd specifier and ${SERVICE_HOME}
                    let expanded = host_path
                        .replace("%h", &home.to_string_lossy())
                        .replace("${SERVICE_HOME}", &service_home.to_string_lossy());
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
    let service_home = crate::service_home(params.service_name)?;
    let data_root = crate::paths::service_data_root()?;
    let canonical_data_root = crate::home_dir()?.join(".local/share/services");

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

        // Skip quadlets claimed by an unselected choice option: not written,
        // not symlinked, image not collected.
        if params
            .excluded_quadlets
            .iter()
            .any(|q| q == file_name.as_ref())
        {
            continue;
        }

        validate_quadlet_env_refs(&content, &file_name, params)?;

        // The registry's canonical `EnvironmentFile=%h/.local/share/services/
        // <svc>/.env` is the one literal path in the unit (systemd resolves
        // EnvironmentFile= before any env exists, so it can't be
        // `${SERVICE_HOME}`-based). When the host resolves the data root
        // somewhere else — the RYRA_DATA_DIR test sandbox, or a custom
        // XDG_DATA_HOME — the line must follow, or the unit would read a
        // `.env` ryra never wrote. On default setups the paths are equal
        // and the quadlet is used exactly as authored.
        if data_root != canonical_data_root {
            content = content.replace("%h/.local/share/services", &data_root.to_string_lossy());
        }

        // Stamp every generated quadlet with a provenance marker so
        // `ryra remove` and `ryra list` can tell registry-managed files
        // from hand-written ones. The marker format matches the one
        // used in Caddyfile site blocks and /etc/hosts entries.
        // Wiring details (exposure / url / auth / registry) live in the
        // service's metadata.toml — this comment carries provenance only.
        let is_main_container = file_name == format!("{}.container", params.service_name);
        let header = format!("# Service-Source: registry/{}\n", params.service_name);
        content = header + &content;

        // Only inject networks/volumes into .container files
        if file_name.ends_with(".container") {
            content = inject_networks(&content, params.extra_networks);
            content = inject_extra_volumes(&content, params.extra_volumes);
            content = inject_podman_args(&content, params.podman_args);
            // Inject ExecStartPre into the main service container
            // (the one named <service>.container, not sidecars)
            if is_main_container {
                for cmd in params.extra_exec_start_pre {
                    content = inject_before_section(
                        &content,
                        &format!("ExecStartPre={cmd}"),
                        "[Install]",
                    );
                }
            }
        }

        // Real files live under service_home; the caller emits Step::Symlink
        // afterwards to expose them at the systemd-mandated quadlet path.
        quadlet_files.push(GeneratedFile {
            path: service_home.join(file_name.as_ref()),
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
    let bind_mount_dirs = extract_bind_mount_dirs(&quadlet_files, &service_home)?;
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
    fn excluded_quadlet_is_skipped_and_its_image_not_pulled() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));
        std::fs::write(
            quadlets_dir.join("svc.container"),
            "[Container]\nImage=app:1\n[Service]\nRestart=always\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));
        // The bundled DB sidecar, which `external` would exclude.
        std::fs::write(
            quadlets_dir.join("svc-postgres.container"),
            "[Container]\nImage=postgres:17\n[Service]\nRestart=always\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let excluded = vec!["svc-postgres.container".to_string()];
        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "svc",
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &[],
            excluded_quadlets: &excluded,
        };
        let bundle = process_quadlet_bundle(&params)
            .unwrap_or_else(|e| unreachable!("process_quadlet_bundle should not fail: {e}"));

        // Only the main container survives; the excluded sidecar is gone, and
        // its image was never collected, so nothing pulls it.
        assert_eq!(bundle.quadlet_files.len(), 1);
        assert!(
            bundle.quadlet_files[0]
                .path
                .to_string_lossy()
                .ends_with("svc.container")
        );
        assert_eq!(bundle.images, vec!["app:1".to_string()]);
        assert!(!bundle.images.iter().any(|i| i.contains("postgres")));
    }

    #[test]
    fn process_quadlet_bundle_errors_on_missing_dir() {
        let params = ProcessBundleParams {
            service_dir: Path::new("/nonexistent"),
            service_name: "test",
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &[],
            excluded_quadlets: &[],
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
            "[Container]\nImage=nginx:latest\nPublishPort=${SERVICE_PORT_HTTP}:80\nVolume=${SERVICE_HOME}/data:/data\n\n[Service]\nEnvironmentFile=%h/.local/share/services/myservice/.env\nRestart=always\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        std::fs::write(
            quadlets_dir.join("app.network"),
            "[Network]\nDriver=bridge\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "myservice",
            extra_networks: &["caddy".to_string()],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &["http".to_string()],
            excluded_quadlets: &[],
        };

        let bundle = process_quadlet_bundle(&params)
            .unwrap_or_else(|e| unreachable!("process_quadlet_bundle should not fail: {e}"));

        assert_eq!(bundle.quadlet_files.len(), 2);
        assert_eq!(bundle.images, vec!["nginx:latest".to_string()]);

        let container_file = bundle
            .quadlet_files
            .iter()
            .find(|f| f.path.to_string_lossy().ends_with(".container"))
            .unwrap_or_else(|| unreachable!("container file must exist"));
        // ${...} is runtime env — the quadlet passes through unmodified.
        assert!(
            container_file
                .content
                .contains("PublishPort=${SERVICE_PORT_HTTP}:80")
        );
        assert!(
            container_file
                .content
                .contains("Volume=${SERVICE_HOME}/data:/data")
        );
        // Check network injection happened
        assert!(container_file.content.contains("Network=caddy.network"));
        // ${SERVICE_HOME} bind mounts resolve to the service home dir.
        let service_home = crate::service_home("myservice")
            .unwrap_or_else(|e| unreachable!("service_home should resolve in tests: {e}"));
        assert!(bundle.bind_mount_dirs.contains(&service_home.join("data")));

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
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &[],
            excluded_quadlets: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("no quadlet files found"));
    }

    #[test]
    fn undeclared_port_var_errors() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));
        std::fs::write(
            quadlets_dir.join("svc.container"),
            "[Container]\nImage=nginx\nPublishPort=${SERVICE_PORT_HTPP}:80\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "svc",
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &["http".to_string()],
            excluded_quadlets: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("SERVICE_PORT_HTPP"));
        assert!(err.to_string().contains("no [[ports]] entry"));
    }

    #[test]
    fn service_vars_without_service_envfile_errors() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));
        // ${SERVICE_HOME} volume but EnvironmentFile only in [Container]:
        // expansion happens from service-level env, so this must error.
        std::fs::write(
            quadlets_dir.join("svc.container"),
            "[Container]\nImage=nginx\nVolume=${SERVICE_HOME}/data:/data\nEnvironmentFile=%h/.local/share/services/svc/.env\n\n[Service]\nRestart=always\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "svc",
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &["http".to_string()],
            excluded_quadlets: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("[Service] section"));
    }

    #[test]
    fn leftover_template_syntax_errors() {
        let tmp = tempfile::tempdir()
            .unwrap_or_else(|e| unreachable!("tempdir creation should not fail in tests: {e}"));
        let service_dir = tmp.path().join("svc");
        let quadlets_dir = service_dir.join("quadlets");
        std::fs::create_dir_all(&quadlets_dir)
            .unwrap_or_else(|e| unreachable!("dir creation should not fail in tests: {e}"));
        std::fs::write(
            quadlets_dir.join("svc.container"),
            "[Container]\nImage=nginx\nPublishPort={{ports.http}}:80\n",
        )
        .unwrap_or_else(|e| unreachable!("write should not fail in tests: {e}"));

        let params = ProcessBundleParams {
            service_dir: &service_dir,
            service_name: "svc",
            extra_networks: &[],
            extra_volumes: &[],
            podman_args: &[],
            extra_exec_start_pre: &[],
            port_names: &["http".to_string()],
            excluded_quadlets: &[],
        };
        let err = process_quadlet_bundle(&params).unwrap_err();
        assert!(err.to_string().contains("plain podman files"));
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

        let service_home = Path::new("/home/user/.local/share/services/svc");

        let files = process_configs(&service_dir, service_home)
            .unwrap_or_else(|e| unreachable!("process_configs should not fail: {e}"));

        assert_eq!(files.len(), 2);

        let main_conf = files
            .iter()
            .find(|f| f.path.ends_with("main.conf"))
            .unwrap_or_else(|| unreachable!("main.conf must exist"));
        assert_eq!(
            main_conf.path,
            PathBuf::from("/home/user/.local/share/services/svc/configs/main.conf")
        );
        assert!(main_conf.content.contains("/some/path"));

        let nested_conf = files
            .iter()
            .find(|f| f.path.ends_with("nested.conf"))
            .unwrap_or_else(|| unreachable!("nested.conf must exist"));
        assert_eq!(
            nested_conf.path,
            PathBuf::from("/home/user/.local/share/services/svc/configs/subdir/nested.conf")
        );
        assert_eq!(nested_conf.content, "no placeholders\n");
    }

    #[test]
    fn extract_bind_mount_dirs_finds_host_paths() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
        let files = vec![
            GeneratedFile {
                path: PathBuf::from("/q/immich.container"),
                content: "Volume=${SERVICE_HOME}/upload:/data:Z\nVolume=%h/backups:/backups:Z\nVolume=immich-db-data.volume:/var/lib/postgresql/data:U\n".to_string(),
            },
            GeneratedFile {
                path: PathBuf::from("/q/immich.network"),
                content: "[Network]\n".to_string(),
            },
        ];
        let service_home = PathBuf::from(format!("{home}/.local/share/services/immich"));
        let dirs = extract_bind_mount_dirs(&files, &service_home).unwrap();
        assert_eq!(
            dirs,
            vec![
                service_home.join("upload"),
                PathBuf::from(format!("{home}/backups")),
            ]
        );
    }

    #[test]
    fn extract_bind_mount_dirs_skips_named_volumes() {
        let files = vec![GeneratedFile {
            path: PathBuf::from("/q/svc.container"),
            content: "Volume=svc-data.volume:/data:U\n".to_string(),
        }];
        let dirs = extract_bind_mount_dirs(&files, Path::new("/srv/svc")).unwrap();
        assert!(dirs.is_empty());
    }

    #[test]
    fn extract_bind_mount_dirs_skips_file_mounts() {
        let files = vec![GeneratedFile {
            path: PathBuf::from("/q/svc.container"),
            content: "Volume=/path/to/ca.crt:/etc/ssl/certs/ca.crt:ro,Z\nVolume=/path/to/config:/config:Z\n".to_string(),
        }];
        let dirs = extract_bind_mount_dirs(&files, Path::new("/srv/svc")).unwrap();
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

        let files = process_configs(
            &service_dir,
            Path::new("/home/user/.local/share/services/svc"),
        )
        .unwrap_or_else(|e| unreachable!("process_configs should not fail: {e}"));

        assert!(files.is_empty());
    }
}
