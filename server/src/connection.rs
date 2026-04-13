use protocol::{Envelope, FrameError, read_envelope, write_envelope};
use tokio::sync::mpsc;

use crate::Connection;
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

                    connection
                        .incoming_seq
                        .validate(&envelope.stream, envelope.seq, envelope.kind);

                    route_client_envelope(&host, &host_stream, &host_output_stream, envelope).await;
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
    write_envelope(&mut connection.writer, &outgoing).await
}
