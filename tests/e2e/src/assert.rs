use anyhow::{Result, bail};

use crate::machine::Machine;

impl Machine {
    pub async fn assert_service_active(&self, user: &str, unit: &str) -> Result<()> {
        let cmd = format!("systemctl --machine=ryra-{user}@ --user is-active {unit}");
        let output = self.exec(&cmd).await?;
        let status = output.stdout_trimmed();
        if status != "active" {
            bail!("expected service {unit} for user ryra-{user} to be active, got: {status}");
        }
        Ok(())
    }

    pub async fn assert_service_inactive(&self, user: &str, unit: &str) -> Result<()> {
        let cmd = format!("systemctl --machine=ryra-{user}@ --user is-active {unit}");
        let output = self
            .exec(&format!("{cmd} 2>/dev/null || echo inactive"))
            .await?;
        let status = output.stdout_trimmed();
        if status == "active" {
            bail!("expected service {unit} for user ryra-{user} to be inactive, but it is active");
        }
        Ok(())
    }

    pub async fn assert_curl(&self, url: &str, expected_status: u16) -> Result<()> {
        let cmd = format!("curl -s -o /dev/null -w '%{{http_code}}' {url}");
        let output = self.exec(&cmd).await?;
        let code: u16 = output.stdout_trimmed().parse().map_err(|e| {
            anyhow::anyhow!(
                "failed to parse HTTP status from curl output '{}': {e}",
                output.stdout_trimmed()
            )
        })?;
        if code != expected_status {
            bail!("expected HTTP {expected_status} from {url}, got {code}");
        }
        Ok(())
    }

    pub async fn assert_journal_clean(&self, unit: &str) -> Result<()> {
        let cmd = format!("journalctl _SYSTEMD_USER_UNIT={unit} -p err -q --no-pager");
        let output = self.exec(&cmd).await?;
        let errors = output.stdout_trimmed();
        if !errors.is_empty() {
            bail!("found error-level journal entries for {unit}:\n{errors}");
        }
        Ok(())
    }

    pub async fn assert_user_exists(&self, username: &str) -> Result<()> {
        self.exec(&format!("id {username}")).await?;
        Ok(())
    }

    pub async fn assert_user_not_exists(&self, username: &str) -> Result<()> {
        let result = self
            .exec(&format!(
                "id {username} 2>/dev/null && echo exists || echo missing"
            ))
            .await?;
        if result.stdout_trimmed().contains("exists") {
            bail!("expected user {username} to not exist, but it does");
        }
        Ok(())
    }

    pub async fn assert_file_exists(&self, path: &str) -> Result<()> {
        self.exec(&format!("test -e {path}")).await?;
        Ok(())
    }

    pub async fn assert_file_not_exists(&self, path: &str) -> Result<()> {
        let result = self
            .exec(&format!("test -e {path} && echo exists || echo missing"))
            .await?;
        if result.stdout_trimmed().contains("exists") {
            bail!("expected {path} to not exist, but it does");
        }
        Ok(())
    }

    pub async fn wait_for_service(
        &self,
        user: &str,
        unit: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            let cmd = format!(
                "systemctl --machine=ryra-{user}@ --user is-active {unit} 2>/dev/null || echo inactive"
            );
            if let Ok(output) = self.exec(&cmd).await {
                if output.stdout_trimmed() == "active" {
                    return Ok(());
                }
            }

            if start.elapsed() > timeout {
                bail!(
                    "timed out waiting for {unit} (user ryra-{user}) to become active after {}s",
                    timeout.as_secs()
                );
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}
