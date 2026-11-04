//! Thread de captura: fonte de quadros → redimensionamento → H.264 (ou tiles JPEG) → rede.
//!
//! - O quadro mais recente sempre vence: se a rede ou o codificador atrasarem, quadros
//!   intermediários são descartados antes da codificação (latência baixa e constante).
//! - A entrega usa um canal de capacidade 1 com `blocking_send`; a taxa de quadros se
//!   adapta sozinha à banda disponível.
//! - A última imagem fica guardada para gerar um quadro completo quando o controlador pede,
//!   mesmo com a tela parada.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lrd_protocol::video::build_cursor_frame;
use tokio::sync::mpsc;

use crate::encoder::{QualitySettings, TileEncoder};
use crate::frame::{resize, target_size, RawFrame};
use crate::h264::H264Encoder;
use crate::source::Source;

pub enum CaptureCmd {
    SelectDisplay(u32),
    SetQuality(QualitySettings),
    Keyframe,
    SetAudio(bool),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    JpegTiles,
}

pub type CursorFn = Box<dyn Fn() -> Option<(i32, i32)> + Send>;

pub struct CaptureParams {
    pub display_id: Option<u32>,
    pub quality: QualitySettings,
    pub codec: VideoCodec,
    /// Posição global do cursor no sistema de coordenadas do SO (opcional).
    pub cursor: Option<CursorFn>,
    /// Destino dos blocos de áudio (`None` = sessão sem áudio).
    pub audio: Option<mpsc::Sender<Vec<u8>>>,
    pub audio_enabled: bool,
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

enum Encoder {
    H264(H264Encoder),
    Tiles(TileEncoder),
}

fn make_encoder(codec: VideoCodec, q: &QualitySettings) -> Encoder {
    if codec == VideoCodec::H264 {
        match H264Encoder::new(q) {
            Ok(e) => return Encoder::H264(e),
            Err(e) => tracing::error!("H.264 indisponível, usando JPEG: {e:#}"),
        }
    }
    Encoder::Tiles(TileEncoder::new())
}

fn run(params: CaptureParams, rx: Receiver<CaptureCmd>, out: mpsc::Sender<Vec<u8>>) {
    let CaptureParams { display_id, quality, codec, cursor, audio, audio_enabled } = params;
    let mut quality = quality;
    let mut display_id = display_id;
    let mut audio_enabled = audio_enabled && audio.is_some();
    let mut encoder = make_encoder(codec, &quality);
    let mut source: Option<Source> = None;
    let mut win_audio: Option<crate::audio::AudioCapture> = None;

    // Quadro mais novo ainda não codificado, e o último já codificado (para keyframes).
    let mut pending: Option<RawFrame> = None;
    let mut last_encoded: Option<RawFrame> = None;
    let mut want_keyframe = true;
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let mut last_cursor: Option<(u16, u16)> = None;
    let mut failures = 0u32;

    loop {
        // ---- comandos ----
        loop {
            match rx.try_recv() {
                Ok(CaptureCmd::SelectDisplay(id)) => {
                    display_id = Some(id);
                    source = None;
                    pending = None;
                    last_encoded = None;
                    want_keyframe = true;
                }
                Ok(CaptureCmd::SetQuality(q)) => {
                    if q != quality {
                        quality = q;
                        encoder = make_encoder(codec, &quality);
                        // No macOS o tamanho e o FPS são aplicados pelo próprio ScreenCaptureKit.
                        if cfg!(target_os = "macos") {
                            source = None;
                        }
                        last_encoded = None;
                        want_keyframe = true;
                    }
                }
                Ok(CaptureCmd::Keyframe) => want_keyframe = true,
                Ok(CaptureCmd::SetAudio(on)) => {
                    let on = on && audio.is_some();
                    if on != audio_enabled {
                        audio_enabled = on;
                        if cfg!(target_os = "macos") {
                            source = None; // reabre o stream com ou sem áudio
                        }
                        if !on {
                            win_audio = None;
                        }
                    }
                }
                Ok(CaptureCmd::Stop) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }
        if out.is_closed() {
            return;
        }

        // ---- áudio no Windows (loopback separado do vídeo) ----
        if cfg!(target_os = "windows") && audio_enabled && win_audio.is_none() {
            if let Some(tx) = &audio {
                match crate::audio::start_system_audio(tx.clone()) {
                    Ok(a) => win_audio = Some(a),
                    Err(e) => {
                        tracing::warn!("áudio do sistema indisponível: {e:#}");
                        audio_enabled = false;
                    }
                }
            }
        }

        // ---- fonte ----
        if source.is_none() {
            let audio_for_source = if audio_enabled { audio.clone() } else { None };
            match Source::open(display_id, &quality, audio_for_source) {
                Ok(s) => {
                    tracing::info!("captura: {} ({}x{})", s.name(), s.display().width, s.display().height);
                    display_id = Some(s.display().id);
                    source = Some(s);
                    want_keyframe = true;
                    failures = 0;
                }
                Err(e) => {
                    tracing::warn!("não foi possível abrir o monitor: {e:#}");
                    std::thread::sleep(Duration::from_millis(1000));
                    continue;
                }
            }
        }
        let src = source.as_mut().unwrap();

        let interval = Duration::from_micros(1_000_000 / quality.max_fps.max(1) as u64);
        let since = last_sent.elapsed();
        let wait = if pending.is_some() || want_keyframe {
            interval.saturating_sub(since).max(Duration::from_millis(1))
        } else {
            interval
        };

        match src.next_frame(wait) {
            Ok(Some(frame)) => {
                pending = Some(frame);
                failures = 0;
            }
            Ok(None) => {}
            Err(e) => {
                failures += 1;
                if failures == 1 || failures % 50 == 0 {
                    tracing::warn!("falha na captura ({failures}x): {e:#}");
                }
                if failures > 5 {
                    source = None;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }

        if last_sent.elapsed() < interval {
            continue;
        }

        // ---- codificação ----
        let to_encode = match pending.take() {
            Some(raw) => {
                let (tw, th) = target_size(raw.width, raw.height, quality.max_width);
                Some(resize(raw, tw, th))
            }
            None if want_keyframe => last_encoded.take(),
            None => None,
        };
        let Some(frame) = to_encode else { continue };

        let encoded = match &mut encoder {
            Encoder::H264(enc) => {
                if want_keyframe {
                    enc.force_keyframe();
                }
                enc.encode(&frame)
            }
            Encoder::Tiles(enc) => {
                if want_keyframe {
                    enc.reset();
                }
                enc.encode(&frame, quality.jpeg_quality)
            }
        };
        want_keyframe = false;
        last_sent = Instant::now();

        if let Some(bytes) = encoded {
            if out.blocking_send(bytes).is_err() {
                return;
            }
        }

        // ---- cursor (quando a imagem não traz o cursor desenhado) ----
        if let (Some(cursor_fn), Some(src)) = (&cursor, source.as_ref()) {
            if !src.draws_cursor() {
                let d = src.display();
                let cur = cursor_fn().and_then(|(cx, cy)| {
                    let (rx_, ry_) = (cx - d.x, cy - d.y);
                    if rx_ < 0 || ry_ < 0 || rx_ >= d.width as i32 || ry_ >= d.height as i32 {
                        return None;
                    }
                    let px = (rx_ as f64 / d.width as f64 * frame.width as f64) as u16;
                    let py = (ry_ as f64 / d.height as f64 * frame.height as f64) as u16;
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

        last_encoded = Some(frame);
    }
}
