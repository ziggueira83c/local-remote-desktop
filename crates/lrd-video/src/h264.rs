//! Codificador H.264 com escolha automática do motor:
//! 1. placa de vídeo — Media Foundation no Windows (NVENC, QuickSync, AMF) e VideoToolbox no
//!    macOS: 2–5 ms por quadro e quase nada de CPU;
//! 2. OpenH264 por software como reserva.
//! Se o motor de hardware falhar a qualquer momento, a sessão continua com o OpenH264.

use anyhow::{anyhow, Result};
use lrd_protocol::video::build_h264_frame;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer};
use openh264::OpenH264API;

use crate::encoder::QualitySettings;
use crate::frame::{PixelFormat, RawFrame};

/// Saída de um motor: bitstream Annex B e se é quadro completo (IDR).
pub struct Encoded {
    pub annexb: Vec<u8>,
    pub keyframe: bool,
}

pub trait Backend: Send {
    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<Option<Encoded>>;
    fn set_bitrate(&mut self, bps: u32);
    fn name(&self) -> &'static str;
}

pub struct H264Encoder {
    backend: Box<dyn Backend>,
    dims: (u32, u32),
    quality: QualitySettings,
    bitrate: u32,
    force_key: bool,
    sequence: u32,
    hardware_failed: bool,
}

impl H264Encoder {
    pub fn new(q: &QualitySettings) -> Result<Self> {
        // O motor de verdade é criado no primeiro quadro (precisa das dimensões).
        Ok(H264Encoder {
            backend: Box::new(NullBackend),
            dims: (0, 0),
            quality: *q,
            bitrate: q.bitrate_bps,
            force_key: true,
            sequence: 0,
            hardware_failed: false,
        })
    }

    pub fn name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        let bps = bps.clamp(300_000, (self.quality.bitrate_bps.max(300_000)) * 3);
        if bps != self.bitrate {
            self.bitrate = bps;
            self.backend.set_bitrate(bps);
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn open_backend(&mut self, w: u32, h: u32) {
        if !self.hardware_failed {
            match open_hardware(w, h, self.quality.max_fps, self.bitrate) {
                Ok(b) => {
                    tracing::info!("H.264 por hardware: {} ({w}x{h})", b.name());
                    self.backend = b;
                    return;
                }
                Err(e) => {
                    tracing::info!("H.264 por hardware indisponível ({e:#}); usando OpenH264");
                    self.hardware_failed = true;
                }
            }
        }
        match OpenH264Backend::new(&self.quality, self.bitrate) {
            Ok(b) => self.backend = Box::new(b),
            Err(e) => {
                tracing::error!("OpenH264 indisponível: {e:#}");
                self.backend = Box::new(NullBackend);
            }
        }
    }

    /// Codifica um quadro (dimensões pares) e devolve o quadro de rede pronto.
    pub fn encode(&mut self, frame: &RawFrame) -> Option<Vec<u8>> {
        let (w, h) = (frame.width, frame.height);
        if w % 2 != 0 || h % 2 != 0 || w > 4096 || h > 4096 {
            return None;
        }
        if self.dims != (w, h) {
            self.dims = (w, h);
            self.open_backend(w, h);
            self.force_key = true;
        }
        let force = std::mem::take(&mut self.force_key);
        let out = match self.backend.encode(frame, force) {
            Ok(o) => o,
            Err(e) => {
                if self.backend.name() != "OpenH264" {
                    tracing::warn!("falha no H.264 por hardware ({e:#}); trocando para OpenH264");
                    self.hardware_failed = true;
                    self.dims = (0, 0);
                    self.force_key = true;
                } else {
                    tracing::warn!("falha no OpenH264: {e:#}");
                }
                return None;
            }
        }?;
        if out.annexb.is_empty() {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        Some(build_h264_frame(w as u16, h as u16, out.keyframe, frame.timestamp_us, self.sequence, &out.annexb))
    }
}

struct NullBackend;

impl Backend for NullBackend {
    fn encode(&mut self, _: &RawFrame, _: bool) -> Result<Option<Encoded>> {
        Ok(None)
    }
    fn set_bitrate(&mut self, _: u32) {}
    fn name(&self) -> &'static str {
        "nenhum"
    }
}

#[cfg(target_os = "windows")]
fn open_hardware(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::hw_win::MfEncoder::new(w, h, fps, bitrate)?))
}

#[cfg(target_os = "macos")]
fn open_hardware(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::hw_mac::VtEncoder::new(w, h, fps, bitrate)?))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_hardware(_w: u32, _h: u32, _fps: u32, _bitrate: u32) -> Result<Box<dyn Backend>> {
    Err(anyhow!("sem codificador de hardware nesta plataforma"))
}

/// Verifica se o bitstream Annex B contém um SPS (NAL tipo 7).
pub(crate) fn has_sps(annexb: &[u8]) -> bool {
    annexb.windows(4).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1f) == 7)
}

// ---------------------------------------------------------------------------------------
// OpenH264 (software)

struct OpenH264Backend {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    dims: (u32, u32),
    target_bps: u32,
    applied: Option<(u32, (u32, u32))>,
}

impl OpenH264Backend {
    fn new(q: &QualitySettings, bitrate: u32) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4) as u16;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(q.max_fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .complexity(Complexity::Low)
            // Sem quadros completos periódicos: o controlador pede um (PLI) quando precisa.
            // Evita picos de tráfego regulares que causam engasgos.
            .intra_frame_period(IntraFramePeriod::from_num_frames(q.max_fps.max(5) * 600))
            .num_threads(threads);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| anyhow!("iniciando OpenH264: {e}"))?;
        Ok(OpenH264Backend { encoder, yuv: None, dims: (0, 0), target_bps: bitrate, applied: None })
    }

    fn apply_bitrate(&mut self) {
        if self.applied == Some((self.target_bps, self.dims)) {
            return;
        }
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: self.target_bps as std::os::raw::c_int,
        };
        // SAFETY: SetOption apenas lê a estrutura durante a chamada.
        let rc = unsafe {
            self.encoder
                .raw_api()
                .set_option(openh264_sys2::ENCODER_OPTION_BITRATE, (&raw mut info).cast::<std::ffi::c_void>())
        };
        if rc != 0 {
            tracing::debug!("SetOption(BITRATE) retornou {rc}");
        }
        self.applied = Some((self.target_bps, self.dims));
    }
}

impl Backend for OpenH264Backend {
    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<Option<Encoded>> {
        let (w, h) = (frame.width, frame.height);
        if self.dims != (w, h) || self.yuv.is_none() {
            self.yuv = Some(YUVBuffer::new(w as usize, h as usize));
            self.dims = (w, h);
        }
        let yuv = self.yuv.as_mut().ok_or_else(|| anyhow!("sem buffer YUV"))?;
        let dims = (w as usize, h as usize);
        match frame.format {
            PixelFormat::Rgba => yuv.read_rgba8(RgbaSliceU8::new(&frame.data, dims)),
            PixelFormat::Bgra => yuv.read_bgra8(BgraSliceU8::new(&frame.data, dims)),
        }
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let stream = self.encoder.encode(&*yuv).map_err(|e| anyhow!("{e}"))?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let mut annexb = Vec::new();
        stream.write_vec(&mut annexb);
        self.apply_bitrate();
        Ok(Some(Encoded { annexb, keyframe }))
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.target_bps = bps;
    }

    fn name(&self) -> &'static str {
        "OpenH264"
    }
}
