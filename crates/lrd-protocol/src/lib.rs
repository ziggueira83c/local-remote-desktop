//! Protocolo compartilhado e versionado do LRD (RN-015).
//!
//! - `messages`: mensagens Protobuf (`lrd.v1`) trafegadas no canal de controle e de entrada.
//! - `framing`: enquadramento com prefixo de tamanho sobre streams QUIC.
//! - `video`: formato binário dos quadros de vídeo (tiles JPEG + cursor).

pub mod framing;
pub mod messages;
pub mod lrd;
pub mod video;

pub use messages::*;

/// Versão atual do protocolo. Incrementar a cada mudança incompatível.
pub const PROTOCOL_VERSION: u32 = 1;
/// Versão mínima aceita de um par.
pub const MIN_PROTOCOL_VERSION: u32 = 1;
/// Identificador ALPN negociado no TLS.
pub const ALPN: &[u8] = b"lrd/1";
/// Porta UDP padrão (QUIC).
pub const DEFAULT_PORT: u16 = 47321;
/// Tipo de serviço DNS-SD.
pub const SERVICE_TYPE: &str = "_lrd._udp.local.";

/// Limite de tamanho de uma mensagem de controle/entrada.
pub const MAX_CONTROL_MESSAGE: usize = 256 * 1024;
/// Limite de tamanho de um quadro de vídeo.
pub const MAX_VIDEO_FRAME: usize = 32 * 1024 * 1024;

/// Códigos de motivo estáveis usados em respostas e encerramentos.
pub mod reason {
    pub const APPROVED: &str = "approved";
    pub const REJECTED: &str = "rejected_by_user";
    pub const TIMEOUT: &str = "approval_timeout";
    pub const BUSY: &str = "busy";
    pub const VERSION: &str = "unsupported_version";
    pub const IDENTITY_MISMATCH: &str = "identity_mismatch";
    pub const WRONG_PASSWORD: &str = "wrong_password";
    pub const USER_CLOSED: &str = "closed_by_user";
    pub const REVOKED: &str = "revoked";
    pub const PROTOCOL_ERROR: &str = "protocol_error";
}

/// Codecs de vídeo conhecidos.
pub mod codec {
    /// H.264 (perfil baseline, Annex B) — padrão quando o controlador consegue decodificar.
    pub const H264: &str = "h264";
    /// Tiles JPEG com envio apenas das regiões alteradas (compatibilidade).
    pub const JPEG_TILES: &str = "jpeg-tiles";
    /// O controlador aceita mídia por WebRTC (H.264 + Opus via RTP/SRTP).
    pub const WEBRTC: &str = "webrtc";
    /// O controlador decodifica H.265 no WebRTC (com hardware).
    pub const WEBRTC_H265: &str = "webrtc-h265";
    /// codecLRD: codec próprio de tela (ver `crate::lrd`).
    pub const LRD: &str = "lrd";
    /// codecLRD com região de vídeo: o controlador decodifica H.264 (WebCodecs).
    pub const LRD_VIDEO: &str = "lrd-video";
    /// ... e também H.265.
    pub const LRD_VIDEO_H265: &str = "lrd-video-h265";
}

/// Relógio monotônico do processo em microssegundos. Vídeo e áudio usam o mesmo relógio
/// para que o controlador consiga sincronizá-los.
pub fn now_us() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
