//! Cenários sintéticos de uso de desktop (1920×1080) para medir o codecLRD.
//!
//! Conteúdo próximo do real: papel de parede "foto" (texturas suaves + granulação), janelas
//! de interface com texto, e vídeo com movimento contínuo.

use crate::frame::{PixelFormat, RawFrame};

pub const NAMES: [&str; 6] = ["parado", "digitando", "rolagem", "arrastar-janela", "video-720p", "video-tela-cheia"];

/// "Foto": gradientes suaves + ondas + granulação (≈ milhares de cores por bloco).
#[inline]
fn photo(x: u32, y: u32, t: u32) -> [u8; 3] {
    let fx = x as f32 * 0.013 + t as f32 * 0.09;
    let fy = y as f32 * 0.017 - t as f32 * 0.05;
    let n = (x.wrapping_mul(1_103_515_245) ^ y.wrapping_mul(12_345) ^ t.wrapping_mul(2_654_435_761)) >> 24;
    let grain = (n & 15) as f32 - 7.5;
    let r = 128.0 + 90.0 * (fx.sin() * fy.cos()) + grain;
    let g = 110.0 + 70.0 * ((fx * 0.7 + fy).sin()) + grain;
    let b = 100.0 + 80.0 * ((fy * 1.3 - fx * 0.4).cos()) + grain;
    [b.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, r.clamp(0.0, 255.0) as u8]
}

/// Janela de interface com texto (linhas de 16 px, glifos de 7 px).
#[inline]
fn window(x: u32, y: u32, scroll: u32) -> [u8; 3] {
    if y < 32 {
        return [235, 235, 235]; // barra de título
    }
    let yy = y + scroll;
    let line = yy / 18;
    let in_line = yy % 18;
    let col = x / 8;
    // Comprimento de cada linha varia; glifos com padrão determinístico.
    let len = 20 + (line.wrapping_mul(2_654_435_761) >> 26);
    if (2..14).contains(&in_line) && col > 1 && col < len {
        let g = (col.wrapping_mul(31) ^ line.wrapping_mul(17)) % 7;
        let px = x % 8;
        let on = match g {
            0 => px == 1 || px == 5 || in_line == 2 || in_line == 13,
            1 => px == 3,
            2 => in_line % 4 == 2 && px < 6,
            3 => px == 1 || (in_line == 7 && px < 6),
            4 => (px + in_line) % 5 == 0,
            5 => px == 5 || in_line == 13,
            _ => px < 6 && (in_line == 2 || in_line == 7),
        };
        if on {
            return [30, 30, 30];
        }
        if px == 0 || px == 6 {
            return [180, 180, 180]; // antisserrilhado
        }
    }
    [255, 255, 255]
}

/// Quadro `t` do cenário (BGRA, 1920×1080).
pub fn frame(name: &str, t: u32) -> RawFrame {
    let (w, h) = (1920u32, 1080u32);
    let mut data = vec![0u8; (w * h * 4) as usize];
    // Janela principal: (200,100)–(1400,900).
    let (wx, wy, ww, wh) = (200u32, 100u32, 1200u32, 800u32);
    let (wx, wy) = if name == "arrastar-janela" { (200 + (t * 15) % 500, 100 + (t * 7) % 150) } else { (wx, wy) };
    let scroll = if name == "rolagem" { t * 54 } else { 0 };
    for y in 0..h {
        for x in 0..w {
            let mut px = photo(x, y, 0); // papel de parede parado
            let in_win = x >= wx && x < wx + ww && y >= wy && y < wy + wh;
            if in_win {
                px = window(x - wx, y - wy, scroll);
            }
            match name {
                "digitando" => {
                    // Um caractere novo por quadro numa linha da janela.
                    let (cx, cy) = (wx + 24 + (t % 120) * 8, wy + 600);
                    if in_win && x >= wx + 24 && x < cx && (cy..cy + 14).contains(&y) && (x % 8) < 5 && (y % 3) != 0 {
                        px = [20, 20, 20];
                    }
                }
                "video-720p" => {
                    if (300..1580).contains(&x) && (200..920).contains(&y) {
                        px = photo(x, y, t + 1);
                    }
                }
                "video-tela-cheia" => px = photo(x, y, t + 1),
                _ => {}
            }
            let o = ((y * w + x) * 4) as usize;
            data[o..o + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    RawFrame::cpu(w, h, data, PixelFormat::Bgra, lrd_protocol::now_us())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lrd::LrdEncoder;

    /// Mede o codecLRD em cada cenário: `cargo test --release -p lrd-video bench_scenarios -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_scenarios() {
        println!(
            "cenário | ms total | comparar | rolagem | classificar | comprimir | KB/quadro | Mbps a 30 qps | LZ4 KB (Mpx) | JPEG KB (Mpx) | blocos alterados | cópias"
        );
        for name in NAMES {
            let frames: Vec<RawFrame> = (0..31).map(|t| frame(name, t)).collect();
            let mut enc = LrdEncoder::new();
            enc.encode(&frames[0], 72).unwrap(); // quadro completo inicial (não entra na média)
            let n = frames.len() - 1;
            let (mut tot, mut diff, mut scr, mut cls, mut cmp, mut bytes, mut lz4b, mut jpgb, mut lz4p, mut jpgp, mut dirty, mut copies) =
                (0u64, 0u64, 0u64, 0u64, 0u64, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
            for f in &frames[1..] {
                let t = std::time::Instant::now();
                let out = enc.encode(f, 72);
                let el = t.elapsed().as_micros() as u64;
                tot += el;
                if out.is_some() {
                    let l = enc.last;
                    diff += l.diff_us as u64;
                    scr += l.scroll_us as u64;
                    cls += l.classify_us as u64;
                    cmp += l.compress_us as u64;
                    bytes += l.bytes;
                    lz4b += l.lz4_bytes;
                    jpgb += l.jpeg_bytes;
                    lz4p += l.lz4_px;
                    jpgp += l.jpeg_px;
                    dirty += l.dirty_tiles;
                    copies += l.copies;
                }
            }
            let nf = n as f64;
            println!(
                "{name} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {:.1} | {:.0} ({:.2}) | {:.0} ({:.2}) | {:.0} | {}",
                tot as f64 / nf / 1000.0,
                diff as f64 / nf / 1000.0,
                scr as f64 / nf / 1000.0,
                cls as f64 / nf / 1000.0,
                cmp as f64 / nf / 1000.0,
                bytes as f64 / nf / 1024.0,
                bytes as f64 / nf * 8.0 * 30.0 / 1e6,
                lz4b as f64 / nf / 1024.0,
                lz4p as f64 / nf / 1e6,
                jpgb as f64 / nf / 1024.0,
                jpgp as f64 / nf / 1e6,
                dirty as f64 / nf,
                copies
            );
        }
        // Comparação dos codificadores JPEG (uma thread, 1920×1080 "foto").
        #[cfg(not(lrd_stub))]
        {
            let f = frame("video-tela-cheia", 3);
            let rgb: Vec<u8> = f.data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            for (name, func) in [
                ("jpeg-encoder (novo)", super::backend::jpeg_encode as fn(&[u8], u32, u32, u8) -> Option<Vec<u8>>),
                ("image (antigo)", super::backend::jpeg_encode_image_crate),
            ] {
                let t = std::time::Instant::now();
                let mut size = 0;
                for _ in 0..5 {
                    size = func(&rgb, 1920, 1080, 72).unwrap().len();
                }
                println!("JPEG {name}: {:.1} ms por quadro 1080p, {} KB", t.elapsed().as_secs_f64() * 1000.0 / 5.0, size / 1024);
            }
        }
        let t = std::time::Instant::now();
        let _ = frame("video-tela-cheia", 3);
        println!("(gerar um quadro sintético leva {:.1} ms — fora das medidas)", t.elapsed().as_secs_f64() * 1000.0);
    }
}
