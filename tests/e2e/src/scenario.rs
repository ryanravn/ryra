use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::machine::Machine;

// ---------------------------------------------------------------------------
// Result types — full trace of what happened in each scenario
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ScenarioResult {
    pub name: String,
    pub events: Vec<Event>,
    pub duration: Duration,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub struct Event {
    pub description: String,
    pub kind: EventKind,
    pub outcome: Outcome,
    pub duration: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Init,
    Step,
    Assertion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed(String),
    Skipped,
}

impl Outcome {
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Passed)
    }

    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }
}

impl ScenarioResult {
    pub fn passed(&self) -> bool {
        self.outcome.is_pass()
    }
}

impl fmt::Display for ScenarioResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let icon = match &self.outcome {
            Outcome::Passed => "PASS",
            Outcome::Failed(_) => "FAIL",
            Outcome::Skipped => "SKIP",
        };
        writeln!(
            f,
            "{icon}  {} ({:.1}s)",
            self.name,
            self.duration.as_secs_f64()
        )?;

        for event in &self.events {
            let mark = match &event.outcome {
                Outcome::Passed => " ok ",
                Outcome::Failed(_) => "FAIL",
                Outcome::Skipped => "skip",
            };
            let kind_label = match event.kind {
                EventKind::Init => "init",
                EventKind::Step => "step",
                EventKind::Assertion => "assert",
            };
            write!(
                f,
                "  [{mark}] {kind_label}: {} ({:.1}s)",
                event.description,
                event.duration.as_secs_f64()
            )?;
            if let Outcome::Failed(msg) = &event.outcome {
                write!(f, "\n         {msg}")?;
            }
            writeln!(f)?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Scenario builder
// ---------------------------------------------------------------------------

pub struct Scenario {
    pub name: &'static str,
    pub repo: &'static str,
    phases: Vec<Phase>,
}

/// A phase is either a step or an assertion, kept in order.
/// This lets assertions run between steps, not just at the end.
enum Phase {
    Step(Step),
    Assertion(Assertion),
}

enum Step {
    Add { service: &'static str },
    Remove { service: &'static str },
    Reset,
}

enum Assertion {
    Running { service: &'static str },
    NotRunning { service: &'static str },
    HttpOk { service: &'static str, status: u16 },
    UserExists { username: &'static str },
    UserNotExists { username: &'static str },
    FileExists { path: &'static str },
    FileNotExists { path: &'static str },
    ConfigContains { text: &'static str },
    ConfigNotContains { text: &'static str },
    JournalClean { service: &'static str },
}

impl Step {
    fn describe(&self) -> String {
        match self {
            Step::Add { service } => format!("ryra add {service}"),
            Step::Remove { service } => format!("ryra remove {service}"),
            Step::Reset => "ryra reset".to_string(),
        }
    }
}

impl Assertion {
    fn describe(&self) -> String {
        match self {
            Assertion::Running { service } => format!("{service} is running"),
            Assertion::NotRunning { service } => format!("{service} is not running"),
            Assertion::HttpOk { service, status } => format!("{service} returns HTTP {status}"),
            Assertion::UserExists { username } => format!("user {username} exists"),
            Assertion::UserNotExists { username } => format!("user {username} does not exist"),
            Assertion::FileExists { path } => format!("{path} exists"),
            Assertion::FileNotExists { path } => format!("{path} does not exist"),
            Assertion::ConfigContains { text } => format!("config contains '{text}'"),
            Assertion::ConfigNotContains { text } => format!("config does not contain '{text}'"),
            Assertion::JournalClean { service } => format!("{service} journal is clean"),
        }
    }
}

impl Scenario {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            repo: "/opt/ryra-test-registry",
            phases: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn repo(mut self, repo: &'static str) -> Self {
        self.repo = repo;
        self
    }

    pub fn add(mut self, service: &'static str) -> Self {
        self.phases.push(Phase::Step(Step::Add { service }));
        self
    }

    pub fn remove(mut self, service: &'static str) -> Self {
        self.phases.push(Phase::Step(Step::Remove { service }));
        self
    }

    pub fn reset(mut self) -> Self {
        self.phases.push(Phase::Step(Step::Reset));
        self
    }

    pub fn assert_running(mut self, service: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::Running { service }));
        self
    }

    pub fn assert_not_running(mut self, service: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::NotRunning { service }));
        self
    }

    pub fn assert_http(mut self, service: &'static str, status: u16) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::HttpOk { service, status }));
        self
    }

    pub fn assert_user_exists(mut self, username: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::UserExists { username }));
        self
    }

    pub fn assert_user_not_exists(mut self, username: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::UserNotExists { username }));
        self
    }

    pub fn assert_file_exists(mut self, path: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::FileExists { path }));
        self
    }

    pub fn assert_file_not_exists(mut self, path: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::FileNotExists { path }));
        self
    }

    pub fn assert_config_contains(mut self, text: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::ConfigContains { text }));
        self
    }

    pub fn assert_config_not_contains(mut self, text: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::ConfigNotContains { text }));
        self
    }

    pub fn assert_journal_clean(mut self, service: &'static str) -> Self {
        self.phases
            .push(Phase::Assertion(Assertion::JournalClean { service }));
        self
    }

    // -----------------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------------

    pub async fn run(&self, c: &Machine) -> ScenarioResult {
        let start = Instant::now();
        let mut events = Vec::new();
        let mut failed = false;

        // Init is always the first event
        let init_event = self.run_init(c).await;
        if init_event.outcome.is_fail() {
            failed = true;
        }
        events.push(init_event);

        // Execute phases in order, skipping remaining if a step fails
        for phase in &self.phases {
            if failed {
                let (desc, kind) = match phase {
                    Phase::Step(s) => (s.describe(), EventKind::Step),
                    Phase::Assertion(a) => (a.describe(), EventKind::Assertion),
                };
                events.push(Event {
                    description: desc,
                    kind,
                    outcome: Outcome::Skipped,
                    duration: Duration::ZERO,
                });
                continue;
            }

            let event = match phase {
                Phase::Step(step) => self.run_step(c, step).await,
                Phase::Assertion(assertion) => self.run_assertion(c, assertion).await,
            };

            // A failed step skips everything after. A failed assertion keeps going.
            if event.outcome.is_fail() && event.kind == EventKind::Step {
                failed = true;
            }
            if event.outcome.is_fail() && !failed {
                failed = true;
            }
            events.push(event);
        }

        let outcome = if failed {
            let first_failure = events
                .iter()
                .find_map(|e| match &e.outcome {
                    Outcome::Failed(msg) => Some(msg.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown failure".to_string());
            Outcome::Failed(first_failure)
        } else {
            Outcome::Passed
        };

        ScenarioResult {
            name: self.name.to_string(),
            events,
            duration: start.elapsed(),
            outcome,
        }
    }

    async fn run_init(&self, c: &Machine) -> Event {
        let t = Instant::now();
        let cmd = format!("ryra init --repo {}", self.repo);
        let outcome = match c.exec(&cmd).await {
            Ok(_) => Outcome::Passed,
            Err(e) => Outcome::Failed(format!("{e:#}")),
        };
        Event {
            description: cmd,
            kind: EventKind::Init,
            outcome,
            duration: t.elapsed(),
        }
    }

    async fn run_step(&self, c: &Machine, step: &Step) -> Event {
        let t = Instant::now();
        let description = step.describe();

        let outcome = match step {
            Step::Add { service } => {
                let cmd = format!("ryra add {service} --repo {}", self.repo);
                match c.exec(&cmd).await {
                    Ok(_) => {
                        // Wait for service to come up
                        // 5 minutes — pulling container images is slow without KVM
                        match c
                            .wait_for_service(
                                service,
                                &format!("{service}.service"),
                                Duration::from_secs(300),
                            )
                            .await
                        {
                            Ok(()) => Outcome::Passed,
                            Err(e) => Outcome::Failed(format!("service didn't start: {e:#}")),
                        }
                    }
                    Err(e) => Outcome::Failed(format!("{e:#}")),
                }
            }
            Step::Remove { service } => match c.exec(&format!("ryra remove {service} --yes")).await
            {
                Ok(_) => Outcome::Passed,
                Err(e) => Outcome::Failed(format!("{e:#}")),
            },
            Step::Reset => match c.exec("ryra reset --yes").await {
                Ok(_) => Outcome::Passed,
                Err(e) => Outcome::Failed(format!("{e:#}")),
            },
        };

        Event {
            description,
            kind: EventKind::Step,
            outcome,
            duration: t.elapsed(),
        }
    }

    async fn run_assertion(&self, c: &Machine, assertion: &Assertion) -> Event {
        let t = Instant::now();
        let description = assertion.describe();

        let result = match assertion {
            Assertion::Running { service } => {
                c.assert_service_active(service, &format!("{service}.service"))
                    .await
            }
            Assertion::NotRunning { service } => {
                c.assert_service_inactive(service, &format!("{service}.service"))
                    .await
            }
            Assertion::HttpOk { service, status } => {
                match self.get_service_port(c, service).await {
                    Ok(port) => {
                        c.assert_curl(&format!("http://127.0.0.1:{port}"), *status)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
            Assertion::UserExists { username } => c.assert_user_exists(username).await,
            Assertion::UserNotExists { username } => c.assert_user_not_exists(username).await,
            Assertion::FileExists { path } => c.assert_file_exists(path).await,
            Assertion::FileNotExists { path } => c.assert_file_not_exists(path).await,
            Assertion::ConfigContains { text } => match c.exec("cat /etc/ryra/ryra.toml").await {
                Ok(output) => {
                    if output.stdout_trimmed().contains(text) {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("config does not contain '{text}'"))
                    }
                }
                Err(e) => Err(e),
            },
            Assertion::ConfigNotContains { text } => {
                match c.exec("cat /etc/ryra/ryra.toml").await {
                    Ok(output) => {
                        if output.stdout_trimmed().contains(text) {
                            Err(anyhow::anyhow!("config unexpectedly contains '{text}'"))
                        } else {
                            Ok(())
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Assertion::JournalClean { service } => {
                c.assert_journal_clean(&format!("{service}.service")).await
            }
        };

        let outcome = match result {
            Ok(()) => Outcome::Passed,
            Err(e) => Outcome::Failed(format!("{e:#}")),
        };

        Event {
            description,
            kind: EventKind::Assertion,
            outcome,
            duration: t.elapsed(),
        }
    }

    async fn get_service_port(&self, c: &Machine, service: &str) -> Result<String> {
        let output = c
            .exec(&format!(
                "grep RYRA_PORT /var/lib/{service}/.env | head -1 | cut -d= -f2"
            ))
            .await?;
        let port = output.stdout_trimmed().to_string();
        if port.is_empty() {
            bail!("could not find RYRA_PORT for service {service}");
        }
        Ok(port)
    }

    // -----------------------------------------------------------------------
    // Introspection (for --list and unit tests)
    // -----------------------------------------------------------------------

    pub fn step_count(&self) -> usize {
        self.phases
            .iter()
            .filter(|p| matches!(p, Phase::Step(_)))
            .count()
    }

    pub fn assertion_count(&self) -> usize {
        self.phases
            .iter()
            .filter(|p| matches!(p, Phase::Assertion(_)))
            .count()
    }

    pub fn summary(&self) -> String {
        let services: Vec<&str> = self
            .phases
            .iter()
            .filter_map(|p| match p {
                Phase::Step(Step::Add { service }) => Some(*service),
                _ => None,
            })
            .collect();
        if services.is_empty() {
            self.name.to_string()
        } else {
            format!("{} ({})", self.name, services.join(" + "))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_empty_scenario() {
        let s = Scenario::new("empty");
        assert_eq!(s.name, "empty");
        assert_eq!(s.step_count(), 0);
        assert_eq!(s.assertion_count(), 0);
    }

    #[test]
    fn build_single_service_scenario() {
        let s = Scenario::new("whoami-only")
            .add("whoami")
            .assert_running("whoami")
            .assert_http("whoami", 200)
            .assert_journal_clean("whoami");

        assert_eq!(s.step_count(), 1);
        assert_eq!(s.assertion_count(), 3);
        assert_eq!(s.summary(), "whoami-only (whoami)");
    }

    #[test]
    fn build_multi_service_scenario() {
        let s = Scenario::new("multi")
            .add("whoami")
            .add("postgres")
            .assert_running("whoami")
            .assert_running("postgres");

        assert_eq!(s.step_count(), 2);
        assert_eq!(s.assertion_count(), 2);
        assert_eq!(s.summary(), "multi (whoami + postgres)");
    }

    #[test]
    fn phases_preserve_insertion_order() {
        // Steps and assertions interleave in the order they're added
        let s = Scenario::new("interleaved")
            .add("whoami")
            .assert_running("whoami")
            .add("postgres")
            .assert_running("postgres");

        assert_eq!(s.phases.len(), 4);
        assert!(matches!(
            s.phases[0],
            Phase::Step(Step::Add { service: "whoami" })
        ));
        assert!(matches!(
            s.phases[1],
            Phase::Assertion(Assertion::Running { service: "whoami" })
        ));
        assert!(matches!(
            s.phases[2],
            Phase::Step(Step::Add {
                service: "postgres"
            })
        ));
        assert!(matches!(
            s.phases[3],
            Phase::Assertion(Assertion::Running {
                service: "postgres"
            })
        ));
    }

    #[test]
    fn build_lifecycle_scenario() {
        let s = Scenario::new("lifecycle")
            .add("whoami")
            .assert_running("whoami")
            .remove("whoami")
            .assert_not_running("whoami")
            .assert_user_not_exists("ryra-whoami");

        assert_eq!(s.step_count(), 2);
        assert_eq!(s.assertion_count(), 3);
    }

    #[test]
    fn build_reset_scenario() {
        let s = Scenario::new("full-reset")
            .add("whoami")
            .reset()
            .assert_not_running("whoami")
            .assert_file_not_exists("/etc/ryra/ryra.toml");

        assert_eq!(s.step_count(), 2);
        assert_eq!(s.assertion_count(), 2);
    }

    #[test]
    fn custom_repo_override() {
        let s = Scenario::new("custom-repo").repo("/my/registry");
        assert_eq!(s.repo, "/my/registry");
    }

    #[test]
    fn summary_with_no_services() {
        let s = Scenario::new("just-init");
        assert_eq!(s.summary(), "just-init");
    }

    #[test]
    fn scenario_result_display_pass() {
        let result = ScenarioResult {
            name: "test".to_string(),
            events: vec![
                Event {
                    description: "ryra init".to_string(),
                    kind: EventKind::Init,
                    outcome: Outcome::Passed,
                    duration: Duration::from_millis(150),
                },
                Event {
                    description: "ryra add whoami".to_string(),
                    kind: EventKind::Step,
                    outcome: Outcome::Passed,
                    duration: Duration::from_secs(3),
                },
            ],
            duration: Duration::from_secs(4),
            outcome: Outcome::Passed,
        };
        let output = format!("{result}");
        assert!(output.contains("PASS"));
        assert!(output.contains("test"));
        assert!(output.contains("ryra init"));
        assert!(output.contains("ryra add whoami"));
    }

    #[test]
    fn scenario_result_display_fail() {
        let result = ScenarioResult {
            name: "broken".to_string(),
            events: vec![
                Event {
                    description: "ryra init".to_string(),
                    kind: EventKind::Init,
                    outcome: Outcome::Passed,
                    duration: Duration::from_millis(100),
                },
                Event {
                    description: "whoami is running".to_string(),
                    kind: EventKind::Assertion,
                    outcome: Outcome::Failed("service not active".to_string()),
                    duration: Duration::from_millis(50),
                },
                Event {
                    description: "whoami returns HTTP 200".to_string(),
                    kind: EventKind::Assertion,
                    outcome: Outcome::Skipped,
                    duration: Duration::ZERO,
                },
            ],
            duration: Duration::from_secs(1),
            outcome: Outcome::Failed("service not active".to_string()),
        };
        let output = format!("{result}");
        assert!(output.contains("FAIL"));
        assert!(output.contains("service not active"));
        assert!(output.contains("skip"));
    }

    #[test]
    fn outcome_predicates() {
        assert!(Outcome::Passed.is_pass());
        assert!(!Outcome::Passed.is_fail());
        assert!(Outcome::Failed("x".into()).is_fail());
        assert!(!Outcome::Skipped.is_pass());
    }
}
