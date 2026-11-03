//! Formato binário dos quadros de vídeo (little-endian, lido direto pelo frontend com DataView).
//!
//! Quadro de tiles (`type = 1`):
//! ```text
//! u8  type = 1
//! u8  flags        (bit 0 = keyframe: a tela inteira foi enviada)
//! u16 width        largura do quadro em pixels
//! u16 height       altura do quadro em pixels
//! u16 tile_count
//! u32 sequence
//! repetido tile_count vezes:
//!   u16 x, u16 y, u16 w, u16 h, u32 jpeg_len, [jpeg_len bytes JPEG]
//! ```
//! Cursor (`type = 2`):
//! ```text
//! u8 type = 2, u8 visible, u16 x, u16 y     (coordenadas em pixels do quadro)
//! ```

pub const FRAME_TILES: u8 = 1;
pub const FRAME_CURSOR: u8 = 2;
pub const FLAG_KEYFRAME: u8 = 1;
pub const TILES_HEADER_LEN: usize = 12;
pub const TILE_HEADER_LEN: usize = 12;

/// Um tile já comprimido.
pub struct EncodedTile {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub jpeg: Vec<u8>,
}

/// Monta um quadro de tiles.
pub fn build_tiles_frame(width: u16, height: u16, keyframe: bool, sequence: u32, tiles: &[EncodedTile]) -> Vec<u8> {
    let total: usize = TILES_HEADER_LEN + tiles.iter().map(|t| TILE_HEADER_LEN + t.jpeg.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.push(FRAME_TILES);
    out.push(if keyframe { FLAG_KEYFRAME } else { 0 });
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&(tiles.len() as u16).to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    for t in tiles {
        out.extend_from_slice(&t.x.to_le_bytes());
        out.extend_from_slice(&t.y.to_le_bytes());
        out.extend_from_slice(&t.w.to_le_bytes());
        out.extend_from_slice(&t.h.to_le_bytes());
        out.extend_from_slice(&(t.jpeg.len() as u32).to_le_bytes());
        out.extend_from_slice(&t.jpeg);
    }
    out
}

/// Monta uma atualização de cursor.
pub fn build_cursor_frame(visible: bool, x: u16, y: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(6);
    out.push(FRAME_CURSOR);
    out.push(visible as u8);
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    out
}

/// Validação estrutural de um quadro recebido (o visualizador não confia no par remoto).
pub fn validate_frame(buf: &[u8]) -> bool {
    match buf.first() {
        Some(&FRAME_CURSOR) => buf.len() == 6,
        Some(&FRAME_TILES) => {
            if buf.len() < TILES_HEADER_LEN {
                return false;
            }
            let width = u16::from_le_bytes([buf[2], buf[3]]) as u32;
            let height = u16::from_le_bytes([buf[4], buf[5]]) as u32;
            let count = u16::from_le_bytes([buf[6], buf[7]]) as usize;
            let mut off = TILES_HEADER_LEN;
            for _ in 0..count {
                if off + TILE_HEADER_LEN > buf.len() {
                    return false;
                }
                let x = u16::from_le_bytes([buf[off], buf[off + 1]]) as u32;
                let y = u16::from_le_bytes([buf[off + 2], buf[off + 3]]) as u32;
                let w = u16::from_le_bytes([buf[off + 4], buf[off + 5]]) as u32;
                let h = u16::from_le_bytes([buf[off + 6], buf[off + 7]]) as u32;
                let len = u32::from_le_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]) as usize;
                if x + w > width || y + h > height {
                    return false;
                }
                off += TILE_HEADER_LEN;
                if off + len > buf.len() {
                    return false;
                }
                off += len;
            }
            off == buf.len()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_validate() {
        let tiles = vec![EncodedTile { x: 0, y: 0, w: 10, h: 10, jpeg: vec![1, 2, 3] }];
        let f = build_tiles_frame(20, 20, true, 1, &tiles);
        assert!(validate_frame(&f));
        let mut bad = f.clone();
        bad.push(0);
        assert!(!validate_frame(&bad));
        assert!(validate_frame(&build_cursor_frame(true, 5, 5)));
    }
}
