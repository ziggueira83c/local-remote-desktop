//! Envio de mídia por WebRTC (lado do computador acessado).
//!
//! - Vídeo H.264 (constrained baseline) e áudio Opus 48 kHz em RTP/SRTP sobre UDP.
//! - Só candidatos ICE locais (rede local, sem STUN/TURN).
//! - Recuperação: NACK (retransmissão de pacotes perdidos) e PLI/FIR → quadro completo.
//! - Controle de banda: REMB do navegador como teto + perdas dos Receiver Reports;
//!   a taxa do H.264 é ajustada sem reiniciar o codificador.
//! - A sinalização (SDP) passa pelo canal QUIC autenticado, então o fingerprint DTLS do
//!   WebRTC fica amarrado à identidade já verificada do par.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use tokio::sync::mpsc;
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
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

use lrd_protocol::video::{AUDIO_HEADER_LEN, FRAME_AUDIO, FRAME_H264, H264_HEADER_LEN};
use lrd_video::CaptureCmd;

const H264_FMTP: &str = "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f";
const OPUS_RATE: u32 = 48_000;
const OPUS_FRAME: usize = 960; // 20 ms por canal

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcState {
    Connected,
    Failed,
}

pub struct RtcSender {
    pc: Arc<RTCPeerConnection>,
    video: Arc<TrackLocalStaticSample>,
    audio: Arc<TrackLocalStaticSample>,
    last_video: Mutex<Option<Instant>>,
    opus: tokio::sync::Mutex<OpusState>,
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
    ) -> Result<(Arc<Self>, String)> {
        let mut media = MediaEngine::default();
        // Sem transport-cc: o navegador então estima a banda e manda REMB.
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: H264_FMTP.to_owned(),
                    rtcp_feedback: vec![
                        feedback("goog-remb", ""),
                        feedback("ccm", "fir"),
                        feedback("nack", ""),
                        feedback("nack", "pli"),
                    ],
                },
                payload_type: 102,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
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
        registry = configure_nack(registry, &mut media);
        registry = configure_rtcp_reports(registry);

        let mut settings = SettingEngine::default();
        settings.set_network_types(vec![webrtc::ice::network_type::NetworkType::Udp4]);

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration { ice_servers: vec![], ..Default::default() }).await?);

        let video = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line: H264_FMTP.to_owned(),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            "lrd".to_owned(),
        ));
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

        tokio::spawn(rate_control(video_sender, cap_cmds, max_bitrate));
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
            audio,
            last_video: Mutex::new(None),
            opus: tokio::sync::Mutex::new(OpusState { encoder: None, pending: Vec::new(), resampler: Resampler::default() }),
        });
        Ok((sender, local.sdp))
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<()> {
        self.pc.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
        Ok(())
    }

    /// Envia um quadro H.264 no formato de `lrd_protocol::video` (o cabeçalho é removido).
    pub async fn write_video(&self, frame: &[u8]) {
        if frame.len() <= H264_HEADER_LEN || frame[0] != FRAME_H264 {
            return;
        }
        let now = Instant::now();
        let duration = {
            let mut last = self.last_video.lock().unwrap();
            let d = last.map(|t| now.duration_since(t)).unwrap_or(Duration::from_millis(33));
            *last = Some(now);
            d.clamp(Duration::from_millis(1), Duration::from_millis(500))
        };
        let sample = Sample {
            data: Bytes::copy_from_slice(&frame[H264_HEADER_LEN..]),
            duration,
            ..Default::default()
        };
        if let Err(e) = self.video.write_sample(&sample).await {
            tracing::debug!("WebRTC vídeo: {e}");
        }
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

/// Lê o RTCP do vídeo: PLI/FIR pedem quadro completo; REMB e perdas ajustam a taxa.
async fn rate_control(sender: Arc<RTCRtpSender>, cap: std::sync::mpsc::Sender<CaptureCmd>, max_bitrate: u32) {
    let min_bitrate = 400_000u32;
    let mut target = max_bitrate;
    let mut remb_cap = max_bitrate;
    let mut last_change = Instant::now();
    let mut last_key = Instant::now() - Duration::from_secs(1);
    let mut last_increase = Instant::now();

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
                if r.bitrate.is_finite() && r.bitrate > 0.0 {
                    remb_cap = ((r.bitrate * 0.9) as u32).clamp(min_bitrate, max_bitrate);
                    desired = desired.min(remb_cap);
                }
            } else if let Some(rr) = any.downcast_ref::<ReceiverReport>() {
                for rep in &rr.reports {
                    let loss = rep.fraction_lost as f32 / 256.0;
                    if loss > 0.08 {
                        desired = ((target as f32) * (1.0 - loss * 0.5)) as u32;
                    } else if loss < 0.02 && last_increase.elapsed() > Duration::from_secs(2) {
                        desired = ((target as f32) * 1.08) as u32;
                        last_increase = Instant::now();
                    }
                }
            }
        }
        let desired = desired.clamp(min_bitrate, remb_cap.max(min_bitrate));
        let change = (desired as f32 - target as f32).abs() / target.max(1) as f32;
        let urgent = desired < target && change > 0.25;
        if change > 0.08 && (urgent || last_change.elapsed() > Duration::from_millis(1000)) {
            target = desired;
            last_change = Instant::now();
            tracing::debug!("taxa de vídeo → {} kbps", target / 1000);
            let _ = cap.send(CaptureCmd::SetBitrate(target));
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
