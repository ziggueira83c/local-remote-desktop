//! Compressores usados pelo codecLRD (isolados para facilitar testes).

use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;

pub fn lz4_compress(raw: &[u8]) -> Vec<u8> {
    lz4_flex::block::compress(raw)
}

#[allow(dead_code)]
pub fn lz4_decompress(data: &[u8], raw_len: usize) -> Option<Vec<u8>> {
    lz4_flex::block::decompress(data, raw_len).ok()
}

/// JPEG de uma região RGB (w*h*3): `jpeg-encoder` (SIMD AVX2 quando o processador tem),
/// croma 4:2:0 — várias vezes mais rápido que o codificador do crate `image`.
pub fn jpeg_encode(rgb: &[u8], w: u32, h: u32, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 8);
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(10, 100));
    enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
    enc.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).ok()?;
    Some(out)
}

/// Codificador antigo (crate `image`), mantido só para comparação nos testes de desempenho.
#[allow(dead_code)]
pub fn jpeg_encode_image_crate(rgb: &[u8], w: u32, h: u32, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 8);
    JpegEncoder::new_with_quality(&mut out, quality.clamp(10, 100))
        .encode(rgb, w, h, ExtendedColorType::Rgb8)
        .ok()?;
    Some(out)
}

/// Decodifica um JPEG em RGB (só para testes/diagnóstico do lado Rust).
#[allow(dead_code)]
pub fn jpeg_decode(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).ok()?.to_rgb8();
    Some((img.width(), img.height(), img.into_raw()))
}
