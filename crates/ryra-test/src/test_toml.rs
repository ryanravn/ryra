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

/// A single named test within a test.toml file.
///
/// Two shapes are accepted for backwards compatibility during the
/// [[tests]]-array migration:
///
/// - **Multi-step (new)**: `steps` non-empty; `run` unset. Produces a
///   lifecycle-style execution reading the given steps directly.
/// - **Shell (legacy)**: `run` set; `steps` empty. Relies on `[setup]`
///   at the file level to deploy services before running `run`.
///
/// Exactly one of `run` / `steps` must be present — validated at parse time.
#[derive(Debug, Clone, Deserialize)]
pub struct TestDef {
    pub name: String,
    /// Legacy: a single shell command run after `[setup]` services deploy.
    #[serde(default)]
    pub run: Option<String>,
    /// New: a sequence of lifecycle steps (add / wait / http / shell / …).
    #[serde(default)]
    pub steps: Vec<StepDef>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Needs a browser VM image (for Playwright steps). Can also be set
    /// at the file level via `[test] browser = true`.
    #[serde(default)]
    pub browser: bool,
    /// Per-test RAM override (MB). File-level `[test] ram` still works.
    pub ram: Option<u32>,
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

fn default_content_type() -> String {
    "application/json".into()
}

/// HTTP method for the `http` test step. Kept as a typed enum so parsing
/// rejects typos at the boundary (per CLAUDE.md: enums over strings).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HttpMethod {
    #[default]
    Get,
    Post,
    Put,
    Delete,
}

impl HttpMethod {
    /// Upper-case verb for curl's `-X` flag.
    pub fn as_curl_arg(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
        }
    }
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
        #[serde(default)]
        method: HttpMethod,
        /// Request body for POST/PUT. Shell heredoc-safe: arbitrary bytes
        /// are supported including quotes and newlines.
        #[serde(default)]
        body: Option<String>,
        /// Content-Type header for requests with a body. Defaults to
        /// application/json since most API triggers we use ship JSON.
        #[serde(default = "default_content_type")]
        content_type: String,
        /// Extra request headers (e.g., `apikey`, `Authorization`). Values
        /// support shell variable expansion after `.env` sourcing.
        #[serde(default)]
        headers: BTreeMap<String, String>,
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
    /// Inbucket mail-delivery assertion. Polls inbucket's `/api/v1/mailbox/
    /// <mailbox>` endpoint until a non-empty response arrives; when
    /// `contains` is set, additionally requires that substring in the raw
    /// JSON body. Collapses the 8-line port-discovery + curl-poll pattern
    /// that previously lived in every SMTP test into one step.
    Mail {
        #[serde(default)]
        name: Option<String>,
        /// Local-part of the recipient address (`smtptest` for `smtptest@example.com`).
        mailbox: String,
        /// Optional substring required in the response body. Matches
        /// against the raw inbucket JSON, which includes subject + body.
        #[serde(default)]
        contains: Option<String>,
        /// Retry config. Defaults favour short SMTP mail delivery; apps
        /// with async mail queues (twenty, supabase) should widen these.
        #[serde(default = "default_mail_poll")]
        poll: PollConfig,
        #[serde(default = "default_timeout")]
        timeout: u64,
    },
}

fn default_mail_poll() -> PollConfig {
    PollConfig {
        interval: 2,
        attempts: 30,
    }
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
            StepDef::Mail { name, mailbox, .. } => {
                write!(f, "mail: {}", name.as_deref().unwrap_or(mailbox))
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

    /// Multi-line description for `--list -v`. Shows every field that
    /// meaningfully changes behaviour (args, env, headers, body, …).
    /// The caller indents each returned line.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        match self {
            StepDef::Add {
                service,
                args,
                env,
                timeout,
            } => {
                let args_s = args
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|a| format!(" {a}"))
                    .unwrap_or_default();
                lines.push(format!("ryra add {service}{args_s}  (timeout={timeout}s)"));
                for (k, v) in env {
                    lines.push(format!("  env {k}={v}"));
                }
            }
            StepDef::Remove { service } => lines.push(format!("ryra remove --purge {service}")),
            StepDef::Reset => lines.push("ryra reset".to_string()),
            StepDef::Wait { service, timeout } => {
                lines.push(format!("wait for {service}.service  (timeout={timeout}s)"));
            }
            StepDef::Shell {
                name,
                run,
                timeout,
                poll,
            } => {
                let poll_s = match poll {
                    Some(p) => {
                        format!(
                            "  poll={{interval={}s, attempts={}}}",
                            p.interval, p.attempts
                        )
                    }
                    None => String::new(),
                };
                lines.push(format!("shell '{name}'  (timeout={timeout}s{poll_s})"));
                for l in run.trim().lines() {
                    lines.push(format!("  | {l}"));
                }
            }
            StepDef::Http {
                name,
                url,
                method,
                body,
                content_type,
                headers,
                status,
                service,
                poll,
                timeout,
            } => {
                let label = name.as_deref().unwrap_or("(anon)");
                let verb = method.as_curl_arg();
                lines.push(format!(
                    "http '{label}': {verb} {url}  (expect {status}, timeout={timeout}s)"
                ));
                if let Some(svc) = service {
                    lines.push(format!("  env-source: {svc}/.env"));
                }
                for (k, v) in headers {
                    lines.push(format!("  header {k}: {v}"));
                }
                if let Some(b) = body {
                    lines.push(format!("  content-type: {content_type}"));
                    for l in b.trim().lines() {
                        lines.push(format!("  body> {l}"));
                    }
                }
                if let Some(p) = poll {
                    lines.push(format!(
                        "  poll: every {}s, up to {} attempts",
                        p.interval, p.attempts
                    ));
                }
            }
            StepDef::Playwright {
                name,
                spec,
                env,
                timeout,
            } => {
                let label = name.as_deref().unwrap_or(spec);
                lines.push(format!(
                    "playwright '{label}': spec={spec}  (timeout={timeout}s)"
                ));
                for (k, v) in env {
                    lines.push(format!("  env {k}={v}"));
                }
            }
            StepDef::Mail {
                name,
                mailbox,
                contains,
                poll,
                timeout,
            } => {
                let label = name.as_deref().unwrap_or(mailbox);
                lines.push(format!(
                    "mail '{label}': mailbox={mailbox}  (timeout={timeout}s)"
                ));
                if let Some(c) = contains {
                    lines.push(format!("  contains: {c}"));
                }
                lines.push(format!(
                    "  poll: every {}s, up to {} attempts",
                    poll.interval, poll.attempts
                ));
            }
        }
        lines
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

        // Top-level [[tests]] coexists with [[steps]] ONLY if all [[tests]]
        // are new-format (each brings its own `steps`). The legacy shape
        // (shell-style `run`-based tests with a shared [setup]) remains
        // mutually exclusive with top-level [[steps]].
        let has_legacy_run_tests = self
            .tests
            .iter()
            .any(|t| t.run.is_some() && t.steps.is_empty());
        if has_legacy_run_tests && !self.steps.is_empty() {
            anyhow::bail!(
                "{ctx}: test.toml cannot mix [setup]+[[tests]] (legacy shell) with top-level [[steps]] — \
                 migrate to the new [[tests]] + [[tests.steps]] format instead",
            );
        }

        for t in &self.tests {
            let has_run = t.run.is_some();
            let has_steps = !t.steps.is_empty();
            if has_run == has_steps {
                anyhow::bail!(
                    "{ctx}: test '{}' must set exactly one of `run` or `steps` \
                     (got run={}, steps={})",
                    t.name,
                    has_run,
                    has_steps,
                );
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
        assert_eq!(
            parsed.tests[0].run.as_deref(),
            Some("curl -sf http://localhost:8080")
        );
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
    fn mail_step_parses() {
        let toml = r#"
[[steps]]
action = "mail"
mailbox = "smtptest"
contains = "ryra smtp test"
poll = { interval = 3, attempts = 20 }
timeout = 60
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Mail {
            ref mailbox,
            ref contains,
            ref poll,
            timeout,
            ..
        } = parsed.steps[0]
        {
            assert_eq!(mailbox, "smtptest");
            assert_eq!(contains.as_deref(), Some("ryra smtp test"));
            assert_eq!(poll.interval, 3);
            assert_eq!(poll.attempts, 20);
            assert_eq!(timeout, 60);
        } else {
            panic!("expected Mail step");
        }
    }

    #[test]
    fn mail_step_defaults() {
        let toml = r#"
[[steps]]
action = "mail"
mailbox = "smtptest"
"#;
        let (_dir, path) = write_temp(toml);
        let parsed = TestToml::parse(&path).expect("parse");
        if let StepDef::Mail {
            ref contains,
            ref poll,
            timeout,
            ..
        } = parsed.steps[0]
        {
            assert!(contains.is_none(), "contains defaults to None");
            assert_eq!(poll.interval, 2, "default poll interval");
            assert_eq!(poll.attempts, 30, "default poll attempts");
            assert_eq!(timeout, 30);
        } else {
            panic!("expected Mail step");
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
