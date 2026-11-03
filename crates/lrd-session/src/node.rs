//! `Node`: estado compartilhado de uma instalação (identidade, banco, rede, sessões).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use lrd_discovery::{Announcement, Discovery, DiscoveredDevice};
use lrd_protocol::Envelope;
use lrd_security::{format_fingerprint, Identity, Permissions, RateLimiter};
use lrd_storage::{DeviceRecord, SessionRecord, Storage};

use crate::viewer::OutboundHandle;
use crate::{platform_name, AppSettings, EventSink, NodeConfig, NodeEvent};

const SETTINGS_KEY: &str = "app";

pub(crate) enum Decision {
    Approve(Permissions),
    Reject,
}

pub(crate) struct InboundHandle {
    pub session_id: String,
    pub conn: quinn::Connection,
    pub ctrl_tx: mpsc::Sender<Envelope>,
    pub perms: Arc<RwLock<Permissions>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkInterface {
    pub name: String,
    pub ip: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDeviceInfo {
    pub device_id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub fingerprint: String,
    pub port: u16,
    pub interfaces: Vec<NetworkInterface>,
    pub listening: bool,
    pub listen_error: Option<String>,
    pub discovery_active: bool,
    pub screen_permission: bool,
    pub accessibility_permission: bool,
    pub inbound_session: Option<String>,
}

pub struct Node {
    pub(crate) identity: Identity,
    pub(crate) storage: Arc<Storage>,
    pub(crate) events: Arc<dyn EventSink>,
    pub(crate) settings: RwLock<AppSettings>,
    pub(crate) hostname: String,
    pub(crate) server: Mutex<Option<quinn::Endpoint>>,
    pub(crate) listen_error: Mutex<Option<String>>,
    pub(crate) client: quinn::Endpoint,
    pub(crate) discovery: Mutex<Option<Discovery>>,
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
    pub(crate) inbound: Mutex<Option<InboundHandle>>,
    pub(crate) outbound: Mutex<Option<OutboundHandle>>,
    pub(crate) rate: Mutex<RateLimiter>,
    pub(crate) injector: Mutex<Option<Arc<lrd_input::InputInjector>>>,
}

impl Node {
    /// Cria o nó, abre a escuta QUIC e inicia a descoberta. Deve rodar dentro do runtime Tokio.
    pub async fn start(config: NodeConfig, events: Arc<dyn EventSink>) -> Result<Arc<Node>> {
        let identity = Identity::load_or_create(&config.data_dir).context("identidade do dispositivo")?;
        let storage = Arc::new(Storage::open(&config.data_dir.join("lrd.sqlite3"))?);
        let hostname = gethostname::gethostname().to_string_lossy().to_string();

        let mut settings: AppSettings = storage.get_setting(SETTINGS_KEY)?.unwrap_or_default();
        if settings.device_name.trim().is_empty() {
            settings.device_name = hostname.clone();
        }
        settings.require_approval = true;

        let client = lrd_transport::client_endpoint()?;
        let node = Arc::new(Node {
            identity,
            storage,
            events,
            settings: RwLock::new(settings),
            hostname,
            server: Mutex::new(None),
            listen_error: Mutex::new(None),
            client,
            discovery: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(None),
            outbound: Mutex::new(None),
            rate: Mutex::new(RateLimiter::default()),
            injector: Mutex::new(None),
        });

        node.restart_listener();
        node.restart_discovery();
        tracing::info!("LRD iniciado: {:?}", node.identity);
        Ok(node)
    }

    pub fn settings(&self) -> AppSettings {
        self.settings.read().unwrap().clone()
    }

    pub fn save_settings(self: &Arc<Self>, mut new: AppSettings) -> Result<AppSettings> {
        new.require_approval = true;
        new.device_name = new.device_name.trim().chars().take(63).collect();
        if new.device_name.is_empty() {
            new.device_name = self.hostname.clone();
        }
        if new.listen_port < 1024 {
            new.listen_port = lrd_protocol::DEFAULT_PORT;
        }
        new.max_fps = new.max_fps.clamp(5, 60);
        if !matches!(new.quality_profile.as_str(), "speed" | "balanced" | "quality") {
            new.quality_profile = "balanced".into();
        }
        let old = self.settings();
        self.storage.put_setting(SETTINGS_KEY, &new)?;
        *self.settings.write().unwrap() = new.clone();

        if old.listen_port != new.listen_port {
            self.restart_listener();
        }
        if old.listen_port != new.listen_port
            || old.device_name != new.device_name
            || old.discovery_enabled != new.discovery_enabled
        {
            self.restart_discovery();
        }
        Ok(new)
    }

    pub(crate) fn restart_listener(self: &Arc<Self>) {
        if let Some(old) = self.server.lock().unwrap().take() {
            old.close(quinn::VarInt::from_u32(0), b"restart");
        }
        let port = self.settings().listen_port;
        match lrd_transport::listen(&self.identity, port) {
            Ok(endpoint) => {
                *self.listen_error.lock().unwrap() = None;
                *self.server.lock().unwrap() = Some(endpoint.clone());
                let node = self.clone();
                tokio::spawn(async move {
                    while let Some(incoming) = endpoint.accept().await {
                        let node = node.clone();
                        tokio::spawn(async move { node.handle_incoming(incoming).await });
                    }
                    tracing::debug!("escuta encerrada");
                });
                tracing::info!("escutando em UDP {port}");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::error!("{msg}");
                *self.listen_error.lock().unwrap() = Some(msg.clone());
                self.notice("error", &msg);
            }
        }
    }

    pub(crate) fn restart_discovery(self: &Arc<Self>) {
        if let Some(old) = self.discovery.lock().unwrap().take() {
            old.stop();
        }
        let s = self.settings();
        if !s.discovery_enabled {
            self.events.emit(NodeEvent::DevicesChanged { devices: vec![] });
            return;
        }
        let events = self.events.clone();
        let ann = Announcement {
            device_id: self.identity.device_id.clone(),
            name: s.device_name.clone(),
            hostname: self.hostname.clone(),
            platform: platform_name().into(),
            fingerprint: self.identity.fingerprint.clone(),
            port: s.listen_port,
        };
        match Discovery::start(ann, move |devices| events.emit(NodeEvent::DevicesChanged { devices })) {
            Ok(d) => *self.discovery.lock().unwrap() = Some(d),
            Err(e) => {
                tracing::warn!("descoberta mDNS indisponível: {e:#}");
                self.notice("warning", "Descoberta automática indisponível. Conecte digitando o IP.");
            }
        }
    }

    pub(crate) fn notice(&self, level: &str, message: &str) {
        self.events.emit(NodeEvent::Notice { level: level.into(), message: message.into() });
    }

    pub fn local_info(&self) -> LocalDeviceInfo {
        let s = self.settings();
        LocalDeviceInfo {
            device_id: self.identity.device_id.clone(),
            name: s.device_name,
            hostname: self.hostname.clone(),
            platform: platform_name().into(),
            fingerprint: format_fingerprint(&self.identity.fingerprint),
            port: s.listen_port,
            interfaces: lrd_discovery::local_interfaces()
                .into_iter()
                .map(|(name, ip)| NetworkInterface { name, ip })
                .collect(),
            listening: self.server.lock().unwrap().is_some(),
            listen_error: self.listen_error.lock().unwrap().clone(),
            discovery_active: self.discovery.lock().unwrap().is_some(),
            screen_permission: lrd_video::permissions::has_screen_capture_permission(),
            accessibility_permission: lrd_input::has_accessibility_permission(),
            inbound_session: self.inbound.lock().unwrap().as_ref().map(|h| h.session_id.clone()),
        }
    }

    pub fn discovered_devices(&self) -> Vec<DiscoveredDevice> {
        self.discovery.lock().unwrap().as_ref().map(|d| d.devices()).unwrap_or_default()
    }

    pub fn known_devices(&self) -> Result<Vec<DeviceRecord>> {
        let mut list = self.storage.list_devices()?;
        for d in &mut list {
            d.fingerprint = format_fingerprint(&d.fingerprint);
        }
        Ok(list)
    }

    pub fn set_favorite(&self, device_id: &str, favorite: bool) -> Result<()> {
        self.storage.set_favorite(device_id, favorite)
    }

    /// Esquece um dispositivo (e o fingerprint registrado para ele).
    pub fn forget_device(&self, device_id: &str) -> Result<()> {
        self.storage.audit(None, "device_forgotten", serde_json::json!({ "device_id": device_id }));
        self.storage.delete_device(device_id)
    }

    pub fn history(&self, limit: u32) -> Result<Vec<SessionRecord>> {
        self.storage.list_sessions(limit)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.storage.clear_history()
    }

    pub fn request_screen_permission(&self) -> bool {
        let ok = lrd_video::permissions::request_screen_capture_permission();
        if !ok {
            lrd_input::open_screen_recording_settings();
        }
        ok
    }

    pub fn request_accessibility_permission(&self) {
        lrd_input::open_accessibility_settings();
    }

    /// Encerra tudo (ao fechar o app).
    pub fn shutdown(&self) {
        if let Some(h) = self.inbound.lock().unwrap().as_ref() {
            h.conn.close(quinn::VarInt::from_u32(0), lrd_protocol::reason::USER_CLOSED.as_bytes());
        }
        if let Some(h) = self.outbound.lock().unwrap().as_ref() {
            h.conn.close(quinn::VarInt::from_u32(0), lrd_protocol::reason::USER_CLOSED.as_bytes());
        }
        if let Some(d) = self.discovery.lock().unwrap().take() {
            d.stop();
        }
        if let Some(s) = self.server.lock().unwrap().take() {
            s.close(quinn::VarInt::from_u32(0), b"shutdown");
        }
    }

    pub(crate) fn injector(&self) -> Result<Arc<lrd_input::InputInjector>> {
        let mut slot = self.injector.lock().unwrap();
        if let Some(i) = slot.as_ref() {
            return Ok(i.clone());
        }
        let inj = Arc::new(lrd_input::InputInjector::start()?);
        *slot = Some(inj.clone());
        Ok(inj)
    }
}

/// Converte o erro de encerramento de uma conexão QUIC em código de motivo.
pub(crate) fn close_reason(err: &quinn::ConnectionError) -> String {
    match err {
        quinn::ConnectionError::ApplicationClosed(ac) => {
            let r = String::from_utf8_lossy(&ac.reason).to_string();
            if r.is_empty() {
                lrd_protocol::reason::USER_CLOSED.into()
            } else {
                r
            }
        }
        quinn::ConnectionError::LocallyClosed => lrd_protocol::reason::USER_CLOSED.into(),
        _ => "connection_lost".into(),
    }
}
