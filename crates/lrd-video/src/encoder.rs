//! Codificador de tiles JPEG com detecção de alterações.

use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;
use lrd_protocol::video::{build_tiles_frame, EncodedTile};

use crate::capture::RawFrame;

pub const TILE_SIZE: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualitySettings {
    pub jpeg_quality: u8,
    /// Largura máxima enviada; telas maiores são reduzidas por um fator inteiro (2x, 3x...).
    pub max_width: u32,
    pub max_fps: u32,
}

impl QualitySettings {
    pub fn from_profile(profile: &str, max_fps: u32) -> Self {
        let fps = match max_fps {
            0 => 30,
            f => f.clamp(5, 60),
        };
        match profile {
            "speed" => QualitySettings { jpeg_quality: 50, max_width: 1440, max_fps: fps },
            "quality" => QualitySettings { jpeg_quality: 88, max_width: 3840, max_fps: fps },
            _ => QualitySettings { jpeg_quality: 72, max_width: 1920, max_fps: fps },
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

    /// Codifica um quadro. Retorna `None` quando nada mudou.
    pub fn encode(&mut self, frame: RawFrame, q: &QualitySettings) -> Option<Vec<u8>> {
        let frame = downscale(frame, q.max_width);
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
                let changed = keyframe || tile_changed(self.prev.as_ref().unwrap(), &frame, tx, ty, tw, th);
                if changed {
                    rects.push((tx, ty, tw, th));
                }
                tx += TILE_SIZE;
            }
            ty += TILE_SIZE;
        }

        if rects.is_empty() {
            self.prev = Some(frame);
            return None;
        }

        let tiles = encode_tiles_parallel(&frame, &rects, q.jpeg_quality);
        self.sequence = self.sequence.wrapping_add(1);
        let out = build_tiles_frame(w as u16, h as u16, keyframe, self.sequence, &tiles);
        self.prev = Some(frame);
        Some(out)
    }
}

fn tile_changed(prev: &RawFrame, cur: &RawFrame, x: u32, y: u32, w: u32, h: u32) -> bool {
    let stride = cur.width as usize * 4;
    let start = x as usize * 4;
    let len = w as usize * 4;
    for row in y..y + h {
        let off = row as usize * stride + start;
        if prev.rgba[off..off + len] != cur.rgba[off..off + len] {
            return true;
        }
    }
    false
}

fn encode_tile(frame: &RawFrame, x: u32, y: u32, w: u32, h: u32, quality: u8) -> Option<EncodedTile> {
    let stride = frame.width as usize * 4;
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for row in y..y + h {
        let off = row as usize * stride + x as usize * 4;
        for px in frame.rgba[off..off + w as usize * 4].chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
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

/// Reduz por um fator inteiro (média de blocos f×f) até caber em `max_width`.
pub fn downscale(frame: RawFrame, max_width: u32) -> RawFrame {
    if max_width == 0 || frame.width <= max_width {
        return frame;
    }
    let f = frame.width.div_ceil(max_width).max(2);
    let nw = frame.width / f;
    let nh = frame.height / f;
    if nw == 0 || nh == 0 {
        return frame;
    }
    let src_stride = frame.width as usize * 4;
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let area = f * f;
    for oy in 0..nh {
        for ox in 0..nw {
            let mut acc = [0u32; 4];
            for dy in 0..f {
                let row = (oy * f + dy) as usize * src_stride;
                for dx in 0..f {
                    let i = row + ((ox * f + dx) as usize) * 4;
                    acc[0] += frame.rgba[i] as u32;
                    acc[1] += frame.rgba[i + 1] as u32;
                    acc[2] += frame.rgba[i + 2] as u32;
                    acc[3] += frame.rgba[i + 3] as u32;
                }
            }
            let o = ((oy * nw + ox) * 4) as usize;
            out[o] = (acc[0] / area) as u8;
            out[o + 1] = (acc[1] / area) as u8;
            out[o + 2] = (acc[2] / area) as u8;
            out[o + 3] = (acc[3] / area) as u8;
        }
    }
    RawFrame { width: nw, height: nh, rgba: out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lrd_protocol::video::validate_frame;

    fn solid(w: u32, h: u32, v: u8) -> RawFrame {
        RawFrame { width: w, height: h, rgba: vec![v; (w * h * 4) as usize] }
    }

    #[test]
    fn keyframe_then_delta_then_nothing() {
        let mut enc = TileEncoder::new();
        let q = QualitySettings::from_profile("balanced", 30);
        let f1 = enc.encode(solid(300, 200, 10), &q).unwrap();
        assert!(validate_frame(&f1));
        assert_eq!(f1[1] & 1, 1);
        assert!(enc.encode(solid(300, 200, 10), &q).is_none());
        let mut changed = solid(300, 200, 10);
        changed.rgba[0] = 200;
        let f3 = enc.encode(changed, &q).unwrap();
        assert_eq!(u16::from_le_bytes([f3[6], f3[7]]), 1);
    }

    #[test]
    fn downscale_halves() {
        let f = downscale(solid(4000, 2000, 50), 1920);
        assert_eq!((f.width, f.height), (1333, 666));
    }
}

/// Dimensões resultantes de `downscale` sem processar pixels.
pub fn downscale_dims(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    if max_width == 0 || width <= max_width {
        return (width, height);
    }
    let f = width.div_ceil(max_width).max(2);
    let (nw, nh) = (width / f, height / f);
    if nw == 0 || nh == 0 {
        (width, height)
    } else {
        (nw, nh)
    }
}
