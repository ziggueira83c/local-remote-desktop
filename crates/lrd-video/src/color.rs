//! Conversão RGBA/BGRA → NV12 (BT.601, faixa limitada), usada pelos codificadores de hardware
//! do Windows. Processa 2 linhas por vez (o plano UV é amostrado 2×2).

use crate::frame::{PixelFormat, RawFrame};

/// Retorna o buffer NV12 (Y com `width` bytes por linha, seguido de UV intercalado).
pub fn to_nv12(frame: &RawFrame) -> Vec<u8> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let (ri, bi) = match frame.format {
        PixelFormat::Rgba => (0usize, 2usize),
        PixelFormat::Bgra => (2, 0),
    };
    let mut out = vec![0u8; w * h + w * h / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let src = &frame.data;
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut su = 0i32;
            let mut sv = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let i = ((y + dy) * w + x + dx) * 4;
                    let r = src[i + ri] as i32;
                    let g = src[i + 1] as i32;
                    let b = src[i + bi] as i32;
                    y_plane[(y + dy) * w + x + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                    su += ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                    sv += ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                }
            }
            let o = (y / 2) * w + x;
            uv_plane[o] = (su / 4).clamp(0, 255) as u8;
            uv_plane[o + 1] = (sv / 4).clamp(0, 255) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_and_black() {
        let f = RawFrame { width: 2, height: 2, data: vec![255; 16], format: PixelFormat::Rgba, timestamp_us: 0 };
        let n = to_nv12(&f);
        assert_eq!(n.len(), 6);
        assert!(n[0] >= 234 && n[0] <= 236);
        assert!((127..=129).contains(&n[4]));
    }
}
