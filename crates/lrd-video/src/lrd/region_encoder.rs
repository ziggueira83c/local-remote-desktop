//! Codificador temporal da região de vídeo (H.265 por hardware; H.264 de reserva).

use crate::frame::RawFrame;

/// Unidade de acesso codificada da região.
pub struct RegionOut {
    pub annexb: Vec<u8>,
    pub idr: bool,
    pub hevc: bool,
}

pub trait RegionEncoder: Send {
    fn encode(&mut self, frame: &RawFrame, force_idr: bool) -> Option<RegionOut>;
    fn set_bitrate(&mut self, bps: u32);
    /// Área com mais qualidade no próximo quadro (texto em movimento); `None` = nenhuma.
    fn set_roi(&mut self, _roi: Option<(u32, u32, u32, u32)>) {}
}

/// Cria um codificador para uma região de (largura, altura); `None` = indisponível.
pub type RegionFactory = Box<dyn FnMut(u32, u32) -> Option<Box<dyn RegionEncoder>> + Send>;

/// Fábrica de produção: `H264Encoder` (NVENC/QuickSync/AMF, VideoToolbox, OpenH264) em modo
/// HEVC quando `hevc` — a própria fachada cai para H.264 se o hardware não tiver H.265.
#[cfg(not(lrd_stub))]
pub fn hardware_region_encoder(hevc: bool, fps: u32, bitrate_bps: u32) -> RegionFactory {
    use crate::h264::H264Encoder;
    use crate::QualitySettings;

    const TEXT_QP_DELTA: i32 = -8;

    struct Hw(H264Encoder);

    impl RegionEncoder for Hw {
        fn encode(&mut self, frame: &RawFrame, force_idr: bool) -> Option<RegionOut> {
            if force_idr {
                self.0.force_keyframe();
            }
            let f = self.0.encode(frame)?;
            if f.len() <= lrd_protocol::video::H264_HEADER_LEN {
                return None;
            }
            Some(RegionOut {
                idr: f[1] & lrd_protocol::video::FLAG_KEYFRAME != 0,
                hevc: f[1] & lrd_protocol::video::FLAG_HEVC != 0,
                annexb: f[lrd_protocol::video::H264_HEADER_LEN..].to_vec(),
            })
        }

        fn set_bitrate(&mut self, bps: u32) {
            self.0.set_bitrate(bps);
        }

        fn set_roi(&mut self, roi: Option<(u32, u32, u32, u32)>) {
            // QP 8 abaixo do resto: texto em movimento bem mais nítido, sem estourar a taxa.
            self.0.set_roi(roi.map(|(x, y, w, h)| (x, y, w, h, TEXT_QP_DELTA)));
        }
    }

    Box::new(move |w, h| {
        let q = QualitySettings { jpeg_quality: 80, max_width: w, max_fps: fps, bitrate_bps };
        match H264Encoder::new(&q) {
            Ok(mut e) => {
                e.set_hevc(hevc);
                Some(Box::new(Hw(e)) as Box<dyn RegionEncoder>)
            }
            Err(err) => {
                tracing::warn!("região de vídeo: codificador indisponível ({err:#})");
                None
            }
        }
    })
}
