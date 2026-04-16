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
    /// Captured stdout from the step. Empty on failure (the error message
    /// already embeds it) and for non-command events (waits, assertions).
    pub stdout: String,
    /// Captured stderr from the step. Same rules as stdout.
    pub stderr: String,
}

impl Event {
    /// Event with no captured output — for wait/service-status checks that
    /// don't run a shell command.
    pub fn bare(description: String, kind: EventKind, outcome: Outcome, duration: Duration) -> Self {
        Self {
            description,
            kind,
            outcome,
            duration,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
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
        if self.events.is_empty()
            && let Outcome::Failed(msg) = &self.outcome
        {
            writeln!(f, "  {msg}")?;
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

            // Emit captured stdout/stderr verbatim, indented. On failure the
            // error message already embeds them, so we only render here when
            // the step passed.
            if matches!(event.outcome, Outcome::Passed) {
                render_output(f, "stdout", &event.stdout)?;
                render_output(f, "stderr", &event.stderr)?;
            }
        }

        Ok(())
    }
}

fn render_output(f: &mut fmt::Formatter<'_>, label: &str, text: &str) -> fmt::Result {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Ok(());
    }
    writeln!(f, "         [{label}]")?;
    for line in trimmed.lines() {
        writeln!(f, "         | {line}")?;
    }
    Ok(())
}
