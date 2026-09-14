use rtc_transport::Peer;
use std::time::Duration;
use tests::rtc::{connect, relay};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn real_turn_preserves_bulk_backpressure_and_server_protocol_on_reconnect() {
    timeout(Duration::from_secs(60), async {
        assert!(
            std::path::Path::new(env!("CARGO_BIN_EXE_tyde-rtc-fixture")).is_file(),
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
        let error = timeout(Duration::from_secs(5), untrusted.offer())
            .await
            .expect("certificate rejection must be prompt")
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
        let error = timeout(Duration::from_secs(5), mismatched.offer())
            .await
            .expect("hostname rejection must be prompt")
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
        sleep(Duration::from_millis(200)).await;
        assert!(
            !writer.is_finished(),
            "a stalled reader must exert backpressure"
        );
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
    })
    .await
    .expect("real TURN flow must complete promptly");
}
