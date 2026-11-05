//! **codecLRD** — formato de quadro do codec próprio de tela (inspirado na arquitetura
//! "DeskStream" do documento de especificação; não é compatível com o DeskRT do AnyDesk).
//!
//! A tela é dividida em blocos de 64×64. Só o que mudou é enviado, cada região com a técnica
//! que melhor lhe serve:
//! - **FILL**: região de uma cor só (fundo, barra) — 3 bytes.
//! - **LZ4**: sem perdas (texto, janelas, ícones): BGR com predição pela esquerda + LZ4.
//! - **JPEG**: com perdas (foto, vídeo). Fica nítido depois: quando a região para de mudar,
//!   é reenviada sem perdas ("refino").
//! - **COPY**: copia uma região já presente no controlador (rolagem): quase nenhum byte.
//!
//! O canal é confiável e ordenado: o estado do controlador é sempre igual ao do host depois
//! de aplicar cada quadro em ordem. Um salto em `frame_id` (quadro perdido) ⇒ pedir quadro
//! completo (`keyframe`).
//!
//! ```text
//! cabeçalho (20 bytes, little-endian)
//!   u8  type = 5
//!   u8  flags          bit 0 = keyframe (estado absoluto: não depende de quadros anteriores)
//!   u16 width, u16 height
//!   u64 capture_us     instante da captura (relógio do host, lrd_protocol::now_us)
//!   u32 frame_id       sequencial
//!   u16 op_count
//! operações, aplicadas em ordem (x, y, w, h: u16, retângulo de destino)
//!   1 FILL : x y w h, u8 b, u8 g, u8 r
//!   2 LZ4  : x y w h, u8 filter (0 = nenhum, 1 = diferença para o pixel à esquerda),
//!            u32 raw_len (= w*h*3), u32 comp_len, [bloco LZ4 de BGR linha a linha]
//!   3 JPEG : x y w h, u32 len, [JPEG]
//!   4 COPY : x y w h, u16 src_x, u16 src_y   (como memmove: origem lida antes de escrever)
//! ```

pub const FRAME_LRD: u8 = 5;
pub const HEADER_LEN: usize = 20;
pub const FLAG_KEYFRAME: u8 = 1;

pub const OP_FILL: u8 = 1;
pub const OP_LZ4: u8 = 2;
pub const OP_JPEG: u8 = 3;
pub const OP_COPY: u8 = 4;

pub const FILTER_NONE: u8 = 0;
pub const FILTER_LEFT: u8 = 1;

/// Limites de segurança do decodificador.
pub const MAX_DIM: u32 = 8192;
pub const MAX_OPS: usize = 65_535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub capture_us: u64,
    pub frame_id: u32,
    pub op_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op<'a> {
    Fill { x: u32, y: u32, w: u32, h: u32, bgr: [u8; 3] },
    Lz4 { x: u32, y: u32, w: u32, h: u32, filter: u8, raw_len: usize, data: &'a [u8] },
    Jpeg { x: u32, y: u32, w: u32, h: u32, data: &'a [u8] },
    Copy { x: u32, y: u32, w: u32, h: u32, sx: u32, sy: u32 },
}

/// Escreve um quadro. `ops` já serializadas (ver `push_*`).
pub struct Writer {
    buf: Vec<u8>,
    count: u16,
}

impl Writer {
    pub fn new(keyframe: bool, width: u32, height: u32, capture_us: u64, frame_id: u32) -> Self {
        let mut buf = Vec::with_capacity(64 * 1024);
        buf.push(FRAME_LRD);
        buf.push(if keyframe { FLAG_KEYFRAME } else { 0 });
        buf.extend_from_slice(&(width as u16).to_le_bytes());
        buf.extend_from_slice(&(height as u16).to_le_bytes());
        buf.extend_from_slice(&capture_us.to_le_bytes());
        buf.extend_from_slice(&frame_id.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        Writer { buf, count: 0 }
    }

    fn rect(&mut self, kind: u8, x: u32, y: u32, w: u32, h: u32) {
        self.buf.push(kind);
        for v in [x, y, w, h] {
            self.buf.extend_from_slice(&(v as u16).to_le_bytes());
        }
        self.count += 1;
    }

    pub fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, bgr: [u8; 3]) {
        self.rect(OP_FILL, x, y, w, h);
        self.buf.extend_from_slice(&bgr);
    }

    pub fn lz4(&mut self, x: u32, y: u32, w: u32, h: u32, filter: u8, raw_len: usize, data: &[u8]) {
        self.rect(OP_LZ4, x, y, w, h);
        self.buf.push(filter);
        self.buf.extend_from_slice(&(raw_len as u32).to_le_bytes());
        self.buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
    }

    pub fn jpeg(&mut self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        self.rect(OP_JPEG, x, y, w, h);
        self.buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(data);
    }

    pub fn copy(&mut self, x: u32, y: u32, w: u32, h: u32, sx: u32, sy: u32) {
        self.rect(OP_COPY, x, y, w, h);
        self.buf.extend_from_slice(&(sx as u16).to_le_bytes());
        self.buf.extend_from_slice(&(sy as u16).to_le_bytes());
    }

    pub fn op_count(&self) -> usize {
        self.count as usize
    }

    pub fn finish(mut self) -> Vec<u8> {
        let c = self.count.to_le_bytes();
        self.buf[18] = c[0];
        self.buf[19] = c[1];
        self.buf
    }
}

fn u16_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]) as u32)
}

fn u32_at(b: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize)
}

pub fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < HEADER_LEN || b[0] != FRAME_LRD {
        return None;
    }
    let h = Header {
        keyframe: b[1] & FLAG_KEYFRAME != 0,
        width: u16_at(b, 2)?,
        height: u16_at(b, 4)?,
        capture_us: u64::from_le_bytes(b[6..14].try_into().ok()?),
        frame_id: u32_at(b, 14)? as u32,
        op_count: u16_at(b, 18)? as usize,
    };
    if h.width == 0 || h.height == 0 || h.width > MAX_DIM || h.height > MAX_DIM {
        return None;
    }
    Some(h)
}

/// Lê todas as operações, validando limites (retângulos dentro do quadro, tamanhos coerentes).
pub fn parse(b: &[u8]) -> Option<(Header, Vec<Op<'_>>)> {
    let h = parse_header(b)?;
    let mut ops = Vec::with_capacity(h.op_count.min(4096));
    let mut o = HEADER_LEN;
    let inside = |x: u32, y: u32, w: u32, h_: u32| w > 0 && h_ > 0 && x + w <= h.width && y + h_ <= h.height;
    for _ in 0..h.op_count {
        let kind = *b.get(o)?;
        let (x, y, w, hh) = (u16_at(b, o + 1)?, u16_at(b, o + 3)?, u16_at(b, o + 5)?, u16_at(b, o + 7)?);
        if !inside(x, y, w, hh) {
            return None;
        }
        o += 9;
        let op = match kind {
            OP_FILL => {
                let bgr = [*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?];
                o += 3;
                Op::Fill { x, y, w, h: hh, bgr }
            }
            OP_LZ4 => {
                let filter = *b.get(o)?;
                let raw_len = u32_at(b, o + 1)?;
                let len = u32_at(b, o + 5)?;
                o += 9;
                if raw_len != (w * hh * 3) as usize || filter > FILTER_LEFT {
                    return None;
                }
                let data = b.get(o..o + len)?;
                o += len;
                Op::Lz4 { x, y, w, h: hh, filter, raw_len, data }
            }
            OP_JPEG => {
                let len = u32_at(b, o)?;
                o += 4;
                let data = b.get(o..o + len)?;
                o += len;
                Op::Jpeg { x, y, w, h: hh, data }
            }
            OP_COPY => {
                let (sx, sy) = (u16_at(b, o)?, u16_at(b, o + 2)?);
                o += 4;
                if !inside(sx, sy, w, hh) {
                    return None;
                }
                Op::Copy { x, y, w, h: hh, sx, sy }
            }
            _ => return None,
        };
        ops.push(op);
    }
    if o != b.len() {
        return None;
    }
    Some((h, ops))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_parse_roundtrip() {
        let mut w = Writer::new(true, 100, 50, 123, 7);
        w.fill(0, 0, 64, 50, [1, 2, 3]);
        w.lz4(64, 0, 36, 10, FILTER_LEFT, 36 * 10 * 3, &[9, 9, 9]);
        w.jpeg(0, 0, 10, 10, &[0xff, 0xd8]);
        w.copy(0, 10, 100, 40, 0, 0);
        let f = w.finish();
        let (h, ops) = parse(&f).unwrap();
        assert!(h.keyframe);
        assert_eq!((h.width, h.height, h.capture_us, h.frame_id, h.op_count), (100, 50, 123, 7, 4));
        assert_eq!(ops[0], Op::Fill { x: 0, y: 0, w: 64, h: 50, bgr: [1, 2, 3] });
        assert!(matches!(ops[3], Op::Copy { sx: 0, sy: 0, .. }));
        // Retângulo fora do quadro é recusado.
        let mut bad = Writer::new(false, 10, 10, 0, 0);
        bad.fill(5, 5, 10, 10, [0; 3]);
        assert!(parse(&bad.finish()).is_none());
        // Bytes sobrando são recusados.
        let mut extra = f.clone();
        extra.push(0);
        assert!(parse(&extra).is_none());
    }
}
