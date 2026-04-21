use protocol::{Envelope, FrameError, FrameKind, read_envelope, write_envelope};
use tokio::sync::mpsc;

use crate::Connection;
use crate::error::AppError;
use crate::host::HostHandle;
use crate::router::route_client_envelope;
use crate::stream::Stream;

const AGENT_OUTPUT_BUFFER: usize = 256;

pub async fn run_connection(
    mut connection: Connection,
    host: HostHandle,
) -> Result<(), FrameError> {
    let host_stream = connection
        .outgoing_seq
        .keys()
        .find(|stream| stream.0.starts_with("/host/"))
        .cloned()
        .expect("missing /host/<uuid> stream in connection outgoing sequence map");

    let (output_tx, mut output_rx) = mpsc::channel::<Envelope>(AGENT_OUTPUT_BUFFER);
    let host_output_stream = Stream::new(host_stream.clone(), output_tx);

    host.register_host_stream(host_output_stream.clone()).await;

    let result = async {
        loop {
            tokio::select! {
                maybe_outgoing = output_rx.recv() => {
                    if let Some(outgoing) = maybe_outgoing {
                        write_outgoing(&mut connection, outgoing).await?;
                    }
                }
                incoming = read_envelope(&mut connection.reader) => {
                    let incoming = incoming?;
                    let Some(envelope) = incoming else {
                        break;
                    };

                    tracing::info!(
                        stream = %envelope.stream,
                        seq = envelope.seq,
                        kind = %envelope.kind,
                        "server received envelope"
                    );

                    connection
                        .incoming_seq
                        .validate(&envelope.stream, envelope.seq, envelope.kind);

                    let request_stream = envelope.stream.clone();
                    let request_kind = envelope.kind;
                    if let Err(error) =
                        route_client_envelope(&host, &host_stream, &host_output_stream, envelope)
                            .await
                    {
                        emit_command_error(
                            &host_output_stream,
                            request_stream,
                            request_kind,
                            &error,
                        )
                        .await?;

                        if error.fatal {
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
    .await;

    host.unregister_host_stream(&host_stream).await;
    result
}

async fn write_outgoing(
    connection: &mut Connection,
    mut outgoing: Envelope,
) -> Result<(), FrameError> {
    let seq = connection
        .outgoing_seq
        .get(&outgoing.stream)
        .copied()
        .unwrap_or(0);
    connection
        .outgoing_seq
        .insert(outgoing.stream.clone(), seq + 1);

    outgoing.seq = seq;
    tracing::info!(
        stream = %outgoing.stream,
        seq = outgoing.seq,
        kind = %outgoing.kind,
        "server sending envelope"
    );
    write_envelope(&mut connection.writer, &outgoing).await
}

async fn emit_command_error(
    host_output_stream: &Stream,
    request_stream: protocol::StreamPath,
    request_kind: FrameKind,
    error: &AppError,
) -> Result<(), FrameError> {
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
    let payload = serde_json::to_value(&payload).map_err(FrameError::Json)?;
    let _ = host_output_stream
        .send_value(FrameKind::CommandError, payload)
        .await;
    Ok(())
}
