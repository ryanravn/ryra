use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::test_toml::{StepDef, TestToml};

/// A discovered test suite — either simple (setup + assertions) or lifecycle (interleaved steps).
#[derive(Debug, Clone)]
pub enum DiscoveredTest {
    /// Simple tests: setup services/quadlets, then run assertions.
    Simple {
        name: String,
        setup: SetupConfig,
        tests: Vec<TestEntry>,
        browser: bool,
        ram_override: Option<u32>,
    },
    /// Lifecycle tests: interleaved actions and assertions.
    Lifecycle {
        name: String,
        steps: Vec<StepEntry>,
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

/// A step in a lifecycle test — either an action or an assertion.
#[derive(Debug, Clone)]
pub enum StepEntry {
    Add {
        service: String,
        args: Option<String>,
    },
    Remove {
        service: String,
    },
    Reset,
    Wait {
        service: String,
        timeout_secs: u64,
    },
    Run {
        name: String,
        run: String,
        timeout_secs: u64,
    },
    Assert {
        name: String,
        run: String,
        timeout_secs: u64,
    },
}

impl DiscoveredTest {
    pub fn name(&self) -> &str {
        match self {
            DiscoveredTest::Simple { name, .. } => name,
            DiscoveredTest::Lifecycle { name, .. } => name,
        }
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
                    if let StepEntry::Add { service, .. } = step {
                        if !svcs.contains(&service.as_str()) {
                            svcs.push(service.as_str());
                        }
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
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if quadlet_extensions.contains(&ext) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    quadlet_files.push(name.to_string());
                }
            }
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

    let mut test =
        discover_from_test_toml(&test_toml_path, &parsed, &dir_name, Some(&project_dir))?;

    // Populate quadlets from discovered files if not explicitly set in [setup]
    if let DiscoveredTest::Simple { ref mut setup, .. } = test {
        if setup.quadlets.is_empty() && !quadlet_files.is_empty() {
            setup.quadlets = quadlet_files;
        }
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
                    Ok(test) => discovered.push(test),
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
                    Ok(test) => discovered.push(test),
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
fn discover_from_test_toml(
    path: &Path,
    parsed: &TestToml,
    service_name_hint: &str,
    quadlet_dir: Option<&Path>,
) -> Result<DiscoveredTest> {
    let name = parsed.name_or_default(path);
    // Use the explicit name if available, otherwise fall back to the hint
    let test_name = if parsed.test.as_ref().and_then(|t| t.name.as_ref()).is_some() {
        name
    } else {
        service_name_hint.to_string()
    };

    let browser = parsed.needs_browser();
    let ram_override = parsed.ram_override();

    if parsed.is_lifecycle() {
        let steps = convert_steps(&parsed.steps, &test_name)?;
        return Ok(DiscoveredTest::Lifecycle {
            name: test_name,
            steps,
            browser,
            ram_override,
        });
    }

    // Simple test
    let setup = match &parsed.setup {
        Some(s) => SetupConfig {
            services: s.services.clone(),
            quadlets: s.quadlets.clone(),
            quadlet_dir: quadlet_dir.map(PathBuf::from),
        },
        None => {
            // For service-level test.toml, infer the service from directory name
            SetupConfig {
                services: vec![service_name_hint.to_string()],
                quadlets: Vec::new(),
                quadlet_dir: quadlet_dir.map(PathBuf::from),
            }
        }
    };

    let tests = parsed
        .tests
        .iter()
        .map(|t| TestEntry {
            name: t.name.clone(),
            run: t.run.clone(),
            timeout_secs: t.timeout,
            env: t.env.clone(),
        })
        .collect();

    Ok(DiscoveredTest::Simple {
        name: test_name,
        setup,
        tests,
        browser,
        ram_override,
    })
}

/// Convert StepDef entries from test_toml into StepEntry enums.
fn convert_steps(step_defs: &[StepDef], test_name: &str) -> Result<Vec<StepEntry>> {
    let mut steps = Vec::new();
    for s in step_defs {
        let step = match s.action.as_str() {
            "add" => {
                let service = s.service.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'add' requires a 'service' field in test '{}'",
                        test_name
                    )
                })?;
                StepEntry::Add {
                    service,
                    args: s.args.clone(),
                }
            }
            "remove" => {
                let service = s.service.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'remove' requires a 'service' field in test '{}'",
                        test_name
                    )
                })?;
                StepEntry::Remove { service }
            }
            "reset" => StepEntry::Reset,
            "wait" => {
                let service = s.service.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'wait' requires a 'service' field in test '{}'",
                        test_name
                    )
                })?;
                StepEntry::Wait {
                    service,
                    timeout_secs: if s.timeout > 0 { s.timeout } else { 60 },
                }
            }
            "run" => {
                let name = s.name.clone().ok_or_else(|| {
                    anyhow::anyhow!("step 'run' requires a 'name' field in test '{}'", test_name)
                })?;
                let run = s.run.clone().ok_or_else(|| {
                    anyhow::anyhow!("step 'run' requires a 'run' field in test '{}'", test_name)
                })?;
                StepEntry::Run {
                    name,
                    run,
                    timeout_secs: s.timeout,
                }
            }
            "assert" => {
                let name = s.name.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'assert' requires a 'name' field in test '{}'",
                        test_name
                    )
                })?;
                let run = s.run.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'assert' requires a 'run' field in test '{}'",
                        test_name
                    )
                })?;
                StepEntry::Assert {
                    name,
                    run,
                    timeout_secs: s.timeout,
                }
            }
            other => {
                anyhow::bail!("unknown step action '{}' in test '{}'", other, test_name);
            }
        };
        steps.push(step);
    }
    Ok(steps)
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
                StepEntry::Add { service, .. } => Some(service.as_str()),
                // Also detect `ryra add <service>` in run steps
                StepEntry::Run { run, .. } => run
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

/// Look up the container image for a service from its service.toml.
pub fn service_image(registry_path: &Path, service_name: &str) -> Result<Option<String>> {
    let service_toml = registry_path.join(service_name).join("service.toml");
    if !service_toml.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&service_toml)
        .with_context(|| format!("failed to read {}", service_toml.display()))?;
    let parsed: ServiceTomlAllImages = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", service_toml.display()))?;
    Ok(Some(parsed.service.image))
}

/// Get all container images for a service (primary + sidecars).
pub fn service_images(registry_path: &Path, service_name: &str) -> Vec<String> {
    let service_toml = registry_path.join(service_name).join("service.toml");
    let mut images = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&service_toml)
        && let Ok(parsed) = toml::from_str::<ServiceTomlAllImages>(&content)
    {
        images.push(parsed.service.image);
        for c in &parsed.containers {
            if !images.contains(&c.image) {
                images.push(c.image.clone());
            }
        }
    }
    images
}

/// Get all container images needed for a test.
pub fn images_for_test(registry_path: &Path, test: &DiscoveredTest) -> Vec<String> {
    let mut images = Vec::new();

    match test {
        DiscoveredTest::Lifecycle { steps, .. } => {
            // Look up images for services referenced in add steps or run steps
            // that call `ryra add <service>`
            for step in steps {
                let service_name = match step {
                    StepEntry::Add { service, .. } => Some(service.as_str()),
                    StepEntry::Run { run, .. } => {
                        // Parse "ryra add <service>" from run commands
                        run.split_whitespace()
                            .collect::<Vec<_>>()
                            .windows(3)
                            .find(|w| w[0] == "ryra" && w[1] == "add")
                            .map(|w| w[2])
                    }
                    _ => None,
                };
                if let Some(service) = service_name {
                    for image in service_images(registry_path, service) {
                        if !images.contains(&image) {
                            images.push(image);
                        }
                    }
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
                    if quadlet.ends_with(".container") {
                        if let Ok(content) = std::fs::read_to_string(&full_path) {
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
    }

    images
}

// ---------------------------------------------------------------------------
// Lightweight TOML structs for parsing service.toml (avoids full ServiceDef dependency)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ServiceTomlAllImages {
    service: ServiceMetaImage,
    #[serde(default)]
    containers: Vec<ContainerImage>,
}

#[derive(serde::Deserialize)]
struct ServiceMetaImage {
    image: String,
}

#[derive(serde::Deserialize)]
struct ContainerImage {
    image: String,
}

#[derive(serde::Deserialize)]
struct ServiceTomlRam {
    #[serde(default)]
    requirements: Option<RequirementsRam>,
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
run = "curl -sf http://127.0.0.1:$RYRA_PORT_HTTP"

[[tests]]
name = "hostname"
run = "curl -s http://127.0.0.1:$RYRA_PORT_HTTP | grep -q Hostname"
timeout = 10
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let test = discover_from_test_toml(&test_toml, &parsed, "whoami", None).unwrap();

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
        let test = discover_from_test_toml(&test_toml, &parsed, "combo", None).unwrap();

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
action = "assert"
name = "responds"
run = "curl -sf http://localhost"

[[steps]]
action = "remove"
service = "whoami"

[[steps]]
action = "assert"
name = "gone"
run = "! id whoami"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let test = discover_from_test_toml(&test_toml, &parsed, "remove-test", None).unwrap();

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
action = "assert"
name = "caddy up"
run = "curl -sf http://proxy.test.local"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let test = discover_from_test_toml(&test_toml, &parsed, "auth-test", None).unwrap();

        assert!(test.is_lifecycle());
        if let DiscoveredTest::Lifecycle { steps, .. } = &test {
            if let StepEntry::Add { service, args } = &steps[0] {
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
    fn discover_registry() {
        let registry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../registry");
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
            assert!(!whoami.is_lifecycle());
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
    fn discover_local_project_no_test_toml() {
        let dir = tempfile::tempdir().unwrap();
        let result = discover_local_project(dir.path()).unwrap();
        assert!(result.is_none());
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
    fn has_quadlets_false_for_simple_without_quadlets() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[[tests]]
name = "check"
run = "true"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let test = discover_from_test_toml(&test_toml, &parsed, "whoami", None).unwrap();
        assert!(!test.has_quadlets());
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
        let test = discover_from_test_toml(&test_toml, &parsed, "my-test", None).unwrap();
        assert!(test.needs_browser());
    }

    #[test]
    fn browser_flag_on_lifecycle_test() {
        let dir = tempfile::tempdir().unwrap();
        let test_toml = dir.path().join("test.toml");
        std::fs::write(
            &test_toml,
            r#"
[test]
name = "sso-test"
browser = true

[[steps]]
action = "add"
service = "authelia"

[[steps]]
action = "assert"
name = "up"
run = "true"
"#,
        )
        .unwrap();

        let parsed = TestToml::parse(&test_toml).unwrap();
        let test = discover_from_test_toml(&test_toml, &parsed, "sso-test", None).unwrap();
        assert!(test.needs_browser());
        assert!(test.is_lifecycle());
    }
}
