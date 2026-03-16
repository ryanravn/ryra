use crate::error::{Error, Result};

/// Run `systemctl --user daemon-reload`.
pub async fn daemon_reload() -> Result<()> {
    run_systemctl(&["--user", "daemon-reload"]).await
}

/// Start a user service by name.
pub async fn start_service(name: &str) -> Result<()> {
    run_systemctl(&["--user", "start", name]).await
}

/// Stop a user service by name.
pub async fn stop_service(name: &str) -> Result<()> {
    run_systemctl(&["--user", "stop", name]).await
}

/// Check if a user service is active.
pub async fn is_active(name: &str) -> Result<bool> {
    let output = tokio::process::Command::new("systemctl")
        .args(["--user", "is-active", name])
        .output()
        .await
        .map_err(|e| Error::Systemctl(format!("failed to run systemctl: {e}")))?;

    Ok(output.status.success())
}

async fn run_systemctl(args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("systemctl")
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Systemctl(format!("failed to run systemctl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Systemctl(format!(
            "systemctl {} failed: {stderr}",
            args.join(" ")
        )));
    }
    Ok(())
}
