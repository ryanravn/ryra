use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::test_toml::{StepDef, TestToml};

/// A discovered test suite — either simple (setup + assertions) or lifecycle (interleaved steps).
#[derive(Debug, Clone)]
pub enum DiscoveredTest {
    /// Simple tests: setup services/quadlets, then run assertions.
    Simple {
        name: String,
        /// The test.toml this test was loaded from. Used by `--list` to
        /// group tests by file and show where to edit them.
        source: PathBuf,
        setup: SetupConfig,
        tests: Vec<TestEntry>,
        browser: bool,
        ram_override: Option<u32>,
    },
    /// Lifecycle tests: interleaved actions and assertions.
    Lifecycle {
        name: String,
        source: PathBuf,
        steps: Vec<StepDef>,
        browser: bool,
        ram_override: Option<u32>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct SetupConfig {
    pub services: Vec<String>,
    pub quadlets: Vec<String>,
    pub quadlet_dir: Option<PathBuf>,
}

impl DiscoveredTest {
    pub fn name(&self) -> &str {
        match self {
            DiscoveredTest::Simple { name, .. } => name,
            DiscoveredTest::Lifecycle { name, .. } => name,
        }
    }

    /// Path to the `test.toml` this test was discovered in. Same-file
    /// tests share this path — used by `--list` to group and show
    /// editable paths.
    pub fn source(&self) -> &Path {
        match self {
            DiscoveredTest::Simple { source, .. } => source,
            DiscoveredTest::Lifecycle { source, .. } => source,
        }
    }

    /// Distinct step action kinds in order of first appearance. Used to
    /// summarize what a test does on `--list` (e.g., "add → wait → http →
    /// playwright" tells you it's a browser test without reading the file).
    pub fn step_kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<&'static str> = Vec::new();
        let push = |k: &'static str, v: &mut Vec<&'static str>| {
            if !v.contains(&k) {
                v.push(k);
            }
        };
        match self {
            DiscoveredTest::Lifecycle { steps, .. } => {
                for step in steps {
                    let kind = match step {
                        StepDef::Add { .. } => "add",
                        StepDef::Remove { .. } => "remove",
                        StepDef::Wait { .. } => "wait",
                        StepDef::Shell { .. } => "shell",
                        StepDef::Http { .. } => "http",
                        StepDef::Playwright { .. } => "playwright",
                        StepDef::Mail { .. } => "mail",
                    };
                    push(kind, &mut kinds);
                }
            }
            DiscoveredTest::Simple { setup, tests, .. } => {
                if !setup.services.is_empty() || !setup.quadlets.is_empty() {
                    push("setup", &mut kinds);
                }
                if !tests.is_empty() {
                    push("shell", &mut kinds);
                }
            }
        }
        kinds
    }

    /// All services that need to be deployed for this test, in install order.
    pub fn services(&self) -> Vec<&str> {
        match self {
            DiscoveredTest::Simple { setup, .. } => {
                setup.services.iter().map(|s| s.as_str()).collect()
            }
            DiscoveredTest::Lifecycle { steps, .. } => {
                let mut svcs = Vec::new();
                for step in steps {
                    if let StepDef::Add { service, .. } = step
                        && !svcs.contains(&service.as_str())
                    {
                        svcs.push(service.as_str());
                    }
                }
                svcs
            }
        }
    }

    pub fn tests(&self) -> &[TestEntry] {
        match self {
            DiscoveredTest::Simple { tests, .. } => tests,
            DiscoveredTest::Lifecycle { .. } => &[],
        }
    }

    pub fn test_count(&self) -> usize {
        match self {
            DiscoveredTest::Simple { tests, .. } => tests.len(),
            DiscoveredTest::Lifecycle { steps, .. } => steps.len(),
        }
    }

    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        match self {
            DiscoveredTest::Simple { name, setup, .. } => {
                if setup.services.is_empty() {
                    name.clone()
                } else {
                    format!("{} ({})", name, setup.services.join(" + "))
                }
            }
            DiscoveredTest::Lifecycle { name, steps, .. } => {
                format!("{} ({} steps)", name, steps.len())
            }
        }
    }

    pub fn is_lifecycle(&self) -> bool {
        matches!(self, DiscoveredTest::Lifecycle { .. })
    }

    pub fn has_quadlets(&self) -> bool {
        match self {
            DiscoveredTest::Simple { setup, .. } => !setup.quadlets.is_empty(),
            DiscoveredTest::Lifecycle { .. } => false,
        }
    }

    pub fn needs_browser(&self) -> bool {
        match self {
            DiscoveredTest::Simple { browser, .. } => *browser,
            DiscoveredTest::Lifecycle { browser, .. } => *browser,
        }
    }

    pub fn ram_override(&self) -> Option<u32> {
        match self {
            DiscoveredTest::Simple { ram_override, .. } => *ram_override,
            DiscoveredTest::Lifecycle { ram_override, .. } => *ram_override,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TestEntry {
    pub name: String,
    pub run: String,
    pub timeout_secs: u64,
    /// Env var overrides to pass to `ryra add` for this test.
    pub env: std::collections::BTreeMap<String, String>,
}

/// Discover tests from a local project directory containing quadlet files + test.toml.
///
/// Returns `None` if the directory doesn't contain a test.toml.
pub fn discover_local_project(project_dir: &Path) -> Result<Option<DiscoveredTest>> {
    let test_toml_path = project_dir.join("test.toml");
    if !test_toml_path.exists() {
        return Ok(None);
    }

    let parsed = TestToml::parse(&test_toml_path)?;

    // Find all quadlet files in the project directory
    let quadlet_extensions = ["container", "volume", "network", "pod", "kube"];
    let mut quadlet_files = Vec::new();
    let entries = std::fs::read_dir(project_dir)
        .with_context(|| format!("failed to read {}", project_dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && quadlet_extensions.contains(&ext)
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            quadlet_files.push(name.to_string());
        }
    }

    if quadlet_files.is_empty() {
        anyhow::bail!(
            "test.toml found at {} but no quadlet files (.container, .volume, .network, .pod) in the same directory",
            test_toml_path.display()
        );
    }

    let project_dir = std::fs::canonicalize(project_dir)
        .with_context(|| format!("failed to canonicalize {}", project_dir.display()))?;

    // Infer directory name for defaults
    let dir_name = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let mut tests =
        discover_from_test_toml(&test_toml_path, &parsed, &dir_name, Some(&project_dir))?;

    // Local projects are a single-file, single-test concept — a `.container`
    // directory with a `test.toml` is expected to describe exactly one test
    // suite. The new multi-test format is a registry-level feature.
    if tests.len() != 1 {
        anyhow::bail!(
            "local project test.toml must describe exactly one test (got {}); \
             multi-test [[tests]] arrays are only supported inside the registry",
            tests.len()
        );
    }
    let mut test = tests.remove(0);

    // Populate quadlets from discovered files if not explicitly set in [setup]
    if let DiscoveredTest::Simple { ref mut setup, .. } = test
        && setup.quadlets.is_empty()
        && !quadlet_files.is_empty()
    {
        setup.quadlets = quadlet_files;
    }

    Ok(Some(test))
}

/// Scan a registry directory for all test definitions.
///
/// Reads `test.toml` from each `<service>/test.toml` and standalone test
/// files from `tests/*.toml`.
pub fn discover(registry_path: &Path) -> Result<Vec<DiscoveredTest>> {
    let mut discovered = Vec::new();

    // Scan service directories for test.toml
    let entries = std::fs::read_dir(registry_path)
        .with_context(|| format!("failed to read registry at {}", registry_path.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip the tests/ directory and hidden files
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if dir_name == "tests" || dir_name.starts_with('.') {
            continue;
        }

        let test_toml_path = path.join("test.toml");
        if !test_toml_path.exists() {
            continue;
        }

        match TestToml::parse(&test_toml_path) {
            Ok(parsed) => {
                match discover_from_test_toml(&test_toml_path, &parsed, &dir_name, None) {
                    Ok(tests) => discovered.extend(tests),
                    Err(e) => {
                        eprintln!(
                            "warning: failed to process {}: {e}",
                            test_toml_path.display()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: failed to parse {}: {e}", test_toml_path.display());
            }
        }
    }

    // Scan tests/ directory for standalone test files
    let tests_dir = registry_path.join("tests");
    if tests_dir.is_dir() {
        let entries = std::fs::read_dir(&tests_dir)
            .with_context(|| format!("failed to read {}", tests_dir.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }

            let file_stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            match TestToml::parse(&path) {
                Ok(parsed) => match discover_from_test_toml(&path, &parsed, &file_stem, None) {
                    Ok(tests) => discovered.extend(tests),
                    Err(e) => {
                        eprintln!("warning: failed to process {}: {e}", path.display());
                    }
                },
                Err(e) => {
                    eprintln!("warning: failed to parse {}: {e}", path.display());
                }
            }
        }
    }

    // Sort by name for deterministic ordering
    discovered.sort_by(|a, b| a.name().cmp(b.name()));

    Ok(discovered)
}

/// Convert a parsed TestToml into a DiscoveredTest.
///
/// `service_name_hint` is used as the default test name and (for service-level test.toml)
/// as the default setup service when no [setup] section exists.
/// `quadlet_dir` is set for local project tests to point to the project directory.
/// Convert a parsed `test.toml` into one-or-more `DiscoveredTest`s.
///
/// Three file shapes are handled:
///
/// 1. **New multi-test**: one-or-more `[[tests]]` entries each with their
///    own `[[tests.steps]]`. Each becomes its own `Lifecycle` test, named
///    as `<service>-<test-name>` (or just `<test-name>` for cross-cutting
///    files in `registry/tests/`).
/// 2. **Legacy lifecycle**: top-level `[[steps]]`, optionally with
///    `[test] name = …`. One `Lifecycle` test per file.
/// 3. **Legacy shell**: `[setup]` + `[[tests]] run = …`. One `Simple`
///    test per file, multiple assertion steps.
fn discover_from_test_toml(
    path: &Path,
    parsed: &TestToml,
    service_name_hint: &str,
    quadlet_dir: Option<&Path>,
) -> Result<Vec<DiscoveredTest>> {
    // --- Shape 1: new multi-test format ---
    let new_format_tests: Vec<&crate::test_toml::TestDef> = parsed
        .tests
        .iter()
        .filter(|t| !t.steps.is_empty())
        .collect();
    if !new_format_tests.is_empty() {
        // Decide how to namespace. For a service's own test.toml (under
        // registry/<svc>/), prefix test names with the service so each
        // test is globally addressable (`ryra test forgejo-smtp`). For
        // cross-cutting tests under registry/tests/, the file stem is
        // already unique; use the test name as-is.
        let is_service_owned = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some(service_name_hint);

        let mut out = Vec::with_capacity(new_format_tests.len());
        for t in new_format_tests {
            let qualified = if is_service_owned && t.name != service_name_hint {
                format!("{service_name_hint}-{}", t.name)
            } else {
                t.name.clone()
            };
            out.push(DiscoveredTest::Lifecycle {
                name: qualified,
                source: path.to_path_buf(),
                steps: t.steps.clone(),
                browser: t.browser || parsed.needs_browser(),
                ram_override: t.ram.or(parsed.ram_override()),
            });
        }
        return Ok(out);
    }

    // --- Shape 2: legacy lifecycle ---
    let name = parsed.name_or_default(path);
    let test_name = if parsed.test.as_ref().and_then(|t| t.name.as_ref()).is_some() {
        name
    } else {
        service_name_hint.to_string()
    };
    let browser = parsed.needs_browser();
    let ram_override = parsed.ram_override();

    if parsed.is_lifecycle() {
        return Ok(vec![DiscoveredTest::Lifecycle {
            name: test_name,
            source: path.to_path_buf(),
            steps: parsed.steps.clone(),
            browser,
            ram_override,
        }]);
    }

    // --- Shape 3: legacy shell-style ---
    let setup = match &parsed.setup {
        Some(s) => SetupConfig {
            services: s.services.clone(),
            quadlets: s.quadlets.clone(),
            quadlet_dir: quadlet_dir.map(PathBuf::from),
        },
        None => SetupConfig {
            services: vec![service_name_hint.to_string()],
            quadlets: Vec::new(),
            quadlet_dir: quadlet_dir.map(PathBuf::from),
        },
    };

    let tests = parsed
        .tests
        .iter()
        .map(|t| TestEntry {
            name: t.name.clone(),
            run: t.run.clone().unwrap_or_default(),
            timeout_secs: t.timeout,
            env: t.env.clone(),
        })
        .collect();

    Ok(vec![DiscoveredTest::Simple {
        name: test_name,
        source: path.to_path_buf(),
        setup,
        tests,
        browser,
        ram_override,
    }])
}

/// Look up the recommended RAM (MB) for a service from its service.toml.
pub fn service_recommended_ram(registry_path: &Path, service_name: &str) -> Result<Option<u64>> {
    let service_toml = registry_path.join(service_name).join("service.toml");
    if !service_toml.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&service_toml)
        .with_context(|| format!("failed to read {}", service_toml.display()))?;
    let parsed: ServiceTomlRam = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", service_toml.display()))?;
    Ok(parsed.requirements.and_then(|r| r.ram.recommended))
}

/// Compute the VM memory (MB) needed for a test based on its services'
/// recommended RAM. Adds 512MB OS overhead, rounds up to 512MB increments,
/// with a 1024MB floor.
pub fn vm_memory_for_test(registry_path: &Path, test: &DiscoveredTest) -> u32 {
    if let Some(ram) = test.ram_override() {
        return ram;
    }

    let services: Vec<&str> = match test {
        DiscoveredTest::Lifecycle { steps, .. } => steps
            .iter()
            .filter_map(|s| match s {
                StepDef::Add { service, .. } => Some(service.as_str()),
                // Also detect `ryra add <service>` in run steps
                StepDef::Shell { run, .. } => run
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .windows(3)
                    .find(|w| w[0] == "ryra" && w[1] == "add")
                    .map(|w| w[2]),
                _ => None,
            })
            .collect(),
        _ => test.services(),
    };

    let service_ram: u64 = services
        .iter()
        .map(|svc| {
            service_recommended_ram(registry_path, svc)
                .ok()
                .flatten()
                .unwrap_or(128) // default if not specified
        })
        .sum();

    // Browser tests need extra memory for chromium + playwright
    let browser_overhead = if test.needs_browser() { 512 } else { 0 };
    let total = service_ram + 512 + browser_overhead; // OS/podman + browser overhead
    let rounded = total.div_ceil(512) * 512; // round up to 512MB
    rounded.max(1024) as u32
}

/// Look up the minimum disk (GB) for a service from its service.toml.
pub fn service_min_disk(registry_path: &Path, service_name: &str) -> Result<Option<u32>> {
    let service_toml = registry_path.join(service_name).join("service.toml");
    if !service_toml.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&service_toml)
        .with_context(|| format!("failed to read {}", service_toml.display()))?;
    let parsed: ServiceTomlDisk = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", service_toml.display()))?;
    Ok(parsed.requirements.and_then(|r| r.disk).map(|d| d.min))
}

/// Compute the VM disk size (GB) needed for a test. Takes the max of all
/// services' disk requirements, with a 20GB floor.
pub fn vm_disk_for_test(registry_path: &Path, test: &DiscoveredTest) -> u32 {
    let services: Vec<&str> = match test {
        DiscoveredTest::Lifecycle { steps, .. } => steps
            .iter()
            .filter_map(|s| match s {
                StepDef::Add { service, .. } => Some(service.as_str()),
                StepDef::Shell { run, .. } => run
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .windows(3)
                    .find(|w| w[0] == "ryra" && w[1] == "add")
                    .map(|w| w[2]),
                _ => None,
            })
            .collect(),
        _ => test.services(),
    };

    let max_disk: u32 = services
        .iter()
        .filter_map(|svc| service_min_disk(registry_path, svc).ok().flatten())
        .max()
        .unwrap_or(0);

    max_disk.max(20)
}

/// Look up the primary container image for a service from its quadlet files.
pub fn service_image(registry_path: &Path, service_name: &str) -> Result<Option<String>> {
    let images = service_images(registry_path, service_name);
    Ok(images.into_iter().next())
}

/// Parse a `ryra add <svc> …` args string and return the well-known service
/// names it *implicitly* pulls in (so the caller can pre-cache their images).
///
/// - `--smtp=<name>` / `--smtp <name>` → `<name>` (typically `inbucket`)
/// - `--auth` → `authelia`
/// - `--domain …` / `--url …` → `caddy`
///
/// Unknown flag values pass through untouched; the caller decides what's real.
fn implied_services_from_args(args: &str) -> Vec<&str> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut out: Vec<&str> = Vec::new();

    let push = |svc: &'static str, out: &mut Vec<&str>| {
        if !out.contains(&svc) {
            out.push(svc);
        }
    };

    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if let Some(val) = t.strip_prefix("--smtp=") {
            if !val.is_empty() && !out.contains(&val) {
                out.push(val);
            }
        } else if t == "--smtp" {
            if let Some(val) = tokens.get(i + 1)
                && !val.starts_with("--")
                && !out.contains(val)
            {
                out.push(val);
                i += 1;
            }
        } else if t == "--auth" || t.starts_with("--auth=") {
            push("authelia", &mut out);
        } else if t == "--domain"
            || t.starts_with("--domain=")
            || t == "--url"
            || t.starts_with("--url=")
        {
            push("caddy", &mut out);
        }
        i += 1;
    }
    out
}

/// Get all container images for a service by scanning its `quadlets/` directory.
pub fn service_images(registry_path: &Path, service_name: &str) -> Vec<String> {
    let quadlets_dir = registry_path.join(service_name).join("quadlets");
    let mut images = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&quadlets_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".container") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if let Some(image) = trimmed.strip_prefix("Image=") {
                        let image = image.trim();
                        if !image.is_empty() && !images.contains(&image.to_string()) {
                            images.push(image.to_string());
                        }
                    }
                }
            }
        }
    }
    images
}

/// Get all container images needed for a test.
pub fn images_for_test(registry_path: &Path, test: &DiscoveredTest) -> Vec<String> {
    let mut images = Vec::new();
    let add_image = |img: String, out: &mut Vec<String>| {
        if !out.contains(&img) {
            out.push(img);
        }
    };
    let include_service = |svc: &str, out: &mut Vec<String>| {
        for image in service_images(registry_path, svc) {
            add_image(image, out);
        }
    };

    match test {
        DiscoveredTest::Lifecycle { steps, .. } => {
            for step in steps {
                match step {
                    StepDef::Add { service, args, .. } => {
                        include_service(service, &mut images);
                        // `ryra add <svc> --smtp=<mail> --auth --domain …` pulls
                        // additional well-known services (inbucket, authelia,
                        // caddy) in *without* explicit add steps. We need to
                        // pre-cache their images too, otherwise the in-VM
                        // podman pull hits the public registry — and any
                        // Docker Hub flake there fails the test.
                        if let Some(args_str) = args {
                            for implied in implied_services_from_args(args_str) {
                                include_service(implied, &mut images);
                            }
                        }
                    }
                    StepDef::Shell { run, .. } => {
                        // Parse "ryra add <service>" from run commands.
                        let tokens: Vec<&str> = run.split_whitespace().collect();
                        if let Some(idx) = tokens
                            .windows(2)
                            .position(|w| w[0] == "ryra" && w[1] == "add")
                        {
                            if let Some(svc) = tokens.get(idx + 2) {
                                include_service(svc, &mut images);
                            }
                            // Also sweep any --smtp=<x> / --auth / --url flags
                            // that appear further along in the same command.
                            let rest = &tokens[idx + 2..];
                            for implied in implied_services_from_args(&rest.join(" ")) {
                                include_service(implied, &mut images);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        DiscoveredTest::Simple { setup, .. } => {
            // Images from registry services
            for service in &setup.services {
                for image in service_images(registry_path, service) {
                    if !images.contains(&image) {
                        images.push(image);
                    }
                }
            }
            // Images from quadlet files in the quadlet_dir
            if let Some(ref dir) = setup.quadlet_dir {
                for quadlet in &setup.quadlets {
                    let full_path = dir.join(quadlet);
                    if quadlet.ends_with(".container")
                        && let Ok(content) = std::fs::read_to_string(&full_path)
                    {
                        for line in content.lines() {
                            let trimmed = line.trim();
                            if let Some(image) = trimmed.strip_prefix("Image=") {
                                let image = image.trim();
                                if !image.is_empty() && !images.contains(&image.to_string()) {
                                    images.push(image.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    images
}

// ---------------------------------------------------------------------------
// Lightweight TOML structs for parsing service.toml (avoids full ServiceDef dependency)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ServiceTomlRam {
    #[serde(default)]
    requirements: Option<RequirementsRam>,
}

#[derive(serde::Deserialize)]
struct ServiceTomlDisk {
    #[serde(default)]
    requirements: Option<RequirementsDisk>,
}

#[derive(serde::Deserialize)]
struct RequirementsDisk {
    #[serde(default)]
    disk: Option<DiskFields>,
}

#[derive(serde::Deserialize)]
struct DiskFields {
    min: u32,
}

#[derive(serde::Deserialize)]
struct RequirementsRam {
    ram: RamFields,
}

#[derive(serde::Deserialize)]
struct RamFields {
    #[serde(default)]
    recommended: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_simple_test_from_test_toml() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[[tests]]
name = "responds"
run = "curl -sf http://127.0.0.1:$SERVICE_PORT_HTTP"

[[tests]]
name = "hostname"
run = "curl -s http://127.0.0.1:$SERVICE_PORT_HTTP | grep -q Hostname"
timeout = 10
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let mut out = discover_from_test_toml(&test_toml, &parsed, "whoami", None).unwrap();
        assert_eq!(
            out.len(),
            1,
            "legacy shell form produces a single Simple test"
        );
        let test = out.remove(0);

        assert_eq!(test.name(), "whoami");
        assert!(!test.is_lifecycle());
        assert_eq!(test.test_count(), 2);
        assert_eq!(test.services(), vec!["whoami"]); // inferred from hint
        assert_eq!(test.tests()[0].name, "responds");
        assert_eq!(test.tests()[1].timeout_secs, 10);
    }

    #[test]
    fn discover_simple_test_with_setup() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[test]
name = "whoami-plus-postgres"

[setup]
services = ["whoami", "postgres"]

[[tests]]
name = "both-running"
run = "echo ok"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let mut out = discover_from_test_toml(&test_toml, &parsed, "combo", None).unwrap();
        assert_eq!(out.len(), 1);
        let test = out.remove(0);

        assert_eq!(test.name(), "whoami-plus-postgres");
        assert_eq!(test.services(), vec!["whoami", "postgres"]);
        assert_eq!(test.test_count(), 1);
    }

    #[test]
    fn discover_lifecycle_test() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[test]
name = "remove-test"

[[steps]]
action = "add"
service = "whoami"

[[steps]]
action = "wait"
service = "whoami"

[[steps]]
action = "shell"
name = "responds"
run = "curl -sf http://localhost"

[[steps]]
action = "remove"
service = "whoami"

[[steps]]
action = "shell"
name = "gone"
run = "! id whoami"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let mut out = discover_from_test_toml(&test_toml, &parsed, "remove-test", None).unwrap();
        assert_eq!(out.len(), 1);
        let test = out.remove(0);

        assert_eq!(test.name(), "remove-test");
        assert!(test.is_lifecycle());
        assert_eq!(test.test_count(), 5);
        assert_eq!(test.services(), vec!["whoami"]); // collected from Add steps
    }

    #[test]
    fn discover_lifecycle_with_args() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[test]
name = "auth-test"

[[steps]]
action = "add"
service = "caddy"
args = "--domain proxy.test.local"

[[steps]]
action = "shell"
name = "caddy up"
run = "curl -sf http://proxy.test.local"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let mut out = discover_from_test_toml(&test_toml, &parsed, "auth-test", None).unwrap();
        assert_eq!(out.len(), 1);
        let test = out.remove(0);

        assert!(test.is_lifecycle());
        if let DiscoveredTest::Lifecycle { steps, .. } = &test {
            if let StepDef::Add { service, args, .. } = &steps[0] {
                assert_eq!(service, "caddy");
                assert_eq!(args.as_deref(), Some("--domain proxy.test.local"));
            } else {
                panic!("expected Add step");
            }
        } else {
            panic!("expected Lifecycle variant");
        }
    }

    #[test]
    fn discover_multi_test_service_owned() {
        // Simulate a service-owned test.toml (path: .../whoami/test.toml)
        // with three named tests, each bringing their own steps.
        let dir = tempfile::tempdir().unwrap();
        let svc_dir = dir.path().join("whoami");
        std::fs::create_dir(&svc_dir).unwrap();
        let test_toml = svc_dir.join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[[tests]]
name = "whoami"
[[tests.steps]]
action = "add"
service = "whoami"
[[tests.steps]]
action = "wait"
service = "whoami"

[[tests]]
name = "diff"
[[tests.steps]]
action = "add"
service = "whoami"
[[tests.steps]]
action = "shell"
name = "idempotent"
run = "true"

[[tests]]
name = "remove"
[[tests.steps]]
action = "add"
service = "whoami"
[[tests.steps]]
action = "remove"
service = "whoami"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let out = discover_from_test_toml(&test_toml, &parsed, "whoami", None).unwrap();
        assert_eq!(out.len(), 3);

        // Test name equal to service name stays un-prefixed. Others get
        // `<service>-<test>` so they're uniquely addressable on the CLI.
        let names: Vec<&str> = out.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["whoami", "whoami-diff", "whoami-remove"]);
        for t in &out {
            assert!(t.is_lifecycle());
        }
    }

    #[test]
    fn discover_multi_test_cross_cutting() {
        // Cross-cutting file under `tests/<stem>.toml` — no service-dir prefix.
        let dir = tempfile::tempdir().unwrap();
        let tests_dir = dir.path().join("tests");
        std::fs::create_dir(&tests_dir).unwrap();
        let test_toml = tests_dir.join("cross-thing.toml");
        std::fs::write(
            &test_toml,
            r#"
[[tests]]
name = "first"
[[tests.steps]]
action = "add"
service = "whoami"

[[tests]]
name = "second"
[[tests.steps]]
action = "add"
service = "whoami"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let out = discover_from_test_toml(&test_toml, &parsed, "cross-thing", None).unwrap();
        assert_eq!(out.len(), 2);
        let names: Vec<&str> = out.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn reject_test_with_both_run_and_steps() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[[tests]]
name = "bad"
run = "true"
[[tests.steps]]
action = "add"
service = "whoami"
"#,
        )
        .unwrap();

        let err = TestToml::parse(&test_toml).expect_err("must reject run+steps");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exactly one of `run` or `steps`"),
            "got: {msg}"
        );
    }

    #[test]
    fn discover_registry() {
        let registry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ryra-core/registry");
        if !registry.exists() {
            return; // skip if not available
        }
        let discovered = discover(&registry).unwrap();

        // All tests should come from test.toml files now.
        // If no test.toml files exist yet, discovered will be empty — that's expected
        // during migration. Once registry/*/test.toml files are created, this test
        // will verify they're discovered correctly.
        if discovered.is_empty() {
            return; // registry hasn't been migrated yet
        }

        let names: Vec<&str> = discovered.iter().map(|d| d.name()).collect();

        // Basic checks — these only apply once test.toml files exist
        for test in &discovered {
            assert!(!test.name().is_empty());
            assert!(test.test_count() > 0);
        }

        // If whoami test.toml exists, verify it
        if names.contains(&"whoami") {
            let whoami = discovered.iter().find(|d| d.name() == "whoami").unwrap();
            assert!(whoami.is_lifecycle());
            assert!(whoami.test_count() >= 1);
        }
    }

    #[test]
    fn discover_local_project_from_dir() {
        // Create a temp dir with quadlet files and test.toml
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();

        // Write test.toml
        std::fs::write(
            project_dir.join("test.toml"),
            r#"
[test]
name = "test-app"

[[tests]]
name = "responds"
run = "curl -sf http://127.0.0.1:8080"
"#,
        )
        .unwrap();

        // Write a .container file
        std::fs::write(
            project_dir.join("test-app.container"),
            "[Container]\nImage=docker.io/traefik/whoami:v1.11.0\n\n[Service]\nRestart=always\n",
        )
        .unwrap();

        // Write a .volume file
        std::fs::write(project_dir.join("test-app.volume"), "[Volume]\n").unwrap();

        let result = discover_local_project(project_dir).unwrap();
        assert!(result.is_some());

        let test = result.unwrap();
        assert_eq!(test.name(), "test-app");
        assert!(test.has_quadlets());
        assert_eq!(test.test_count(), 1);

        if let DiscoveredTest::Simple { setup, .. } = &test {
            assert!(setup.quadlet_dir.is_some());
            // The inferred service is the directory name (temp dir name), not "test-app"
            // since there's no [setup] section, the hint (dir name) is used
        } else {
            panic!("expected Simple variant");
        }
    }

    #[test]
    fn discover_local_project_with_setup_services() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();

        std::fs::write(
            project_dir.join("test.toml"),
            r#"
[test]
name = "my-app"

[setup]
services = ["postgres", "redis"]
quadlets = ["my-app.container"]

[[tests]]
name = "health-check"
run = "curl -sf http://127.0.0.1:8080/health"
timeout = 10
"#,
        )
        .unwrap();

        // Write a .container file so discovery doesn't fail
        std::fs::write(
            project_dir.join("my-app.container"),
            "[Container]\nImage=docker.io/myapp:latest\n",
        )
        .unwrap();

        let result = discover_local_project(project_dir).unwrap();
        let test = result.unwrap();
        assert_eq!(test.name(), "my-app");
        assert_eq!(test.services(), vec!["postgres", "redis"]);
        assert!(test.has_quadlets());
    }

    #[test]
    fn discover_local_project_no_quadlets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.toml"),
            "[[tests]]\nname = \"check\"\nrun = \"true\"\n",
        )
        .unwrap();

        let result = discover_local_project(dir.path());
        assert!(result.is_err()); // should error about missing quadlet files
    }

    #[test]
    fn browser_flag_on_simple_test() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[test]
browser = true

[[tests]]
name = "browser check"
run = "true"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let mut out = discover_from_test_toml(&test_toml, &parsed, "my-test", None).unwrap();
        assert_eq!(out.len(), 1);
        let test = out.remove(0);
        assert!(test.needs_browser());
    }
}
