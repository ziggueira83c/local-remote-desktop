//! Verificadores TLS personalizados.
//!
//! Os certificados são autoassinados, então a confiança vem do fingerprint, não de uma CA:
//! - Cliente (controlador): aceita o certificado do servidor somente se for igual ao
//!   fingerprint esperado (quando conhecido) e sempre registra o fingerprint visto, para
//!   comparação com o histórico (TOFU) e exibição ao usuário.
//! - Servidor (controlado): exige certificado do cliente (mTLS) e verifica a assinatura do
//!   handshake; a decisão de confiança acontece depois, na aprovação do usuário.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::identity::fingerprint_of;

#[derive(Debug)]
pub struct CapturingServerVerifier {
    provider: Arc<CryptoProvider>,
    expected_fingerprint: Option<String>,
    seen: Arc<Mutex<Option<Vec<u8>>>>,
}

impl CapturingServerVerifier {
    pub fn new(
        provider: Arc<CryptoProvider>,
        expected_fingerprint: Option<String>,
        seen: Arc<Mutex<Option<Vec<u8>>>>,
    ) -> Self {
        CapturingServerVerifier {
            provider,
            expected_fingerprint: expected_fingerprint.map(|s| s.to_lowercase()),
            seen,
        }
    }
}

impl ServerCertVerifier for CapturingServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let der = end_entity.as_ref().to_vec();
        let fp = fingerprint_of(&der);
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(der);
        }
        if let Some(expected) = &self.expected_fingerprint {
            if &fp != expected {
                return Err(rustls::Error::General(format!(
                    "fingerprint do servidor não confere (esperado {expected}, recebido {fp})"
                )));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
pub struct AcceptAnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl AcceptAnyClientCert {
    pub fn new(provider: Arc<CryptoProvider>) -> Self {
        AcceptAnyClientCert { provider }
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
