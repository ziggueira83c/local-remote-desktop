//! Detector da região de vídeo/jogo (ver especificação 2026-11-05, seção 2.1).
//!
//! Um bloco de 64×64 é "vídeo" depois de ser classificado com perdas por 6 quadros seguidos.
//! A região é o retângulo do maior grupo 4-conectado desses blocos, alinhado a 16 px, com
//! histerese: só muda de tamanho se algum lado andar mais de 64 px e só desliga depois de
//! 1 s sem nenhum movimento (dentro ou fora dela).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{Rect, TILE};

const STREAK_TO_VIDEO: u8 = 6;
const MIN_SIDE: u32 = 128;
const HYSTERESIS: u32 = 64;
const IDLE_OFF: Duration = Duration::from_secs(1);
const ALIGN: u32 = 16;

pub struct RegionTracker {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    streak: Vec<u8>,
    active: Option<Rect>,
    last_activity: Option<Instant>,
}

impl RegionTracker {
    pub fn new(width: u32, height: u32) -> Self {
        let (cols, rows) = (width.div_ceil(TILE), height.div_ceil(TILE));
        RegionTracker {
            width,
            height,
            cols,
            rows,
            streak: vec![0; (cols * rows) as usize],
            active: None,
            last_activity: None,
        }
    }

    pub fn reset(&mut self) {
        self.streak.iter_mut().for_each(|s| *s = 0);
        self.active = None;
        self.last_activity = None;
    }

    /// `moving_tiles`: um por bloco (linha a linha), verdadeiro se o bloco foi classificado com
    /// perdas neste quadro. `inside_changed`: a região ativa mudou neste quadro.
    pub fn update(&mut self, moving_tiles: &[bool], inside_changed: bool, now: Instant) -> Option<Rect> {
        let mut any_video = false;
        for (s, m) in self.streak.iter_mut().zip(moving_tiles) {
            *s = if *m { s.saturating_add(1) } else { 0 };
            any_video |= *s >= STREAK_TO_VIDEO;
        }
        if any_video || (inside_changed && self.active.is_some()) {
            self.last_activity = Some(now);
        }
        if let Some(cand) = self.candidate() {
            match self.active {
                Some(cur) if !differs(cur, cand) => {}
                _ => self.active = Some(cand),
            }
        }
        if self.active.is_some() && self.last_activity.is_none_or(|t| now.duration_since(t) >= IDLE_OFF) {
            self.active = None;
        }
        self.active
    }

    /// Retângulo do maior grupo conectado de blocos de vídeo (alinhado), se grande o bastante.
    fn candidate(&self) -> Option<Rect> {
        let (cols, rows) = (self.cols as usize, self.rows as usize);
        let is_video = |i: usize| self.streak[i] >= STREAK_TO_VIDEO;
        let mut seen = vec![false; cols * rows];
        let mut best: Option<(usize, (u32, u32, u32, u32))> = None;
        let mut queue = VecDeque::new();
        for start in 0..cols * rows {
            if seen[start] || !is_video(start) {
                continue;
            }
            seen[start] = true;
            queue.push_back(start);
            let (mut c0, mut r0, mut c1, mut r1, mut n) = (usize::MAX, usize::MAX, 0, 0, 0);
            while let Some(i) = queue.pop_front() {
                let (c, r) = (i % cols, i / cols);
                n += 1;
                c0 = c0.min(c);
                r0 = r0.min(r);
                c1 = c1.max(c + 1);
                r1 = r1.max(r + 1);
                let mut push = |j: usize| {
                    if !seen[j] && is_video(j) {
                        seen[j] = true;
                        queue.push_back(j);
                    }
                };
                if c > 0 {
                    push(i - 1);
                }
                if c + 1 < cols {
                    push(i + 1);
                }
                if r > 0 {
                    push(i - cols);
                }
                if r + 1 < rows {
                    push(i + cols);
                }
            }
            if best.is_none_or(|(bn, _)| n > bn) {
                best = Some((n, (c0 as u32, r0 as u32, c1 as u32, r1 as u32)));
            }
        }
        let (_, (c0, r0, c1, r1)) = best?;
        let x0 = (c0 * TILE) / ALIGN * ALIGN;
        let y0 = (r0 * TILE) / ALIGN * ALIGN;
        let x1 = ((c1 * TILE).min(self.width)).div_ceil(ALIGN) * ALIGN;
        let y1 = ((r1 * TILE).min(self.height)).div_ceil(ALIGN) * ALIGN;
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        let (w, h) = ((x1 - x0) & !1, (y1 - y0) & !1);
        (w >= MIN_SIDE && h >= MIN_SIDE).then_some((x0, y0, w, h))
    }
}

/// Algum lado andou mais que a histerese?
fn differs(a: Rect, b: Rect) -> bool {
    let d = |p: u32, q: u32| p.abs_diff(q) > HYSTERESIS;
    d(a.0, b.0) || d(a.1, b.1) || d(a.0 + a.2, b.0 + b.2) || d(a.1 + a.3, b.1 + b.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const W: u32 = 1920;
    const H: u32 = 1080;
    const COLS: u32 = 30; // 1920 / 64
    const ROWS: u32 = 17; // ceil(1080 / 64)

    fn mask(rects: &[(u32, u32, u32, u32)]) -> Vec<bool> {
        let mut m = vec![false; (COLS * ROWS) as usize];
        for &(c0, r0, c1, r1) in rects {
            for r in r0..r1 {
                for c in c0..c1 {
                    m[(r * COLS + c) as usize] = true;
                }
            }
        }
        m
    }

    struct Clock(Instant);
    impl Clock {
        fn tick(&mut self) -> Instant {
            self.0 += Duration::from_millis(33);
            self.0
        }
    }

    fn contains(r: Rect, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
        r.0 <= x0 && r.1 <= y0 && r.0 + r.2 >= x1.min(W) && r.1 + r.3 >= y1.min(H)
    }

    #[test]
    fn activates_after_six_frames() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(5, 3, 25, 15)]);
        for _ in 0..5 {
            assert_eq!(t.update(&m, false, clk.tick()), None);
        }
        let r = t.update(&m, false, clk.tick()).expect("deveria ligar no 6º quadro");
        assert_eq!((r.0 % 16, r.1 % 16, r.2 % 2, r.3 % 2), (0, 0, 0, 0));
        assert!(contains(r, 5 * 64, 3 * 64, 25 * 64, 15 * 64), "{r:?}");
    }

    #[test]
    fn ignores_small_regions() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(10, 5, 11, 6)]);
        for _ in 0..20 {
            assert_eq!(t.update(&m, false, clk.tick()), None);
        }
    }

    #[test]
    fn picks_largest_group() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(1, 1, 3, 3), (10, 5, 16, 9)]);
        let mut r = None;
        for _ in 0..6 {
            r = t.update(&m, false, clk.tick());
        }
        let r = r.unwrap();
        assert!(contains(r, 640, 320, 1024, 576), "{r:?}");
        assert!(r.0 >= 600, "pegou o grupo pequeno: {r:?}");
    }

    #[test]
    fn stable_under_jitter() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(5, 3, 25, 15)]);
        let mut r = None;
        for _ in 0..6 {
            r = t.update(&m, false, clk.tick());
        }
        let first = r.unwrap();
        for k in 0..30 {
            let m = if k % 2 == 0 { mask(&[(5, 3, 24, 15)]) } else { mask(&[(5, 3, 26, 15)]) };
            assert_eq!(t.update(&m, true, clk.tick()), Some(first));
        }
    }

    #[test]
    fn resizes_on_big_change() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(5, 3, 20, 15)]);
        let mut r = None;
        for _ in 0..6 {
            r = t.update(&m, false, clk.tick());
        }
        let first = r.unwrap();
        let m = mask(&[(5, 3, 23, 15)]);
        let mut r2 = None;
        for _ in 0..6 {
            r2 = t.update(&m, true, clk.tick());
        }
        let r2 = r2.unwrap();
        assert!(r2.2 >= first.2 + 128, "{first:?} → {r2:?}");
    }

    #[test]
    fn stays_while_inside_changes_then_turns_off() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(5, 3, 25, 15)]);
        for _ in 0..6 {
            t.update(&m, false, clk.tick());
        }
        let none = mask(&[]);
        for _ in 0..60 {
            assert!(t.update(&none, true, clk.tick()).is_some());
        }
        // 27 quadros × 33 ms ≈ 891 ms sem atividade: ainda ligada.
        for _ in 0..27 {
            assert!(t.update(&none, false, clk.tick()).is_some());
        }
        // Passando de 1 s: desliga.
        let mut off = false;
        for _ in 0..4 {
            off |= t.update(&none, false, clk.tick()).is_none();
        }
        assert!(off);
    }

    #[test]
    fn does_not_grow_over_static_text() {
        let mut t = RegionTracker::new(W, H);
        let mut clk = Clock(Instant::now());
        let m = mask(&[(5, 3, 25, 15)]);
        let mut r = None;
        for _ in 0..6 {
            r = t.update(&m, false, clk.tick());
        }
        let r = r.unwrap();
        assert!(r.0 + r.2 <= 25 * 64 + 15, "{r:?}");
        assert!(r.1 + r.3 <= 15 * 64 + 15, "{r:?}");
    }
}
