#![cfg(unix)]
mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use mobile_pairing::DirectMobilePairingQrPayload;
use protocol::{FrameKind, MobileDirectHostingStatus, ProjectCreatePayload, ProjectRootPath};
use tyde_dev_driver::mobile_direct::{Adapter, Expected};

#[tokio::test]
async fn typed_adapter_drives_real_server_directory_offer_revoke_disable() {
    server::install_default_crypto_provider();
    server::set_host_release_version("0.9.6-beta.15");
    let mut fixture = Fixture::new().await;
    let project = tempfile::tempdir().expect("project");
    let project_dir = project.path().canonicalize().expect("canonical project");
    fixture
        .client
        .project_create(ProjectCreatePayload {
            name: "Disposable adapter flow".into(),
            roots: vec![ProjectRootPath(project_dir.to_string_lossy().into_owned())],
        })
        .await
        .expect("create project over protocol");
    next_frame_matching_on(&mut fixture.client, "project publication", |e| {
        e.kind == FrameKind::ProjectNotify
    })
    .await;
    let (endpoint, stop, task) = fixture.listen_tcp().await;
    let expected = Expected {
        protocol_version: protocol::PROTOCOL_VERSION,
        tyde_version: protocol::TYDE_VERSION,
        release_version: protocol::TydeReleaseVersion::parse("0.9.6-beta.15").expect("release"),
        project_dir,
    };
    let mut wrong = expected.clone();
    wrong.release_version = protocol::TydeReleaseVersion::parse("0.9.6-beta.14").expect("release");
    assert!(
        Adapter::connect(endpoint, &wrong).await.is_err(),
        "must observe exact Welcome release, not just client::connect"
    );
    let mut adapter = Adapter::connect(endpoint, &expected)
        .await
        .expect("exact Welcome and HostBootstrap");
    let bundle = tempfile::tempdir().expect("bundle");
    std::fs::write(
        bundle.path().join("index.html"),
        "<!doctype html><title>Adapter boundary</title>",
    )
    .expect("shell");
    std::fs::write(bundle.path().join("manifest.json"), "{}").expect("manifest");
    let bundle_dir = bundle.path().canonicalize().expect("canonical bundle");
    adapter
        .configure(&bundle_dir, 2)
        .await
        .expect("atomic directory CAS and Online");
    let bind = match &adapter.mobile_access.direct_hosting {
        MobileDirectHostingStatus::Online { bind_addr, .. } => bind_addr.clone(),
        _ => panic!("directory must be Online"),
    };
    let http = reqwest::Client::new();
    let shell = http
        .get(format!("http://{bind}/tyde/"))
        .send()
        .await
        .expect("real HTTP");
    assert_eq!(shell.status(), 200);
    assert_eq!(
        shell.text().await.expect("body"),
        "<!doctype html><title>Adapter boundary</title>"
    );
    // The real actor emits Failed without any offer or CommandError when
    // no public origin has been configured. This must return, not await a QR.
    assert_eq!(
        adapter.offer().await.err().as_deref(),
        Some("pairing failed before offer handoff")
    );
    adapter
        .set_origin("https://adapter.invalid", 2)
        .await
        .expect("origin CAS");
    // Reconnect like the executable's next private-file operation.
    drop(adapter);
    let mut adapter = Adapter::connect(endpoint, &expected)
        .await
        .expect("reconnect after origin");
    let offer = adapter
        .offer()
        .await
        .expect("one requester-scoped production offer");
    assert!(
        adapter.disable().await.is_err(),
        "must not toggle hosting to mask an active offer"
    );
    let fragment = offer
        .qr_uri
        .0
        .split_once('#')
        .expect("production fragment")
        .1;
    let qr = DirectMobilePairingQrPayload::from_uri(fragment).expect("production parser");
    // Only this sim test acts as a phone; the adapter has no redemption operation.
    let response = http
        .post(format!("http://{bind}/tyde/pair"))
        .json(&protocol::MobileDirectPairRequest {
            offer_id: qr.offer_id,
            offer_secret: qr.offer_secret,
            device_label: "Disposable sim device".into(),
        })
        .send()
        .await
        .expect("real HTTP pairing boundary");
    assert_eq!(response.status(), 200);
    drop(adapter);
    let mut adapter = Adapter::connect(endpoint, &expected)
        .await
        .expect("reconnect with consumed offer");
    adapter
        .revoke(&offer)
        .await
        .expect("revoke only this run disposable device");
    assert!(adapter.mobile_access.paired_devices.is_empty());
    adapter
        .disable()
        .await
        .expect("CAS disable and authoritative Disabled");
    assert!(matches!(
        adapter.mobile_access.direct_hosting,
        MobileDirectHostingStatus::Disabled
    ));
    drop(adapter);
    stop.send(()).expect("stop fixture listener");
    task.await.expect("join fixture connections");
}
