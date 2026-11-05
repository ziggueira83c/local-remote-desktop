//! Orquestração de sessões: cada instalação é ao mesmo tempo controladora (viewer) e
//! controlada (host). Toda conexão é autenticada por certificado (RN-005) e toda sessão de
//! entrada exige aprovação explícita no computador controlado (RN-006).

mod host;
mod node;
#[doc(hidden)]
pub mod rtc;
#[doc(hidden)]
pub mod rtp_video;
mod test_loss;
mod viewer;

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use lrd_discovery::DiscoveredDevice;
pub use lrd_security::{Permissions, Profile};
pub use lrd_storage::{DeviceRecord, SessionRecord};
pub use node::{LocalDeviceInfo, Node};
pub use viewer::{ConnectParams, InputDto};

pub struct NodeConfig {
    pub data_dir: PathBuf,
}

/// Configurações do usuário (persistidas no SQLite, chave `app`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub device_name: String,
    pub listen_port: u16,
    pub discovery_enabled: bool,
    /// "speed" | "balanced" | "quality"
    pub quality_profile: String,
    pub max_fps: u32,
    /// Ao controlar um computador de outra plataforma, troca Ctrl ↔ Cmd automaticamente.
    pub swap_ctrl_cmd: bool,
    /// Sempre `true` nesta versão: acesso não supervisionado está desativado (RN-007).
    pub require_approval: bool,
    /// Permite enviar o áudio do sistema quando este computador é acessado.
    pub share_audio: bool,
    /// Ao controlar: receber o áudio do outro computador.
    pub receive_audio: bool,
    /// Ao controlar: aplicar melhoria de imagem (nitidez) na tela remota.
    pub enhance_image: bool,
    /// Intensidade da melhoria de imagem, de 0.0 a 1.0.
    pub enhance_strength: f32,
    /// Aceitar conexões com a senha de acesso sem pedir aprovação (exige senha definida).
    pub password_access: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            device_name: String::new(),
            listen_port: lrd_protocol::DEFAULT_PORT,
            discovery_enabled: true,
            quality_profile: "balanced".into(),
            max_fps: 60,
            swap_ctrl_cmd: true,
            require_approval: true,
            share_audio: true,
            receive_audio: true,
            enhance_image: false,
            enhance_strength: 0.5,
            password_access: false,
        }
    }
}

/// Monitor remoto, como exibido no controlador.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDisplay {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Eventos enviados à interface.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeEvent {
    DevicesChanged {
        devices: Vec<DiscoveredDevice>,
    },
    /// Alguém pediu para acessar este computador.
    IncomingRequest {
        session_id: String,
        device_id: String,
        device_name: String,
        platform: String,
        address: String,
        fingerprint: String,
        sas: String,
        known_device: bool,
        requested_profile: Profile,
    },
    IncomingCancelled {
        session_id: String,
        reason: String,
    },
    InboundStarted {
        session_id: String,
        device_name: String,
        address: String,
        permissions: Permissions,
    },
    InboundEnded {
        session_id: String,
        reason: String,
    },
    /// Progresso de uma conexão de saída.
    OutboundState {
        state: String,
        remote_name: Option<String>,
        fingerprint: Option<String>,
        sas: Option<String>,
    },
    OutboundStarted {
        session_id: String,
        remote_name: String,
        remote_platform: String,
        displays: Vec<RemoteDisplay>,
        active_display: String,
        permissions: Permissions,
        fingerprint: String,
        sas: String,
        video_codec: String,
        audio_enabled: bool,
    },
    OutboundEnded {
        session_id: String,
        reason: String,
    },
    /// Diagnóstico do computador acessado (a cada segundo).
    HostStats {
        session_id: String,
        capture_source: String,
        encoder: String,
        width: u32,
        height: u32,
        capture_fps: f32,
        encode_fps: f32,
        encode_ms: f32,
        bitrate_kbps: u32,
        transport: String,
    },
    /// Oferta WebRTC do host; a interface responde com `webrtc_answer`.
    WebRtcOffer {
        session_id: String,
        sdp: String,
    },
    PermissionsChanged {
        session_id: String,
        permissions: Permissions,
    },
    Metrics {
        session_id: String,
        fps: f64,
        kbps: f64,
        rtt_ms: f64,
    },
    Notice {
        level: String,
        message: String,
    },
}

/// Destino dos eventos (a camada Tauri repassa para o frontend).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: NodeEvent);
}

/// Recebe cada quadro de vídeo binário (formato `lrd_protocol::video`).
pub type FrameSink = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "other"
    }
}

/// Mensagem amigável para um código de motivo.
pub fn describe_reason(code: &str) -> String {
    use lrd_protocol::reason::*;
    match code {
        REJECTED => "O pedido foi recusado no outro computador.".into(),
        TIMEOUT => "Ninguém respondeu ao pedido no outro computador a tempo.".into(),
        BUSY => "O outro computador já está em uma sessão.".into(),
        VERSION => "Versões incompatíveis do aplicativo.".into(),
        IDENTITY_MISMATCH => "A identidade deste computador não confere com a registrada no outro lado.".into(),
        WRONG_PASSWORD => "Senha incorreta.".into(),
        USER_CLOSED => "Sessão encerrada.".into(),
        REVOKED => "O acesso foi revogado no computador controlado.".into(),
        "connection_lost" => "Conexão perdida.".into(),
        "peer_cancelled" => "O solicitante desistiu.".into(),
        other => other.to_string(),
    }
}
