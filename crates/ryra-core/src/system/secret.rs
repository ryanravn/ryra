use rand::Rng;

use crate::registry::service_def::EnvFormat;

const ALPHANUMERIC: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const HEX: &[u8] = b"0123456789abcdef";

/// Default lengths per format. UUID has no configurable length.
fn default_length(format: &EnvFormat) -> Option<usize> {
    match format {
        EnvFormat::String => Some(32),
        EnvFormat::Hex => Some(64),
        EnvFormat::Uuid => None,
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
    }
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
