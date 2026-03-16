use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const CF_API: &str = "https://api.cloudflare.com/client/v4";

/// Get the account ID by looking it up from the zone details.
/// This avoids needing Account-level API permissions.
pub async fn get_account_id(api_token: &str, zone_id: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{CF_API}/zones/{zone_id}"))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to get zone: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!("API error: {errors}")));
    }

    let id = body["result"]["account"]["id"]
        .as_str()
        .ok_or_else(|| Error::Cloudflare("no account found in zone data".into()))?
        .to_string();

    Ok(id)
}

/// A Cloudflare Tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
}

/// An ingress rule for a tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressRule {
    pub hostname: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// List tunnels for this account.
pub async fn list_tunnels(api_token: &str, account_id: &str) -> Result<Vec<Tunnel>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "{CF_API}/accounts/{account_id}/cfd_tunnel?is_deleted=false"
        ))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to list tunnels: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!("API error: {errors}")));
    }

    let tunnels: Vec<Tunnel> = serde_json::from_value(body["result"].clone())
        .map_err(|e| Error::Cloudflare(format!("failed to parse tunnels: {e}")))?;

    Ok(tunnels)
}

/// Get the current tunnel configuration (ingress rules).
pub async fn get_tunnel_config(
    api_token: &str,
    account_id: &str,
    tunnel_id: &str,
) -> Result<Vec<IngressRule>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "{CF_API}/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"
        ))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to get tunnel config: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!("API error: {errors}")));
    }

    // Extract ingress rules from the config
    let ingress = body["result"]["config"]["ingress"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let rules: Vec<IngressRule> = ingress
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        // Filter out the catch-all rule (no hostname)
        .filter(|r: &IngressRule| !r.hostname.is_empty())
        .collect();

    Ok(rules)
}

/// Update the tunnel configuration with new ingress rules.
/// Always appends a catch-all 404 rule at the end.
pub async fn update_tunnel_config(
    api_token: &str,
    account_id: &str,
    tunnel_id: &str,
    rules: &[IngressRule],
) -> Result<()> {
    let mut ingress: Vec<serde_json::Value> = rules
        .iter()
        .map(|r| {
            serde_json::json!({
                "hostname": r.hostname,
                "service": r.service,
            })
        })
        .collect();

    // Catch-all rule must be last
    ingress.push(serde_json::json!({
        "service": "http_status:404"
    }));

    let client = reqwest::Client::new();
    let resp = client
        .put(format!(
            "{CF_API}/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"
        ))
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "config": {
                "ingress": ingress
            }
        }))
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to update tunnel config: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to update tunnel config: {errors}"
        )));
    }

    Ok(())
}

/// Create a CNAME DNS record pointing a hostname to a tunnel.
pub async fn create_tunnel_dns(
    api_token: &str,
    zone_id: &str,
    hostname: &str,
    tunnel_id: &str,
) -> Result<()> {
    let target = format!("{tunnel_id}.cfargotunnel.com");

    // Check for existing record first
    let existing = crate::integrations::dns::find_record(api_token, zone_id, hostname).await;
    if let Ok(Some(record)) = existing {
        // Delete existing record
        crate::integrations::dns::delete_record(api_token, zone_id, &record.id).await?;
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{CF_API}/zones/{zone_id}/dns_records"
        ))
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "type": "CNAME",
            "name": hostname,
            "content": target,
            "proxied": true,
        }))
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to create CNAME: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to create tunnel CNAME: {errors}"
        )));
    }

    Ok(())
}
