#[cfg(debug_assertions)]
mod debug_http;
mod mqtt_connection;
mod paired_hosts;
mod psk_store;
mod types;

use std::sync::Arc;

use host_config::HostLineEvent;
use mqtt_transport::{MOBILE_QR_VERSION, MobilePairingQrPayload};
use protocol::{MobileAccessErrorCode, PROTOCOL_VERSION};
use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::paired_hosts::{PairedHostRecord, credential_fingerprint};
use crate::psk_store::{PskStore, SystemPskStore};
use crate::types::{
    KnownConnectionInstance, LocalHostId, MobilePairingPreview, MobileShellErrorEvent,
    PairedHostConnectionStatusEvent, PairedHostSummary, PairedHostsChangedEvent,
};

const MOBILE_SHELL_ERROR_EVENT: &str = "tyde://mobile-shell-error";

struct MobileShellState {
    paired_hosts: Arc<paired_hosts::Store>,
    connections: Arc<mqtt_connection::Manager>,
    psk_store: Arc<dyn PskStore>,
    #[cfg(debug_assertions)]
    ui_debug: Arc<debug_http::UiDebugBridgeState>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MobileCommandError {
    code: MobileAccessErrorCode,
    message: String,
}

impl MobileCommandError {
    fn new(code: MobileAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<paired_hosts::StoreError> for MobileCommandError {
    fn from(error: paired_hosts::StoreError) -> Self {
        Self::new(MobileAccessErrorCode::StoreLoadFailed, error.to_string())
    }
}

impl From<psk_store::PskStoreError> for MobileCommandError {
    fn from(error: psk_store::PskStoreError) -> Self {
        Self::new(MobileAccessErrorCode::KeystoreFailed, error.to_string())
    }
}

impl From<mqtt_connection::ManagerError> for MobileCommandError {
    fn from(error: mqtt_connection::ManagerError) -> Self {
        let code = match &error {
            mqtt_connection::ManagerError::Store(_) => MobileAccessErrorCode::StoreLoadFailed,
            mqtt_connection::ManagerError::PskStore(_) => MobileAccessErrorCode::KeystoreFailed,
            mqtt_connection::ManagerError::SendLineFailed { .. }
            | mqtt_connection::ManagerError::ConnectionActorStopped { .. }
            | mqtt_connection::ManagerError::ConnectionNotFound(_)
            | mqtt_connection::ManagerError::ActorStopped
            | mqtt_connection::ManagerError::ResponseClosed => {
                MobileAccessErrorCode::TransportFailed
            }
        };
        Self::new(code, error.to_string())
    }
}

#[tauri::command]
async fn list_paired_hosts(
    state: tauri::State<'_, MobileShellState>,
) -> Result<Vec<PairedHostSummary>, MobileCommandError> {
    state
        .paired_hosts
        .list_summaries()
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn list_paired_host_connection_statuses(
    state: tauri::State<'_, MobileShellState>,
) -> Result<Vec<PairedHostConnectionStatusEvent>, MobileCommandError> {
    state
        .connections
        .connection_statuses()
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn list_pending_host_lines(
    state: tauri::State<'_, MobileShellState>,
) -> Result<Vec<HostLineEvent>, MobileCommandError> {
    state
        .connections
        .pending_host_lines()
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn ack_host_line(
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
    delivery_id: u64,
) -> Result<(), MobileCommandError> {
    state
        .connections
        .ack_host_line(local_host_id, delivery_id)
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn frontend_attached(
    state: tauri::State<'_, MobileShellState>,
    known_connection_instance_ids: Option<Vec<KnownConnectionInstance>>,
) -> Result<(), MobileCommandError> {
    state
        .connections
        .frontend_attached(known_connection_instance_ids.unwrap_or_default())
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn preview_pairing_uri(qr_uri: String) -> Result<MobilePairingPreview, MobileCommandError> {
    let qr_payload = parse_qr_uri(&qr_uri)?;
    validate_qr_payload(&qr_payload)?;
    Ok(MobilePairingPreview {
        host_label: normalize_host_label(qr_payload.host_label)?,
        broker_url: qr_payload.broker.url,
    })
}

#[tauri::command]
async fn start_pairing(
    app: tauri::AppHandle,
    state: tauri::State<'_, MobileShellState>,
    qr_uri: String,
) -> Result<(), MobileCommandError> {
    let qr_payload = parse_qr_uri(&qr_uri)?;
    validate_qr_payload(&qr_payload)?;

    let key_id = state.psk_store.store(&qr_payload.psk)?;
    let fingerprint = credential_fingerprint(&qr_payload.broker, &qr_payload.room, &qr_payload.psk);
    let record = PairedHostRecord {
        local_host_id: LocalHostId(uuid::Uuid::new_v4().to_string()),
        host_label: normalize_host_label(qr_payload.host_label.clone())?,
        broker: qr_payload.broker.clone(),
        room: qr_payload.room,
        psk_keychain_key_id: key_id.clone(),
        credential_fingerprint: fingerprint,
        auto_connect: true,
        last_connected_at_ms: None,
    };
    let local_host_id = record.local_host_id.clone();

    if let Err(store_error) = state.paired_hosts.insert(record).await {
        match state.psk_store.delete(&key_id) {
            Ok(()) => return Err(store_error.into()),
            Err(cleanup_error) => {
                return Err(MobileCommandError::new(
                    MobileAccessErrorCode::StoreLoadFailed,
                    format!(
                        "failed to persist paired host after storing PSK: {store_error}; PSK cleanup failed: {cleanup_error}"
                    ),
                ));
            }
        }
    }

    if let Err(connect_error) = state.connections.connect(local_host_id.clone()).await {
        let _remove_result = state.paired_hosts.remove(local_host_id).await;
        let _delete_result = state.psk_store.delete(&key_id);
        return Err(connect_error.into());
    }

    emit_paired_hosts_changed(&app, &state.paired_hosts).await;
    Ok(())
}

#[tauri::command]
async fn connect_paired_host(
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
) -> Result<(), MobileCommandError> {
    state
        .connections
        .connect(local_host_id)
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn disconnect_paired_host(
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
) -> Result<(), MobileCommandError> {
    state
        .connections
        .disconnect(local_host_id)
        .await
        .map_err(Into::into)
}

#[tauri::command]
async fn forget_paired_host(
    app: tauri::AppHandle,
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
) -> Result<(), MobileCommandError> {
    let record = state.paired_hosts.get(local_host_id.clone()).await?;
    match state.connections.disconnect(local_host_id.clone()).await {
        Ok(()) => {}
        Err(mqtt_connection::ManagerError::ConnectionNotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    state.psk_store.delete(&record.psk_keychain_key_id)?;
    state.paired_hosts.remove(local_host_id).await?;
    emit_paired_hosts_changed(&app, &state.paired_hosts).await;
    Ok(())
}

#[tauri::command]
async fn set_paired_host_auto_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
    auto_connect: bool,
) -> Result<(), MobileCommandError> {
    state
        .paired_hosts
        .set_auto_connect(local_host_id, auto_connect)
        .await?;
    emit_paired_hosts_changed(&app, &state.paired_hosts).await;
    Ok(())
}

#[tauri::command]
async fn send_host_line(
    state: tauri::State<'_, MobileShellState>,
    local_host_id: LocalHostId,
    line: String,
) -> Result<(), MobileCommandError> {
    state
        .connections
        .send_line(local_host_id, line)
        .await
        .map_err(Into::into)
}

/// Diagnostic: WASM calls this to log messages visible on the Rust side.
#[tauri::command]
async fn wasm_log(level: String, message: String) -> Result<(), String> {
    match level.as_str() {
        "error" => tracing::error!("[wasm] {message}"),
        "warn" => tracing::warn!("[wasm] {message}"),
        "trace" => tracing::trace!("[wasm] {message}"),
        _ => tracing::info!("[wasm] {message}"),
    }
    Ok(())
}

#[cfg(debug_assertions)]
#[tauri::command]
async fn mark_ui_debug_ready(
    state: tauri::State<'_, MobileShellState>,
    session_id: String,
) -> Result<(), String> {
    state.ui_debug.mark_ready(session_id).await;
    Ok(())
}

#[cfg(debug_assertions)]
#[tauri::command]
async fn take_ui_debug_request(
    state: tauri::State<'_, MobileShellState>,
    session_id: String,
) -> Result<String, String> {
    tracing::trace!("[CMD] take_ui_debug_request session_id={session_id}");
    let result = match state.ui_debug.take_request(session_id).await {
        Some(request) => serde_json::to_string(&request)
            .map_err(|error| format!("failed to serialize UI debug request: {error}")),
        None => Ok("null".to_string()),
    };
    tracing::trace!("[CMD] take_ui_debug_request done");
    result
}

#[cfg(debug_assertions)]
#[tauri::command]
async fn submit_ui_debug_response(
    state: tauri::State<'_, MobileShellState>,
    request_id: String,
    response: devtools_protocol::UiDebugResponse,
) -> Result<(), String> {
    state
        .ui_debug
        .submit_response(devtools_protocol::UiDebugResponseSubmission {
            request_id,
            response,
        })
        .await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init()
    {
        eprintln!("failed to initialize mobile shell logging: {error}");
    }

    tracing::info!("starting tyde mobile shell");

    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    let builder = builder.setup(move |app| {
        tracing::info!("mobile setup: loading paired hosts");

        let app_handle = app.handle().clone();
        let (paired_hosts_store, load_failure) =
            tauri::async_runtime::block_on(paired_hosts::Store::open(&app_handle));
        let paired_hosts = Arc::new(paired_hosts_store);
        let psk_store: Arc<dyn PskStore> = Arc::new(SystemPskStore::new());
        let connections = Arc::new(mqtt_connection::Manager::start(
            app_handle.clone(),
            paired_hosts.clone(),
            psk_store.clone(),
        ));
        #[cfg(debug_assertions)]
        let ui_debug = Arc::new(debug_http::UiDebugBridgeState::default());

        app.manage(MobileShellState {
            paired_hosts: paired_hosts.clone(),
            connections: connections.clone(),
            psk_store,
            #[cfg(debug_assertions)]
            ui_debug: ui_debug.clone(),
        });

        #[cfg(debug_assertions)]
        debug_http::start(app.handle().clone(), ui_debug);
        spawn_boot_flow(app_handle.clone(), paired_hosts, connections, load_failure);

        #[cfg(all(debug_assertions, not(mobile)))]
        {
            if let Some(webview) = app.webview_windows().values().next() {
                webview.open_devtools();
            }
        }

        Ok(())
    });

    #[cfg(debug_assertions)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        list_paired_hosts,
        list_paired_host_connection_statuses,
        list_pending_host_lines,
        ack_host_line,
        frontend_attached,
        preview_pairing_uri,
        start_pairing,
        connect_paired_host,
        disconnect_paired_host,
        forget_paired_host,
        set_paired_host_auto_connect,
        send_host_line,
        wasm_log,
        mark_ui_debug_ready,
        take_ui_debug_request,
        submit_ui_debug_response
    ]);

    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        list_paired_hosts,
        list_paired_host_connection_statuses,
        list_pending_host_lines,
        ack_host_line,
        frontend_attached,
        preview_pairing_uri,
        start_pairing,
        connect_paired_host,
        disconnect_paired_host,
        forget_paired_host,
        set_paired_host_auto_connect,
        send_host_line,
        wasm_log
    ]);

    if let Err(error) = builder.run(tauri::generate_context!()) {
        tracing::error!(error = %error, "mobile shell exited with error");
    }
}

fn spawn_boot_flow(
    app: tauri::AppHandle,
    paired_hosts: Arc<paired_hosts::Store>,
    connections: Arc<mqtt_connection::Manager>,
    load_failure: Option<paired_hosts::StoreLoadFailure>,
) {
    tauri::async_runtime::spawn(async move {
        if let Some(failure) = load_failure {
            let message = match failure.path {
                Some(path) => format!(
                    "failed to load paired hosts from {}: {}",
                    path.display(),
                    failure.message
                ),
                None => failure.message,
            };
            emit_shell_error(&app, MobileAccessErrorCode::StoreLoadFailed, message);
            return;
        }

        match paired_hosts.list_records().await {
            Ok(records) => {
                for record in records.iter().filter(|record| record.auto_connect) {
                    if let Err(error) = connections.connect(record.local_host_id.clone()).await {
                        emit_shell_error(
                            &app,
                            MobileAccessErrorCode::TransportFailed,
                            format!(
                                "failed to auto-connect paired host {}: {error}",
                                record.local_host_id
                            ),
                        );
                    }
                }
                emit_paired_hosts_changed(&app, &paired_hosts).await;
            }
            Err(error) => emit_shell_error(
                &app,
                MobileAccessErrorCode::StoreLoadFailed,
                error.to_string(),
            ),
        }
    });
}

async fn emit_paired_hosts_changed(app: &tauri::AppHandle, store: &paired_hosts::Store) {
    match store.list_summaries().await {
        Ok(hosts) => {
            if let Err(error) = app.emit(
                mqtt_connection::PAIRED_HOSTS_CHANGED_EVENT,
                PairedHostsChangedEvent { hosts },
            ) {
                tracing::warn!(error = %error, "failed to emit paired hosts changed event");
            }
        }
        Err(error) => emit_shell_error(
            app,
            MobileAccessErrorCode::StoreLoadFailed,
            error.to_string(),
        ),
    }
}

fn emit_shell_error(app: &tauri::AppHandle, code: MobileAccessErrorCode, message: String) {
    if let Err(error) = app.emit(
        MOBILE_SHELL_ERROR_EVENT,
        MobileShellErrorEvent {
            code,
            message: message.clone(),
        },
    ) {
        tracing::warn!(emit_error = %error, error = %message, "failed to emit mobile shell error event");
    }
}

fn parse_qr_uri(qr_uri: &str) -> Result<MobilePairingQrPayload, MobileCommandError> {
    MobilePairingQrPayload::from_any(qr_uri).map_err(|error| {
        MobileCommandError::new(
            MobileAccessErrorCode::InvalidPairingQr,
            format!("invalid mobile pairing URI: {error}"),
        )
    })
}

fn validate_qr_payload(payload: &MobilePairingQrPayload) -> Result<(), MobileCommandError> {
    if payload.v != MOBILE_QR_VERSION {
        return Err(MobileCommandError::new(
            MobileAccessErrorCode::InvalidPairingQr,
            format!(
                "unsupported mobile pairing QR version {}, expected {}",
                payload.v, MOBILE_QR_VERSION
            ),
        ));
    }
    if payload.protocol_version != PROTOCOL_VERSION {
        return Err(MobileCommandError::new(
            MobileAccessErrorCode::VersionMismatch,
            format!(
                "unsupported Tyde protocol version {}, expected {}",
                payload.protocol_version, PROTOCOL_VERSION
            ),
        ));
    }
    let _ = normalize_host_label(payload.host_label.clone())?;
    Ok(())
}

fn normalize_host_label(host_label: String) -> Result<String, MobileCommandError> {
    let trimmed = host_label.trim().to_owned();
    if trimmed.is_empty() {
        return Err(MobileCommandError::new(
            MobileAccessErrorCode::InvalidPairingQr,
            "mobile pairing QR host_label must not be empty",
        ));
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mqtt_transport::{PreSharedKey, RoomId, default_mobile_broker_endpoint};
    use protocol::{TYDE_VERSION, Version};

    fn valid_payload() -> MobilePairingQrPayload {
        MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            default_mobile_broker_endpoint(),
            RoomId([1_u8; 16]),
            PreSharedKey::from_slice(&[2_u8; 32]).expect("psk"),
            "Tyde Host".to_owned(),
        )
    }

    #[test]
    fn pairing_validation_accepts_different_tyde_patch_versions() {
        let mut payload = valid_payload();
        payload.tyde_version = Version {
            patch: TYDE_VERSION.patch + 1,
            ..TYDE_VERSION
        };

        validate_qr_payload(&payload).expect("compatible protocol version is enough");
    }

    #[test]
    fn pairing_validation_still_rejects_protocol_mismatch() {
        let mut payload = valid_payload();
        payload.protocol_version = PROTOCOL_VERSION + 1;

        let error = validate_qr_payload(&payload).expect_err("protocol mismatch");
        assert_eq!(error.code, MobileAccessErrorCode::VersionMismatch);
        assert!(error.message.contains("protocol version"));
    }
}
