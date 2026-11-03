//! Captura de tela e codificação de vídeo.
//!
//! Pipeline: captura nativa (xcap: GDI/DXGI no Windows, CoreGraphics no macOS) →
//! redução de escala opcional → detecção de tiles alterados → JPEG por tile →
//! quadro binário (`lrd_protocol::video`). Só o que mudou na tela é enviado.

pub mod capture;
pub mod encoder;
pub mod permissions;
pub mod pipeline;

pub use capture::{list_displays, Display};
pub use encoder::{QualitySettings, TileEncoder};
pub use pipeline::{spawn_capture, CaptureCmd, CaptureHandle, CaptureParams};
