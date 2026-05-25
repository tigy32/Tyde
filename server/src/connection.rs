use protocol::{Envelope, FrameError, FrameKind, read_envelope};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::Connection;
use crate::error::AppError;
use crate::host::HostHandle;
use crate::router::route_client_envelope;
use crate::stream::Stream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionOrigin {
    Desktop,
    Mobile,
}

pub async fn run_connection(connection: Connection, host: HostHandle) -> Result<(), FrameError> {
    run_connection_with_origin(connection, host, ConnectionOrigin::Desktop).await
}

pub(crate) async fn run_mobile_connection(
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

    let deferred_attachments = host.register_host_stream(host_output_stream.clone()).await;
    tokio::spawn(async move {
        for (agent_handle, stream) in deferred_attachments {
            agent_handle.attach(stream).await;
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
    let app_task = {
        let cancel = cancel.clone();
        let host = host.clone();
        let host_stream = host_stream.clone();
        let host_output_stream = host_output_stream.clone();
        tokio::spawn(async move {
            app_loop(
                host,
                host_stream,
                host_output_stream,
                inbound_rx,
                incoming_seq,
                cancel,
                origin,
            )
            .await
        })
    };

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
    tracing::info!(
        stream = %envelope.stream,
        seq = envelope.seq,
        kind = %envelope.kind,
        "server sending envelope"
    );
    let mut bytes = serde_json::to_vec(envelope)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn app_loop(
    host: HostHandle,
    host_stream: protocol::StreamPath,
    host_output_stream: Stream,
    mut inbound_rx: mpsc::UnboundedReceiver<Envelope>,
    mut incoming_seq: protocol::SeqValidator,
    cancel: CancellationToken,
    origin: ConnectionOrigin,
) -> Result<(), FrameError> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            maybe_envelope = inbound_rx.recv() => {
                let Some(envelope) = maybe_envelope else {
                    return Ok(());
                };

                tracing::info!(
                    stream = %envelope.stream,
                    seq = envelope.seq,
                    kind = %envelope.kind,
                    "server received envelope"
                );

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
                        &error,
                    );
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
                        &error,
                    );

                    if error.fatal {
                        return Ok(());
                    }
                }
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

fn emit_command_error(
    host_output_stream: &Stream,
    request_stream: protocol::StreamPath,
    request_kind: FrameKind,
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

    let payload = error.to_payload(request_stream, request_kind);
    let payload = match serde_json::to_value(&payload) {
        Ok(value) => value,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialize command error payload");
            return;
        }
    };
    let _ = host_output_stream.send_value(FrameKind::CommandError, payload);
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
}
