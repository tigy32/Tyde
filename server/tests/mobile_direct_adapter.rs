#![cfg(unix)]
mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use mobile_pairing::DirectMobilePairingQrPayload;
use protocol::{FrameKind, MobileDirectHostingStatus, ProjectCreatePayload, ProjectRootPath};
use tyde_dev_driver::mobile_direct::{Adapter, Expected};

#[tokio::test]
async fn typed_adapter_drives_real_server_directory_offer_revoke_disable() {
    server::install_default_crypto_provider();
    let package: serde_json::Value =
        serde_json::from_str(include_str!("../../package.json")).unwrap();
    let release = package["version"].as_str().unwrap();
    server::set_host_release_version(release);
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
        release_version: protocol::TydeReleaseVersion::parse(release).expect("release"),
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standalone_uds_driver_attests_real_peer_and_configures_without_dev_ready() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    use tyde_dev_driver::mobile_direct::{StandaloneTarget, run_uds};
    server::install_default_crypto_provider();
    let package: serde_json::Value =
        serde_json::from_str(include_str!("../../package.json")).unwrap();
    let release = package["version"].as_str().unwrap();
    server::set_host_release_version(release);
    let fixture = Fixture::new().await;
    let private = tempfile::tempdir().unwrap();
    std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = private.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let bundle = private.path().join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(
        bundle.join("index.html"),
        "<!doctype html><title>Actual standalone boundary</title>",
    )
    .unwrap();
    std::fs::write(bundle.join("manifest.json"), "{}").unwrap();
    let socket_path = private.path().join("host.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let executable_sha256 = format!("{:x}", Sha256::digest(std::fs::read(&executable).unwrap()));
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let start_ticks = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let target = StandaloneTarget {
        socket_path,
        executable,
        executable_sha256,
        pid: std::process::id(),
        start_ticks,
        store_environment: std::env::vars()
            .filter(|(name, _)| {
                name == "HOME"
                    || name == "TYDE_SKILLS_DIR_PATH"
                    || (name.starts_with("TYDE_")
                        && (name.ends_with("_STORE_PATH") || name.ends_with("_STORE_DIR")))
            })
            .map(|(name, value)| (name, std::path::PathBuf::from(value)))
            .collect(),
    };
    let expected = Expected {
        protocol_version: protocol::PROTOCOL_VERSION,
        tyde_version: protocol::TYDE_VERSION,
        release_version: protocol::TydeReleaseVersion::parse(release).unwrap(),
        project_dir: project.clone(),
    };
    let inventory = std::fs::read_dir(&bundle)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_str().unwrap().to_owned(),
                format!("{:x}", Sha256::digest(std::fs::read(p).unwrap())),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let run = private.path().join("run.json");
    let request = private.path().join("configure.json");
    fn save(path: &std::path::Path, value: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = serde_json::json!({"target":target,"expected":expected,"bundle_dir":bundle,"inventory":inventory});
    save(&run, &config);
    save(
        &request,
        &serde_json::json!({"run":run,"operation":{"kind":"configure"}}),
    );
    let configured = private.path().join("configured.json");
    let args = vec![
        request.to_string_lossy().into_owned(),
        configured.to_string_lossy().into_owned(),
    ];
    assert!(
        tyde_dev_driver::mobile_direct::run(&args).await.is_err(),
        "standalone input is not a fabricated DEV Ready"
    );
    let mut wrong = config.clone();
    wrong["target"]["executable_sha256"] = serde_json::json!("0".repeat(64));
    save(&run, &wrong);
    assert_eq!(
        run_uds(&args).await.err().as_deref(),
        Some("live host executable hash mismatch")
    );
    // Rejected image/store custody must finish before connecting, not spend
    // the host's Hello window hashing an arbitrarily large executable.
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    let mut wrong_store = config.clone();
    wrong_store["target"]["store_environment"]["HOME"] = serde_json::json!("/not-the-owned-home");
    save(&run, &wrong_store);
    assert_eq!(
        run_uds(&args).await.err().as_deref(),
        Some("owned host store environment mismatch")
    );
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    drop(listener);
    std::fs::remove_file(&target.socket_path).unwrap();
    save(&run, &config);
    let rejected = Fixture::new_with_settings_file(
        &serde_json::json!({"settings": {
            "enable_mobile_connections":true,"mobile_direct_hosting_enabled":true,
            "mobile_direct_bundle_dir":bundle,"mobile_direct_bind_addr":"127.0.0.1:0"
        }})
        .to_string(),
    )
    .await;
    let (reject_stop, reject_task) = rejected.listen_uds(&target.socket_path).await;
    let rejected_receipt = private.path().join("rejected.json");
    assert_eq!(
        run_uds(&[
            request.to_string_lossy().into_owned(),
            rejected_receipt.to_string_lossy().into_owned()
        ])
        .await
        .err()
        .as_deref(),
        Some("configure requires disabled disposable host")
    );
    let (_, refused) = rejected.connect_with_bootstrap().await;
    assert!(
        refused.projects.is_empty() && refused.settings.mobile_direct_hosting_enabled,
        "rejected configure must leave the project list and settings untouched"
    );
    reject_stop.cancel();
    reject_task.await.unwrap().unwrap();
    let (stop, task) = fixture.listen_uds(&target.socket_path).await;
    let (_, before) = fixture.connect_with_bootstrap().await;
    assert!(
        before.projects.is_empty()
            && matches!(
                before.mobile_access.direct_hosting,
                MobileDirectHostingStatus::Disabled
            )
    );
    save(&run, &config);
    run_uds(&args)
        .await
        .expect("real UDS, source identity, project publication and configure");
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&configured).unwrap()).unwrap();
    assert!(
        receipt["standalone_attestation"]["kind"] == "linux_uds"
            && receipt["phase"] == "configured"
            && receipt.get("ready_sha256").is_none()
    );
    let origin = private.path().join("origin.json");
    save(&origin, &serde_json::json!("https://standalone.invalid"));
    let origin_request = private.path().join("set-origin.json");
    save(
        &origin_request,
        &serde_json::json!({"run":run,"operation":{"kind":"set_origin","previous":configured,"origin_path":origin}}),
    );
    let origin_receipt = private.path().join("origin-receipt.json");
    run_uds(&[
        origin_request.to_string_lossy().into_owned(),
        origin_receipt.to_string_lossy().into_owned(),
    ])
    .await
    .expect("same pinned peer and real receipt chain");
    let (_, after) = fixture.connect_with_bootstrap().await;
    assert!(
        after.projects.len() == 1
            && after.projects[0]
                .root_paths()
                .iter()
                .any(|p| std::path::Path::new(&p.0) == project)
    );
    assert!(
        after.settings.mobile_direct_public_origin.as_deref() == Some("https://standalone.invalid")
    );
    let offer_path = private.path().join("offer.json");
    let offer_request = private.path().join("offer-request.json");
    let offered = private.path().join("offered.json");
    save(
        &offer_request,
        &serde_json::json!({"run":run,"operation":{"kind":"offer","previous":origin_receipt,"offer_path":offer_path}}),
    );
    run_uds(&[
        offer_request.to_string_lossy().into_owned(),
        offered.to_string_lossy().into_owned(),
    ])
    .await
    .expect("one real direct offer from an initially empty UDS host");
    let offer: protocol::MobilePairingOfferPayload =
        serde_json::from_slice(&std::fs::read(offer_path).unwrap()).unwrap();
    let (_, active) = fixture.connect_with_bootstrap().await;
    assert!(
        matches!(active.mobile_access.pairing, protocol::MobilePairingState::Active { offer_id, expires_at_ms }
        if offer_id == offer.offer_id && expires_at_ms == offer.expires_at_ms)
    );
    stop.cancel();
    task.await.unwrap().unwrap();
}
