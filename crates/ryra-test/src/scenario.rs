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

        // Show top-level failure reason when there are no events (setup failure)
        if self.events.is_empty() {
            if let Outcome::Failed(msg) = &self.outcome {
                writeln!(f, "  {msg}")?;
            }
        }

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

