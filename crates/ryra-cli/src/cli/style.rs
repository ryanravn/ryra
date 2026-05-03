//! Tiny color palette for CLI output. Centralized so adding a new service
//! status (or a new prefix) forces a decision about its color, instead of
//! drifting into "some lines colored, some not."
//!
//! Sticks to the 16-color ANSI palette — works on every terminal, no
//! terminfo lookups, and `console::style` auto-disables when stdout isn't
//! a TTY or when `NO_COLOR` is set.

use console::{StyledObject, style};

use ryra_core::data::ServiceStatus;

/// `ryra list` status word. The label is padded to `width` *before* coloring
/// because ANSI escape codes are zero-width visually but inflate the byte
/// length, breaking `{:<N}` alignment in `println!` callers.
pub fn list_status(status: &ServiceStatus, active: bool, width: usize) -> String {
    let (raw, styled): (&str, StyledObject<String>) = match (status, active) {
        (ServiceStatus::Installed, true) => {
            let p = format!("{:<width$}", "running");
            ("running", style(p).green())
        }
        (ServiceStatus::Installed, false) => {
            let p = format!("{:<width$}", "stopped");
            ("stopped", style(p).yellow())
        }
        (ServiceStatus::Orphan, _) => {
            let p = format!("{:<width$}", "removed");
            ("removed", style(p).dim())
        }
    };
    let _ = raw;
    styled.to_string()
}

/// `→` prefix for `print_plan_header`. Yellow is the closest 16-color
/// match to ryra's cabin-orange brand.
pub fn arrow() -> StyledObject<&'static str> {
    style("→").yellow()
}

/// `WARNING:` prefix — actionable, the user is expected to read it.
pub fn warning() -> StyledObject<&'static str> {
    style("WARNING:").yellow().bold()
}

/// `NOTE:` prefix — informational, slightly de-emphasized.
pub fn note() -> StyledObject<&'static str> {
    style("NOTE:").cyan()
}

/// `Error:` and similar fatal prefixes, on stderr.
pub fn error_prefix(s: &'static str) -> StyledObject<&'static str> {
    style(s).red().bold()
}

/// Colorize a single SUPPORTS chip (`oidc`, `smtp`, …) in `ryra search`.
/// Does not pad — the caller is responsible for column width since it
/// joins multiple chips with `", "`.
pub fn support_chip(s: &str) -> String {
    style(s.to_string()).cyan().to_string()
}
