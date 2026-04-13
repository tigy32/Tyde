mod fixture;

use fixture::Fixture;
use protocol::{
    BootstrapData, Envelope, FrameKind, HelloPayload, PROTOCOL_VERSION, RejectCode, RejectPayload,
    StreamPath, TYDE_VERSION, WelcomePayload, read_envelope, write_envelope,
};
use tokio::io::BufReader;
use uuid::Uuid;

#[tokio::test]
async fn handshake_happy_path() {
    let fixture = Fixture::new().await;

    assert_eq!(fixture.client.outgoing_seq.len(), 1);
}

#[tokio::test]
async fn handshake_rejects_incompatible_protocol() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();
    let client_config = client::ClientConfig {
        protocol_version: 999,
        ..client::ClientConfig::current()
    };

    let server_handle =
        tokio::spawn(async move { server::accept(&server_config, server_stream).await });

    let client_result = client::connect(&client_config, client_stream).await;
    match client_result {
        Err(client::HandshakeError::Rejected(reject)) => {
            assert_eq!(reject.code, protocol::RejectCode::IncompatibleProtocol);
            assert_eq!(reject.server_protocol_version, protocol::PROTOCOL_VERSION);
        }
        _ => panic!("expected incompatible protocol error"),
    }

    let server_result = server_handle.await.expect("server task panicked");
    match server_result {
        Err(server::HandshakeError::IncompatibleProtocol { client, server }) => {
            assert_eq!(client, 999);
            assert_eq!(server, protocol::PROTOCOL_VERSION);
        }
        _ => panic!("expected server incompatible protocol error"),
    }
}

#[tokio::test]
async fn handshake_rejects_wrong_first_frame_kind() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();

    let server_handle =
        tokio::spawn(async move { server::accept(&server_config, server_stream).await });

    let (read_half, mut write_half) = tokio::io::split(client_stream);
    let mut reader = BufReader::new(read_half);

    let stream = StreamPath(format!("/host/{}", Uuid::new_v4()));
    let payload = WelcomePayload {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        bootstrap: BootstrapData::default(),
    };
    let frame = Envelope::from_payload(stream, FrameKind::Welcome, 0, &payload)
        .expect("failed to serialize welcome payload");
    write_envelope(&mut write_half, &frame)
        .await
        .expect("failed to send invalid first frame");

    let reject = read_envelope(&mut reader)
        .await
        .expect("failed to read reject frame")
        .expect("server closed connection without reject");
    assert_eq!(reject.kind, FrameKind::Reject);
    let payload: RejectPayload = reject
        .parse_payload()
        .expect("failed to parse reject payload");
    assert_eq!(payload.code, RejectCode::InvalidHandshake);

    let server_result = server_handle.await.expect("server task panicked");
    match server_result {
        Err(server::HandshakeError::UnexpectedKind { expected, got }) => {
            assert_eq!(expected, FrameKind::Hello);
            assert_eq!(got, FrameKind::Welcome);
        }
        _ => panic!("expected server invalid first-kind error"),
    }
}

#[tokio::test]
async fn handshake_rejects_invalid_stream_path() {
    let (client_stream, server_stream) = tokio::io::duplex(8192);
    let server_config = server::ServerConfig::current();

    let server_handle =
        tokio::spawn(async move { server::accept(&server_config, server_stream).await });

    let (read_half, mut write_half) = tokio::io::split(client_stream);
    let mut reader = BufReader::new(read_half);

    let stream = StreamPath(format!("/invalid/{}", Uuid::new_v4()));
    let payload = HelloPayload {
        protocol_version: PROTOCOL_VERSION,
        tyde_version: TYDE_VERSION,
        client_name: "test-client".to_owned(),
        platform: "test-platform".to_owned(),
    };
    let frame =
        Envelope::from_payload(stream, FrameKind::Hello, 0, &payload).expect("invalid hello");
    write_envelope(&mut write_half, &frame)
        .await
        .expect("failed to send hello frame");

    let reject = read_envelope(&mut reader)
        .await
        .expect("failed to read reject frame")
        .expect("server closed connection without reject");
    assert_eq!(reject.kind, FrameKind::Reject);
    let payload: RejectPayload = reject
        .parse_payload()
        .expect("failed to parse reject payload");
    assert_eq!(payload.code, RejectCode::InvalidHandshake);

    let server_result = server_handle.await.expect("server task panicked");
    match server_result {
        Err(server::HandshakeError::InvalidHandshake(message)) => {
            assert!(message.contains("/host/<uuid>"));
        }
        _ => panic!("expected server invalid stream-path error"),
    }
}
