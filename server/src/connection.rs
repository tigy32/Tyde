use std::sync::Arc;

use protocol::{
    Envelope, FrameError, FrameKind, HostSettingErrorTarget, SetSettingPayload, read_envelope,
};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::Connection;
use crate::error::AppError;
use crate::host::{AgentReplayMode, HostHandle};
use crate::router::route_client_envelope;
use crate::stream::Stream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionOrigin {
    Desktop,
    Mobile,
}

const BOOTSTRAP_REPLAY_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

struct AppLoopResources {
    host: HostHandle,
    host_stream: protocol::StreamPath,
    host_output_stream: Stream,
    inbound_rx: mpsc::UnboundedReceiver<Envelope>,
    incoming_seq: protocol::SeqValidator,
    cancel: CancellationToken,
    origin: ConnectionOrigin,
    first_request: Arc<Notify>,
}

pub async fn run_connection(connection: Connection, host: HostHandle) -> Result<(), FrameError> {
    run_connection_with_origin(connection, host, ConnectionOrigin::Desktop).await
}

pub async fn run_mobile_connection(
    connection: Connection,
    host: HostHandle,
) -> Result<(), FrameError> {
    run_connection_with_origin(connection, host, ConnectionOrigin::Mobile).await
}

async fn run_connection_with_origin(
    connection: Connection,
    host: HostHandle,
    origin: ConnectionOrigin,
) -> Result<(), FrameError> {
    let host_stream = connection
        .outgoing_seq
        .keys()
        .find(|stream| stream.0.starts_with("/host/"))
        .cloned()
        .expect("missing /host/<uuid> stream in connection outgoing sequence map");

    let Connection {
        reader,
        writer,
        incoming_seq,
        outgoing_seq,
    } = connection;

    let (output_tx, output_rx) = mpsc::unbounded_channel::<Envelope>();
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Envelope>();

    let host_output_stream = Stream::new(host_stream.clone(), output_tx.clone());

    let agent_replay = match origin {
        ConnectionOrigin::Desktop => AgentReplayMode::Eager,
        ConnectionOrigin::Mobile => AgentReplayMode::Lazy,
    };
    let deferred_attachments = host
        .register_host_stream(host_output_stream.clone(), agent_replay)
        .await;
    let host_for_attachments = host.clone();
    tokio::spawn(async move {
        for attachment in deferred_attachments {
            host_for_attachments
                .attach_deferred_agent_stream(attachment)
                .await;
        }
    });

    let cancel = CancellationToken::new();

    let reader_task = {
        let cancel = cancel.clone();
        tokio::spawn(reader_loop(reader, inbound_tx, cancel))
    };
    let writer_task = {
        let cancel = cancel.clone();
        tokio::spawn(writer_loop(writer, output_rx, outgoing_seq, cancel))
    };
    let first_request = Arc::new(Notify::new());
    let app_task = {
        let cancel = cancel.clone();
        let host = host.clone();
        let host_stream = host_stream.clone();
        let host_output_stream = host_output_stream.clone();
        let app_first_request = Arc::clone(&first_request);
        tokio::spawn(async move {
            app_loop(AppLoopResources {
                host,
                host_stream,
                host_output_stream,
                inbound_rx,
                incoming_seq,
                cancel,
                origin,
                first_request: app_first_request,
            })
            .await
        })
    };

    let capacity_replay_host = host.clone();
    let capacity_replay_stream = host_stream.clone();
    let capacity_replay_cancel = cancel.clone();
    let capacity_replay_task = tokio::spawn(async move {
        tokio::select! {
            _ = capacity_replay_cancel.cancelled() => return,
            _ = first_request.notified() => {}
            _ = tokio::time::sleep(BOOTSTRAP_REPLAY_GRACE) => {}
        }
        capacity_replay_host
            .replay_backend_capacity_for_host_stream(&capacity_replay_stream)
            .await;
    });

    // Wait for the first task to finish, then tear the scope down.
    // Drain only the two losers — re-polling a JoinHandle that tokio::select!
    // already resolved panics with "JoinHandle polled after completion".
    enum SelectWinner {
        Reader,
        Writer,
        App,
    }
    let mut reader_task = reader_task;
    let mut writer_task = writer_task;
    let mut app_task = app_task;
    let (result, winner) = tokio::select! {
        res = &mut reader_task => {
            cancel.cancel();
            writer_task.abort();
            app_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::Reader)
        }
        res = &mut writer_task => {
            cancel.cancel();
            reader_task.abort();
            app_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::Writer)
        }
        res = &mut app_task => {
            cancel.cancel();
            reader_task.abort();
            writer_task.abort();
            (res.unwrap_or(Ok(())), SelectWinner::App)
        }
    };

    // Drain the two aborted tasks so their Drop runs (socket halves close, fd released).
    match winner {
        SelectWinner::Reader => {
            let _ = writer_task.await;
            let _ = app_task.await;
        }
        SelectWinner::Writer => {
            let _ = reader_task.await;
            let _ = app_task.await;
        }
        SelectWinner::App => {
            let _ = reader_task.await;
            let _ = writer_task.await;
        }
    }
    capacity_replay_task.abort();
    let _ = capacity_replay_task.await;

    host.unregister_host_stream(&host_stream).await;
    result
}

async fn reader_loop<R>(
    mut reader: R,
    inbound_tx: mpsc::UnboundedSender<Envelope>,
    cancel: CancellationToken,
) -> Result<(), FrameError>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            read = read_envelope(&mut reader) => {
                match read? {
                    Some(envelope) => {
                        if inbound_tx.send(envelope).is_err() {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

async fn writer_loop<W>(
    mut writer: W,
    mut output_rx: mpsc::UnboundedReceiver<Envelope>,
    mut outgoing_seq: std::collections::HashMap<protocol::StreamPath, u64>,
    cancel: CancellationToken,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            next = output_rx.recv() => {
                let Some(mut envelope) = next else {
                    return Ok(());
                };
                write_envelope_line(&mut writer, &mut outgoing_seq, &mut envelope).await?;
                const MAX_BATCHED_ENVELOPES: usize = 64;
                for _ in 1..MAX_BATCHED_ENVELOPES {
                    let Ok(mut envelope) = output_rx.try_recv() else {
                        break;
                    };
                    write_envelope_line(&mut writer, &mut outgoing_seq, &mut envelope)
                        .await?;
                }
                writer.flush().await?;
            }
        }
    }
}

async fn write_envelope_line<W>(
    writer: &mut W,
    outgoing_seq: &mut std::collections::HashMap<protocol::StreamPath, u64>,
    envelope: &mut Envelope,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let seq = outgoing_seq.get(&envelope.stream).copied().unwrap_or(0);
    outgoing_seq.insert(envelope.stream.clone(), seq + 1);
    envelope.seq = seq;
    if is_high_volume_code_intel_frame(envelope.kind) {
        tracing::debug!(
            stream = %envelope.stream,
            seq = envelope.seq,
            kind = %envelope.kind,
            "server sending high-volume code-intel envelope"
        );
    } else {
        tracing::info!(
            stream = %envelope.stream,
            seq = envelope.seq,
            kind = %envelope.kind,
            "server sending envelope"
        );
    }
    let mut bytes = serde_json::to_vec(envelope)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn app_loop(resources: AppLoopResources) -> Result<(), FrameError> {
    let AppLoopResources {
        host,
        host_stream,
        host_output_stream,
        mut inbound_rx,
        mut incoming_seq,
        cancel,
        origin,
        first_request,
    } = resources;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe_envelope = inbound_rx.recv() => {
                let Some(envelope) = maybe_envelope else {
                    return Ok(());
                };

                if is_high_volume_code_intel_frame(envelope.kind) {
                    tracing::debug!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        "server received high-volume code-intel envelope"
                    );
                } else {
                    tracing::info!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        "server received envelope"
                    );
                }

                if let Err(error) =
                    incoming_seq.validate(&envelope.stream, envelope.seq, envelope.kind)
                {
                    tracing::warn!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        %error,
                        "closing connection after protocol violation",
                    );
                    return Err(error.into());
                }

                let request_stream = envelope.stream.clone();
                let request_kind = envelope.kind;
                let setting_target = set_setting_error_target(&envelope);
                if origin == ConnectionOrigin::Mobile
                    && is_terminal_control_command(request_kind)
                {
                    let error = AppError::invalid(
                        "mobile_terminal_command",
                        "terminal commands are not allowed from Tyde Mobile",
                    );
                    emit_command_error(
                        &host_output_stream,
                        request_stream,
                        request_kind,
                        setting_target,
                        &error,
                    );
                    first_request.notify_one();
                    continue;
                }

                if let Err(error) =
                    route_client_envelope(&host, &host_stream, &host_output_stream, envelope)
                        .await
                {
                    emit_command_error(
                        &host_output_stream,
                        request_stream,
                        request_kind,
                        setting_target,
                        &error,
                    );

                    if error.fatal {
                        return Ok(());
                    }
                }
                first_request.notify_one();
            }
        }
    }
}

fn is_terminal_control_command(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::TerminalCreate
            | FrameKind::TerminalSend
            | FrameKind::TerminalResize
            | FrameKind::TerminalClose
    )
}

fn is_high_volume_code_intel_frame(kind: FrameKind) -> bool {
    matches!(
        kind,
        FrameKind::CodeIntelSubscribeFile
            | FrameKind::CodeIntelUnsubscribeFile
            | FrameKind::CodeIntelSetVisibleRange
            | FrameKind::CodeIntelHover
            | FrameKind::CodeIntelNavigate
            | FrameKind::CodeIntelFindReferences
            | FrameKind::CodeIntelCancelReferences
            | FrameKind::CodeIntelOverview
            | FrameKind::CodeIntelStatus
            | FrameKind::CodeIntelFileModel
            | FrameKind::CodeIntelDiagnostics
            | FrameKind::CodeIntelHoverResult
            | FrameKind::CodeIntelNavigateResult
            | FrameKind::CodeIntelReferencesResults
            | FrameKind::CodeIntelReferencesComplete
    )
}

fn emit_command_error(
    host_output_stream: &Stream,
    request_stream: protocol::StreamPath,
    request_kind: FrameKind,
    setting_target: Option<HostSettingErrorTarget>,
    error: &AppError,
) {
    if let Some(source) = error.source.as_ref() {
        tracing::warn!(
            operation = error.operation,
            request_kind = %request_kind,
            request_stream = %request_stream,
            code = ?error.kind,
            fatal = error.fatal,
            error = %error,
            source = %source,
            "client command failed"
        );
    } else {
        tracing::warn!(
            operation = error.operation,
            request_kind = %request_kind,
            request_stream = %request_stream,
            code = ?error.kind,
            fatal = error.fatal,
            error = %error,
            "client command failed"
        );
    }

    let payload = command_error_payload(request_stream, request_kind, setting_target, error);
    let payload = match serde_json::to_value(&payload) {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialize command error payload");
            return;
        }
    };
    let _ = host_output_stream.send_value(FrameKind::CommandError, payload);
}

fn command_error_payload(
    request_stream: protocol::StreamPath,
    request_kind: FrameKind,
    setting_target: Option<HostSettingErrorTarget>,
    error: &AppError,
) -> protocol::CommandErrorPayload {
    let mut payload = error.to_payload(request_stream, request_kind);
    if request_kind == FrameKind::SetSetting {
        payload.setting_target = Some(setting_target.unwrap_or(HostSettingErrorTarget::Malformed));
    }
    payload
}

fn set_setting_error_target(envelope: &Envelope) -> Option<HostSettingErrorTarget> {
    if envelope.kind != FrameKind::SetSetting {
        return None;
    }

    Some(
        envelope
            .parse_payload::<SetSettingPayload>()
            .map_or(HostSettingErrorTarget::Malformed, |payload| {
                payload.setting.error_target()
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_terminal_blocklist_covers_all_terminal_control_frames() {
        for kind in [
            FrameKind::TerminalCreate,
            FrameKind::TerminalSend,
            FrameKind::TerminalResize,
            FrameKind::TerminalClose,
        ] {
            assert!(
                is_terminal_control_command(kind),
                "{kind} must be blocked from mobile connections"
            );
        }

        assert!(!is_terminal_control_command(FrameKind::SendMessage));
        assert!(!is_terminal_control_command(FrameKind::HostBrowseStart));
    }

    #[test]
    fn set_setting_command_errors_have_value_free_typed_targets() {
        let reset_projection_id = "projection-reset-01J";
        let reset_state_hash = "sha256:reset-state";
        let cases = [
            (
                protocol::HostSettingValue::ResetTycodeManagedProjection {
                    backend: protocol::BackendKind::Tycode,
                    expected_projection_id: protocol::TycodeProjectionId(
                        reset_projection_id.to_owned(),
                    ),
                    expected_state_hash: protocol::TycodeProjectionStateHash(
                        reset_state_hash.to_owned(),
                    ),
                },
                HostSettingErrorTarget::ResetTycodeManagedProjection,
            ),
            (
                protocol::HostSettingValue::BackendNativeSettings {
                    backend: protocol::BackendKind::Tycode,
                    settings: serde_json::json!({"api_key": "native-secret"}),
                },
                HostSettingErrorTarget::BackendNativeSettings,
            ),
            (
                protocol::HostSettingValue::BackendConfig {
                    backend: protocol::BackendKind::Tycode,
                    values: protocol::BackendConfigValues::default(),
                },
                HostSettingErrorTarget::BackendConfig,
            ),
            (
                protocol::HostSettingValue::EnableMobileConnections { enabled: true },
                HostSettingErrorTarget::EnableMobileConnections,
            ),
        ];

        for (setting, expected_target) in cases {
            let envelope = Envelope::from_payload(
                protocol::StreamPath("/host/error-target".to_owned()),
                FrameKind::SetSetting,
                1,
                &protocol::SetSettingPayload { setting },
            )
            .expect("encode typed setting command");
            let payload = command_error_payload(
                envelope.stream.clone(),
                envelope.kind,
                set_setting_error_target(&envelope),
                &AppError::invalid("set_setting", "setting save failed"),
            );
            assert_eq!(payload.setting_target, Some(expected_target));
            let encoded = serde_json::to_value(&payload).expect("serialize setting error");
            assert_eq!(
                encoded["setting_target"],
                serde_json::to_value(expected_target).expect("serialize setting target")
            );
            let encoded = encoded.to_string();
            assert!(!encoded.contains(reset_projection_id));
            assert!(!encoded.contains(reset_state_hash));
            assert!(!encoded.contains("native-secret"));
        }

        let reset_error = command_error_payload(
            protocol::StreamPath("/host/error-target".to_owned()),
            FrameKind::SetSetting,
            Some(HostSettingErrorTarget::ResetTycodeManagedProjection),
            &AppError::conflict("set_setting", "reset token is stale"),
        );
        let encoded = serde_json::to_string(&reset_error).expect("serialize reset error");
        assert!(encoded.contains("reset_tycode_managed_projection"));
    }

    #[test]
    fn malformed_set_setting_errors_are_typed_and_other_errors_remain_compatible() {
        let malformed = Envelope {
            stream: protocol::StreamPath("/host/error-target".to_owned()),
            kind: FrameKind::SetSetting,
            seq: 1,
            payload: serde_json::json!({
                "setting": {
                    "kind": "reset_tycode_managed_projection",
                    "expected_projection_id": {"unexpected": "raw-token"},
                },
            }),
        };
        let malformed_error = command_error_payload(
            malformed.stream.clone(),
            malformed.kind,
            set_setting_error_target(&malformed),
            &AppError::invalid("set_setting", "invalid setting payload"),
        );
        assert_eq!(
            malformed_error.setting_target,
            Some(HostSettingErrorTarget::Malformed)
        );
        let encoded = serde_json::to_string(&malformed_error).expect("serialize malformed error");
        assert!(!encoded.contains("raw-token"));

        let other_error = command_error_payload(
            protocol::StreamPath("/host/error-target".to_owned()),
            FrameKind::SpawnAgent,
            None,
            &AppError::invalid("spawn_agent", "invalid agent"),
        );
        assert_eq!(other_error.setting_target, None);
        let encoded = serde_json::to_value(&other_error).expect("serialize other command error");
        assert!(encoded.get("setting_target").is_none());
    }
}
