//! Thread de captura: captura → codifica → entrega para o envio pela rede.
//!
//! A entrega usa um canal com capacidade 1 e `blocking_send`: se a rede estiver lenta, a
//! captura espera, então a taxa de quadros se adapta sozinha à banda disponível e nenhum
//! quadro delta é descartado (o que corromperia a imagem do outro lado).

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lrd_protocol::video::build_cursor_frame;
use tokio::sync::mpsc;

use crate::capture::Capturer;
use crate::encoder::{QualitySettings, TileEncoder};

pub enum CaptureCmd {
    SelectDisplay(u32),
    SetQuality(QualitySettings),
    Keyframe,
    Stop,
}

pub type CursorFn = Box<dyn Fn() -> Option<(i32, i32)> + Send>;

pub struct CaptureParams {
    pub display_id: Option<u32>,
    pub quality: QualitySettings,
    /// Posição global do cursor no sistema de coordenadas do SO (opcional).
    pub cursor: Option<CursorFn>,
}

pub struct CaptureHandle {
    pub commands: Sender<CaptureCmd>,
    join: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn send(&self, cmd: CaptureCmd) {
        let _ = self.commands.send(cmd);
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(CaptureCmd::Stop);
        // Não bloqueia esperando a thread: ela termina sozinha ao ver Stop ou o canal fechado.
        let _ = self.join.take();
    }
}

pub fn spawn_capture(params: CaptureParams, out: mpsc::Sender<Vec<u8>>) -> CaptureHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    let join = std::thread::Builder::new()
        .name("lrd-capture".into())
        .spawn(move || run(params, rx, out))
        .expect("criando thread de captura");
    CaptureHandle { commands: tx, join: Some(join) }
}

fn run(params: CaptureParams, rx: Receiver<CaptureCmd>, out: mpsc::Sender<Vec<u8>>) {
    let mut quality = params.quality;
    let mut display_id = params.display_id;
    let mut capturer: Option<Capturer> = None;
    let mut encoder = TileEncoder::new();
    let mut failures = 0u32;
    let mut last_cursor: Option<(u16, u16)> = None;

    loop {
        // Comandos pendentes.
        loop {
            match rx.try_recv() {
                Ok(CaptureCmd::SelectDisplay(id)) => {
                    display_id = Some(id);
                    capturer = None;
                    encoder.reset();
                }
                Ok(CaptureCmd::SetQuality(q)) => {
                    if q.max_width != quality.max_width || q.jpeg_quality != quality.jpeg_quality {
                        encoder.reset();
                    }
                    quality = q;
                }
                Ok(CaptureCmd::Keyframe) => encoder.reset(),
                Ok(CaptureCmd::Stop) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }
        if out.is_closed() {
            return;
        }

        let started = Instant::now();
        let interval = Duration::from_millis(1000 / quality.max_fps.max(1) as u64);

        if capturer.is_none() {
            match Capturer::open(display_id) {
                Ok(c) => {
                    display_id = Some(c.display.id);
                    capturer = Some(c);
                    encoder.reset();
                }
                Err(e) => {
                    tracing::warn!("não foi possível abrir o monitor: {e:#}");
                    std::thread::sleep(Duration::from_millis(1000));
                    continue;
                }
            }
        }
        let cap = capturer.as_ref().unwrap();

        match cap.capture() {
            Ok(frame) => {
                failures = 0;
                let (fw, fh) = (frame.width, frame.height);
                let encoded = encoder.encode(frame, &quality);
                if let Some(bytes) = encoded {
                    if out.blocking_send(bytes).is_err() {
                        return;
                    }
                }
                // Cursor: converte a posição global em pixels do quadro enviado.
                if let Some(cursor_fn) = &params.cursor {
                    let d = &cap.display;
                    let scaled_w = crate::encoder::downscale_dims(fw, fh, quality.max_width);
                    let cur = cursor_fn().and_then(|(cx, cy)| {
                        let rx = cx - d.x;
                        let ry = cy - d.y;
                        if rx < 0 || ry < 0 || rx >= d.width as i32 || ry >= d.height as i32 {
                            return None;
                        }
                        let px = (rx as f64 / d.width as f64 * scaled_w.0 as f64) as u16;
                        let py = (ry as f64 / d.height as f64 * scaled_w.1 as f64) as u16;
                        Some((px, py))
                    });
                    if cur != last_cursor {
                        let msg = match cur {
                            Some((x, y)) => build_cursor_frame(true, x, y),
                            None => build_cursor_frame(false, 0, 0),
                        };
                        if out.blocking_send(msg).is_err() {
                            return;
                        }
                        last_cursor = cur;
                    }
                }
            }
            Err(e) => {
                failures += 1;
                if failures == 1 || failures % 50 == 0 {
                    tracing::warn!("falha na captura ({failures}x): {e:#}");
                }
                if failures > 5 {
                    // Monitor pode ter sido desconectado: reabre.
                    capturer = None;
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        }

        let elapsed = started.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}
