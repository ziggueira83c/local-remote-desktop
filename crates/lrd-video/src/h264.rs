//! Codificador de vídeo (H.264, ou H.265 quando os dois lados suportam) com escolha
//! automática do motor:
//! 1. placa de vídeo — Media Foundation no Windows (NVENC, QuickSync, AMF) e VideoToolbox no
//!    macOS: 2–5 ms por quadro e quase nada de CPU;
//! 2. OpenH264 por software como reserva.
//! Se o motor de hardware falhar a qualquer momento, a sessão continua com o OpenH264.

use anyhow::{anyhow, Result};
use lrd_protocol::video::build_video_frame;
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
    /// Área (x, y, w, h) que recebe mais qualidade no próximo quadro, com o delta de QP
    /// (negativo = melhor). Só alguns codificadores suportam; os outros ignoram.
    fn set_roi(&mut self, _roi: Option<(u32, u32, u32, u32, i32)>) {}
    fn name(&self) -> &'static str;
    /// Aceita quadros na GPU (`RawFrame::native`).
    fn accepts_native(&self) -> bool {
        false
    }
    /// Aceita quadros na memória.
    fn accepts_cpu(&self) -> bool {
        true
    }
    /// Produz H.265 em vez de H.264.
    fn hevc(&self) -> bool {
        false
    }
}

pub struct H264Encoder {
    backend: Box<dyn Backend>,
    dims: (u32, u32),
    quality: QualitySettings,
    bitrate: u32,
    force_key: bool,
    sequence: u32,
    hardware_failed: bool,
    /// H.265 desejado (o controlador decodifica); cai para H.264 se não houver hardware.
    want_hevc: bool,
    hevc_failed: bool,
    native_failed: bool,
    /// Dispositivo do modo GPU em uso (`None` = modo CPU).
    native_key: Option<usize>,
    roi: Option<(u32, u32, u32, u32, i32)>,
}

impl H264Encoder {
    pub fn new(q: &QualitySettings) -> Result<Self> {
        // O motor de verdade é criado no primeiro quadro (precisa das dimensões).
        Ok(H264Encoder {
            backend: Box::new(NullBackend),
            roi: None,
            dims: (0, 0),
            quality: *q,
            bitrate: q.bitrate_bps,
            force_key: true,
            sequence: 0,
            hardware_failed: false,
            want_hevc: false,
            hevc_failed: false,
            native_failed: false,
            native_key: None,
        })
    }

    pub fn set_hevc(&mut self, on: bool) {
        if on != self.want_hevc {
            self.want_hevc = on;
            self.hevc_failed = false;
            self.dims = (0, 0); // recria o motor no próximo quadro
        }
    }

    pub fn is_hevc(&self) -> bool {
        self.backend.hevc()
    }

    /// O motor atual lê direto da GPU.
    pub fn zero_copy(&self) -> bool {
        self.native_key.is_some()
    }

    /// Decide se `frame` (na GPU) pode ir direto ao codificador com saída `(tw, th)`;
    /// (re)abre o motor no modo GPU se preciso. `false` = use o caminho da CPU.
    pub fn prepare_native(&mut self, frame: &RawFrame, tw: u32, th: u32) -> bool {
        let Some(native) = &frame.native else { return false };
        if self.hardware_failed || self.native_failed {
            return false;
        }
        // O VideoToolbox não reduz a imagem: só usa o caminho direto no tamanho exato
        // (o ScreenCaptureKit já entrega reduzido).
        if cfg!(target_os = "macos") && (frame.width, frame.height) != (tw, th) {
            return false;
        }
        let key = native.device_key();
        if self.dims == (tw, th) && self.native_key == Some(key) && self.backend.accepts_native() {
            return true;
        }
        match self.open_native(tw, th, native) {
            Ok(b) => {
                tracing::info!("vídeo direto da GPU: {} ({tw}x{th}, H.265: {})", b.name(), b.hevc());
                self.backend = b;
                self.dims = (tw, th);
                self.native_key = Some(key);
                self.force_key = true;
                true
            }
            Err(e) => {
                tracing::info!("caminho direto da GPU indisponível ({e:#}); usando cópia");
                self.native_failed = true;
                false
            }
        }
    }

    fn open_native(&mut self, w: u32, h: u32, native: &crate::frame::NativeFrame) -> Result<Box<dyn Backend>> {
        let fps = self.quality.max_fps;
        if self.want_hevc && !self.hevc_failed {
            match open_hardware(w, h, fps, self.bitrate, true, Some(native)) {
                Ok(b) => return Ok(b),
                Err(e) => {
                    tracing::info!("H.265 por hardware indisponível ({e:#}); usando H.264");
                    self.hevc_failed = true;
                }
            }
        }
        open_hardware(w, h, fps, self.bitrate, false, Some(native))
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
        self.native_key = None;
        if !self.hardware_failed {
            let mut opened = None;
            if self.want_hevc && !self.hevc_failed {
                match open_hardware(w, h, self.quality.max_fps, self.bitrate, true, None) {
                    Ok(b) => opened = Some(b),
                    Err(e) => {
                        tracing::info!("H.265 por hardware indisponível ({e:#}); usando H.264");
                        self.hevc_failed = true;
                    }
                }
            }
            let opened = match opened {
                Some(b) => Ok(b),
                None => open_hardware(w, h, self.quality.max_fps, self.bitrate, false, None),
            };
            match opened {
                Ok(b) => {
                    tracing::info!("vídeo por hardware: {} ({w}x{h}, H.265: {})", b.name(), b.hevc());
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

    /// Codifica um quadro e devolve o quadro de rede pronto. `frame` vem da CPU no tamanho
    /// final (dimensões pares), ou da GPU depois de `prepare_native` devolver `true`.
    /// Ver `Backend::set_roi`; vale para os próximos quadros até mudar.
    pub fn set_roi(&mut self, roi: Option<(u32, u32, u32, u32, i32)>) {
        self.roi = roi;
    }

    pub fn encode(&mut self, frame: &RawFrame) -> Option<Vec<u8>> {
        let native = frame.native.is_some() && self.native_key.is_some() && self.backend.accepts_native();
        let (w, h) = if native { self.dims } else { (frame.width, frame.height) };
        if w % 2 != 0 || h % 2 != 0 || w > 4096 || h > 4096 {
            return None;
        }
        if !native && (self.dims != (w, h) || !self.backend.accepts_cpu() || self.native_key.is_some()) {
            self.dims = (w, h);
            self.open_backend(w, h);
            self.force_key = true;
        }
        let force = std::mem::take(&mut self.force_key);
        self.backend.set_roi(self.roi);
        let out = match self.backend.encode(frame, force) {
            Ok(o) => o,
            Err(e) => {
                if native {
                    tracing::warn!("falha no caminho direto da GPU ({e:#}); usando cópia");
                    self.native_failed = true;
                    self.native_key = None;
                    self.dims = (0, 0);
                    self.force_key = true;
                } else if self.backend.name() != "OpenH264" {
                    tracing::warn!("falha no codificador de hardware ({e:#}); trocando para OpenH264");
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
        Some(build_video_frame(w as u16, h as u16, out.keyframe, self.backend.hevc(), frame.timestamp_us, self.sequence, &out.annexb))
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
fn open_hardware(
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    hevc: bool,
    native: Option<&crate::frame::NativeFrame>,
) -> Result<Box<dyn Backend>> {
    let gpu = match native {
        Some(crate::frame::NativeFrame::D3d(f)) => Some(&f.gpu),
        _ => None,
    };
    Ok(Box::new(crate::hw_win::MfEncoder::new(w, h, fps, bitrate, hevc, gpu)?))
}

#[cfg(target_os = "macos")]
fn open_hardware(
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    hevc: bool,
    _native: Option<&crate::frame::NativeFrame>,
) -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::hw_mac::VtEncoder::new(w, h, fps, bitrate, hevc)?))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_hardware(
    _w: u32,
    _h: u32,
    _fps: u32,
    _bitrate: u32,
    _hevc: bool,
    _native: Option<&crate::frame::NativeFrame>,
) -> Result<Box<dyn Backend>> {
    Err(anyhow!("sem codificador de hardware nesta plataforma"))
}

/// Verifica se o bitstream Annex B traz os conjuntos de parâmetros (SPS: NAL 7 no H.264,
/// 33 no H.265).
#[allow(dead_code)]
pub(crate) fn has_param_sets(annexb: &[u8], hevc: bool) -> bool {
    annexb.windows(4).any(|w| {
        w[0] == 0 && w[1] == 0 && w[2] == 1 && if hevc { (w[3] >> 1) & 0x3f == 33 } else { (w[3] & 0x1f) == 7 }
    })
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
