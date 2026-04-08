use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A discovered test suite from the registry — either single-service, multi-service, or lifecycle.
#[derive(Debug, Clone)]
pub enum DiscoveredTest {
    /// Tests from a `[[tests]]` section inside a `service.toml`.
    SingleService {
        service_name: String,
        /// Services that must be installed before this one (from `[[requires]]`).
        requires: Vec<String>,
        tests: Vec<TestEntry>,
    },
    /// Tests from a `tests/*.toml` file in the registry (services + tests format).
    MultiService {
        name: String,
        services: Vec<String>,
        tests: Vec<TestEntry>,
    },
    /// Lifecycle tests from a `tests/*.toml` file with `[[steps]]` — interleaved actions and assertions.
    Lifecycle {
        name: String,
        images: Vec<String>,
        steps: Vec<StepEntry>,
        /// Whether this test needs a browser-ready VM (bun + playwright + chromium).
        browser: bool,
    },
}

/// A step in a lifecycle test — either an action or an assertion.
#[derive(Debug, Clone)]
pub enum StepEntry {
    Add {
        service: String,
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
            DiscoveredTest::SingleService { service_name, .. } => service_name,
            DiscoveredTest::MultiService { name, .. } => name,
            DiscoveredTest::Lifecycle { name, .. } => name,
        }
    }

    /// All services that need to be deployed for this test, in install order.
    /// For SingleService, this includes requires (dependencies first) then the service itself.
    pub fn services(&self) -> Vec<&str> {
        match self {
            DiscoveredTest::SingleService {
                service_name,
                requires,
                ..
            } => {
                let mut svcs: Vec<&str> = requires.iter().map(|s| s.as_str()).collect();
                svcs.push(service_name.as_str());
                svcs
            }
            DiscoveredTest::MultiService { services, .. } => {
                services.iter().map(|s| s.as_str()).collect()
            }
            DiscoveredTest::Lifecycle { images, .. } => {
                // Lifecycle tests declare images directly, not services
                // Return empty — image loading uses images_for_test() which handles this
                images.iter().map(|s| s.as_str()).collect()
            }
        }
    }

    pub fn tests(&self) -> &[TestEntry] {
        match self {
            DiscoveredTest::SingleService { tests, .. } => tests,
            DiscoveredTest::MultiService { tests, .. } => tests,
            DiscoveredTest::Lifecycle { .. } => &[],
        }
    }

    pub fn test_count(&self) -> usize {
        match self {
            DiscoveredTest::SingleService { tests, .. } => tests.len(),
            DiscoveredTest::MultiService { tests, .. } => tests.len(),
            DiscoveredTest::Lifecycle { steps, .. } => steps.len(),
        }
    }

    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        match self {
            DiscoveredTest::SingleService { service_name, .. } => service_name.clone(),
            DiscoveredTest::MultiService { services, name, .. } => {
                format!("{} ({})", name, services.join(" + "))
            }
            DiscoveredTest::Lifecycle { name, steps, .. } => {
                format!("{} ({} steps)", name, steps.len())
            }
        }
    }

    #[allow(dead_code)]
    pub fn is_multi_service(&self) -> bool {
        matches!(self, DiscoveredTest::MultiService { .. })
    }

    pub fn is_lifecycle(&self) -> bool {
        matches!(self, DiscoveredTest::Lifecycle { .. })
    }

    pub fn needs_browser(&self) -> bool {
        matches!(self, DiscoveredTest::Lifecycle { browser: true, .. })
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

/// Scan a registry directory for all test definitions.
///
/// Reads `[[tests]]` from every `*/service.toml` and standalone test
/// files from `tests/*.toml`.
pub fn discover(registry_path: &Path) -> Result<Vec<DiscoveredTest>> {
    let mut discovered = Vec::new();

    // Scan service directories for [[tests]] in service.toml
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

        let service_toml = path.join("service.toml");
        if !service_toml.exists() {
            continue;
        }

        match discover_single_service(&service_toml, &dir_name) {
            Ok(Some(test)) => discovered.push(test),
            Ok(None) => {} // no tests defined, that's fine
            Err(e) => {
                eprintln!("warning: failed to parse {}: {e}", service_toml.display());
            }
        }
    }

    // Scan tests/ directory for multi-service test files
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

            match discover_multi_service(&path) {
                Ok(test) => discovered.push(test),
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

/// Parse a service.toml and extract its [[tests]] if any.
fn discover_single_service(path: &PathBuf, service_name: &str) -> Result<Option<DiscoveredTest>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Parse just enough to get the tests — we use a lightweight struct
    // to avoid needing the full ServiceDef deserialization (which requires
    // the image field etc.)
    let parsed: ServiceTomlTests =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;

    if parsed.tests.is_empty() {
        return Ok(None);
    }

    // Validate required env vars are covered
    let required: Vec<&str> = parsed
        .env
        .iter()
        .filter(|e| e.kind.as_deref() == Some("required"))
        .map(|e| e.name.as_str())
        .collect();

    if !required.is_empty() {
        for test in &parsed.tests {
            for var in &required {
                if !test.env.contains_key(*var) {
                    anyhow::bail!(
                        "test '{}' in service '{}' missing required env var '{}'",
                        test.name,
                        service_name,
                        var
                    );
                }
            }
        }
    }

    let tests = parsed
        .tests
        .into_iter()
        .map(|t| TestEntry {
            name: t.name,
            run: t.run,
            timeout_secs: t.timeout,
            env: t.env,
        })
        .collect();

    let requires: Vec<String> = parsed.requires.iter().map(|r| r.service.clone()).collect();

    Ok(Some(DiscoveredTest::SingleService {
        service_name: service_name.to_string(),
        requires,
        tests,
    }))
}

/// Parse a test file from tests/*.toml — detects lifecycle vs multi-service format.
fn discover_multi_service(path: &PathBuf) -> Result<DiscoveredTest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Try lifecycle format first (has [[steps]])
    if content.contains("[[steps]]") {
        return discover_lifecycle(path, &content);
    }

    let parsed: MultiServiceToml =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;

    if parsed.test.services.is_empty() {
        anyhow::bail!(
            "multi-service test '{}' has no services listed",
            parsed.test.name
        );
    }

    let tests = parsed
        .tests
        .into_iter()
        .map(|t| TestEntry {
            name: t.name,
            run: t.run,
            timeout_secs: t.timeout,
            env: t.env,
        })
        .collect();

    Ok(DiscoveredTest::MultiService {
        name: parsed.test.name,
        services: parsed.test.services,
        tests,
    })
}

/// Parse a lifecycle test file with [[steps]].
fn discover_lifecycle(path: &Path, content: &str) -> Result<DiscoveredTest> {
    let parsed: LifecycleToml =
        toml::from_str(content).with_context(|| format!("failed to parse {}", path.display()))?;

    if parsed.steps.is_empty() {
        anyhow::bail!("lifecycle test '{}' has no steps defined", parsed.test.name);
    }

    let mut steps = Vec::new();
    for s in parsed.steps {
        let step = match s.action.as_str() {
            "add" => {
                let service = s.service.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'add' requires a 'service' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                StepEntry::Add { service }
            }
            "remove" => {
                let service = s.service.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'remove' requires a 'service' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                StepEntry::Remove { service }
            }
            "reset" => StepEntry::Reset,
            "wait" => {
                let service = s.service.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'wait' requires a 'service' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                StepEntry::Wait {
                    service,
                    timeout_secs: if s.timeout > 0 { s.timeout } else { 60 },
                }
            }
            "run" => {
                let name = s.name.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'run' requires a 'name' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                let run = s.run.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'run' requires a 'run' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                StepEntry::Run {
                    name,
                    run,
                    timeout_secs: s.timeout,
                }
            }
            "assert" => {
                let name = s.name.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'assert' requires a 'name' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                let run = s.run.ok_or_else(|| {
                    anyhow::anyhow!(
                        "step 'assert' requires a 'run' field in test '{}'",
                        parsed.test.name
                    )
                })?;
                StepEntry::Assert {
                    name,
                    run,
                    timeout_secs: s.timeout,
                }
            }
            other => {
                anyhow::bail!(
                    "unknown step action '{}' in test '{}'",
                    other,
                    parsed.test.name
                );
            }
        };
        steps.push(step);
    }

    Ok(DiscoveredTest::Lifecycle {
        name: parsed.test.name,
        images: parsed.test.images,
        steps,
        browser: parsed.test.browser,
    })
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
    let services: Vec<&str> = match test {
        DiscoveredTest::Lifecycle { steps, .. } => steps
            .iter()
            .filter_map(|s| match s {
                StepEntry::Add { service } => Some(service.as_str()),
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
        DiscoveredTest::Lifecycle {
            images: declared,
            steps,
            ..
        } => {
            // Use explicitly declared images
            images.extend(declared.iter().cloned());
            // Look up images for services referenced in add steps or run steps
            // that call `ryra add <service>`
            for step in steps {
                let service_name = match step {
                    StepEntry::Add { service } => Some(service.as_str()),
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
        _ => {
            for service in test.services() {
                for image in service_images(registry_path, service) {
                    if !images.contains(&image) {
                        images.push(image);
                    }
                }
            }
        }
    }

    images
}

// ---------------------------------------------------------------------------
// Lightweight TOML structs for parsing (avoids full ServiceDef dependency)
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

#[derive(serde::Deserialize)]
struct ServiceTomlTests {
    #[serde(default)]
    tests: Vec<TestToml>,
    #[serde(default)]
    env: Vec<EnvToml>,
    #[serde(default)]
    requires: Vec<RequiresToml>,
}

#[derive(serde::Deserialize)]
struct RequiresToml {
    service: String,
}

#[derive(serde::Deserialize)]
struct EnvToml {
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(serde::Deserialize)]
struct TestToml {
    name: String,
    run: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}

#[derive(serde::Deserialize)]
struct MultiServiceToml {
    test: MultiServiceMeta,
    #[serde(default)]
    tests: Vec<TestToml>,
}

#[derive(serde::Deserialize)]
struct MultiServiceMeta {
    name: String,
    services: Vec<String>,
}

#[derive(serde::Deserialize)]
struct LifecycleToml {
    test: LifecycleMeta,
    #[serde(default)]
    steps: Vec<StepToml>,
}

#[derive(serde::Deserialize)]
struct LifecycleMeta {
    name: String,
    /// Container images to pre-load into the VM.
    #[serde(default)]
    images: Vec<String>,
    /// Whether this test needs a browser (bun + playwright + chromium).
    #[serde(default)]
    browser: bool,
}

#[derive(serde::Deserialize)]
struct StepToml {
    action: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_single_service_tests() {
        let toml = r#"
[service]
name = "whoami"
description = "test"
image = "traefik/whoami"

[[ports]]
name = "http"
container_port = 80

[[tests]]
name = "responds"
run = "curl -sf http://127.0.0.1:$RYRA_PORT_HTTP"

[[tests]]
name = "hostname"
run = "curl -s http://127.0.0.1:$RYRA_PORT_HTTP | grep -q Hostname"
timeout = 10
"#;
        let parsed: ServiceTomlTests = toml::from_str(toml).unwrap();
        assert_eq!(parsed.tests.len(), 2);
        assert_eq!(parsed.tests[0].name, "responds");
        assert_eq!(parsed.tests[1].timeout, 10);
    }

    #[test]
    fn discover_multi_service_tests() {
        let toml = r#"
[test]
name = "whoami-plus-postgres"
services = ["whoami", "postgres"]

[[tests]]
name = "both-running"
run = "echo ok"
"#;
        let parsed: MultiServiceToml = toml::from_str(toml).unwrap();
        assert_eq!(parsed.test.name, "whoami-plus-postgres");
        assert_eq!(parsed.test.services, vec!["whoami", "postgres"]);
        assert_eq!(parsed.tests.len(), 1);
    }

    #[test]
    fn required_env_validation() {
        let toml = r#"
[service]
name = "gitea"
description = "test"
image = "gitea/gitea"

[[env]]
name = "GITEA_DOMAIN"
kind = "required"
prompt = "Enter domain"

[[tests]]
name = "responds"
run = "curl -sf http://localhost"
"#;
        let parsed: ServiceTomlTests = toml::from_str(toml).unwrap();
        let required: Vec<&str> = parsed
            .env
            .iter()
            .filter(|e| e.kind.as_deref() == Some("required"))
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(required, vec!["GITEA_DOMAIN"]);

        // Test is missing the required var
        assert!(!parsed.tests[0].env.contains_key("GITEA_DOMAIN"));
    }

    #[test]
    fn required_env_provided() {
        let toml = r#"
[service]
name = "gitea"
description = "test"
image = "gitea/gitea"

[[env]]
name = "GITEA_DOMAIN"
kind = "required"
prompt = "Enter domain"

[[tests]]
name = "responds"
run = "curl -sf http://localhost"
env = { GITEA_DOMAIN = "localhost" }
"#;
        let parsed: ServiceTomlTests = toml::from_str(toml).unwrap();
        assert_eq!(
            parsed.tests[0].env.get("GITEA_DOMAIN").unwrap(),
            "localhost"
        );
    }

    #[test]
    fn discover_registry() {
        let registry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../registry");
        if !registry.exists() {
            return; // skip if not available
        }
        let discovered = discover(&registry).unwrap();
        let names: Vec<&str> = discovered.iter().map(|d| d.name()).collect();
        assert!(names.contains(&"whoami"));
        assert!(names.contains(&"postgres"));
        assert!(names.contains(&"whoami-plus-postgres"));

        // Check whoami has tests
        let whoami = discovered.iter().find(|d| d.name() == "whoami").unwrap();
        assert!(!whoami.is_multi_service());
        assert!(whoami.test_count() >= 2);

        // Check multi-service test
        let combo = discovered
            .iter()
            .find(|d| d.name() == "whoami-plus-postgres")
            .unwrap();
        assert!(combo.is_multi_service());
        assert_eq!(combo.services(), vec!["whoami", "postgres"]);

        // Check lifecycle tests are discovered
        let remove = discovered
            .iter()
            .find(|d| d.name() == "remove-whoami")
            .unwrap();
        assert!(remove.is_lifecycle());
        assert!(remove.test_count() > 0);

        let reset = discovered.iter().find(|d| d.name() == "reset").unwrap();
        assert!(reset.is_lifecycle());

        let readd = discovered
            .iter()
            .find(|d| d.name() == "re-add-after-remove")
            .unwrap();
        assert!(readd.is_lifecycle());

        let idempotent = discovered
            .iter()
            .find(|d| d.name() == "idempotent-init")
            .unwrap();
        assert!(idempotent.is_lifecycle());
    }

    #[test]
    fn parse_lifecycle_toml() {
        let toml = r#"
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
"#;
        let parsed: LifecycleToml = toml::from_str(toml).unwrap();
        assert_eq!(parsed.test.name, "remove-test");
        assert_eq!(parsed.steps.len(), 5);
        assert_eq!(parsed.steps[0].action, "add");
        assert_eq!(parsed.steps[0].service.as_deref(), Some("whoami"));
        assert_eq!(parsed.steps[2].action, "assert");
        assert_eq!(parsed.steps[2].name.as_deref(), Some("responds"));
        assert_eq!(parsed.steps[3].action, "remove");
    }

    #[test]
    fn lifecycle_with_images() {
        let toml = r#"
[test]
name = "custom-images"
images = ["docker.io/custom/image:latest"]

[[steps]]
action = "add"
service = "whoami"

[[steps]]
action = "reset"
"#;
        let parsed: LifecycleToml = toml::from_str(toml).unwrap();
        assert_eq!(parsed.test.images, vec!["docker.io/custom/image:latest"]);
        assert_eq!(parsed.steps.len(), 2);
        assert_eq!(parsed.steps[1].action, "reset");
        assert!(parsed.steps[1].service.is_none());
    }
}
