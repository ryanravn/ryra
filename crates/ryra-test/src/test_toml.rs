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

#[derive(Debug, Clone, Deserialize)]
pub struct StepDef {
    pub action: StepAction,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub args: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub spec: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    30
}

/// The action a lifecycle test step performs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepAction {
    Add,
    Remove,
    Reset,
    Wait,
    Run,
    Assert,
    Browser,
}

impl std::fmt::Display for StepAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepAction::Add => write!(f, "add"),
            StepAction::Remove => write!(f, "remove"),
            StepAction::Reset => write!(f, "reset"),
            StepAction::Wait => write!(f, "wait"),
            StepAction::Run => write!(f, "run"),
            StepAction::Assert => write!(f, "assert"),
            StepAction::Browser => write!(f, "browser"),
        }
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
    pub fn validate(&self, path: &Path) -> Result<()> {
        let ctx = path.display();

        if !self.tests.is_empty() && !self.steps.is_empty() {
            anyhow::bail!(
                "{ctx}: test.toml cannot have both [[tests]] and [[steps]] — \
                 use [[tests]] for simple assertions or [[steps]] for lifecycle tests",
            );
        }

        // Validate step required fields based on action type
        for (i, s) in self.steps.iter().enumerate() {
            let default_name = format!("step {}", i + 1);
            let step_ctx = s.name.as_deref().unwrap_or(&default_name);
            match s.action {
                StepAction::Add | StepAction::Remove | StepAction::Wait => {
                    if s.service.is_none() {
                        anyhow::bail!(
                            "{ctx}: {step_ctx} (action={}) requires a 'service' field",
                            s.action,
                        );
                    }
                }
                StepAction::Run | StepAction::Assert => {
                    if s.name.is_none() {
                        anyhow::bail!(
                            "{ctx}: step {} (action={}) requires a 'name' field",
                            i + 1,
                            s.action,
                        );
                    }
                    if s.run.is_none() {
                        anyhow::bail!(
                            "{ctx}: {step_ctx} (action={}) requires a 'run' field",
                            s.action,
                        );
                    }
                }
                StepAction::Reset => {}
                StepAction::Browser => {
                    if s.spec.is_none() {
                        anyhow::bail!(
                            "{ctx}: {step_ctx} (action=browser) requires a 'spec' field",
                        );
                    }
                }
            }
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
            if step.action == StepAction::Add
                && let Some(ref svc) = step.service
                && !services.contains(svc)
            {
                services.push(svc.clone());
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
action = "run"
name = "check-auth"
run = "curl -sf http://auth.test.local"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(parsed.needs_browser());
        assert!(parsed.is_lifecycle());
        assert_eq!(parsed.steps.len(), 2);
        assert_eq!(parsed.steps[0].action, StepAction::Add);
        assert_eq!(parsed.steps[0].service.as_deref(), Some("authelia"));
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
action = "browser"
name = "sso-test"
spec = "seafile-auth.spec.ts"
timeout = 120
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        assert!(parsed.is_lifecycle());
        assert_eq!(parsed.steps.len(), 2);
        assert_eq!(parsed.steps[1].action, StepAction::Browser);
        assert_eq!(parsed.steps[1].spec.as_deref(), Some("seafile-auth.spec.ts"));
    }

    #[test]
    fn browser_step_requires_spec() {
        let toml = r#"
[[steps]]
action = "browser"
name = "missing-spec"
"#;
        let (_dir, path) = write_temp(toml);
        let result = TestToml::parse(&path);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("spec"));
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
        assert_eq!(
            parsed.steps[0].args.as_deref(),
            Some("--domain proxy.test.local")
        );
    }
}
