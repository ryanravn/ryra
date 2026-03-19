use std::fmt;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Result types — full trace of what happened in each test
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
