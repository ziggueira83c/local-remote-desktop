//! Quadro bruto e redimensionamento.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba,
    Bgra,
}

/// Quadro de 4 bytes por pixel, sem preenchimento entre linhas.
#[derive(Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub format: PixelFormat,
    /// Momento da captura (`lrd_protocol::now_us`).
    pub timestamp_us: u64,
}

/// Tamanho de saída: cabe em `max_width`, mantém a proporção e usa dimensões pares
/// (exigência do H.264 4:2:0).
pub fn target_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let (w, h) = if max_width == 0 || width <= max_width {
        (width, height)
    } else {
        let nw = max_width;
        let nh = ((height as u64 * nw as u64 + width as u64 / 2) / width as u64) as u32;
        (nw, nh)
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Redimensiona para `(tw, th)`. Reduções grandes passam antes por médias 2×2 (evita
/// serrilhado em texto); o restante é bilinear em ponto fixo.
pub fn resize(mut frame: RawFrame, tw: u32, th: u32) -> RawFrame {
    if frame.width == tw && frame.height == th {
        return frame;
    }
    while frame.width >= tw * 2 && frame.height >= th * 2 && frame.width >= 4 && frame.height >= 4 {
        frame = halve(frame);
    }
    if frame.width == tw && frame.height == th {
        return frame;
    }
    if tw <= frame.width && th <= frame.height && (frame.width - tw) <= 1 && (frame.height - th) <= 1 {
        return crop(frame, tw, th);
    }
    bilinear(frame, tw, th)
}

fn crop(frame: RawFrame, tw: u32, th: u32) -> RawFrame {
    let src_stride = frame.width as usize * 4;
    let dst_stride = tw as usize * 4;
    let mut out = Vec::with_capacity(dst_stride * th as usize);
    for y in 0..th as usize {
        out.extend_from_slice(&frame.data[y * src_stride..y * src_stride + dst_stride]);
    }
    RawFrame { width: tw, height: th, data: out, ..frame }
}

fn halve(frame: RawFrame) -> RawFrame {
    let nw = frame.width / 2;
    let nh = frame.height / 2;
    let s = frame.width as usize * 4;
    let mut out = vec![0u8; nw as usize * nh as usize * 4];
    for y in 0..nh as usize {
        let r0 = &frame.data[(2 * y) * s..(2 * y) * s + s];
        let r1 = &frame.data[(2 * y + 1) * s..(2 * y + 1) * s + s];
        let o = &mut out[y * nw as usize * 4..(y + 1) * nw as usize * 4];
        for x in 0..nw as usize {
            let i = x * 8;
            for c in 0..4 {
                let sum = r0[i + c] as u16 + r0[i + 4 + c] as u16 + r1[i + c] as u16 + r1[i + 4 + c] as u16;
                o[x * 4 + c] = ((sum + 2) >> 2) as u8;
            }
        }
    }
    RawFrame { width: nw, height: nh, data: out, ..frame }
}

fn bilinear(frame: RawFrame, tw: u32, th: u32) -> RawFrame {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (tw_u, th_u) = (tw as usize, th as usize);
    let s = sw * 4;
    // Coordenadas de origem em ponto fixo 16.16 (centro do pixel).
    let xs: Vec<(usize, usize, u32)> = (0..tw_u)
        .map(|x| {
            let fx = ((x as f64 + 0.5) * sw as f64 / tw_u as f64 - 0.5).max(0.0);
            let x0 = (fx as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let w = ((fx - x0 as f64) * 256.0) as u32;
            (x0 * 4, x1 * 4, w.min(256))
        })
        .collect();
    let mut out = vec![0u8; tw_u * th_u * 4];
    for y in 0..th_u {
        let fy = ((y as f64 + 0.5) * sh as f64 / th_u as f64 - 0.5).max(0.0);
        let y0 = (fy as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (((fy - y0 as f64) * 256.0) as u32).min(256);
        let r0 = &frame.data[y0 * s..y0 * s + s];
        let r1 = &frame.data[y1 * s..y1 * s + s];
        let o = &mut out[y * tw_u * 4..(y + 1) * tw_u * 4];
        for (x, &(a, b, wx)) in xs.iter().enumerate() {
            for c in 0..4 {
                let top = r0[a + c] as u32 * (256 - wx) + r0[b + c] as u32 * wx;
                let bot = r1[a + c] as u32 * (256 - wx) + r1[b + c] as u32 * wx;
                o[x * 4 + c] = ((top * (256 - wy) + bot * wy + 32768) >> 16) as u8;
            }
        }
    }
    RawFrame { width: tw, height: th, data: out, ..frame }
}

/// Copia um buffer com `bytes_per_row` ≥ `width * 4` para um buffer contíguo.
pub fn pack_rows(src: &[u8], width: usize, height: usize, bytes_per_row: usize) -> Option<Vec<u8>> {
    let row = width * 4;
    if bytes_per_row < row || src.len() < bytes_per_row * (height.saturating_sub(1)) + row {
        return None;
    }
    if bytes_per_row == row {
        return Some(src[..row * height].to_vec());
    }
    let mut out = Vec::with_capacity(row * height);
    for y in 0..height {
        out.extend_from_slice(&src[y * bytes_per_row..y * bytes_per_row + row]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32) -> RawFrame {
        RawFrame { width: w, height: h, data: vec![100; (w * h * 4) as usize], format: PixelFormat::Rgba, timestamp_us: 0 }
    }

    #[test]
    fn sizes() {
        assert_eq!(target_size(2880, 1800, 1920), (1920, 1200));
        assert_eq!(target_size(1366, 768, 1920), (1366, 768));
        assert_eq!(target_size(1365, 767, 1920), (1364, 766));
    }

    #[test]
    fn resize_paths() {
        let f = resize(solid(2880, 1800), 1920, 1200);
        assert_eq!((f.width, f.height), (1920, 1200));
        assert!(f.data.iter().all(|&v| v == 100));
        let f = resize(solid(1365, 767), 1364, 766);
        assert_eq!(f.data.len(), 1364 * 766 * 4);
        let f = resize(solid(3840, 2160), 1280, 720);
        assert_eq!((f.width, f.height), (1280, 720));
    }
}
