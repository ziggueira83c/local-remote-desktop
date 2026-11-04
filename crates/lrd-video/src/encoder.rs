//! Configurações de qualidade e codificador de tiles JPEG (modo de compatibilidade).

use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;
use lrd_protocol::video::{build_tiles_frame, EncodedTile};

use crate::frame::{PixelFormat, RawFrame};

pub const TILE_SIZE: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualitySettings {
    pub jpeg_quality: u8,
    /// Largura máxima enviada; telas maiores são reduzidas mantendo a proporção.
    pub max_width: u32,
    pub max_fps: u32,
    /// Taxa alvo do H.264 em bits por segundo.
    pub bitrate_bps: u32,
}

impl QualitySettings {
    pub fn from_profile(profile: &str, max_fps: u32) -> Self {
        let fps = match max_fps {
            0 => 30,
            f => f.clamp(5, 60),
        };
        // A taxa cresce com o FPS: o mesmo nível de nitidez a 60 FPS precisa de mais bits.
        let fps_factor = (fps as f32 / 30.0).clamp(0.6, 1.6);
        let (q, w, mbps) = match profile {
            "speed" => (55, 1600, 3.0),
            "quality" => (88, 2560, 14.0),
            _ => (72, 1920, 7.0),
        };
        QualitySettings {
            jpeg_quality: q,
            max_width: w,
            max_fps: fps,
            bitrate_bps: (mbps * fps_factor * 1_000_000.0) as u32,
        }
    }
}

pub struct TileEncoder {
    prev: Option<RawFrame>,
    sequence: u32,
}

impl Default for TileEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TileEncoder {
    pub fn new() -> Self {
        TileEncoder { prev: None, sequence: 0 }
    }

    /// Esquece o quadro anterior: o próximo será completo (keyframe).
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Codifica só os tiles alterados. Retorna `None` quando nada mudou.
    pub fn encode(&mut self, frame: &RawFrame, quality: u8) -> Option<Vec<u8>> {
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            return None;
        }
        let keyframe = match &self.prev {
            Some(p) => p.width != w || p.height != h,
            None => true,
        };

        let mut rects = Vec::new();
        let mut ty = 0;
        while ty < h {
            let th = TILE_SIZE.min(h - ty);
            let mut tx = 0;
            while tx < w {
                let tw = TILE_SIZE.min(w - tx);
                if keyframe || tile_changed(self.prev.as_ref().unwrap(), frame, tx, ty, tw, th) {
                    rects.push((tx, ty, tw, th));
                }
                tx += TILE_SIZE;
            }
            ty += TILE_SIZE;
        }
        if rects.is_empty() {
            return None;
        }
        let tiles = encode_tiles_parallel(frame, &rects, quality);
        self.sequence = self.sequence.wrapping_add(1);
        let out = build_tiles_frame(w as u16, h as u16, keyframe, self.sequence, &tiles);
        self.prev = Some(frame.clone());
        Some(out)
    }
}

fn tile_changed(prev: &RawFrame, cur: &RawFrame, x: u32, y: u32, w: u32, h: u32) -> bool {
    let stride = cur.width as usize * 4;
    let start = x as usize * 4;
    let len = w as usize * 4;
    (y..y + h).any(|row| {
        let off = row as usize * stride + start;
        prev.data[off..off + len] != cur.data[off..off + len]
    })
}

fn encode_tile(frame: &RawFrame, x: u32, y: u32, w: u32, h: u32, quality: u8) -> Option<EncodedTile> {
    let stride = frame.width as usize * 4;
    let (r, b) = match frame.format {
        PixelFormat::Rgba => (0, 2),
        PixelFormat::Bgra => (2, 0),
    };
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for row in y..y + h {
        let off = row as usize * stride + x as usize * 4;
        for px in frame.data[off..off + w as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[px[r], px[1], px[b]]);
        }
    }
    let mut jpeg = Vec::with_capacity(rgb.len() / 8);
    let result = {
        let mut enc = JpegEncoder::new_with_quality(&mut jpeg, quality);
        enc.encode(&rgb, w, h, ExtendedColorType::Rgb8)
    };
    if let Err(e) = result {
        tracing::warn!("falha ao codificar tile: {e}");
        return None;
    }
    Some(EncodedTile { x: x as u16, y: y as u16, w: w as u16, h: h as u16, jpeg })
}

fn encode_tiles_parallel(frame: &RawFrame, rects: &[(u32, u32, u32, u32)], quality: u8) -> Vec<EncodedTile> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 8);
    if rects.len() < 8 || threads == 1 {
        return rects.iter().filter_map(|&(x, y, w, h)| encode_tile(frame, x, y, w, h, quality)).collect();
    }
    let chunk = rects.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = rects
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter_map(|&(x, y, w, h)| encode_tile(frame, x, y, w, h, quality))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lrd_protocol::video::validate_frame;

    fn solid(w: u32, h: u32, v: u8) -> RawFrame {
        RawFrame::cpu(w, h, vec![v; (w * h * 4) as usize], PixelFormat::Rgba, 0)
    }

    #[test]
    fn keyframe_then_delta_then_nothing() {
        let mut enc = TileEncoder::new();
        let f1 = enc.encode(&solid(300, 200, 10), 70).unwrap();
        assert!(validate_frame(&f1));
        assert_eq!(f1[1] & 1, 1);
        assert!(enc.encode(&solid(300, 200, 10), 70).is_none());
        let mut changed = solid(300, 200, 10);
        changed.data[0] = 200;
        let f3 = enc.encode(&changed, 70).unwrap();
        assert_eq!(u16::from_le_bytes([f3[6], f3[7]]), 1);
    }
}
