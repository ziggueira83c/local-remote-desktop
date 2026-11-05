//! Lado controlado (host): recebe pedidos, pede aprovação, transmite a tela e aplica a
//! entrada — sempre verificando as permissões concedidas (RN-009).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use lrd_protocol::envelope::Payload;
use lrd_protocol::framing::{read_envelope, write_envelope, write_frame};
use lrd_protocol::input_event::Event;
use lrd_protocol::{
    reason, ConnectResponse, DisplayInfo, Envelope, Hello, InputEvent, PermissionUpdate, SessionClose, WebRtcSignal,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use lrd_security::{fingerprint_of, format_fingerprint, is_allowed_peer_ip, sas_code, Permissions, Profile};
use lrd_video::{CaptureCmd, CaptureParams, Display, QualitySettings, VideoCodec};

use crate::node::{close_reason, Decision, InboundHandle, Node};
use crate::rtc::{RtcSender, RtcState};
use crate::{platform_name, NodeEvent};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING: usize = 3;

enum Outcome {
    Decided(Decision),
    Timeout,
    PeerGone,
}

impl Node {
    pub(crate) async fn handle_incoming(self: Arc<Self>, incoming: quinn::Incoming) {
        let remote = incoming.remote_address();
        let ip = remote.ip();
        if !is_allowed_peer_ip(ip) {
            tracing::warn!("conexão recusada de endereço fora da rede local: {ip}");
            self.storage.audit(None, "refused_non_local", serde_json::json!({ "ip": ip.to_string() }));
            incoming.refuse();
            return;
        }
        if self.rate.lock().unwrap().is_blocked(ip) {
            tracing::warn!("conexão recusada (muitas tentativas): {ip}");
            incoming.refuse();
            return;
        }
        let conn = match incoming.accept() {
            Ok(connecting) => match timeout(HANDSHAKE_TIMEOUT, connecting).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    tracing::debug!("handshake falhou com {ip}: {e}");
                    return;
                }
                Err(_) => return,
            },
            Err(e) => {
                tracing::debug!("falha ao aceitar {ip}: {e}");
                return;
            }
        };
        if let Err(e) = self.clone().serve_inbound(conn.clone(), remote).await {
            tracing::warn!("sessão de entrada com {ip} terminou com erro: {e:#}");
            conn.close(quinn::VarInt::from_u32(1), reason::PROTOCOL_ERROR.as_bytes());
        }
    }

    async fn serve_inbound(self: Arc<Self>, conn: quinn::Connection, remote: SocketAddr) -> Result<()> {
        let peer_cert = lrd_transport::peer_certificate(&conn)?;
        let peer_fp = fingerprint_of(&peer_cert);

        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await??;

        // 1. Hello
        let hello = match timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut recv)).await??.payload {
            Some(Payload::Hello(h)) => h,
            _ => bail!("esperava Hello"),
        };
        if hello.max_version < MIN_PROTOCOL_VERSION || hello.min_version > PROTOCOL_VERSION {
            let env = Envelope::new("", 1, Payload::SessionClose(SessionClose { reason_code: reason::VERSION.into() }));
            write_envelope(&mut send, &env).await?;
            let _ = send.finish();
            let _ = timeout(Duration::from_secs(2), conn.closed()).await;
            return Ok(());
        }
        let device_name: String = hello.device_name.chars().take(63).collect();
        let my_hello = Hello {
            device_id: self.identity.device_id.clone(),
            device_name: self.settings().device_name,
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            codecs: vec![
                lrd_protocol::codec::WEBRTC.into(),
                lrd_protocol::codec::H264.into(),
                lrd_protocol::codec::JPEG_TILES.into(),
            ],
            platform: platform_name().into(),
        };
        write_envelope(&mut send, &Envelope::new("", 1, Payload::Hello(my_hello))).await?;
        // WebRTC sempre usa H.264. Sem WebRTC: H.264 via WebCodecs se possível, senão JPEG.
        let viewer_webrtc = hello.codecs.iter().any(|c| c == lrd_protocol::codec::WEBRTC);
        let viewer_h264 = hello.codecs.iter().any(|c| c == lrd_protocol::codec::H264);
        let viewer_lrd = hello.codecs.iter().any(|c| c == lrd_protocol::codec::LRD);
        let rtc_codecs = crate::rtc::RtcCodecs {
            hevc: hello.codecs.iter().any(|c| c == lrd_protocol::codec::WEBRTC_H265),
            no_fec: false,
            lrd: viewer_lrd,
        };
        // codecLRD (próprio) sempre que o controlador entende; senão H.264/JPEG.
        let codec = if viewer_lrd {
            VideoCodec::Lrd
        } else if viewer_webrtc || viewer_h264 {
            VideoCodec::H264
        } else {
            VideoCodec::JpegTiles
        };

        // 2. ConnectRequest
        let req = match timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut recv)).await??.payload {
            Some(Payload::ConnectRequest(r)) => r,
            _ => bail!("esperava ConnectRequest"),
        };

        let busy = self.inbound.lock().unwrap().is_some() || self.pending.lock().unwrap().len() >= MAX_PENDING;
        if busy {
            return reject(&conn, &mut send, "", reason::BUSY).await;
        }

        // 3. Identidade: o device_id deve continuar com o mesmo certificado de antes.
        let known = self.storage.get_device(&hello.device_id)?;
        if let Some(dev) = &known {
            if dev.fingerprint != peer_fp {
                tracing::warn!("{} apresentou fingerprint diferente do registrado", hello.device_id);
                self.storage.audit(
                    None,
                    "identity_mismatch",
                    serde_json::json!({ "device_id": hello.device_id, "ip": remote.ip().to_string() }),
                );
                self.rate.lock().unwrap().record_failure(remote.ip());
                self.notice(
                    "warning",
                    &format!(
                        "Pedido de \"{device_name}\" recusado: a identidade não confere com a registrada. \
                         Se o app foi reinstalado nesse computador, remova-o dos dispositivos conhecidos."
                    ),
                );
                return reject(&conn, &mut send, "", reason::IDENTITY_MISMATCH).await;
            }
        }

        // 4. Pedido de aprovação ao usuário.
        let session_id = uuid::Uuid::new_v4().to_string();
        let address = remote.ip().to_string();
        self.storage.insert_session(&session_id, "inbound", "awaiting_approval", Some(&device_name), Some(&address))?;
        let requested = Profile::parse(&req.requested_profile).unwrap_or(Profile::ViewOnly);

        // Acesso com senha (opcional): senha correta = aprovado sem perguntar.
        let mut password_login = false;
        if !req.password.is_empty() && self.settings().password_access {
            if let Some(hash) = self.password_hash() {
                let pw = req.password.clone();
                let ok = tokio::task::spawn_blocking(move || lrd_security::verify_password(&pw, &hash))
                    .await
                    .unwrap_or(false);
                if !ok {
                    self.rate.lock().unwrap().record_failure(remote.ip());
                    self.storage.audit(
                        Some(&session_id),
                        "wrong_password",
                        serde_json::json!({ "device": device_name, "ip": address }),
                    );
                    self.storage.session_ended(&session_id, "failed", Some(reason::WRONG_PASSWORD))?;
                    self.notice("warning", &format!("Tentativa de acesso com senha incorreta de \"{device_name}\" ({address})."));
                    return reject(&conn, &mut send, &session_id, reason::WRONG_PASSWORD).await;
                }
                password_login = true;
            }
        }

        let outcome = if password_login {
            self.storage.audit(Some(&session_id), "password_login", serde_json::json!({ "device": device_name }));
            self.notice("info", &format!("\"{device_name}\" entrou com a senha de acesso."));
            Outcome::Decided(Decision::Approve(Permissions::for_profile(Profile::RemoteControl)))
        } else {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(session_id.clone(), tx);
            self.events.emit(NodeEvent::IncomingRequest {
                session_id: session_id.clone(),
                device_id: hello.device_id.clone(),
                device_name: device_name.clone(),
                platform: hello.platform.clone(),
                address: address.clone(),
                fingerprint: format_fingerprint(&peer_fp),
                sas: sas_code(&self.identity.fingerprint, &peer_fp),
                known_device: known.is_some(),
                requested_profile: requested,
            });

            let o = tokio::select! {
                r = timeout(APPROVAL_TIMEOUT, rx) => match r {
                    Ok(Ok(d)) => Outcome::Decided(d),
                    Ok(Err(_)) => Outcome::Decided(Decision::Reject),
                    Err(_) => Outcome::Timeout,
                },
                _ = conn.closed() => Outcome::PeerGone,
            };
            self.pending.lock().unwrap().remove(&session_id);
            o
        };

        let granted = match outcome {
            Outcome::PeerGone => {
                self.events.emit(NodeEvent::IncomingCancelled { session_id: session_id.clone(), reason: "peer_cancelled".into() });
                self.storage.session_ended(&session_id, "failed", Some("peer_cancelled"))?;
                return Ok(());
            }
            Outcome::Timeout => {
                self.events.emit(NodeEvent::IncomingCancelled { session_id: session_id.clone(), reason: reason::TIMEOUT.into() });
                self.storage.session_ended(&session_id, "failed", Some(reason::TIMEOUT))?;
                return reject(&conn, &mut send, &session_id, reason::TIMEOUT).await;
            }
            Outcome::Decided(Decision::Reject) => {
                self.rate.lock().unwrap().record_failure(remote.ip());
                self.storage.audit(Some(&session_id), "session_rejected", serde_json::json!({ "device": device_name }));
                self.storage.session_ended(&session_id, "closed", Some(reason::REJECTED))?;
                return reject(&conn, &mut send, &session_id, reason::REJECTED).await;
            }
            Outcome::Decided(Decision::Approve(p)) => p,
        };

        // Outra sessão pode ter sido aprovada enquanto esta aguardava.
        if self.inbound.lock().unwrap().is_some() {
            self.storage.session_ended(&session_id, "failed", Some(reason::BUSY))?;
            return reject(&conn, &mut send, &session_id, reason::BUSY).await;
        }

        // Nunca conceder mais do que o pedido; ver a tela é a base de qualquer sessão.
        let mut granted = granted.intersect(&Permissions::for_profile(requested));
        granted.view_screen = true;

        if !lrd_video::permissions::has_screen_capture_permission() {
            lrd_video::permissions::request_screen_capture_permission();
            self.notice(
                "warning",
                "O macOS precisa da permissão de Gravação de Tela para este app. Conceda em Ajustes do Sistema › Privacidade e reinicie o app.",
            );
        }
        let mut injector = None;
        if granted.control_mouse || granted.control_keyboard {
            if !lrd_input::has_accessibility_permission() {
                self.notice(
                    "warning",
                    "O macOS precisa da permissão de Acessibilidade para que o controle remoto funcione.",
                );
            }
            match self.injector() {
                Ok(i) => injector = Some(i),
                Err(e) => {
                    self.notice("error", &format!("Controle remoto indisponível: {e:#}"));
                    granted.control_mouse = false;
                    granted.control_keyboard = false;
                }
            }
        }

        let displays = tokio::task::spawn_blocking(lrd_video::list_displays).await??;
        if displays.is_empty() {
            bail!("nenhum monitor disponível para captura");
        }
        let active = req
            .requested_display
            .parse::<u32>()
            .ok()
            .and_then(|id| displays.iter().find(|d| d.id == id))
            .or_else(|| displays.iter().find(|d| d.primary))
            .unwrap_or(&displays[0])
            .clone();

        self.storage.upsert_device(&lrd_storage::NewDevice {
            id: &hello.device_id,
            hostname: &device_name,
            display_name: &device_name,
            os_type: &hello.platform,
            cert_der: &peer_cert,
            fingerprint: &peer_fp,
            last_ip: Some(&address),
            last_port: None,
        })?;
        self.storage.session_activated(&session_id, &hello.device_id, &granted.to_list())?;
        self.storage.audit(
            Some(&session_id),
            "session_approved",
            serde_json::json!({ "device": device_name, "permissions": granted.to_list() }),
        );

        let audio_allowed = self.settings().share_audio && audio_supported();
        let audio_on = audio_allowed && req.want_audio;
        let resp = ConnectResponse {
            approved: true,
            reason_code: reason::APPROVED.into(),
            granted_permissions: granted.to_list(),
            displays: displays.iter().map(to_display_info).collect(),
            active_display: active.id.to_string(),
            video_codec: match codec {
                VideoCodec::H264 => lrd_protocol::codec::H264.into(),
                VideoCodec::JpegTiles => lrd_protocol::codec::JPEG_TILES.into(),
                VideoCodec::Lrd => lrd_protocol::codec::LRD.into(),
            },
            audio_enabled: audio_on,
        };
        write_envelope(&mut send, &Envelope::new(&session_id, 2, Payload::ConnectResponse(resp))).await?;

        let media = MediaOptions { codec, webrtc: viewer_webrtc, viewer_h264, audio_allowed, audio_on, rtc_codecs };
        self.run_inbound(conn, send, recv, session_id, device_name, address, granted, displays, active, injector, media)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inbound(
        self: Arc<Self>,
        conn: quinn::Connection,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        session_id: String,
        device_name: String,
        address: String,
        granted: Permissions,
        displays: Vec<Display>,
        active: Display,
        injector: Option<Arc<lrd_input::InputInjector>>,
        media: MediaOptions,
    ) -> Result<()> {
        let perms = Arc::new(RwLock::new(granted));
        let geometry = Arc::new(RwLock::new(active.clone()));

        // Captura de tela.
        let s = self.settings();
        let quality = QualitySettings::from_profile(&s.quality_profile, s.max_fps);
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(1);
        // Áudio: fila curta; se a rede engasgar, blocos antigos são descartados na origem.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(32);
        let capture = lrd_video::spawn_capture(
            CaptureParams {
                display_id: Some(active.id),
                quality,
                codec: media.codec,
                cursor: Some(Box::new(|| {
                    lrd_input::cursor_position().map(|(x, y)| {
                        let (kind, visible) = lrd_input::cursor_shape();
                        lrd_video::CursorInfo { x, y, kind, visible }
                    })
                })),
                audio: if media.audio_allowed { Some(audio_tx) } else { None },
                audio_enabled: media.audio_on,
            },
            frame_tx,
        );
        let cap_cmds = capture.commands.clone();

        // Escritor do canal de controle.
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Envelope>(32);
        let writer = tokio::spawn(async move {
            while let Some(env) = ctrl_rx.recv().await {
                if write_envelope(&mut send, &env).await.is_err() {
                    break;
                }
            }
        });

        *self.inbound.lock().unwrap() = Some(InboundHandle {
            session_id: session_id.clone(),
            conn: conn.clone(),
            ctrl_tx: ctrl_tx.clone(),
            perms: perms.clone(),
        });
        self.events.emit(NodeEvent::InboundStarted {
            session_id: session_id.clone(),
            device_name: device_name.clone(),
            address,
            permissions: granted,
        });

        // ---- Rota da mídia: WebRTC (preferido) ou streams QUIC (reserva) ----
        let route: Arc<Mutex<Route>> =
            Arc::new(Mutex::new(if media.webrtc { Route::Pending } else { Route::Quic }));
        let (answer_tx, mut answer_rx) = mpsc::channel::<String>(1);

        // Vídeo (e cursor).
        let conn_v = conn.clone();
        let route_v = route.clone();
        let video = tokio::spawn(async move {
            let mut uni: Option<quinn::SendStream> = None;
            while let Some(frame) = frame_rx.recv().await {
                let r = route_v.lock().unwrap().clone();
                let via_quic = match &r {
                    Route::Pending => false,
                    Route::Quic => true,
                    Route::Rtc(rtc) if media.codec == VideoCodec::Lrd => {
                        // codecLRD e cursor pelo canal de dados (ordem garantida, direto ao JS).
                        rtc.write_media(&frame).await;
                        false
                    }
                    Route::Rtc(rtc) => {
                        if frame[0] == lrd_protocol::video::FRAME_H264 {
                            rtc.write_video(&frame).await;
                            false
                        } else {
                            // Cursor (e eventuais tiles) continuam pelo QUIC.
                            true
                        }
                    }
                };
                if via_quic {
                    if uni.is_none() {
                        match conn_v.open_uni().await {
                            Ok(u) => uni = Some(u),
                            Err(_) => break,
                        }
                    }
                    if let Some(stream) = uni.as_mut() {
                        if write_frame(stream, &frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Áudio.
        let conn_a = conn.clone();
        let route_a = route.clone();
        let audio_task = tokio::spawn(async move {
            let mut uni: Option<quinn::SendStream> = None;
            while let Some(block) = audio_rx.recv().await {
                let r = route_a.lock().unwrap().clone();
                match r {
                    Route::Pending => {}
                    Route::Rtc(rtc) => rtc.write_audio(&block).await,
                    Route::Quic => {
                        if uni.is_none() {
                            match conn_a.open_uni().await {
                                Ok(u) => uni = Some(u),
                                Err(_) => break,
                            }
                        }
                        if let Some(stream) = uni.as_mut() {
                            if write_frame(stream, &block).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Negociação WebRTC; se falhar, cai para QUIC sem derrubar a sessão.
        let route_r = route.clone();
        let cap_r = cap_cmds.clone();
        let ctrl_r = ctrl_tx.clone();
        let sid_r = session_id.clone();
        let max_bitrate = quality.bitrate_bps;
        let perms_dc = perms.clone();
        let geometry_dc = geometry.clone();
        let injector_dc = injector.clone();
        let last_move = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        let dc_handler: crate::rtc::InputHandler = Arc::new(move |b: &[u8]| {
            handle_dc_input(b, &perms_dc, &geometry_dc, injector_dc.as_deref(), &last_move)
        });
        let rtc_codecs = media.rtc_codecs;
        let rtc_task = tokio::spawn(async move {
            if !media.webrtc {
                return;
            }
            let fallback = |why: &str| {
                tracing::warn!("WebRTC indisponível ({why}); usando QUIC");
                *route_r.lock().unwrap() = Route::Quic;
                // O caminho QUIC (WebCodecs) decodifica só H.264.
                let _ = cap_r.send(CaptureCmd::SetHevc(false));
                if media.codec != VideoCodec::Lrd && !media.viewer_h264 {
                    let _ = cap_r.send(CaptureCmd::SetCodec(VideoCodec::JpegTiles));
                }
                let _ = cap_r.send(CaptureCmd::Keyframe);
            };
            let (state_tx, mut state_rx) = mpsc::unbounded_channel();
            let (rtc, offer) = match RtcSender::start(cap_r.clone(), max_bitrate, state_tx, dc_handler, rtc_codecs).await {
                Ok(v) => v,
                Err(e) => return fallback(&format!("{e:#}")),
            };
            let signal = WebRtcSignal { kind: "offer".into(), sdp: offer };
            if ctrl_r.send(Envelope::new(&sid_r, 0, Payload::WebRtcSignal(signal))).await.is_err() {
                return;
            }
            let answer = match timeout(Duration::from_secs(10), answer_rx.recv()).await {
                Ok(Some(a)) => a,
                _ => {
                    rtc.close().await;
                    return fallback("sem resposta do controlador");
                }
            };
            if let Err(e) = rtc.accept_answer(answer).await {
                rtc.close().await;
                return fallback(&format!("{e:#}"));
            }
            match timeout(Duration::from_secs(10), state_rx.recv()).await {
                Ok(Some(RtcState::Connected)) => {
                    let (h264, h265, fec) = rtc.negotiated().await;
                    tracing::info!("mídia por WebRTC (H.264 {h264}, H.265 {h265}, FEC {fec})");
                    if h265 {
                        let _ = cap_r.send(CaptureCmd::SetHevc(true));
                    }
                    *route_r.lock().unwrap() = Route::Rtc(rtc.clone());
                    let _ = cap_r.send(CaptureCmd::Keyframe);
                }
                _ => {
                    rtc.close().await;
                    return fallback("ICE não conectou");
                }
            }
            // Se o WebRTC cair no meio da sessão, continua por QUIC.
            while let Some(st) = state_rx.recv().await {
                if st == RtcState::Failed {
                    fallback("conexão perdida");
                    break;
                }
            }
            rtc.close().await;
        });

        // Entrada: stream unidirecional viewer → host.
        let conn_i = conn.clone();
        let perms_i = perms.clone();
        let geometry_i = geometry.clone();
        let injector_i = injector.clone();
        let input = tokio::spawn(async move {
            let mut uni = match conn_i.accept_uni().await {
                Ok(u) => u,
                Err(_) => return,
            };
            while let Ok(env) = read_envelope(&mut uni).await {
                if let Some(Payload::InputEvent(ev)) = env.payload {
                    if let Some(inj) = &injector_i {
                        let p = *perms_i.read().unwrap();
                        let g = geometry_i.read().unwrap().clone();
                        apply_input(ev, &p, &g, inj);
                    }
                }
            }
        });

        // Leitor do canal de controle.
        let conn_c = conn.clone();
        let perms_c = perms.clone();
        let geometry_c = geometry.clone();
        // Diagnóstico para o controlador, a cada segundo.
        let stats_src = capture.stats.clone();
        let route_s = route.clone();
        let ctrl_s = ctrl_tx.clone();
        let sid_s = session_id.clone();
        let stats_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut prev = lrd_video::PipelineStats::default();
            loop {
                tick.tick().await;
                let cur = stats_src.lock().map(|s| s.clone()).unwrap_or_default();
                let encoded = cur.encoded.saturating_sub(prev.encoded);
                let encode_us = cur.encode_us.saturating_sub(prev.encode_us);
                let transport = match &*route_s.lock().unwrap() {
                    Route::Pending => "negociando",
                    Route::Rtc(_) => "webrtc",
                    Route::Quic => "quic",
                };
                let msg = lrd_protocol::HostStats {
                    capture_source: cur.source.clone(),
                    encoder: cur.encoder.clone(),
                    width: cur.width,
                    height: cur.height,
                    capture_fps: cur.captured.saturating_sub(prev.captured) as f32,
                    encode_fps: encoded as f32,
                    encode_ms: if encoded > 0 { encode_us as f32 / encoded as f32 / 1000.0 } else { 0.0 },
                    bitrate_kbps: cur.bitrate_bps / 1000,
                    transport: transport.into(),
                };
                prev = cur;
                if ctrl_s.send(Envelope::new(&sid_s, 0, Payload::HostStats(msg))).await.is_err() {
                    break;
                }
            }
        });

        let control = tokio::spawn(async move {
            loop {
                let env = match read_envelope(&mut recv).await {
                    Ok(e) => e,
                    Err(_) => break,
                };
                match env.payload {
                    Some(Payload::SelectDisplay(sd)) => {
                        if !perms_c.read().unwrap().switch_displays {
                            continue;
                        }
                        if let Some(d) = sd.display_id.parse::<u32>().ok().and_then(|id| displays.iter().find(|d| d.id == id)) {
                            *geometry_c.write().unwrap() = d.clone();
                            let _ = cap_cmds.send(CaptureCmd::SelectDisplay(d.id));
                        }
                    }
                    Some(Payload::SetQuality(q)) => {
                        let _ = cap_cmds.send(CaptureCmd::SetQuality(QualitySettings::from_profile(&q.profile, q.max_fps)));
                    }
                    Some(Payload::RequestKeyframe(_)) => {
                        let _ = cap_cmds.send(CaptureCmd::Keyframe);
                    }
                    Some(Payload::WebRtcSignal(sig)) if sig.kind == "answer" => {
                        let _ = answer_tx.try_send(sig.sdp);
                    }
                    Some(Payload::SetAudio(a)) => {
                        let _ = cap_cmds.send(CaptureCmd::SetAudio(a.enabled && media.audio_allowed));
                    }
                    Some(Payload::SessionClose(_)) => {
                        conn_c.close(quinn::VarInt::from_u32(0), reason::USER_CLOSED.as_bytes());
                        break;
                    }
                    _ => {}
                }
            }
        });

        let err = conn.closed().await;
        let why = close_reason(&err);

        video.abort();
        audio_task.abort();
        rtc_task.abort();
        stats_task.abort();
        if let Route::Rtc(rtc) = route.lock().unwrap().clone() {
            tokio::spawn(async move { rtc.close().await });
        }
        input.abort();
        control.abort();
        writer.abort();
        drop(capture);
        if let Some(inj) = &injector {
            inj.send(lrd_input::InjectCmd::ReleaseAll);
        }

        {
            let mut slot = self.inbound.lock().unwrap();
            if slot.as_ref().map(|h| h.session_id == session_id).unwrap_or(false) {
                *slot = None;
            }
        }
        let status = if why == "connection_lost" { "failed" } else { "closed" };
        self.storage.session_ended(&session_id, status, Some(&why))?;
        self.storage.audit(Some(&session_id), "session_ended", serde_json::json!({ "reason": why }));
        self.events.emit(NodeEvent::InboundEnded { session_id, reason: why });
        Ok(())
    }

    // ---------- API usada pela interface ----------

    /// Aprova um pedido pendente com as permissões escolhidas.
    pub fn approve(&self, session_id: &str, permissions: Permissions) -> Result<()> {
        let tx = self
            .pending
            .lock()
            .unwrap()
            .remove(session_id)
            .ok_or_else(|| anyhow!("pedido não encontrado ou já expirado"))?;
        tx.send(Decision::Approve(permissions)).map_err(|_| anyhow!("o pedido já foi encerrado"))
    }

    pub fn reject(&self, session_id: &str) -> Result<()> {
        if let Some(tx) = self.pending.lock().unwrap().remove(session_id) {
            let _ = tx.send(Decision::Reject);
        }
        Ok(())
    }

    /// Encerra a sessão de entrada ativa (revogação imediata — RN-011).
    pub fn end_inbound(&self) {
        if let Some(h) = self.inbound.lock().unwrap().as_ref() {
            h.conn.close(quinn::VarInt::from_u32(0), reason::REVOKED.as_bytes());
        }
    }

    /// Altera as permissões da sessão de entrada em andamento.
    pub fn set_inbound_permissions(&self, permissions: Permissions) -> Result<Permissions> {
        let (session_id, tx, applied) = {
            let guard = self.inbound.lock().unwrap();
            let h = guard.as_ref().ok_or_else(|| anyhow!("nenhuma sessão de entrada ativa"))?;
            let mut p = h.perms.write().unwrap();
            // Só é possível reduzir ou restaurar até o perfil completo de controle remoto.
            let mut next = permissions.intersect(&Permissions::for_profile(Profile::RemoteControl));
            next.view_screen = true;
            *p = next;
            (h.session_id.clone(), h.ctrl_tx.clone(), next)
        };
        if (applied.control_mouse || applied.control_keyboard) && self.injector().is_err() {
            bail!("controle remoto indisponível neste computador");
        }
        let env = Envelope::new(
            &session_id,
            0,
            Payload::PermissionUpdate(PermissionUpdate { granted_permissions: applied.to_list() }),
        );
        let _ = tx.try_send(env);
        self.storage.audit(Some(&session_id), "permissions_changed", serde_json::json!({ "permissions": applied.to_list() }));
        Ok(applied)
    }
}

#[derive(Clone)]
enum Route {
    /// Aguardando a negociação WebRTC (mídia descartada; um quadro completo vai na troca).
    Pending,
    Rtc(Arc<RtcSender>),
    Quic,
}

#[derive(Clone, Copy)]
struct MediaOptions {
    codec: VideoCodec,
    /// O controlador aceita WebRTC.
    webrtc: bool,
    /// O controlador decodifica H.264 também pelo caminho QUIC (WebCodecs).
    viewer_h264: bool,
    audio_allowed: bool,
    audio_on: bool,
    /// Codecs extras do WebRTC aceitos pelo controlador (H.265).
    rtc_codecs: crate::rtc::RtcCodecs,
}

fn audio_supported() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

async fn reject(conn: &quinn::Connection, send: &mut quinn::SendStream, session_id: &str, code: &str) -> Result<()> {
    let resp = ConnectResponse { approved: false, reason_code: code.into(), ..Default::default() };
    write_envelope(send, &Envelope::new(session_id, 2, Payload::ConnectResponse(resp))).await?;
    let _ = send.finish();
    // Dá tempo para o outro lado ler a resposta antes de fechar.
    let _ = timeout(Duration::from_secs(3), conn.closed()).await;
    conn.close(quinn::VarInt::from_u32(0), code.as_bytes());
    Ok(())
}

fn to_display_info(d: &Display) -> DisplayInfo {
    DisplayInfo {
        id: d.id.to_string(),
        name: d.name.clone(),
        width: (d.width as f32 * d.scale_factor.max(1.0)).round() as u32,
        height: (d.height as f32 * d.scale_factor.max(1.0)).round() as u32,
        primary: d.primary,
    }
}

/// Entrada binária vinda do data channel do WebRTC (caminho rápido, sem passar pelo QUIC):
/// `[1,x,y]` mover (u16 LE normalizados), `[2,botão,pressionado]`, `[3,hid,pressionado]`,
/// `[4,dx,dy]` rolagem (i8), `[5,t0]` ping (f64 LE) → resposta `[6,t0,host_us]`.
/// Retorna a resposta a enviar, se houver.
/// Entrada vinda do canal de dados do WebRTC (formato binário compacto, little-endian):
/// - `[1, x u16, y u16, seq u16]` mover (x/y normalizados 0..65535; `seq` descarta atrasados)
/// - `[2, botão, pressionado]`, `[3, hid u16, pressionado]`, `[4, dx i8, dy i8]`
/// - `[5, t0 f64]` → responde `[6, t0, agora_us u64]` (sincronia de relógio e ida e volta)
pub(crate) fn handle_dc_input(
    b: &[u8],
    perms: &RwLock<Permissions>,
    geometry: &RwLock<Display>,
    inj: Option<&lrd_input::InputInjector>,
    last_move: &std::sync::atomic::AtomicU32,
) -> Option<Vec<u8>> {
    use std::sync::atomic::Ordering;
    use lrd_input::InjectCmd;
    let (&kind, rest) = b.split_first()?;
    if kind == 5 && rest.len() >= 8 {
        let mut out = Vec::with_capacity(17);
        out.push(6);
        out.extend_from_slice(&rest[..8]);
        out.extend_from_slice(&lrd_protocol::now_us().to_le_bytes());
        return Some(out);
    }
    let inj = inj?;
    let p = *perms.read().ok()?;
    match kind {
        1 if p.control_mouse && rest.len() >= 4 => {
            if rest.len() >= 6 {
                // Canal sem ordem: ignora um movimento mais antigo que o último aplicado.
                let seq = u16::from_le_bytes([rest[4], rest[5]]) as u32;
                let prev = last_move.load(Ordering::Relaxed);
                if prev <= 0xffff && ((seq.wrapping_sub(prev)) & 0xffff) > 0x8000 {
                    return None;
                }
                last_move.store(seq, Ordering::Relaxed);
            }
            let d = geometry.read().ok()?.clone();
            let nx = u16::from_le_bytes([rest[0], rest[1]]) as f64 / 65535.0;
            let ny = u16::from_le_bytes([rest[2], rest[3]]) as f64 / 65535.0;
            let x = d.x + (nx * d.width.saturating_sub(1) as f64).round() as i32;
            let y = d.y + (ny * d.height.saturating_sub(1) as f64).round() as i32;
            inj.send(InjectCmd::MoveAbs { x, y });
        }
        2 if p.control_mouse && rest.len() >= 2 => {
            inj.send(InjectCmd::Button { button: (rest[0] as u32).min(2), pressed: rest[1] != 0 });
        }
        3 if p.control_keyboard && rest.len() >= 3 => {
            inj.send(InjectCmd::Key { hid: u16::from_le_bytes([rest[0], rest[1]]) as u32, pressed: rest[2] != 0 });
        }
        4 if p.control_mouse && rest.len() >= 2 => {
            inj.send(InjectCmd::Scroll { dx: rest[0] as i8 as i32, dy: rest[1] as i8 as i32 });
        }
        _ => {}
    }
    None
}

/// Aplica um evento de entrada respeitando as permissões. Eventos não permitidos são ignorados.
fn apply_input(ev: InputEvent, perms: &Permissions, display: &Display, inj: &lrd_input::InputInjector) {
    use lrd_input::InjectCmd;
    match ev.event {
        Some(Event::PointerMove(p)) if perms.control_mouse => {
            if p.display_id != display.id.to_string() {
                return; // evento de um monitor anterior à troca
            }
            let nx = p.x.min(65535) as f64 / 65535.0;
            let ny = p.y.min(65535) as f64 / 65535.0;
            let x = display.x + (nx * display.width.saturating_sub(1) as f64).round() as i32;
            let y = display.y + (ny * display.height.saturating_sub(1) as f64).round() as i32;
            inj.send(InjectCmd::MoveAbs { x, y });
        }
        Some(Event::MouseButton(b)) if perms.control_mouse => {
            inj.send(InjectCmd::Button { button: b.button.min(2), pressed: b.pressed });
        }
        Some(Event::MouseScroll(s)) if perms.control_mouse => {
            inj.send(InjectCmd::Scroll { dx: s.delta_x, dy: s.delta_y });
        }
        Some(Event::KeyEvent(k)) if perms.control_keyboard => {
            inj.send(InjectCmd::Key { hid: k.hid_usage, pressed: k.pressed });
        }
        _ => {}
    }
}
