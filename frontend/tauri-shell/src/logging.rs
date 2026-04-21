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

    match build_file_writer(file_prefix) {
        Ok((file_writer, file_guard)) => {
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
        }
        Err(err) => {
            eprintln!("warning: failed to initialize Tyde file logging: {err}");
            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .try_init()
                .map_err(|init_err| {
                    format!(
                        "failed to initialize tracing subscriber without file logging: {init_err}"
                    )
                })?;
        }
    }

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

    let file_appender = tracing_appender::rolling::hourly(log_dir, file_prefix);
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

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "failed to resolve HOME for tracing directory".to_string())?;
    Ok(PathBuf::from(home).join(".tyde").join("tracing"))
}
