//! Identidade persistente do dispositivo.
//!
//! - `device_id`: UUID persistente (arquivo `identity.json`, público).
//! - Certificado autoassinado (público) salvo em `identity.json`.
//! - Chave privada (PKCS#8):
//!   - Windows: Credential Manager (protegido pela conta do usuário, sem perguntas).
//!   - macOS: arquivo `private_key.b64` na pasta do app, legível só pelo usuário (0600).
//!     O Keychain amarra cada item à assinatura do app; como o app não tem assinatura da
//!     Apple, cada versão nova virava "outro app" e o macOS pedia a senha de novo.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const KEYRING_SERVICE: &str = "dev.lrd.localremotedesktop";

#[derive(Clone)]
pub struct Identity {
    pub device_id: String,
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    /// SHA-256 do certificado em hexadecimal minúsculo (64 caracteres).
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nunca imprimir a chave privada.
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    device_id: String,
    cert_der_b64: String,
}

impl Identity {
    /// Carrega a identidade existente ou cria uma nova.
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir).with_context(|| format!("criando {}", data_dir.display()))?;
        let id_path = data_dir.join("identity.json");

        if id_path.exists() {
            let stored: StoredIdentity = serde_json::from_slice(&fs::read(&id_path)?)
                .context("identity.json inválido")?;
            let cert_der = B64.decode(&stored.cert_der_b64).context("certificado inválido")?;
            match load_private_key(data_dir, &stored.device_id) {
                Ok(key_der) => {
                    let fingerprint = fingerprint_of(&cert_der);
                    return Ok(Identity { device_id: stored.device_id, cert_der, key_der, fingerprint });
                }
                Err(e) => {
                    tracing::warn!("chave privada não encontrada ({e:#}); gerando nova identidade");
                }
            }
        }

        let device_id = uuid::Uuid::new_v4().to_string();
        let key_pair = rcgen::KeyPair::generate().context("gerando par de chaves")?;
        let mut params = rcgen::CertificateParams::new(vec!["lrd.local".to_string()])
            .context("parâmetros do certificado")?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("LRD {device_id}"));
        let cert = params.self_signed(&key_pair).context("autoassinando certificado")?;
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();

        store_private_key(data_dir, &device_id, &key_der)?;
        let stored = StoredIdentity { device_id: device_id.clone(), cert_der_b64: B64.encode(&cert_der) };
        fs::write(&id_path, serde_json::to_vec_pretty(&stored)?)?;

        let fingerprint = fingerprint_of(&cert_der);
        tracing::info!("nova identidade criada: {device_id}");
        Ok(Identity { device_id, cert_der, key_der, fingerprint })
    }
}

fn fallback_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("private_key.b64")
}

/// Usar o cofre do sistema? (no macOS, não — ver o cabeçalho do módulo)
const USE_SYSTEM_VAULT: bool = !cfg!(target_os = "macos");

fn store_private_key(data_dir: &Path, device_id: &str, key_der: &[u8]) -> Result<()> {
    let encoded = B64.encode(key_der);
    if !USE_SYSTEM_VAULT {
        return write_private_file(&fallback_key_path(data_dir), encoded.as_bytes());
    }
    match keyring::Entry::new(KEYRING_SERVICE, device_id).and_then(|e| e.set_password(&encoded)) {
        Ok(()) => {
            // Confere se o cofre realmente guardou (alguns ambientes usam armazenamento falso).
            if let Ok(back) = keyring::Entry::new(KEYRING_SERVICE, device_id).and_then(|e| e.get_password()) {
                if back == encoded {
                    let _ = fs::remove_file(fallback_key_path(data_dir));
                    return Ok(());
                }
            }
            tracing::warn!("cofre do sistema não confirmou a gravação; usando arquivo local");
        }
        Err(e) => tracing::warn!("cofre do sistema indisponível ({e}); usando arquivo local"),
    }
    write_private_file(&fallback_key_path(data_dir), encoded.as_bytes())
}

fn load_private_key(data_dir: &Path, device_id: &str) -> Result<Vec<u8>> {
    let path = fallback_key_path(data_dir);
    if !USE_SYSTEM_VAULT {
        if let Ok(encoded) = fs::read_to_string(&path) {
            return Ok(B64.decode(encoded.trim())?);
        }
        // Migração única de versões antigas: lê do Keychain, passa para o arquivo e apaga
        // o item do Keychain (pode pedir a senha do Mac esta última vez).
        let entry = keyring::Entry::new(KEYRING_SERVICE, device_id)?;
        let encoded = entry.get_password().context("chave privada no Keychain")?;
        let key = B64.decode(encoded.trim())?;
        write_private_file(&path, encoded.trim().as_bytes())?;
        let _ = entry.delete_credential();
        tracing::info!("chave privada migrada do Keychain para arquivo local");
        return Ok(key);
    }
    if let Ok(encoded) = keyring::Entry::new(KEYRING_SERVICE, device_id).and_then(|e| e.get_password()) {
        return Ok(B64.decode(encoded.trim())?);
    }
    let encoded = fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
    Ok(B64.decode(encoded.trim())?)
}

#[cfg(unix)]
fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data)?;
    Ok(())
}

/// SHA-256 de um certificado DER, em hexadecimal minúsculo.
pub fn fingerprint_of(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Formata o fingerprint para comparação visual: `A1B2 C3D4 ...` (16 grupos de 4).
pub fn format_fingerprint(fp_hex: &str) -> String {
    fp_hex
        .to_uppercase()
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Código curto de verificação (SAS) derivado dos dois fingerprints.
/// Os dois lados calculam o mesmo valor; se coincidir, não há intermediário na conexão.
pub fn sas_code(fp_a: &str, fp_b: &str) -> String {
    let (first, second) = if fp_a <= fp_b { (fp_a, fp_b) } else { (fp_b, fp_a) };
    let mut h = Sha256::new();
    h.update(b"lrd-sas-v1");
    h.update(first.as_bytes());
    h.update(second.as_bytes());
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sas_is_symmetric() {
        assert_eq!(sas_code("aa", "bb"), sas_code("bb", "aa"));
        assert_eq!(sas_code("aa", "bb").len(), 7);
    }

    #[test]
    fn format_groups() {
        assert_eq!(format_fingerprint("a1b2c3d4"), "A1B2 C3D4");
    }
}
