//! Envio de mídia por WebRTC (lado do computador acessado).
//!
//! - Vídeo H.264 (constrained baseline) e áudio Opus 48 kHz em RTP/SRTP sobre UDP.
//! - Só candidatos ICE locais (rede local, sem STUN/TURN).
//! - Recuperação: NACK (retransmissão de pacotes perdidos) e PLI/FIR → quadro completo.
//! - Controle de banda: REMB do navegador como teto + perdas dos Receiver Reports;
//!   a taxa do H.264 é ajustada sem reiniciar o codificador.
//! - A sinalização (SDP) passa pelo canal QUIC autenticado, então o fingerprint DTLS do
//!   WebRTC fica amarrado à identidade já verificada do par.
//! - Mouse e teclado chegam por **canais de dados** direto do JavaScript do controlador
//!   ("pointer": sem ordem e sem retransmissão, para movimentos; "input": confiável, para
//!   cliques, teclas e rolagem). Evita o caminho JS → Tauri → Rust → QUIC.
//! - Vídeo enviado pela faixa própria de `rtp_video` (pacing + FEC + carimbo de captura).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use lrd_protocol::video::{AUDIO_HEADER_LEN, FLAG_HEVC, FRAME_AUDIO, FRAME_H264, H264_HEADER_LEN};
use lrd_video::CaptureCmd;

use crate::rtp_video::{OutFrame, Pacer, VideoTrack};

const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const H265_FMTP: &str = "level-id=93;profile-id=1;tier-flag=0;tx-mode=SRST";
const PT_H264: u8 = 102;
const PT_H265: u8 = 104;
const PT_RED: u8 = 116;
const PT_ULPFEC: u8 = 117;

/// Trata uma mensagem de entrada vinda do canal de dados; pode devolver uma resposta.
pub type InputHandler = Arc<dyn Fn(&[u8]) -> Option<Vec<u8>> + Send + Sync>;

/// Codecs extras que o controlador declarou suportar no WebRTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct RtcCodecs {
    pub hevc: bool,
    /// Desliga a proteção FEC (que só liga com LRD_FEC=1, experimental).
    pub no_fec: bool,
}
const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME: usize = 960; // 20 ms por canal

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcState {
    Connected,
    Failed,
}

pub struct RtcSender {
    pc: Arc<RTCPeerConnection>,
    video: Arc<VideoTrack>,
    pacer: Pacer,
    audio: Arc<TrackLocalStaticSample>,
    opus: tokio::sync::Mutex<OpusState>,
    _channels: Vec<Arc<RTCDataChannel>>,
}

struct OpusState {
    encoder: Option<opus::Encoder>,
    pending: Vec<i16>,
    resampler: Resampler,
}

fn feedback(typ: &str, parameter: &str) -> RTCPFeedback {
    RTCPFeedback { typ: typ.to_owned(), parameter: parameter.to_owned() }
}

impl RtcSender {
    /// Cria a conexão e devolve a oferta SDP completa (ICE já coletado).
    pub async fn start(
        cap_cmds: std::sync::mpsc::Sender<CaptureCmd>,
        max_bitrate: u32,
        state_tx: mpsc::UnboundedSender<RtcState>,
        input: InputHandler,
        codecs: RtcCodecs,
    ) -> Result<(Arc<Self>, String)> {
        let mut media = MediaEngine::default();
        // Sem transport-cc: o navegador então estima a banda e manda REMB.
        let video_fb = || {
            vec![feedback("goog-remb", ""), feedback("ccm", "fir"), feedback("nack", ""), feedback("nack", "pli")]
        };
        let mut video_codecs: Vec<(&str, u8, &str, Vec<RTCPFeedback>)> =
            vec![(MIME_TYPE_H264, PT_H264, H264_FMTP, video_fb())];
        if codecs.hevc {
            video_codecs.push(("video/H265", PT_H265, H265_FMTP, video_fb()));
        }
        // Proteção contra perdas (FEC): RED como envelope + ULPFEC como paridade.
        // Desligado por padrão: nos testes com Chromium (WebView2) e WebKit (Safari), o vídeo
        // envelopado em RED não era decodificado. Perdas são cobertas por NACK + pacing.
        if !codecs.no_fec && std::env::var_os("LRD_FEC").is_some() {
            video_codecs.push(("video/red", PT_RED, "", vec![]));
            video_codecs.push(("video/ulpfec", PT_ULPFEC, "", vec![]));
        }
        for (mime, pt, fmtp, fb) in video_codecs {
            media.register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback: fb,
                    },
                    payload_type: pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )?;
        }
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: OPUS_RATE,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        let mut registry = Registry::new();
        // Só em testes: perda simulada, registrada antes do NACK (fica mais perto da rede).
        if let Some(pct) = std::env::var("LRD_TEST_DROP_PCT").ok().and_then(|v| v.parse::<u32>().ok()) {
            registry.add(Box::new(crate::test_loss::LossBuilder { pct }));
        }
        registry = configure_nack(registry, &mut media);
        registry = configure_rtcp_reports(registry);

        let mut settings = SettingEngine::default();
        settings.set_network_types(vec![webrtc::ice::network_type::NetworkType::Udp4]);
        // ICE-lite: o navegador conduz a conexão (ele testa os nossos endereços e escolhe o par).
        // Sem isso, com Safari/WKWebView (que só informa nomes ".local" via mDNS), o lado Rust
        // não conseguia formar pares e a conexão ficava em "checking" — tela preta e queda
        // para o caminho de reserva (imagem borrada e travando).
        settings.set_lite(true);
        settings.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
        // Testes com navegador na mesma máquina: o Safari responde pelo 127.0.0.1.
        if std::env::var_os("LRD_TEST_LOOPBACK").is_some() {
            settings.set_include_loopback_candidate(true);
        }

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration { ice_servers: vec![], ..Default::default() }).await?);

        let video = Arc::new(VideoTrack::new());
        let audio = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: OPUS_RATE,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1".to_owned(),
                rtcp_feedback: vec![],
            },
            "audio".to_owned(),
            "lrd".to_owned(),
        ));
        let video_sender = pc.add_track(Arc::clone(&video) as Arc<dyn TrackLocal + Send + Sync>).await?;
        let audio_sender = pc.add_track(Arc::clone(&audio) as Arc<dyn TrackLocal + Send + Sync>).await?;

        let pacer = Pacer::start(video.clone(), max_bitrate);
        tokio::spawn(rate_control(video_sender, cap_cmds, max_bitrate, pacer.target_bps.clone()));

        // Canais de dados para a entrada (criados aqui para entrarem na oferta).
        let pointer = pc
            .create_data_channel(
                "pointer",
                Some(RTCDataChannelInit { ordered: Some(false), max_retransmits: Some(0), ..Default::default() }),
            )
            .await?;
        let reliable = pc
            .create_data_channel("input", Some(RTCDataChannelInit { ordered: Some(true), ..Default::default() }))
            .await?;
        for dc in [&pointer, &reliable] {
            let handler = input.clone();
            let weak = Arc::downgrade(dc);
            dc.on_message(Box::new(move |msg: DataChannelMessage| {
                let reply = handler(&msg.data[..]);
                let weak = weak.clone();
                Box::pin(async move {
                    if let (Some(r), Some(dc)) = (reply, weak.upgrade()) {
                        let _ = dc.send(&Bytes::from(r)).await;
                    }
                })
            }));
        }
        tokio::spawn(async move {
            // O RTCP do áudio precisa ser lido para os interceptadores funcionarem.
            while audio_sender.read_rtcp().await.is_ok() {}
        });

        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            let tx = state_tx.clone();
            Box::pin(async move {
                tracing::info!("WebRTC: {s}");
                match s {
                    RTCPeerConnectionState::Connected => {
                        let _ = tx.send(RtcState::Connected);
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                        let _ = tx.send(RtcState::Failed);
                    }
                    _ => {}
                }
            })
        }));

        let offer = pc.create_offer(None).await?;
        let mut gathered = pc.gathering_complete_promise().await;
        pc.set_local_description(offer).await?;
        let _ = tokio::time::timeout(Duration::from_secs(3), gathered.recv()).await;
        let local = pc.local_description().await.ok_or_else(|| anyhow!("oferta WebRTC vazia"))?;

        let sender = Arc::new(RtcSender {
            pc,
            video,
            pacer,
            audio,
            opus: tokio::sync::Mutex::new(OpusState { encoder: None, pending: Vec::new(), resampler: Resampler::default() }),
            _channels: vec![pointer, reliable],
        });
        Ok((sender, local.sdp))
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<()> {
        self.pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
        Ok(())
    }

    /// (H.264, H.265, FEC) negociados com o navegador. Válido depois de conectar.
    pub async fn negotiated(&self) -> (bool, bool, bool) {
        self.video.negotiated().await
    }

    /// Envia um quadro H.264/H.265 no formato de `lrd_protocol::video` (o cabeçalho é removido).
    /// Fila cheia = espera; a captura descarta quadros enquanto isso (o mais novo vence).
    pub async fn write_video(&self, frame: &[u8]) {
        if frame.len() <= H264_HEADER_LEN || frame[0] != FRAME_H264 {
            return;
        }
        let capture_us = u64::from_le_bytes(frame[6..14].try_into().unwrap());
        self.pacer
            .send(OutFrame {
                annexb: Bytes::copy_from_slice(&frame[H264_HEADER_LEN..]),
                hevc: frame[1] & FLAG_HEVC != 0,
                capture_us,
            })
            .await;
    }

    /// Recebe PCM (formato de `lrd_protocol::video`), converte para 48 kHz estéreo e envia em Opus.
    pub async fn write_audio(&self, frame: &[u8]) {
        if frame.len() < AUDIO_HEADER_LEN || frame[0] != FRAME_AUDIO {
            return;
        }
        let channels = frame[1].max(1) as usize;
        let rate = u32::from_le_bytes([frame[2], frame[3], frame[4], frame[5]]);
        let samples: Vec<i16> = frame[AUDIO_HEADER_LEN..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        // Estéreo intercalado.
        let mut stereo = Vec::with_capacity(samples.len() / channels * 2);
        for f in samples.chunks_exact(channels) {
            let l = f[0];
            let r = if channels > 1 { f[1] } else { f[0] };
            stereo.push(l);
            stereo.push(r);
        }

        let packets = {
            let mut st = self.opus.lock().await;
            if st.encoder.is_none() {
                match opus::Encoder::new(OPUS_RATE, opus::Channels::Stereo, opus::Application::LowDelay) {
                    Ok(mut e) => {
                        let _ = e.set_bitrate(opus::Bitrate::Bits(128_000));
                        let _ = e.set_inband_fec(true);
                        let _ = e.set_packet_loss_perc(5);
                        st.encoder = Some(e);
                    }
                    Err(e) => {
                        tracing::warn!("Opus indisponível: {e}");
                        return;
                    }
                }
            }
            let resampled = st.resampler.process(&stereo, rate, OPUS_RATE);
            st.pending.extend_from_slice(&resampled);
            let mut out = Vec::new();
            while st.pending.len() >= OPUS_FRAME * 2 {
                let chunk: Vec<i16> = st.pending.drain(..OPUS_FRAME * 2).collect();
                let mut buf = vec![0u8; 4000];
                if let Some(enc) = st.encoder.as_mut() {
                    if let Ok(n) = enc.encode(&chunk, &mut buf) {
                        buf.truncate(n);
                        out.push(buf);
                    }
                }
            }
            out
        };
        for p in packets {
            let sample = Sample { data: Bytes::from(p), duration: Duration::from_millis(20), ..Default::default() };
            if let Err(e) = self.audio.write_sample(&sample).await {
                tracing::debug!("WebRTC áudio: {e}");
            }
        }
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}

/// Lê o RTCP do vídeo: PLI/FIR pedem quadro completo; perdas e quedas bruscas do REMB
/// reduzem a taxa.
///
/// O REMB do navegador NÃO é usado como teto direto: ele começa em ~300 kbps e sobe só
/// ~8% por segundo, e nunca passa de 1,5× o que está chegando. Usá-lo como teto prendia o
/// vídeo em 400 kbps (imagem borrada e resolução reduzida) por quase um minuto. Numa rede
/// local a banda sobra; só uma QUEDA do REMB (o navegador detectou fila crescendo) ou perda
/// de pacotes indica congestionamento de verdade.
async fn rate_control(
    sender: Arc<RTCRtpSender>,
    cap: std::sync::mpsc::Sender<CaptureCmd>,
    max_bitrate: u32,
    pacing: Arc<std::sync::atomic::AtomicU32>,
) {
    let min_bitrate = 500_000u32.min(max_bitrate);
    let mut target = max_bitrate;
    let mut last_change = Instant::now();
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut last_increase = Instant::now();
    let mut last_congestion = Instant::now() - Duration::from_secs(10);
    let mut remb_prev: Option<f64> = None;

    while let Ok((packets, _)) = sender.read_rtcp().await {
        let mut desired = target;
        for p in packets {
            let any = p.as_any();
            if any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>() {
                if last_key.elapsed() > Duration::from_millis(300) {
                    let _ = cap.send(CaptureCmd::Keyframe);
                    last_key = Instant::now();
                }
            } else if let Some(r) = any.downcast_ref::<ReceiverEstimatedMaximumBitrate>() {
                let r = r.bitrate as f64;
                if r.is_finite() && r > 0.0 {
                    if let Some(prev) = remb_prev {
                        // Queda brusca = fila crescendo na rede: segue a estimativa.
                        if r < prev * 0.85 && (r as u32) < target {
                            desired = desired.min((r * 0.9) as u32);
                            last_congestion = Instant::now();
                        }
                    }
                    remb_prev = Some(r);
                }
            } else if let Some(rr) = any.downcast_ref::<ReceiverReport>() {
                for rep in &rr.reports {
                    let loss = rep.fraction_lost as f32 / 256.0;
                    if loss > 0.10 {
                        desired = desired.min(((target as f32) * (1.0 - loss * 0.5)) as u32);
                        last_congestion = Instant::now();
                    } else if loss < 0.02
                        && last_increase.elapsed() > Duration::from_secs(1)
                        && last_congestion.elapsed() > Duration::from_secs(3)
                    {
                        desired = desired.max(((target as f32) * 1.15) as u32);
                        last_increase = Instant::now();
                    }
                }
            }
        }
        let desired = desired.clamp(min_bitrate, max_bitrate);
        let change = (desired as f32 - target as f32).abs() / target.max(1) as f32;
        let urgent = desired < target && change > 0.25;
        if change > 0.05 && (urgent || last_change.elapsed() > Duration::from_millis(1000)) {
            target = desired;
            last_change = Instant::now();
            tracing::debug!("taxa de vídeo → {} kbps", target / 1000);
            let _ = cap.send(CaptureCmd::SetBitrate(target));
            pacing.store(target, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Reamostragem linear simples (ex.: 44,1 kHz do Windows → 48 kHz do Opus).
#[derive(Default)]
struct Resampler {
    pos: f64,
    last: [i16; 2],
}

impl Resampler {
    fn process(&mut self, stereo: &[i16], from: u32, to: u32) -> Vec<i16> {
        if from == to || from == 0 {
            return stereo.to_vec();
        }
        let frames = stereo.len() / 2;
        if frames == 0 {
            return Vec::new();
        }
        let step = from as f64 / to as f64;
        let mut out = Vec::with_capacity((frames as f64 / step) as usize * 2 + 4);
        // Índice -1 = última amostra do bloco anterior (continuidade entre blocos).
        let get = |i: isize, c: usize, last: &[i16; 2]| -> f64 {
            if i < 0 {
                last[c] as f64
            } else {
                stereo[(i as usize).min(frames - 1) * 2 + c] as f64
            }
        };
        let mut pos = self.pos;
        while pos < frames as f64 - 1.0 {
            let i = pos.floor() as isize;
            let t = pos - i as f64;
            for c in 0..2 {
                let a = get(i, c, &self.last);
                let b = get(i + 1, c, &self.last);
                out.push((a + (b - a) * t) as i16);
            }
            pos += step;
        }
        self.pos = pos - frames as f64;
        self.last = [stereo[(frames - 1) * 2], stereo[(frames - 1) * 2 + 1]];
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::default();
        let input = vec![1000i16; 441 * 2 * 10];
        let mut total = 0;
        for chunk in input.chunks(441 * 2) {
            total += r.process(chunk, 44_100, 48_000).len() / 2;
        }
        // 4410 quadros a 44,1 kHz ≈ 4800 a 48 kHz
        assert!((4780..=4810).contains(&total), "{total}");
    }
}
