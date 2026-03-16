use crate::error::{Error, Result};

/// Reload the nginx container via systemctl (runs as root).
pub async fn reload() -> Result<()> {
    let output = tokio::process::Command::new("sudo")
        .args(["systemctl", "reload", "nginx"])
        .output()
        .await
        .map_err(|e| Error::NginxReload(format!("failed to run sudo systemctl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::NginxReload(format!(
            "nginx reload failed: {stderr}"
        )));
    }
    Ok(())
}

/// Start the nginx service (root quadlet).
pub async fn start() -> Result<()> {
    let output = tokio::process::Command::new("sudo")
        .args(["systemctl", "start", "nginx"])
        .output()
        .await
        .map_err(|e| Error::NginxReload(format!("failed to start nginx: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::NginxReload(format!(
            "nginx start failed: {stderr}"
        )));
    }
    Ok(())
}

/// Daemon-reload for the system (root) systemd.
pub async fn daemon_reload_system() -> Result<()> {
    let output = tokio::process::Command::new("sudo")
        .args(["systemctl", "daemon-reload"])
        .output()
        .await
        .map_err(|e| Error::NginxReload(format!("failed to daemon-reload: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::NginxReload(format!(
            "daemon-reload failed: {stderr}"
        )));
    }
    Ok(())
}
