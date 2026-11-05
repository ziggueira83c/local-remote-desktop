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
//! u8 type = 2, u8 visible, u16 x, u16 y, [u8 kind]   (pixels do quadro; kind opcional)
//! ```
//! Vídeo H.264 (`type = 3`):
//! ```text
//! u8 type = 3, u8 flags (bit 0 = keyframe, bit 1 = HEVC), u16 width, u16 height, u64 timestamp_us,
//! u32 sequence, [bitstream Annex B]
//! ```
//! Áudio PCM (`type = 4`):
//! ```text
//! u8 type = 4, u8 channels, u32 sample_rate, u64 timestamp_us, [i16 LE intercalado]
//! ```
//! `timestamp_us` vem de `crate::now_us()` no host (mesmo relógio para vídeo e áudio).

pub const FRAME_TILES: u8 = 1;
pub const FRAME_CURSOR: u8 = 2;
pub const FRAME_H264: u8 = 3;
pub const FRAME_AUDIO: u8 = 4;
pub const H264_HEADER_LEN: usize = 18;
pub const AUDIO_HEADER_LEN: usize = 14;
pub const FLAG_KEYFRAME: u8 = 1;
/// Quadro tipo 3 com bitstream H.265/HEVC em vez de H.264.
pub const FLAG_HEVC: u8 = 2;
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

/// Formatos de cursor (mapeados para `cursor` do CSS no controlador).
pub mod cursor_kind {
    pub const DEFAULT: u8 = 0;
    pub const TEXT: u8 = 1;
    pub const POINTER: u8 = 2;
    pub const WAIT: u8 = 3;
    pub const PROGRESS: u8 = 4;
    pub const CROSSHAIR: u8 = 5;
    pub const EW_RESIZE: u8 = 6;
    pub const NS_RESIZE: u8 = 7;
    pub const NWSE_RESIZE: u8 = 8;
    pub const NESW_RESIZE: u8 = 9;
    pub const MOVE: u8 = 10;
    pub const NOT_ALLOWED: u8 = 11;
    pub const HELP: u8 = 12;
}

/// Cursor com formato.
pub fn build_cursor_frame_kind(visible: bool, x: u16, y: u16, kind: u8) -> Vec<u8> {
    let mut out = build_cursor_frame(visible, x, y);
    out.push(kind);
    out
}

/// Monta um quadro H.264.
pub fn build_h264_frame(width: u16, height: u16, keyframe: bool, timestamp_us: u64, sequence: u32, bitstream: &[u8]) -> Vec<u8> {
    build_video_frame(width, height, keyframe, false, timestamp_us, sequence, bitstream)
}

/// Monta um quadro de vídeo comprimido (H.264 ou, com `hevc`, H.265).
pub fn build_video_frame(
    width: u16,
    height: u16,
    keyframe: bool,
    hevc: bool,
    timestamp_us: u64,
    sequence: u32,
    bitstream: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(H264_HEADER_LEN + bitstream.len());
    out.push(FRAME_H264);
    out.push(if keyframe { FLAG_KEYFRAME } else { 0 } | if hevc { FLAG_HEVC } else { 0 });
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&timestamp_us.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(bitstream);
    out
}

/// Monta um bloco de áudio PCM 16 bits intercalado.
pub fn build_audio_frame(sample_rate: u32, channels: u8, timestamp_us: u64, samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + samples.len() * 2);
    out.push(FRAME_AUDIO);
    out.push(channels);
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&timestamp_us.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Validação estrutural de um quadro recebido (o visualizador não confia no par remoto).
pub fn validate_frame(buf: &[u8]) -> bool {
    match buf.first() {
        Some(&FRAME_CURSOR) => buf.len() == 6 || buf.len() == 7,
        Some(&FRAME_H264) => buf.len() > H264_HEADER_LEN,
        Some(&FRAME_AUDIO) => {
            buf.len() >= AUDIO_HEADER_LEN
                && buf[1] >= 1
                && buf[1] <= 8
                && (buf.len() - AUDIO_HEADER_LEN) % (2 * buf[1] as usize) == 0
        }
        Some(&crate::lrd::FRAME_LRD) => crate::lrd::parse(buf).is_some(),
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
        assert!(validate_frame(&build_h264_frame(16, 16, true, 1, 1, &[0, 0, 0, 1, 0x67])));
        assert!(validate_frame(&build_audio_frame(48000, 2, 5, &[1, 2, 3, 4])));
        assert!(!validate_frame(&build_audio_frame(48000, 2, 5, &[1, 2, 3])));
    }
}
