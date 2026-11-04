//! Captura de tela, áudio do sistema e codificação.
//!
//! Vídeo: captura contínua (DXGI Desktop Duplication no Windows, ScreenCaptureKit no macOS,
//! com captura por cópia como reserva) → redimensionamento → H.264 (OpenH264, modo
//! "screen content" de baixa latência). Tiles JPEG continuam disponíveis para controladores
//! que não decodificam H.264.
//!
//! Áudio: loopback WASAPI no Windows e ScreenCaptureKit no macOS, PCM 16 bits.
//! Vídeo e áudio levam carimbos de tempo do mesmo relógio (`lrd_protocol::now_us`).

pub mod audio;
pub mod capture;
pub mod color;
pub mod encoder;
pub mod frame;
pub mod h264;
#[cfg(target_os = "macos")]
mod hw_mac;
#[cfg(target_os = "windows")]
mod hw_win;
#[cfg(target_os = "windows")]
mod win_gpu;
#[cfg(target_os = "macos")]
mod mac_sck;
pub mod permissions;
pub mod pipeline;
mod power;
pub mod source;

pub use capture::{list_displays, Display};
pub use encoder::{QualitySettings, TileEncoder};
pub use frame::{NativeFrame, PixelFormat, RawFrame};
pub use pipeline::{spawn_capture, CaptureCmd, CaptureHandle, CaptureParams, CursorInfo, PipelineStats, VideoCodec};
