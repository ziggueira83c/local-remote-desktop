//! Protocolo compartilhado e versionado do LRD (RN-015).
//!
//! - `messages`: mensagens Protobuf (`lrd.v1`) trafegadas no canal de controle e de entrada.
//! - `framing`: enquadramento com prefixo de tamanho sobre streams QUIC.
//! - `video`: formato binário dos quadros de vídeo (tiles JPEG + cursor).

pub mod framing;
pub mod messages;
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
    pub const USER_CLOSED: &str = "closed_by_user";
    pub const REVOKED: &str = "revoked";
    pub const PROTOCOL_ERROR: &str = "protocol_error";
}

/// Codecs de vídeo conhecidos.
pub mod codec {
    /// Tiles JPEG com envio apenas das regiões alteradas.
    pub const JPEG_TILES: &str = "jpeg-tiles";
}
