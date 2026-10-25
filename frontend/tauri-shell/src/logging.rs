use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_appender::non_blocking::NonBlocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::prelude::*;

struct LoggingGuards {
    _file_guard: WorkerGuard,
}

static LOGGING_GUARDS: OnceLock<LoggingGuards> = OnceLock::new();

#[derive(Clone, Copy)]
enum ConsoleTarget {
    Stdout,
    Stderr,
}

pub(crate) fn init_gui_logging() -> Result<(), String> {
    init_logging(ConsoleTarget::Stdout, "tyde-shell.log")
}

pub(crate) fn init_host_stdio_logging() -> Result<(), String> {
    init_logging(ConsoleTarget::Stderr, "tyde-host-stdio.log")
}

pub(crate) fn init_host_uds_logging() -> Result<(), String> {
    init_logging(ConsoleTarget::Stderr, "tyde-host-uds.log")
}

pub(crate) fn init_host_bridge_uds_logging() -> Result<(), String> {
    init_logging(ConsoleTarget::Stderr, "tyde-host-bridge-uds.log")
}

fn init_logging(console_target: ConsoleTarget, file_prefix: &str) -> Result<(), String> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console_writer = match console_target {
        ConsoleTarget::Stdout => BoxMakeWriter::new(std::io::stdout),
        ConsoleTarget::Stderr => BoxMakeWriter::new(std::io::stderr),
    };
    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(console_writer)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_line_number(true);

    let (file_writer, file_guard) = build_file_writer(file_prefix)?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_line_number(true);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .try_init()
        .map_err(|err| format!("failed to initialize tracing subscriber: {err}"))?;
    let _ = LOGGING_GUARDS.set(LoggingGuards {
        _file_guard: file_guard,
    });

    Ok(())
}

fn build_file_writer(file_prefix: &str) -> Result<(NonBlocking, WorkerGuard), String> {
    let log_dir = tracing_dir()?;
    std::fs::create_dir_all(&log_dir).map_err(|err| {
        format!(
            "failed to create tracing directory {}: {err}",
            log_dir.display()
        )
    })?;

    let builder = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::HOURLY)
        .filename_prefix(file_prefix);
    let builder = if file_prefix == "tyde-shell.log" {
        builder.max_log_files(48)
    } else {
        builder
    };
    let file_appender = builder
        .build(log_dir)
        .map_err(|error| format!("failed to open tracing file: {error}"))?;
    let (writer, guard) = tracing_appender::non_blocking(file_appender);
    Ok((writer, guard))
}

fn tracing_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("TYDE_TRACING_DIR_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    Ok(server::paths::home_dir()?.join(".tyde").join("tracing"))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StartupPhase {
    #[cfg(desktop)]
    ProcessEnvironment,
    #[cfg(target_os = "macos")]
    WebviewDefaults,
    BuildShell,
    Setup,
    HostStore,
    SpawnHost,
    #[cfg(not(target_os = "windows"))]
    NativeMedia,
    #[cfg(desktop)]
    Updates,
}

static STARTED: OnceLock<std::time::Instant> = OnceLock::new();

pub(crate) struct StartupSpan {
    phase: StartupPhase,
    started: std::time::Instant,
    complete: bool,
}

pub(crate) fn startup(phase: StartupPhase) -> StartupSpan {
    let started = std::time::Instant::now();
    let origin = STARTED.get_or_init(|| started);
    tracing::info!(
        "ui.startup layer=native phase={phase:?} event=begin since_start_ms={}",
        origin.elapsed().as_millis()
    );
    StartupSpan {
        phase,
        started,
        complete: false,
    }
}

impl StartupSpan {
    pub(crate) fn finish(mut self) {
        self.complete = true;
    }
}

impl Drop for StartupSpan {
    fn drop(&mut self) {
        let phase = self.phase;
        let outcome = if self.complete { "complete" } else { "aborted" };
        tracing::info!(
            "ui.startup layer=native phase={phase:?} event=end outcome={outcome} duration_ms={}",
            self.started.elapsed().as_millis()
        );
    }
}

struct DiagnosticRequest {
    record: host_config::diagnostics::FrontendDiagnosticRecord,
    settled: tokio::sync::oneshot::Sender<Result<(), &'static str>>,
}

pub(crate) struct DiagnosticSink(tokio::sync::mpsc::Sender<DiagnosticRequest>);

impl DiagnosticSink {
    pub(crate) async fn write(
        &self,
        record: host_config::diagnostics::FrontendDiagnosticRecord,
    ) -> Result<(), &'static str> {
        if let host_config::diagnostics::FrontendDiagnostic::EventLoopGap {
            longest_work: Some(work),
            ..
        } = &record.diagnostic
            && let Some(frame) = &work.frame
            && serde_json::from_value::<protocol::FrameKind>(serde_json::Value::String(
                frame.as_str().to_owned(),
            ))
            .is_err()
        {
            return Err("Local diagnostic names an unknown frame kind");
        }
        let (settled, result) = tokio::sync::oneshot::channel();
        self.0
            .try_send(DiagnosticRequest { record, settled })
            .map_err(|_| "Local diagnostic queue is full or closed")?;
        result
            .await
            .map_err(|_| "Local diagnostic logger stopped")?
    }
}

struct LogBudget {
    started: std::time::Instant,
    count: u32,
    dropped: u32,
}

impl LogBudget {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            count: 0,
            dropped: 0,
        }
    }
    fn admit(&mut self) -> Option<u32> {
        if self.started.elapsed() >= std::time::Duration::from_secs(60) {
            self.started = std::time::Instant::now();
            self.count = 0;
        }
        if self.count == 32 {
            self.dropped = self.dropped.saturating_add(1);
            return None;
        }
        self.count += 1;
        Some(std::mem::take(&mut self.dropped))
    }
}

pub(crate) fn start_ui_diagnostics(app: &tauri::AppHandle) -> DiagnosticSink {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<DiagnosticRequest>(16);
    tauri::async_runtime::spawn(async move {
        let mut budget = LogBudget::new();
        while let Some(request) = receiver.recv().await {
            let result = match budget.admit() {
                Some(native_dropped) => serde_json::to_string(&request.record)
                    .map(|record| tracing::info!("ui.diagnostic layer=frontend native_dropped={native_dropped} record={record}"))
                    .map_err(|_| "Local diagnostic serialization failed"),
                None => Ok(()),
            };
            let _ = request.settled.send(result);
        }
    });
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut budget = LogBudget::new();
        loop {
            ticks.tick().await;
            let queued = std::time::Instant::now();
            let (done, mut completion) = tokio::sync::oneshot::channel();
            if app
                .run_on_main_thread(move || {
                    let _ = done.send(std::time::Instant::now());
                })
                .is_err()
            {
                tracing::error!("ui.diagnostic layer=native event=dispatch_failed");
                return;
            }
            let completed = tokio::select! {
                result = &mut completion => result,
                _ = ticks.tick() => {
                    if let Some(dropped) = budget.admit() {
                        tracing::warn!("ui.diagnostic layer=native event=dispatch_pending elapsed_ms={} dropped={dropped}", queued.elapsed().as_millis());
                    }
                    completion.await
                }
            };
            match completed {
                Ok(at) if at.duration_since(queued) >= std::time::Duration::from_secs(1) => {
                    if let Some(dropped) = budget.admit() {
                        tracing::warn!(
                            "ui.diagnostic layer=native event=dispatch_completed queue_ms={} dropped={dropped}",
                            at.duration_since(queued).as_millis()
                        );
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::error!("ui.diagnostic layer=native event=dispatch_cancelled");
                    return;
                }
            }
        }
    });
    tracing::info!(
        "ui.diagnostic event=enabled version={} native_probe_ms=1000 frontend_probe_ms=1000 frontend_gap_ms=1250 records_per_minute=32 retention_files=48",
        env!("CARGO_PKG_VERSION")
    );
    DiagnosticSink(sender)
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_web_process(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    window.with_webview(|webview| {
        use std::sync::atomic::{AtomicBool, Ordering};
        use webkit2gtk::{WebProcessTerminationReason, WebViewExt};

        let recorded = AtomicBool::new(false);
        webview.inner().connect_web_process_terminated(move |_, reason| {
            if recorded.swap(true, Ordering::Relaxed) {
                return;
            }
            let reason = match reason {
                WebProcessTerminationReason::Crashed => "crashed",
                WebProcessTerminationReason::ExceededMemoryLimit => "exceeded_memory_limit",
                WebProcessTerminationReason::TerminatedByApi => "terminated_by_api",
                _ => "unrecognized",
            };
            tracing::error!("ui.diagnostic layer=native event=web_process_terminated reason={reason} first_only=true");
        });
    })
}
