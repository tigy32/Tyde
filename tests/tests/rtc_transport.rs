use rtc_transport::Peer;
use std::time::Duration;
use tests::rtc::{connect, relay};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn real_turn_preserves_bulk_backpressure_and_server_protocol_on_reconnect() {
    {
        let fixture_binary = std::env::var_os("NEXTEST_BIN_EXE_tyde_rtc_fixture")
            .expect("nextest must provide the current browser relay fixture");
        let fixture_exists = std::path::Path::new(&fixture_binary).is_file();
        eprintln!("RTC test paths: runtime_fixture_exists={fixture_exists}");
        assert!(
            fixture_exists,
            "browser relay fixture must be built with native tests"
        );
        tracing_subscriber::fmt()
            .with_env_filter("rtc_transport=debug")
            .with_test_writer()
            .try_init()
            .expect("transport diagnostics");
        eprintln!("TURN flow: start relay");
        let relay = relay().await;
        let ice = &relay.tls_ice;
        let roots = &relay.roots;
        let mut untrusted = Peer::new(std::slice::from_ref(ice))
            .await
            .expect("untrusted TLS peer setup");
        let error = untrusted
            .offer()
            .await
            .expect_err("an untrusted TURN certificate must fail");
        assert!(
            error.to_string().contains("certificate"),
            "TLS failure must report certificate verification: {error}"
        );
        drop(untrusted);
        let mut wrong_name = ice.clone();
        wrong_name.urls[0].0 = format!("turns:{}?transport=tcp", relay.tls_address);
        let mut mismatched = Peer::with_tls_roots(&[wrong_name], roots.clone())
            .await
            .expect("hostname mismatch setup");
        let error = mismatched
            .offer()
            .await
            .expect_err("a trusted certificate for the wrong hostname must fail");
        assert!(
            error.to_string().contains("certificate"),
            "hostname failure must report certificate verification: {error}"
        );
        drop(mismatched);
        let (mut mobile, mut host) = connect(ice, roots).await;
        assert!(
            relay.interrupt_traffic(Duration::from_secs(8)).await > 0,
            "the real relay must drop traffic beyond ICE's five-second disconnect threshold"
        );
        eprintln!("TURN flow: relay restored after temporary packet loss");
        let bulk: Vec<u8> = (0..4 * 1024 * 1024)
            .map(|index| (index % 251) as u8)
            .collect();
        let expected = bulk.clone();
        let writer = tokio::spawn(async move {
            mobile.write_all(&bulk).await.expect("write bulk");
            mobile.flush().await.expect("peer must acknowledge bulk");
            eprintln!("TURN flow: bulk acknowledged by host");
            let mut reply = [0; 5];
            mobile
                .read_exact(&mut reply)
                .await
                .expect("bidirectional reply");
            assert_eq!(&reply, b"ready");
            eprintln!("TURN flow: reverse reply received");
        });
        let mut received = vec![0; expected.len()];
        host.read_exact(&mut received)
            .await
            .expect("receive full bulk without overflowing");
        assert_eq!(
            received, expected,
            "TURN transport must preserve bytes and ordering"
        );
        host.write_all(b"ready")
            .await
            .expect("write reverse direction");
        host.flush().await.expect("flush reverse direction");
        eprintln!("TURN flow: reverse reply acknowledged");
        writer.await.expect("bulk task");
        drop(host);

        let store = tempfile::tempdir().expect("host store");
        let host_handle = server::spawn_host_with_mock_backend(
            store.path().join("sessions.json"),
            store.path().join("projects.json"),
            store.path().join("settings.json"),
        )
        .expect("real Tyde server");
        for iteration in 0..2 {
            eprintln!("TURN flow: server reconnect {iteration}");
            let (mobile, host) = connect(ice, roots).await;
            let server_host = host_handle.clone();
            let server_task = tokio::spawn(async move {
                let accepted = server::accept(&server::ServerConfig::current(), host)
                    .await
                    .expect("real server handshake over TURN");
                server::run_connection(accepted, server_host).await
            });
            let mut client = client::connect(&client::ClientConfig::current(), mobile)
                .await
                .expect("same Tyde client protocol over TURN");
            let bootstrap = client
                .next_event()
                .await
                .expect("read bootstrap")
                .expect("bootstrap event");
            assert_eq!(bootstrap.kind, protocol::FrameKind::HostBootstrap);
            eprintln!("TURN flow: server bootstrap received");
            let _: settings_model::HostBootstrapPayload =
                bootstrap.parse_payload().expect("typed server replay");
            drop(client);
            server_task.abort();
        }
        relay.close().await;
    }
}

#[tokio::test]
async fn push_signaling_connects_rejects_offline_and_reconnects_host() {
    use protocol::MobilePeerRole;
    use rtc_transport::connect_with_tls_roots;
    use tests::rtc::PushSignaling;
    let relay = relay().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("signaling listener");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("signaling address")
    );
    let (signal, router) = PushSignaling::router(&base);
    let service = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("signaling service");
    });
    let host_credentials = signal.credentials(MobilePeerRole::Host, relay.tls_ice.clone());
    let mobile_credentials = signal.credentials(MobilePeerRole::Mobile, relay.tls_ice.clone());
    let error = connect_with_tls_roots(
        mobile_credentials.clone(),
        &[53; 32],
        |_| {},
        relay.roots.clone(),
    )
    .await
    .err()
    .expect("offline host must fail the mobile attempt");
    assert!(error.to_string().contains("host offline"));
    let old_credentials = host_credentials.clone();
    let roots = relay.roots.clone();
    let mut old_host = tokio::spawn(async move {
        connect_with_tls_roots(old_credentials, &[53; 32], |_| {}, roots).await
    });
    tokio::select! {
        () = signal.host_connected_after(0) => {},
        _ = &mut old_host => panic!("host failed before opening its push socket"),
    }
    // Replace an idle host socket, exactly as a new desktop connection does.
    let previous = signal.generation();
    let new_credentials = host_credentials.clone();
    let roots = relay.roots.clone();
    let host = tokio::spawn(async move {
        connect_with_tls_roots(new_credentials, &[53; 32], |_| {}, roots).await
    });
    signal.host_connected_after(previous).await;
    assert!(old_host.await.expect("replaced host task").is_err());
    signal.disconnect().await;
    assert!(host.await.expect("closed host task").is_err());
    let store = tempfile::tempdir().expect("push host store");
    let server_host = server::spawn_host_with_mock_backend(
        store.path().join("sessions.json"),
        store.path().join("projects.json"),
        store.path().join("settings.json"),
    )
    .expect("push protocol server");
    for _ in 0..2 {
        let previous = signal.generation();
        let credentials = host_credentials.clone();
        let roots = relay.roots.clone();
        let host = tokio::spawn(async move {
            connect_with_tls_roots(credentials, &[53; 32], |_| {}, roots).await
        });
        signal.host_connected_after(previous).await;
        let mut mobile = connect_with_tls_roots(
            mobile_credentials.clone(),
            &[53; 32],
            |_| {},
            relay.roots.clone(),
        )
        .await
        .expect("held POST returns authenticated answer");
        let mut host = host
            .await
            .expect("host task")
            .expect("push host connection");
        let write = async {
            mobile
                .write_all(b"push")
                .await
                .expect("write relayed bytes");
            mobile.flush().await.expect("ack relayed bytes");
        };
        let read = async {
            let mut bytes = [0; 4];
            host.read_exact(&mut bytes)
                .await
                .expect("read relayed bytes");
            assert_eq!(&bytes, b"push");
        };
        tokio::join!(write, read);
        let server_host = server_host.clone();
        let server_task = tokio::spawn(async move {
            let connection = server::accept(&server::ServerConfig::current(), host)
                .await
                .expect("Tyde handshake through push and TURN");
            server::run_connection(connection, server_host).await
        });
        let mut client = client::connect(&client::ClientConfig::current(), mobile)
            .await
            .expect("mobile protocol handshake");
        let event = client
            .next_event()
            .await
            .expect("bootstrap read")
            .expect("bootstrap event");
        assert_eq!(event.kind, protocol::FrameKind::HostBootstrap);
        let _: settings_model::HostBootstrapPayload =
            event.parse_payload().expect("typed server bootstrap");
        drop(client);
        server_task.abort();
    }
    let mut incompatible = host_credentials;
    incompatible.protocol_version -= 1;
    assert!(
        connect_with_tls_roots(incompatible, &[53; 32], |_| {}, relay.roots.clone())
            .await
            .err()
            .expect("old transport version must fail")
            .to_string()
            .contains("unsupported WebRTC protocol version")
    );
    service.abort();
    relay.close().await;
}
