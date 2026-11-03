//! Descoberta de pares na LAN via mDNS / DNS-SD (`_lrd._udp.local.`).
//! É apenas auxiliar: a conexão direta por IP funciona mesmo sem multicast.
//! O anúncio publica nome, plataforma, versão do protocolo e fingerprint — nunca credenciais.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;

use lrd_protocol::SERVICE_TYPE;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub fingerprint: String,
    pub addresses: Vec<String>,
    pub port: u16,
    pub protocol_version: u32,
}

pub struct Announcement {
    pub device_id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub fingerprint: String,
    pub port: u16,
}

pub struct Discovery {
    daemon: ServiceDaemon,
    devices: Arc<Mutex<HashMap<String, DiscoveredDevice>>>,
    fullname: String,
}

impl Discovery {
    /// Anuncia este dispositivo e começa a procurar outros. `on_change` é chamado sempre que a
    /// lista de dispositivos encontrados muda.
    pub fn start<F>(ann: Announcement, on_change: F) -> Result<Self>
    where
        F: Fn(Vec<DiscoveredDevice>) + Send + 'static,
    {
        let daemon = ServiceDaemon::new().context("iniciando mDNS")?;

        let short_id: String = ann.device_id.chars().take(8).collect();
        let instance = sanitize(&format!("{}-{}", ann.name, short_id));
        let host = format!("{}.local.", sanitize(&ann.hostname));
        let version = lrd_protocol::PROTOCOL_VERSION.to_string();
        let props: Vec<(&str, &str)> = vec![
            ("id", ann.device_id.as_str()),
            ("name", ann.name.as_str()),
            ("platform", ann.platform.as_str()),
            ("fp", ann.fingerprint.as_str()),
            ("ver", version.as_str()),
        ];
        // IP vazio + enable_addr_auto(): o daemon anuncia os endereços de todas as interfaces.
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", ann.port, &props[..])
            .context("criando anúncio mDNS")?
            .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).context("registrando anúncio mDNS")?;

        let receiver = daemon.browse(SERVICE_TYPE).context("iniciando busca mDNS")?;
        let devices: Arc<Mutex<HashMap<String, DiscoveredDevice>>> = Arc::new(Mutex::new(HashMap::new()));
        let devices_bg = devices.clone();
        let own_id = ann.device_id.clone();

        std::thread::Builder::new()
            .name("lrd-mdns".into())
            .spawn(move || {
                // key = fullname do serviço
                while let Ok(event) = receiver.recv() {
                    let changed = match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let id = info.get_property_val_str("id").unwrap_or_default().to_string();
                            if id.is_empty() || id == own_id {
                                false
                            } else {
                                let mut addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                                // Redes domésticas típicas primeiro; adaptadores virtuais
                                // (WSL, Hyper-V, VPN) e IPv6 por último.
                                addrs.sort_by_key(address_rank);
                                let dev = DiscoveredDevice {
                                    device_id: id,
                                    name: info.get_property_val_str("name").unwrap_or("?").to_string(),
                                    platform: info.get_property_val_str("platform").unwrap_or("other").to_string(),
                                    fingerprint: info.get_property_val_str("fp").unwrap_or_default().to_string(),
                                    addresses: addrs.iter().map(|a| a.to_string()).collect(),
                                    port: info.get_port(),
                                    protocol_version: info
                                        .get_property_val_str("ver")
                                        .and_then(|v| v.parse().ok())
                                        .unwrap_or(0),
                                };
                                devices_bg.lock().unwrap().insert(info.get_fullname().to_string(), dev);
                                true
                            }
                        }
                        ServiceEvent::ServiceRemoved(_ty, fullname) => {
                            devices_bg.lock().unwrap().remove(&fullname).is_some()
                        }
                        _ => false,
                    };
                    if changed {
                        let list: Vec<DiscoveredDevice> = devices_bg.lock().unwrap().values().cloned().collect();
                        on_change(list);
                    }
                }
                tracing::debug!("busca mDNS encerrada");
            })?;

        Ok(Discovery { daemon, devices, fullname })
    }

    pub fn devices(&self) -> Vec<DiscoveredDevice> {
        self.devices.lock().unwrap().values().cloned().collect()
    }

    pub fn stop(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
    }
}

/// Endereços IPv4 não-loopback das interfaces locais.
pub fn local_ipv4_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .filter(|i| !i.is_loopback())
                .map(|i| i.ip())
                .filter(|ip| ip.is_ipv4())
                .collect()
        })
        .unwrap_or_default()
}

/// Interfaces locais (nome, IP) para exibição.
pub fn local_interfaces() -> Vec<(String, String)> {
    if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .filter(|i| !i.is_loopback() && i.ip().is_ipv4())
                .map(|i| (i.name.clone(), i.ip().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn address_rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            match o[0] {
                192 if o[1] == 168 => 0,
                10 => 1,
                172 if (16..=31).contains(&o[1]) => 2,
                169 if o[1] == 254 => 4,
                _ => 3,
            }
        }
        IpAddr::V6(_) => 5,
    }
}

fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "lrd-device".to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}
