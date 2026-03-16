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

/// A newly created tunnel with its connector token.
pub struct CreatedTunnel {
    pub id: String,
    pub name: String,
    pub token: String,
}

/// Create a new Cloudflare Tunnel.
pub async fn create_tunnel(
    api_token: &str,
    account_id: &str,
    name: &str,
) -> Result<CreatedTunnel> {
    let client = reqwest::Client::new();

    // Create the tunnel
    let resp = client
        .post(format!(
            "{CF_API}/accounts/{account_id}/cfd_tunnel"
        ))
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "name": name,
            "tunnel_secret": base64_random_secret(),
            "config_src": "cloudflare",
        }))
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to create tunnel: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to create tunnel: {errors}"
        )));
    }

    let id = body["result"]["id"]
        .as_str()
        .ok_or_else(|| Error::Cloudflare("no tunnel id in response".into()))?
        .to_string();

    let tunnel_name = body["result"]["name"]
        .as_str()
        .unwrap_or(name)
        .to_string();

    // Get the connector token
    let token = get_tunnel_token(api_token, account_id, &id).await?;

    Ok(CreatedTunnel {
        id,
        name: tunnel_name,
        token,
    })
}

/// Get the connector install token for a tunnel.
pub async fn get_tunnel_token(
    api_token: &str,
    account_id: &str,
    tunnel_id: &str,
) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "{CF_API}/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token"
        ))
        .bearer_auth(api_token)
        .send()
        .await
        .map_err(|e| Error::Cloudflare(format!("failed to get tunnel token: {e}")))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| Error::Cloudflare(format!("invalid response: {e}")))?;

    if !body["success"].as_bool().unwrap_or(false) {
        let errors = &body["errors"];
        return Err(Error::Cloudflare(format!(
            "failed to get tunnel token: {errors}"
        )));
    }

    body["result"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Cloudflare("no token in response".into()))
}

/// Generate a random 32-byte base64-encoded secret for tunnel creation.
/// Uses a simple encoding since this is just an initial handshake secret.
fn base64_random_secret() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.random::<u8>()).collect();
    // Simple base64 without pulling in the base64 crate
    data_encoding_base64(&bytes)
}

fn data_encoding_base64(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Create a CNAME DNS record pointing a hostname to a tunnel.
pub async fn create_tunnel_dns(
    api_token: &str,
    zone_id: &str,
    hostname: &str,
    tunnel_id: &str,
) -> Result<()> {
    let target = format!("{tunnel_id}.cfargotunnel.com");

    // Delete all existing records for this hostname (A, AAAA, CNAME, etc.)
    let _ = crate::integrations::dns::delete_all_records(api_token, zone_id, hostname).await;

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
