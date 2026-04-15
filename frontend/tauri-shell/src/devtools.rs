use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use devtools_protocol::{UiDebugResponse, UiDebugResponseSubmission};
use tokio::sync::{Mutex, oneshot};

#[derive(Default)]
pub struct UiDebugBridgeState {
    ready: AtomicBool,
    pending: Mutex<HashMap<String, oneshot::Sender<UiDebugResponse>>>,
}

impl UiDebugBridgeState {
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub async fn submit_response(
        &self,
        submission: UiDebugResponseSubmission,
    ) -> Result<(), String> {
        let tx = self.pending.lock().await.remove(&submission.request_id);
        let Some(tx) = tx else {
            return Err(format!(
                "unknown UI debug request_id '{}'",
                submission.request_id
            ));
        };

        tx.send(submission.response)
            .map_err(|_| "failed to deliver UI debug response".to_string())
    }
}

pub fn start_ui_debug_http_server(
    _app: &tauri::AppHandle,
    _bridge: Arc<UiDebugBridgeState>,
) -> Result<Option<String>, String> {
    Ok(None)
}
