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

// ---------- vídeo da tela inteira ----------

use super::region_encoder::{RegionEncoder, RegionOut};
use std::sync::{Arc as SArc, Mutex as SMutex};

#[derive(Default)]
struct StubLog {
    created: Vec<(u32, u32)>,
    idrs: usize,
    rois: Vec<Option<(u32, u32, u32, u32)>>,
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
    fn set_roi(&mut self, roi: Option<(u32, u32, u32, u32)>) {
        self.log.lock().unwrap().rois.push(roi);
    }
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
    let v = ops.iter().filter(|o| matches!(o, lrd_protocol::lrd::Op::VideoRects { .. })).count();
    let j = ops.iter().filter(|o| matches!(o, lrd_protocol::lrd::Op::Jpeg { .. })).count();
    (v, j)
}


fn video_rects(bytes: &[u8]) -> Vec<(u32, u32, u32, u32)> {
    ops_of(bytes)
        .into_iter()
        .filter_map(|o| match o {
            lrd_protocol::lrd::Op::VideoRects { x, y, w, h, rects, .. } => {
                assert_eq!((x, y), (0, 0));
                assert_eq!((w, h), (1024, 512), "o vídeo é sempre da tela inteira");
                Some(rects)
            }
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn motion_goes_through_full_screen_video_only_where_it_changed() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    let mut frames = Vec::new();
    for t in 0..6 {
        frames.push(enc.encode(&video_frame(t, 768), 60).unwrap());
    }
    // Um codificador só, do tamanho da tela, criado uma vez.
    assert_eq!(log.lock().unwrap().created, vec![(1024, 512)]);
    // Vídeo de verdade (muitas cores): sem área de qualidade de texto.
    assert!(log.lock().unwrap().rois.iter().all(|r| r.is_none()), "{:?}", log.lock().unwrap().rois);
    // Nenhum JPEG: o que se mexe vai pelo vídeo; os retângulos cobrem só a área do vídeo.
    for f in &frames[1..] {
        let (v, j) = count_ops(f);
        assert_eq!((v, j), (1, 0));
        for (x, y, w, h) in video_rects(f) {
            assert!(x >= 128 && x + w <= 768 && y >= 64 && y + h <= 448, "retângulo fora do vídeo: {:?}", (x, y, w, h));
        }
    }
    // O texto em volta continua exato.
    let (fb, _) = decode(&frames);
    let want = video_frame(5, 768);
    for y in 0..512usize {
        for x in 0..1024usize {
            if !((128..768).contains(&x) && (64..448).contains(&y)) {
                let o = (y * 1024 + x) * 4;
                assert_eq!(fb[o..o + 3], want.data[o..o + 3], "pixel ({x},{y})");
            }
        }
    }
}

#[test]
fn typing_text_stays_lossless_without_video() {
    // Interface parada com um cursor de texto piscando e letras aparecendo: sem vídeo, exato.
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    let text = |k: u32| {
        frame(1024, 512, move |x, y| {
            let caret = (k / 15) % 2 == 0 && (300 + k..302 + k).contains(&x) && (200..216).contains(&y);
            let typed = (300..300 + k).contains(&x) && (202..214).contains(&y) && (x + y) % 3 == 0;
            if caret || typed { [0, 0, 0] } else { ui(x, y, 0) }
        })
    };
    let mut frames = Vec::new();
    for k in 0..60 {
        if let Some(b) = enc.encode(&text(k), 60) {
            assert_eq!(count_ops(&b), (0, 0), "texto foi pelo vídeo/JPEG no quadro {k}");
            frames.push(b);
        }
    }
    let (fb, _) = decode(&frames);
    assert!(bgr_eq(&fb, &text(59).data));
}

#[test]
fn video_area_refines_lossless_when_it_stops() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    // Conteúdo com muitas cores em movimento que para (ex.: página com imagens rolando).
    let grad = |t: u32| frame(1024, 512, move |x, y| [((x + t * 7) % 256) as u8, ((y + t * 3) % 256) as u8, ((x ^ y) % 256) as u8]);
    let mut now = Instant::now();
    let mut frames = Vec::new();
    for t in 0..6 {
        now += Duration::from_millis(33);
        frames.extend(enc.encode_at(&grad(t), 60, now));
    }
    assert!(frames.iter().any(|f| count_ops(f).0 == 1), "deveria ter ido pelo vídeo");
    // Para: a captura não entrega mais nada; o refino reenvia a área sem perdas/JPEG alto.
    for _ in 0..20 {
        now += Duration::from_millis(50);
        frames.extend(enc.idle_tick(60, now));
        if enc.refine_due_at(now) {
            frames.extend(enc.refine_at(0, now));
        }
    }
    let (fb, _) = decode(&frames);
    let painted = fb.chunks_exact(4).filter(|p| p[..3] == [1, 2, 3]).count();
    assert_eq!(painted, 0, "{painted} pixels ainda só com o vídeo depois de parar");
}

#[test]
fn keyframe_request_forces_idr() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    for t in 0..4 {
        enc.encode(&video_frame(t, 768), 60);
    }
    let idrs = log.lock().unwrap().idrs;
    enc.force_keyframe();
    let b = enc.encode(&video_frame(9, 768), 60).unwrap();
    assert!(lrd_protocol::lrd::parse(&b).unwrap().0.keyframe);
    assert_eq!(log.lock().unwrap().idrs, idrs + 1);
}

#[test]
fn encoder_failure_falls_back_to_jpeg() {
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log, true), 8_000_000);
    let mut last = Vec::new();
    for t in 0..4 {
        last = enc.encode(&video_frame(t, 768), 60).unwrap();
    }
    let (v, j) = count_ops(&last);
    assert_eq!(v, 0);
    assert!(j > 0);
}

#[test]
fn screen_stops_without_new_frames_still_refines() {
    // A captura não entrega quadro nenhum depois que a tela para (Windows/Mac só entregam
    // quando algo muda). O texto que foi com perdas tem que voltar exato mesmo assim.
    let mut enc = LrdEncoder::new();
    let grad = |x: u32, y: u32, t: u32| [((x + t) % 256) as u8, ((y / 8) * 16 + t) as u8, 9];
    let mut now = Instant::now();
    let mut frames = Vec::new();
    for t in 0..5 {
        now += Duration::from_millis(33);
        frames.extend(enc.encode_at(&frame(128, 128, |x, y| grad(x, y, t)), 60, now));
    }
    assert!(enc.last.lossy > 0);
    let last = frame(128, 128, |x, y| grad(x, y, 4));
    for _ in 0..40 {
        now += Duration::from_millis(50);
        frames.extend(enc.idle_tick(60, now));
        if enc.refine_due_at(now) {
            frames.extend(enc.refine_at(0, now));
        }
    }
    let (fb, _) = decode(&frames);
    assert!(bgr_eq(&fb, &last.data), "texto parado ficou com perdas");
}

#[test]
fn dragging_a_big_window_goes_through_video() {
    // Janela de interface (poucas cores) de 512×320 arrastada: área grande mudando a cada quadro
    // ⇒ vídeo (não reenviar centenas de KB sem perdas por quadro); exata de novo ao parar.
    let log = SArc::new(SMutex::new(StubLog::default()));
    let mut enc = LrdEncoder::new();
    enc.enable_video(stub_factory(log.clone(), false), 8_000_000);
    let win = |dx: u32| {
        frame(1024, 512, move |x, y| {
            if (100 + dx..612 + dx).contains(&x) && (80..400).contains(&y) {
                ui(x - dx, y, 7)
            } else {
                [40, 40, 40]
            }
        })
    };
    let mut now = Instant::now();
    let mut frames = Vec::new();
    let mut video = 0;
    let mut big_lossless = 0;
    for t in 0..12 {
        now += Duration::from_millis(33);
        if let Some(b) = enc.encode_at(&win(t * 9), 60, now) {
            if t >= 3 {
                video += count_ops(&b).0;
                if enc.last.lz4_bytes > 20_000 {
                    big_lossless += 1;
                }
            }
            frames.push(b);
        }
    }
    assert!(video >= 8, "janela arrastada foi pelo vídeo só em {video} quadros");
    assert_eq!(big_lossless, 0, "quadros com muito LZ4 durante o arraste");
    // A janela é interface (texto): recebe mais qualidade no vídeo.
    let rois = log.lock().unwrap().rois.clone();
    let last_roi = rois.iter().rev().find_map(|r| *r).expect("sem área de qualidade para o texto");
    assert!(last_roi.0 <= 100 + 99 && last_roi.0 + last_roi.2 >= 612 + 99 - 64, "área {last_roi:?}");
    assert!(last_roi.1 <= 80 && last_roi.1 + last_roi.3 >= 400, "área {last_roi:?}");
    for _ in 0..20 {
        now += Duration::from_millis(50);
        frames.extend(enc.idle_tick(60, now));
        if enc.refine_due_at(now) {
            frames.extend(enc.refine_at(0, now));
        }
    }
    let (fb, _) = decode(&frames);
    assert!(bgr_eq(&fb, &win(11 * 9).data), "janela não ficou exata depois de parar");
}
