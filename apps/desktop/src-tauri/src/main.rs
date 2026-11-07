// Sem janela de console no Windows em builds de release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod admin;
mod commands;

use std::path::PathBuf;
use std::sync::Arc;

use lrd_session::{EventSink, Node, NodeConfig, NodeEvent};
use tauri::{Emitter, Manager};

pub struct AppState {
    pub node: Arc<Node>,
}

/// Encaminha eventos do núcleo para o frontend (evento "lrd-event").
struct TauriSink(tauri::AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: NodeEvent) {
        if let Err(e) = self.0.emit("lrd-event", &event) {
            tracing::warn!("falha ao emitir evento: {e}");
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Acesso total ligado e o app foi aberto sem ser administrador: abre pela tarefa elevada.
    if admin::redirect_to_elevated() {
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // LRD_DATA_DIR permite rodar duas instâncias na mesma máquina para testes.
            let data_dir = match std::env::var_os("LRD_DATA_DIR") {
                Some(d) => PathBuf::from(d),
                None => app.path().app_data_dir()?,
            };
            let sink = Arc::new(TauriSink(app.handle().clone()));
            let node = tauri::async_runtime::block_on(Node::start(NodeConfig { data_dir }, sink))
                .map_err(|e| -> Box<dyn std::error::Error> { format!("{e:#}").into() })?;
            app.manage(AppState { node });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_local_device,
            commands::list_discovered,
            commands::list_known_devices,
            commands::set_favorite,
            commands::forget_device,
            commands::connect_device,
            commands::disconnect_session,
            commands::send_input,
            commands::select_display,
            commands::set_video_quality,
            commands::request_keyframe,
            commands::set_audio,
            commands::webrtc_answer,
            commands::set_access_password,
            commands::forget_saved_password,
            commands::approve_session,
            commands::reject_session,
            commands::end_inbound_session,
            commands::set_inbound_permissions,
            commands::get_settings,
            commands::save_settings,
            commands::list_history,
            commands::clear_history,
            commands::request_screen_permission,
            commands::request_accessibility_permission,
            commands::admin_status,
            commands::admin_enable,
            commands::admin_disable,
        ])
        .build(tauri::generate_context!())
        .expect("erro ao iniciar o aplicativo")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app.try_state::<AppState>() {
                    state.node.shutdown();
                }
            }
        });
}
