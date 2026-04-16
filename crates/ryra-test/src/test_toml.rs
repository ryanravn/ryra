use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Parsed test.toml — the unified test definition format.
#[derive(Debug, Deserialize)]
pub struct TestToml {
    #[serde(default)]
    pub test: Option<TestMeta>,
    #[serde(default)]
    pub setup: Option<SetupSection>,
    #[serde(default)]
    pub tests: Vec<TestDef>,
    #[serde(default)]
    pub steps: Vec<StepDef>,
}

#[derive(Debug, Deserialize)]
pub struct TestMeta {
    pub name: Option<String>,
    #[serde(default)]
    pub browser: bool,
    /// Optional RAM override (MB). When set, bypasses auto-calculation from
    /// service requirements. Use for tests that run many services and need
    /// more headroom than the sum of individual recommendations.
    pub ram: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SetupSection {
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub quadlets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestDef {
    pub name: String,
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}

fn default_add_timeout() -> u64 {
    300
}

fn default_http_status() -> u16 {
    200
}

/// Retry configuration for run steps. The runner re-executes the command
/// up to `attempts` times, sleeping `interval` seconds between tries.
#[derive(Debug, Clone, Deserialize)]
pub struct PollConfig {
    /// Seconds between retries.
    pub interval: u64,
    /// Maximum number of attempts before giving up.
    pub attempts: u64,
}

/// A lifecycle test step — serde deserializes directly into the correct
/// variant based on the `action` field. Invalid field combinations are
/// rejected at parse time rather than runtime.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum StepDef {
    Add {
        service: String,
        #[serde(default)]
        args: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default = "default_add_timeout")]
        timeout: u64,
    },
    Remove {
        service: String,
    },
    Reset,
    Wait {
        service: String,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
    /// Shell command step. Fails the test on non-zero exit code.
    Shell {
        name: String,
        run: String,
        #[serde(default = "default_timeout")]
        timeout: u64,
        /// Optional retry configuration. When set, the runner re-executes
        /// the command on failure, up to `attempts` times.
        #[serde(default)]
        poll: Option<PollConfig>,
    },
    /// HTTP request step. Sends a request and checks the response status code.
    /// The URL supports shell variable expansion (e.g., `$RYRA_PORT_HTTP`)
    /// after sourcing service `.env` files. Follows redirects automatically.
    Http {
        #[serde(default)]
        name: Option<String>,
        url: String,
        #[serde(default = "default_http_status")]
        status: u16,
        /// When set, only source this service's `.env` file (needed when
        /// multiple services define the same port variable).
        #[serde(default)]
        service: Option<String>,
        #[serde(default)]
        poll: Option<PollConfig>,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
    /// Playwright browser test step.
    Playwright {
        #[serde(default)]
        name: Option<String>,
        spec: String,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default = "default_browser_timeout")]
        timeout: u64,
    },
}

fn default_browser_timeout() -> u64 {
    120
}

impl std::fmt::Display for StepDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepDef::Add { service, .. } => write!(f, "add {service}"),
            StepDef::Remove { service } => write!(f, "remove {service}"),
            StepDef::Reset => write!(f, "reset"),
            StepDef::Wait { service, .. } => write!(f, "wait {service}"),
            StepDef::Shell { name, .. } => write!(f, "shell: {name}"),
            StepDef::Http { name, url, .. } => {
                write!(f, "http: {}", name.as_deref().unwrap_or(url))
            }
            StepDef::Playwright { name, spec, .. } => {
                write!(f, "browser: {}", name.as_deref().unwrap_or(spec))
            }
        }
    }
}

impl StepDef {
    /// The service name referenced by this step, if any.
    pub fn service(&self) -> Option<&str> {
        match self {
            StepDef::Add { service, .. }
            | StepDef::Remove { service }
            | StepDef::Wait { service, .. } => Some(service),
            _ => None,
        }
    }

    /// Whether this step is a setup step (vs. a test/assertion step).
    /// Used by `--retest` to skip setup and only re-run test steps.
    pub fn is_setup(&self) -> bool {
        matches!(
            self,
            StepDef::Add { .. } | StepDef::Remove { .. } | StepDef::Reset | StepDef::Wait { .. }
        )
    }

    /// Human-readable name for this step (used in output).
    pub fn step_name(&self) -> String {
        format!("{self}")
    }
}

impl TestToml {
    /// Read and deserialize a test.toml file, then validate it.
    pub fn parse(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read test.toml at {}", path.display()))?;
        let parsed: Self = toml::from_str(&content)
            .with_context(|| format!("failed to parse test.toml at {}", path.display()))?;
        parsed.validate(path)?;
        Ok(parsed)
    }

    /// Validate structural invariants after deserialization.
    ///
    /// Most field-level validation is handled by serde (the tagged enum
    /// rejects missing required fields at parse time). This only checks
    /// cross-field invariants that serde can't express.
    pub fn validate(&self, path: &Path) -> Result<()> {
        let ctx = path.display();

        if !self.tests.is_empty() && !self.steps.is_empty() {
            anyhow::bail!(
                "{ctx}: test.toml cannot have both [[tests]] and [[steps]] — \
                 use [[tests]] for simple assertions or [[steps]] for lifecycle tests",
            );
        }

        Ok(())
    }

    /// True if this is a lifecycle test (uses [[steps]] instead of [[tests]]).
    pub fn is_lifecycle(&self) -> bool {
        !self.steps.is_empty()
    }

    /// True if this test requires a browser VM image.
    pub fn needs_browser(&self) -> bool {
        self.test.as_ref().is_some_and(|t| t.browser)
    }

    /// Explicit RAM override (MB) from [test] metadata, if set.
    pub fn ram_override(&self) -> Option<u32> {
        self.test.as_ref().and_then(|t| t.ram)
    }

    /// The test name from [test] metadata, or the file stem as a fallback.
    pub fn name_or_default(&self, path: &Path) -> String {
        if let Some(ref meta) = self.test
            && let Some(ref name) = meta.name
        {
            return name.clone();
        }
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    /// All services referenced: from setup.services + any `add` steps.
    pub fn referenced_services(&self) -> Vec<String> {
        let mut services: Vec<String> = self
            .setup
            .as_ref()
            .map_or_else(Vec::new, |s| s.services.clone());

        for step in &self.steps {
            if let StepDef::Add { service, .. } = step
                && !services.contains(service)
            {
                services.push(service.clone());
            }
        }

        services
    }

    /// Quadlet files declared in [setup].
    pub fn quadlet_files(&self) -> Vec<String> {
        self.setup
            .as_ref()
            .map_or_else(Vec::new, |s| s.quadlets.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_temp(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(content.as_bytes()).expect("write");
        (dir, path)
    }

    #[test]
    fn parse_simple_test_toml() {
        let toml = r#"
[setup]
services = ["caddy", "myapp"]

[[tests]]
name = "app responds"
run = "curl -sf http://localhost:8080"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert_eq!(
            parsed.setup.as_ref().unwrap().services,
            vec!["caddy", "myapp"]
        );
        assert_eq!(parsed.tests.len(), 1);
        assert_eq!(parsed.tests[0].name, "app responds");
        assert_eq!(parsed.tests[0].run, "curl -sf http://localhost:8080");
        assert!(!parsed.is_lifecycle());
    }

    #[test]
    fn parse_lifecycle_test_toml() {
        let toml = r#"
[test]
name = "sso flow"
browser = true

[[steps]]
action = "add"
service = "authelia"

[[steps]]
action = "shell"
name = "check-auth"
run = "curl -sf http://auth.test.local"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(parsed.needs_browser());
        assert!(parsed.is_lifecycle());
        assert_eq!(parsed.steps.len(), 2);
        assert!(matches!(parsed.steps[0], StepDef::Add { .. }));
        if let StepDef::Add { ref service, .. } = parsed.steps[0] {
            assert_eq!(service, "authelia");
        }
    }

    #[test]
    fn parse_quadlet_test_toml() {
        let toml = r#"
[setup]
services = ["caddy"]
quadlets = ["myapp.container", "myapp-db.container"]

[[tests]]
name = "quadlet app responds"
run = "curl -sf http://localhost:9000"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert_eq!(
            parsed.setup.as_ref().unwrap().quadlets,
            vec!["myapp.container", "myapp-db.container"]
        );
        assert_eq!(
            parsed.quadlet_files(),
            vec!["myapp.container", "myapp-db.container"]
        );
        assert_eq!(parsed.setup.as_ref().unwrap().services, vec!["caddy"]);
        assert_eq!(parsed.tests.len(), 1);
    }

    #[test]
    fn reject_mixed_tests_and_steps() {
        let toml = r#"
[[tests]]
name = "foo"
run = "true"

[[steps]]
action = "add"
service = "bar"
"#;
        let (_dir, path) = write_temp(toml);
        let result = TestToml::parse(&path);
        assert!(result.is_err(), "expected error for mixed tests+steps");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("[[tests]]") || msg.contains("[[steps]]"));
    }

    #[test]
    fn name_from_metadata() {
        let toml = r#"
[test]
name = "my explicit name"

[[tests]]
name = "check"
run = "true"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert_eq!(parsed.name_or_default(&path), "my explicit name");
    }

    #[test]
    fn name_from_filename() {
        let toml = r#"
[[tests]]
name = "check"
run = "true"
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("immich-sso.toml");
        std::fs::write(&path, toml).expect("write");
        let parsed = TestToml::parse(&path).expect("parse");
        assert_eq!(parsed.name_or_default(&path), "immich-sso");
    }

    #[test]
    fn parse_browser_step() {
        let toml = r#"
[test]
name = "auth-browser"
browser = true

[[steps]]
action = "add"
service = "caddy"

[[steps]]
action = "playwright"
name = "sso-test"
spec = "seafile-auth.spec.ts"
timeout = 120
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(parsed.is_lifecycle());
        assert_eq!(parsed.steps.len(), 2);
        assert!(matches!(parsed.steps[1], StepDef::Playwright { .. }));
        if let StepDef::Playwright { ref spec, .. } = parsed.steps[1] {
            assert_eq!(spec, "seafile-auth.spec.ts");
        }
    }

    #[test]
    fn browser_step_requires_spec() {
        let toml = r#"
[[steps]]
action = "playwright"
"#;
        let (_dir, path) = write_temp(toml);
        let result = TestToml::parse(&path);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("spec") || msg.contains("missing field"));
    }

    #[test]
    fn step_with_args() {
        let toml = r#"
[[steps]]
action = "add"
service = "caddy"
args = "--domain proxy.test.local"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Add { ref args, .. } = parsed.steps[0] {
            assert_eq!(args.as_deref(), Some("--domain proxy.test.local"));
        } else {
            panic!("expected Add step");
        }
    }

    #[test]
    fn shell_step_parses() {
        let toml = r#"
[[steps]]
action = "shell"
name = "check"
run = "true"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(matches!(parsed.steps[0], StepDef::Shell { .. }));
    }

    #[test]
    fn run_step_with_poll() {
        let toml = r#"
[[steps]]
action = "shell"
name = "wait-for-http"
run = "curl -sf http://localhost:8080"
poll = { interval = 5, attempts = 10 }
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Shell { ref poll, .. } = parsed.steps[0] {
            let p = poll.as_ref().expect("poll should be Some");
            assert_eq!(p.interval, 5);
            assert_eq!(p.attempts, 10);
        } else {
            panic!("expected Run step");
        }
    }

    #[test]
    fn run_step_rejects_missing_name() {
        let toml = r#"
[[steps]]
action = "shell"
run = "true"
"#;
        let (_dir, path) = write_temp(toml);
        let result = TestToml::parse(&path);
        assert!(result.is_err(), "run step without 'name' should fail");
    }

    #[test]
    fn add_step_with_env() {
        let toml = r#"
[[steps]]
action = "add"
service = "authelia"
args = "--url https://auth.localhost:8443"
env = { AUTHELIA_ADMIN_USER = "testuser", AUTHELIA_ADMIN_PASSWORD = "pass123" }
timeout = 300
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Add {
            ref service,
            ref args,
            ref env,
            timeout,
        } = parsed.steps[0]
        {
            assert_eq!(service, "authelia");
            assert_eq!(args.as_deref(), Some("--url https://auth.localhost:8443"));
            assert_eq!(env.get("AUTHELIA_ADMIN_USER").unwrap(), "testuser");
            assert_eq!(env.get("AUTHELIA_ADMIN_PASSWORD").unwrap(), "pass123");
            assert_eq!(timeout, 300);
        } else {
            panic!("expected Add step");
        }
    }

    #[test]
    fn add_step_default_timeout() {
        let toml = r#"
[[steps]]
action = "add"
service = "whoami"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Add { timeout, .. } = parsed.steps[0] {
            assert_eq!(timeout, 300);
        } else {
            panic!("expected Add step");
        }
    }

    #[test]
    fn http_step_parses() {
        let toml = r#"
[[steps]]
action = "http"
url = "http://127.0.0.1:$RYRA_PORT_HTTP/health"
status = 200
poll = { interval = 5, attempts = 10 }
timeout = 60
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Http {
            ref url,
            status,
            ref poll,
            timeout,
            ..
        } = parsed.steps[0]
        {
            assert_eq!(url, "http://127.0.0.1:$RYRA_PORT_HTTP/health");
            assert_eq!(status, 200);
            assert_eq!(poll.as_ref().unwrap().attempts, 10);
            assert_eq!(timeout, 60);
        } else {
            panic!("expected Http step");
        }
    }

    #[test]
    fn http_step_defaults() {
        let toml = r#"
[[steps]]
action = "http"
url = "http://localhost:8080"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Http {
            status, timeout, ..
        } = parsed.steps[0]
        {
            assert_eq!(status, 200);
            assert_eq!(timeout, 30);
        } else {
            panic!("expected Http step");
        }
    }

    #[test]
    fn is_setup_classification() {
        let toml = r#"
[[steps]]
action = "add"
service = "whoami"

[[steps]]
action = "remove"
service = "whoami"

[[steps]]
action = "reset"

[[steps]]
action = "wait"
service = "whoami"

[[steps]]
action = "shell"
name = "check"
run = "true"

[[steps]]
action = "http"
url = "http://localhost:8080"

[[steps]]
action = "playwright"
spec = "test.spec.ts"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(parsed.steps[0].is_setup(), "add should be setup");
        assert!(parsed.steps[1].is_setup(), "remove should be setup");
        assert!(parsed.steps[2].is_setup(), "reset should be setup");
        assert!(parsed.steps[3].is_setup(), "wait should be setup");
        assert!(!parsed.steps[4].is_setup(), "shell should not be setup");
        assert!(!parsed.steps[5].is_setup(), "http should not be setup");
        assert!(
            !parsed.steps[6].is_setup(),
            "playwright should not be setup"
        );
    }
}
