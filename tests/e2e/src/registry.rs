use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A discovered test suite from the registry — either single-service or multi-service.
#[derive(Debug, Clone)]
pub enum DiscoveredTest {
    /// Tests from a `[[tests]]` section inside a `service.toml`.
    SingleService {
        service_name: String,
        tests: Vec<TestEntry>,
    },
    /// Tests from a `tests/*.toml` file in the registry.
    MultiService {
        name: String,
        services: Vec<String>,
        tests: Vec<TestEntry>,
    },
}

impl DiscoveredTest {
    pub fn name(&self) -> &str {
        match self {
            DiscoveredTest::SingleService { service_name, .. } => service_name,
            DiscoveredTest::MultiService { name, .. } => name,
        }
    }

    pub fn services(&self) -> Vec<&str> {
        match self {
            DiscoveredTest::SingleService { service_name, .. } => vec![service_name.as_str()],
            DiscoveredTest::MultiService { services, .. } => {
                services.iter().map(|s| s.as_str()).collect()
            }
        }
    }

    pub fn tests(&self) -> &[TestEntry] {
        match self {
            DiscoveredTest::SingleService { tests, .. } => tests,
            DiscoveredTest::MultiService { tests, .. } => tests,
        }
    }

    pub fn test_count(&self) -> usize {
        self.tests().len()
    }

    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        match self {
            DiscoveredTest::SingleService { service_name, .. } => service_name.clone(),
            DiscoveredTest::MultiService { services, name, .. } => {
                format!("{} ({})", name, services.join(" + "))
            }
        }
    }

    pub fn is_multi_service(&self) -> bool {
        matches!(self, DiscoveredTest::MultiService { .. })
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

    Ok(Some(DiscoveredTest::SingleService {
        service_name: service_name.to_string(),
        tests,
    }))
}

/// Parse a multi-service test file from tests/*.toml.
fn discover_multi_service(path: &PathBuf) -> Result<DiscoveredTest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

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

/// Look up the container image for a service from its service.toml.
pub fn service_image(registry_path: &Path, service_name: &str) -> Result<Option<String>> {
    let service_toml = registry_path.join(service_name).join("service.toml");
    if !service_toml.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&service_toml)
        .with_context(|| format!("failed to read {}", service_toml.display()))?;
    let parsed: ServiceTomlImage = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", service_toml.display()))?;
    Ok(parsed.service.image)
}

/// Get all container images needed for a test (including nginx for web services).
pub fn images_for_test(registry_path: &Path, test: &DiscoveredTest) -> Vec<String> {
    let mut images = Vec::new();
    for service in test.services() {
        if let Ok(Some(image)) = service_image(registry_path, service) {
            images.push(image);
        }
    }
    // nginx is always needed (ryra deploys it for web services)
    images.push("docker.io/library/nginx:alpine".to_string());
    images
}

// ---------------------------------------------------------------------------
// Lightweight TOML structs for parsing (avoids full ServiceDef dependency)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ServiceTomlImage {
    service: ServiceMetaImage,
}

#[derive(serde::Deserialize)]
struct ServiceMetaImage {
    #[serde(default)]
    image: Option<String>,
}

#[derive(serde::Deserialize)]
struct ServiceTomlTests {
    #[serde(default)]
    tests: Vec<TestToml>,
    #[serde(default)]
    env: Vec<EnvToml>,
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
    fn default_timeout_is_30() {
        let toml = r#"
[[tests]]
name = "basic"
run = "echo ok"
"#;
        let parsed: ServiceTomlTests = toml::from_str(toml).unwrap();
        assert_eq!(parsed.tests[0].timeout, 30);
    }

    #[test]
    fn discover_fixture_registry() {
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/registry");
        if !fixtures.exists() {
            return; // skip if not available
        }
        let discovered = discover(&fixtures).unwrap();
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
    }
}
