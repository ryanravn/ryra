use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;

use crate::registry::service_def::EnvFormat;

type HmacSha256 = Hmac<Sha256>;

const ALPHANUMERIC: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const HEX: &[u8] = b"0123456789abcdef";

/// Default lengths per format. UUID has no configurable length.
fn default_length(format: &EnvFormat) -> Option<usize> {
    match format {
        EnvFormat::String => Some(32),
        EnvFormat::Hex => Some(64),
        EnvFormat::Uuid | EnvFormat::JwtHs256 => None,
    }
}

/// Generate a random secret string using the default format (32-char alphanumeric).
pub fn generate_secret() -> String {
    generate(&EnvFormat::String, None)
}

/// Generate a random secret with the given format and optional length override.
pub fn generate(format: &EnvFormat, length: Option<u32>) -> String {
    match format {
        EnvFormat::String => {
            let default = default_length(format).unwrap_or(32);
            let len = length.map(|l| l as usize).unwrap_or(default);
            random_string(ALPHANUMERIC, len)
        }
        EnvFormat::Hex => {
            let default = default_length(format).unwrap_or(64);
            let len = length.map(|l| l as usize).unwrap_or(default);
            random_string(HEX, len)
        }
        EnvFormat::Uuid => {
            let mut rng = rand::rng();
            let bytes: [u8; 16] = rng.random();
            // Set version 4 and variant bits
            let mut b = bytes;
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0],
                b[1],
                b[2],
                b[3],
                b[4],
                b[5],
                b[6],
                b[7],
                b[8],
                b[9],
                b[10],
                b[11],
                b[12],
                b[13],
                b[14],
                b[15]
            )
        }
        // JWT secrets are generated via generate_jwt_hs256, not this function.
        EnvFormat::JwtHs256 => String::new(),
    }
}

/// Generate an HS256-signed JWT with the given claims, signed by `signing_key`.
/// Adds `iat` (now) and `exp` (now + 5 years) if not already present in claims.
pub fn generate_jwt_hs256(
    signing_key: &str,
    claims: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = r#"{"alg":"HS256","typ":"JWT"}"#;
    let header_b64 = b64.encode(header.as_bytes());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut payload_claims = claims.clone();
    payload_claims
        .entry("iat".to_string())
        .or_insert(serde_json::Value::Number(now.into()));
    payload_claims
        .entry("exp".to_string())
        .or_insert(serde_json::Value::Number((now + 157_680_000).into())); // 5 years

    let payload_json = serde_json::to_string(&payload_claims).unwrap_or_default();
    let payload_b64 = b64.encode(payload_json.as_bytes());

    let message = format!("{header_b64}.{payload_b64}");

    let mut mac = HmacSha256::new_from_slice(signing_key.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
    mac.update(message.as_bytes());
    let sig = b64.encode(mac.finalize().into_bytes());

    format!("{message}.{sig}")
}

fn random_string(charset: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..charset.len());
            charset[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_secret_is_32_alphanumeric() {
        let s = generate_secret();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn hex_default_is_64() {
        let s = generate(&EnvFormat::Hex, None);
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hex_custom_length() {
        let s = generate(&EnvFormat::Hex, Some(16));
        assert_eq!(s.len(), 16);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn string_custom_length() {
        let s = generate(&EnvFormat::String, Some(48));
        assert_eq!(s.len(), 48);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn uuid_format() {
        let s = generate(&EnvFormat::Uuid, None);
        assert_eq!(s.len(), 36); // 8-4-4-4-12
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
        // Version 4 bit
        assert_eq!(s.as_bytes()[14], b'4');
    }
}
