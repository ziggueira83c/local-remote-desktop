//! Lado controlador (viewer): conecta, autentica, aguarda aprovação, recebe a tela e envia
//! mouse/teclado.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::timeout;

use lrd_protocol::envelope::Payload;
use lrd_protocol::framing::{read_envelope, read_frame, write_envelope};
use lrd_protocol::input_event::Event;
use lrd_protocol::video::{validate_frame, FRAME_H264, FRAME_TILES};
use lrd_protocol::{
    reason, ConnectRequest, Envelope, Hello, InputEvent, KeyEvent, MouseButton, MouseScroll, PointerMove,
    RequestKeyframe, SelectDisplay, SetAudio, SetQuality, WebRtcSignal, DEFAULT_PORT, MAX_VIDEO_FRAME, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use lrd_security::{fingerprint_of, format_fingerprint, is_allowed_peer_ip, sas_code, Permissions, Profile};

use crate::node::{close_reason, Node};
use crate::{describe_reason, platform_name, FrameSink, NodeEvent, RemoteDisplay};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Um pouco acima do tempo que o host espera pela decisão do usuário (60 s).
const APPROVAL_WAIT: Duration = Duration::from_secs(75);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub address: String,
    pub port: Option<u16>,
    pub expected_fingerprint: Option<String>,
    pub profile: Profile,
    /// Codecs que a interface consegue decodificar (ex.: ["h264", "jpeg-tiles"]).
    #[serde(default)]
    pub codecs: Vec<String>,
    /// Receber o áudio do outro computador.
    #[serde(default)]
    pub audio: bool,
    /// Senha de acesso do outro computador (opcional).
    #[serde(default)]
    pub password: Option<String>,
    /// Guardar a senha no cofre do sistema para as próximas conexões.
    #[serde(default)]
    pub remember_password: bool,
}

/// Evento de entrada vindo da interface.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum InputDto {
    /// `x`, `y` normalizados de 0 a 65535 dentro do monitor remoto.
    Pointer { display_id: String, x: u32, y: u32 },
    Button { button: u32, pressed: bool },
    Key { hid: u32, pressed: bool },
    Scroll { dx: i32, dy: i32 },
}

pub(crate) struct OutboundHandle {
    pub session_id: String,
    pub conn: quinn::Connection,
    pub ctrl_tx: Option<mpsc::Sender<Envelope>>,
    pub input_tx: Option<mpsc::Sender<InputEvent>>,
    pub permissions: Permissions,
}

struct MediaRequest {
    codecs: Vec<String>,
    audio: bool,
    password: Option<String>,
    remember: bool,
}

#[derive(Default)]
struct Stats {
    frames: AtomicU64,
    bytes: AtomicU64,
}

impl Node {
    fn emit_outbound_state(&self, state: &str, remote_name: Option<String>, fingerprint: Option<String>, sas: Option<String>) {
        self.events.emit(NodeEvent::OutboundState { state: state.into(), remote_name, fingerprint, sas });
    }

    /// Conecta a outro computador. Retorna o ID da sessão depois que o outro lado aprovar.
    /// Os quadros de vídeo são entregues a `sink`.
    pub async fn connect(self: &Arc<Self>, params: ConnectParams, sink: FrameSink) -> Result<String> {
        if self.outbound.lock().unwrap().is_some() {
            bail!("Já existe uma sessão de controle aberta. Desconecte antes de abrir outra.");
        }
        let port = params.port.filter(|p| *p != 0).unwrap_or(DEFAULT_PORT);
        let addr = resolve(&params.address, port).await?;
        if !is_allowed_peer_ip(addr.ip()) {
            bail!("Somente endereços da rede local são permitidos ({} não é).", addr.ip());
        }
        let expected = params
            .expected_fingerprint
            .as_deref()
            .map(normalize_fingerprint)
            .filter(|f| f.len() == 64);

        self.emit_outbound_state("connecting", None, None, None);
        let seen = Arc::new(Mutex::new(None));
        let cfg = lrd_transport::client_config(&self.identity, expected.clone(), seen.clone())?;
        let connecting = self.client.connect_with(cfg, addr, "lrd").context("endereço inválido")?;
        let conn = match timeout(CONNECT_TIMEOUT, connecting).await {
            Err(_) => {
                self.emit_outbound_state("failed", None, None, None);
                bail!(
                    "Sem resposta de {addr}. Confira o IP e a porta, se o LRD está aberto no outro computador \
                     e se o firewall permite UDP {port}."
                )
            }
            Ok(Err(e)) => {
                self.emit_outbound_state("failed", None, None, None);
                let msg = e.to_string();
                if expected.is_some() && msg.contains("fingerprint") {
                    bail!("A identidade do computador em {addr} não confere com a anunciada na rede.");
                }
                bail!("Falha ao conectar em {addr}: {msg}");
            }
            Ok(Ok(c)) => c,
        };

        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut slot = self.outbound.lock().unwrap();
            if slot.is_some() {
                conn.close(quinn::VarInt::from_u32(0), reason::BUSY.as_bytes());
                bail!("Já existe uma sessão de controle aberta.");
            }
            *slot = Some(OutboundHandle {
                session_id: session_id.clone(),
                conn: conn.clone(),
                ctrl_tx: None,
                input_tx: None,
                permissions: Permissions::default(),
            });
        }

        let mut codecs: Vec<String> = params
            .codecs
            .iter()
            .filter(|c| {
                *c == lrd_protocol::codec::WEBRTC
                    || *c == lrd_protocol::codec::H264
                    || *c == lrd_protocol::codec::JPEG_TILES
            })
            .cloned()
            .collect();
        if !codecs.iter().any(|c| c == lrd_protocol::codec::JPEG_TILES) {
            codecs.push(lrd_protocol::codec::JPEG_TILES.into());
        }
        let media = MediaRequest {
            codecs,
            audio: params.audio,
            password: params.password.clone().filter(|p| !p.is_empty()),
            remember: params.remember_password,
        };
        match self.clone().establish(conn.clone(), session_id.clone(), addr, params.profile, media, seen, sink).await {
            Ok(()) => Ok(session_id),
            Err(e) => {
                {
                    let mut slot = self.outbound.lock().unwrap();
                    if slot.as_ref().map(|h| h.session_id == session_id).unwrap_or(false) {
                        *slot = None;
                    }
                }
                conn.close(quinn::VarInt::from_u32(0), reason::USER_CLOSED.as_bytes());
                let _ = self.storage.session_ended(&session_id, "failed", Some(&e.to_string()));
                self.emit_outbound_state("failed", None, None, None);
                Err(e)
            }
        }
    }

    async fn establish(
        self: Arc<Self>,
        conn: quinn::Connection,
        session_id: String,
        addr: SocketAddr,
        profile: Profile,
        media: MediaRequest,
        seen: Arc<Mutex<Option<Vec<u8>>>>,
        sink: FrameSink,
    ) -> Result<()> {
        let MediaRequest { codecs, audio: want_audio, password: typed_password, remember } = media;
        let server_cert = seen.lock().unwrap().clone().ok_or_else(|| anyhow!("certificado do servidor ausente"))?;
        let server_fp = fingerprint_of(&server_cert);
        self.emit_outbound_state("authenticating", None, Some(format_fingerprint(&server_fp)), None);

        let (mut send, mut recv) = conn.open_bi().await?;
        let my_hello = Hello {
            device_id: self.identity.device_id.clone(),
            device_name: self.settings().device_name,
            min_version: MIN_PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            codecs,
            platform: platform_name().into(),
        };
        write_envelope(&mut send, &Envelope::new("", 1, Payload::Hello(my_hello))).await?;

        let hello = match timeout(HANDSHAKE_TIMEOUT, read_envelope(&mut recv)).await {
            Err(_) => bail!("O outro computador não respondeu à apresentação."),
            Ok(Err(_)) => bail!("A conexão foi encerrada durante a apresentação."),
            Ok(Ok(env)) => match env.payload {
                Some(Payload::Hello(h)) => h,
                Some(Payload::SessionClose(c)) => bail!(describe_reason(&c.reason_code)),
                _ => bail!("Resposta inesperada do outro computador."),
            },
        };
        if hello.device_id == self.identity.device_id {
            bail!("Esse endereço é deste próprio computador.");
        }
        if hello.max_version < MIN_PROTOCOL_VERSION || hello.min_version > PROTOCOL_VERSION {
            bail!(describe_reason(reason::VERSION));
        }

        // TOFU: o certificado de um dispositivo conhecido não pode mudar.
        if let Some(dev) = self.storage.get_device(&hello.device_id)? {
            if dev.fingerprint != server_fp {
                self.storage.audit(
                    Some(&session_id),
                    "identity_changed",
                    serde_json::json!({ "device_id": hello.device_id, "ip": addr.ip().to_string() }),
                );
                bail!(
                    "A identidade de \"{}\" mudou desde a última conexão. Isso acontece se o app foi \
                     reinstalado lá, mas também pode indicar um ataque. Confira o fingerprint no outro \
                     computador e, se estiver tudo certo, remova-o dos dispositivos conhecidos para \
                     confiar novamente.",
                    dev.display_name
                );
            }
        }

        let remote_name: String = hello.device_name.chars().take(63).collect();
        let sas = sas_code(&self.identity.fingerprint, &server_fp);
        self.storage.insert_session(
            &session_id,
            "outbound",
            "awaiting_approval",
            Some(&remote_name),
            Some(&addr.ip().to_string()),
        )?;
        self.emit_outbound_state(
            "awaiting_approval",
            Some(remote_name.clone()),
            Some(format_fingerprint(&server_fp)),
            Some(sas.clone()),
        );

        // Senha: a digitada agora ou a lembrada para este computador.
        let saved_password = if typed_password.is_none() { self.load_saved_password(&hello.device_id) } else { None };
        let password = typed_password.clone().or(saved_password.clone()).unwrap_or_default();
        if !password.is_empty() {
            self.emit_outbound_state("authenticating", Some(remote_name.clone()), Some(format_fingerprint(&server_fp)), Some(sas.clone()));
        }
        let req = ConnectRequest {
            requested_profile: profile.as_str().into(),
            requested_display: String::new(),
            control_requested: profile != Profile::ViewOnly,
            want_audio,
            password,
        };
        write_envelope(&mut send, &Envelope::new("", 2, Payload::ConnectRequest(req))).await?;

        let resp = match timeout(APPROVAL_WAIT, read_envelope(&mut recv)).await {
            Err(_) => bail!(describe_reason(reason::TIMEOUT)),
            Ok(Err(_)) => bail!("A conexão foi encerrada antes da resposta."),
            Ok(Ok(env)) => match env.payload {
                Some(Payload::ConnectResponse(r)) => r,
                Some(Payload::SessionClose(c)) => bail!(describe_reason(&c.reason_code)),
                _ => bail!("Resposta inesperada do outro computador."),
            },
        };
        if !resp.approved {
            if resp.reason_code == reason::WRONG_PASSWORD && saved_password.is_some() {
                let _ = self.forget_saved_password(&hello.device_id);
                bail!("A senha lembrada para este computador não vale mais. Digite a senha nova.");
            }
            bail!(describe_reason(&resp.reason_code));
        }
        if remember {
            if let Some(p) = &typed_password {
                self.save_password(&hello.device_id, p);
            }
        }
        let permissions = Permissions::from_list(&resp.granted_permissions);

        self.storage.upsert_device(&lrd_storage::NewDevice {
            id: &hello.device_id,
            hostname: &remote_name,
            display_name: &remote_name,
            os_type: &hello.platform,
            cert_der: &server_cert,
            fingerprint: &server_fp,
            last_ip: Some(&addr.ip().to_string()),
            last_port: Some(addr.port()),
        })?;
        self.storage.session_activated(&session_id, &hello.device_id, &resp.granted_permissions)?;

        let stats = Arc::new(Stats::default());
        let close_note: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Vídeo e áudio: o host abre um stream unidirecional para cada um.
        let conn_v = conn.clone();
        let stats_v = stats.clone();
        let video = tokio::spawn(async move {
            let mut readers = Vec::new();
            while let Ok(mut uni) = conn_v.accept_uni().await {
                let conn_r = conn_v.clone();
                let stats_r = stats_v.clone();
                let sink_r = sink.clone();
                readers.push(tokio::spawn(async move {
                    while let Ok(frame) = read_frame(&mut uni, MAX_VIDEO_FRAME).await {
                        if !validate_frame(&frame) {
                            tracing::warn!("quadro de mídia inválido; encerrando");
                            conn_r.close(quinn::VarInt::from_u32(1), reason::PROTOCOL_ERROR.as_bytes());
                            return;
                        }
                        if frame[0] == FRAME_TILES || frame[0] == FRAME_H264 {
                            stats_r.frames.fetch_add(1, Ordering::Relaxed);
                        }
                        stats_r.bytes.fetch_add(frame.len() as u64, Ordering::Relaxed);
                        sink_r(frame);
                    }
                }));
            }
            for r in readers {
                r.abort();
            }
        });

        // Escritor de controle.
        let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<Envelope>(32);
        let writer = tokio::spawn(async move {
            while let Some(env) = ctrl_rx.recv().await {
                if write_envelope(&mut send, &env).await.is_err() {
                    break;
                }
            }
        });

        // Leitor de controle.
        let node_c = self.clone();
        let sid_c = session_id.clone();
        let note_c = close_note.clone();
        let control = tokio::spawn(async move {
            while let Ok(env) = read_envelope(&mut recv).await {
                match env.payload {
                    Some(Payload::PermissionUpdate(u)) => {
                        let p = Permissions::from_list(&u.granted_permissions);
                        if let Some(h) = node_c.outbound.lock().unwrap().as_mut() {
                            if h.session_id == sid_c {
                                h.permissions = p;
                            }
                        }
                        node_c.events.emit(NodeEvent::PermissionsChanged { session_id: sid_c.clone(), permissions: p });
                    }
                    Some(Payload::SessionClose(c)) => {
                        *note_c.lock().unwrap() = Some(c.reason_code);
                    }
                    Some(Payload::HostStats(st)) => {
                        node_c.events.emit(NodeEvent::HostStats {
                            session_id: sid_c.clone(),
                            capture_source: st.capture_source,
                            encoder: st.encoder,
                            width: st.width,
                            height: st.height,
                            capture_fps: st.capture_fps,
                            encode_fps: st.encode_fps,
                            encode_ms: st.encode_ms,
                            bitrate_kbps: st.bitrate_kbps,
                            transport: st.transport,
                        });
                    }
                    Some(Payload::WebRtcSignal(sig)) if sig.kind == "offer" => {
                        node_c.events.emit(NodeEvent::WebRtcOffer { session_id: sid_c.clone(), sdp: sig.sdp });
                    }
                    _ => {}
                }
            }
        });

        // Entrada.
        let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(256);
        let conn_in = conn.clone();
        let sid_in = session_id.clone();
        let input = tokio::spawn(async move {
            let mut uni: Option<quinn::SendStream> = None;
            let mut seq = 0u64;
            while let Some(ev) = input_rx.recv().await {
                if uni.is_none() {
                    match conn_in.open_uni().await {
                        Ok(u) => uni = Some(u),
                        Err(_) => break,
                    }
                }
                seq += 1;
                let env = Envelope::new(&sid_in, seq, Payload::InputEvent(ev));
                if let Some(stream) = uni.as_mut() {
                    if write_envelope(stream, &env).await.is_err() {
                        break;
                    }
                }
            }
        });

        // Métricas a cada segundo.
        let node_m = self.clone();
        let conn_m = conn.clone();
        let sid_m = session_id.clone();
        let metrics = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last = (0u64, 0u64, std::time::Instant::now());
            loop {
                tick.tick().await;
                let frames = stats.frames.load(Ordering::Relaxed);
                let bytes = stats.bytes.load(Ordering::Relaxed);
                let secs = last.2.elapsed().as_secs_f64().max(0.001);
                node_m.events.emit(NodeEvent::Metrics {
                    session_id: sid_m.clone(),
                    fps: (frames - last.0) as f64 / secs,
                    kbps: (bytes - last.1) as f64 * 8.0 / 1000.0 / secs,
                    rtt_ms: conn_m.rtt().as_secs_f64() * 1000.0,
                });
                last = (frames, bytes, std::time::Instant::now());
            }
        });

        {
            let mut slot = self.outbound.lock().unwrap();
            match slot.as_mut() {
                Some(h) if h.session_id == session_id => {
                    h.ctrl_tx = Some(ctrl_tx);
                    h.input_tx = Some(input_tx);
                    h.permissions = permissions;
                }
                _ => bail!("A sessão foi cancelada."),
            }
        }

        self.events.emit(NodeEvent::OutboundStarted {
            session_id: session_id.clone(),
            remote_name: remote_name.clone(),
            remote_platform: hello.platform.clone(),
            displays: resp
                .displays
                .iter()
                .map(|d| RemoteDisplay {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    width: d.width,
                    height: d.height,
                    primary: d.primary,
                })
                .collect(),
            active_display: resp.active_display.clone(),
            permissions,
            fingerprint: format_fingerprint(&server_fp),
            sas,
            video_codec: if resp.video_codec.is_empty() {
                lrd_protocol::codec::JPEG_TILES.into()
            } else {
                resp.video_codec.clone()
            },
            audio_enabled: resp.audio_enabled,
        });

        // Encerramento: limpa tudo quando a conexão cair ou for fechada.
        let node_w = self.clone();
        tokio::spawn(async move {
            let err = conn.closed().await;
            let why = close_note.lock().unwrap().take().unwrap_or_else(|| close_reason(&err));
            video.abort();
            writer.abort();
            control.abort();
            input.abort();
            metrics.abort();
            {
                let mut slot = node_w.outbound.lock().unwrap();
                if slot.as_ref().map(|h| h.session_id == session_id).unwrap_or(false) {
                    *slot = None;
                }
            }
            let status = if why == "connection_lost" { "failed" } else { "closed" };
            let _ = node_w.storage.session_ended(&session_id, status, Some(&why));
            node_w.events.emit(NodeEvent::OutboundEnded { session_id, reason: describe_reason(&why) });
        });

        Ok(())
    }

    /// Envia um evento de mouse/teclado para a sessão de controle ativa.
    pub async fn send_input(&self, dto: InputDto) -> Result<()> {
        let (tx, perms) = {
            let slot = self.outbound.lock().unwrap();
            let h = slot.as_ref().ok_or_else(|| anyhow!("nenhuma sessão ativa"))?;
            (h.input_tx.clone().ok_or_else(|| anyhow!("sessão ainda não iniciada"))?, h.permissions)
        };
        // Filtro local apenas para economizar tráfego; a verificação que vale é a do host.
        let event = match dto {
            InputDto::Pointer { display_id, x, y } if perms.control_mouse => {
                Event::PointerMove(PointerMove { display_id, x: x.min(65535), y: y.min(65535) })
            }
            InputDto::Button { button, pressed } if perms.control_mouse => {
                Event::MouseButton(MouseButton { button, pressed })
            }
            InputDto::Scroll { dx, dy } if perms.control_mouse => {
                Event::MouseScroll(MouseScroll { delta_x: dx, delta_y: dy })
            }
            InputDto::Key { hid, pressed } if perms.control_keyboard => {
                Event::KeyEvent(KeyEvent { hid_usage: hid, pressed })
            }
            _ => return Ok(()),
        };
        tx.send(InputEvent { event: Some(event) }).await.map_err(|_| anyhow!("sessão encerrada"))
    }

    fn send_control(&self, payload: Payload) -> Result<()> {
        let slot = self.outbound.lock().unwrap();
        let h = slot.as_ref().ok_or_else(|| anyhow!("nenhuma sessão ativa"))?;
        let tx = h.ctrl_tx.as_ref().ok_or_else(|| anyhow!("sessão ainda não iniciada"))?;
        tx.try_send(Envelope::new(&h.session_id, 0, payload)).map_err(|_| anyhow!("canal de controle ocupado"))
    }

    pub fn select_display(&self, display_id: String) -> Result<()> {
        self.send_control(Payload::SelectDisplay(SelectDisplay { display_id }))
    }

    pub fn set_quality(&self, profile: String, max_fps: u32) -> Result<()> {
        self.send_control(Payload::SetQuality(SetQuality { profile, max_fps }))
    }

    /// Resposta SDP do WebRTC gerada pela interface.
    pub fn webrtc_answer(&self, sdp: String) -> Result<()> {
        self.send_control(Payload::WebRtcSignal(WebRtcSignal { kind: "answer".into(), sdp }))
    }

    pub fn set_audio(&self, enabled: bool) -> Result<()> {
        self.send_control(Payload::SetAudio(SetAudio { enabled }))
    }

    pub fn request_keyframe(&self) -> Result<()> {
        self.send_control(Payload::RequestKeyframe(RequestKeyframe {}))
    }

    /// Encerra a sessão de controle (ou cancela uma conexão em andamento).
    pub fn disconnect(&self) {
        if let Some(h) = self.outbound.lock().unwrap().as_ref() {
            h.conn.close(quinn::VarInt::from_u32(0), reason::USER_CLOSED.as_bytes());
        }
    }
}

fn normalize_fingerprint(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase()
}

async fn resolve(address: &str, port: u16) -> Result<SocketAddr> {
    let a = address.trim().trim_start_matches('[').trim_end_matches(']');
    if a.is_empty() {
        bail!("Informe o IP ou o nome do computador.");
    }
    if let Ok(ip) = a.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    if let Ok(sa) = a.parse::<SocketAddr>() {
        return Ok(sa);
    }
    let found: Vec<SocketAddr> = tokio::net::lookup_host((a, port))
        .await
        .with_context(|| format!("não foi possível encontrar o computador \"{a}\""))?
        .collect();
    found
        .iter()
        .find(|s| s.is_ipv4())
        .or_else(|| found.first())
        .copied()
        .ok_or_else(|| anyhow!("não foi possível encontrar o computador \"{a}\""))
}
