use super::*;

fn frame(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> RawFrame {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let [b, g, r] = f(x, y);
            let o = ((y * w + x) * 4) as usize;
            data[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    RawFrame::cpu(w, h, data, PixelFormat::Bgra, 1)
}

fn bgr_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.chunks_exact(4).zip(b.chunks_exact(4)).all(|(p, q)| p[..3] == q[..3])
}

/// "Interface" com poucas cores: faixas, texto falso (padrão fino) e um ícone.
fn ui(x: u32, y: u32, offset: u32) -> [u8; 3] {
    let yy = y + offset;
    if y < 40 {
        return [60, 60, 60]; // barra fixa no topo (não rola)
    }
    // "Texto": linhas de 14 px com glifos que variam por linha, separadas por 6 px em branco.
    if yy % 20 < 14 && (x / 50) % 3 != 0 && (x.wrapping_mul(31) ^ yy.wrapping_mul(17).wrapping_add(x / 9)) % 11 < 3 {
        return [20, 20, 20];
    }
    if (yy / 100) % 2 == 0 { [250, 250, 250] } else { [235, 240, 245] }
}

fn decode(frames: &[Vec<u8>]) -> (Vec<u8>, (u32, u32)) {
    let mut fb = Vec::new();
    let mut dims = (0, 0);
    for f in frames {
        apply_frame(f, &mut fb, &mut dims).expect("quadro válido");
    }
    (fb, dims)
}

#[test]
fn keyframe_is_lossless_and_exact() {
    let mut enc = LrdEncoder::new();
    let f = frame(300, 200, |x, y| ui(x, y, 0));
    let bytes = enc.encode(&f, 70).unwrap();
    assert!(enc.last.keyframe);
    let (fb, dims) = decode(&[bytes]);
    assert_eq!(dims, (300, 200));
    assert!(bgr_eq(&fb, &f.data));
}

#[test]
fn unchanged_frame_sends_nothing() {
    let mut enc = LrdEncoder::new();
    let f = frame(256, 128, |x, y| ui(x, y, 0));
    enc.encode(&f, 70).unwrap();
    assert!(enc.encode(&f, 70).is_none());
}

#[test]
fn small_change_sends_only_that_region() {
    let mut enc = LrdEncoder::new();
    let a = frame(640, 480, |x, y| ui(x, y, 0));
    let k = enc.encode(&a, 70).unwrap();
    let b = frame(640, 480, |x, y| if (300..310).contains(&x) && (200..210).contains(&y) { [0, 0, 255] } else { ui(x, y, 0) });
    let d = enc.encode(&b, 70).unwrap();
    assert_eq!(enc.last.dirty_tiles, 1);
    assert!(d.len() < k.len() / 10);
    let (fb, _) = decode(&[k, d]);
    assert!(bgr_eq(&fb, &b.data));
}

#[test]
fn solid_tiles_become_fill() {
    let mut enc = LrdEncoder::new();
    let f = frame(256, 256, |_, _| [10, 20, 30]);
    let bytes = enc.encode(&f, 70).unwrap();
    assert_eq!(enc.last.fills, 16);
    assert!(bytes.len() < 200);
    let (fb, _) = decode(&[bytes]);
    assert!(bgr_eq(&fb, &f.data));
}

#[test]
fn scroll_uses_copy_and_stays_exact() {
    let mut enc = LrdEncoder::new();
    let a = frame(800, 600, |x, y| ui(x, y, 0));
    let k = enc.encode(&a, 70).unwrap();
    // Conteúdo abaixo da barra sobe 37 linhas (rolagem para baixo).
    let b = frame(800, 600, |x, y| ui(x, y, 37));
    let d = enc.encode(&b, 70).unwrap();
    assert_eq!(enc.last.copies, 1, "deveria detectar a rolagem");
    let (fb, _) = decode(&[k.clone(), d.clone()]);
    assert!(bgr_eq(&fb, &b.data));
    // Comparado com mandar tudo de novo, a rolagem custa bem menos.
    let mut enc2 = LrdEncoder::new();
    enc2.encode(&a, 70).unwrap();
    enc2.force_keyframe();
    let full = enc2.encode(&b, 70).unwrap();
    assert!(d.len() * 2 < full.len(), "rolagem {} vs completo {}", d.len(), full.len());
    // Rolagem para cima também.
    let c = frame(800, 600, |x, y| ui(x, y, 11));
    let d2 = enc.encode(&c, 70).unwrap();
    assert_eq!(enc.last.copies, 1);
    let (fb, _) = decode(&[k, d, d2]);
    assert!(bgr_eq(&fb, &c.data));
}

fn noisy(x: u32, y: u32, t: u32) -> [u8; 3] {
    let v = x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503) ^ t.wrapping_mul(97);
    [(v >> 3) as u8, (v >> 11) as u8, (v >> 19) as u8]
}

#[test]
fn photo_goes_lossy_then_refines() {
    let mut enc = LrdEncoder::new();
    enc.refine_after = Duration::from_millis(0);
    let f = frame(256, 128, |x, y| if x < 128 { noisy(x, y, 0) } else { ui(x, y, 0) });
    let k = enc.encode(&f, 60).unwrap();
    assert!(enc.last.lossy >= 4);
    assert!(enc.encode(&f, 60).is_none()); // parou
    assert!(enc.refine_due());
    let r = enc.refine(2).unwrap();
    assert!(!enc.refine_due());
    let (fb, _) = decode(&[k, r]);
    // Fotos são refinadas com JPEG de alta qualidade (aqui o falso: 3 bits baixos zerados).
    for (p, q) in fb.chunks_exact(4).zip(f.data.chunks_exact(4)) {
        for c in 0..3 {
            assert!((p[c] as i32 - (q[c] & 0xF8) as i32).abs() <= 0 || p[c] == q[c]);
        }
    }
}

#[test]
fn moving_content_goes_lossy_then_refines_lossless() {
    let mut enc = LrdEncoder::new();
    enc.refine_after = Duration::from_millis(0);
    // Degradê com ~512 cores por bloco, mudando a cada quadro ("vídeo").
    let grad = |x: u32, y: u32, t: u32| [((x + t) % 256) as u8, ((y / 8) * 16 + t) as u8, 9];
    let mut frames = Vec::new();
    for t in 0..5 {
        let f = frame(128, 128, |x, y| grad(x, y, t));
        frames.push(enc.encode(&f, 60).unwrap());
    }
    assert!(enc.last.lossy > 0, "vídeo deveria ir com perdas: {:?}", enc.last);
    let last = frame(128, 128, |x, y| grad(x, y, 4));
    assert!(enc.encode(&last, 60).is_none());
    let r = enc.refine(9).unwrap();
    frames.push(r);
    let (fb, _) = decode(&frames);
    assert!(bgr_eq(&fb, &last.data), "depois do refino tem que ficar exato");
}

#[test]
fn rgba_input_and_odd_sizes() {
    let mut enc = LrdEncoder::new();
    let mut f = frame(130, 70, |x, y| ui(x, y, 0));
    for px in f.data.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    f.format = PixelFormat::Rgba;
    let k = enc.encode(&f, 70).unwrap();
    let (fb, _) = decode(&[k]);
    let expected = frame(130, 70, |x, y| ui(x, y, 0));
    assert!(bgr_eq(&fb, &expected.data));
}

#[test]
fn resolution_change_is_keyframe() {
    let mut enc = LrdEncoder::new();
    enc.encode(&frame(128, 128, |x, y| ui(x, y, 0)), 70).unwrap();
    enc.encode(&frame(192, 128, |x, y| ui(x, y, 0)), 70).unwrap();
    assert!(enc.last.keyframe);
}

/// Desempenho em 1920×1080 (rodar com `--ignored --nocapture`).
#[test]
#[ignore]
fn bench_1080p() {
    let mut enc = LrdEncoder::new();
    let a = frame(1920, 1080, |x, y| ui(x, y, 0));
    let t = Instant::now();
    let k = enc.encode(&a, 70).unwrap();
    println!("keyframe: {:?}, {} bytes", t.elapsed(), k.len());
    let b = frame(1920, 1080, |x, y| ui(x, y, 60));
    let t = Instant::now();
    let d = enc.encode(&b, 70).unwrap();
    println!("rolagem: {:?}, {} bytes, cópias {}", t.elapsed(), d.len(), enc.last.copies);
    let t = Instant::now();
    assert!(enc.encode(&b, 70).is_none());
    println!("parado: {:?}", t.elapsed());
    let c = frame(1920, 1080, |x, y| if (500..900).contains(&x) && (300..600).contains(&y) { noisy(x, y, 1) } else { ui(x, y, 60) });
    let t = Instant::now();
    let d = enc.encode(&c, 70).unwrap();
    println!("vídeo 400x300: {:?}, {} bytes", t.elapsed(), d.len());
}

#[test]
fn static_text_next_to_motion_stays_exact() {
    let mut enc = LrdEncoder::new();
    // Bloco 0..64: metade esquerda = "texto" parado; metade direita = vídeo mudando sempre.
    let f = |t: u32| frame(64, 64, move |x, y| if x < 32 { ui(x, y, 0) } else { noisy(x, y, t) });
    let mut frames = Vec::new();
    for t in 0..6 {
        frames.push(enc.encode(&f(t), 60).unwrap());
    }
    assert!(enc.last.lossy > 0);
    let (fb, _) = decode(&frames);
    let want = f(5);
    for y in 0..64usize {
        for x in 0..32usize {
            let o = (y * 64 + x) * 4;
            assert_eq!(fb[o..o + 3], want.data[o..o + 3], "texto mudou em ({x},{y})");
        }
    }
}
