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
    // A metade de interface (x >= 128) fica exata; a foto é refinada em JPEG de alta qualidade.
    for y in 0..128usize {
        for x in 128..256usize {
            let o = (y * 256 + x) * 4;
            assert_eq!(fb[o..o + 3], f.data[o..o + 3], "interface mudou em ({x},{y})");
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

#[test]
fn scroll_inside_window_with_static_sides_uses_copy() {
    // Janela em x 100..500 com o conteúdo rolando; fora dela, "papel de parede" parado.
    let page = |x: u32, y: u32, off: u32| {
        if (100..500).contains(&x) && y >= 40 { ui(x - 100, y, off) } else { noisy(x, y, 0) }
    };
    let mut enc = LrdEncoder::new();
    let a = frame(640, 480, |x, y| page(x, y, 0));
    let k = enc.encode(&a, 70).unwrap();
    let b = frame(640, 480, |x, y| page(x, y, 54));
    let d = enc.encode(&b, 70).unwrap();
    assert_eq!(enc.last.copies, 1, "rolagem dentro de janela não detectada");
    // Parou: o refino deixa a janela exata (pixels copiados de regiões com perdas inclusive).
    enc.refine_after = Duration::from_millis(0);
    assert!(enc.encode(&b, 70).is_none());
    let mut frames = vec![k, d];
    while let Some(r) = enc.refine(3) {
        frames.push(r);
    }
    let (fb, _) = decode(&frames);
    // Área da janela tem que ficar exata (sem perdas). Exceção: a faixa de 16 colunas (e a
    // linha de blocos) que divide com o papel de parede (foto) pode ficar em JPEG de alta
    // qualidade — x 100..112 e y 40..64.
    for y in 64..480usize {
        for x in 112..500usize {
            let o = (y * 640 + x) * 4;
            assert_eq!(fb[o..o + 3], b.data[o..o + 3], "pixel ({x},{y})");
        }
    }
}

#[test]
fn video_region_uses_few_large_operations() {
    // Vídeo 640×384 alinhado aos blocos, mudando a cada quadro, no meio de uma interface.
    let f = |t: u32| frame(1024, 512, move |x, y| if (128..768).contains(&x) && (64..448).contains(&y) { noisy(x, y, t) } else { ui(x, y, 0) });
    let mut enc = LrdEncoder::new();
    enc.encode(&f(0), 60).unwrap();
    let mut last = Vec::new();
    for t in 1..5 {
        last = enc.encode(&f(t), 60).unwrap();
    }
    let (_, ops) = lrd_protocol::lrd::parse(&last).unwrap();
    // 6 linhas de blocos ⇒ no máximo 2 retângulos (até 4 linhas de blocos por operação).
    assert!(ops.len() <= 2, "{} operações para um vídeo retangular", ops.len());
}

#[test]
fn unaligned_video_region_uses_few_operations() {
    // Vídeo fora do alinhamento dos blocos (x 100..740, y 40..424).
    let f = |t: u32| frame(1024, 512, move |x, y| if (100..740).contains(&x) && (40..424).contains(&y) { noisy(x, y, t) } else { ui(x, y, 0) });
    let mut enc = LrdEncoder::new();
    enc.encode(&f(0), 60).unwrap();
    let mut last = Vec::new();
    for t in 1..5 {
        last = enc.encode(&f(t), 60).unwrap();
    }
    let (_, ops) = lrd_protocol::lrd::parse(&last).unwrap();
    assert!(ops.len() <= 12, "{} operações para um vídeo retangular desalinhado", ops.len());
    // E continua exato onde não é vídeo.
    let mut frames = vec![];
    let mut enc2 = LrdEncoder::new();
    for t in 0..5 {
        frames.push(enc2.encode(&f(t), 60).unwrap());
    }
    let (fb, _) = decode(&frames);
    let want = f(4);
    for y in 0..512usize {
        for x in 0..1024usize {
            let inside = (96..752).contains(&x) && (0..448).contains(&y);
            if !inside {
                let o = (y * 1024 + x) * 4;
                assert_eq!(fb[o..o + 3], want.data[o..o + 3], "pixel ({x},{y})");
            }
        }
    }
}

// ---------- região de vídeo (Task 3) ----------

use super::region_encoder::{RegionEncoder, RegionOut};
use std::sync::{Arc as SArc, Mutex as SMutex};

#[derive(Default)]
struct StubLog {
    created: Vec<(u32, u32)>,
    idrs: usize,
}

struct StubRegion {
    first: bool,
    n: u32,
    log: SArc<SMutex<StubLog>>,
}

impl RegionEncoder for StubRegion {
    fn encode(&mut self, frame: &RawFrame, force_idr: bool) -> Option<RegionOut> {
        let idr = force_idr || self.first;
        self.first = false;
        self.n += 1;
        if idr {
            self.log.lock().unwrap().idrs += 1;
        }
        let mut annexb = vec![0, 0, 0, 1];
        annexb.extend_from_slice(&frame.width.to_le_bytes());
        annexb.extend_from_slice(&frame.height.to_le_bytes());
        annexb.extend_from_slice(&self.n.to_le_bytes());
        Some(RegionOut { annexb, idr, hevc: true })
    }
    fn set_bitrate(&mut self, _bps: u32) {}
}

fn stub_factory(log: SArc<SMutex<StubLog>>, fail: bool) -> super::RegionFactory {
    Box::new(move |w, h| {
        if fail {
            return None;
        }
        log.lock().unwrap().created.push((w, h));
        Some(Box::new(StubRegion { first: true, n: 0, log: log.clone() }) as Box<dyn RegionEncoder>)
    })
}

fn video_frame(t: u32, x1: u32) -> RawFrame {
    frame(1024, 512, move |x, y| if (128..x1).contains(&x) && (64..448).contains(&y) { noisy(x, y, t) } else { ui(x, y, 0) })
}

fn ops_of(bytes: &[u8]) -> Vec<lrd_protocol::lrd::Op<'_>> {
    lrd_protocol::lrd::parse(bytes).unwrap().1
}

fn count_ops(bytes: &[u8]) -> (usize, usize) {
    let ops = ops_of(bytes);
    let v = ops.iter().filter(|o| matches!(o, lrd_protocol::lrd::Op::Video { .. })).count();
    let j = ops.iter().filter(|o| matches!(o, lrd_protocol::lrd::Op::Jpeg { .. })).count();
    (v, j)
}

#[test]
fn video_region_replaces_jpeg_and_keeps_text_exact() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    let mut frames = Vec::new();
    for t in 0..12 {
        frames.push(enc.encode(&video_frame(t, 768), 60).unwrap());
    }
    for f in &frames[8..] {
        assert_eq!(count_ops(f), (1, 0), "esperado só a operação de vídeo");
    }
    assert_eq!(log.lock().unwrap().created.len(), 1);
    let (fb, _) = decode(&frames);
    let want = video_frame(11, 768);
    for y in 0..512usize {
        for x in 0..1024usize {
            if (128..768).contains(&x) && (64..448).contains(&y) {
                continue;
            }
            let o = (y * 1024 + x) * 4;
            assert_eq!(fb[o..o + 3], want.data[o..o + 3], "pixel ({x},{y})");
        }
    }
}

#[test]
fn region_resize_starts_with_idr() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    for t in 0..10 {
        enc.encode(&video_frame(t, 768), 60).unwrap();
    }
    let mut saw_resize = false;
    for t in 10..24 {
        let b = enc.encode(&video_frame(t, 960), 60).unwrap();
        for op in ops_of(&b) {
            if let lrd_protocol::lrd::Op::Video { w, flags, .. } = op {
                if w > 640 && !saw_resize {
                    assert!(flags & lrd_protocol::lrd::VIDEO_IDR != 0, "primeiro quadro do tamanho novo sem IDR");
                    saw_resize = true;
                }
            }
        }
    }
    assert!(saw_resize, "a região não cresceu");
    assert_eq!(log.lock().unwrap().created.len(), 2);
}

#[test]
fn keyframe_request_forces_idr_and_full_frame() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    for t in 0..10 {
        enc.encode(&video_frame(t, 768), 60).unwrap();
    }
    enc.force_keyframe();
    let b = enc.encode(&video_frame(10, 768), 60).unwrap();
    let (h, ops) = lrd_protocol::lrd::parse(&b).unwrap();
    assert!(h.keyframe);
    assert!(ops.iter().any(|o| matches!(o, lrd_protocol::lrd::Op::Video { flags, .. } if flags & lrd_protocol::lrd::VIDEO_IDR != 0)));
}

#[test]
fn released_area_returns_exact() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    let mut now = Instant::now();
    let mut frames = Vec::new();
    for t in 0..10 {
        now += Duration::from_millis(33);
        frames.extend(enc.encode_at(&video_frame(t, 768), 60, now));
    }
    // O vídeo para: a área vira interface parada. Passa 1,5 s.
    let still = frame(1024, 512, |x, y| ui(x, y, 0));
    for _ in 0..45 {
        now += Duration::from_millis(33);
        frames.extend(enc.encode_at(&still, 60, now));
    }
    let (fb, _) = decode(&frames);
    assert!(bgr_eq(&fb, &still.data), "a área liberada não voltou exata");
}

#[test]
fn encoder_failure_disables_video() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), true), 8_000_000);
    let mut last = Vec::new();
    for t in 0..12 {
        last = enc.encode(&video_frame(t, 768), 60).unwrap();
    }
    let (v, j) = count_ops(&last);
    assert_eq!(v, 0);
    assert!(j > 0, "a área deveria seguir em JPEG");
}
