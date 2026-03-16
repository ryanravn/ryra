use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const CF_API: &str = "https://api.cloudflare.com/client/v4";

/// A Cloudflare zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

/// A Cloudflare DNS record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub record_type: String,
    pub content: String,
}

// --- State machine for DNS record creation ---

/// States for the DNS record creation flow.
pub enum CreateRecordState {
    /// Initial: need to check for existing records.
    CheckExisting,
    /// Found an existing record — caller must decide.
    ConflictFound { existing: DnsRecord },
    /// Existing record needs to be deleted before creating.
    DeleteExisting { record_id: String },
    /// Ready to resolve public IP and create the record.
    Create,
    /// Done.
    Done { ip: String },
}

/// Actions the caller must take to advance the state machine.
pub enum CreateRecordAction {
    /// Caller must decide: overwrite or abort?
    ResolveConflict { existing_ip: String },
    /// Terminal: record was created.
    Created { ip: String },
}

/// Drive the DNS record creation state machine.
pub struct CreateRecordMachine {
    api_token: String,
    zone_id: String,
    domain: String,
    proxied: bool,
    state: CreateRecordState,
}

impl CreateRecordMachine {
    pub fn new(api_token: String, zone_id: String, domain: String, proxied: bool) -> Self {
        Self {
            api_token,
            zone_id,
            domain,
            proxied,
            state: CreateRecordState::CheckExisting,
        }
    }

    /// Advance the state machine. Returns an action when caller input is needed,
    /// or None when it can keep advancing internally.
    pub async fn advance(&mut self) -> Result<Option<CreateRecordAction>> {
        loop {
            match &self.state {
                CreateRecordState::CheckExisting => {
                    let existing = find_record(&self.api_token, &self.zone_id, &self.domain).await;
                    match existing {
                        Ok(Some(record)) => {
                            let ip = record.content.clone();
                            self.state = CreateRecordState::ConflictFound { existing: record };
                            return Ok(Some(CreateRecordAction::ResolveConflict {
                                existing_ip: ip,
                            }));
                        }
                        Ok(None) => {
                            self.state = CreateRecordState::Create;
                        }
                        Err(_) => {
                            // Can't check — proceed anyway
                            self.state = CreateRecordState::Create;
                        }
                    }
                }
                CreateRecordState::ConflictFound { existing } => {
                    // Caller hasn't resolved yet — shouldn't be called again without resolving
                    let id = existing.id.clone();
                    self.state = CreateRecordState::DeleteExisting { record_id: id };
                }
                CreateRecordState::DeleteExisting { record_id } => {
                    delete_record(&self.api_token, &self.zone_id, record_id).await?;
                    self.state = CreateRecordState::Create;
                }
                CreateRecordState::Create => {
                    let ip = get_public_ip().await?;
                    create_a_record(
                        &self.api_token,
                        &self.zone_id,
                        &self.domain,
                        &ip,
                        self.proxied,
                    )
                    .await?;
                    self.state = CreateRecordState::Done { ip: ip.clone() };
                    return Ok(Some(CreateRecordAction::Created { ip }));
                }
                CreateRecordState::Done { .. } => {
                    return Ok(None);
                }
            }
        }
    }

    /// Tell the state machine to overwrite the conflicting record.
    pub fn confirm_overwrite(&mut self) {
        if let CreateRecordState::ConflictFound { existing } = &self.state {
            let id = existing.id.clone();
            self.state = CreateRecordState::DeleteExisting { record_id: id };
        }
    }

    /// Abort — don't create anything.
    pub fn abort(&self) -> Result<()> {
        Err(Error::Cloudflare("aborted by user".to_string()))
    }
}

// --- Raw API functions ---

/// Verify the API token is valid.
pub async fn verify_token(api_token: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{CF_API}/user/tokens/verify"))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to verify token: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!("invalid API token: {errors}")));
    }

    let status = body["result"]["status"].as_str().unwrap_or("unknown");
    if status != "active" {
        return Err(Error::Cloudflare(format!(
            "API token is not active (status: {status})"
        )));
    }

    Ok(())
}

/// List all zones accessible with this API token.
pub async fn list_zones(api_token: &str) -> Result<Vec<Zone>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{CF_API}/zones?per_page=50"))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to list zones: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!("API error: {errors}")));
    }

    let zones: Vec<Zone> = serde_json::from_value(body["result"].clone())
        .map_err(|e| Error::Cloudflare(format!("failed to parse zones: {e}")))?;

    Ok(zones)
}

/// Create an A record for a domain.
pub async fn create_a_record(
    api_token: &str,
    zone_id: &str,
    domain: &str,
    ip: &str,
    proxied: bool,
) -> Result<DnsRecord> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{CF_API}/zones/{zone_id}/dns_records"))
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "type": "A",
            "name": domain,
            "content": ip,
            "ttl": 1,
            "proxied": proxied,
        }))
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to create record: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to create DNS record: {errors}"
        )));
    }

    let record: DnsRecord = serde_json::from_value(body["result"].clone())
        .map_err(|e| Error::Cloudflare(format!("failed to parse record: {e}")))?;

    Ok(record)
}

/// Delete a DNS record by ID.
pub async fn delete_record(api_token: &str, zone_id: &str, record_id: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!(
            "{CF_API}/zones/{zone_id}/dns_records/{record_id}"
        ))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to delete record: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to delete record: {errors}"
        )));
    }

    Ok(())
}

/// Find an A record by domain name.
pub async fn find_record(
    api_token: &str,
    zone_id: &str,
    domain: &str,
) -> Result<Option<DnsRecord>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "{CF_API}/zones/{zone_id}/dns_records?type=A&name={domain}"
        ))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to find record: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to find record: {errors}"
        )));
    }

    let records: Vec<DnsRecord> = serde_json::from_value(body["result"].clone())
        .map_err(|e| Error::Cloudflare(format!("failed to parse records: {e}")))?;

    Ok(records.into_iter().next())
}

/// Get this server's public IP.
pub async fn get_public_ip() -> Result<String> {
    let resp = reqwest::get("https://api.ipify.org")
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to get public IP: {e}")))?;

    let ip = resp
        .text()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid IP response: {e}")))?;

    Ok(ip.trim().to_string())
}
