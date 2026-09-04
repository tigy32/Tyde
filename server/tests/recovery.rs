mod fixture;

use fixture::{Fixture, next_frame_matching_on};
use protocol::{FrameKind, ProjectCreatePayload, ProjectNotifyPayload, ProjectRootPath};
use server::recovery::{Registry, Session};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

struct Wire {
    relay: tokio::task::JoinHandle<()>,
    downlink: tokio::sync::watch::Sender<bool>,
    attachment: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Wire {
    async fn drop_transport(self) {
        self.relay.abort();
        let _ = self.relay.await;
        timeout(Duration::from_secs(2), self.attachment)
            .await
            .expect("detach without destroying logical session")
            .expect("attachment task")
            .expect_err("wire must fail");
    }
}

async fn attach(
    fixture: &Fixture,
    registry: &Registry,
    session: &Arc<Session>,
    id: Option<uuid::Uuid>,
) -> (uuid::Uuid, Wire) {
    let (wire, relay, downlink) = fixture.recovery_wire_controlled(registry.clone());
    let (mut reader, mut writer) = tokio::io::split(wire);
    let (id, received, _) = server::recovery::connect(&mut reader, &mut writer, id, session)
        .await
        .expect("attach recovery wire");
    let session = session.clone();
    let attachment = tokio::spawn(session.attach(reader, writer, received));
    (
        id,
        Wire {
            relay,
            attachment,
            downlink,
        },
    )
}

#[tokio::test]
async fn connection_resumes_projects_and_commands_without_new_bootstrap() {
    let fixture = Fixture::new().await;
    let registry = Registry::default();
    let (session, logical) = Session::new();
    let (id, wire) = attach(&fixture, &registry, &session, None).await;
    let mut client = client::connect(&client::ClientConfig::current(), logical)
        .await
        .expect("real client handshake");
    let bootstrap = next_frame_matching_on(&mut client, "initial bootstrap", |e| {
        e.kind == FrameKind::HostBootstrap
    })
    .await;
    let host_stream = bootstrap.stream;
    let (mut observer, _) = fixture.connect_with_bootstrap().await;
    wire.downlink.send(false).unwrap();

    // A command already accepted by the local transport must survive a lost
    // socket. The remote host must execute it once, even across another resume.
    let root = tempfile::tempdir().unwrap();
    client
        .project_create(ProjectCreatePayload {
            name: "Created across disconnect".into(),
            roots: vec![ProjectRootPath(root.path().to_string_lossy().into_owned())],
        })
        .await
        .expect("queue in-flight command");
    next_frame_matching_on(&mut observer, "server executed command before ACK loss", |e| {
        e.kind == FrameKind::ProjectNotify && matches!(e.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Upsert { project }) if project.name == "Created across disconnect")
    }).await;
    wire.drop_transport().await;
    let (resumed_id, wire) = attach(&fixture, &registry, &session, Some(id)).await;
    assert_eq!(resumed_id, id);
    let event = next_frame_matching_on(&mut client, "replayed command result", |e| {
        assert_ne!(
            e.kind,
            FrameKind::Welcome,
            "resume must not handshake again"
        );
        assert_ne!(
            e.kind,
            FrameKind::HostBootstrap,
            "resume must not reinitialize projects"
        );
        e.kind == FrameKind::ProjectNotify
    })
    .await;
    assert_eq!(
        event.stream, host_stream,
        "preserve the logical stream identity"
    );
    assert!(event.seq > 1, "continue the same sequence");
    let ProjectNotifyPayload::Upsert { project } = event.parse_payload().unwrap() else {
        panic!("created project")
    };
    assert_eq!(project.name, "Created across disconnect");
    wire.drop_transport().await;

    // Another real client changes the host while this client is detached.
    let mut other = fixture.connect().await;
    other
        .project_rename(protocol::ProjectRenamePayload {
            id: project.id.clone(),
            name: "Changed while offline".into(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut other, "rename on observer", |e| {
        e.kind == FrameKind::ProjectNotify && matches!(e.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Upsert { project }) if project.name == "Changed while offline")
    }).await;
    let (_, wire) = attach(&fixture, &registry, &session, Some(id)).await;
    next_frame_matching_on(&mut client, "offline notification replay", |e| {
        assert_ne!(e.kind, FrameKind::HostBootstrap);
        e.kind == FrameKind::ProjectNotify && matches!(e.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Upsert { project }) if project.name == "Changed while offline")
    }).await;
    // Replace a still-open socket, as happens when only one side has detected
    // an outage. Cleanup of the old attachment must leave its replacement live.
    let old_wire = wire;
    let (_, wire) = attach(&fixture, &registry, &session, Some(id)).await;
    let old_result = timeout(Duration::from_secs(2), old_wire.attachment)
        .await
        .expect("superseded attachment stops")
        .expect("old attachment task");
    assert!(old_result.is_ok() || !session.is_closed());
    old_wire.relay.abort();
    client
        .project_rename(protocol::ProjectRenamePayload {
            id: project.id.clone(),
            name: "Live after socket replacement".into(),
        })
        .await
        .unwrap();
    next_frame_matching_on(&mut client, "command after socket replacement", |e| {
        assert_ne!(e.kind, FrameKind::HostBootstrap);
        e.kind == FrameKind::ProjectNotify && matches!(e.parse_payload::<ProjectNotifyPayload>(), Ok(ProjectNotifyPayload::Upsert { project }) if project.name == "Live after socket replacement")
    }).await;
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    assert_eq!(
        bootstrap
            .projects
            .iter()
            .filter(|p| p.id == project.id)
            .count(),
        1
    );
    assert_eq!(
        bootstrap
            .projects
            .iter()
            .filter(|p| p.name == "Live after socket replacement")
            .count(),
        1
    );
    session.close();
    wire.relay.abort();
}

#[tokio::test]
async fn expired_and_overflowed_sessions_require_a_fresh_bootstrap() {
    let fixture = Fixture::new().await;
    for (limit, window, overflow) in [
        (128 * 1024 * 1024, Duration::from_millis(500), false),
        (1, Duration::from_secs(300), true),
    ] {
        let registry = Registry::with_limits(limit, window);
        let (session, logical) = Session::new();
        let (id, wire) = attach(&fixture, &registry, &session, None).await;
        if overflow {
            // Even the real Welcome exceeds this deliberately tiny test budget.
            let handshake = tokio::spawn(async move {
                client::connect(&client::ClientConfig::current(), logical).await
            });
            timeout(Duration::from_secs(2), wire.attachment)
                .await
                .expect("overflow closes attachment")
                .unwrap()
                .expect_err("overflow is not resumable");
            wire.relay.abort();
            handshake.abort();
        } else {
            let mut client = client::connect(&client::ClientConfig::current(), logical)
                .await
                .unwrap();
            next_frame_matching_on(&mut client, "bootstrap before expiry", |e| {
                e.kind == FrameKind::HostBootstrap
            })
            .await;
            wire.drop_transport().await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        let (new_wire, relay) = fixture.recovery_wire(registry);
        let (mut reader, mut writer) = tokio::io::split(new_wire);
        let error = server::recovery::connect(&mut reader, &mut writer, Some(id), &session)
            .await
            .expect_err("lost replay cannot resume");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        relay.abort();
        session.close();
    }
    let (_, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.projects.is_empty(),
        "fresh bootstrap remains available after replay loss"
    );
}
