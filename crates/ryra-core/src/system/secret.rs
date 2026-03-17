use rand::Rng;

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
