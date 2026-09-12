use protocol::{MobileIceServer, MobileRtcSessionId, MobileSdpKind};
use rtc_transport::{Peer, RtcStream, authenticate_description, verify_description};
use std::time::Duration;
use tests::rtc::relay;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, timeout};

async fn connect(ice: &MobileIceServer) -> (RtcStream, RtcStream) {
    let mut mobile = Peer::new(std::slice::from_ref(ice))
        .await
        .expect("mobile peer");
    let mut host = Peer::new(std::slice::from_ref(ice))
        .await
        .expect("host peer");
    let session = MobileRtcSessionId(uuid::Uuid::new_v4().to_string());
    let key = [53; 32];
    eprintln!("TURN flow: gathering mobile offer");
    let offer = mobile.offer().await.expect("gather mobile TURN candidates");
    eprintln!("TURN flow: mobile offer gathered");
    assert!(offer.lines().any(|line| line.contains(" typ relay")));
    assert!(
        !offer.lines().any(|line| line.contains(" typ host")),
        "test must require the relay"
    );
    let mut signed = authenticate_description(session.clone(), MobileSdpKind::Offer, offer, &key)
        .expect("sign offer");
    assert!(
        verify_description(&signed, &session, MobileSdpKind::Offer, &[54; 32]).is_err(),
        "another pairing cannot authenticate this real peer"
    );
    let original = signed.sdp.clone();
    signed.sdp = signed
        .sdp
        .replace("a=fingerprint:", "a=fingerprint:tampered");
    assert!(
        verify_description(&signed, &session, MobileSdpKind::Offer, &key).is_err(),
        "signaling cannot substitute a DTLS fingerprint"
    );
    signed.sdp = original;
    verify_description(&signed, &session, MobileSdpKind::Offer, &key)
        .expect("authenticate host offer");
    let answer = host
        .answer(signed.sdp)
        .await
        .expect("gather host TURN candidates");
    let signed = authenticate_description(session.clone(), MobileSdpKind::Answer, answer, &key)
        .expect("sign answer");
    verify_description(&signed, &session, MobileSdpKind::Answer, &key)
        .expect("authenticate mobile answer");
    mobile.set_answer(signed.sdp).await.expect("apply answer");
    let (mobile, host) =
        tokio::try_join!(mobile.into_stream(), host.into_stream()).expect("open relayed channels");
    eprintln!("TURN flow: channels open");
    (mobile, host)
}

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
        let (relay, ice) = relay().await;
        let (mut mobile, mut host) = connect(&ice).await;
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
            let (mobile, host) = connect(&ice).await;
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
        relay.close().await.expect("close relay");
    })
    .await
    .expect("real TURN flow must complete promptly");
}
