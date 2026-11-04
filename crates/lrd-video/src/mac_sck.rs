//! macOS: captura com ScreenCaptureKit. Um único stream entrega vídeo (BGRA, já reduzido
//! pela GPU) e áudio do sistema (float 32 bits), com o mesmo relógio.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use anyhow::{anyhow, Result};
use screencapturekit::cm::*;
use screencapturekit::prelude::*;
use tokio::sync::mpsc;

use crate::capture::Display;
use crate::encoder::QualitySettings;
use crate::frame::{pack_rows, target_size, PixelFormat as FrameFormat, RawFrame};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;

pub struct SckSource {
    stream: SCStream,
    rx: Receiver<RawFrame>,
    pub display: Display,
    pub audio: bool,
}

struct VideoHandler {
    tx: SyncSender<RawFrame>,
}

impl SCStreamOutputTrait for VideoHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }
        let ts = lrd_protocol::now_us();
        let Some(pixel_buffer) = sample.pixel_buffer() else { return };
        let (w, h, bpr) = (pixel_buffer.width(), pixel_buffer.height(), pixel_buffer.bytes_per_row());
        let Ok(guard) = pixel_buffer.lock_read_only() else { return };
        let Some(src) = (unsafe { guard.as_slice() }) else { return };
        let Some(data) = pack_rows(src, w, h, bpr) else { return };
        // Se o envio estiver ocupado, descarta: o próximo quadro traz a tela atualizada.
        let _ = self.tx.try_send(RawFrame {
            width: w as u32,
            height: h as u32,
            data,
            format: FrameFormat::Bgra,
            timestamp_us: ts,
        });
    }
}

struct AudioHandler {
    tx: mpsc::Sender<Vec<u8>>,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Audio) {
            return;
        }
        let now = lrd_protocol::now_us();
        let Ok(list) = sample.audio_buffer_list() else { return };
        let as_f32 = |bytes: &[u8]| -> Vec<f32> {
            bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
        };
        let pcm: Vec<i16> = match list.num_buffers() {
            0 => return,
            1 => {
                // Intercalado.
                let Some(buf) = list.get(0) else { return };
                as_f32(buf.data()).into_iter().map(to_i16).collect()
            }
            _ => {
                // Planar: um buffer por canal.
                let (Some(l), Some(r)) = (list.get(0), list.get(1)) else { return };
                let (l, r) = (as_f32(l.data()), as_f32(r.data()));
                let n = l.len().min(r.len());
                let mut out = Vec::with_capacity(n * 2);
                for i in 0..n {
                    out.push(to_i16(l[i]));
                    out.push(to_i16(r[i]));
                }
                out
            }
        };
        if pcm.is_empty() {
            return;
        }
        let frames = (pcm.len() / CHANNELS) as u64;
        let ts = now.saturating_sub(frames * 1_000_000 / SAMPLE_RATE as u64);
        let _ = self.tx.try_send(lrd_protocol::video::build_audio_frame(SAMPLE_RATE, CHANNELS as u8, ts, &pcm));
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

impl SckSource {
    pub fn start(display: &Display, q: &QualitySettings, audio: Option<mpsc::Sender<Vec<u8>>>) -> Result<Self> {
        let content = SCShareableContent::get().map_err(|e| anyhow!("ScreenCaptureKit: {e:?}"))?;
        let displays = content.displays();
        let sc_display = displays
            .iter()
            .find(|d| d.display_id() == display.id)
            .or_else(|| displays.first())
            .ok_or_else(|| anyhow!("nenhum monitor no ScreenCaptureKit"))?;

        let scale = display.scale_factor.max(1.0);
        let px_w = (display.width as f32 * scale).round() as u32;
        let px_h = (display.height as f32 * scale).round() as u32;
        let (tw, th) = target_size(px_w, px_h, q.max_width);

        let filter = SCContentFilter::create()
            .with_display(sc_display)
            .with_excluding_windows(&[])
            .build()
            .map_err(|e| anyhow!("filtro do ScreenCaptureKit: {e:?}"))?;

        let mut config = SCStreamConfiguration::new()
            .with_width(tw)
            .with_height(th)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(false) // o controlador desenha o cursor localmente
            .with_fps(q.max_fps)
            .with_queue_depth(5);
        let audio_on = audio.is_some();
        if audio_on {
            config = config
                .with_captures_audio(true)
                .with_sample_rate(SAMPLE_RATE as i32)
                .with_channel_count(CHANNELS as i32)
                .with_excludes_current_process_audio(true);
        }

        let (tx, rx) = sync_channel::<RawFrame>(2);
        let mut stream = SCStream::new(&filter, &config).map_err(|e| anyhow!("stream do ScreenCaptureKit: {e:?}"))?;
        stream
            .add_output_handler(VideoHandler { tx }, SCStreamOutputType::Screen)
            .map_err(|e| anyhow!("ScreenCaptureKit (vídeo): {e:?}"))?;
        if let Some(audio_tx) = audio {
            stream
                .add_output_handler(AudioHandler { tx: audio_tx }, SCStreamOutputType::Audio)
                .map_err(|e| anyhow!("ScreenCaptureKit (áudio): {e:?}"))?;
        }
        stream.start_capture().map_err(|e| anyhow!("iniciando ScreenCaptureKit: {e:?}"))?;
        tracing::info!("ScreenCaptureKit: {tw}x{th} @ {} fps, áudio: {audio_on}", q.max_fps);

        Ok(SckSource { stream, rx, display: display.clone(), audio: audio_on })
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>> {
        let first = match self.rx.recv_timeout(timeout) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("ScreenCaptureKit encerrado")),
        };
        let mut latest = first;
        while let Ok(f) = self.rx.try_recv() {
            latest = f;
        }
        Ok(Some(latest))
    }
}

impl Drop for SckSource {
    fn drop(&mut self) {
        let _ = self.stream.stop_capture();
    }
}
