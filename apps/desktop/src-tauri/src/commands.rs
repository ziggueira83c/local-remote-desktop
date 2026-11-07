//! Comandos chamados pelo frontend (IPC do Tauri). Todos são `async` para rodarem no runtime
//! Tokio (a rede e as tarefas de sessão dependem dele).

use std::sync::Arc;

use lrd_session::{
    AppSettings, ConnectParams, DeviceRecord, DiscoveredDevice, InputDto, LocalDeviceInfo, Permissions,
    SessionRecord,
};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

use crate::AppState;

type CmdResult<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub async fn get_local_device(state: State<'_, AppState>) -> CmdResult<LocalDeviceInfo> {
    Ok(state.node.local_info())
}

#[tauri::command]
pub async fn list_discovered(state: State<'_, AppState>) -> CmdResult<Vec<DiscoveredDevice>> {
    Ok(state.node.discovered_devices())
}

#[tauri::command]
pub async fn list_known_devices(state: State<'_, AppState>) -> CmdResult<Vec<DeviceRecord>> {
    state.node.known_devices().map_err(err)
}

#[tauri::command]
pub async fn set_favorite(state: State<'_, AppState>, device_id: String, favorite: bool) -> CmdResult<()> {
    state.node.set_favorite(&device_id, favorite).map_err(err)
}

#[tauri::command]
pub async fn forget_device(state: State<'_, AppState>, device_id: String) -> CmdResult<()> {
    state.node.forget_device(&device_id).map_err(err)
}

/// Conecta e devolve o ID da sessão após a aprovação. Os quadros de vídeo chegam em `on_frame`
/// como bytes crus (sem JSON), no formato descrito em `lrd_protocol::video`.
#[tauri::command]
pub async fn connect_device(
    state: State<'_, AppState>,
    params: ConnectParams,
    on_frame: Channel<InvokeResponseBody>,
) -> CmdResult<String> {
    let sink: lrd_session::FrameSink = Arc::new(move |bytes: Vec<u8>| {
        let _ = on_frame.send(InvokeResponseBody::Raw(bytes));
    });
    let node = state.node.clone();
    node.connect(params, sink).await.map_err(err)
}

#[tauri::command]
pub async fn disconnect_session(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.disconnect();
    Ok(())
}

#[tauri::command]
pub async fn send_input(state: State<'_, AppState>, event: InputDto) -> CmdResult<()> {
    state.node.send_input(event).await.map_err(err)
}

#[tauri::command]
pub async fn select_display(state: State<'_, AppState>, display_id: String) -> CmdResult<()> {
    state.node.select_display(display_id).map_err(err)
}

#[tauri::command]
pub async fn set_video_quality(state: State<'_, AppState>, profile: String, max_fps: u32) -> CmdResult<()> {
    state.node.set_quality(profile, max_fps).map_err(err)
}

#[tauri::command]
pub async fn set_access_password(state: State<'_, AppState>, password: Option<String>) -> CmdResult<()> {
    state.node.set_access_password(password).map_err(err)
}

#[tauri::command]
pub async fn forget_saved_password(state: State<'_, AppState>, device_id: String) -> CmdResult<()> {
    state.node.forget_saved_password(&device_id).map_err(err)
}

#[tauri::command]
pub async fn webrtc_answer(state: State<'_, AppState>, sdp: String) -> CmdResult<()> {
    state.node.webrtc_answer(sdp).map_err(err)
}

#[tauri::command]
pub async fn set_audio(state: State<'_, AppState>, enabled: bool) -> CmdResult<()> {
    state.node.set_audio(enabled).map_err(err)
}

#[tauri::command]
pub async fn request_keyframe(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.request_keyframe().map_err(err)
}

#[tauri::command]
pub async fn approve_session(
    state: State<'_, AppState>,
    session_id: String,
    permissions: Permissions,
) -> CmdResult<()> {
    state.node.approve(&session_id, permissions).map_err(err)
}

#[tauri::command]
pub async fn reject_session(state: State<'_, AppState>, session_id: String) -> CmdResult<()> {
    state.node.reject(&session_id).map_err(err)
}

#[tauri::command]
pub async fn end_inbound_session(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.end_inbound();
    Ok(())
}

#[tauri::command]
pub async fn set_inbound_permissions(
    state: State<'_, AppState>,
    permissions: Permissions,
) -> CmdResult<Permissions> {
    state.node.set_inbound_permissions(permissions).map_err(err)
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CmdResult<AppSettings> {
    Ok(state.node.settings())
}

#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, settings: AppSettings) -> CmdResult<AppSettings> {
    state.node.save_settings(settings).map_err(err)
}

#[tauri::command]
pub async fn list_history(state: State<'_, AppState>, limit: Option<u32>) -> CmdResult<Vec<SessionRecord>> {
    state.node.history(limit.unwrap_or(200).min(1000)).map_err(err)
}

#[tauri::command]
pub async fn clear_history(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.clear_history().map_err(err)
}

#[tauri::command]
pub async fn request_screen_permission(state: State<'_, AppState>) -> CmdResult<bool> {
    Ok(state.node.request_screen_permission())
}

#[tauri::command]
pub async fn request_accessibility_permission(state: State<'_, AppState>) -> CmdResult<()> {
    state.node.request_accessibility_permission();
    Ok(())
}

// ---------- Acesso total (administrador, Windows) ----------

#[tauri::command]
pub async fn admin_status() -> CmdResult<crate::admin::AdminStatus> {
    tauri::async_runtime::spawn_blocking(crate::admin::status).await.map_err(|e| e.to_string())
}

/// Liga o acesso total (um pedido do UAC). Se este processo ainda não é administrador, o app
/// reabre sozinho já elevado.
#[tauri::command]
pub async fn admin_enable(app: tauri::AppHandle, uac_on_normal_desktop: bool) -> CmdResult<crate::admin::AdminStatus> {
    let st = tauri::async_runtime::spawn_blocking(move || {
        crate::admin::enable(uac_on_normal_desktop)?;
        Ok::<_, String>(crate::admin::status())
    })
    .await
    .map_err(|e| e.to_string())??;
    if !st.elevated {
        crate::admin::relaunch_elevated_later()?;
        // Sai logo depois de responder à interface (a nova instância abre em ~2 s).
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            app.exit(0);
        });
    }
    Ok(st)
}

#[tauri::command]
pub async fn admin_disable() -> CmdResult<crate::admin::AdminStatus> {
    tauri::async_runtime::spawn_blocking(|| {
        crate::admin::disable()?;
        Ok::<_, String>(crate::admin::status())
    })
    .await
    .map_err(|e| e.to_string())?
}
