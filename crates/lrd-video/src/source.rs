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
        /// Só o quadro mais recente (a fila do xcap é ilimitada: se o envio atrasar, quadros
        /// de 8 MB se acumulariam até esgotar a memória do computador).
        latest: std::sync::Arc<(std::sync::Mutex<Option<xcap::Frame>>, std::sync::Condvar)>,
        display: Display,
    },
    /// Windows: DXGI Desktop Duplication próprio, com o quadro mantido na GPU.
    #[cfg(target_os = "windows")]
    Gpu(crate::win_gpu::DuplSource),
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
            use std::sync::atomic::{AtomicU32, Ordering};
            // Falhas seguidas da captura na GPU. Ela falha de propósito enquanto o UAC ou a
            // tela de bloqueio estão na frente: insiste (o pipeline tenta de novo a cada 1 s)
            // em vez de cair de vez para a captura com cópia.
            static GPU_FAILS: AtomicU32 = AtomicU32::new(0);
            if std::env::var_os("LRD_NO_GPU_CAPTURE").is_none() {
                match crate::win_gpu::DuplSource::open(&capturer.display) {
                    Ok(s) => {
                        GPU_FAILS.store(0, Ordering::Relaxed);
                        return Ok(Source::Gpu(s));
                    }
                    Err(e) => {
                        let n = GPU_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                        if n < 10 {
                            return Err(e.context("captura na GPU (tentando de novo)"));
                        }
                        tracing::warn!("captura na GPU indisponível ({e:#}); usando DXGI com cópia");
                    }
                }
            }
            match capturer.monitor.video_recorder() {
                Ok((recorder, rx)) => match recorder.start() {
                    Ok(()) => {
                        let display = capturer.display.clone();
                        let latest = std::sync::Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
                        let slot = latest.clone();
                        // Esvazia a fila do xcap continuamente, guardando só o último quadro.
                        std::thread::Builder::new()
                            .name("lrd-xcap".into())
                            .spawn(move || {
                                while let Ok(f) = rx.recv() {
                                    *slot.0.lock().unwrap() = Some(f);
                                    slot.1.notify_one();
                                }
                            })
                            .ok();
                        return Ok(Source::Dxgi { recorder, latest, display });
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
            #[cfg(target_os = "windows")]
            Source::Gpu(s) => &s.display,
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
            #[cfg(target_os = "windows")]
            Source::Gpu(_) => "DXGI (GPU)",
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
            Source::Dxgi { latest: slot, .. } => {
                let (lock, cv) = &**slot;
                let guard = lock.lock().unwrap();
                let (mut guard, _) = cv.wait_timeout_while(guard, timeout, |f| f.is_none()).unwrap();
                let Some(latest) = guard.take() else { return Ok(None) };
                drop(guard);
                let expected = latest.width as usize * latest.height as usize * 4;
                if latest.raw.len() < expected {
                    return Ok(None);
                }
                let mut data = latest.raw;
                data.truncate(expected);
                Ok(Some(RawFrame::cpu(
                    latest.width,
                    latest.height,
                    data,
                    crate::frame::PixelFormat::Rgba,
                    lrd_protocol::now_us(),
                )))
            }
            #[cfg(target_os = "windows")]
            Source::Gpu(s) => s.next_frame(timeout),
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
