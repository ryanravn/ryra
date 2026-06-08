use anyhow::{Result, bail};
use ryra_core::Lifecycle;

use super::apply;

/// `ryra start` / `ryra stop`. Drives one service (and its sidecars) or,
/// with `--all`, every installed service through a start/stop transition.
///
/// `service` and `all` are mutually exclusive and exactly one is required
/// — clap enforces this at the arg layer, so reaching here with neither is
/// a defensive `bail!`.
pub async fn run(service: Option<&str>, all: bool, action: Lifecycle, dry_run: bool) -> Result<()> {
    let targets: Vec<String> = if all {
        let installed = ryra_core::list_installed()?;
        if installed.is_empty() {
            println!("No services installed.");
            return Ok(());
        }
        installed.into_iter().map(|s| s.name).collect()
    } else {
        match service {
            Some(s) => vec![s.to_string()],
            None => bail!("specify a service name or --all"),
        }
    };

    // Build the full step list up front so an unknown service name (e.g. a
    // typo, or `--all` racing a concurrent remove) fails before we start
    // toggling anything.
    let mut steps = Vec::new();
    for name in &targets {
        steps.extend(ryra_core::lifecycle_steps(name, action)?);
    }

    if dry_run {
        super::print_dry_run(&steps);
        return Ok(());
    }

    let verb = match action {
        Lifecycle::Start => "Starting",
        Lifecycle::Stop => "Stopping",
    };
    println!("{verb} {}...", targets.join(", "));
    apply::execute_all(&steps).await?;
    Ok(())
}
