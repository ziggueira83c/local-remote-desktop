//! Senha de acesso: guardada só como hash Argon2id com sal aleatório (nunca em texto puro).
//! Formato: `argon2id$<sal base64>$<hash base64>`.

use base64::engine::general_purpose::STANDARD_NO_PAD as B64;
use base64::Engine;

fn derive(password: &str, salt: &[u8]) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    argon2::Argon2::default().hash_password_into(password.as_bytes(), salt, &mut out).ok()?;
    Some(out)
}

/// Gera o hash para armazenar.
pub fn hash_password(password: &str) -> Option<String> {
    let mut salt = [0u8; 16];
    salt.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let hash = derive(password, &salt)?;
    Some(format!("argon2id${}${}", B64.encode(salt), B64.encode(hash)))
}

/// Confere a senha contra o hash armazenado (comparação em tempo constante).
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    if parts.next() != Some("argon2id") {
        return false;
    }
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else { return false };
    let (Ok(salt), Ok(expected)) = (B64.decode(salt), B64.decode(hash)) else { return false };
    let Some(actual) = derive(password, &salt) else { return false };
    if expected.len() != actual.len() {
        return false;
    }
    expected.iter().zip(actual.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify() {
        let h = hash_password("segredo123").unwrap();
        assert!(verify_password("segredo123", &h));
        assert!(!verify_password("errada", &h));
        assert_ne!(h, hash_password("segredo123").unwrap()); // sal diferente
    }
}
