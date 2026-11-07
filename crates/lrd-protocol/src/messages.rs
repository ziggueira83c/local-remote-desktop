//! Mensagens `lrd.v1` (equivalentes a `proto/lrd.proto`), declaradas com `prost` derive
//! para não exigir `protoc` na compilação. O formato de fio é Protobuf padrão.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Envelope {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(string, tag = "2")]
    pub session_id: String,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    #[prost(oneof = "envelope::Payload", tags = "10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22")]
    pub payload: Option<envelope::Payload>,
}

pub mod envelope {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Hello(super::Hello),
        #[prost(message, tag = "11")]
        ConnectRequest(super::ConnectRequest),
        #[prost(message, tag = "12")]
        ConnectResponse(super::ConnectResponse),
        #[prost(message, tag = "13")]
        InputEvent(super::InputEvent),
        #[prost(message, tag = "14")]
        PermissionUpdate(super::PermissionUpdate),
        #[prost(message, tag = "15")]
        SessionClose(super::SessionClose),
        #[prost(message, tag = "16")]
        SelectDisplay(super::SelectDisplay),
        #[prost(message, tag = "17")]
        SetQuality(super::SetQuality),
        #[prost(message, tag = "18")]
        RequestKeyframe(super::RequestKeyframe),
        #[prost(message, tag = "19")]
        SetAudio(super::SetAudio),
        #[prost(message, tag = "20")]
        WebRtcSignal(super::WebRtcSignal),
        #[prost(message, tag = "21")]
        HostStats(super::HostStats),
        #[prost(message, tag = "22")]
        ClipboardText(super::ClipboardText),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Hello {
    #[prost(string, tag = "1")]
    pub device_id: String,
    #[prost(string, tag = "2")]
    pub device_name: String,
    #[prost(uint32, tag = "3")]
    pub min_version: u32,
    #[prost(uint32, tag = "4")]
    pub max_version: u32,
    #[prost(string, repeated, tag = "5")]
    pub codecs: Vec<String>,
    /// "windows" | "macos" | "other"
    #[prost(string, tag = "6")]
    pub platform: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectRequest {
    /// "view_only" | "remote_control" | "full_control"
    #[prost(string, tag = "1")]
    pub requested_profile: String,
    #[prost(string, tag = "2")]
    pub requested_display: String,
    #[prost(bool, tag = "3")]
    pub control_requested: bool,
    /// O controlador quer receber o áudio do sistema.
    #[prost(bool, tag = "4")]
    pub want_audio: bool,
    /// Senha de acesso (opcional). Trafega só dentro do TLS; o host guarda apenas o hash.
    #[prost(string, tag = "5")]
    pub password: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct DisplayInfo {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(bool, tag = "5")]
    pub primary: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectResponse {
    #[prost(bool, tag = "1")]
    pub approved: bool,
    #[prost(string, tag = "2")]
    pub reason_code: String,
    #[prost(string, repeated, tag = "3")]
    pub granted_permissions: Vec<String>,
    #[prost(message, repeated, tag = "4")]
    pub displays: Vec<DisplayInfo>,
    #[prost(string, tag = "5")]
    pub active_display: String,
    /// Codec de vídeo escolhido pelo host ("h264" ou "jpeg-tiles").
    #[prost(string, tag = "6")]
    pub video_codec: String,
    /// O host vai transmitir áudio.
    #[prost(bool, tag = "7")]
    pub audio_enabled: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct InputEvent {
    #[prost(oneof = "input_event::Event", tags = "1, 2, 3, 4")]
    pub event: Option<input_event::Event>,
}

pub mod input_event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        PointerMove(super::PointerMove),
        #[prost(message, tag = "2")]
        MouseButton(super::MouseButton),
        #[prost(message, tag = "3")]
        KeyEvent(super::KeyEvent),
        #[prost(message, tag = "4")]
        MouseScroll(super::MouseScroll),
    }
}

/// Posição absoluta normalizada: `x` e `y` vão de 0 a 65535 dentro do monitor `display_id`.
/// A normalização evita depender de DPI/escala diferentes entre controlador e controlado.
#[derive(Clone, PartialEq, Message)]
pub struct PointerMove {
    #[prost(string, tag = "1")]
    pub display_id: String,
    #[prost(uint32, tag = "2")]
    pub x: u32,
    #[prost(uint32, tag = "3")]
    pub y: u32,
}

/// 0 = esquerdo, 1 = meio, 2 = direito.
#[derive(Clone, PartialEq, Message)]
pub struct MouseButton {
    #[prost(uint32, tag = "1")]
    pub button: u32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

/// Tecla física identificada pelo USB HID usage (página 0x07).
#[derive(Clone, PartialEq, Message)]
pub struct KeyEvent {
    #[prost(uint32, tag = "1")]
    pub hid_usage: u32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

/// Rolagem em "cliques" de roda. Positivo = para baixo / para a direita.
#[derive(Clone, PartialEq, Message)]
pub struct MouseScroll {
    #[prost(sint32, tag = "1")]
    pub delta_x: i32,
    #[prost(sint32, tag = "2")]
    pub delta_y: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct PermissionUpdate {
    #[prost(string, repeated, tag = "1")]
    pub granted_permissions: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SessionClose {
    #[prost(string, tag = "1")]
    pub reason_code: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectDisplay {
    #[prost(string, tag = "1")]
    pub display_id: String,
}

/// `profile`: "speed" | "balanced" | "quality".
#[derive(Clone, PartialEq, Message)]
pub struct SetQuality {
    #[prost(string, tag = "1")]
    pub profile: String,
    #[prost(uint32, tag = "2")]
    pub max_fps: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestKeyframe {}

/// Sinalização WebRTC (SDP), trafegando pelo canal QUIC já autenticado por certificado.
/// Isso amarra a identidade DTLS do WebRTC à identidade verificada do par.
#[derive(Clone, PartialEq, Message)]
pub struct WebRtcSignal {
    /// "offer" (host → controlador) ou "answer" (controlador → host).
    #[prost(string, tag = "1")]
    pub kind: String,
    #[prost(string, tag = "2")]
    pub sdp: String,
}

/// Diagnóstico do host, enviado a cada segundo.
#[derive(Clone, PartialEq, Message)]
pub struct HostStats {
    #[prost(string, tag = "1")]
    pub capture_source: String,
    #[prost(string, tag = "2")]
    pub encoder: String,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(float, tag = "5")]
    pub capture_fps: f32,
    #[prost(float, tag = "6")]
    pub encode_fps: f32,
    #[prost(float, tag = "7")]
    pub encode_ms: f32,
    #[prost(uint32, tag = "8")]
    pub bitrate_kbps: u32,
    /// "webrtc" | "quic" | "negociando"
    #[prost(string, tag = "9")]
    pub transport: String,
}

/// Liga/desliga o envio de áudio durante a sessão.
#[derive(Clone, PartialEq, Message)]
pub struct SetAudio {
    #[prost(bool, tag = "1")]
    pub enabled: bool,
}

/// Texto copiado num computador, para colar no outro (nos dois sentidos).
#[derive(Clone, PartialEq, Message)]
pub struct ClipboardText {
    #[prost(string, tag = "1")]
    pub text: String,
}

impl Envelope {
    pub fn new(session_id: &str, sequence: u64, payload: envelope::Payload) -> Self {
        Envelope {
            protocol_version: crate::PROTOCOL_VERSION,
            session_id: session_id.to_string(),
            sequence,
            payload: Some(payload),
        }
    }
}

/// Converte bytes em `Envelope`, com erro legível.
pub fn decode_envelope(buf: &[u8]) -> Result<Envelope, prost::DecodeError> {
    Envelope::decode(buf)
}

/// Serializa um `Envelope`.
pub fn encode_envelope(env: &Envelope) -> Vec<u8> {
    env.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_input() {
        let env = Envelope::new(
            "s1",
            7,
            envelope::Payload::InputEvent(InputEvent {
                event: Some(input_event::Event::KeyEvent(KeyEvent { hid_usage: 4, pressed: true })),
            }),
        );
        let bytes = encode_envelope(&env);
        let back = decode_envelope(&bytes).unwrap();
        assert_eq!(env, back);
    }
}
