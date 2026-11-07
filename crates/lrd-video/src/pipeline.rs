//! Thread de captura: fonte de quadros → redimensionamento → H.264 (ou tiles JPEG) → rede.
//!
//! - O quadro mais recente sempre vence: se a rede ou o codificador atrasarem, quadros
//!   intermediários são descartados ANTES da codificação (latência baixa e constante).
//! - Tela parada por ~350 ms → um quadro completo em alta qualidade (texto nítido).
//! - Banda baixa → a resolução cai antes de a imagem borrar.
//! - Estatísticas por etapa alimentam o painel de diagnóstico.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lrd_protocol::video::build_cursor_frame_kind;
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
    /// Nova taxa alvo do H.264 (controle de banda do WebRTC).
    SetBitrate(u32),
    /// Troca de codec (ex.: WebRTC falhou e o controlador só decodifica JPEG).
    SetCodec(VideoCodec),
    /// Usar H.265 quando houver codificador de hardware (o controlador decodifica).
    SetHevc(bool),
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    JpegTiles,
    /// codecLRD: codec próprio de tela (blocos sem perdas/JPEG/cópia de rolagem).
    Lrd,
}

/// Cursor do computador acessado: posição global (coordenadas do SO), formato e visibilidade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorInfo {
    pub x: i32,
    pub y: i32,
    /// Formato (ver `lrd_protocol::video::cursor_kind`).
    pub kind: u8,
    pub visible: bool,
}

pub type CursorFn = Box<dyn Fn() -> Option<CursorInfo> + Send>;

pub struct CaptureParams {
    pub display_id: Option<u32>,
    pub quality: QualitySettings,
    pub codec: VideoCodec,
    pub cursor: Option<CursorFn>,
    /// Destino dos blocos de áudio (`None` = sessão sem áudio).
    pub audio: Option<mpsc::Sender<Vec<u8>>>,
    pub audio_enabled: bool,
    /// codecLRD: último quadro aplicado no controlador (controle de fluxo).
    pub lrd_ack: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// codecLRD com região de vídeo: `Some(h265)` quando o controlador decodifica H.264
    /// (`lrd-video`), e H.265 também (`lrd-video-h265`).
    pub lrd_video: Option<bool>,
}

/// Números do pipeline, lidos a cada segundo para o painel de diagnóstico.
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    pub source: String,
    pub encoder: String,
    pub width: u32,
    pub height: u32,
    pub captured: u64,
    pub encoded: u64,
    pub encode_us: u64,
    pub bitrate_bps: u32,
}

pub struct CaptureHandle {
    pub commands: Sender<CaptureCmd>,
    pub stats: Arc<Mutex<PipelineStats>>,
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
    let stats = Arc::new(Mutex::new(PipelineStats::default()));
    let stats_t = stats.clone();
    let join = std::thread::Builder::new()
        .name("lrd-capture".into())
        .spawn(move || {
            let _power = crate::power::SessionPower::begin();
            run(params, rx, out, stats_t)
        })
        .expect("criando thread de captura");
    CaptureHandle { commands: tx, stats, join: Some(join) }
}

enum Encoder {
    H264(H264Encoder),
    Tiles(TileEncoder),
    Lrd(Box<crate::lrd::LrdEncoder>),
}

impl Encoder {
    fn name(&self) -> String {
        match self {
            Encoder::H264(e) => format!(
                "{} · {}{}",
                if e.is_hevc() { "H.265" } else { "H.264" },
                e.name(),
                if e.zero_copy() { " · direto da GPU" } else { "" }
            ),
            Encoder::Tiles(_) => "JPEG (tiles)".into(),
            Encoder::Lrd(e) => {
                let l = e.last;
                match l.video_rect {
                    Some((_, _, w, h)) => format!(
                        "codecLRD + vídeo {} {w}×{h} · {}/{} blocos",
                        if l.video_hevc { "H.265" } else { "H.264" },
                        l.dirty_tiles,
                        l.total_tiles
                    ),
                    None => format!("codecLRD · {}/{} blocos · {} cópia(s)", l.dirty_tiles, l.total_tiles, l.copies),
                }
            }
        }
    }
}

fn make_encoder(
    codec: VideoCodec,
    q: &QualitySettings,
    hevc: bool,
    ack: &Option<Arc<std::sync::atomic::AtomicU32>>,
    lrd_video: Option<bool>,
) -> Encoder {
    if codec == VideoCodec::Lrd {
        let mut e = crate::lrd::LrdEncoder::new();
        e.ack = ack.clone();
        if let Some(h265) = lrd_video {
            e.enable_video(
                crate::lrd::hardware_region_encoder(h265, q.max_fps, q.bitrate_bps),
                q.bitrate_bps,
            );
        }
        return Encoder::Lrd(Box::new(e));
    }
    if codec == VideoCodec::H264 {
        match H264Encoder::new(q) {
            Ok(mut e) => {
                e.set_hevc(hevc);
                return Encoder::H264(e);
            }
            Err(e) => tracing::error!("H.264 indisponível, usando JPEG: {e:#}"),
        }
    }
    Encoder::Tiles(TileEncoder::new())
}

/// Largura máxima efetiva: com pouca banda, menos pixels em vez de imagem borrada.
fn effective_max_width(q: &QualitySettings, bitrate: Option<u32>) -> u32 {
    match bitrate {
        Some(b) if b < 900_000 => q.max_width.min(960),
        Some(b) if b < 1_800_000 => q.max_width.min(1280),
        Some(b) if b < 3_000_000 => q.max_width.min(1600),
        _ => q.max_width,
    }
}

const IDLE_REFINE_AFTER: Duration = Duration::from_millis(350);
/// codecLRD: intervalo dos "quadros parados" sintéticos quando a captura para de entregar.
const LRD_IDLE_TICK: Duration = Duration::from_millis(100);

fn run(params: CaptureParams, rx: Receiver<CaptureCmd>, out: mpsc::Sender<Vec<u8>>, stats: Arc<Mutex<PipelineStats>>) {
    let CaptureParams { display_id, quality, codec, cursor, audio, audio_enabled, lrd_ack, lrd_video } = params;
    let mut codec = codec;
    let mut quality = quality;
    let mut bitrate_override: Option<u32> = None;
    let mut display_id = display_id;
    let mut audio_enabled = audio_enabled && audio.is_some();
    let mut hevc = false;
    let mut lrd_bytes: u64 = 0;
    let mut lrd_rate: u32 = 0;
    let mut lrd_rate_at = Instant::now();
    let mut encoder = make_encoder(codec, &quality, hevc, &lrd_ack, lrd_video);
    let mut source: Option<Source> = None;
    let mut win_audio: Option<crate::audio::AudioCapture> = None;

    let mut pending: Option<RawFrame> = None;
    let mut last_encoded: Option<RawFrame> = None;
    let mut want_keyframe = true;
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let mut last_change = Instant::now();
    let mut refined = true;
    let mut last_idle_tick = Instant::now();
    let mut last_cursor: Option<(u16, u16, u8, bool)> = None;
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
                        encoder = make_encoder(codec, &quality, hevc, &lrd_ack, lrd_video);
                        bitrate_override = None;
                        if cfg!(target_os = "macos") {
                            source = None; // o ScreenCaptureKit aplica tamanho e FPS
                        }
                        last_encoded = None;
                        want_keyframe = true;
                    }
                }
                Ok(CaptureCmd::Keyframe) => want_keyframe = true,
                Ok(CaptureCmd::SetBitrate(bps)) => {
                    let before = effective_max_width(&quality, bitrate_override);
                    bitrate_override = Some(bps);
                    if let Encoder::H264(enc) = &mut encoder {
                        enc.set_bitrate(bps);
                    }
                    if effective_max_width(&quality, bitrate_override) != before && cfg!(target_os = "macos") {
                        source = None;
                    }
                }
                Ok(CaptureCmd::SetCodec(c)) => {
                    if c != codec {
                        codec = c;
                        encoder = make_encoder(codec, &quality, hevc, &lrd_ack, lrd_video);
                        last_encoded = None;
                        want_keyframe = true;
                    }
                }
                Ok(CaptureCmd::SetHevc(on)) => {
                    if on != hevc {
                        hevc = on;
                        if let Encoder::H264(enc) = &mut encoder {
                            enc.set_hevc(on);
                        }
                        want_keyframe = true;
                    }
                }
                Ok(CaptureCmd::SetAudio(on)) => {
                    let on = on && audio.is_some();
                    if on != audio_enabled {
                        audio_enabled = on;
                        if cfg!(target_os = "macos") {
                            source = None;
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

        let max_width = effective_max_width(&quality, bitrate_override);

        // ---- fonte ----
        if source.is_none() {
            let audio_for_source = if audio_enabled { audio.clone() } else { None };
            let mut q = quality;
            q.max_width = max_width;
            match Source::open(display_id, &q, audio_for_source) {
                Ok(s) => {
                    tracing::info!("captura: {} ({}x{})", s.name(), s.display().width, s.display().height);
                    display_id = Some(s.display().id);
                    if let Ok(mut st) = stats.lock() {
                        st.source = s.name().to_string();
                    }
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
        } else if !refined {
            IDLE_REFINE_AFTER.saturating_sub(last_change.elapsed()).max(Duration::from_millis(1)).min(interval)
        } else {
            interval
        };

        match src.next_frame(wait) {
            Ok(Some(frame)) => {
                pending = Some(frame);
                failures = 0;
                last_change = Instant::now();
                refined = false;
                if let Ok(mut st) = stats.lock() {
                    st.captured += 1;
                }
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

        // codecLRD: controlador ainda aplicando quadros anteriores ⇒ espera (as mudanças
        // seguem acumuladas no próximo quadro; nada se perde).
        if let Encoder::Lrd(enc) = &mut encoder {
            if !want_keyframe && !enc.ready() {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
        }

        // codecLRD: blocos enviados com perdas que pararam de mudar voltam sem perdas.
        if pending.is_none() && !want_keyframe {
            if let Encoder::Lrd(enc) = &mut encoder {
                // Tela parada: a captura não entrega mais quadros, mas o codificador ainda
                // precisa "ver" a tela parada para refinar e desligar a região de vídeo.
                if last_change.elapsed() >= LRD_IDLE_TICK && last_idle_tick.elapsed() >= LRD_IDLE_TICK && enc.needs_idle_tick() {
                    last_idle_tick = Instant::now();
                    if let Some(bytes) = enc.idle_tick(quality.jpeg_quality, Instant::now()) {
                        lrd_bytes += bytes.len() as u64;
                        last_sent = Instant::now();
                        if out.blocking_send(bytes).is_err() {
                            return;
                        }
                        continue;
                    }
                }
                if enc.refine_due() {
                    if let Some(bytes) = enc.refine(lrd_protocol::now_us()) {
                        lrd_bytes += bytes.len() as u64;
                        last_sent = Instant::now();
                        if out.blocking_send(bytes).is_err() {
                            return;
                        }
                    }
                }
                continue;
            }
        }

        // Tela parada: refina com um quadro completo em alta qualidade.
        let mut refine_now = false;
        if pending.is_none() && !want_keyframe && !refined && last_change.elapsed() >= IDLE_REFINE_AFTER {
            refined = true;
            if last_encoded.is_some() && matches!(encoder, Encoder::H264(_)) {
                refine_now = true;
                want_keyframe = true;
            }
        }

        // ---- codificação ----
        let to_encode = match pending.take() {
            Some(raw) => Some(raw),
            None if want_keyframe => last_encoded.take(),
            None => None,
        };
        let Some(raw) = to_encode else { continue };
        let (tw, th) = target_size(raw.width, raw.height, max_width);
        // Quadro na GPU vai direto ao codificador de hardware (que reduz e converte lá mesmo);
        // senão, cópia para a memória + redução na CPU.
        let native_ok = match &mut encoder {
            Encoder::H264(e) => e.prepare_native(&raw, tw, th),
            Encoder::Tiles(_) | Encoder::Lrd(_) => false,
        };
        let frame = if native_ok { raw } else { resize(raw, tw, th) };

        let started = Instant::now();
        let encoded = match &mut encoder {
            Encoder::H264(enc) => {
                let normal = enc.bitrate();
                if refine_now {
                    enc.set_bitrate((normal.saturating_mul(3)).min(40_000_000));
                }
                if want_keyframe {
                    enc.force_keyframe();
                }
                let r = enc.encode(&frame);
                if refine_now {
                    enc.set_bitrate(normal);
                }
                r
            }
            Encoder::Tiles(enc) => {
                if want_keyframe {
                    enc.reset();
                }
                enc.encode(&frame, quality.jpeg_quality)
            }
            Encoder::Lrd(enc) => {
                if want_keyframe {
                    enc.force_keyframe();
                }
                let r = enc.encode(&frame, quality.jpeg_quality);
                if let Some(b) = &r {
                    lrd_bytes += b.len() as u64;
                }
                r
            }
        };
        let took = started.elapsed();
        want_keyframe = false;
        last_sent = Instant::now();

        if let Ok(mut st) = stats.lock() {
            st.encoder = encoder.name();
            st.width = tw;
            st.height = th;
            if encoded.is_some() {
                st.encoded += 1;
                st.encode_us += took.as_micros() as u64;
            }
            st.bitrate_bps = match &encoder {
                Encoder::H264(e) => e.bitrate(),
                Encoder::Tiles(_) => 0,
                Encoder::Lrd(_) => {
                    // Taxa real enviada (média do último segundo).
                    let dt = lrd_rate_at.elapsed().as_secs_f64();
                    if dt >= 1.0 {
                        lrd_rate = (lrd_bytes as f64 * 8.0 / dt) as u32;
                        lrd_bytes = 0;
                        lrd_rate_at = Instant::now();
                    }
                    lrd_rate
                }
            };
        }

        if let Some(bytes) = encoded {
            if out.blocking_send(bytes).is_err() {
                return;
            }
        }

        // ---- cursor: posição (modo só visualizar) e formato (cursor local no controlador) ----
        if let (Some(cursor_fn), Some(src)) = (&cursor, source.as_ref()) {
            let d = src.display();
            let cur = cursor_fn().map(|c| {
                let (rx_, ry_) = (c.x - d.x, c.y - d.y);
                let inside = rx_ >= 0 && ry_ >= 0 && rx_ < d.width as i32 && ry_ < d.height as i32;
                let px = (rx_.max(0) as f64 / d.width.max(1) as f64 * tw as f64) as u16;
                let py = (ry_.max(0) as f64 / d.height.max(1) as f64 * th as f64) as u16;
                (px, py, c.kind, c.visible && inside)
            });
            if cur != last_cursor {
                let msg = match cur {
                    Some((x, y, kind, visible)) => build_cursor_frame_kind(visible, x, y, kind),
                    None => build_cursor_frame_kind(false, 0, 0, 0),
                };
                if out.blocking_send(msg).is_err() {
                    return;
                }
                last_cursor = cur;
            }
        }

        last_encoded = Some(frame);
    }
}
