//! Transporte QUIC + TLS 1.3 com autenticação mútua por certificado (mTLS).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use lrd_protocol::ALPN;
use lrd_security::{AcceptAnyClientCert, CapturingServerVerifier, Identity};

pub use quinn;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport_config() -> Result<Arc<quinn::TransportConfig>> {
    let mut t = quinn::TransportConfig::default();
    // Sem tráfego por 15 s = par perdido. Keep-alive a cada 2 s funciona como heartbeat.
    t.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    Ok(Arc::new(t))
}

fn cert_and_key(identity: &Identity) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    (vec![cert], key)
}

/// Configuração do lado que recebe conexões (computador controlado).
pub fn server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    let provider = provider();
    let (certs, key) = cert_and_key(identity);
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert::new(provider)))
        .with_single_cert(certs, key)
        .context("carregando certificado do servidor")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(quic));
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// Configuração de uma conexão de saída. `seen` recebe o certificado DER do servidor.
pub fn client_config(
    identity: &Identity,
    expected_fingerprint: Option<String>,
    seen: Arc<Mutex<Option<Vec<u8>>>>,
) -> Result<quinn::ClientConfig> {
    let provider = provider();
    let (certs, key) = cert_and_key(identity);
    let verifier = CapturingServerVerifier::new(provider.clone(), expected_fingerprint, seen);
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(certs, key)
        .context("carregando certificado do cliente")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// Abre o endpoint de escuta em todas as interfaces IPv4. A restrição a redes locais é
/// aplicada a cada conexão recebida (ver `lrd_security::is_allowed_peer_ip`).
pub fn listen(identity: &Identity, port: u16) -> Result<quinn::Endpoint> {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    quinn::Endpoint::server(server_config(identity)?, addr)
        .with_context(|| format!("não foi possível escutar na porta UDP {port} (já está em uso?)"))
}

/// Endpoint para conexões de saída (porta efêmera).
pub fn client_endpoint() -> Result<quinn::Endpoint> {
    let addr: SocketAddr = ([0, 0, 0, 0], 0).into();
    Ok(quinn::Endpoint::client(addr)?)
}

/// Certificado DER apresentado pelo par (exigido no mTLS).
pub fn peer_certificate(conn: &quinn::Connection) -> Result<Vec<u8>> {
    let identity = conn.peer_identity().ok_or_else(|| anyhow!("par sem certificado"))?;
    let certs = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow!("tipo de identidade inesperado"))?;
    let first = certs.first().ok_or_else(|| anyhow!("cadeia de certificados vazia"))?;
    Ok(first.as_ref().to_vec())
}
