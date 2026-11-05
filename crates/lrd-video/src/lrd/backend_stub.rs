//! Substituto sem dependências externas (só para compilar/testar a lógica com `--cfg lrd_stub`).
pub fn lz4_compress(raw: &[u8]) -> Vec<u8> {
    raw.to_vec()
}
pub fn lz4_decompress(data: &[u8], raw_len: usize) -> Option<Vec<u8>> {
    (data.len() == raw_len).then(|| data.to_vec())
}
/// "JPEG" falso com perdas: guarda as dimensões e os pixels com os 3 bits baixos zerados.
pub fn jpeg_encode(rgb: &[u8], w: u32, h: u32, _q: u8) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() + 8);
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend(rgb.iter().map(|v| v & 0xF8));
    Some(out)
}
pub fn jpeg_decode(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let w = u32::from_le_bytes(data.get(0..4)?.try_into().ok()?);
    let h = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?);
    Some((w, h, data[8..].to_vec()))
}
