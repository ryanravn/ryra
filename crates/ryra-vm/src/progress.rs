//! Standardized heartbeat for poll-until-ready loops.
//!
//! Every "wait for X to become ready" loop in ryra has the same shape: probe a
//! condition on a fixed interval, bounded by a timeout, and bail if the timeout
//! trips. Left silent, a slow wait reads as a hang. [`WaitProgress`] gives all
//! of them one consistent line — *what* is being probed, *how many* checks have
//! run, and the *limit* — printed on a throttled cadence so fast waits stay
//! quiet while slow ones always show life.

use std::time::{Duration, Instant};

/// Tracks one poll-until-ready loop: owns the clock, counts probes, and emits a
/// throttled heartbeat line.
///
/// Construct it just before the loop, then per iteration use [`timed_out`] for
/// the bound and [`tick`] to count the probe and print a heartbeat when due:
///
/// ```ignore
/// let mut progress = WaitProgress::new("vikunja", "systemctl + healthcheck", timeout)
///     .with_prefix(format!("[{test_name}]     "));
/// loop {
///     if ready { return Ok(()); }
///     if failed { bail!("..."); }
///     if progress.timed_out() { bail!("not ready after {}s", timeout.as_secs()); }
///     progress.tick();
///     tokio::time::sleep(interval).await;
/// }
/// ```
///
/// [`timed_out`]: WaitProgress::timed_out
/// [`tick`]: WaitProgress::tick
pub struct WaitProgress {
    /// What we're waiting for, e.g. a service name.
    label: String,
    /// How readiness is probed, e.g. "systemctl is-active" — the "how it
    /// checks" the heartbeat reports.
    method: String,
    /// Line prefix so the heartbeat lines up with surrounding output.
    prefix: String,
    /// Bound on the loop.
    timeout: Duration,
    /// Minimum gap between heartbeat lines.
    heartbeat: Duration,
    /// When the loop started — the single clock for `elapsed`/`timed_out`.
    start: Instant,
    /// When the last heartbeat printed (starts at `start`, so the first line
    /// waits a full `heartbeat` — fast waits print nothing).
    last_logged: Instant,
    /// How many probes have run.
    checks: u32,
}

impl WaitProgress {
    /// `label` is what we're waiting for; `method` describes how readiness is
    /// probed; the loop is bounded by `timeout`. Defaults to a two-space prefix
    /// and a 5s heartbeat cadence — override with [`with_prefix`] /
    /// [`with_heartbeat`].
    ///
    /// [`with_prefix`]: WaitProgress::with_prefix
    /// [`with_heartbeat`]: WaitProgress::with_heartbeat
    pub fn new(label: impl Into<String>, method: impl Into<String>, timeout: Duration) -> Self {
        let start = Instant::now();
        Self {
            label: label.into(),
            method: method.into(),
            prefix: "  ".to_string(),
            timeout,
            heartbeat: Duration::from_secs(5),
            start,
            last_logged: start,
            checks: 0,
        }
    }

    /// Set the line prefix so the heartbeat aligns with the surrounding output
    /// (e.g. `"[my-test]     "`). Defaults to two spaces.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Override how often a heartbeat prints. Defaults to 5s — right for the
    /// short service/port waits; long VM boot/SSH waits set this to 30s so they
    /// don't flood the log.
    pub fn with_heartbeat(mut self, every: Duration) -> Self {
        self.heartbeat = every;
        self
    }

    /// Time since the loop started.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Whether the loop has run past its timeout.
    pub fn timed_out(&self) -> bool {
        self.start.elapsed() > self.timeout
    }

    /// Record one probe attempt and print a heartbeat line if the cadence is
    /// due. Call once per loop iteration.
    pub fn tick(&mut self) {
        self.checks += 1;
        if self.last_logged.elapsed() >= self.heartbeat {
            println!(
                "{}still waiting for {}... via {} (check {}, limit {}s)",
                self.prefix,
                self.label,
                self.method,
                self.checks,
                self.timeout.as_secs(),
            );
            self.last_logged = Instant::now();
        }
    }
}
