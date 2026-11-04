//! Fontes de quadros: captura contínua quando disponível, cópia periódica como reserva.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::capture::{Capturer, Display};
use crate::encoder::QualitySettings;
use crate::frame::RawFrame;

pub enum Source {
    /// Cópia de tela a cada chamada (lenta, mas funciona em qualquer lugar).
    Poll(Capturer),
    /// Windows: DXGI Desktop Duplication (entrega quadros só quando a tela muda).
    #[cfg(target_os = "windows")]
    Dxgi {
        recorder: xcap::VideoRecorder,
        rx: std::sync::mpsc::Receiver<xcap::Frame>,
        display: Display,
    },
    /// macOS: ScreenCaptureKit (vídeo e, opcionalmente, áudio).
    #[cfg(target_os = "macos")]
    Sck(crate::mac_sck::SckSource),
}

impl Source {
    /// Abre a melhor fonte disponível. `audio` só é usado no macOS (o áudio vem do mesmo
    /// stream do ScreenCaptureKit).
    pub fn open(display_id: Option<u32>, q: &QualitySettings, audio: Option<mpsc::Sender<Vec<u8>>>) -> Result<Source> {
        let capturer = Capturer::open(display_id)?;

        #[cfg(target_os = "macos")]
        {
            match crate::mac_sck::SckSource::start(&capturer.display, q, audio) {
                Ok(s) => return Ok(Source::Sck(s)),
                Err(e) => tracing::warn!("ScreenCaptureKit indisponível, usando captura por cópia: {e:#}"),
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (q, audio);

        #[cfg(target_os = "windows")]
        {
            match capturer.monitor.video_recorder() {
                Ok((recorder, rx)) => match recorder.start() {
                    Ok(()) => {
                        let display = capturer.display.clone();
                        return Ok(Source::Dxgi { recorder, rx, display });
                    }
                    Err(e) => tracing::warn!("DXGI indisponível ({e}), usando captura por cópia"),
                },
                Err(e) => tracing::warn!("DXGI indisponível ({e}), usando captura por cópia"),
            }
        }

        Ok(Source::Poll(capturer))
    }

    pub fn display(&self) -> &Display {
        match self {
            Source::Poll(c) => &c.display,
            #[cfg(target_os = "windows")]
            Source::Dxgi { display, .. } => display,
            #[cfg(target_os = "macos")]
            Source::Sck(s) => &s.display,
        }
    }

    /// O áudio vem desta fonte (macOS)?
    pub fn provides_audio(&self) -> bool {
        match self {
            #[cfg(target_os = "macos")]
            Source::Sck(s) => s.audio,
            _ => false,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Source::Poll(_) => "cópia",
            #[cfg(target_os = "windows")]
            Source::Dxgi { .. } => "DXGI",
            #[cfg(target_os = "macos")]
            Source::Sck(_) => "ScreenCaptureKit",
        }
    }

    /// Próximo quadro, esperando no máximo `timeout`. Fontes contínuas devolvem sempre o
    /// quadro mais recente (descartam os intermediários).
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
        match self {
            Source::Poll(c) => {
                std::thread::sleep(timeout.min(Duration::from_millis(5)));
                c.capture().map(Some)
            }
            #[cfg(target_os = "windows")]
            Source::Dxgi { rx, .. } => {
                let first = match rx.recv_timeout(timeout) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(anyhow::anyhow!("captura DXGI encerrada"))
                    }
                };
                let mut latest = first;
                while let Ok(f) = rx.try_recv() {
                    latest = f;
                }
                let expected = latest.width as usize * latest.height as usize * 4;
                if latest.raw.len() < expected {
                    return Ok(None);
                }
                let mut data = latest.raw;
                data.truncate(expected);
                Ok(Some(RawFrame {
                    width: latest.width,
                    height: latest.height,
                    data,
                    format: crate::frame::PixelFormat::Rgba,
                    timestamp_us: lrd_protocol::now_us(),
                }))
            }
            #[cfg(target_os = "macos")]
            Source::Sck(s) => s.next_frame(timeout),
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        if let Source::Dxgi { recorder, .. } = self {
            let _ = recorder.stop();
        }
    }
}
