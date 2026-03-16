use rand::Rng;

use crate::config::state::{SecretEntry, State};

const SECRET_LENGTH: usize = 32;
const SECRET_CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Generate a random secret string.
pub fn generate_secret() -> String {
    let mut rng = rand::rng();
    (0..SECRET_LENGTH)
        .map(|_| {
            let idx = rng.random_range(0..SECRET_CHARSET.len());
            SECRET_CHARSET[idx] as char
        })
        .collect()
}

/// Get or create a secret for a service, storing it in state.
pub fn ensure_secret(state: &mut State, service: &str, name: &str) -> String {
    if let Some(existing) = state
        .secrets
        .iter()
        .find(|s| s.service == service && s.name == name)
    {
        return existing.value.clone();
    }

    let value = generate_secret();
    state.secrets.push(SecretEntry {
        service: service.to_string(),
        name: name.to_string(),
        value: value.clone(),
    });
    value
}

/// Remove all secrets for a service.
pub fn remove_secrets(state: &mut State, service: &str) {
    state.secrets.retain(|s| s.service != service);
}
