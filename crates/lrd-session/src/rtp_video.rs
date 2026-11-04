//! Envio de vídeo por RTP com controle próprio do empacotamento:
//! - **Carimbo RTP = momento da captura** (90 kHz), o que permite medir o atraso real no controlador.
//! - **Pacing:** os pacotes de cada quadro são espalhados no tempo, em vez de saírem todos de
//!   uma vez (rajadas causam perdas no Wi-Fi).
//! - **FEC (ULPFEC sobre RED, RFC 5109/2198):** um pacote de paridade XOR a cada poucos pacotes
//!   de mídia. O navegador reconstrói um pacote perdido sem esperar retransmissão.
//! - H.264 e H.265 na mesma faixa: o tipo de carga (PT) acompanha o codec de cada quadro.

use std::any::Any;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use tokio::sync::{mpsc, Mutex};
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::codecs::h265::HevcPayloader;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Payloader;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::{TrackLocal, TrackLocalContext, TrackLocalWriter};

const MTU: usize = 1150;
/// Um pacote de paridade a cada N pacotes de mídia (~20% de redundância).
const FEC_GROUP: usize = 5;

#[derive(Clone)]
struct Binding {
    ssrc: u32,
    writer: Arc<dyn TrackLocalWriter + Send + Sync>,
    pt_h264: Option<u8>,
    pt_h265: Option<u8>,
    pt_red: Option<u8>,
    pt_ulpfec: Option<u8>,
}

/// Faixa de vídeo própria (implementa `TrackLocal` do webrtc-rs).
pub struct VideoTrack {
    binding: Mutex<Option<Binding>>,
    id: String,
    stream_id: String,
}

fn find_pt(codecs: &[RTCRtpCodecParameters], mime: &str, fmtp_hint: Option<&str>) -> Option<u8> {
    let matching: Vec<&RTCRtpCodecParameters> =
        codecs.iter().filter(|c| c.capability.mime_type.eq_ignore_ascii_case(mime)).collect();
    if let Some(hint) = fmtp_hint {
        if let Some(c) = matching.iter().find(|c| c.capability.sdp_fmtp_line.contains(hint)) {
            return Some(c.payload_type);
        }
    }
    matching.first().map(|c| c.payload_type)
}

impl VideoTrack {
    pub fn new() -> Self {
        VideoTrack { binding: Mutex::new(None), id: "video".into(), stream_id: "lrd".into() }
    }

    /// Codecs efetivamente negociados (para log/diagnóstico).
    pub async fn negotiated(&self) -> (bool, bool, bool) {
        match &*self.binding.lock().await {
            Some(b) => (b.pt_h264.is_some(), b.pt_h265.is_some(), b.pt_red.is_some() && b.pt_ulpfec.is_some()),
            None => (false, false, false),
        }
    }
}

#[async_trait]
impl TrackLocal for VideoTrack {
    async fn bind(&self, t: &TrackLocalContext) -> webrtc::error::Result<RTCRtpCodecParameters> {
        let codecs = t.codec_parameters();
        let b = Binding {
            ssrc: t.ssrc(),
            writer: t.write_stream(),
            pt_h264: find_pt(codecs, "video/H264", Some("packetization-mode=1")),
            pt_h265: find_pt(codecs, "video/H265", None),
            pt_red: find_pt(codecs, "video/red", None),
            pt_ulpfec: find_pt(codecs, "video/ulpfec", None),
        };
        tracing::info!(
            "WebRTC vídeo: H.264={:?} H.265={:?} RED={:?} ULPFEC={:?}",
            b.pt_h264,
            b.pt_h265,
            b.pt_red,
            b.pt_ulpfec
        );
        let chosen = codecs
            .iter()
            .find(|c| Some(c.payload_type) == b.pt_h264)
            .or_else(|| codecs.iter().find(|c| Some(c.payload_type) == b.pt_h265))
            .cloned()
            .ok_or(webrtc::Error::ErrUnsupportedCodec)?;
        *self.binding.lock().await = Some(b);
        Ok(chosen)
    }

    async fn unbind(&self, _t: &TrackLocalContext) -> webrtc::error::Result<()> {
        *self.binding.lock().await = None;
        Ok(())
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn rid(&self) -> Option<&str> {
        None
    }

    fn stream_id(&self) -> &str {
        &self.stream_id
    }

    fn kind(&self) -> RTPCodecType {
        RTPCodecType::Video
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Quadro pronto para envio (bitstream Annex B).
pub struct OutFrame {
    pub annexb: Bytes,
    pub hevc: bool,
    pub capture_us: u64,
}

/// Empacota, protege e envia os quadros com pacing. A taxa de pacing acompanha a taxa alvo.
pub struct Pacer {
    tx: mpsc::Sender<OutFrame>,
    pub target_bps: Arc<AtomicU32>,
}

impl Pacer {
    pub fn start(track: Arc<VideoTrack>, initial_bps: u32) -> Self {
        let (tx, rx) = mpsc::channel::<OutFrame>(2);
        let target_bps = Arc::new(AtomicU32::new(initial_bps));
        tokio::spawn(run(track, rx, target_bps.clone()));
        Pacer { tx, target_bps }
    }

    /// Espera se houver fila (contrapressão até a captura, que então descarta quadros).
    pub async fn send(&self, f: OutFrame) {
        let _ = self.tx.send(f).await;
    }
}

async fn run(track: Arc<VideoTrack>, mut rx: mpsc::Receiver<OutFrame>, target_bps: Arc<AtomicU32>) {
    let mut seq: u16 = rand_u16();
    let mut h264 = H264Payloader::default();
    let mut h265 = HevcPayloader::default();
    while let Some(frame) = rx.recv().await {
        let Some(b) = track.binding.lock().await.clone() else { continue };
        let (payloader, pt): (&mut dyn Payloader, Option<u8>) = if frame.hevc {
            (&mut h265, b.pt_h265)
        } else {
            (&mut h264, b.pt_h264)
        };
        let Some(media_pt) = pt else { continue };
        let payloads = match payloader.payload(MTU, &frame.annexb) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("empacotamento: {e}");
                continue;
            }
        };
        if payloads.is_empty() {
            continue;
        }
        let ts = ((frame.capture_us as u128 * 90 / 1000) & 0xffff_ffff) as u32;
        let fec = match (b.pt_red, b.pt_ulpfec) {
            (Some(red), Some(ulp)) => Some((red, ulp)),
            _ => None,
        };

        // Monta todos os pacotes do quadro (mídia + paridade).
        let mut out: Vec<Packet> = Vec::with_capacity(payloads.len() + payloads.len() / FEC_GROUP + 1);
        let last = payloads.len() - 1;
        let mut group: Vec<(u16, bool, Bytes)> = Vec::with_capacity(FEC_GROUP);
        for (i, payload) in payloads.into_iter().enumerate() {
            let marker = i == last;
            let this_seq = seq;
            seq = seq.wrapping_add(1);
            match fec {
                Some((red, _)) => {
                    let mut p = BytesMut::with_capacity(payload.len() + 1);
                    p.put_u8(media_pt & 0x7f);
                    p.extend_from_slice(&payload);
                    out.push(packet(b.ssrc, red, this_seq, ts, marker, p.freeze()));
                    group.push((this_seq, marker, payload));
                }
                None => out.push(packet(b.ssrc, media_pt, this_seq, ts, marker, payload)),
            }
            if let Some((red, ulp)) = fec {
                if group.len() == FEC_GROUP || (marker && !group.is_empty()) {
                    let fec_payload = ulpfec(&group, media_pt, ts);
                    let mut p = BytesMut::with_capacity(fec_payload.len() + 1);
                    p.put_u8(ulp & 0x7f);
                    p.extend_from_slice(&fec_payload);
                    out.push(packet(b.ssrc, red, seq, ts, false, p.freeze()));
                    seq = seq.wrapping_add(1);
                    group.clear();
                }
            }
        }

        // Pacing: espalha a ~2,5× a taxa alvo (mínimo 25 Mbps), em fatias de 1 ms.
        // Se outro quadro já estiver esperando, despeja o resto sem atrasar.
        let rate_bps = (target_bps.load(Ordering::Relaxed) as u64 * 5 / 2).max(25_000_000);
        let bytes_per_ms = (rate_bps / 8 / 1000).max(1500) as usize;
        let mut budget = 0usize;
        let mut next = tokio::time::Instant::now();
        for p in out {
            if budget == 0 && rx.is_empty() {
                tokio::time::sleep_until(next).await;
                next += Duration::from_millis(1);
                budget = bytes_per_ms;
            }
            budget = budget.saturating_sub(p.payload.len() + 12);
            if let Err(e) = b.writer.write_rtp(&p).await {
                tracing::debug!("RTP: {e}");
            }
        }
    }
}

fn packet(ssrc: u32, pt: u8, seq: u16, ts: u32, marker: bool, payload: Bytes) -> Packet {
    Packet {
        header: Header {
            version: 2,
            marker,
            payload_type: pt,
            sequence_number: seq,
            timestamp: ts,
            ssrc,
            ..Default::default()
        },
        payload,
    }
}

/// Pacote ULPFEC (RFC 5109, L=0) protegendo `group` (sequência, marcador, carga de mídia).
/// A proteção é calculada sobre os pacotes de mídia como seriam sem RED (PT de mídia).
fn ulpfec(group: &[(u16, bool, Bytes)], media_pt: u8, ts: u32) -> Vec<u8> {
    let base = group[0].0;
    let max_len = group.iter().map(|g| g.2.len()).max().unwrap_or(0);
    let mut byte1 = 0u8;
    let mut ts_x = 0u32;
    let mut len_x = 0u16;
    let mut mask = 0u16;
    let mut payload_x = vec![0u8; max_len];
    for (seq, marker, payload) in group {
        byte1 ^= ((*marker as u8) << 7) | (media_pt & 0x7f);
        ts_x ^= ts;
        len_x ^= payload.len() as u16;
        let offset = seq.wrapping_sub(base);
        if offset < 16 {
            mask |= 1 << (15 - offset);
        }
        for (i, v) in payload.iter().enumerate() {
            payload_x[i] ^= v;
        }
    }
    let mut out = Vec::with_capacity(14 + max_len);
    out.push(0); // E=0, L=0, P/X/CC recuperação = 0
    out.push(byte1);
    out.extend_from_slice(&base.to_be_bytes());
    out.extend_from_slice(&ts_x.to_be_bytes());
    out.extend_from_slice(&len_x.to_be_bytes());
    out.extend_from_slice(&(max_len as u16).to_be_bytes());
    out.extend_from_slice(&mask.to_be_bytes());
    out.extend_from_slice(&payload_x);
    out
}

fn rand_u16() -> u16 {
    let b = uuid::Uuid::new_v4();
    u16::from_le_bytes([b.as_bytes()[0], b.as_bytes()[1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fec_recovers_single_loss() {
        let pkts: Vec<(u16, bool, Bytes)> = vec![
            (10, false, Bytes::from_static(&[1, 2, 3])),
            (11, false, Bytes::from_static(&[4, 5])),
            (12, true, Bytes::from_static(&[6, 7, 8, 9])),
        ];
        let fec = ulpfec(&pkts, 102, 1234);
        // Recupera o pacote 11 a partir do FEC e dos outros dois.
        let max = u16::from_be_bytes([fec[10], fec[11]]) as usize;
        let mut rec = fec[14..14 + max].to_vec();
        for (_, _, p) in [&pkts[0], &pkts[2]] {
            for (i, v) in p.iter().enumerate() {
                rec[i] ^= v;
            }
        }
        let len = u16::from_be_bytes([fec[8], fec[9]]) ^ 3 ^ 4;
        assert_eq!(&rec[..len as usize], &[4, 5]);
        assert_eq!(u16::from_be_bytes([fec[12], fec[13]]), 0b1110_0000_0000_0000);
    }
}
