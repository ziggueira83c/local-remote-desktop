//! Codificador H.264 (OpenH264) ajustado para conteúdo de tela em tempo real.

use anyhow::{anyhow, Result};
use lrd_protocol::video::build_h264_frame;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer};
use openh264::OpenH264API;

use crate::encoder::QualitySettings;
use crate::frame::{PixelFormat, RawFrame};

pub struct H264Encoder {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    dims: (u32, u32),
    sequence: u32,
}

impl H264Encoder {
    pub fn new(q: &QualitySettings) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4) as u16;
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(q.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(q.max_fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .complexity(Complexity::Low)
            // Quadro completo periódico: recupera a imagem sozinho se algo se perder.
            .intra_frame_period(IntraFramePeriod::from_num_frames(q.max_fps.max(5) * 10))
            .num_threads(threads);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| anyhow!("iniciando H.264: {e}"))?;
        Ok(H264Encoder { encoder, yuv: None, dims: (0, 0), sequence: 0 })
    }

    pub fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Codifica um quadro (dimensões pares). Retorna o quadro de rede pronto.
    pub fn encode(&mut self, frame: &RawFrame) -> Option<Vec<u8>> {
        let (w, h) = (frame.width, frame.height);
        if w % 2 != 0 || h % 2 != 0 || w > 3840 || h > 2160 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            return None;
        }
        if self.dims != (w, h) || self.yuv.is_none() {
            self.yuv = Some(YUVBuffer::new(w as usize, h as usize));
            self.dims = (w, h);
            self.encoder.force_intra_frame();
        }
        let yuv = self.yuv.as_mut()?;
        let dims = (w as usize, h as usize);
        match frame.format {
            PixelFormat::Rgba => yuv.read_rgba8(RgbaSliceU8::new(&frame.data, dims)),
            PixelFormat::Bgra => yuv.read_bgra8(BgraSliceU8::new(&frame.data, dims)),
        }
        let stream = match self.encoder.encode(&*yuv) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("falha no H.264: {e}");
                return None;
            }
        };
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        let mut bitstream = Vec::new();
        stream.write_vec(&mut bitstream);
        if bitstream.is_empty() {
            return None;
        }
        self.sequence = self.sequence.wrapping_add(1);
        Some(build_h264_frame(w as u16, h as u16, keyframe, frame.timestamp_us, self.sequence, &bitstream))
    }
}
