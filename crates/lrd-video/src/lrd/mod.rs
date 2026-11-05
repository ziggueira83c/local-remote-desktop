//! **codecLRD** — codec próprio para telas de computador (formato em `lrd_protocol::lrd`).
//!
//! Etapas de cada quadro:
//! 1. **O que mudou:** compara cada bloco de 64×64 com o estado que o controlador já tem
//!    (`reference`). Blocos iguais não custam nada.
//! 2. **Rolagem:** se uma faixa grande mudou, procura um deslocamento vertical (hash das
//!    linhas + votação). Achando, manda um `COPY` (a imagem já está lá, só muda de lugar) e
//!    só as linhas novas.
//! 3. **Classificação** de cada bloco alterado pela quantidade de cores e pelo histórico:
//!    - 1 cor → `FILL`;
//!    - até 1024 cores, ou parado → sem perdas (`LZ4` + predição): texto e interface nítidos;
//!    - muitas cores mudando sem parar (vídeo) ou foto → `JPEG`.
//! 4. **Refino:** bloco enviado em JPEG que parou de mudar é reenviado sem perdas
//!    (ou JPEG quase perfeito, se for foto), em pequenas levas.
//! 5. Compressão em paralelo (vários núcleos).

#[cfg(not(lrd_stub))]
mod backend;
#[cfg(lrd_stub)]
#[path = "backend_stub.rs"]
mod backend;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lrd_protocol::lrd::{Writer, FILTER_LEFT};

use crate::frame::{PixelFormat, RawFrame};

pub const TILE: u32 = 64;
/// Bloco com mais cores que isso é tratado como foto.
const PHOTO_COLORS: u32 = 1024;
/// Bloco com mais cores que isso, mudando em quadros seguidos, é tratado como vídeo.
const MOTION_COLORS: u32 = 160;
/// Quadros seguidos mudando para considerar "movimento".
const MOTION_STREAK: u8 = 3;
/// Quadros enviados e ainda não aplicados no controlador, no máximo.
const MAX_IN_FLIGHT: u32 = 3;

/// Faixa de 16 colunas com mais cores que isso vai com perdas (dentro de um bloco com perdas).
const STRIP_COLORS: u32 = 96;

/// Bits das faixas de 16 colunas do bloco que começa em `tx` que cruzam `x..x+w`.
fn full_strips(tx: u32, x: u32, w: u32) -> u8 {
    let mut m = 0;
    for st in 0..4u32 {
        let (a, b) = (tx + st * 16, tx + st * 16 + 16);
        if b > x && a < x + w {
            m |= 1 << st;
        }
    }
    m
}

/// Blocos refinados por quadro de refino (limita o tamanho dos quadros de refino).
const REFINE_BATCH: usize = 160;

#[derive(Debug, Clone, Copy, Default)]
struct TileState {
    /// Quadros seguidos em que o bloco mudou.
    streak: u8,
    /// Enviado com perdas (precisa de refino quando parar).
    lossy: bool,
    /// Foto (refino em JPEG de alta qualidade em vez de sem perdas).
    photo: bool,
    lossy_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Clean,
    Fill([u8; 3]),
    Lossless,
    /// `mask`: faixas de 16 colunas do bloco que vão com perdas (bit s = faixa s); as demais
    /// (texto, interface ao lado de um vídeo) vão sem perdas.
    Lossy { photo: bool, mask: u8 },
}

/// (x, y, largura, altura)
type Rect = (u32, u32, u32, u32);

#[derive(Debug, Clone, Copy)]
enum JobKind {
    Lossless,
    Jpeg(u8),
}

#[derive(Debug, Clone, Copy)]
struct Job {
    kind: JobKind,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

enum Out {
    Fill { x: u32, y: u32, w: u32, h: u32, bgr: [u8; 3] },
    Copy { x: u32, y: u32, w: u32, h: u32, sy: u32 },
    Lz4 { x: u32, y: u32, w: u32, h: u32, raw_len: usize, data: Vec<u8> },
    Jpeg { x: u32, y: u32, w: u32, h: u32, data: Vec<u8> },
}

/// Números do último quadro (diagnóstico).
#[derive(Debug, Clone, Copy, Default)]
pub struct LrdStats {
    pub bytes: usize,
    pub dirty_tiles: usize,
    pub total_tiles: usize,
    pub copies: usize,
    pub lossless: usize,
    pub lossy: usize,
    pub fills: usize,
    pub keyframe: bool,
    /// Tempos por etapa (µs): comparação, rolagem, classificação, compressão, total.
    pub diff_us: u32,
    pub scroll_us: u32,
    pub classify_us: u32,
    pub compress_us: u32,
    pub total_us: u32,
    /// Bytes por tipo de operação.
    pub lz4_bytes: usize,
    pub jpeg_bytes: usize,
    /// Pixels por tipo (área codificada).
    pub lz4_px: usize,
    pub jpeg_px: usize,
}

pub struct LrdEncoder {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    /// Estado que o controlador tem (BGRA, mesmos pixels que foram enviados).
    reference: Vec<u8>,
    tiles: Vec<TileState>,
    frame_id: u32,
    force_key: bool,
    workers: usize,
    /// Tempo parado até refinar um bloco enviado com perdas.
    pub refine_after: Duration,
    /// Último `frame_id` aplicado pelo controlador (`u32::MAX` = sem confirmações: não limita).
    pub ack: Option<Arc<AtomicU32>>,
    blocked_since: Option<Instant>,
    pub last: LrdStats,
}

impl Default for LrdEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LrdEncoder {
    pub fn new() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        LrdEncoder {
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            reference: Vec::new(),
            tiles: Vec::new(),
            frame_id: 0,
            force_key: true,
            workers: (cpus / 2).clamp(1, 6),
            refine_after: Duration::from_millis(350),
            ack: None,
            blocked_since: None,
            last: LrdStats::default(),
        }
    }

    /// Pode mandar mais um quadro? Com confirmações do controlador, no máximo
    /// `MAX_IN_FLIGHT` quadros sem confirmação: se a tela de lá não acompanha, as mudanças se
    /// acumulam aqui (num quadro só, mais tarde) em vez de virar fila e atraso.
    pub fn ready(&mut self) -> bool {
        let Some(ack) = &self.ack else { return true };
        let acked = ack.load(Ordering::Relaxed);
        if acked == u32::MAX || self.reference.is_empty() {
            return true;
        }
        let in_flight = self.frame_id.wrapping_sub(acked.wrapping_add(1));
        if in_flight < MAX_IN_FLIGHT || in_flight > 1 << 30 {
            self.blocked_since = None;
            return true;
        }
        // Confirmação perdida (ou controlador travado): não espera para sempre.
        let since = *self.blocked_since.get_or_insert_with(Instant::now);
        if since.elapsed() > Duration::from_millis(1000) {
            self.blocked_since = None;
            return true;
        }
        false
    }

    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    fn reset(&mut self, w: u32, h: u32) {
        self.width = w;
        self.height = h;
        self.cols = w.div_ceil(TILE);
        self.rows = h.div_ceil(TILE);
        self.reference = vec![0; (w * h * 4) as usize];
        self.tiles = vec![TileState::default(); (self.cols * self.rows) as usize];
    }

    fn tile_rect(&self, i: usize) -> (u32, u32, u32, u32) {
        let (c, r) = (i as u32 % self.cols, i as u32 / self.cols);
        let (x, y) = (c * TILE, r * TILE);
        (x, y, TILE.min(self.width - x), TILE.min(self.height - y))
    }

    /// Codifica um quadro. `None` = nada mudou (nenhum byte a enviar).
    pub fn encode(&mut self, frame: &RawFrame, jpeg_quality: u8) -> Option<Vec<u8>> {
        let t_start = Instant::now();
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 || frame.data.len() < (w * h * 4) as usize || w > 8192 || h > 8192 {
            return None;
        }
        let cur: Cow<[u8]> = match frame.format {
            PixelFormat::Bgra => Cow::Borrowed(&frame.data[..(w * h * 4) as usize]),
            PixelFormat::Rgba => {
                let mut v = frame.data[..(w * h * 4) as usize].to_vec();
                for px in v.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                Cow::Owned(v)
            }
        };
        let cur: &[u8] = &cur;
        let key = self.force_key || w != self.width || h != self.height || self.reference.is_empty();
        if key {
            self.reset(w, h);
        }
        let n = self.tiles.len();
        let mut stats = LrdStats { total_tiles: n, keyframe: key, ..Default::default() };
        let mut outs: Vec<Out> = Vec::new();

        // 1. O que mudou: por bloco, o retângulo exato das mudanças (o resto do bloco, que o
        //    controlador já tem certo, não é reenviado — texto parado ao lado de algo que se
        //    mexe continua exato).
        let mut dirty: Vec<Option<Rect>> =
            if key { (0..n).map(|i| Some(self.tile_rect(i))).collect() } else { (0..n).map(|i| self.change_box(cur, i)).collect() };
        stats.diff_us = t_start.elapsed().as_micros() as u32;
        let t_scroll = Instant::now();

        // 2. Rolagem.
        if !key && dirty.iter().filter(|d| d.is_some()).count() >= 4 {
            if let Some((x0, x1, y0, y1)) = self.dirty_bbox(&dirty) {
                if y1 - y0 >= 2 * TILE {
                    if let Some((sy, dy, len)) = detect_scroll(&self.reference, cur, w, x0, x1, y0, y1) {
                        outs.push(Out::Copy { x: x0, y: dy, w: x1 - x0, h: len, sy });
                        apply_copy(&mut self.reference, w, x0, x1 - x0, sy, dy, len);
                        self.propagate_lossy(x0, x1, sy, dy, len);
                        stats.copies += 1;
                        for (i, d) in dirty.iter_mut().enumerate() {
                            let (_, ty, _, th) = self.tile_rect(i);
                            if ty + th > dy && ty < dy + len {
                                *d = self.change_box(cur, i);
                            }
                        }
                    }
                }
            }
        }

        stats.scroll_us = t_scroll.elapsed().as_micros() as u32;
        let t_classify = Instant::now();
        // 3. Classificação (em paralelo por faixas de linhas).
        for (i, d) in dirty.iter().enumerate() {
            let t = &mut self.tiles[i];
            t.streak = if d.is_some() { t.streak.saturating_add(1) } else { 0 };
        }
        let classes = self.classify(cur, &dirty, key);
        stats.classify_us = t_classify.elapsed().as_micros() as u32;

        // 4. Agrupa blocos vizinhos inteiros da mesma classe em faixas horizontais; blocos com
        //    mudança parcial vão sozinhos, só com o retângulo alterado.
        let full = |i: usize| dirty[i] == Some(self.tile_rect(i));
        let mut jobs: Vec<Job> = Vec::new();
        for r in 0..self.rows {
            let mut c = 0;
            while c < self.cols {
                let i = (r * self.cols + c) as usize;
                let class = classes[i];
                let mut end = c + 1;
                let mixed = matches!(class, Class::Lossy { mask, .. } if mask != 0b1111);
                if full(i) && !mixed {
                    while end < self.cols
                        && classes[(r * self.cols + end) as usize] == class
                        && full((r * self.cols + end) as usize)
                    {
                        end += 1;
                    }
                }
                let (x, y, run_w, th) = match dirty[i] {
                    Some((bx, by, bw, bh)) if !full(i) => (bx, by, bw, bh),
                    _ => {
                        let (x, y, _, th) = self.tile_rect(i);
                        let (lx, _, lw, _) = self.tile_rect((r * self.cols + end - 1) as usize);
                        (x, y, lx + lw - x, th)
                    }
                };
                match class {
                    Class::Clean => {}
                    Class::Fill(bgr) => {
                        outs.push(Out::Fill { x, y, w: run_w, h: th, bgr });
                        stats.fills += (end - c) as usize;
                    }
                    Class::Lossless => {
                        jobs.push(Job { kind: JobKind::Lossless, x, y, w: run_w, h: th });
                        stats.lossless += (end - c) as usize;
                    }
                    Class::Lossy { mask, .. } if mask == 0b1111 => {
                        jobs.push(Job { kind: JobKind::Jpeg(jpeg_quality), x, y, w: run_w, h: th });
                        stats.lossy += (end - c) as usize;
                    }
                    Class::Lossy { mask, .. } => {
                        // Bloco misto: faixas de 16 colunas, juntando vizinhas do mesmo tipo.
                        let (tx, _, _, _) = self.tile_rect(i);
                        let mut s0 = 0u32;
                        while s0 < 4 {
                            let lossy = mask & (1 << s0) != 0;
                            let mut s1 = s0 + 1;
                            while s1 < 4 && (mask & (1 << s1) != 0) == lossy {
                                s1 += 1;
                            }
                            let a = (tx + s0 * 16).max(x);
                            let b = (tx + s1 * 16).min(x + run_w);
                            if b > a {
                                let kind = if lossy { JobKind::Jpeg(jpeg_quality) } else { JobKind::Lossless };
                                jobs.push(Job { kind, x: a, y, w: b - a, h: th });
                            }
                            s0 = s1;
                        }
                        stats.lossy += 1;
                    }
                }
                c = end;
            }
        }

        let jobs = merge_vertical(jobs);

        // 5. Compressão em paralelo.
        let t_compress = Instant::now();
        outs.extend(self.run_jobs(cur, &jobs));
        stats.compress_us = t_compress.elapsed().as_micros() as u32;
        for o in &outs {
            match o {
                Out::Lz4 { w, h, data, .. } => {
                    stats.lz4_bytes += data.len();
                    stats.lz4_px += (*w * *h) as usize;
                }
                Out::Jpeg { w, h, data, .. } => {
                    stats.jpeg_bytes += data.len();
                    stats.jpeg_px += (*w * *h) as usize;
                }
                _ => {}
            }
        }

        // Estado: o controlador passa a ter estes pixels.
        let now = Instant::now();
        for (i, class) in classes.iter().enumerate() {
            if *class == Class::Clean {
                continue;
            }
            stats.dirty_tiles += 1;
            let Some((x, y, bw, bh)) = dirty[i] else { continue };
            let whole = (x, y, bw, bh) == self.tile_rect(i);
            copy_rect(cur, &mut self.reference, w, x, y, bw, bh);
            let t = &mut self.tiles[i];
            match class {
                Class::Lossy { photo, .. } => {
                    t.lossy = true;
                    t.photo = *photo || (!whole && t.photo);
                    t.lossy_at = Some(now);
                }
                // Parte do bloco sem perdas: o resto pode continuar com perdas de antes.
                _ if !whole => {}
                _ => {
                    t.lossy = false;
                    t.photo = false;
                    t.lossy_at = None;
                }
            }
        }
        self.force_key = false;
        if outs.is_empty() && !key {
            self.last = stats;
            return None;
        }
        let bytes = self.write(key, frame.timestamp_us, outs);
        stats.bytes = bytes.len();
        stats.total_us = t_start.elapsed().as_micros() as u32;
        self.last = stats;
        Some(bytes)
    }

    /// Há blocos com perdas parados há tempo suficiente para refinar?
    pub fn refine_due(&self) -> bool {
        self.tiles.iter().any(|t| t.lossy && t.streak == 0 && t.lossy_at.is_some_and(|a| a.elapsed() >= self.refine_after))
    }

    /// Reenvia sem perdas (ou JPEG quase perfeito, para fotos) os blocos parados.
    pub fn refine(&mut self, capture_us: u64) -> Option<Vec<u8>> {
        if self.reference.is_empty() {
            return None;
        }
        let mut chosen = 0usize;
        let mut mark = vec![None::<bool>; self.tiles.len()]; // Some(photo)
        for (i, t) in self.tiles.iter().enumerate() {
            if chosen >= REFINE_BATCH {
                break;
            }
            if t.lossy && t.streak == 0 && t.lossy_at.is_some_and(|a| a.elapsed() >= self.refine_after) {
                mark[i] = Some(t.photo);
                chosen += 1;
            }
        }
        if chosen == 0 {
            return None;
        }
        // Blocos de foto: só as faixas de 16 colunas com muitas cores vão em JPEG de alta
        // qualidade; faixas de texto/interface do mesmo bloco vão sem perdas.
        let mut set = ColorSet::new();
        let mut jobs = Vec::new();
        for r in 0..self.rows {
            let mut c = 0;
            while c < self.cols {
                let i = (r * self.cols + c) as usize;
                let Some(photo) = mark[i] else {
                    c += 1;
                    continue;
                };
                if photo {
                    let (tx, ty, tw, th) = self.tile_rect(i);
                    let mut s0 = 0u32;
                    while s0 * 16 < tw {
                        let strip_lossy = |set: &mut ColorSet, st: u32| {
                            let a = tx + st * 16;
                            let b = (a + 16).min(tx + tw);
                            set.count(&self.reference, self.width, a, ty, b - a, th, STRIP_COLORS + 1).0 > STRIP_COLORS
                        };
                        let lossy = strip_lossy(&mut set, s0);
                        let mut s1 = s0 + 1;
                        while s1 * 16 < tw && strip_lossy(&mut set, s1) == lossy {
                            s1 += 1;
                        }
                        let a = tx + s0 * 16;
                        let b = (tx + s1 * 16).min(tx + tw);
                        let kind = if lossy { JobKind::Jpeg(92) } else { JobKind::Lossless };
                        jobs.push(Job { kind, x: a, y: ty, w: b - a, h: th });
                        s0 = s1;
                    }
                    c += 1;
                    continue;
                }
                let mut end = c + 1;
                while end < self.cols && mark[(r * self.cols + end) as usize] == Some(false) {
                    end += 1;
                }
                let (x, y, _, th) = self.tile_rect(i);
                let (lx, _, lw, _) = self.tile_rect((r * self.cols + end - 1) as usize);
                jobs.push(Job { kind: JobKind::Lossless, x, y, w: lx + lw - x, h: th });
                c = end;
            }
        }
        let jobs = merge_vertical(jobs);
        let reference = std::mem::take(&mut self.reference);
        let outs = self.run_jobs(&reference, &jobs);
        self.reference = reference;
        for (i, m) in mark.iter().enumerate() {
            if m.is_some() {
                let t = &mut self.tiles[i];
                t.lossy = false;
                t.lossy_at = None;
            }
        }
        let bytes = self.write(false, capture_us, outs);
        self.last = LrdStats { bytes: bytes.len(), dirty_tiles: chosen, total_tiles: self.tiles.len(), ..Default::default() };
        Some(bytes)
    }

    fn write(&mut self, key: bool, capture_us: u64, outs: Vec<Out>) -> Vec<u8> {
        let mut wr = Writer::new(key, self.width, self.height, capture_us, self.frame_id);
        self.frame_id = self.frame_id.wrapping_add(1);
        // COPY primeiro (as demais regiões são escritas por cima do resultado da cópia).
        for o in outs.iter() {
            if let Out::Copy { x, y, w, h, sy } = o {
                wr.copy(*x, *y, *w, *h, *x, *sy);
            }
        }
        for o in outs {
            match o {
                Out::Copy { .. } => {}
                Out::Fill { x, y, w, h, bgr } => wr.fill(x, y, w, h, bgr),
                Out::Lz4 { x, y, w, h, raw_len, data } => wr.lz4(x, y, w, h, FILTER_LEFT, raw_len, &data),
                Out::Jpeg { x, y, w, h, data } => wr.jpeg(x, y, w, h, &data),
            }
        }
        wr.finish()
    }

    /// Retângulo (x, y, w, h) dos pixels do bloco `i` diferentes do estado do controlador.
    fn change_box(&self, cur: &[u8], i: usize) -> Option<Rect> {
        let (x, y, tw, th) = self.tile_rect(i);
        let stride = (self.width * 4) as usize;
        let (a, b) = ((x * 4) as usize, ((x + tw) * 4) as usize);
        let (mut y0, mut y1, mut c0, mut c1) = (u32::MAX, 0u32, tw, 0u32);
        for r in y..y + th {
            let o = r as usize * stride;
            let (rc, rr) = (&cur[o + a..o + b], &self.reference[o + a..o + b]);
            if rc == rr {
                continue;
            }
            y0 = y0.min(r);
            y1 = r + 1;
            // Colunas: primeira e última diferentes nesta linha.
            let first = rc.chunks_exact(4).zip(rr.chunks_exact(4)).position(|(p, q)| p[..3] != q[..3]);
            let last = rc.chunks_exact(4).zip(rr.chunks_exact(4)).rposition(|(p, q)| p[..3] != q[..3]);
            if let (Some(f), Some(l)) = (first, last) {
                c0 = c0.min(f as u32);
                c1 = c1.max(l as u32 + 1);
            }
        }
        if y0 == u32::MAX || c1 <= c0 {
            return None;
        }
        Some((x + c0, y0, c1 - c0, y1 - y0))
    }

    /// Uma cópia leva junto pixels que podem ter ido com perdas: o bloco de destino passa a
    /// precisar de refino se algum bloco de origem estava com perdas.
    fn propagate_lossy(&mut self, x0: u32, x1: u32, sy: u32, dy: u32, len: u32) {
        let (c0, c1) = (x0 / TILE, (x1 - 1) / TILE);
        let (sr0, sr1) = (sy / TILE, (sy + len - 1) / TILE);
        let (dr0, dr1) = (dy / TILE, (dy + len - 1) / TILE);
        let now = Instant::now();
        for c in c0..=c1 {
            let src_lossy = (sr0..=sr1).any(|r| self.tiles[(r * self.cols + c) as usize].lossy);
            if !src_lossy {
                continue;
            }
            let photo = (sr0..=sr1).any(|r| self.tiles[(r * self.cols + c) as usize].photo);
            for r in dr0..=dr1 {
                let t = &mut self.tiles[(r * self.cols + c) as usize];
                t.lossy = true;
                t.photo |= photo;
                t.lossy_at = Some(now);
            }
        }
    }

    /// Limites (x0, x1, y0, y1) dos pixels alterados. As colunas vêm dos retângulos exatos
    /// das mudanças — não dos blocos — para que colunas paradas ao lado de uma janela rolando
    /// não entrem na comparação das linhas (senão nenhuma linha "bate" e a rolagem some).
    fn dirty_bbox(&self, dirty: &[Option<Rect>]) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
        for d in dirty.iter().flatten() {
            let (x, y, bw, bh) = *d;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + bw);
            y1 = y1.max(y + bh);
        }
        (x0 < x1).then_some((x0, x1, y0, y1))
    }

    fn classify(&self, cur: &[u8], dirty: &[Option<Rect>], key: bool) -> Vec<Class> {
        let n = self.tiles.len();
        let out: Vec<Mutex<Class>> = (0..n).map(|_| Mutex::new(Class::Clean)).collect();
        let next = AtomicUsize::new(0);
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        std::thread::scope(|s| {
            for _ in 0..self.workers {
                s.spawn(|| {
                    let mut set = ColorSet::new();
                    loop {
                        let r = next.fetch_add(1, Ordering::Relaxed);
                        if r >= rows {
                            break;
                        }
                        for c in 0..cols {
                            let i = r * cols + c;
                            let Some((x, y, tw, th)) = dirty[i] else { continue };
                            let (colors, first) = set.count(cur, self.width, x, y, tw, th, PHOTO_COLORS + 1);
                            let streak = self.tiles[i].streak;
                            let lossy = if colors == 1 {
                                None
                            } else if colors > PHOTO_COLORS {
                                Some(true)
                            } else if !key && colors > MOTION_COLORS && streak >= MOTION_STREAK {
                                Some(false)
                            } else {
                                None
                            };
                            let class = match lossy {
                                None if colors == 1 => Class::Fill(first),
                                None => Class::Lossless,
                                Some(photo) => {
                                    // Quais faixas de 16 colunas têm muitas cores.
                                    let (tx, _, _, _) = self.tile_rect(i);
                                    let mut mask = 0u8;
                                    for st in 0..4u32 {
                                        let a = (tx + st * 16).max(x);
                                        let b = (tx + st * 16 + 16).min(x + tw);
                                        if b <= a {
                                            continue;
                                        }
                                        let (sc, _) = set.count(cur, self.width, a, y, b - a, th, STRIP_COLORS + 1);
                                        if sc > STRIP_COLORS {
                                            mask |= 1 << st;
                                        }
                                    }
                                    // Faixas fora do retângulo alterado contam como "com perdas"
                                    // para não quebrar a junção de blocos inteiros.
                                    for st in 0..4u32 {
                                        let (a, b) = (tx + st * 16, tx + st * 16 + 16);
                                        if b <= x || a >= x + tw {
                                            mask |= 1 << st;
                                        }
                                    }
                                    if mask & full_strips(tx, x, tw) == 0 {
                                        Class::Lossless
                                    } else {
                                        Class::Lossy { photo, mask }
                                    }
                                }
                            };
                            *out[i].lock().unwrap() = class;
                        }
                    }
                });
            }
        });
        out.into_iter().map(|m| m.into_inner().unwrap()).collect()
    }

    fn run_jobs(&self, src: &[u8], jobs: &[Job]) -> Vec<Out> {
        let results: Vec<Mutex<Option<Out>>> = (0..jobs.len()).map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let width = self.width;
        std::thread::scope(|s| {
            for _ in 0..self.workers.min(jobs.len().max(1)) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    let j = jobs[i];
                    let out = match j.kind {
                        JobKind::Lossless => {
                            let raw = extract_bgr_filtered(src, width, j.x, j.y, j.w, j.h);
                            let data = backend::lz4_compress(&raw);
                            Some(Out::Lz4 { x: j.x, y: j.y, w: j.w, h: j.h, raw_len: raw.len(), data })
                        }
                        JobKind::Jpeg(q) => {
                            let rgb = extract_rgb(src, width, j.x, j.y, j.w, j.h);
                            match backend::jpeg_encode(&rgb, j.w, j.h, q) {
                                Some(data) => Some(Out::Jpeg { x: j.x, y: j.y, w: j.w, h: j.h, data }),
                                None => {
                                    // Sem JPEG: manda sem perdas.
                                    let raw = extract_bgr_filtered(src, width, j.x, j.y, j.w, j.h);
                                    let data = backend::lz4_compress(&raw);
                                    Some(Out::Lz4 { x: j.x, y: j.y, w: j.w, h: j.h, raw_len: raw.len(), data })
                                }
                            }
                        }
                    };
                    *results[i].lock().unwrap() = out;
                });
            }
        });
        results.into_iter().filter_map(|m| m.into_inner().unwrap()).collect()
    }
}

/// Linhas de blocos juntadas numa operação, no máximo (mantém a compressão em paralelo).
const MERGE_ROWS: u32 = 4;

/// Junta retângulos do mesmo tipo empilhados exatamente (mesmo x e largura, um logo abaixo
/// do outro) em retângulos maiores: menos operações para o controlador decodificar.
fn merge_vertical(jobs: Vec<Job>) -> Vec<Job> {
    let mut jobs = merge_horizontal(jobs);
    if jobs.len() < 2 {
        return jobs;
    }
    let key = |j: &Job| -> (u8, u8, u32, u32, u32) {
        let (k, q) = match j.kind {
            JobKind::Lossless => (0, 0),
            JobKind::Jpeg(q) => (1, q),
        };
        (k, q, j.x, j.w, j.y)
    };
    jobs.sort_by_key(key);
    let mut out: Vec<Job> = Vec::with_capacity(jobs.len());
    for j in jobs {
        if let Some(last) = out.last_mut() {
            let same_kind = key(last).0 == key(&j).0 && key(last).1 == key(&j).1;
            if same_kind && last.x == j.x && last.w == j.w && last.y + last.h == j.y && last.h + j.h <= MERGE_ROWS * TILE {
                last.h += j.h;
                continue;
            }
        }
        out.push(j);
    }
    out
}

/// Junta retângulos do mesmo tipo lado a lado (mesmo y e altura, encostados): as bordas
/// de cima/baixo de uma região que não começa no alinhamento dos blocos viram uma faixa só.
fn merge_horizontal(mut jobs: Vec<Job>) -> Vec<Job> {
    if jobs.len() < 2 {
        return jobs;
    }
    let kind = |j: &Job| -> (u8, u8) {
        match j.kind {
            JobKind::Lossless => (0, 0),
            JobKind::Jpeg(q) => (1, q),
        }
    };
    jobs.sort_by_key(|j| (kind(j), j.y, j.h, j.x));
    let mut out: Vec<Job> = Vec::with_capacity(jobs.len());
    for j in jobs {
        if let Some(last) = out.last_mut() {
            if kind(last) == kind(&j) && last.y == j.y && last.h == j.h && last.x + last.w == j.x {
                last.w += j.w;
                continue;
            }
        }
        out.push(j);
    }
    out
}

/// Conjunto de cores de um bloco (tabela aberta reaproveitada; para no limite).
struct ColorSet {
    keys: Vec<u32>,
    stamp: Vec<u32>,
    gen: u32,
}

impl ColorSet {
    fn new() -> Self {
        ColorSet { keys: vec![0; 4096], stamp: vec![0; 4096], gen: 0 }
    }

    /// (quantidade de cores até `limit`, primeira cor em BGR)
    #[allow(clippy::too_many_arguments)]
    fn count(&mut self, src: &[u8], width: u32, x: u32, y: u32, w: u32, h: u32, limit: u32) -> (u32, [u8; 3]) {
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.gen = 1;
        }
        let stride = (width * 4) as usize;
        let first_o = y as usize * stride + x as usize * 4;
        let first = [src[first_o], src[first_o + 1], src[first_o + 2]];
        let mut count = 0u32;
        let mut last = u32::MAX;
        for r in y..y + h {
            let o = r as usize * stride + x as usize * 4;
            for px in src[o..o + w as usize * 4].chunks_exact(4) {
                let k = u32::from_le_bytes([px[0], px[1], px[2], 0]);
                if k == last {
                    continue; // pixels vizinhos iguais: o caso mais comum em telas
                }
                last = k;
                let mut slot = (k.wrapping_mul(0x9E37_79B1) >> 20) as usize;
                loop {
                    if self.stamp[slot] != self.gen {
                        self.stamp[slot] = self.gen;
                        self.keys[slot] = k;
                        count += 1;
                        if count >= limit {
                            return (count, first);
                        }
                        break;
                    }
                    if self.keys[slot] == k {
                        break;
                    }
                    slot = (slot + 1) & 4095;
                }
            }
        }
        (count, first)
    }
}

/// BGR da região, com cada byte menos o do pixel à esquerda (predição; decodificação soma).
fn extract_bgr_filtered(src: &[u8], width: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let stride = (width * 4) as usize;
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for r in y..y + h {
        let o = r as usize * stride + x as usize * 4;
        let (mut pb, mut pg, mut pr) = (0u8, 0u8, 0u8);
        for px in src[o..o + w as usize * 4].chunks_exact(4) {
            out.push(px[0].wrapping_sub(pb));
            out.push(px[1].wrapping_sub(pg));
            out.push(px[2].wrapping_sub(pr));
            (pb, pg, pr) = (px[0], px[1], px[2]);
        }
    }
    out
}

fn extract_rgb(src: &[u8], width: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let stride = (width * 4) as usize;
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for r in y..y + h {
        let o = r as usize * stride + x as usize * 4;
        for px in src[o..o + w as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    out
}

fn copy_rect(src: &[u8], dst: &mut [u8], width: u32, x: u32, y: u32, w: u32, h: u32) {
    let stride = (width * 4) as usize;
    for r in y..y + h {
        let o = r as usize * stride + x as usize * 4;
        let n = w as usize * 4;
        dst[o..o + n].copy_from_slice(&src[o..o + n]);
    }
}

/// Copia as linhas `sy..sy+len` para `dy..dy+len` nas colunas `x..x+w` (como memmove).
fn apply_copy(buf: &mut [u8], width: u32, x: u32, w: u32, sy: u32, dy: u32, len: u32) {
    let stride = (width * 4) as usize;
    let (a, n) = (x as usize * 4, w as usize * 4);
    let rows: Box<dyn Iterator<Item = u32>> = if dy > sy { Box::new((0..len).rev()) } else { Box::new(0..len) };
    for k in rows {
        let s = (sy + k) as usize * stride + a;
        let d = (dy + k) as usize * stride + a;
        buf.copy_within(s..s + n, d);
    }
}

fn row_hash(b: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x0100_0000_01B3).rotate_left(29);
    }
    for &v in chunks.remainder() {
        h = (h ^ v as u64).wrapping_mul(0x0100_0000_01B3);
    }
    h
}

/// Procura rolagem vertical dentro de `x0..x1 × y0..y1`. Devolve (linha de origem, linha de
/// destino, quantidade de linhas) da maior faixa que só mudou de lugar.
fn detect_scroll(reference: &[u8], cur: &[u8], width: u32, x0: u32, x1: u32, y0: u32, y1: u32) -> Option<(u32, u32, u32)> {
    let stride = (width * 4) as usize;
    let (a, b) = (x0 as usize * 4, x1 as usize * 4);
    let row = |y: u32| -> std::ops::Range<usize> {
        let o = y as usize * stride;
        o + a..o + b
    };
    let hr: Vec<u64> = (y0..y1).map(|y| row_hash(&reference[row(y)])).collect();
    let hc: Vec<u64> = (y0..y1).map(|y| row_hash(&cur[row(y)])).collect();
    let mut index: HashMap<u64, Vec<u32>> = HashMap::with_capacity(hr.len());
    for (k, h) in hr.iter().enumerate() {
        index.entry(*h).or_default().push(k as u32);
    }
    let mut votes: HashMap<i32, u32> = HashMap::new();
    for (k, h) in hc.iter().enumerate() {
        if *h == hr[k] {
            continue;
        }
        if let Some(cands) = index.get(h) {
            if cands.len() <= 8 {
                for &rk in cands {
                    let dy = k as i32 - rk as i32;
                    if dy != 0 {
                        *votes.entry(dy).or_default() += 1;
                    }
                }
            }
        }
    }
    let (&dy, &count) = votes.iter().max_by_key(|(_, c)| **c)?;
    if count < 24 {
        return None;
    }
    // Maior faixa contínua de linhas em que cur[y] == reference[y - dy] (conferido byte a byte).
    let span = y1 - y0;
    let (mut best_start, mut best_len) = (0u32, 0u32);
    let (mut start, mut len) = (0u32, 0u32);
    for k in 0..span {
        let src = k as i32 - dy;
        let ok = src >= 0 && (src as u32) < span && {
            let (yd, ys) = (y0 + k, y0 + src as u32);
            hc[k as usize] == hr[src as usize] && cur[row(yd)] == reference[row(ys)]
        };
        if ok {
            if len == 0 {
                start = k;
            }
            len += 1;
            if len > best_len {
                best_len = len;
                best_start = start;
            }
        } else {
            len = 0;
        }
    }
    if best_len < 48 {
        return None;
    }
    let dst = y0 + best_start;
    let src = (dst as i32 - dy) as u32;
    Some((src, dst, best_len))
}

/// Decodificador de referência (testes e diagnóstico): aplica um quadro num framebuffer BGRA.
#[allow(dead_code)]
pub fn apply_frame(bytes: &[u8], fb: &mut Vec<u8>, dims: &mut (u32, u32)) -> Option<lrd_protocol::lrd::Header> {
    use lrd_protocol::lrd::{parse, Op};
    let (h, ops) = parse(bytes)?;
    if (h.width, h.height) != *dims {
        *dims = (h.width, h.height);
        *fb = vec![0; (h.width * h.height * 4) as usize];
    }
    let stride = (h.width * 4) as usize;
    for op in ops {
        match op {
            Op::Fill { x, y, w, h: hh, bgr } => {
                for r in y..y + hh {
                    for c in x..x + w {
                        let o = r as usize * stride + c as usize * 4;
                        fb[o..o + 4].copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
                    }
                }
            }
            Op::Lz4 { x, y, w, h: hh, filter, raw_len, data } => {
                let raw = backend::lz4_decompress(data, raw_len)?;
                let mut k = 0;
                for r in y..y + hh {
                    let (mut pb, mut pg, mut pr) = (0u8, 0u8, 0u8);
                    for c in x..x + w {
                        let (mut b, mut g, mut rr) = (raw[k], raw[k + 1], raw[k + 2]);
                        if filter == FILTER_LEFT {
                            b = b.wrapping_add(pb);
                            g = g.wrapping_add(pg);
                            rr = rr.wrapping_add(pr);
                        }
                        (pb, pg, pr) = (b, g, rr);
                        k += 3;
                        let o = r as usize * stride + c as usize * 4;
                        fb[o..o + 4].copy_from_slice(&[b, g, rr, 255]);
                    }
                }
            }
            Op::Jpeg { x, y, w, h: hh, data } => {
                let (jw, jh, rgb) = backend::jpeg_decode(data)?;
                if (jw, jh) != (w, hh) {
                    return None;
                }
                for r in 0..hh {
                    for c in 0..w {
                        let s = ((r * w + c) * 3) as usize;
                        let o = (y + r) as usize * stride + (x + c) as usize * 4;
                        fb[o..o + 4].copy_from_slice(&[rgb[s + 2], rgb[s + 1], rgb[s], 255]);
                    }
                }
            }
            Op::Copy { x, y, w, h: hh, sx, sy } => {
                if sx != x {
                    return None; // o codificador só gera cópia vertical
                }
                apply_copy(fb, h.width, x, w, sy, y, hh);
            }
        }
    }
    Some(h)
}

pub mod scenarios;

#[cfg(test)]
mod tests;
