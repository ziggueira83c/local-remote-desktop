//! Lado controlado (host): recebe pedidos, pede aprovação, transmite a tela e aplica a
//! entrada — sempre verificando as permissões concedidas (RN-009).

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use lrd_protocol::envelope::Payload;
use lrd_protocol::framing::{read_envelope, write_envelope, write_frame};
use lrd_protocol::input_event::Event;
use lrd_protocol::{
    reason, ConnectResponse, DisplayInfo, Envelope, Hello, InputEvent, PermissionUpdate, SessionClose,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use lrd_security::{fingerprint_of, format_fingerprint, is_allowed_peer_ip, sas_code, Permissions, Profile};
use lrd_video::{CaptureCmd, CaptureParams, Display, QualitySettings, VideoCodec};

use crate::node::{close_reason, Decision, InboundHandle, Node};
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
            codecs: vec![lrd_protocol::codec::H264.into(), lrd_protocol::codec::JPEG_TILES.into()],
            platform: platform_name().into(),
        };
        write_envelope(&mut send, &Envelope::new("", 1, Payload::Hello(my_hello))).await?;
        // H.264 quando o controlador consegue decodificar; senão, tiles JPEG.
        let codec = if hello.codecs.iter().any(|c| c == lrd_protocol::codec::H264) {
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

        let outcome = tokio::select! {
            r = timeout(APPROVAL_TIMEOUT, rx) => match r {
                Ok(Ok(d)) => Outcome::Decided(d),
                Ok(Err(_)) => Outcome::Decided(Decision::Reject),
                Err(_) => Outcome::Timeout,
            },
            _ = conn.closed() => Outcome::PeerGone,
        };
        self.pending.lock().unwrap().remove(&session_id);

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
            },
            audio_enabled: audio_on,
        };
        write_envelope(&mut send, &Envelope::new(&session_id, 2, Payload::ConnectResponse(resp))).await?;

        let media = MediaOptions { codec, audio_allowed, audio_on };
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
                cursor: Some(Box::new(lrd_input::cursor_position)),
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

        // Vídeo: stream unidirecional host → viewer.
        let conn_v = conn.clone();
        let video = tokio::spawn(async move {
            let mut uni = conn_v.open_uni().await?;
            while let Some(frame) = frame_rx.recv().await {
                write_frame(&mut uni, &frame).await?;
            }
            Ok::<_, anyhow::Error>(())
        });

        // Áudio: stream unidirecional próprio, para não esperar atrás de quadros de vídeo grandes.
        let conn_a = conn.clone();
        let audio_task = tokio::spawn(async move {
            let mut uni: Option<quinn::SendStream> = None;
            while let Some(block) = audio_rx.recv().await {
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

#[derive(Clone, Copy)]
struct MediaOptions {
    codec: VideoCodec,
    audio_allowed: bool,
    audio_on: bool,
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
