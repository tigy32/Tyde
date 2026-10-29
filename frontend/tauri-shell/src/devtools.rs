use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use devtools_protocol::{
    UiDebugRequest, UiDebugRequestEvent, UiDebugResponse, UiDebugResponseSubmission,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, sleep, timeout};
use uuid::Uuid;

const UI_DEBUG_BIND_ENV: &str = "TYDE_DEV_UI_DEBUG_BIND_ADDR";
const UI_DEBUG_DEFAULT_TIMEOUT_MS: u64 = 5_000;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct VerifiedNativeIdentity {
    instance_id: devtools_protocol::DevInstanceId,
    identity: devtools_protocol::DevNativeIdentity,
}

/// Runs before the first native preference write or webview creation.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn verify_native_identity(
    config: &tauri::Config,
) -> Result<Option<VerifiedNativeIdentity>, String> {
    let instance_id = match std::env::var(devtools_protocol::DEV_INSTANCE_ID_ENV) {
        Ok(instance_id) => devtools_protocol::DevInstanceId(instance_id),
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(format!("Invalid DEV instance identity: {error}")),
    };
    #[cfg(target_os = "macos")]
    let (bundle_identifier, preferences_domain) = observe_macos_identity(
        &devtools_protocol::dev_instance_application_identifier(&instance_id),
    )?;
    #[cfg(not(target_os = "macos"))]
    let (bundle_identifier, preferences_domain) = (None, None);
    let identity = devtools_protocol::DevNativeIdentity {
        application_identifier: config.identifier.clone(),
        bundle_identifier,
        preferences_domain,
    };
    identity.verify_owned(&instance_id)?;
    Ok(Some(VerifiedNativeIdentity {
        instance_id,
        identity,
    }))
}

/// Probes preferences only once the embedded bundle identity is owned, so a
/// misbuilt child never reads another application's domain.
#[cfg(target_os = "macos")]
fn observe_macos_identity(owned: &str) -> Result<(Option<String>, Option<String>), String> {
    let bundle = crate::macos_webview_defaults::main_bundle_identifier()?;
    if bundle.as_deref() != Some(owned) {
        return Ok((bundle, None));
    }
    let preferences = crate::macos_webview_defaults::current_application_preferences_domain(owned)?;
    Ok((bundle, preferences))
}

/// Observes every created webview's actual data store before the host and UI
/// startup signals.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn attest_native_state(
    app: &tauri::App,
    verified: VerifiedNativeIdentity,
) -> Result<(), String> {
    let mut windows: Vec<_> = app.webview_windows().into_iter().collect();
    windows.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut webviews = Vec::with_capacity(windows.len());
    for (label, window) in windows {
        let (sender, receiver) = std::sync::mpsc::channel();
        window
            .with_webview(move |webview| {
                let _ = sender.send(webview_data_store(webview));
            })
            .map_err(|error| format!("Cannot inspect webview {label}: {error}"))?;
        let data_store = receiver
            .try_recv()
            .map_err(|_| format!("Webview {label} was not inspected on the main thread"))??;
        webviews.push(devtools_protocol::DevWebviewNativeState { label, data_store });
    }
    let attestation = devtools_protocol::DevNativeStateAttestation {
        identity: verified.identity,
        webviews,
    };
    attestation.verify_isolated(&verified.instance_id)?;
    devtools_protocol::notify_dev_instance_startup(
        devtools_protocol::DevInstanceStartupSignal::NativeState { attestation },
    )
}

/// Runs before any window exists, so the launcher owns every directory the
/// child creates even when startup fails before it attests native state.
pub fn announce_application_directories(app: &tauri::App) -> Result<(), String> {
    let Some(owner) = devtools_protocol::DevInstanceStartupOwner::from_env()? else {
        return Ok(());
    };
    owner.notify(
        devtools_protocol::DevInstanceStartupSignal::ApplicationDirectories {
            directories: application_directories(app)?,
        },
    )
}

/// Tauri keys these by the configured identifier and creates the local data
/// directory as every webview's data directory, even an incognito one.
fn application_directories(app: &tauri::App) -> Result<Vec<std::path::PathBuf>, String> {
    let paths = app.path();
    let resolved = [
        paths.app_config_dir(),
        paths.app_data_dir(),
        paths.app_local_data_dir(),
        paths.app_cache_dir(),
    ];
    // Elsewhere the log directory lies inside the local data directory.
    #[cfg(target_os = "macos")]
    let resolved = resolved.into_iter().chain([paths.app_log_dir()]);
    let mut directories = resolved
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Cannot resolve application directories: {error}"))?;
    // System frameworks in this process key a cache directory by its bundle
    // identifier under the per-user Darwin cache directory, which ignores HOME.
    #[cfg(target_os = "macos")]
    directories.push(darwin_user_cache_dir()?.join(&app.config().identifier));
    directories.sort();
    directories.dedup();
    Ok(directories)
}

#[cfg(target_os = "macos")]
fn darwin_user_cache_dir() -> Result<std::path::PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let unresolved = || {
        format!(
            "Cannot resolve the Darwin user cache directory: {}",
            std::io::Error::last_os_error()
        )
    };
    let length = unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, std::ptr::null_mut(), 0) };
    if length == 0 {
        return Err(unresolved());
    }
    let mut value = vec![0_u8; length];
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if written == 0 || written > value.len() {
        return Err(unresolved());
    }
    value.truncate(written - 1);
    Ok(std::ffi::OsString::from_vec(value).into())
}

#[cfg(target_os = "macos")]
fn webview_data_store(
    webview: tauri::webview::PlatformWebview,
) -> Result<devtools_protocol::DevWebviewDataStore, String> {
    crate::macos_webview_defaults::webview_data_store(webview)
}

#[cfg(target_os = "linux")]
fn webview_data_store(
    webview: tauri::webview::PlatformWebview,
) -> Result<devtools_protocol::DevWebviewDataStore, String> {
    use webkit2gtk::WebViewExt;
    Ok(if webview.inner().is_ephemeral() {
        devtools_protocol::DevWebviewDataStore::NonPersistent
    } else {
        devtools_protocol::DevWebviewDataStore::Persistent
    })
}

pub struct UiDebugBridgeState {
    ready: AtomicBool,
    pending: Mutex<HashMap<String, oneshot::Sender<UiDebugResponse>>>,
    startup: tokio::sync::mpsc::UnboundedSender<oneshot::Sender<Result<(), String>>>,
}

impl Default for UiDebugBridgeState {
    fn default() -> Self {
        let (startup, mut requests) =
            tokio::sync::mpsc::unbounded_channel::<oneshot::Sender<Result<(), String>>>();
        tauri::async_runtime::spawn(async move {
            let mut outcome: Option<Result<(), String>> = None;
            while let Some(reply) = requests.recv().await {
                let result = match &outcome {
                    Some(result) => result.clone(),
                    None => {
                        let result = match tokio::task::spawn_blocking(|| {
                            devtools_protocol::notify_dev_instance_startup(
                                devtools_protocol::DevInstanceStartupSignal::UiReady,
                            )
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(error) => {
                                Err(format!("DEV startup notification task failed: {error}"))
                            }
                        };
                        outcome = Some(result.clone());
                        result
                    }
                };
                let _ = reply.send(result);
            }
        });
        Self {
            ready: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            startup,
        }
    }
}

impl UiDebugBridgeState {
    pub async fn mark_ready(&self) -> Result<(), String> {
        self.ready.store(true, Ordering::Release);
        let (reply, result) = oneshot::channel();
        self.startup
            .send(reply)
            .map_err(|_| "DEV startup notification owner stopped".to_owned())?;
        result
            .await
            .map_err(|_| "DEV startup notification owner dropped its result".to_owned())?
    }

    pub async fn dispatch_request(
        &self,
        app: &AppHandle,
        request: UiDebugRequest,
    ) -> Result<UiDebugResponse, String> {
        let timeout_ms = request_timeout_ms(&request);
        self.wait_until_ready(timeout_ms).await?;

        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let previous = self.pending.lock().await.insert(request_id.clone(), tx);
        assert!(
            previous.is_none(),
            "duplicate pending UI debug request_id {request_id}"
        );

        let event = UiDebugRequestEvent {
            request_id: request_id.clone(),
            request,
        };
        if let Err(err) = app.emit("tyde://ui-debug-request", event) {
            self.pending.lock().await.remove(&request_id);
            return Err(format!("failed to emit UI debug request: {err}"));
        }

        match timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err("UI debug frontend dropped the response channel".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(format!("UI debug request timed out after {timeout_ms}ms"))
            }
        }
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

    async fn wait_until_ready(&self, timeout_ms: u64) -> Result<(), String> {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self.ready.load(Ordering::Acquire) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("UI debug frontend is not ready".to_string());
            }
            sleep(Duration::from_millis(25)).await;
        }
    }
}

pub fn start_ui_debug_http_server(
    app: &tauri::AppHandle,
    bridge: Arc<UiDebugBridgeState>,
) -> Result<Option<String>, String> {
    let bind_addr = resolve_bind_addr_from_env()?.unwrap_or_else(default_bind_addr);
    let listener = std::net::TcpListener::bind(bind_addr)
        .map_err(|err| format!("failed to bind UI debug listener at {bind_addr}: {err}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("failed to set nonblocking UI debug listener: {err}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to resolve UI debug listener addr: {err}"))?;
    devtools_protocol::notify_dev_instance_startup(
        devtools_protocol::DevInstanceStartupSignal::UiListening {
            address: local_addr,
        },
    )?;

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!("failed to create async UI debug listener: {err}");
                return;
            }
        };

        loop {
            let (stream, remote_addr) = match listener.accept().await {
                Ok(parts) => parts,
                Err(err) => {
                    tracing::error!("UI debug listener accept failed: {err}");
                    break;
                }
            };

            if !remote_addr.ip().is_loopback() {
                tracing::warn!("rejecting non-loopback UI debug client {remote_addr}");
                continue;
            }

            let app = app.clone();
            let bridge = bridge.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = handle_client(app, bridge, stream).await {
                    tracing::warn!("UI debug request failed: {err}");
                }
            });
        }
    });

    Ok(Some(local_addr.to_string()))
}

async fn handle_client(
    app: AppHandle,
    bridge: Arc<UiDebugBridgeState>,
    mut stream: tokio::net::TcpStream,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .map_err(|err| format!("failed to read UI debug request: {err}"))?;
    if bytes.is_empty() {
        return Err("UI debug client sent an empty request".to_string());
    }

    let request: UiDebugRequest = serde_json::from_slice(&bytes)
        .map_err(|err| format!("failed to parse UI debug request JSON: {err}"))?;
    let response = match request {
        UiDebugRequest::CaptureScreenshot { .. } => UiDebugResponse::Error {
            message: "capture_screenshot is not implemented yet".to_string(),
        },
        other => bridge
            .dispatch_request(&app, other)
            .await
            .unwrap_or_else(|message| UiDebugResponse::Error { message }),
    };
    let body = serde_json::to_vec(&response)
        .map_err(|err| format!("failed to serialize UI debug response JSON: {err}"))?;
    stream
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write UI debug response: {err}"))?;
    Ok(())
}

fn request_timeout_ms(request: &UiDebugRequest) -> u64 {
    match request {
        UiDebugRequest::Evaluate { timeout_ms, .. } => {
            timeout_ms.unwrap_or(UI_DEBUG_DEFAULT_TIMEOUT_MS)
        }
        UiDebugRequest::Ping | UiDebugRequest::CaptureScreenshot { .. } => {
            UI_DEBUG_DEFAULT_TIMEOUT_MS
        }
    }
}

fn resolve_bind_addr_from_env() -> Result<Option<SocketAddr>, String> {
    let raw = match std::env::var(UI_DEBUG_BIND_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => return Err(format!("failed to read {UI_DEBUG_BIND_ENV}: {err}")),
    };

    let addr = raw
        .parse::<SocketAddr>()
        .map_err(|err| format!("invalid {UI_DEBUG_BIND_ENV}='{raw}': {err}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "non-loopback {UI_DEBUG_BIND_ENV} is not allowed: {addr}"
        ));
    }

    Ok(Some(addr))
}

fn default_bind_addr() -> SocketAddr {
    "127.0.0.1:0"
        .parse()
        .expect("default UI debug bind addr must parse")
}
