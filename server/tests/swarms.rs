mod fixture;

use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentControlStatus, Envelope, FrameKind, LaunchProfileEntry, LaunchProfileKind, Project,
    ProjectCreatePayload, ProjectNotifyPayload, ProjectRootPath, Swarm, SwarmAuthor,
    SwarmBackendAllocation, SwarmBoard, SwarmBoardNotifyPayload, SwarmBoardPage, SwarmBoardRead,
    SwarmBodySegment, SwarmCommandPayload, SwarmDeliveryState, SwarmDescribe, SwarmDraft,
    SwarmDraftGeneration, SwarmDraftId, SwarmDraftNotifyPayload, SwarmErrorCode,
    SwarmErrorNotifyPayload, SwarmId, SwarmLifecycle, SwarmMemberState, SwarmNotifyPayload,
    SwarmPost, SwarmPostNotifyPayload, SwarmPublication, SwarmPublicationId,
    SwarmPublicationOutcome, SwarmRetirementPolicy, SwarmThreadNotifyPayload, SwarmThreadPage,
    SwarmThreadRead, SwarmWorkspacePolicy,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use server::backend::mock::{
    MockControl, MockGateHandle, MockRequest, MockResumeReplay, MockScript, MockTurn,
};

struct Scenario {
    fixture: Fixture,
    project: Project,
    pending: Vec<Envelope>,
}

fn frame_error_context(error: &protocol::FrameError) -> String {
    match error {
        protocol::FrameError::Io(error) => format!(
            "FrameError::Io kind={:?} os_code={:?}",
            error.kind(),
            error.raw_os_error()
        ),
        protocol::FrameError::Json(error) => format!(
            "FrameError::Json category={:?} line={} column={}",
            error.classify(),
            error.line(),
            error.column()
        ),
        protocol::FrameError::Protocol(message) => {
            let reason = match message.as_str() {
                "invalid TYD2 record magic"
                | "record reserved flags are nonzero"
                | "record length exceeds bound"
                | "record checksum mismatch"
                | "fragment reassembly timed out"
                | "EOF during fragmented envelope"
                | "JSON record has a binary body"
                | "invalid fragment metadata"
                | "invalid fragmented envelope length"
                | "fragment reassembly bound/order violation"
                | "fragment sequence mismatch"
                | "fragment data exceeds declared length"
                | "fragmented envelope length mismatch"
                | "binary frame requires next_frame" => message.as_str(),
                value if value.starts_with("sequence mismatch for stream ") => {
                    "sequence mismatch (stream redacted)"
                }
                value if value.starts_with("unsupported record version ") => {
                    "unsupported record version"
                }
                value if value.starts_with("unknown record kind ") => "unknown record kind",
                _ => "other protocol invariant (details redacted)",
            };
            format!("FrameError::Protocol {reason}")
        }
    }
}

impl Scenario {
    async fn new() -> Self {
        let mut fixture = Fixture::new_with_mock_backend_for_enabled_backends(vec![
            protocol::BackendKind::Claude,
        ])
        .await;
        let root = fixture.store_dir().join("workspace");
        std::fs::create_dir(&root).expect("create swarm project root");
        std::fs::write(root.join("reference.txt"), "Fixture project reference\n")
            .expect("create project reference");
        fixture
            .client
            .project_create(ProjectCreatePayload {
                name: "Swarm protocol workspace".to_owned(),
                roots: vec![ProjectRootPath(root.to_string_lossy().into_owned())],
            })
            .await
            .expect("create swarm project over protocol");
        let project = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let env = match fixture.client.next_event().await {
                    Ok(Some(env)) => env,
                    Ok(None) => panic!("project creation connection closed"),
                    Err(error) => panic!(
                        "project creation protocol read failed: {}",
                        frame_error_context(&error)
                    ),
                };
                if env.kind == FrameKind::ProjectNotify {
                    let ProjectNotifyPayload::Upsert { project } =
                        env.parse_payload().expect("parse project creation")
                    else {
                        panic!("project creation emitted a deletion");
                    };
                    return project;
                }
            }
        })
        .await
        .expect("server must publish the new project");
        Self {
            fixture,
            project,
            pending: Vec::new(),
        }
    }

    async fn add_project(&mut self, name: &str) -> Project {
        let root = self.fixture.store_dir().join(name);
        std::fs::create_dir(&root).expect("create additional project root");
        std::fs::write(root.join("reference.txt"), "Project reference\n")
            .expect("write additional reference");
        self.fixture
            .client
            .project_create(ProjectCreatePayload {
                name: name.to_owned(),
                roots: vec![ProjectRootPath(root.to_string_lossy().into_owned())],
            })
            .await
            .expect("create additional project over protocol");
        let event: ProjectNotifyPayload = self.wait(FrameKind::ProjectNotify, "additional project", |event| {
            matches!(event, ProjectNotifyPayload::Upsert { project } if project.name == name)
        }).await;
        match event {
            ProjectNotifyPayload::Upsert { project } => project,
            ProjectNotifyPayload::Delete { .. } => panic!("expected additional project"),
        }
    }

    async fn add_workbench(&mut self, parent: &Project, branch: &str) -> Project {
        self.fixture
            .client
            .workbench_create(protocol::WorkbenchCreatePayload {
                parent_project_id: parent.id.clone(),
                branch: protocol::GitBranchName(branch.to_owned()),
                name: branch.to_owned(),
            })
            .await
            .expect("create workbench over protocol");
        let event: ProjectNotifyPayload = self.wait(FrameKind::ProjectNotify, "new workbench", |event| {
            matches!(event, ProjectNotifyPayload::Upsert { project } if project.name == branch)
        }).await;
        match event {
            ProjectNotifyPayload::Upsert { project } => project,
            ProjectNotifyPayload::Delete { .. } => panic!("expected new workbench"),
        }
    }

    fn constraints(&self, count: u32) -> protocol::SwarmConstraints {
        let profile = self
            .fixture
            .bootstrap
            .launch_profile_catalog
            .entries
            .iter()
            .find_map(|entry| match entry {
                LaunchProfileEntry::Ready { profile }
                    if profile.backend_kind == protocol::BackendKind::Claude
                        && profile.kind == LaunchProfileKind::BackendDefault =>
                {
                    Some(profile)
                }
                _ => None,
            })
            .expect("mock fixture must expose a ready Claude launch profile");
        protocol::SwarmConstraints {
            project_id: self.project.id.clone(),
            workspace_policy: SwarmWorkspacePolicy::ReadOnly,
            max_live_agents: count,
            allocations: vec![SwarmBackendAllocation {
                backend_kind: profile.backend_kind,
                launch_profile_id: profile.id.clone(),
                session_settings: profile.session_settings.clone(),
                count,
            }],
            shared_guidance: "Coordinate using shared board posts".to_owned(),
            agent_wake_budget: 16,
        }
    }

    async fn send(&mut self, command: SwarmCommandPayload) {
        let stream = self
            .fixture
            .client
            .outgoing_seq
            .keys()
            .find(|stream| stream.0.starts_with("/host/"))
            .cloned()
            .expect("swarm commands require the authenticated host stream");
        let seq = *self
            .fixture
            .client
            .outgoing_seq
            .get(&stream)
            .expect("host stream has an outgoing sequence");
        let envelope =
            Envelope::from_payload(stream.clone(), FrameKind::SwarmCommand, seq, &command)
                .expect("serialize typed swarm command");
        self.fixture.client.outgoing_seq.insert(stream, seq + 1);
        protocol::write_envelope(&mut self.fixture.client.writer, &envelope)
            .await
            .expect("write swarm command");
    }

    async fn wait<T: DeserializeOwned>(
        &mut self,
        kind: FrameKind,
        context: &str,
        predicate: impl Fn(&T) -> bool,
    ) -> T {
        if let Some(index) = self.pending.iter().position(|env| {
            env.kind == kind
                && predicate(
                    &env.parse_payload::<T>()
                        .expect("parse buffered protocol event"),
                )
        }) {
            return self
                .pending
                .remove(index)
                .parse_payload()
                .expect("parse buffered event");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut observed = Vec::new();
        loop {
            let env =
                match tokio::time::timeout_at(deadline, self.fixture.client.next_event()).await {
                    Ok(Ok(Some(env))) => env,
                    Ok(Ok(None)) => panic!("connection closed during {context}"),
                    Ok(Err(error)) => panic!(
                        "protocol read failed during {context}: {}",
                        frame_error_context(&error)
                    ),
                    Err(_) => panic!("timed out during {context}; observed kinds: {observed:?}"),
                };
            observed.push(env.kind);
            if env.kind == kind {
                let payload = env
                    .parse_payload::<T>()
                    .expect("parse typed protocol event");
                if predicate(&payload) {
                    return payload;
                }
            }
            if env.kind == FrameKind::SwarmErrorNotify && kind != FrameKind::SwarmErrorNotify {
                let error: SwarmErrorNotifyPayload =
                    env.parse_payload().expect("parse swarm error");
                if error.code != SwarmErrorCode::CommittedDurabilityUncertain {
                    panic!("unexpected swarm error during {context}: {:?}", error.code);
                }
            }
            if env.kind == FrameKind::CommandError && kind != FrameKind::CommandError {
                let error: protocol::CommandErrorPayload =
                    env.parse_payload().expect("parse command rejection");
                panic!(
                    "unexpected command rejection during {context}: {:?} for {:?}",
                    error.code, error.request_kind
                );
            }
            if matches!(
                env.kind,
                FrameKind::SwarmNotify
                    | FrameKind::SwarmDraftNotify
                    | FrameKind::SwarmPostNotify
                    | FrameKind::SwarmBoardNotify
                    | FrameKind::SwarmThreadNotify
                    | FrameKind::SwarmImageNotify
                    | FrameKind::SwarmErrorNotify
                    | FrameKind::ProjectNotify
                    | FrameKind::TeamNotify
                    | FrameKind::TeamMemberNotify
                    | FrameKind::TeamMemberBindingNotify
                    | FrameKind::AgentClosed
                    | FrameKind::NewAgent
            ) {
                self.pending.push(env);
            }
        }
    }

    async fn draft(&mut self, id: &SwarmDraftId, minimum_revision: u64) -> SwarmDraft {
        let event: SwarmDraftNotifyPayload = self
            .wait(FrameKind::SwarmDraftNotify, "draft update", |event| {
                matches!(event, SwarmDraftNotifyPayload::Upsert { draft }
                    if draft.id == *id && draft.revision >= minimum_revision)
            })
            .await;
        match event {
            SwarmDraftNotifyPayload::Upsert { draft } => *draft,
            SwarmDraftNotifyPayload::Delete { .. } => panic!("expected a draft upsert"),
        }
    }

    async fn generate(&mut self, constraints: protocol::SwarmConstraints) -> SwarmDraft {
        let id = SwarmDraftId(uuid::Uuid::new_v4().to_string());
        self.send(SwarmCommandPayload::GenerateDraft {
            draft_id: id.clone(),
            expected_revision: None,
            name: "Protocol swarm".to_owned(),
            opening_brief: "Review the project and publish useful findings".to_owned(),
            constraints,
        })
        .await;
        self.draft(&id, 0).await
    }

    async fn swarm(&mut self, id: &SwarmId, predicate: impl Fn(&Swarm) -> bool) -> Swarm {
        let mut last_state = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event: SwarmNotifyPayload = self
                    .wait(
                        FrameKind::SwarmNotify,
                        "swarm state transition",
                        |event: &SwarmNotifyPayload| event.swarm.id == *id,
                    )
                    .await;
                last_state = Some((
                    event.swarm.lifecycle,
                    event.swarm.members.iter().map(|member|
                        (member.state, member.runtime_status, member.error.is_some())).collect::<Vec<_>>(),
                    event.swarm.notifications.iter().filter(|intent| intent.state == SwarmDeliveryState::Pending).count(),
                    event.swarm.notifications.iter().filter(|intent| intent.state == SwarmDeliveryState::Dispatching).count(),
                    event.swarm.notifications.iter().filter(|intent| intent.state == SwarmDeliveryState::Accepted).count(),
                ));
                if predicate(&event.swarm) {
                    return event.swarm;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("server must emit the requested swarm transition; last lifecycle/member status/error presence/pending/dispatching/accepted counts: {last_state:?}"))
    }

    async fn wait_mock_turn(&mut self, gate: &MockGateHandle, phase: &str) {
        eprintln!("Swarm sim mock-boundary phase begin: {phase}");
        // Gate entry does not depend on draining the UI connection. Selecting
        // against next_event canceled read_record's local partial wire buffers.
        tokio::time::timeout(Duration::from_secs(5), gate.wait_until_entered())
            .await
            .unwrap_or_else(|_| panic!("mock boundary did not enter phase {phase}"));
        eprintln!("Swarm sim mock-boundary phase entered: {phase}");
    }

    async fn observe_for(&mut self, duration: Duration, phase: &str) {
        let deadline = tokio::time::Instant::now() + duration;
        let mut frames = 0usize;
        eprintln!("Swarm sim observation begin: {phase}");
        loop {
            let envelope =
                match tokio::time::timeout_at(deadline, self.fixture.client.next_event()).await {
                    Ok(Ok(Some(envelope))) => envelope,
                    Ok(Ok(None)) => panic!("connection closed observing {phase}"),
                    Ok(Err(error)) => panic!(
                        "protocol read failed observing {phase}: {}",
                        frame_error_context(&error)
                    ),
                    Err(_) => break,
                };
            if envelope.kind == FrameKind::CommandError {
                let error: protocol::CommandErrorPayload = envelope
                    .parse_payload()
                    .expect("parse observation command error");
                panic!(
                    "command rejection observing {phase}: {:?} for {:?}",
                    error.code, error.request_kind
                );
            }
            if envelope.kind == FrameKind::SwarmErrorNotify {
                let error: SwarmErrorNotifyPayload = envelope
                    .parse_payload()
                    .expect("parse observation swarm error");
                assert_eq!(
                    error.code,
                    SwarmErrorCode::CommittedDurabilityUncertain,
                    "unexpected swarm error during bounded observation"
                );
            }
            frames += 1;
            self.pending.push(envelope);
        }
        eprintln!("Swarm sim observation complete: {phase}; frames={frames}");
    }

    async fn set_supervisor_setting<V: serde::Serialize, E: serde::Serialize>(
        &mut self,
        path: &str,
        value: V,
        expected: E,
    ) {
        let write_id = self
            .fixture
            .client
            .replace_setting(path, value, expected)
            .await
            .expect("send scoped supervisor setting through protocol");
        let result: protocol::SettingsWriteResultPayload = self
            .wait(
                FrameKind::SettingsWriteResult,
                "supervisor setting commit",
                |result: &protocol::SettingsWriteResultPayload| result.write_id == write_id,
            )
            .await;
        assert!(
            result.applied && result.field_errors.is_empty(),
            "the supervision regression requires actual enabled host settings"
        );
    }

    async fn launch(&mut self, draft: &SwarmDraft) -> Swarm {
        self.send(SwarmCommandPayload::Launch {
            draft_id: draft.id.clone(),
            expected_revision: draft.revision,
        })
        .await;
        let event: SwarmNotifyPayload = self
            .wait(
                FrameKind::SwarmNotify,
                "approved draft launch",
                |event: &SwarmNotifyPayload| {
                    event.swarm.source_draft_id.as_ref() == Some(&draft.id)
                },
            )
            .await;
        assert!(
            event
                .swarm
                .members
                .iter()
                .map(|member| &member.spec)
                .eq(draft.members.iter()),
            "launch must activate exactly the reviewed lineup, without hidden substitutions"
        );
        event.swarm
    }

    async fn launched(&mut self, count: u32) -> Swarm {
        let draft = self.generate(self.constraints(count)).await;
        let swarm = self.launch(&draft).await;
        self.swarm(&swarm.id, ready).await
    }

    async fn pause(&mut self, id: &SwarmId) -> Swarm {
        self.send(SwarmCommandPayload::Pause {
            swarm_id: id.clone(),
        })
        .await;
        self.swarm(id, |swarm| swarm.lifecycle == SwarmLifecycle::Paused)
            .await
    }

    async fn post(&mut self, id: &SwarmId, publication: SwarmPublication) -> SwarmPost {
        let key = publication.publication_id.clone();
        self.send(SwarmCommandPayload::Post {
            swarm_id: id.clone(),
            publication,
        })
        .await;
        let event: SwarmPostNotifyPayload = self
            .wait(
                FrameKind::SwarmPostNotify,
                "durable publication",
                |event: &SwarmPostNotifyPayload| {
                    // MCP publications share this event stream and may reuse
                    // the same key in their distinct authenticated author scope.
                    event.post.swarm_id == *id
                        && event.post.publication_id == key
                        && event.post.author == SwarmAuthor::Human
                },
            )
            .await;
        event.post
    }

    async fn board(&mut self, id: &SwarmId, query: SwarmBoardRead) -> SwarmBoardPage {
        let board = query.board;
        self.send(SwarmCommandPayload::ReadBoard {
            swarm_id: id.clone(),
            query,
        })
        .await;
        let event: SwarmBoardNotifyPayload = self
            .wait(
                FrameKind::SwarmBoardNotify,
                "board read",
                |event: &SwarmBoardNotifyPayload| {
                    event.page.swarm_id == *id && event.page.board == board
                },
            )
            .await;
        event.page
    }

    async fn thread(&mut self, id: &SwarmId, query: SwarmThreadRead) -> SwarmThreadPage {
        let thread_id = query.thread_id.clone();
        self.send(SwarmCommandPayload::ReadThread {
            swarm_id: id.clone(),
            query,
        })
        .await;
        let event: SwarmThreadNotifyPayload = self
            .wait(
                FrameKind::SwarmThreadNotify,
                "thread read",
                |event: &SwarmThreadNotifyPayload| {
                    event.page.swarm_id == *id && event.page.thread_id == thread_id
                },
            )
            .await;
        event.page
    }

    async fn error(&mut self, expected: SwarmErrorCode) -> SwarmErrorNotifyPayload {
        let event: SwarmErrorNotifyPayload = self
            .wait(
                FrameKind::SwarmErrorNotify,
                "rejected swarm command",
                |event: &SwarmErrorNotifyPayload| {
                    expected == SwarmErrorCode::CommittedDurabilityUncertain
                        || event.code != SwarmErrorCode::CommittedDurabilityUncertain
                },
            )
            .await;
        assert_eq!(
            event.code, expected,
            "command must fail with the typed error code"
        );
        assert!(
            !event.message.is_empty(),
            "rejection must explain the failure"
        );
        event
    }

    async fn command_conflict(&mut self, request_kind: FrameKind) {
        let error: protocol::CommandErrorPayload = self
            .wait(
                FrameKind::CommandError,
                "rejected legacy mutation",
                |error: &protocol::CommandErrorPayload| error.request_kind == request_kind,
            )
            .await;
        assert_eq!(
            error.code,
            protocol::CommandErrorCode::Conflict,
            "converted legacy mutation must reach the ownership conflict guard; request_kind={request_kind:?} actual_code={:?}",
            error.code
        );
        assert!(
            !error.fatal && !error.message.is_empty(),
            "converted legacy mutation must explain its nonfatal rejection"
        );
    }

    async fn snapshot(&self, id: &SwarmId) -> Swarm {
        let (_, bootstrap) = self.fixture.connect_with_bootstrap().await;
        bootstrap
            .swarms
            .into_iter()
            .find(|swarm| swarm.id == *id)
            .expect("new subscriber must receive persisted swarm state")
    }

    async fn snapshot_after_commands(&mut self, id: &SwarmId) -> Swarm {
        // The router finishes a host command before handling the next one on
        // this connection. Healthy RetryMember is a no-op, not a new revision.
        self.fixture
            .client
            .list_sessions(protocol::ListSessionsPayload {
                scope: Some(protocol::SessionListScope::RootSessions),
                cursor: None,
                limit: Some(1),
            })
            .await
            .expect("send same-connection host command barrier");
        let _: protocol::SessionListPayload = self
            .wait(
                FrameKind::SessionList,
                "host command completion barrier",
                |page: &protocol::SessionListPayload| {
                    page.page.scope == protocol::SessionListScope::RootSessions
                        && page.page.limit == Some(1)
                },
            )
            .await;
        self.snapshot(id).await
    }

    async fn settings_for_agent(&self, id: &protocol::AgentId) -> protocol::SessionSettingsPayload {
        let (mut client, bootstrap) = self.fixture.connect_with_bootstrap().await;
        let stream = bootstrap
            .agents
            .iter()
            .find(|agent| agent.agent_id == *id)
            .expect("effective selection requires a real active agent")
            .instance_stream
            .clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let envelope = match client.next_event().await {
                    Ok(Some(envelope)) => envelope,
                    Ok(None) => panic!("effective settings connection closed"),
                    Err(error) => panic!(
                        "effective settings protocol read failed: {}",
                        frame_error_context(&error)
                    ),
                };
                if envelope.kind == FrameKind::AgentBootstrap && envelope.stream == stream {
                    let agent: protocol::AgentBootstrapPayload = envelope
                        .parse_payload()
                        .expect("parse actual activation bootstrap");
                    return agent
                        .events
                        .into_iter()
                        .find_map(|event| match event {
                            protocol::AgentBootstrapEvent::SessionSettings(settings) => {
                                Some(settings)
                            }
                            _ => None,
                        })
                        .expect(
                            "active member bootstrap must include its effective session selection",
                        );
                }
            }
        })
        .await
        .expect("server must publish actual activation session settings")
    }

    async fn start_for_agent(&self, id: &protocol::AgentId) -> protocol::AgentStartPayload {
        let (mut client, bootstrap) = self.fixture.connect_with_bootstrap().await;
        let stream = bootstrap
            .agents
            .iter()
            .find(|agent| agent.agent_id == *id)
            .expect("owned start requires an actual runtime")
            .instance_stream
            .clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let envelope = match client.next_event().await {
                    Ok(Some(envelope)) => envelope,
                    Ok(None) => panic!("owned-start connection closed"),
                    Err(error) => panic!(
                        "owned-start protocol read failed: {}",
                        frame_error_context(&error)
                    ),
                };
                if envelope.kind == FrameKind::AgentBootstrap && envelope.stream == stream {
                    let agent: protocol::AgentBootstrapPayload = envelope
                        .parse_payload()
                        .expect("parse owned activation bootstrap");
                    return agent
                        .events
                        .into_iter()
                        .find_map(|event| match event {
                            protocol::AgentBootstrapEvent::AgentStart(start) => Some(start),
                            _ => None,
                        })
                        .expect("owned activation must expose typed start membership");
                }
            }
        })
        .await
        .expect("server must publish actual owned start")
    }

    async fn refresh_migration(&mut self, previous: &SwarmDraft) -> SwarmDraft {
        self.send(SwarmCommandPayload::PreviewMigration {
            team_id: previous
                .legacy_team_id
                .clone()
                .expect("explicit legacy draft source"),
        })
        .await;
        let refreshed = self.draft(&previous.id, previous.revision + 1).await;
        self.send(SwarmCommandPayload::GenerateDraft {
            draft_id: refreshed.id.clone(),
            expected_revision: Some(refreshed.revision),
            name: previous.name.clone(),
            opening_brief: previous.opening_brief.clone(),
            constraints: refreshed.constraints.clone(),
        })
        .await;
        self.draft(&refreshed.id, refreshed.revision + 1).await
    }
}

fn ready(swarm: &Swarm) -> bool {
    swarm.lifecycle == SwarmLifecycle::Running
        && swarm.members.iter().all(|member| {
            member.state == SwarmMemberState::Live
                && member.runtime_status == Some(AgentControlStatus::Idle)
                && member.agent_id.is_some()
                && member.session_id.is_some()
        })
}

fn publication(board: SwarmBoard, key: &str, body: Vec<SwarmBodySegment>) -> SwarmPublication {
    SwarmPublication {
        images: Vec::new(),
        board,
        publication_id: SwarmPublicationId(key.to_owned()),
        body,
        thread_id: None,
        attachments: Vec::new(),
    }
}

fn text(value: &str) -> SwarmBodySegment {
    SwarmBodySegment::Text {
        text: value.to_owned(),
    }
}

fn read_board(board: SwarmBoard) -> SwarmBoardRead {
    SwarmBoardRead {
        board,
        after_cursor: None,
        limit: None,
    }
}

fn request_count(requests: &[MockRequest]) -> usize {
    requests
        .iter()
        .filter(|request| {
            matches!(
                request,
                MockRequest::Launch { .. } | MockRequest::Input(_) | MockRequest::Steer(_)
            )
        })
        .count()
}

fn last_dispatch_context(requests: &[MockRequest]) -> (Vec<protocol::SwarmPostId>, Vec<SwarmPost>) {
    let message = requests
        .iter()
        .rev()
        .find_map(|request| match request {
            MockRequest::Launch { message } => Some(message.as_str()),
            MockRequest::Input(input) | MockRequest::Steer(input) => Some(input.message.as_str()),
            _ => None,
        })
        .expect("accepted dispatch must reach the actual mock input boundary");
    let (prefix, inline) = message
        .rsplit_once("\nBoard activity:\n")
        .expect("native input must include its complete serialized board-context array");
    let (_, references) = prefix.rsplit_once("Required notification post references: ")
        .expect("native input must retain required notification references independently of inline bodies");
    assert!(
        inline.len() <= protocol::SWARM_MAX_INLINE_CONTEXT_BYTES,
        "actual serialized native inline context must respect the canonical byte bound"
    );
    assert!(
        message.len() <= protocol::SWARM_MAX_INLINE_CONTEXT_BYTES + 8192,
        "wake input with this bounded guidance and focus must not grow with durable history"
    );
    (
        serde_json::from_str(references)
            .expect("parse canonical required notification post IDs from actual native input"),
        serde_json::from_str(inline).expect("parse full canonical posts from actual native input"),
    )
}

async fn controls(scenario: &Scenario, swarm: &Swarm) -> Vec<MockControl> {
    let mut controls = Vec::new();
    for member in &swarm.members {
        let agent_id = member
            .agent_id
            .as_ref()
            .expect("live member must have an agent binding");
        controls.push(scenario.fixture.mock_by_id(agent_id).await);
    }
    controls
}

async fn client_event(client: &mut client::Connection, phase: &str) -> Envelope {
    match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Ok(Some(event))) => event,
        Ok(Ok(None)) => panic!("secondary protocol connection closed during {phase}"),
        Ok(Err(error)) => panic!(
            "secondary protocol read failed during {phase}: {}",
            frame_error_context(&error)
        ),
        Err(_) => panic!("secondary protocol event timed out during {phase}"),
    }
}

async fn client_event_without_swarm(
    client: &mut client::Connection,
    phase: &str,
    predicate: impl Fn(&Envelope) -> bool,
) -> Envelope {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = client_event(client, phase).await;
            assert!(
                !matches!(
                    event.kind,
                    FrameKind::SwarmNotify
                        | FrameKind::SwarmErrorNotify
                        | FrameKind::AgentError
                        | FrameKind::CommandError
                ),
                "unrelated ordinary activity cannot emit swarm state or fail during {phase}: {:?}",
                event.kind
            );
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("ordinary activity barrier timed out during {phase}"))
}

async fn call_tool(
    caller: &server::AgentControlMcpCaller,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    let bearer = caller
        .authorization
        .strip_prefix("Bearer ")
        .expect("fixture must provide bearer authentication")
        .to_owned();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(caller.url.clone()).auth_header(bearer),
    );
    let service = ().serve(transport).await.expect("connect authenticated MCP transport");
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: name.to_owned().into(),
            arguments: arguments.as_object().cloned(),
            task: None,
        })
        .await
        .expect("call MCP tool over HTTP");
    service.cancel().await.expect("close MCP client");
    result
}

fn tool_value<T: DeserializeOwned>(result: &CallToolResult) -> T {
    assert!(
        result.is_error == Some(false),
        "authenticated tool must report explicit success"
    );
    let content = result
        .content
        .first()
        .expect("tool result must include content");
    let RawContent::Text(content) = &content.raw else {
        panic!("tool result must contain typed JSON text");
    };
    serde_json::from_str(&content.text).expect("deserialize canonical tool result")
}

fn tool_error(result: &CallToolResult) {
    assert!(
        result.is_error == Some(true),
        "unauthorized or invalid tool call must fail"
    );
    assert!(
        !result.content.is_empty(),
        "tool failure must be visible to its caller"
    );
}

fn swarm_tool_error(result: &CallToolResult, expected: SwarmErrorCode) {
    tool_error(result);
    let content = result
        .content
        .first()
        .expect("swarm tool failure must include content");
    let RawContent::Text(content) = &content.raw else {
        panic!("swarm tool failure must contain typed JSON text");
    };
    let failure: protocol::SwarmFailure = serde_json::from_str(&content.text)
        .expect("swarm tool errors must use the canonical failure type");
    assert_eq!(
        failure.code, expected,
        "swarm tool must return the specified typed refusal"
    );
    assert!(!failure.message.is_empty());
}

#[tokio::test]
async fn empty_swarm_waits_for_conversation_and_new_members_wait_for_real_posts() {
    let mut scenario = Scenario::new().await;
    let id = SwarmDraftId(uuid::Uuid::new_v4().to_string());
    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: id.clone(),
            expected_revision: None,
            name: "Interactive peers".into(),
            opening_brief: String::new(),
            constraints: scenario.constraints(1),
        })
        .await;
    let draft = scenario.draft(&id, 0).await;
    let swarm = scenario.launch(&draft).await;
    assert_eq!(swarm.lifecycle, SwarmLifecycle::Running);
    assert!(swarm.opening_post_id.is_none());
    assert!(swarm.notifications.is_empty() && swarm.rounds.is_empty());
    assert!(
        swarm
            .members
            .iter()
            .all(|member| member.state == SwarmMemberState::Proposed && member.agent_id.is_none())
    );
    for board in [SwarmBoard::Briefing, SwarmBoard::Coordination] {
        let page = scenario.board(&swarm.id, read_board(board)).await;
        assert!(page.posts.is_empty());
        assert_eq!(page.high_water, 0);
    }
    scenario
        .observe_for(Duration::from_millis(150), "empty swarm awaits the human")
        .await;
    assert!(scenario.fixture.agent_ids().await.is_empty());
    assert!(scenario.fixture.agent_session_ids().await.is_empty());

    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: swarm.id.clone(),
            expected_revision: swarm.revision,
            constraints: scenario.constraints(2),
        })
        .await;
    let previewed = scenario
        .swarm(&swarm.id, |s| s.change_preview.is_some())
        .await;
    let preview = previewed.change_preview.as_ref().unwrap();
    assert!(preview.conflicts.is_empty());
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: swarm.id.clone(),
            preview_revision: preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let expanded = scenario
        .swarm(&swarm.id, |s| s.revision > swarm.revision)
        .await;
    assert_eq!(
        expanded.lifecycle,
        SwarmLifecycle::Running,
        "configured peers are not unfinished startup work"
    );
    assert_eq!(expanded.members.len(), 2);
    assert!(expanded.notifications.is_empty());
    assert!(scenario.fixture.agent_ids().await.is_empty());

    let first = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Briefing,
                "first-real-message",
                vec![text("Let's look at the project together")],
            ),
        )
        .await;
    let active = scenario.swarm(&swarm.id, ready).await;
    for control in controls(&scenario, &active).await {
        let requests = control.requests().await;
        let (references, context) = last_dispatch_context(&requests);
        assert!(references.contains(&first.id));
        assert!(
            context
                .iter()
                .any(|post| post.id == first.id && post.author == SwarmAuthor::Human)
        );
    }
    let page = scenario
        .board(&swarm.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        page.posts.len(),
        1,
        "creation and constraint edits invent no messages"
    );
    assert!(page.posts[0].id == first.id);
}

#[tokio::test]
async fn preview_pins_stale_revisions_and_partial_launch_retry_survive_reconnect() {
    let mut scenario = Scenario::new().await;
    let initial = scenario.generate(scenario.constraints(2)).await;
    assert_eq!(
        initial.generation,
        SwarmDraftGeneration::DeterministicGeneralists
    );
    assert_eq!(
        initial.members.len(),
        2,
        "preview must state exactly who will launch"
    );
    assert!(initial.conflicts.is_empty());
    assert!(
        scenario.fixture.agent_ids().await.is_empty(),
        "generation must not activate agents"
    );
    assert!(
        scenario.fixture.agent_session_ids().await.is_empty(),
        "generation must not create sessions"
    );

    let mut draft = initial.clone();
    for (index, spec) in initial.members.iter().enumerate() {
        let mut member = spec.clone();
        member.name = format!("Pinned peer {}", index + 1);
        member.focus = Some("Editable guidance, not an assigned role".to_owned());
        member.pinned = true;
        scenario
            .send(SwarmCommandPayload::EditDraftMember {
                draft_id: draft.id.clone(),
                expected_revision: draft.revision,
                member: member.clone(),
            })
            .await;
        draft = scenario.draft(&draft.id, draft.revision + 1).await;
        assert!(
            draft.members.contains(&member),
            "member edits must be preserved"
        );
    }
    let pinned = draft.members.clone();
    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: draft.id.clone(),
            expected_revision: Some(draft.revision),
            name: draft.name.clone(),
            opening_brief: draft.opening_brief.clone(),
            constraints: scenario.constraints(1),
        })
        .await;
    draft = scenario.draft(&draft.id, draft.revision + 1).await;
    assert!(
        !draft.conflicts.is_empty(),
        "pins that exceed capacity need a visible conflict"
    );
    assert!(
        draft.members == pinned,
        "regeneration must not silently remove pinned peers"
    );
    scenario
        .send(SwarmCommandPayload::Launch {
            draft_id: draft.id.clone(),
            expected_revision: draft.revision,
        })
        .await;
    let error = scenario.error(SwarmErrorCode::Conflict).await;
    assert!(
        error.draft_id.as_ref() == Some(&draft.id),
        "draft errors must identify their subject"
    );
    assert!(
        scenario.fixture.agent_ids().await.is_empty(),
        "conflicting preview cannot launch"
    );

    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: draft.id.clone(),
            expected_revision: Some(draft.revision),
            name: draft.name.clone(),
            opening_brief: draft.opening_brief.clone(),
            constraints: scenario.constraints(2),
        })
        .await;
    draft = scenario.draft(&draft.id, draft.revision + 1).await;
    assert!(draft.conflicts.is_empty());
    assert!(
        draft.members == pinned,
        "restoring capacity must preserve the reviewed identities"
    );
    scenario
        .send(SwarmCommandPayload::EditDraftMember {
            draft_id: draft.id.clone(),
            expected_revision: initial.revision,
            member: pinned[0].clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    scenario
        .send(SwarmCommandPayload::Launch {
            draft_id: draft.id.clone(),
            expected_revision: initial.revision,
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;

    let (_, before_launch) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        before_launch.swarm_drafts.contains(&draft),
        "reconnect must replay the reviewed draft"
    );
    assert!(before_launch.swarms.is_empty());
    let failure = scenario
        .fixture
        .reserve_mock_launch_behaviors(vec![
            (
                draft.members[0].name.clone(),
                server::PendingMockLaunchBehavior::Launch(
                    server::backend::mock::MockLaunch::Script(MockScript::one(MockTurn::text(
                        "Successful approved member activation",
                    ))),
                ),
            ),
            (
                draft.members[1].name.clone(),
                server::PendingMockLaunchBehavior::FailSpawn {
                    message: "Fixture activation unavailable".to_owned(),
                },
            ),
        ])
        .await;
    let starting = scenario.launch(&draft).await;
    let partial = scenario
        .swarm(&starting.id, |swarm| {
            swarm
                .members
                .iter()
                .any(|member| member.state == SwarmMemberState::Failed)
                && swarm.members.iter().any(|member| {
                    member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Idle)
                })
        })
        .await;
    drop(failure);
    assert_eq!(partial.members.len(), draft.members.len());
    let succeeded = partial
        .members
        .iter()
        .find(|member| member.state == SwarmMemberState::Live)
        .expect("partial launch retains a successful activation")
        .clone();
    let failed = partial
        .members
        .iter()
        .find(|member| member.state == SwarmMemberState::Failed)
        .expect("partial launch identifies the failed activation")
        .clone();
    assert!(failed.error.is_some(), "activation failure must be visible");
    assert!(
        succeeded.spec.id == draft.members[0].id && failed.spec.id == draft.members[1].id,
        "partial launch must succeed and fail the specifically reserved approved members"
    );
    let activated: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "canonical swarm activation origin",
            |agent: &protocol::NewAgentPayload| {
                Some(&agent.agent_id) == succeeded.agent_id.as_ref()
            },
        )
        .await;
    let membership = protocol::SwarmMembership {
        swarm_id: partial.id.clone(),
        member_id: succeeded.spec.id.clone(),
    };
    assert_eq!(
        activated.origin,
        protocol::AgentOrigin::SwarmMember,
        "live swarm activation must have canonical SwarmMember provenance, not ordinary User origin"
    );
    assert!(activated.swarm_membership.as_ref() == Some(&membership));
    assert!(
        failed
            .error
            .as_ref()
            .is_some_and(|error| error.contains("Fixture activation unavailable")),
        "partial launch must expose the intended backend-boundary failure, not a fixture reservation mismatch"
    );
    let opening = scenario
        .board(&partial.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        opening.posts.len(),
        1,
        "opening brief is persisted once even on partial launch"
    );
    assert!(Some(&opening.posts[0].id) == partial.opening_post_id.as_ref());
    assert!(opening.posts[0].author == SwarmAuthor::Human);
    assert!(
        opening.posts[0].body == vec![text(&draft.opening_brief)],
        "opening brief must match the approved preview"
    );
    let (_, after_launch) = scenario.fixture.connect_with_bootstrap().await;
    let replayed_activation = after_launch
        .agents
        .iter()
        .find(|agent| agent.agent_id == activated.agent_id)
        .expect("reconnected canonical swarm activation descriptor");
    assert!(
        replayed_activation.origin == protocol::AgentOrigin::SwarmMember
            && replayed_activation.swarm_membership.as_ref() == Some(&membership),
        "fresh bootstrap must retain the same typed swarm origin and exact membership as the live NewAgent event"
    );
    assert!(
        !after_launch
            .swarm_drafts
            .iter()
            .any(|stored| stored.id == draft.id),
        "launched draft must be removed from initial state"
    );
    assert!(
        after_launch.swarms.iter().any(
            |swarm| swarm.id == partial.id && swarm.source_draft_id.as_ref() == Some(&draft.id)
        ),
        "launched swarm must explicitly reference its source draft"
    );
    let deleted: SwarmDraftNotifyPayload = scenario.wait(FrameKind::SwarmDraftNotify, "launched draft removal", |event| {
        matches!(event, SwarmDraftNotifyPayload::Delete { draft_id } if *draft_id == draft.id)
    }).await;
    assert!(matches!(deleted, SwarmDraftNotifyPayload::Delete { .. }));

    let successful_control = scenario
        .fixture
        .mock_by_id(succeeded.agent_id.as_ref().expect("live binding"))
        .await;
    let successful_request_count = request_count(&successful_control.requests().await);
    scenario
        .send(SwarmCommandPayload::RetryMember {
            swarm_id: partial.id.clone(),
            member_id: failed.spec.id.clone(),
        })
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: partial.id.clone(),
        })
        .await;
    let retried = scenario.swarm(&partial.id, ready).await;
    let retained = retried
        .members
        .iter()
        .find(|member| member.spec.id == succeeded.spec.id)
        .expect("successful member retained on retry");
    assert!(
        retained.agent_id == succeeded.agent_id && retained.session_id == succeeded.session_id,
        "retry must not restart successful members"
    );
    assert_eq!(
        request_count(&successful_control.requests().await),
        successful_request_count,
        "retry must not redeliver the opening brief to successful members"
    );
    let board = scenario
        .board(&partial.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        board.posts.len(),
        1,
        "activation retry must not republish the opening brief"
    );
    assert!(board.posts[0].id == opening.posts[0].id);
    let stored = scenario.snapshot(&partial.id).await;
    assert!(
        stored.members == retried.members,
        "reconnect must expose retry outcomes and stable sessions"
    );

    let uncertain_draft = scenario.generate(scenario.constraints(1)).await;
    let before_uncertain_launch = scenario.fixture.agent_ids().await.len();
    scenario.fixture.fail_next_swarm_directory_sync().await;
    let committed_launch = scenario.launch(&uncertain_draft).await;
    scenario
        .error(SwarmErrorCode::CommittedDurabilityUncertain)
        .await;
    let launch_attention = scenario
        .swarm(&committed_launch.id, |state| {
            state.lifecycle == SwarmLifecycle::AttentionRequired && state.error.is_some()
        })
        .await;
    assert!(
        launch_attention
            .members
            .iter()
            .all(|member| member.agent_id.is_none() && member.session_id.is_none()),
        "committed uncertain launch must withhold fresh native execution until explicit human Resume"
    );
    assert_eq!(
        launch_attention.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    scenario
        .send(SwarmCommandPayload::RetryMember {
            swarm_id: committed_launch.id.clone(),
            member_id: launch_attention.members[0].spec.id.clone(),
        })
        .await;
    let retried_uncertain_launch = scenario.snapshot_after_commands(&committed_launch.id).await;
    assert_eq!(retried_uncertain_launch.revision, launch_attention.revision);
    assert_eq!(
        retried_uncertain_launch.lifecycle,
        SwarmLifecycle::AttentionRequired,
        "retrying a healthy Proposed member cannot bypass launch durability recovery"
    );
    assert_eq!(
        retried_uncertain_launch.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    assert!(
        retried_uncertain_launch.error.is_some()
            && retried_uncertain_launch.members == launch_attention.members
            && retried_uncertain_launch.notifications == launch_attention.notifications
            && retried_uncertain_launch.rounds == launch_attention.rounds
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        before_uncertain_launch
    );
    let _: SwarmDraftNotifyPayload = scenario.wait(FrameKind::SwarmDraftNotify, "committed uncertain launch draft deletion", |event|
        matches!(event, SwarmDraftNotifyPayload::Delete { draft_id } if *draft_id == uncertain_draft.id)).await;
    let committed_brief = scenario
        .board(&committed_launch.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(committed_brief.posts.len(), 1);
    assert!(
        committed_brief.posts[0].body == vec![text(&uncertain_draft.opening_brief)]
            && launch_attention.opening_post_id.as_ref() == Some(&committed_brief.posts[0].id),
        "the committed opening post must be observable despite an uncertain directory sync"
    );
    scenario
        .send(SwarmCommandPayload::MarkRead {
            swarm_id: committed_launch.id.clone(),
            board: SwarmBoard::Briefing,
            cursor: committed_brief.posts[0].cursor,
        })
        .await;
    let marked_uncertain_launch = scenario
        .swarm(&committed_launch.id, |state| {
            state.board_positions.iter().any(|position| {
                position.board == SwarmBoard::Briefing
                    && position.human_read_cursor == committed_brief.posts[0].cursor
            })
        })
        .await;
    assert_eq!(
        marked_uncertain_launch.lifecycle,
        SwarmLifecycle::AttentionRequired
    );
    assert_eq!(
        marked_uncertain_launch.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    assert!(
        marked_uncertain_launch.notifications == launch_attention.notifications
            && marked_uncertain_launch.members == launch_attention.members
            && marked_uncertain_launch.error.is_some(),
        "a durable read acknowledgement cannot authorize an uncertain launch"
    );
    scenario
        .send(SwarmCommandPayload::Launch {
            draft_id: uncertain_draft.id.clone(),
            expected_revision: uncertain_draft.revision,
        })
        .await;
    scenario.error(SwarmErrorCode::NotFound).await;
    let (_, launch_retry_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert_eq!(
        launch_retry_bootstrap
            .swarms
            .iter()
            .filter(|swarm| swarm.source_draft_id.as_ref() == Some(&uncertain_draft.id))
            .count(),
        1,
        "retry after a committed launch warning cannot create a second swarm from the consumed draft"
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        before_uncertain_launch,
        "read and retry commands cannot reauthorize a committed uncertain launch"
    );
    let affected_prior = scenario.snapshot(&partial.id).await;
    assert_eq!(affected_prior.lifecycle, SwarmLifecycle::AttentionRequired);
    assert!(
        affected_prior.error.is_some() && affected_prior.members == retried.members,
        "shared-store uncertainty must preserve existing member ownership while requiring human review"
    );
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: committed_launch.id.clone(),
        })
        .await;
    let committed_live = scenario.swarm(&committed_launch.id, ready).await;
    assert_eq!(
        committed_live.recovery_requirement,
        protocol::SwarmRecoveryRequirement::None
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        before_uncertain_launch + 1,
        "only explicit Resume may activate the committed uncertain launch"
    );
    assert_eq!(
        scenario.snapshot(&partial.id).await.lifecycle,
        SwarmLifecycle::AttentionRequired,
        "a subsequent durable write cannot silently clear another group's required review"
    );
    scenario.pause(&committed_live.id).await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: partial.id.clone(),
        })
        .await;
    scenario.swarm(&partial.id, ready).await;
    let caller = scenario
        .fixture
        .agent_control_caller(
            succeeded
                .agent_id
                .as_ref()
                .expect("successful authenticated member"),
        )
        .await;
    let target = retried
        .members
        .iter()
        .find(|member| member.spec.id != succeeded.spec.id)
        .expect("second approved peer");
    let target_control = scenario
        .fixture
        .mock_by_id(target.agent_id.as_ref().expect("second peer runtime"))
        .await;
    let before_warning_dispatch = request_count(&target_control.requests().await);
    let uncertain_publication = publication(
        SwarmBoard::Coordination,
        "post-rename-committed-publication",
        vec![
            text(
                "A committed publication remains shared even when directory durability is uncertain",
            ),
            SwarmBodySegment::MemberMention {
                member_id: target.spec.id.clone(),
            },
        ],
    );
    scenario.fixture.fail_next_swarm_directory_sync().await;
    let uncertain_outcome: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(&uncertain_publication)
                .expect("serialize committed-warning publication"),
        )
        .await,
    );
    assert!(
        matches!(&uncertain_outcome.commit_status,
        protocol::SwarmCommitStatus::CommittedDurabilityUncertain { message } if !message.is_empty()),
        "MCP must return the canonical typed committed-but-durability-uncertain outcome, not a failed rollback"
    );
    assert!(
        !uncertain_outcome.duplicate
            && uncertain_outcome.post.author
                == SwarmAuthor::Member {
                    member_id: succeeded.spec.id.clone()
                }
    );
    assert_eq!(uncertain_outcome.deliveries.len(), 1);
    assert!(uncertain_outcome.deliveries[0].member_id == target.spec.id);
    assert_eq!(
        uncertain_outcome.deliveries[0].state,
        SwarmDeliveryState::Pending
    );
    let broadcast: SwarmPostNotifyPayload = scenario
        .wait(
            FrameKind::SwarmPostNotify,
            "committed warning post broadcast",
            |event: &SwarmPostNotifyPayload| event.post.id == uncertain_outcome.post.id,
        )
        .await;
    assert!(
        broadcast.post == uncertain_outcome.post,
        "all clients must receive the actual committed post, not only a storage warning"
    );
    scenario
        .error(SwarmErrorCode::CommittedDurabilityUncertain)
        .await;
    let before_warning_retry = scenario.snapshot(&partial.id).await;
    assert_eq!(
        before_warning_retry.lifecycle,
        SwarmLifecycle::AttentionRequired
    );
    assert!(before_warning_retry.error.is_some());
    assert_eq!(
        before_warning_retry.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    scenario
        .send(SwarmCommandPayload::RetryMember {
            swarm_id: partial.id.clone(),
            member_id: target.spec.id.clone(),
        })
        .await;
    let member_retry_attention = scenario.snapshot_after_commands(&partial.id).await;
    assert_eq!(
        member_retry_attention.revision,
        before_warning_retry.revision
    );
    assert_eq!(
        member_retry_attention.lifecycle,
        SwarmLifecycle::AttentionRequired,
        "retrying a healthy live member cannot authorize a post with uncertain directory durability"
    );
    assert_eq!(
        member_retry_attention.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    assert!(
        member_retry_attention.error.is_some()
            && member_retry_attention.members == before_warning_retry.members
            && member_retry_attention.notifications == before_warning_retry.notifications
            && member_retry_attention.rounds == before_warning_retry.rounds
    );
    let before_warning_retry = member_retry_attention;
    assert_eq!(
        scenario.snapshot(&committed_live.id).await.lifecycle,
        SwarmLifecycle::Paused,
        "directory uncertainty must not unpause an already-paused group"
    );
    assert_eq!(
        request_count(&target_control.requests().await),
        before_warning_dispatch,
        "committed uncertain peer publication cannot cross native admission before human Resume"
    );
    let duplicate: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(&uncertain_publication)
                .expect("serialize committed publication retry"),
        )
        .await,
    );
    assert!(
        duplicate.duplicate
            && duplicate.post == uncertain_outcome.post
            && duplicate.deliveries == uncertain_outcome.deliveries,
        "retry after a post-rename warning must resolve the committed post and original intents exactly once"
    );
    if matches!(
        duplicate.commit_status,
        protocol::SwarmCommitStatus::CommittedDurabilityUncertain { .. }
    ) {
        scenario
            .error(SwarmErrorCode::CommittedDurabilityUncertain)
            .await;
    }
    assert!(
        scenario.snapshot(&partial.id).await == before_warning_retry,
        "idempotent committed publication retry cannot change causal allowances, notifications or runtime ownership"
    );
    scenario
        .send(SwarmCommandPayload::MarkRead {
            swarm_id: partial.id.clone(),
            board: SwarmBoard::Coordination,
            cursor: uncertain_outcome.post.cursor,
        })
        .await;
    let durably_review_required = scenario
        .swarm(&partial.id, |state| {
            state.board_positions.iter().any(|position| {
                position.board == SwarmBoard::Coordination
                    && position.human_read_cursor == uncertain_outcome.post.cursor
            })
        })
        .await;
    assert_eq!(
        durably_review_required.lifecycle,
        SwarmLifecycle::AttentionRequired
    );
    assert!(durably_review_required.error.is_some());
    assert_eq!(
        durably_review_required.recovery_requirement,
        protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    let durable_retry: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(&uncertain_publication)
                .expect("serialize retry after subsequent durable write"),
        )
        .await,
    );
    assert_eq!(
        durable_retry.commit_status,
        protocol::SwarmCommitStatus::Durable
    );
    assert!(
        durable_retry.duplicate
            && durable_retry.post == uncertain_outcome.post
            && durable_retry.deliveries == uncertain_outcome.deliveries
    );
    assert_eq!(
        request_count(&target_control.requests().await),
        before_warning_dispatch,
        "confirming durability or retrying the post cannot silently clear attention or dispatch a peer wake"
    );
    let persisted_store: protocol::SwarmStoreSnapshot = serde_json::from_slice(
        &std::fs::read(scenario.fixture.swarm_store_path())
            .expect("read actual post-rename committed store"),
    )
    .expect("decode committed filesystem snapshot");
    assert_eq!(
        persisted_store
            .posts
            .iter()
            .filter(|post| post.id == uncertain_outcome.post.id)
            .count(),
        1
    );
    assert!(
        persisted_store
            .swarms
            .iter()
            .any(|swarm| swarm.id == committed_live.id)
            && !persisted_store
                .drafts
                .iter()
                .any(|draft| draft.id == uncertain_draft.id),
        "the real renamed file must agree with canonical launch and draft-consumption events"
    );
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: partial.id.clone(),
        })
        .await;
    let accepted_warning = scenario
        .swarm(&partial.id, |state| {
            ready(state)
                && state.notifications.iter().any(|intent| {
                    intent.id == uncertain_outcome.deliveries[0].id
                        && intent.state == SwarmDeliveryState::Accepted
                })
        })
        .await;
    assert_eq!(
        accepted_warning.recovery_requirement,
        protocol::SwarmRecoveryRequirement::None
    );
    assert_eq!(
        request_count(&target_control.requests().await),
        before_warning_dispatch + 1
    );
    assert_eq!(
        accepted_warning
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&uncertain_outcome.post.id))
            .count(),
        1,
        "human Resume must accept the exact committed intent only once"
    );
    scenario.pause(&partial.id).await;
    let restarted = scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let restored_launch = restarted
        .swarms
        .iter()
        .find(|swarm| swarm.id == committed_live.id)
        .expect("post-rename committed launch survives restart");
    assert_eq!(restored_launch.lifecycle, SwarmLifecycle::Paused);
    assert!(restored_launch.members[0].session_id == committed_live.members[0].session_id);
    let restored_post_owner = restarted
        .swarms
        .iter()
        .find(|swarm| swarm.id == partial.id)
        .expect("post-rename committed publication owner survives restart");
    assert!(
        restored_post_owner.notifications == accepted_warning.notifications
            && restored_post_owner.rounds == accepted_warning.rounds,
        "restart must retain committed post cause and exact intent identities after directory-sync warning"
    );
    let restored_coordination = scenario
        .board(&partial.id, read_board(SwarmBoard::Coordination))
        .await;
    assert_eq!(restored_coordination.posts.len(), 1);
    assert!(restored_coordination.posts[0] == uncertain_outcome.post);
    let restored_brief = scenario
        .board(&committed_live.id, read_board(SwarmBoard::Briefing))
        .await;
    assert!(
        restored_brief.posts == committed_brief.posts,
        "restart must not lose or duplicate the uncertain committed opening post"
    );
}

fn initialize_scope_repository(project: &Project) {
    for root in project.root_paths() {
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.email", "tests@example.com"],
            vec!["config", "user.name", "Tests"],
            vec!["add", "."],
            vec!["commit", "-m", "Initial reference"],
        ] {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&root.0)
                .args(args)
                .output()
                .expect("run fixture git");
            assert!(
                result.status.success(),
                "real git fixture setup must succeed"
            );
        }
    }
}

#[tokio::test]
async fn workspace_scopes_authorize_current_projects_workbenches_and_resume_roots() {
    for policy in [
        SwarmWorkspacePolicy::SharedProject {
            writable_consent: true,
        },
        SwarmWorkspacePolicy::SharedHost {
            writable_consent: true,
        },
        SwarmWorkspacePolicy::SharedWorkbench {
            writable_consent: true,
        },
        SwarmWorkspacePolicy::ReadOnly,
    ] {
        let mut scenario = Scenario::new().await;
        initialize_scope_repository(&scenario.project);
        let other = scenario.add_project("other-project").await;
        initialize_scope_repository(&other);
        let parent = scenario.project.clone();
        let existing = scenario.add_workbench(&parent, "existing-workbench").await;
        let broad = matches!(
            policy,
            SwarmWorkspacePolicy::SharedProject { .. } | SwarmWorkspacePolicy::SharedHost { .. }
        );
        let host_scope = matches!(policy, SwarmWorkspacePolicy::SharedHost { .. });
        let mut constraints = scenario.constraints(1);
        constraints.workspace_policy = policy;
        if matches!(policy, SwarmWorkspacePolicy::SharedWorkbench { .. }) {
            constraints.project_id = existing.id.clone();
        }
        let draft = scenario.generate(constraints).await;
        let launched = scenario.launch(&draft).await;
        let swarm = scenario.swarm(&launched.id, ready).await;
        scenario.pause(&swarm.id).await;
        let agent = swarm.members[0]
            .agent_id
            .as_ref()
            .expect("scoped live member");
        let caller = scenario.fixture.agent_control_caller(agent).await;
        let describe: SwarmDescribe =
            tool_value(&call_tool(&caller, "tyde_swarm_describe", json!({})).await);
        assert_eq!(
            describe.workspace_projects.len(),
            if host_scope {
                3
            } else if broad {
                2
            } else {
                1
            }
        );
        assert!(
            describe
                .workspace_projects
                .iter()
                .any(|project| project.id == other.id)
                == host_scope
        );
        let start = scenario.start_for_agent(agent).await;
        let expected_roots = describe
            .workspace_projects
            .iter()
            .flat_map(|project| project.root_paths())
            .map(|root| root.0)
            .collect::<Vec<_>>();
        assert_eq!(
            start.workspace_roots, expected_roots,
            "activation must carry server-authorized roots"
        );
        let bearer = caller
            .authorization
            .strip_prefix("Bearer ")
            .expect("fixture bearer")
            .to_owned();
        let service = ()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(caller.url.clone())
                    .auth_header(bearer),
            ))
            .await
            .expect("connect scoped catalog");
        let tools = service.list_all_tools().await.expect("list scoped tools");
        let mut names = tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        names.sort_unstable();
        let mut expected = vec![
            "tyde_swarm_describe",
            "tyde_swarm_read_board",
            "tyde_swarm_read_thread",
            "tyde_swarm_read_image",
            "tyde_swarm_post",
        ];
        if broad {
            expected.extend([
                "tyde_list_workbenches",
                "tyde_create_workbench",
                "tyde_remove_workbench",
            ]);
        }
        expected.sort_unstable();
        assert_eq!(
            names, expected,
            "scope exposes only its reviewed board and workbench tools"
        );
        assert!(!tools.iter().any(|tool| tool.name == "tyde_spawn_agent"));
        service.cancel().await.expect("close scoped catalog");
        tool_error(
            &call_tool(
                &caller,
                "tyde_spawn_agent",
                json!({ "prompt": "Do not spawn" }),
            )
            .await,
        );

        let create_args = json!({ "parent_project_id": parent.id.0, "branch": "new-workbench" });
        if !broad {
            tool_error(&call_tool(&caller, "tyde_create_workbench", create_args).await);
            tool_error(&call_tool(&caller, "tyde_list_workbenches", json!({})).await);
            let mut outside = publication(
                SwarmBoard::Coordination,
                "outside-scope",
                vec![text("Reference")],
            );
            outside.attachments.push(protocol::SwarmAttachment {
                project_id: other.id.clone(),
                path: protocol::ProjectPath {
                    root: other.root_paths()[0].clone(),
                    relative_path: "reference.txt".to_owned(),
                },
            });
            swarm_tool_error(
                &call_tool(
                    &caller,
                    "tyde_swarm_post",
                    serde_json::to_value(outside).expect("attachment publication"),
                )
                .await,
                SwarmErrorCode::Unauthorized,
            );
            continue;
        }
        let created: Value =
            tool_value(&call_tool(&caller, "tyde_create_workbench", create_args).await);
        let fresh_id = protocol::ProjectId(
            created["project_id"]
                .as_str()
                .expect("created ID")
                .to_owned(),
        );
        let current: SwarmDescribe =
            tool_value(&call_tool(&caller, "tyde_swarm_describe", json!({})).await);
        let fresh = current
            .workspace_projects
            .iter()
            .find(|project| project.id == fresh_id)
            .expect("new workbench joins the active scope without recreating the swarm")
            .clone();
        let listed: Value =
            tool_value(&call_tool(&caller, "tyde_list_workbenches", json!({})).await);
        assert_eq!(
            listed["projects"]
                .as_array()
                .expect("scoped projects")
                .len(),
            current.workspace_projects.len()
        );
        let mut attached = publication(
            SwarmBoard::Coordination,
            "new-workbench-reference",
            vec![text("Reference")],
        );
        attached.attachments.push(protocol::SwarmAttachment {
            project_id: fresh.id.clone(),
            path: protocol::ProjectPath {
                root: fresh.root_paths()[0].clone(),
                relative_path: "reference.txt".to_owned(),
            },
        });
        let published: SwarmPublicationOutcome = tool_value(
            &call_tool(
                &caller,
                "tyde_swarm_post",
                serde_json::to_value(attached).expect("attachment publication"),
            )
            .await,
        );
        assert_eq!(published.post.attachments.len(), 1);
        let late = scenario.add_project("late-project").await;
        initialize_scope_repository(&late);
        let after_add: SwarmDescribe =
            tool_value(&call_tool(&caller, "tyde_swarm_describe", json!({})).await);
        assert!(
            after_add
                .workspace_projects
                .iter()
                .any(|project| project.id == late.id)
                == host_scope
        );
        let outside_args = json!({ "parent_project_id": late.id.0, "branch": "late-workbench" });
        if host_scope {
            let _: Value =
                tool_value(&call_tool(&caller, "tyde_create_workbench", outside_args).await);
        } else {
            tool_error(&call_tool(&caller, "tyde_create_workbench", outside_args).await);
            let mut outside = publication(
                SwarmBoard::Coordination,
                "outside-project",
                vec![text("Reference")],
            );
            outside.attachments.push(protocol::SwarmAttachment {
                project_id: late.id.clone(),
                path: protocol::ProjectPath {
                    root: late.root_paths()[0].clone(),
                    relative_path: "reference.txt".to_owned(),
                },
            });
            swarm_tool_error(
                &call_tool(
                    &caller,
                    "tyde_swarm_post",
                    serde_json::to_value(outside).expect("outside publication"),
                )
                .await,
                SwarmErrorCode::Unauthorized,
            );
        }
        scenario
            .post(
                &swarm.id,
                publication(
                    SwarmBoard::Briefing,
                    "resume-with-current-scope",
                    vec![text("Inspect the current workspace scope")],
                ),
            )
            .await;
        let resumed_bootstrap = scenario.fixture.restart_host().await;
        scenario.pending.clear();
        assert!(
            resumed_bootstrap
                .swarms
                .iter()
                .any(|saved| saved.id == swarm.id && saved.constraints == swarm.constraints)
        );
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: swarm.id.clone(),
            })
            .await;
        let resumed = scenario.swarm(&swarm.id, ready).await;
        let resumed_agent = resumed.members[0]
            .agent_id
            .as_ref()
            .expect("resumed member");
        let resumed_caller = scenario.fixture.agent_control_caller(resumed_agent).await;
        let resumed_scope: SwarmDescribe =
            tool_value(&call_tool(&resumed_caller, "tyde_swarm_describe", json!({})).await);
        let resumed_start = scenario.start_for_agent(resumed_agent).await;
        let resumed_roots = resumed_scope
            .workspace_projects
            .iter()
            .flat_map(|project| project.root_paths())
            .map(|root| root.0)
            .collect::<Vec<_>>();
        assert_eq!(
            resumed_start.workspace_roots, resumed_roots,
            "resume must resolve new workbenches, not retain original roots"
        );
        assert!(
            resumed_start
                .workspace_roots
                .contains(&fresh.root_paths()[0].0)
        );
        let _: Value = tool_value(
            &call_tool(
                &resumed_caller,
                "tyde_remove_workbench",
                json!({ "project_id": fresh.id.0 }),
            )
            .await,
        );
        let after_remove: SwarmDescribe =
            tool_value(&call_tool(&resumed_caller, "tyde_swarm_describe", json!({})).await);
        assert!(
            !after_remove
                .workspace_projects
                .iter()
                .any(|project| project.id == fresh.id)
        );
        scenario.pause(&swarm.id).await;
        scenario
            .post(
                &swarm.id,
                publication(
                    SwarmBoard::Briefing,
                    "resume-after-removal",
                    vec![text("Inspect the remaining workspace")],
                ),
            )
            .await;
        scenario.fixture.restart_host().await;
        scenario.pending.clear();
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: swarm.id.clone(),
            })
            .await;
        let resumed_after_removal = scenario.swarm(&swarm.id, ready).await;
        let remaining_start = scenario
            .start_for_agent(
                resumed_after_removal.members[0]
                    .agent_id
                    .as_ref()
                    .expect("remaining scope member"),
            )
            .await;
        assert!(
            !remaining_start
                .workspace_roots
                .contains(&fresh.root_paths()[0].0),
            "removed workbenches must not survive in resumed configuration"
        );
        scenario.pause(&swarm.id).await;
    }
}

#[tokio::test]
async fn constraint_and_workspace_errors_start_no_work_and_preserve_the_draft() {
    let mut scenario = Scenario::new().await;
    let draft = scenario.generate(scenario.constraints(1)).await;
    let valid = draft.constraints.clone();
    let mut invalid_constraints = Vec::new();
    let mut zero = valid.clone();
    zero.max_live_agents = 0;
    invalid_constraints.push(zero);
    let mut over_host_limit = valid.clone();
    over_host_limit.max_live_agents = 17;
    invalid_constraints.push(over_host_limit);
    let mut oversubscribed = valid.clone();
    oversubscribed.allocations[0].count = 2;
    invalid_constraints.push(oversubscribed);
    let mut zero_allocation = valid.clone();
    zero_allocation.allocations[0].count = 0;
    invalid_constraints.push(zero_allocation);
    let mut mismatched_backend = valid.clone();
    mismatched_backend.allocations[0].backend_kind = protocol::BackendKind::Codex;
    invalid_constraints.push(mismatched_backend);
    let mut unbounded_wake = valid.clone();
    unbounded_wake.agent_wake_budget = 129;
    invalid_constraints.push(unbounded_wake);
    let mut unsafe_workspace = valid.clone();
    unsafe_workspace.workspace_policy = SwarmWorkspacePolicy::SharedWorkbench {
        writable_consent: true,
    };
    invalid_constraints.push(unsafe_workspace);
    for workspace_policy in [
        SwarmWorkspacePolicy::SharedProject {
            writable_consent: false,
        },
        SwarmWorkspacePolicy::SharedHost {
            writable_consent: false,
        },
    ] {
        let mut unconsented = valid.clone();
        unconsented.workspace_policy = workspace_policy;
        invalid_constraints.push(unconsented);
    }
    let mut invalid_settings = valid.clone();
    invalid_settings.allocations[0].session_settings.0.insert(
        "not_a_schema_owned_field".to_owned(),
        protocol::SessionSettingValue::String("invalid".to_owned()),
    );
    invalid_constraints.push(invalid_settings);
    for constraints in invalid_constraints {
        scenario
            .send(SwarmCommandPayload::GenerateDraft {
                draft_id: draft.id.clone(),
                expected_revision: Some(draft.revision),
                name: draft.name.clone(),
                opening_brief: draft.opening_brief.clone(),
                constraints,
            })
            .await;
        let error = scenario.error(SwarmErrorCode::Invalid).await;
        assert!(error.draft_id.as_ref() == Some(&draft.id));
    }
    let mut unavailable = valid;
    unavailable.allocations[0].launch_profile_id =
        protocol::LaunchProfileId("unavailable-profile".to_owned());
    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: draft.id.clone(),
            expected_revision: Some(draft.revision),
            name: draft.name.clone(),
            opening_brief: draft.opening_brief.clone(),
            constraints: unavailable,
        })
        .await;
    scenario.error(SwarmErrorCode::Unsupported).await;
    let (_, bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.swarm_drafts.contains(&draft),
        "failed generation must leave the prior preview intact"
    );
    assert!(bootstrap.swarms.is_empty());
    assert!(bootstrap.sessions.is_empty());
    assert!(scenario.fixture.agent_ids().await.is_empty());
    scenario
        .send(SwarmCommandPayload::DiscardDraft {
            draft_id: draft.id.clone(),
        })
        .await;
    let _: SwarmDraftNotifyPayload = scenario.wait(FrameKind::SwarmDraftNotify, "discard saved draft", |event| {
        matches!(event, SwarmDraftNotifyPayload::Delete { draft_id } if *draft_id == draft.id)
    }).await;
    let (_, discarded) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        discarded.swarm_drafts.is_empty(),
        "discard must be durable and replayable"
    );
}

#[tokio::test]
async fn durable_boards_route_only_typed_mentions_and_page_old_thread_activity() {
    let mut scenario = Scenario::new().await;
    let swarm = scenario.launched(2).await;
    let mock_controls = controls(&scenario, &swarm).await;
    scenario.pause(&swarm.id).await;
    let first_member = swarm.members[0].spec.id.clone();
    let second_member = swarm.members[1].spec.id.clone();
    let (mut peer, peer_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        peer_bootstrap
            .swarms
            .iter()
            .any(|state| state.id == swarm.id)
    );
    let literal = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Coordination,
                "literal-reference",
                vec![text(&format!(
                    "@{} is a literal display name, not an authenticated mention",
                    swarm.members[0].spec.name
                ))],
            ),
        )
        .await;
    let shared_only = scenario.snapshot(&swarm.id).await;
    assert!(
        !shared_only
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&literal.id)),
        "unmentioned Coordination root is shared context, not a wake-up"
    );

    let mut peer_publication = false;
    let mut peer_state = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !peer_publication || !peer_state {
            let event = client_event(&mut peer, "shared committed publication").await;
            match event.kind {
                FrameKind::SwarmPostNotify => {
                    let post: SwarmPostNotifyPayload = event.parse_payload().expect("shared post");
                    if post.post.id == literal.id {
                        assert!(post.post == literal, "both clients must see the exact committed post");
                        peer_publication = true;
                    }
                }
                FrameKind::SwarmNotify => {
                    let state: SwarmNotifyPayload = event.parse_payload().expect("shared state");
                    if state.swarm.id == swarm.id && state.swarm.revision >= shared_only.revision {
                        assert!(state.swarm == shared_only, "both clients must see the canonical publication state");
                        peer_state = true;
                    }
                }
                kind => assert!(!matches!(kind, FrameKind::SwarmBoardNotify | FrameKind::SwarmThreadNotify | FrameKind::SwarmErrorNotify), "publication must not invent a read response or rejection on the other client: {kind:?}"),
            }
        }
    }).await.expect("both clients must receive committed publication and state");

    let private_board = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(private_board.posts == vec![literal.clone()]);
    let private_thread = scenario
        .thread(
            &swarm.id,
            SwarmThreadRead {
                thread_id: literal.thread_id.clone(),
                after_cursor: None,
                limit: Some(1),
            },
        )
        .await;
    assert!(private_thread.root == literal && private_thread.posts.is_empty());
    scenario
        .send(SwarmCommandPayload::ReadPost {
            swarm_id: swarm.id.clone(),
            post_id: literal.id.clone(),
        })
        .await;
    let deep_link: SwarmThreadNotifyPayload = scenario
        .wait(
            FrameKind::SwarmThreadNotify,
            "requester-local deep link",
            |event: &SwarmThreadNotifyPayload| event.page.root.id == literal.id,
        )
        .await;
    assert!(
        deep_link.page == private_thread,
        "deep-link resolution must retain exact thread context on its caller"
    );
    scenario
        .send(SwarmCommandPayload::ReadBoard {
            swarm_id: swarm.id.clone(),
            query: SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: None,
                limit: Some(0),
            },
        })
        .await;
    let private_error = scenario.error(SwarmErrorCode::Invalid).await;
    assert!(private_error.swarm_id.as_ref() == Some(&swarm.id));

    // Pages and rejections belong to their requesting UI stream; only committed
    // state/publications are shared. The peer's own bulk response is a FIFO
    // barrier that exposes any leaked earlier response without a quiet sleep.
    peer.swarm_command(SwarmCommandPayload::ReadBoard {
        swarm_id: swarm.id.clone(),
        query: SwarmBoardRead {
            board: SwarmBoard::Briefing,
            after_cursor: None,
            limit: Some(1),
        },
    })
    .await
    .expect("peer board-read barrier");
    let peer_board = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = client_event(&mut peer, "caller-local read barrier").await;
            match event.kind {
                FrameKind::SwarmBoardNotify => {
                    let board: SwarmBoardNotifyPayload =
                        event.parse_payload().expect("peer's own board page");
                    assert!(
                        board.page.swarm_id == swarm.id && board.page.board == SwarmBoard::Briefing,
                        "a peer must not receive another client's Coordination page"
                    );
                    return board.page;
                }
                kind => assert!(
                    !matches!(
                        kind,
                        FrameKind::SwarmThreadNotify | FrameKind::SwarmErrorNotify
                    ),
                    "another client's thread/deep-link/rejection must not reach this peer: {kind:?}"
                ),
            }
        }
    })
    .await
    .expect("peer must receive its own requested page");
    assert_eq!(peer_board.posts.len(), 1);
    assert!(Some(&peer_board.posts[0].id) == swarm.opening_post_id.as_ref());
    let reverse_barrier = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(reverse_barrier == private_board);
    assert!(
        !scenario.pending.iter().any(|event| match event.kind {
            FrameKind::SwarmBoardNotify => {
                let board: SwarmBoardNotifyPayload =
                    event.parse_payload().expect("pending page scope");
                board.page.swarm_id == swarm.id && board.page.board == SwarmBoard::Briefing
            }
            _ => false,
        }),
        "peer's own read must not broadcast a Briefing response back to the first client"
    );
    assert!(
        scenario.snapshot(&swarm.id).await == shared_only,
        "caller-local reads and rejection cannot mutate shared canonical state"
    );
    drop(peer);

    let mut typed_publication = publication(
        SwarmBoard::Coordination,
        "typed-reference",
        vec![
            text("Please review the linked context"),
            SwarmBodySegment::MemberMention {
                member_id: first_member.clone(),
            },
            SwarmBodySegment::MemberMention {
                member_id: first_member.clone(),
            },
            SwarmBodySegment::PostLink {
                post_id: swarm.opening_post_id.clone().expect("opening reference"),
            },
        ],
    );
    let typed = scenario.post(&swarm.id, typed_publication.clone()).await;
    let after_typed = scenario.snapshot(&swarm.id).await;
    let notifications = after_typed
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&typed.id))
        .collect::<Vec<_>>();
    assert_eq!(
        notifications.len(),
        1,
        "duplicate typed mentions must deduplicate recipients"
    );
    assert!(notifications[0].member_id == first_member);
    assert_eq!(notifications[0].state, SwarmDeliveryState::Pending);
    let repeated = scenario.post(&swarm.id, typed_publication.clone()).await;
    assert!(
        repeated == typed,
        "retry must return the existing durable publication"
    );
    let after_repeat = scenario.snapshot(&swarm.id).await;
    assert_eq!(
        after_repeat
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&typed.id))
            .count(),
        1,
        "idempotent retry must not create another delivery intent"
    );
    assert_eq!(
        after_repeat.rounds.len(),
        after_typed.rounds.len(),
        "retry cannot mint a new causal allowance"
    );
    typed_publication
        .body
        .push(text("A different body under the same key"));
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: typed_publication,
        })
        .await;
    let conflict = scenario.error(SwarmErrorCode::Conflict).await;
    assert!(conflict.swarm_id.as_ref() == Some(&swarm.id));
    assert!(
        conflict.publication_id.as_ref() == Some(&typed.publication_id),
        "publication errors must bind to the rejected submission"
    );

    let briefing_all = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Briefing,
                "human-briefing",
                vec![text("Please publish your findings")],
            ),
        )
        .await;
    let briefing_one = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Briefing,
                "narrowed-briefing",
                vec![
                    text("One peer needs this context"),
                    SwarmBodySegment::MemberMention {
                        member_id: second_member.clone(),
                    },
                ],
            ),
        )
        .await;
    let routed = scenario.snapshot(&swarm.id).await;
    assert_eq!(
        routed
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&briefing_all.id))
            .count(),
        2,
        "unmentioned human Briefing root must notify all eligible members"
    );
    let narrowed = routed
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&briefing_one.id))
        .collect::<Vec<_>>();
    assert_eq!(
        narrowed.len(),
        1,
        "explicit Briefing mention narrows the audience"
    );
    assert!(narrowed[0].member_id == second_member);

    let mut reply_publication = publication(
        SwarmBoard::Coordination,
        "old-root-reply",
        vec![text("Reply after newer roots and cross-board activity")],
    );
    reply_publication.thread_id = Some(literal.thread_id.clone());
    let reply = scenario.post(&swarm.id, reply_publication.clone()).await;
    assert!(reply.thread_id == literal.thread_id);
    assert!(reply.cursor > typed.cursor && reply.cursor > briefing_one.cursor);
    assert!(reply.author == SwarmAuthor::Human);
    let mut second_reply_publication = publication(
        SwarmBoard::Coordination,
        "old-root-reply-two",
        vec![text("Further reply activity")],
    );
    second_reply_publication.thread_id = Some(literal.thread_id.clone());
    let second_reply = scenario.post(&swarm.id, second_reply_publication).await;
    reply_publication.board = SwarmBoard::Briefing;
    reply_publication.publication_id = SwarmPublicationId("wrong-board-reply".to_owned());
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: reply_publication,
        })
        .await;
    scenario.error(SwarmErrorCode::Invalid).await;

    let first_two = scenario
        .board(
            &swarm.id,
            SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: None,
                limit: Some(2),
            },
        )
        .await;
    assert!(
        first_two
            .posts
            .iter()
            .map(|post| &post.id)
            .eq([&literal.id, &typed.id])
    );
    let first_page = scenario
        .board(
            &swarm.id,
            SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: None,
                limit: Some(1),
            },
        )
        .await;
    assert_eq!(first_page.posts.len(), 1);
    assert!(first_page.posts[0].id == literal.id);
    assert!(first_page.has_more);
    assert_eq!(first_page.next_cursor.position, literal.cursor);
    assert_eq!(first_page.high_water, second_reply.cursor);
    let late_root = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Coordination,
                "live-root-during-pagination",
                vec![text(
                    "New live activity must not extend an existing historical snapshot",
                )],
            ),
        )
        .await;
    assert!(late_root.cursor > first_page.high_water);
    let remaining = scenario
        .board(
            &swarm.id,
            SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: Some(first_page.next_cursor.clone()),
                limit: Some(100),
            },
        )
        .await;
    assert!(
        remaining
            .posts
            .iter()
            .map(|post| &post.id)
            .eq([&typed.id, &reply.id, &second_reply.id]),
        "board pages must include old-thread replies in activity order"
    );
    assert!(!remaining.has_more);
    assert_eq!(remaining.high_water, first_page.high_water);
    assert_eq!(remaining.next_cursor.position, second_reply.cursor);
    assert_eq!(
        remaining.next_cursor.snapshot_high_water, first_page.high_water,
        "new live traffic cannot extend a historical pagination snapshot"
    );
    let activity = scenario
        .board(
            &swarm.id,
            SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: Some(first_two.next_cursor),
                limit: Some(100),
            },
        )
        .await;
    assert!(
        activity
            .posts
            .iter()
            .map(|post| &post.id)
            .eq([&reply.id, &second_reply.id]),
        "new replies remain discoverable even when their root predates the cursor"
    );
    let first_thread_page = scenario
        .thread(
            &swarm.id,
            SwarmThreadRead {
                thread_id: literal.thread_id.clone(),
                after_cursor: None,
                limit: Some(1),
            },
        )
        .await;
    assert!(first_thread_page.root == literal);
    assert_eq!(first_thread_page.posts.len(), 1);
    assert!(first_thread_page.posts[0].id == reply.id);
    assert!(first_thread_page.has_more);
    let mut late_reply_publication = publication(
        SwarmBoard::Coordination,
        "live-reply-during-pagination",
        vec![text(
            "Thread snapshot must remain finite under new reply traffic",
        )],
    );
    late_reply_publication.thread_id = Some(literal.thread_id.clone());
    let late_reply = scenario.post(&swarm.id, late_reply_publication).await;
    let last_thread_page = scenario
        .thread(
            &swarm.id,
            SwarmThreadRead {
                thread_id: literal.thread_id.clone(),
                after_cursor: Some(first_thread_page.next_cursor.clone()),
                limit: Some(1),
            },
        )
        .await;
    assert!(
        last_thread_page.root == literal,
        "every thread page must retain root context"
    );
    assert_eq!(last_thread_page.posts.len(), 1);
    assert!(last_thread_page.posts[0].id == second_reply.id);
    assert!(!last_thread_page.has_more);
    assert_eq!(last_thread_page.high_water, second_reply.cursor);
    assert_eq!(
        last_thread_page.next_cursor.snapshot_high_water,
        first_thread_page.high_water
    );
    let fresh_thread = scenario
        .thread(
            &swarm.id,
            SwarmThreadRead {
                thread_id: literal.thread_id.clone(),
                after_cursor: None,
                limit: Some(100),
            },
        )
        .await;
    assert!(
        fresh_thread.posts.iter().map(|post| &post.id).eq([
            &reply.id,
            &second_reply.id,
            &late_reply.id
        ]),
        "fresh snapshots must include activity excluded from earlier historical pages"
    );
    scenario
        .send(SwarmCommandPayload::ReadPost {
            swarm_id: swarm.id.clone(),
            post_id: late_reply.id.clone(),
        })
        .await;
    let linked: SwarmThreadNotifyPayload = scenario
        .wait(
            FrameKind::SwarmThreadNotify,
            "typed reply deep link",
            |event: &SwarmThreadNotifyPayload| {
                event.page.swarm_id == swarm.id
                    && event.page.posts.iter().any(|post| post.id == late_reply.id)
            },
        )
        .await;
    assert!(
        linked.page.root.id == literal.id,
        "post deep links must resolve their actual root thread server-side"
    );
    let briefing = scenario
        .board(&swarm.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        briefing.posts.len(),
        3,
        "boards must not leak posts or replies across scope"
    );
    assert!(
        briefing
            .posts
            .iter()
            .all(|post| post.board == SwarmBoard::Briefing)
    );
    let mut invalid_position = first_page.next_cursor.clone();
    invalid_position.position = first_page.high_water + 1;
    let mut future_snapshot = first_page.next_cursor.clone();
    future_snapshot.snapshot_high_water = late_reply.cursor + 1;
    for query in [
        SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: None,
            limit: Some(0),
        },
        SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: None,
            limit: Some(101),
        },
        SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: Some(invalid_position),
            limit: None,
        },
        SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: Some(future_snapshot),
            limit: None,
        },
    ] {
        scenario
            .send(SwarmCommandPayload::ReadBoard {
                swarm_id: swarm.id.clone(),
                query,
            })
            .await;
        scenario.error(SwarmErrorCode::Invalid).await;
    }
    for query in [
        SwarmBoardRead {
            board: SwarmBoard::Briefing,
            after_cursor: Some(first_page.next_cursor.clone()),
            limit: None,
        },
        SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: Some(first_thread_page.next_cursor.clone()),
            limit: None,
        },
    ] {
        scenario
            .send(SwarmCommandPayload::ReadBoard {
                swarm_id: swarm.id.clone(),
                query,
            })
            .await;
        scenario.error(SwarmErrorCode::Unauthorized).await;
    }
    scenario
        .send(SwarmCommandPayload::ReadThread {
            swarm_id: swarm.id.clone(),
            query: SwarmThreadRead {
                thread_id: typed.thread_id.clone(),
                after_cursor: Some(first_thread_page.next_cursor),
                limit: None,
            },
        })
        .await;
    scenario.error(SwarmErrorCode::Unauthorized).await;
    let before_read = scenario.snapshot(&swarm.id).await;
    scenario
        .send(SwarmCommandPayload::MarkRead {
            swarm_id: swarm.id.clone(),
            board: SwarmBoard::Coordination,
            cursor: late_reply.cursor,
        })
        .await;
    let marked = scenario
        .swarm(&swarm.id, |state| {
            state.board_positions.iter().any(|position| {
                position.board == SwarmBoard::Coordination
                    && position.human_read_cursor == late_reply.cursor
            })
        })
        .await;
    let coord_position = marked
        .board_positions
        .iter()
        .find(|position| position.board == SwarmBoard::Coordination)
        .expect("Coordination position");
    assert_eq!(coord_position.unread_count, 0);
    assert!(
        marked.notifications == before_read.notifications,
        "human read position must not acknowledge member delivery"
    );
    assert!(
        marked.members == before_read.members,
        "board reads must not activate members or advance their context cursors"
    );
    for control in &mock_controls {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "paused posts and historical reads must not wake anyone"
        );
        assert!(
            control.violations().await.is_empty(),
            "server must not violate backend dispatch boundaries"
        );
    }

    let bootstrap = scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let persisted = bootstrap
        .swarms
        .iter()
        .find(|stored| stored.id == swarm.id)
        .expect("swarm must survive host restart");
    assert_eq!(persisted.lifecycle, SwarmLifecycle::Paused);
    assert!(
        persisted.notifications == marked.notifications,
        "paused notification intents must survive restart"
    );
    let durable = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(durable.posts.iter().map(|post| &post.id).eq([
        &literal.id,
        &typed.id,
        &reply.id,
        &second_reply.id,
        &late_root.id,
        &late_reply.id,
    ]));
    let retried_after_restart = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Coordination,
                "typed-reference",
                typed.body.clone(),
            ),
        )
        .await;
    assert!(
        retried_after_restart == typed,
        "publication identity must remain durable across restart"
    );
    let reread = scenario.snapshot(&swarm.id).await;
    assert!(
        reread.notifications == marked.notifications,
        "restart retry cannot duplicate recipients"
    );

    let foreign = scenario.launched(1).await;
    let foreign_opening = foreign
        .opening_post_id
        .as_ref()
        .expect("foreign swarm opening post");
    scenario
        .send(SwarmCommandPayload::ReadBoard {
            swarm_id: foreign.id.clone(),
            query: SwarmBoardRead {
                board: SwarmBoard::Coordination,
                after_cursor: Some(first_page.next_cursor),
                limit: None,
            },
        })
        .await;
    scenario.error(SwarmErrorCode::Unauthorized).await;
    let foreign_thread = protocol::SwarmThreadId(foreign_opening.0.clone());
    scenario
        .send(SwarmCommandPayload::ReadThread {
            swarm_id: swarm.id.clone(),
            query: SwarmThreadRead {
                thread_id: foreign_thread.clone(),
                after_cursor: None,
                limit: Some(10),
            },
        })
        .await;
    scenario.error(SwarmErrorCode::NotFound).await;
    let mut foreign_reply = publication(
        SwarmBoard::Briefing,
        "foreign-thread-reply",
        vec![text("No cross-swarm replies")],
    );
    foreign_reply.thread_id = Some(foreign_thread);
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: foreign_reply,
        })
        .await;
    scenario.error(SwarmErrorCode::NotFound).await;
    for segment in [
        SwarmBodySegment::MemberMention {
            member_id: foreign.members[0].spec.id.clone(),
        },
        SwarmBodySegment::PostLink {
            post_id: foreign_opening.clone(),
        },
    ] {
        scenario
            .send(SwarmCommandPayload::Post {
                swarm_id: swarm.id.clone(),
                publication: publication(
                    SwarmBoard::Coordination,
                    "foreign-reference",
                    vec![segment],
                ),
            })
            .await;
        scenario.error(SwarmErrorCode::Unauthorized).await;
    }
    let attachment = protocol::SwarmAttachment {
        project_id: scenario.project.id.clone(),
        path: protocol::ProjectPath {
            root: scenario.project.root_paths()[0].clone(),
            relative_path: "reference.txt".to_owned(),
        },
    };
    let mut attached_publication = publication(
        SwarmBoard::Coordination,
        "project-reference",
        vec![text("Authorized project file reference")],
    );
    attached_publication.attachments.push(attachment.clone());
    let attached = scenario.post(&swarm.id, attached_publication.clone()).await;
    assert!(
        attached.attachments == vec![attachment],
        "publication must preserve authorized file references without embedding file content"
    );
    attached_publication.publication_id =
        SwarmPublicationId("outside-project-attachment".to_owned());
    attached_publication.attachments[0].project_id =
        protocol::ProjectId("foreign-project".to_owned());
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: attached_publication.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Unauthorized).await;
    attached_publication.publication_id = SwarmPublicationId("traversal-attachment".to_owned());
    attached_publication.attachments[0].project_id = scenario.project.id.clone();
    attached_publication.attachments[0].path.relative_path = "../sessions.json".to_owned();
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: attached_publication.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Unauthorized).await;
    attached_publication.publication_id = SwarmPublicationId("missing-attachment".to_owned());
    attached_publication.attachments[0].path.relative_path = "missing-file.txt".to_owned();
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: attached_publication,
        })
        .await;
    scenario.error(SwarmErrorCode::NotFound).await;
    let scoped = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert_eq!(
        scoped.posts.len(),
        7,
        "scope errors cannot leave partially persisted posts"
    );
    assert!(
        scoped
            .posts
            .last()
            .is_some_and(|post| post.id == attached.id)
    );

    scenario.pause(&foreign.id).await;
    let before_failure = scenario.snapshot(&swarm.id).await;
    let store_path = scenario.fixture.swarm_store_path();
    let retained_path = store_path.with_file_name("agent_swarms.retained.json");
    let retained_bytes =
        std::fs::read(&store_path).expect("read durable swarm store before blocking publication");
    std::fs::rename(&store_path, &retained_path).expect("retain committed swarm store");
    // A directory at the destination makes the actual atomic file publication
    // fail even when the test process can bypass filesystem permissions.
    std::fs::create_dir(&store_path)
        .expect("block swarm store publication with a real filesystem object");
    let failed_publication = publication(
        SwarmBoard::Coordination,
        "recoverable-store-failure",
        vec![
            text("Publish this only after durable storage recovers"),
            SwarmBodySegment::MemberMention {
                member_id: swarm.members[0].spec.id.clone(),
            },
        ],
    );
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: failed_publication.clone(),
        })
        .await;
    let storage_error = scenario.error(SwarmErrorCode::Storage).await;
    assert!(
        storage_error.swarm_id.as_ref() == Some(&swarm.id)
            && storage_error.publication_id.as_ref() == Some(&failed_publication.publication_id),
        "storage rejection must identify the failed publication for the client"
    );
    let failed_page = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(
        failed_page.posts == scoped.posts && failed_page.high_water == scoped.high_water,
        "failed persistence cannot expose a post or advance board activity"
    );
    let after_failure = scenario.snapshot(&swarm.id).await;
    assert!(
        after_failure == before_failure,
        "failed publication must leave members, notifications, causal rounds and read state unchanged"
    );
    assert!(
        !scenario.pending.iter().any(|envelope| {
            envelope.kind == FrameKind::SwarmPostNotify
                && envelope
                    .parse_payload::<SwarmPostNotifyPayload>()
                    .expect("parse publication event after storage failure")
                    .post
                    .publication_id
                    == failed_publication.publication_id
        }),
        "failed persistence cannot emit a committed post notification"
    );
    assert!(
        std::fs::read(&retained_path).expect("read retained committed swarm store")
            == retained_bytes,
        "failed publication must not modify the last committed filesystem snapshot"
    );
    std::fs::remove_dir(&store_path).expect("remove filesystem publication blocker");
    std::fs::rename(&retained_path, &store_path).expect("restore committed swarm store path");

    let recovered_post = scenario.post(&swarm.id, failed_publication.clone()).await;
    assert_eq!(
        recovered_post.cursor,
        scoped.high_water + 1,
        "failed storage must not consume a durable post cursor"
    );
    let recovered = scenario.snapshot(&swarm.id).await;
    assert_eq!(recovered.lifecycle, SwarmLifecycle::Paused);
    assert_eq!(recovered.rounds.len(), before_failure.rounds.len() + 1);
    assert_eq!(
        recovered.notifications.len(),
        before_failure.notifications.len() + 1
    );
    let recovered_intents = recovered
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&recovered_post.id))
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_intents.len(),
        1,
        "recovered publication must create exactly one typed mentioned-recipient intent"
    );
    assert!(
        recovered_intents[0].member_id == swarm.members[0].spec.id
            && recovered_intents[0].round_id == recovered_post.round_id
    );
    assert_eq!(recovered_intents[0].state, SwarmDeliveryState::Pending);
    let recovery_retry = scenario.post(&swarm.id, failed_publication.clone()).await;
    assert!(
        recovery_retry == recovered_post,
        "storage recovery retry must preserve committed publication identity"
    );
    let recovery_reconnect = scenario.snapshot(&swarm.id).await;
    assert!(
        recovery_reconnect == recovered,
        "retry after storage recovery cannot duplicate durable work"
    );
    scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let restarted_recovery = scenario.snapshot(&swarm.id).await;
    assert_eq!(restarted_recovery.lifecycle, SwarmLifecycle::Paused);
    assert!(
        restarted_recovery.notifications == recovered.notifications
            && restarted_recovery.rounds == recovered.rounds,
        "recovered posts and their exact intent/cause identities must survive host restart"
    );
    let recovered_page = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert_eq!(recovered_page.posts.len(), 8);
    assert!(
        recovered_page.posts[..7] == scoped.posts && recovered_page.posts[7] == recovered_post,
        "filesystem recovery must retain all earlier board activity and persist the retried post once"
    );
    let restart_retry = scenario.post(&swarm.id, failed_publication).await;
    assert!(
        restart_retry == recovered_post,
        "publication retry must remain idempotent after repaired-store restart"
    );
    assert!(scenario.snapshot(&swarm.id).await.notifications == recovered.notifications);

    let empty_body_bytes = serde_json::to_vec(&vec![text("")])
        .expect("measure canonical body envelope")
        .len();
    let text_budget = protocol::SWARM_MAX_BODY_BYTES - empty_body_bytes;
    let mut escaped_text = "\\".repeat(text_budget / 2);
    if !text_budget.is_multiple_of(2) {
        escaped_text.push('x');
    }
    let large_body = vec![text(&escaped_text)];
    assert_eq!(
        serde_json::to_vec(&large_body)
            .expect("measure maximum valid escaped body")
            .len(),
        protocol::SWARM_MAX_BODY_BYTES
    );
    let large_root = scenario
        .post(
            &foreign.id,
            publication(
                SwarmBoard::Coordination,
                "large-page-root",
                large_body.clone(),
            ),
        )
        .await;
    let mut large_posts = vec![large_root.clone()];
    for index in 1..protocol::SWARM_MAX_PAGE_LIMIT {
        let mut reply = publication(
            SwarmBoard::Coordination,
            &format!("large-page-reply-{index}"),
            large_body.clone(),
        );
        reply.thread_id = Some(large_root.thread_id.clone());
        large_posts.push(scenario.post(&foreign.id, reply).await);
    }
    let high_water = large_posts
        .last()
        .expect("maximum page has committed activity")
        .cursor;
    let mut board_queries = Vec::new();
    let mut board_pages = Vec::new();
    let mut board_cursor = None;
    loop {
        let query = SwarmBoardRead {
            board: SwarmBoard::Coordination,
            after_cursor: board_cursor,
            limit: Some(protocol::SWARM_MAX_PAGE_LIMIT),
        };
        let page = scenario.board(&foreign.id, query.clone()).await;
        assert_eq!(page.high_water, high_water);
        assert!(
            !page.posts.is_empty(),
            "bounded nonempty board continuation must advance"
        );
        assert!(
            serde_json::to_vec(&SwarmBoardNotifyPayload { page: page.clone() })
                .expect("measure bounded board response")
                .len()
                <= protocol::SWARM_MAX_READ_PAGE_BYTES,
            "serialized board page must respect the canonical byte bound"
        );
        let more = page.has_more;
        board_cursor = Some(page.next_cursor.clone());
        board_queries.push(query);
        board_pages.push(page);
        if !more {
            break;
        }
    }
    let mut thread_queries = Vec::new();
    let mut thread_pages = Vec::new();
    let mut thread_cursor = None;
    loop {
        let query = SwarmThreadRead {
            thread_id: large_root.thread_id.clone(),
            after_cursor: thread_cursor,
            limit: Some(protocol::SWARM_MAX_PAGE_LIMIT),
        };
        let page = scenario.thread(&foreign.id, query.clone()).await;
        assert_eq!(page.high_water, high_water);
        assert!(
            page.root == large_root && !page.posts.is_empty(),
            "bounded thread continuation must retain its root and advance"
        );
        assert!(
            serde_json::to_vec(&SwarmThreadNotifyPayload { page: page.clone() })
                .expect("measure bounded thread response")
                .len()
                <= protocol::SWARM_MAX_READ_PAGE_BYTES,
            "serialized thread page must include its root within the byte bound"
        );
        let more = page.has_more;
        thread_cursor = Some(page.next_cursor.clone());
        thread_queries.push(query);
        thread_pages.push(page);
        if !more {
            break;
        }
    }
    assert!(
        board_pages[0].has_more
            && board_pages[0].posts.len() < protocol::SWARM_MAX_PAGE_LIMIT as usize
            && thread_pages[0].has_more
            && thread_pages[0].posts.len() < protocol::SWARM_MAX_PAGE_LIMIT as usize,
        "byte-bounded pages must use authoritative continuation rather than pretending the requested count always fits"
    );
    assert!(
        board_pages
            .iter()
            .flat_map(|page| page.posts.iter())
            .eq(large_posts.iter())
            && thread_pages
                .iter()
                .flat_map(|page| page.posts.iter())
                .eq(large_posts[1..].iter()),
        "all authoritative continuation pages must retain the exact ordered post set without duplicates or omissions"
    );
    let queued_page_bytes = board_pages
        .iter()
        .map(|page| {
            serde_json::to_vec(&SwarmBoardNotifyPayload { page: page.clone() })
                .expect("measure board response for transport load")
                .len()
        })
        .sum::<usize>()
        + thread_pages
            .iter()
            .map(|page| {
                serde_json::to_vec(&SwarmThreadNotifyPayload { page: page.clone() })
                    .expect("measure thread response for transport load")
                    .len()
            })
            .sum::<usize>();
    assert!(
        queued_page_bytes > 8 * 1024 * 1024 && queued_page_bytes < 32 * 1024 * 1024,
        "the actual queued page set must exercise bulk capacity beyond the chat byte budget"
    );
    let (mut slow, release_output) = scenario.fixture.connect_with_paused_output().await;
    for query in board_queries {
        slow.swarm_command(SwarmCommandPayload::ReadBoard {
            swarm_id: foreign.id.clone(),
            query,
        })
        .await
        .expect("queue bounded board pages behind real writer backpressure");
    }
    for query in thread_queries {
        slow.swarm_command(SwarmCommandPayload::ReadThread {
            swarm_id: foreign.id.clone(),
            query,
        })
        .await
        .expect("queue bounded thread pages behind real writer backpressure");
    }
    // The live client observes this final command only after the paused
    // connection has queued every preceding read, without a timing sleep.
    slow.swarm_command(SwarmCommandPayload::MarkRead {
        swarm_id: foreign.id.clone(),
        board: SwarmBoard::Coordination,
        cursor: high_water,
    })
    .await
    .expect("place a protocol barrier after all queued bounded pages");
    let bulk_barrier = scenario
        .swarm(&foreign.id, |state| {
            state.board_positions.iter().any(|position| {
                position.board == SwarmBoard::Coordination
                    && position.human_read_cursor == high_water
            })
        })
        .await;
    assert_eq!(bulk_barrier.lifecycle, SwarmLifecycle::Paused);
    assert!(
        !scenario.pending.iter().any(|event| matches!(
            event.kind,
            FrameKind::SwarmBoardNotify
                | FrameKind::SwarmThreadNotify
                | FrameKind::SwarmImageNotify
                | FrameKind::SwarmErrorNotify
        )),
        "the live observer sees shared MarkRead, never the paused client's private bulk pages or errors"
    );
    let prior_client = std::mem::replace(&mut scenario.fixture.client, slow);
    drop(prior_client);
    scenario.pending.clear();
    release_output
        .send(())
        .expect("large page connection must remain alive until backpressure releases");
    for expected in board_pages {
        let event: SwarmBoardNotifyPayload = scenario
            .wait(
                FrameKind::SwarmBoardNotify,
                "bounded board page on slow connection",
                |event: &SwarmBoardNotifyPayload| {
                    event.page.swarm_id == foreign.id
                        && event.page.next_cursor == expected.next_cursor
                },
            )
            .await;
        assert!(
            event.page == expected,
            "backpressured board pages must retain exact committed content and scoped cursors"
        );
    }
    for expected in thread_pages {
        let event: SwarmThreadNotifyPayload = scenario
            .wait(
                FrameKind::SwarmThreadNotify,
                "bounded thread page on slow connection",
                |event: &SwarmThreadNotifyPayload| {
                    event.page.swarm_id == foreign.id
                        && event.page.next_cursor == expected.next_cursor
                },
            )
            .await;
        assert!(
            event.page == expected,
            "backpressured thread pages must retain exact committed content and scoped cursors"
        );
    }
    let still_connected = scenario
        .board(
            &foreign.id,
            SwarmBoardRead {
                board: SwarmBoard::Briefing,
                after_cursor: None,
                limit: Some(1),
            },
        )
        .await;
    assert_eq!(
        still_connected.posts.len(),
        1,
        "the same sequenced protocol connection must still accept commands after large page delivery"
    );
    assert!(
        scenario.snapshot(&foreign.id).await.notifications == bulk_barrier.notifications,
        "large shared-history reads cannot create or acknowledge backend delivery intents"
    );

    let before_inline = scenario.snapshot(&foreign.id).await;
    assert!(before_inline.members[0].context_cursor < large_root.cursor);
    let wake = scenario.post(&foreign.id, publication(SwarmBoard::Coordination, "large-history-wake", vec![
        text("Read the referenced notification and any omitted shared history through the board tools"),
        SwarmBodySegment::MemberMention { member_id: foreign.members[0].spec.id.clone() },
    ])).await;
    let inline_gate = MockGateHandle::new();
    let inline_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &foreign.members[0].spec.name,
            MockScript::one(MockTurn::gated_text(
                "Bounded large-history wake",
                &inline_gate,
            )),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: foreign.id.clone(),
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), inline_gate.wait_until_entered())
        .await
        .expect("resumed large-history wake must reach the actual mock backend");
    let inline_accepted = scenario
        .swarm(&foreign.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&wake.id) && intent.state == SwarmDeliveryState::Accepted
            }) && state.members[0].runtime_status == Some(AgentControlStatus::Thinking)
        })
        .await;
    let inline_control = scenario
        .fixture
        .mock_by_id(
            inline_accepted.members[0]
                .agent_id
                .as_ref()
                .expect("bounded wake must own a real resumed runtime"),
        )
        .await;
    let inline_requests = inline_control.requests().await;
    assert_eq!(request_count(&inline_requests), 1);
    let (references, inline_posts) = last_dispatch_context(&inline_requests);
    assert!(
        references == vec![wake.id.clone()],
        "every required notification reference must survive even when its body is outside the inline prefix"
    );
    assert!(
        !inline_posts.is_empty()
            && inline_posts.len() < large_posts.len()
            && inline_posts
                .iter()
                .eq(large_posts[..inline_posts.len()].iter()),
        "native delivery must contain only a whole chronological prefix, never split or skip an oversized next post"
    );
    let mut over_budget = inline_posts.clone();
    over_budget.push(large_posts[inline_posts.len()].clone());
    assert!(
        serde_json::to_vec(&over_budget)
            .expect("measure first omitted complete post")
            .len()
            > protocol::SWARM_MAX_INLINE_CONTEXT_BYTES,
        "the first omitted full post must exceed the remaining inline byte budget"
    );
    let accepted_cursor = inline_posts
        .last()
        .expect("accepted full inline prefix")
        .cursor;
    assert_eq!(inline_accepted.members[0].context_cursor, accepted_cursor);
    assert!(
        accepted_cursor < high_water && accepted_cursor < wake.cursor,
        "accepting a notification reference cannot falsely mark its omitted body or later history delivered"
    );
    assert!(
        inline_accepted.members[0].session_id == before_inline.members[0].session_id,
        "byte-bounded wake must still resume the retained native session"
    );
    let caller = scenario
        .fixture
        .agent_control_caller(
            inline_accepted.members[0]
                .agent_id
                .as_ref()
                .expect("authenticated bounded-wake caller"),
        )
        .await;
    let omitted_history: SwarmThreadPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_thread",
            serde_json::to_value(SwarmThreadRead {
                thread_id: large_root.thread_id.clone(),
                after_cursor: None,
                limit: Some(protocol::SWARM_MAX_PAGE_LIMIT),
            })
            .expect("serialize omitted-history tool query"),
        )
        .await,
    );
    assert!(
        omitted_history.root == large_root
            && omitted_history
                .posts
                .iter()
                .any(|post| post.cursor > accepted_cursor
                    && large_posts.iter().any(|stored| stored == post)),
        "authenticated paginated tools must retain full bodies omitted from native inline context"
    );
    let omitted_notification: SwarmThreadPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_thread",
            serde_json::to_value(SwarmThreadRead {
                thread_id: wake.thread_id.clone(),
                after_cursor: None,
                limit: Some(1),
            })
            .expect("serialize required-notification tool query"),
        )
        .await,
    );
    assert!(
        omitted_notification.root == wake && omitted_notification.posts.is_empty(),
        "the required notification body absent from inline input must remain readable by its authenticated tool reference"
    );
    let after_tools = scenario.snapshot(&foreign.id).await;
    assert_eq!(after_tools.members[0].context_cursor, accepted_cursor);
    assert!(
        after_tools.notifications == inline_accepted.notifications,
        "tool reads cannot pretend omitted bodies were accepted by native delivery"
    );
    assert_eq!(request_count(&inline_control.requests().await), 1);
    inline_gate.release_one();
    scenario.swarm(&foreign.id, ready).await;
    scenario.pause(&foreign.id).await;
    assert!(inline_control.violations().await.is_empty());
    drop(inline_reservation);

    let oversized_swarm = scenario.launched(1).await;
    let oversized_control = controls(&scenario, &oversized_swarm).await.remove(0);
    let before_oversized = scenario.pause(&oversized_swarm.id).await;
    let mut relative_file = std::path::PathBuf::new();
    // Escape-heavy real paths amplify serialized attachment bytes without
    // exceeding native filename or path-length limits.
    for _ in 0..4 {
        relative_file.push("\u{0001}".repeat(180));
    }
    relative_file.push("reference.txt");
    let absolute_file =
        std::path::Path::new(&scenario.project.root_paths()[0].0).join(&relative_file);
    std::fs::create_dir_all(
        absolute_file
            .parent()
            .expect("escaped attachment has a parent"),
    )
    .expect("create real in-scope escaped attachment directories");
    std::fs::write(&absolute_file, "Authorized attachment reference\n")
        .expect("create real readable in-scope attachment");
    let mut oversized_publication = publication(
        SwarmBoard::Coordination,
        "oversized-inline-post",
        large_body,
    );
    oversized_publication.attachments = vec![
        protocol::SwarmAttachment {
            project_id: scenario.project.id.clone(),
            path: protocol::ProjectPath {
                root: scenario.project.root_paths()[0].clone(),
                relative_path: relative_file.to_string_lossy().into_owned(),
            },
        };
        protocol::SWARM_MAX_ATTACHMENTS
    ];
    let oversized_post = scenario
        .post(&oversized_swarm.id, oversized_publication)
        .await;
    let oversized_bytes = serde_json::to_vec(&oversized_post)
        .expect("measure committed attachment-heavy full post")
        .len();
    assert!(
        oversized_bytes > protocol::SWARM_MAX_INLINE_CONTEXT_BYTES
            && oversized_bytes <= protocol::SWARM_MAX_POST_BYTES,
        "a valid readable full post must exceed the inline budget without exceeding the durable or page bounds"
    );
    let oversized_wake = scenario.post(&oversized_swarm.id, publication(SwarmBoard::Coordination, "wake-behind-oversized-post", vec![
        text("Read the required notification through tools despite the preceding oversized shared post"),
        SwarmBodySegment::MemberMention { member_id: oversized_swarm.members[0].spec.id.clone() },
    ])).await;
    let oversized_gate = MockGateHandle::new();
    oversized_control
        .enqueue(MockTurn::gated_text(
            "Empty inline prefix remains truthful",
            &oversized_gate,
        ))
        .await;
    let before_oversized_count = request_count(&oversized_control.requests().await);
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: oversized_swarm.id.clone(),
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), oversized_gate.wait_until_entered())
        .await
        .expect("attachment-heavy wake must reach the actual mock input boundary");
    let oversized_accepted = scenario
        .swarm(&oversized_swarm.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&oversized_wake.id)
                    && intent.state == SwarmDeliveryState::Accepted
            }) && state.members[0].runtime_status == Some(AgentControlStatus::Thinking)
        })
        .await;
    let oversized_requests = oversized_control.requests().await;
    assert_eq!(
        request_count(&oversized_requests),
        before_oversized_count + 1
    );
    let (references, inline_posts) = last_dispatch_context(&oversized_requests);
    assert!(
        references == vec![oversized_wake.id.clone()] && inline_posts.is_empty(),
        "an oversized first unread post must yield an empty inline prefix, never a first-post exception or skipped-middle delivery"
    );
    assert_eq!(
        oversized_accepted.members[0].context_cursor, before_oversized.members[0].context_cursor,
        "accepted references with no full inline bodies cannot advance the delivery cursor"
    );
    let caller = scenario
        .fixture
        .agent_control_caller(
            oversized_accepted.members[0]
                .agent_id
                .as_ref()
                .expect("attachment-heavy authenticated caller"),
        )
        .await;
    for expected in [&oversized_post, &oversized_wake] {
        let readable: SwarmThreadPage = tool_value(
            &call_tool(
                &caller,
                "tyde_swarm_read_thread",
                serde_json::to_value(SwarmThreadRead {
                    thread_id: expected.thread_id.clone(),
                    after_cursor: None,
                    limit: Some(1),
                })
                .expect("serialize omitted full-post tool query"),
            )
            .await,
        );
        assert!(
            readable.root == *expected && readable.posts.is_empty(),
            "authenticated tools must return the entire durable omitted post including body and attachment references"
        );
        assert!(
            serde_json::to_vec(&readable)
                .expect("measure omitted-post tool response")
                .len()
                <= protocol::SWARM_MAX_READ_PAGE_BYTES
        );
    }
    assert_eq!(
        scenario.snapshot(&oversized_swarm.id).await.members[0].context_cursor,
        before_oversized.members[0].context_cursor,
        "reading an oversized post through tools must not imply native inline acceptance"
    );
    assert_eq!(
        request_count(&oversized_control.requests().await),
        before_oversized_count + 1
    );
    oversized_gate.release_one();
    scenario.swarm(&oversized_swarm.id, ready).await;
    assert!(oversized_control.violations().await.is_empty());
    scenario.pause(&oversized_swarm.id).await;
    let mut boundary_posts: Vec<SwarmPost> = Vec::new();
    for key in ["exact-max-root", "exact-max-reply"] {
        let mut measured = oversized_post.clone();
        measured.cursor = oversized_wake.cursor + boundary_posts.len() as u64 + 1;
        measured.publication_id = SwarmPublicationId(key.to_owned());
        measured.body = vec![text("Authorized boundary post")];
        measured.attachments = vec![protocol::SwarmAttachment {
            project_id: scenario.project.id.clone(),
            path: protocol::ProjectPath {
                root: scenario.project.root_paths()[0].clone(),
                relative_path: "reference.txt".to_owned(),
            },
        }];
        let mut padding = protocol::SWARM_MAX_POST_BYTES
            - serde_json::to_vec(&measured)
                .expect("measure boundary post metadata")
                .len();
        if !padding.is_multiple_of(2) {
            let SwarmBodySegment::Text { text } = &mut measured.body[0] else {
                panic!("boundary body must be text");
            };
            text.push('x');
            padding -= 1;
        }
        // CurDir components are explicitly valid project paths. canonicalize
        // resolves this alias to the real authorized file before metadata; no
        // invented attachment, over-budget body, or filesystem limit bypass.
        measured.attachments[0].path.relative_path =
            format!("{}reference.txt", "./".repeat(padding / 2));
        assert_eq!(
            serde_json::to_vec(&measured)
                .expect("measure exact boundary publication")
                .len(),
            protocol::SWARM_MAX_POST_BYTES
        );
        assert!(
            serde_json::to_vec(&measured.body)
                .expect("measure boundary body")
                .len()
                <= protocol::SWARM_MAX_BODY_BYTES
        );
        let mut request = publication(SwarmBoard::Coordination, key, measured.body.clone());
        request.attachments = measured.attachments.clone();
        request.thread_id = boundary_posts.first().map(|root| root.thread_id.clone());
        let accepted = scenario.post(&oversized_swarm.id, request).await;
        assert_eq!(
            serde_json::to_vec(&accepted)
                .expect("measure actual accepted boundary post")
                .len(),
            protocol::SWARM_MAX_POST_BYTES,
            "the regression requires actual server-accepted maximum-sized posts, not approximations"
        );
        assert!(accepted.attachments == measured.attachments && accepted.body == measured.body);
        boundary_posts.push(accepted);
    }
    let root = &boundary_posts[0];
    let mut cursor = None;
    let mut read_replies = Vec::new();
    loop {
        let page = scenario
            .thread(
                &oversized_swarm.id,
                SwarmThreadRead {
                    thread_id: root.thread_id.clone(),
                    after_cursor: cursor.clone(),
                    limit: Some(protocol::SWARM_MAX_PAGE_LIMIT),
                },
            )
            .await;
        assert!(
            page.root == *root,
            "every maximum-sized thread page must preserve the complete actual root"
        );
        assert!(
            serde_json::to_vec(&page)
                .expect("measure accepted-boundary thread page")
                .len()
                <= protocol::SWARM_MAX_READ_PAGE_BYTES
        );
        let has_more = page.has_more;
        let next = page.next_cursor;
        assert!(
            !has_more || (!page.posts.is_empty() && cursor.as_ref() != Some(&next)),
            "authoritative continuation must make progress without dropping a maximum-sized reply"
        );
        read_replies.extend(page.posts);
        if !has_more {
            break;
        }
        cursor = Some(next);
    }
    assert!(
        read_replies == boundary_posts[1..],
        "an accepted exact-maximum root and reply must be exhaustively readable, without separator-budget failure or loss"
    );
}

#[tokio::test]
async fn pause_interrupts_active_turns_and_restart_resume_reuses_sessions_and_pending_context() {
    let mut scenario = Scenario::new().await;
    let draft = scenario.generate(scenario.constraints(2)).await;
    let scripts = draft
        .members
        .iter()
        .map(|member| {
            (
                member.name.clone(),
                MockScript::one(MockTurn::held_text("Private member launch output")),
            )
        })
        .collect();
    let launch_reservation = scenario.fixture.reserve_mock_launches(scripts).await;
    let starting = scenario.launch(&draft).await;
    let busy = scenario
        .swarm(&starting.id, |swarm| {
            swarm.members.iter().all(|member| {
                member.state == SwarmMemberState::Live
                    && member.runtime_status == Some(AgentControlStatus::Thinking)
                    && member.session_id.is_some()
            })
        })
        .await;
    drop(launch_reservation);
    let original_controls = controls(&scenario, &busy).await;
    scenario
        .send(SwarmCommandPayload::Pause {
            swarm_id: busy.id.clone(),
        })
        .await;
    scenario
        .swarm(&busy.id, |swarm| swarm.lifecycle == SwarmLifecycle::Pausing)
        .await;
    let paused = scenario
        .swarm(&busy.id, |swarm| swarm.lifecycle == SwarmLifecycle::Paused)
        .await;
    assert!(
        paused
            .members
            .iter()
            .all(|member| member.runtime_status == Some(AgentControlStatus::Idle)),
        "Paused cannot be published while cancellation remains unconfirmed"
    );
    for control in &original_controls {
        let requests = control.requests().await;
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, MockRequest::Interrupt)),
            "pausing must request real cancellation through the backend boundary"
        );
        assert_eq!(request_count(&requests), 1);
        assert!(control.violations().await.is_empty());
    }
    let first = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Briefing,
                "paused-context-one",
                vec![text("First pending user context")],
            ),
        )
        .await;
    let second = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Briefing,
                "paused-context-two",
                vec![text("Second pending user context")],
            ),
        )
        .await;
    assert!(
        first.round_id != second.round_id,
        "distinct human publications establish distinct causal rounds"
    );
    let persisted = scenario.snapshot(&busy.id).await;
    assert_eq!(persisted.lifecycle, SwarmLifecycle::Paused);
    assert_eq!(
        persisted
            .notifications
            .iter()
            .filter(|notification| {
                notification.post_ids.contains(&first.id)
                    || notification.post_ids.contains(&second.id)
            })
            .count(),
        4,
        "each paused publication must persist its recipient intents"
    );
    assert!(
        persisted
            .notifications
            .iter()
            .filter(|notification| {
                notification.post_ids.contains(&first.id)
                    || notification.post_ids.contains(&second.id)
            })
            .all(|notification| notification.state == SwarmDeliveryState::Pending)
    );
    for control in &original_controls {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "paused posts cannot dispatch"
        );
    }

    let bootstrap = scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let recovered = bootstrap
        .swarms
        .iter()
        .find(|swarm| swarm.id == busy.id)
        .expect("restarted swarm snapshot");
    assert_eq!(recovered.lifecycle, SwarmLifecycle::Paused);
    assert!(
        recovered.notifications == persisted.notifications,
        "restart must preserve pending versus accepted transport states"
    );
    for old_member in &persisted.members {
        let member = recovered
            .members
            .iter()
            .find(|member| member.spec.id == old_member.spec.id)
            .expect("stable member identity on restart");
        assert!(
            member.session_id == old_member.session_id,
            "restart must retain resumable session identity"
        );
        assert!(
            member.agent_id.is_none(),
            "paused recovery must not invent a live member binding"
        );
    }
    let first_round_gates = [MockGateHandle::new(), MockGateHandle::new()];
    let second_round_gates = [MockGateHandle::new(), MockGateHandle::new()];
    let scripts = recovered
        .members
        .iter()
        .zip(&first_round_gates)
        .zip(&second_round_gates)
        .map(|((member, first_gate), second_gate)| {
            (
                member.spec.name.clone(),
                MockScript::one(MockTurn::gated_text(
                    "First resumed causal round",
                    first_gate,
                ))
                .then(MockTurn::gated_text(
                    "Second resumed causal round",
                    second_gate,
                )),
            )
        })
        .collect();
    let resume_reservation = scenario.fixture.reserve_mock_launches(scripts).await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: busy.id.clone(),
        })
        .await;
    for gate in &first_round_gates {
        scenario
            .wait_mock_turn(gate, "restart-first-human-round")
            .await;
    }
    let first_accepted = scenario
        .swarm(&busy.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
                && state
                    .notifications
                    .iter()
                    .filter(|notification| notification.post_ids.contains(&first.id))
                    .all(|notification| notification.state == SwarmDeliveryState::Accepted)
                && state
                    .notifications
                    .iter()
                    .filter(|notification| notification.post_ids.contains(&second.id))
                    .all(|notification| notification.state == SwarmDeliveryState::Pending)
                && state.members.iter().all(|member| {
                    member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Thinking)
                        && member.current_round_id.as_ref() == Some(&first.round_id)
                })
        })
        .await;
    let resumed_controls = controls(&scenario, &first_accepted).await;
    for control in &resumed_controls {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "first resumed turn cannot silently absorb another human round's notification"
        );
    }
    for gate in &first_round_gates {
        gate.release_one();
    }
    for gate in &second_round_gates {
        scenario
            .wait_mock_turn(gate, "restart-second-human-round")
            .await;
    }
    let accepted = scenario
        .swarm(&busy.id, |state| {
            state
                .notifications
                .iter()
                .filter(|notification| {
                    notification.post_ids.contains(&first.id)
                        || notification.post_ids.contains(&second.id)
                })
                .all(|notification| notification.state == SwarmDeliveryState::Accepted)
                && state.members.iter().all(|member| {
                    member.runtime_status == Some(AgentControlStatus::Thinking)
                        && member.current_round_id.as_ref() == Some(&second.round_id)
                })
        })
        .await;
    for original_intent in persisted.notifications.iter().filter(|notification| {
        notification.post_ids.contains(&first.id) || notification.post_ids.contains(&second.id)
    }) {
        assert!(
            accepted
                .notifications
                .iter()
                .any(|notification| notification.id == original_intent.id
                    && notification.state == SwarmDeliveryState::Accepted),
            "separate causal-round delivery must retain and accept every original notification intent"
        );
    }
    for (member, control) in accepted.members.iter().zip(&resumed_controls) {
        let prior = persisted
            .members
            .iter()
            .find(|old| old.spec.id == member.spec.id)
            .expect("prior session binding");
        assert!(
            member.session_id == prior.session_id,
            "Resume must reuse sessions, not recreate the roster"
        );
        assert!(
            member.agent_id != prior.agent_id,
            "restarted activations must expose new runtime bindings"
        );
        let requests = control.requests().await;
        assert_eq!(
            request_count(&requests),
            2,
            "different human causal rounds must be delivered in separate turns with a singular round identity"
        );
        for post_id in [&first.id, &second.id] {
            assert!(
                requests.iter().any(|request| match request {
                    MockRequest::Input(input) | MockRequest::Steer(input) =>
                        input.message.contains(&post_id.0),
                    MockRequest::Launch { message } => message.contains(&post_id.0),
                    _ => false,
                }),
                "resumed deliveries must retain every underlying post reference"
            );
        }
        assert!(
            member.context_cursor >= second.cursor,
            "accepted context must advance the server-owned delivery cursor"
        );
    }
    for gate in &second_round_gates {
        gate.release_one();
    }
    let idle = scenario
        .swarm(&busy.id, |state| {
            ready(state)
                && state
                    .members
                    .iter()
                    .all(|member| member.context_cursor >= second.cursor)
        })
        .await;
    drop(resume_reservation);
    for control in &resumed_controls {
        assert!(control.violations().await.is_empty());
    }
    let board = scenario
        .board(&busy.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        board.posts.len(),
        3,
        "resume must not republish the brief or turn private output into shared posts"
    );
    assert!(board.posts.iter().map(|post| &post.id).eq([
        busy.opening_post_id.as_ref().expect("original brief"),
        &first.id,
        &second.id,
    ]));
    let reconnect = scenario.snapshot(&busy.id).await;
    assert!(
        reconnect.members == idle.members,
        "reconnect must replay actual resumed runtime state"
    );

    let admission_gate = scenario.fixture.install_swarm_admission_test_gate().await;
    let before_admission = request_count(&resumed_controls[0].requests().await);
    let admission_post = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Coordination,
                "pause-before-final-handoff",
                vec![
                    text("Pause must win over this prepared but unadmitted turn"),
                    SwarmBodySegment::MemberMention {
                        member_id: idle.members[0].spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), admission_gate.wait_until_entered())
        .await
        .expect("dispatch must reach final checked handoff outside the lifecycle lock");
    let reserved = scenario
        .swarm(&busy.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&admission_post.id)
                    && intent.state == SwarmDeliveryState::Dispatching
            })
        })
        .await;
    let reserved_intent = reserved
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&admission_post.id))
        .expect("durable reserved delivery")
        .clone();
    assert_eq!(reserved.members[0].state, SwarmMemberState::Reserved);
    let (mut lifecycle_client, _) = scenario.fixture.connect_with_bootstrap().await;
    lifecycle_client
        .swarm_command(SwarmCommandPayload::Pause {
            swarm_id: busy.id.clone(),
        })
        .await
        .expect("pause from a second real client while final admission is gated");
    // Reserved dispatch is still unresolved, so Pause truthfully commits
    // Pausing rather than claiming cancellation is already complete.
    scenario
        .swarm(&busy.id, |state| state.lifecycle == SwarmLifecycle::Pausing)
        .await;
    assert_eq!(
        request_count(&resumed_controls[0].requests().await),
        before_admission,
        "Pause must commit before any native input crosses the gated handoff"
    );
    admission_gate.release_one();
    drop(admission_gate);
    let deferred = scenario
        .swarm(&busy.id, |state| {
            state.lifecycle == SwarmLifecycle::Paused
                && state.notifications.iter().any(|intent| {
                    intent.id == reserved_intent.id && intent.state == SwarmDeliveryState::Pending
                })
        })
        .await;
    assert!(
        deferred.members[0].agent_id == idle.members[0].agent_id
            && deferred.members[0].session_id == idle.members[0].session_id
            && deferred.members[0].context_cursor == idle.members[0].context_cursor
            && deferred.members[0].current_round_id == idle.members[0].current_round_id,
        "pre-admission pause must roll back the reservation without losing ownership or delivered causal context"
    );
    assert_eq!(
        request_count(&resumed_controls[0].requests().await),
        before_admission,
        "releasing a stale prepared dispatch cannot start a turn after Pause"
    );
    let resumed_handoff_gate = MockGateHandle::new();
    resumed_controls[0]
        .enqueue(MockTurn::gated_text(
            "Explicit resume of deferred handoff",
            &resumed_handoff_gate,
        ))
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: busy.id.clone(),
        })
        .await;
    scenario
        .wait_mock_turn(
            &resumed_handoff_gate,
            "explicit-resume-deferred-existing-runtime",
        )
        .await;
    let accepted_handoff = scenario
        .swarm(&busy.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == reserved_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert_eq!(
        accepted_handoff
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&admission_post.id))
            .count(),
        1,
        "explicit resume must accept the original deferred intent, not manufacture a replacement"
    );
    assert_eq!(
        request_count(&resumed_controls[0].requests().await),
        before_admission + 1
    );
    assert_eq!(
        request_count(&resumed_controls[1].requests().await),
        2,
        "a narrow deferred mention cannot wake another member during Resume"
    );
    resumed_handoff_gate.release_one();
    scenario
        .swarm(&busy.id, |state| {
            ready(state) && state.members[0].context_cursor >= admission_post.cursor
        })
        .await;
    scenario.pause(&busy.id).await;
    drop(lifecycle_client);

    eprintln!("Swarm sim phase: second-restart-control-swarm-launch");
    let restart_race = scenario.launched(1).await;
    scenario.pause(&restart_race.id).await;
    eprintln!("Swarm sim phase: second-restart-with-paused-groups");
    let restarted = scenario.fixture.restart_host().await;
    scenario.pending.clear();
    assert!(
        restarted
            .swarms
            .iter()
            .all(|state| state.lifecycle == SwarmLifecycle::Paused)
    );
    assert!(scenario.fixture.agent_ids().await.is_empty());
    let restart_post = scenario
        .post(
            &restart_race.id,
            publication(
                SwarmBoard::Coordination,
                "pause-before-resumed-runtime",
                vec![
                    text("Do not create a resumed runtime after Pause commits"),
                    SwarmBodySegment::MemberMention {
                        member_id: restart_race.members[0].spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    let resumed_runtime_gate = MockGateHandle::new();
    let resumed_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &restart_race.members[0].spec.name,
            MockScript::one(MockTurn::gated_text(
                "Explicitly resumed original session",
                &resumed_runtime_gate,
            )),
        )
        .await;
    let admission_gate = scenario.fixture.install_swarm_admission_test_gate().await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: restart_race.id.clone(),
        })
        .await;
    eprintln!("Swarm sim phase: second-restart-first-resume-final-admission");
    tokio::time::timeout(Duration::from_secs(5), admission_gate.wait_until_entered())
        .await
        .expect("restarted member must reach final admission before any backend is created");
    let reserved_restart = scenario
        .swarm(&restart_race.id, |state| {
            state.members[0].state == SwarmMemberState::Reserved
        })
        .await;
    assert!(
        reserved_restart.members[0].agent_id.is_none()
            && reserved_restart.members[0].session_id == restart_race.members[0].session_id
    );
    let restart_intent = reserved_restart
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&restart_post.id))
        .expect("restarted reserved intent")
        .clone();
    let (mut lifecycle_client, _) = scenario.fixture.connect_with_bootstrap().await;
    lifecycle_client
        .swarm_command(SwarmCommandPayload::Pause {
            swarm_id: restart_race.id.clone(),
        })
        .await
        .expect("pause before creation of a resumed backend runtime");
    scenario
        .swarm(&restart_race.id, |state| {
            state.lifecycle == SwarmLifecycle::Pausing
        })
        .await;
    admission_gate.release_one();
    drop(admission_gate);
    eprintln!("Swarm sim phase: second-restart-paused-admission-released");
    let deferred_restart = scenario
        .swarm(&restart_race.id, |state| {
            state.lifecycle == SwarmLifecycle::Paused
                && state.notifications.iter().any(|intent| {
                    intent.id == restart_intent.id && intent.state == SwarmDeliveryState::Pending
                })
        })
        .await;
    assert!(
        deferred_restart.members[0].agent_id.is_none()
            && deferred_restart.members[0].session_id == restart_race.members[0].session_id
            && deferred_restart.members[0].context_cursor == restart_race.members[0].context_cursor,
        "losing final admission must not create a private runtime or advance the retained session context"
    );
    assert!(
        scenario.fixture.agent_ids().await.is_empty(),
        "Pause must prevent late resumed runtime creation"
    );
    eprintln!("Swarm sim phase: second-restart-explicit-resume-original-reservation");
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: restart_race.id.clone(),
        })
        .await;
    scenario
        .wait_mock_turn(
            &resumed_runtime_gate,
            "second-restart-authorized-native-resume",
        )
        .await;
    let accepted_restart = scenario
        .swarm(&restart_race.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == restart_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert!(accepted_restart.members[0].session_id == restart_race.members[0].session_id);
    assert_eq!(scenario.fixture.agent_ids().await.len(), 1);
    let resumed_control = scenario
        .fixture
        .mock_by_id(
            accepted_restart.members[0]
                .agent_id
                .as_ref()
                .expect("approved resumed binding"),
        )
        .await;
    assert_eq!(
        request_count(&resumed_control.requests().await),
        1,
        "only explicit reauthorization may create and deliver the restarted member activation"
    );
    resumed_runtime_gate.release_one();
    scenario.swarm(&restart_race.id, ready).await;
    drop(resumed_reservation);
    assert!(resumed_control.violations().await.is_empty());

    scenario.pause(&restart_race.id).await;
    for pause_during_replay in [true, false] {
        let replay = MockResumeReplay::default();
        let replay_draft = scenario.generate(scenario.constraints(1)).await;
        let initial_replay_reservation = scenario
            .fixture
            .reserve_next_mock_launch(
                &replay_draft.members[0].name,
                MockScript::one(MockTurn::text("Initial replay-controlled member"))
                    .with_controlled_resume_replay(&replay),
            )
            .await;
        let starting_replay = scenario.launch(&replay_draft).await;
        let original_replay = scenario.swarm(&starting_replay.id, ready).await;
        drop(initial_replay_reservation);
        let replay_paused = scenario.pause(&original_replay.id).await;
        let replay_post = scenario
            .post(
                &original_replay.id,
                publication(
                    SwarmBoard::Coordination,
                    "pause-during-native-replay",
                    vec![
                        text("A deferred prompt must be revocable until native replay is ready"),
                        SwarmBodySegment::MemberMention {
                            member_id: original_replay.members[0].spec.id.clone(),
                        },
                    ],
                ),
            )
            .await;
        let before_replay_restart = scenario.snapshot(&original_replay.id).await;
        let replay_intent = before_replay_restart
            .notifications
            .iter()
            .find(|intent| intent.post_ids.contains(&replay_post.id))
            .expect("durable replay wake intent")
            .clone();
        let replay_bootstrap = scenario.fixture.restart_host().await;
        scenario.pending.clear();
        assert!(replay_bootstrap.agents.is_empty());
        let (mut pause_client, _) = scenario.fixture.connect_with_bootstrap().await;
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: original_replay.id.clone(),
            })
            .await;
        eprintln!("Swarm sim phase: controlled-native-replay-start");
        tokio::time::timeout(Duration::from_secs(5), replay.wait_until_started())
            .await
            .expect("actual mock native resume replay must begin before lifecycle revocation");
        let replaying = scenario
            .swarm(&original_replay.id, |state| {
                state.members[0].agent_id.is_some()
                    && state.members[0].state == SwarmMemberState::Reserved
            })
            .await;
        let replay_control = scenario
            .fixture
            .mock_by_id(
                replaying.members[0]
                    .agent_id
                    .as_ref()
                    .expect("runtime waiting on native replay"),
            )
            .await;
        assert_eq!(
            request_count(&replay_control.requests().await),
            0,
            "resuming native history cannot pre-accept a board prompt behind its replay barrier"
        );
        let expected_lifecycle = if pause_during_replay {
            pause_client
                .swarm_command(SwarmCommandPayload::Pause {
                    swarm_id: original_replay.id.clone(),
                })
                .await
                .expect("Pause while actual native replay is held");
            scenario
                .swarm(&original_replay.id, |state| {
                    state.lifecycle == SwarmLifecycle::Pausing
                })
                .await;
            SwarmLifecycle::Paused
        } else {
            scenario.fixture.fail_next_swarm_directory_sync().await;
            scenario
                .post(
                    &original_replay.id,
                    publication(
                        SwarmBoard::Coordination,
                        "durability-during-native-replay",
                        vec![text("Durability review must revoke the held native prompt")],
                    ),
                )
                .await;
            scenario
                .error(SwarmErrorCode::CommittedDurabilityUncertain)
                .await;
            let attention = scenario
                .swarm(&original_replay.id, |state| {
                    state.lifecycle == SwarmLifecycle::AttentionRequired
                })
                .await;
            assert_eq!(
                attention.recovery_requirement,
                protocol::SwarmRecoveryRequirement::ExplicitResume
            );
            SwarmLifecycle::AttentionRequired
        };
        eprintln!("Swarm sim phase: native-replay-revoked; lifecycle={expected_lifecycle:?}");
        replay.history_batch(1);
        replay.complete();
        let replay_deferred = scenario
            .swarm(&original_replay.id, |state| {
                state.lifecycle == expected_lifecycle
                    && state.notifications.iter().any(|intent| {
                        intent.id == replay_intent.id && intent.state == SwarmDeliveryState::Pending
                    })
            })
            .await;
        assert!(
            replay_deferred.members[0].session_id == original_replay.members[0].session_id
                && replay_deferred.members[0].context_cursor
                    == replay_paused.members[0].context_cursor,
            "replay completion after lifecycle revocation cannot advance or replace retained delivery context"
        );
        assert_eq!(
            request_count(&replay_control.requests().await),
            0,
            "native replay completion after committed Pause or durability attention cannot send the formerly prepared prompt"
        );
        let replay_turn = MockGateHandle::new();
        replay_control
            .enqueue(MockTurn::gated_text(
                "Explicitly authorized replay wake",
                &replay_turn,
            ))
            .await;
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: original_replay.id.clone(),
            })
            .await;
        scenario
            .wait_mock_turn(&replay_turn, "native-replay-explicit-resume")
            .await;
        let replay_accepted = scenario
            .swarm(&original_replay.id, |state| {
                state.notifications.iter().any(|intent| {
                    intent.id == replay_intent.id && intent.state == SwarmDeliveryState::Accepted
                })
            })
            .await;
        assert!(
            replay_accepted.members[0].agent_id == replaying.members[0].agent_id
                && replay_accepted.members[0].session_id == original_replay.members[0].session_id
        );
        assert_eq!(
            replay_accepted
                .notifications
                .iter()
                .filter(|intent| intent.post_ids.contains(&replay_post.id))
                .count(),
            1
        );
        assert_eq!(request_count(&replay_control.requests().await), 1);
        let (references, _) = last_dispatch_context(&replay_control.requests().await);
        assert!(references == vec![replay_post.id.clone()]);
        replay_turn.release_one();
        scenario.swarm(&original_replay.id, ready).await;
        assert!(replay_control.violations().await.is_empty());
        scenario.pause(&original_replay.id).await;
        drop(pause_client);
    }
}

#[tokio::test]
async fn authenticated_mcp_members_share_boards_without_author_spoofing_or_private_children() {
    let mut scenario = Scenario::new().await;
    let swarm = scenario.launched(2).await;
    let mocks = controls(&scenario, &swarm).await;
    scenario.pause(&swarm.id).await;
    let first = &swarm.members[0];
    let second = &swarm.members[1];
    let caller = scenario
        .fixture
        .agent_control_caller(first.agent_id.as_ref().expect("member binding"))
        .await;
    let bearer = caller
        .authorization
        .strip_prefix("Bearer ")
        .expect("fixture must provide bearer authentication")
        .to_owned();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(caller.url.clone()).auth_header(bearer),
    );
    let service = ().serve(transport).await.expect("connect swarm MCP catalog over HTTP");
    let tools = service
        .list_all_tools()
        .await
        .expect("list authenticated swarm tools over HTTP");
    assert_eq!(
        tools.len(),
        5,
        "a swarm caller sees the five shared-board tools, never ordinary orchestration tools"
    );
    for (name, read_only) in [
        ("tyde_swarm_describe", true),
        ("tyde_swarm_read_board", true),
        ("tyde_swarm_read_thread", true),
        ("tyde_swarm_read_image", true),
        ("tyde_swarm_post", false),
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("authenticated swarm catalog is missing {name}"));
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("authenticated swarm tool requires explicit hints: {name}"));
        // Native read-only approval treats missing hints as open-world/destructive;
        // publication is additive and idempotent, but must never claim to be read-only.
        assert!(
            annotations.read_only_hint == Some(read_only)
                && annotations.destructive_hint == Some(false)
                && annotations.idempotent_hint == Some(true)
                && annotations.open_world_hint == Some(false),
            "authenticated swarm tool must advertise its exact closed-world policy: {name}; read_only={:?}; destructive={:?}; idempotent={:?}; open_world={:?}",
            annotations.read_only_hint,
            annotations.destructive_hint,
            annotations.idempotent_hint,
            annotations.open_world_hint
        );
    }
    service
        .cancel()
        .await
        .expect("close swarm MCP catalog client");
    let describe: SwarmDescribe =
        tool_value(&call_tool(&caller, "tyde_swarm_describe", json!({})).await);
    assert!(
        describe.swarm.id == swarm.id && describe.member_id == first.spec.id,
        "bearer identity must determine the caller's swarm and member identity"
    );
    assert!(describe.swarm.constraints == swarm.constraints);
    assert_eq!(describe.swarm.members.len(), 2);
    let opening_page: SwarmBoardPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_board",
            serde_json::to_value(read_board(SwarmBoard::Briefing))
                .expect("serialize typed board query"),
        )
        .await,
    );
    assert_eq!(
        opening_page.posts.len(),
        1,
        "private assistant replies must not be republished to Briefing"
    );
    let opening = &opening_page.posts[0];
    let opening_thread: SwarmThreadPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_thread",
            serde_json::to_value(SwarmThreadRead {
                thread_id: opening.thread_id.clone(),
                after_cursor: None,
                limit: Some(1),
            })
            .expect("serialize typed thread query"),
        )
        .await,
    );
    assert!(opening_thread.root.id == opening.id);
    assert!(opening_thread.posts.is_empty());
    let empty_coordination: SwarmBoardPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_board",
            serde_json::to_value(read_board(SwarmBoard::Coordination))
                .expect("serialize typed board query"),
        )
        .await,
    );
    assert!(empty_coordination.posts.is_empty());

    let member_publication = publication(
        SwarmBoard::Coordination,
        "caller-scoped-key",
        vec![
            text("A shared finding, not an implicit assignment"),
            SwarmBodySegment::MemberMention {
                member_id: first.spec.id.clone(),
            },
        ],
    );
    let member_post: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(&member_publication).expect("serialize typed member publication"),
        )
        .await,
    );
    assert!(!member_post.duplicate);
    assert!(
        member_post.post.author
            == SwarmAuthor::Member {
                member_id: first.spec.id.clone()
            },
        "model-facing tools must derive authorship from the authenticated caller"
    );
    assert!(
        member_post.deliveries.is_empty(),
        "members cannot wake themselves with a mention"
    );
    assert!(
        member_post.post.round_id == opening.round_id,
        "agent publication must inherit its delivered causal round"
    );
    let human_post = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Coordination,
                "caller-scoped-key",
                vec![text("A human publication uses its own key scope")],
            ),
        )
        .await;
    assert!(
        human_post.id != member_post.post.id,
        "publication identities are caller-scoped, not globally reserved"
    );
    assert!(human_post.author == SwarmAuthor::Human);
    let caller_scoped_page = scenario
        .board(&swarm.id, read_board(SwarmBoard::Coordination))
        .await;
    assert_eq!(
        caller_scoped_page
            .posts
            .iter()
            .filter(|post| post.publication_id == member_post.post.publication_id)
            .count(),
        2,
        "the shared board must retain distinct authenticated authors with the same publication key"
    );
    assert!(
        caller_scoped_page
            .posts
            .iter()
            .any(|post| post == &member_post.post)
            && caller_scoped_page
                .posts
                .iter()
                .any(|post| post == &human_post),
        "the protocol and MCP notifications must describe the actual two committed posts"
    );
    let duplicate: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(&member_publication).expect("serialize retry"),
        )
        .await,
    );
    assert!(duplicate.duplicate && duplicate.post.id == member_post.post.id);
    assert!(duplicate.deliveries.is_empty());
    let mut changed = member_publication;
    changed.body.push(text("Changed publication content"));
    swarm_tool_error(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(changed).expect("serialize changed publication"),
        )
        .await,
        SwarmErrorCode::Conflict,
    );

    let mut human_reply = publication(
        SwarmBoard::Coordination,
        "human-reply-to-member",
        vec![
            text("Root author and explicitly mentioned peer should be notified"),
            SwarmBodySegment::MemberMention {
                member_id: second.spec.id.clone(),
            },
            SwarmBodySegment::MemberMention {
                member_id: second.spec.id.clone(),
            },
        ],
    );
    human_reply.thread_id = Some(member_post.post.thread_id.clone());
    let reply = scenario.post(&swarm.id, human_reply).await;
    let state = scenario.snapshot(&swarm.id).await;
    let reply_intents = state
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&reply.id))
        .collect::<Vec<_>>();
    assert_eq!(
        reply_intents.len(),
        2,
        "human replies notify agent root author plus deduplicated mentions"
    );
    assert!(
        reply_intents
            .iter()
            .any(|intent| intent.member_id == first.spec.id)
    );
    assert!(
        reply_intents
            .iter()
            .any(|intent| intent.member_id == second.spec.id)
    );
    let agent_update = publication(
        SwarmBoard::Briefing,
        "agent-cross-board-update",
        vec![
            text("Question for a peer, with a linked Coordination thread"),
            SwarmBodySegment::PostLink {
                post_id: member_post.post.id.clone(),
            },
            SwarmBodySegment::MemberMention {
                member_id: second.spec.id.clone(),
            },
            SwarmBodySegment::MemberMention {
                member_id: first.spec.id.clone(),
            },
        ],
    );
    let update: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_post",
            serde_json::to_value(agent_update).expect("serialize cross-board post"),
        )
        .await,
    );
    assert!(
        update.post.round_id == opening.round_id,
        "changing board and thread cannot mint a causal allowance"
    );
    assert_eq!(
        update.deliveries.len(),
        1,
        "agent Briefing posts wake only explicitly mentioned peers"
    );
    assert!(update.deliveries[0].member_id == second.spec.id);
    assert_eq!(update.deliveries[0].state, SwarmDeliveryState::Pending);
    let shared: SwarmThreadPage = tool_value(
        &call_tool(
            &caller,
            "tyde_swarm_read_thread",
            serde_json::to_value(SwarmThreadRead {
                thread_id: member_post.post.thread_id.clone(),
                after_cursor: None,
                limit: Some(10),
            })
            .expect("serialize authored-thread query"),
        )
        .await,
    );
    assert!(shared.root.id == member_post.post.id);
    assert_eq!(shared.posts.len(), 1);
    assert!(shared.posts[0].id == reply.id);

    let normal_name = "Ordinary nonmember";
    scenario
        .fixture
        .client
        .spawn_agent(protocol::SpawnAgentPayload {
            name: Some(normal_name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(scenario.project.id.clone()),
            params: protocol::SpawnAgentParams::New {
                workspace_roots: scenario
                    .project
                    .root_paths()
                    .iter()
                    .map(|root| root.0.clone())
                    .collect(),
                prompt: "Ordinary agent outside swarm membership".to_owned(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: Some(swarm.members[0].spec.launch_profile_id.clone()),
                cost_hint: None,
                access_mode: protocol::BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        })
        .await
        .expect("spawn mock nonmember over protocol");
    let normal: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "ordinary nonmember activation",
            |event: &protocol::NewAgentPayload| event.name == normal_name,
        )
        .await;
    let outsider = scenario
        .fixture
        .agent_control_caller(&normal.agent_id)
        .await;
    for (name, arguments) in [
        ("tyde_swarm_describe", json!({})),
        (
            "tyde_swarm_read_board",
            serde_json::to_value(read_board(SwarmBoard::Briefing)).expect("board query"),
        ),
        (
            "tyde_swarm_read_thread",
            serde_json::to_value(SwarmThreadRead {
                thread_id: opening.thread_id.clone(),
                after_cursor: None,
                limit: Some(10),
            })
            .expect("thread query"),
        ),
        (
            "tyde_swarm_post",
            serde_json::to_value(publication(
                SwarmBoard::Briefing,
                "nonmember-post",
                vec![text("Unauthorized publication")],
            ))
            .expect("publication"),
        ),
    ] {
        swarm_tool_error(
            &call_tool(&outsider, name, arguments).await,
            SwarmErrorCode::Unauthorized,
        );
    }
    let targets: protocol::WorkflowTargetsResponse =
        tool_value(&call_tool(&outsider, "tyde_workflow_targets", json!({})).await);
    let workflow_target = protocol::WorkflowSaveTarget::Project {
        project_id: scenario.project.id.clone(),
        root: scenario.project.root_paths()[0].clone(),
    };
    assert!(
        targets
            .targets
            .iter()
            .any(|target| target.target == workflow_target),
        "ordinary authenticated callers must retain project workflow target discovery"
    );
    let workflow_markdown = |id: &str, body: &str| {
        format!(
            "---\nid: {id}\nname: Protocol workflow\ndescription: Explicit workflow guidance\ncoordinator:\n  backend: claude\n  access_mode: read_only\ndeclared_backends: [claude]\ntriggers: [global]\n---\n{body}\n"
        )
    };
    let preserved_request = protocol::WorkflowSaveRequest {
        target: workflow_target.clone(),
        mode: protocol::WorkflowSaveMode::Create,
        filename: "preserved-workflow.md".to_owned(),
        markdown: workflow_markdown("preserved-workflow", "Preserve this ordinary workflow"),
    };
    let preserved_save: protocol::WorkflowSaveResponse = tool_value(
        &call_tool(
            &outsider,
            "tyde_workflow_save",
            serde_json::to_value(preserved_request).expect("serialize ordinary workflow creation"),
        )
        .await,
    );
    let preserved_path = std::path::PathBuf::from(&preserved_save.path);
    let preserved_bytes =
        std::fs::read(&preserved_path).expect("read actual ordinary workflow file");
    let (_, workflows_before) = scenario.fixture.connect_with_bootstrap().await;
    let blocked_request = protocol::WorkflowSaveRequest {
        target: workflow_target.clone(),
        mode: protocol::WorkflowSaveMode::Create,
        filename: "blocked-swarm-workflow.md".to_owned(),
        markdown: workflow_markdown(
            "blocked-swarm-workflow",
            "Valid guidance must not bypass swarm board-only authority",
        ),
    };
    let blocked_path = preserved_path
        .parent()
        .expect("project workflow directory")
        .join(&blocked_request.filename);
    let replacement_request = protocol::WorkflowSaveRequest {
        target: workflow_target,
        mode: protocol::WorkflowSaveMode::Replace {
            existing_path: preserved_save.path.clone(),
            existing_id: preserved_save.summary.id.clone(),
        },
        filename: "preserved-workflow.md".to_owned(),
        markdown: workflow_markdown(
            "preserved-workflow",
            "A valid reviewed ordinary replacement",
        ),
    };
    tool_error(&call_tool(&caller, "tyde_workflow_targets", json!({})).await);
    tool_error(
        &call_tool(
            &caller,
            "tyde_workflow_save",
            serde_json::to_value(&blocked_request)
                .expect("serialize hidden workflow creation attempt"),
        )
        .await,
    );
    tool_error(
        &call_tool(
            &caller,
            "tyde_workflow_save",
            serde_json::to_value(&replacement_request)
                .expect("serialize hidden workflow replacement attempt"),
        )
        .await,
    );
    tool_error(&call_tool(&caller, "tyde_list_agents", json!({})).await);
    assert!(
        !blocked_path.exists(),
        "direct invocation of a hidden workflow tool cannot write a new file for a swarm caller"
    );
    assert!(
        std::fs::read(&preserved_path).expect("read workflow after refused replacement")
            == preserved_bytes,
        "a hidden workflow replacement must leave the actual existing file unchanged"
    );
    let (_, workflows_after) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        workflows_after.workflow_summaries == workflows_before.workflow_summaries,
        "refused hidden tool invocations cannot change the visible workflow catalog"
    );
    let ordinary_create: protocol::WorkflowSaveResponse = tool_value(
        &call_tool(
            &outsider,
            "tyde_workflow_save",
            serde_json::to_value(&blocked_request)
                .expect("serialize identical ordinary workflow creation"),
        )
        .await,
    );
    assert!(
        ordinary_create.created && blocked_path.is_file(),
        "the refused swarm creation input must succeed for an ordinary caller, not fail because of malformed input"
    );
    let ordinary_replace: protocol::WorkflowSaveResponse = tool_value(
        &call_tool(
            &outsider,
            "tyde_workflow_save",
            serde_json::to_value(&replacement_request)
                .expect("serialize identical ordinary workflow replacement"),
        )
        .await,
    );
    assert!(
        !ordinary_replace.created
            && ordinary_replace.path == preserved_save.path
            && std::fs::read(&preserved_path).expect("read ordinary workflow replacement")
                != preserved_bytes,
        "ordinary callers retain valid workflow replacement behavior with the exact previously refused request"
    );

    let unauthenticated_body = reqwest::Client::new()
        .post(&caller.url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "tyde_swarm_describe", "arguments": {} },
        }))
        .send()
        .await
        .expect("call swarm tool without credentials")
        .text()
        .await
        .expect("read unauthenticated MCP SSE refusal");
    let unauthenticated: Value = serde_json::from_str(
        unauthenticated_body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("unauthenticated MCP refusal must include an SSE data event"),
    )
    .expect("parse unauthenticated MCP refusal");
    assert!(
        unauthenticated.get("error").is_some()
            || unauthenticated
                .get("result")
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool)
                == Some(true),
        "board access must reject missing caller authentication"
    );
    let before_children = scenario.fixture.agent_ids().await.len();
    let spawn_arguments = json!({
        "name": "Forbidden private child",
        "workspace_roots": scenario.project.root_paths().iter().map(|root| root.0.clone()).collect::<Vec<_>>(),
        "prompt": "Do not create a worker outside the approved swarm capacity",
        "backend_kind": "claude",
        "access_mode": "read_only",
        "launch_profile_id": first.spec.launch_profile_id,
    });
    tool_error(&call_tool(&caller, "tyde_spawn_agent", spawn_arguments.clone()).await);
    let mut explicit_parent = spawn_arguments.clone();
    explicit_parent
        .as_object_mut()
        .expect("spawn object")
        .insert("parent_agent_id".to_owned(), json!(first.agent_id));
    tool_error(&call_tool(&caller, "tyde_spawn_agent", explicit_parent).await);
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        before_children,
        "swarm members cannot reserve private children through either parent argument path"
    );
    let normal_child: Value =
        tool_value(&call_tool(&outsider, "tyde_spawn_agent", spawn_arguments).await);
    assert!(
        normal_child
            .get("agent_id")
            .and_then(Value::as_str)
            .is_some(),
        "ordinary nonmembers retain existing Tyde child orchestration behavior"
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        before_children + 1
    );
    for control in &mocks {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "MCP publication and reads while paused must not activate swarm members"
        );
        assert!(control.violations().await.is_empty());
    }

    let terminal_gate = MockGateHandle::new();
    mocks[0]
        .enqueue(MockTurn::busy_then_close_stream(&terminal_gate))
        .await;
    let fatal_post = scenario
        .post(
            &swarm.id,
            publication(
                SwarmBoard::Coordination,
                "fatal-member-authority",
                vec![
                    text("A terminated caller cannot inherit ordinary orchestration authority"),
                    SwarmBodySegment::MemberMention {
                        member_id: first.spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: swarm.id.clone(),
        })
        .await;
    scenario
        .wait_mock_turn(&terminal_gate, "fatal-member-native-stream-close")
        .await;
    terminal_gate.release_one();
    let fatal: protocol::AgentErrorPayload = scenario
        .wait(
            FrameKind::AgentError,
            "fatal member protocol error",
            |error: &protocol::AgentErrorPayload| {
                error.fatal && first.agent_id.as_ref() == Some(&error.agent_id)
            },
        )
        .await;
    assert_eq!(fatal.code, protocol::AgentErrorCode::BackendFailed);
    let terminated = scenario
        .swarm(&swarm.id, |state| {
            state.members.iter().any(|member| {
                member.spec.id == first.spec.id
                    && member.state == SwarmMemberState::Failed
                    && member.agent_id.is_none()
            })
        })
        .await;
    assert!(
        terminated
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&fatal_post.id)),
        "fatal shutdown must retain the actual durable delivery cause"
    );
    let before_terminal_files =
        std::fs::read(&preserved_path).expect("read workflow before terminated authority probes");
    let (_, before_terminal_catalog) = scenario.fixture.connect_with_bootstrap().await;
    let mut terminal_create = blocked_request.clone();
    terminal_create.filename = "blocked-terminated-workflow.md".to_owned();
    terminal_create.markdown = workflow_markdown(
        "blocked-terminated-workflow",
        "Valid terminated-caller creation probe",
    );
    let terminal_path = preserved_path
        .parent()
        .expect("actual workflow directory")
        .join(&terminal_create.filename);
    for (name, arguments) in [
        ("tyde_workflow_targets", json!({})),
        (
            "tyde_workflow_save",
            serde_json::to_value(&terminal_create).expect("serialize terminated create"),
        ),
        (
            "tyde_workflow_save",
            serde_json::to_value(&replacement_request).expect("serialize terminated replacement"),
        ),
        ("tyde_list_agents", json!({})),
        ("tyde_swarm_describe", json!({})),
    ] {
        tool_error(&call_tool(&caller, name, arguments).await);
    }
    assert!(!terminal_path.exists());
    assert!(
        std::fs::read(&preserved_path).expect("read workflow after terminated probes")
            == before_terminal_files,
        "fatal member credentials cannot become ordinary filesystem mutation authority after its swarm binding clears"
    );
    let (_, after_terminal_catalog) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        after_terminal_catalog.workflow_summaries == before_terminal_catalog.workflow_summaries
    );
    let live_targets: protocol::WorkflowTargetsResponse =
        tool_value(&call_tool(&outsider, "tyde_workflow_targets", json!({})).await);
    // workflow_location_for_scope reports actual directory.is_dir(). The
    // ordinary positive-control save created the formerly absent directory;
    // authority, scope and directory paths must remain exactly unchanged.
    let mut expected_live_targets = targets.targets.clone();
    for target in &mut expected_live_targets {
        target.location.exists = std::path::Path::new(&target.location.directory).is_dir();
    }
    assert!(
        live_targets.targets == expected_live_targets,
        "ordinary workflow authority must retain the exact targets/scopes/paths and report actual filesystem existence after successful save"
    );
    let live_create: protocol::WorkflowSaveResponse = tool_value(
        &call_tool(
            &outsider,
            "tyde_workflow_save",
            serde_json::to_value(terminal_create).expect("serialize live create control"),
        )
        .await,
    );
    assert!(
        live_create.created && terminal_path.is_file(),
        "the identical valid creation denied to the terminated member must still succeed for a live ordinary caller"
    );
    let live_replace: protocol::WorkflowSaveResponse = tool_value(
        &call_tool(
            &outsider,
            "tyde_workflow_save",
            serde_json::to_value(replacement_request).expect("serialize live replacement control"),
        )
        .await,
    );
    assert!(!live_replace.created && live_replace.path == preserved_save.path);
    assert!(mocks[0].violations().await.is_empty());
}

#[tokio::test]
async fn reviewed_capacity_changes_count_retiring_slots_and_never_redirect_retired_mentions() {
    let mut scenario = Scenario::new().await;
    let maximum = scenario.generate(scenario.constraints(16)).await;
    assert_eq!(
        maximum.members.len(),
        16,
        "host capacity boundary must produce an honest preview"
    );
    assert!(maximum.conflicts.is_empty());
    assert!(scenario.fixture.agent_ids().await.is_empty());
    scenario
        .send(SwarmCommandPayload::DiscardDraft {
            draft_id: maximum.id.clone(),
        })
        .await;
    let _: SwarmDraftNotifyPayload = scenario.wait(FrameKind::SwarmDraftNotify, "discard maximum-capacity preview", |event| {
        matches!(event, SwarmDraftNotifyPayload::Delete { draft_id } if *draft_id == maximum.id)
    }).await;

    let mut draft = scenario.generate(scenario.constraints(2)).await;
    for (index, original) in draft.members.clone().into_iter().enumerate() {
        let mut edited = original;
        edited.name = format!("Reviewed capacity peer {}", index + 1);
        edited.focus = Some("Human-reviewed optional starting focus".to_owned());
        edited.pinned = true;
        scenario
            .send(SwarmCommandPayload::EditDraftMember {
                draft_id: draft.id.clone(),
                expected_revision: draft.revision,
                member: edited.clone(),
            })
            .await;
        draft = scenario.draft(&draft.id, draft.revision + 1).await;
        assert!(
            draft.members.contains(&edited),
            "edited draft pins must survive until launch approval"
        );
    }
    let finish_gate = MockGateHandle::new();
    let retirement_teardown_gate = MockGateHandle::new();
    let launch_reservation = scenario
        .fixture
        .reserve_mock_launches(vec![
            (
                draft.members[0].name.clone(),
                MockScript::one(MockTurn::text("Retained peer launch")).with_unbounded_echo(),
            ),
            (
                draft.members[1].name.clone(),
                MockScript::one(MockTurn::gated_text(
                    "Turn that must finish before retirement",
                    &finish_gate,
                ))
                .with_shutdown_gate(&retirement_teardown_gate),
            ),
        ])
        .await;
    let starting = scenario.launch(&draft).await;
    scenario
        .wait_mock_turn(&finish_gate, "capacity-current-turn-before-retirement")
        .await;
    let busy = scenario
        .swarm(&starting.id, |state| {
            state.members.len() == 2
                && state
                    .members
                    .iter()
                    .all(|member| member.state == SwarmMemberState::Live)
                && state.members[0].runtime_status == Some(AgentControlStatus::Idle)
                && state.members[1].runtime_status == Some(AgentControlStatus::Thinking)
        })
        .await;
    drop(launch_reservation);
    let retained = busy.members[0].clone();
    let retiring = busy.members[1].clone();
    let retiring_control = scenario
        .fixture
        .mock_by_id(retiring.agent_id.as_ref().expect("retiring binding"))
        .await;
    let retiring_caller = scenario
        .fixture
        .agent_control_caller(retiring.agent_id.as_ref().expect("retiring caller binding"))
        .await;
    let pending_post = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Coordination,
                "pending-before-retire",
                vec![
                    text("Pending context for a member still working"),
                    SwarmBodySegment::MemberMention {
                        member_id: retiring.spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    let pending_state = scenario.snapshot(&busy.id).await;
    assert!(
        pending_state
            .notifications
            .iter()
            .any(|intent| intent.member_id == retiring.spec.id
                && intent.post_ids.contains(&pending_post.id)
                && intent.state == SwarmDeliveryState::Pending)
    );
    assert_eq!(
        request_count(&retiring_control.requests().await),
        1,
        "busy member delivery must stay pending rather than interrupting the current turn"
    );
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: busy.id.clone(),
            expected_revision: busy.revision,
            constraints: scenario.constraints(1),
        })
        .await;
    let first_preview = scenario
        .swarm(&busy.id, |state| state.change_preview.is_some())
        .await;
    let old_preview = first_preview
        .change_preview
        .expect("reviewed lower-capacity preview");
    assert!(old_preview.conflicts.is_empty());
    assert!(old_preview.retained == vec![retained.spec.id.clone()]);
    assert!(old_preview.retirements == vec![retiring.spec.id.clone()]);
    assert!(old_preview.additions.is_empty());
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: busy.id.clone(),
            expected_revision: busy.revision,
            constraints: scenario.constraints(1),
        })
        .await;
    let renewed = scenario
        .swarm(&busy.id, |state| {
            state
                .change_preview
                .as_ref()
                .is_some_and(|preview| preview.revision > old_preview.revision)
        })
        .await;
    let renewed_preview = renewed.change_preview.expect("replacement preview");
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: busy.id.clone(),
            preview_revision: old_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    scenario
        .send(SwarmCommandPayload::DiscardChangePreview {
            swarm_id: busy.id.clone(),
        })
        .await;
    scenario
        .swarm(&busy.id, |state| state.change_preview.is_none())
        .await;
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: busy.id.clone(),
            expected_revision: busy.revision,
            constraints: scenario.constraints(1),
        })
        .await;
    let reviewed = scenario
        .swarm(&busy.id, |state| state.change_preview.is_some())
        .await;
    let lower_preview = reviewed.change_preview.expect("fresh reviewed preview");
    assert!(lower_preview.base_revision == renewed_preview.base_revision);
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: busy.id.clone(),
            preview_revision: old_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: busy.id.clone(),
            preview_revision: lower_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let transition = scenario
        .swarm(&busy.id, |state| {
            state.revision > busy.revision
                && state.members.iter().any(|member| {
                    member.spec.id == retiring.spec.id && member.state == SwarmMemberState::Retiring
                })
        })
        .await;
    assert_eq!(transition.lifecycle, SwarmLifecycle::Transitioning);
    assert_eq!(transition.constraints.max_live_agents, 1);
    assert_eq!(
        transition
            .members
            .iter()
            .filter(|member| matches!(
                member.state,
                SwarmMemberState::Reserved | SwarmMemberState::Live | SwarmMemberState::Retiring
            ))
            .count(),
        2,
        "a lower limit must expose in-flight occupancy, not pretend instant compliance"
    );
    assert!(transition.change_preview.is_none());
    assert!(
        transition
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&pending_post.id)
                && intent.state == SwarmDeliveryState::Undeliverable
                && intent.error.is_some()),
        "pending notification for a retiring recipient must remain inspectably undeliverable"
    );
    assert!(
        !retiring_control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt)),
        "FinishTurn must not cancel the active turn"
    );
    // Proposal §2.4 allows FinishTurn's final handoff; §3 rejects Retired,
    // not Retiring callers. Revoking tools here would prevent that handoff.
    eprintln!("Swarm sim capacity phase: retiring-current-turn-board-tools");
    let retiring_describe: SwarmDescribe =
        tool_value(&call_tool(&retiring_caller, "tyde_swarm_describe", json!({})).await);
    assert!(
        retiring_describe.swarm.id == busy.id
            && retiring_describe.member_id == retiring.spec.id
            && retiring_describe
                .swarm
                .members
                .iter()
                .any(|member| member.spec.id == retiring.spec.id
                    && member.state == SwarmMemberState::Retiring
                    && member.agent_id == retiring.agent_id),
        "the retiring current-turn caller must remain authenticated to its canonical member, not an unrelated or replacement peer"
    );
    let retiring_read: SwarmBoardPage = tool_value(
        &call_tool(
            &retiring_caller,
            "tyde_swarm_read_board",
            serde_json::to_value(read_board(SwarmBoard::Coordination))
                .expect("serialize retiring current-turn read"),
        )
        .await,
    );
    assert!(
        retiring_read.posts.iter().any(|post| post == &pending_post),
        "a still-running retiring member must read shared context needed for its final handoff"
    );
    let (mut private_client, private_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    let retiring_stream = private_bootstrap
        .agents
        .iter()
        .find(|agent| retiring.agent_id.as_ref() == Some(&agent.agent_id))
        .expect("current retiring turn still owns an active protocol stream")
        .instance_stream
        .clone();
    private_client.send_message(&retiring_stream, "A retiring member cannot start another private turn".to_owned()).await
        .expect("attempt independent new input while the retiring member is still finishing its admitted turn");
    let private_refusal: protocol::AgentErrorPayload =
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let envelope = match private_client.next_event().await {
                    Ok(Some(envelope)) => envelope,
                    Ok(None) => panic!("retiring private-input connection closed before refusal"),
                    Err(error) => panic!(
                        "retiring private-input protocol read failed: {}",
                        frame_error_context(&error)
                    ),
                };
                if envelope.kind == FrameKind::AgentError {
                    let error: protocol::AgentErrorPayload = envelope
                        .parse_payload()
                        .expect("parse retiring private-input refusal");
                    if retiring.agent_id.as_ref() == Some(&error.agent_id) {
                        assert!(
                            envelope.stream == retiring_stream,
                            "retiring private-input refusal must target its real instance stream"
                        );
                        return error;
                    }
                }
            }
        })
        .await
        .expect("retiring private new-turn attempt must receive a visible bounded refusal");
    assert_eq!(private_refusal.code, protocol::AgentErrorCode::Unsupported);
    assert!(
        !private_refusal.fatal && !private_refusal.message.is_empty(),
        "retiring new-turn refusal must be visible without cancelling the currently admitted turn"
    );
    drop(private_client);
    let mut final_handoff_publication = publication(
        SwarmBoard::Coordination,
        "retiring-current-turn-handoff",
        vec![text(
            "Final durable handoff from the already-admitted retiring turn",
        )],
    );
    final_handoff_publication.thread_id = Some(pending_post.thread_id.clone());
    let final_handoff: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &retiring_caller,
            "tyde_swarm_post",
            serde_json::to_value(&final_handoff_publication)
                .expect("serialize retiring current-turn handoff"),
        )
        .await,
    );
    assert!(
        !final_handoff.duplicate
            && final_handoff.deliveries.is_empty()
            && final_handoff.post.thread_id == pending_post.thread_id
            && final_handoff.post.body == final_handoff_publication.body
            && final_handoff.post.author
                == SwarmAuthor::Member {
                    member_id: retiring.spec.id.clone()
                }
            && retiring.current_round_id.as_ref() == Some(&final_handoff.post.round_id),
        "retiring handoff must publish once with its authenticated author and existing cause, without starting another turn"
    );
    assert_eq!(
        request_count(&retiring_control.requests().await),
        1,
        "describe/read/final publication and refused private input cannot admit a second retiring backend turn"
    );
    assert!(
        !retiring_control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt)),
        "current-turn board access must not turn FinishTurn into cancellation"
    );

    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: busy.id.clone(),
            expected_revision: transition.revision,
            constraints: scenario.constraints(2),
        })
        .await;
    let expansion = scenario
        .swarm(&busy.id, |state| {
            state
                .change_preview
                .as_ref()
                .is_some_and(|preview| preview.base_revision == transition.revision)
        })
        .await;
    let expansion_preview = expansion
        .change_preview
        .expect("reviewed replacement addition");
    assert_eq!(expansion_preview.additions.len(), 1);
    let addition = expansion_preview.additions[0].clone();
    assert!(
        addition.id != retiring.spec.id,
        "retirement must not silently mutate an existing member into its replacement"
    );
    let addition_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &addition.name,
            MockScript::one(MockTurn::text("Explicitly reviewed replacement activation")),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: busy.id.clone(),
            preview_revision: expansion_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let blocked = scenario
        .swarm(&busy.id, |state| {
            state.revision > transition.revision
                && state
                    .members
                    .iter()
                    .any(|member| member.spec.id == addition.id)
        })
        .await;
    let proposed = blocked
        .members
        .iter()
        .find(|member| member.spec.id == addition.id)
        .expect("approved addition");
    assert_eq!(proposed.state, SwarmMemberState::Proposed);
    assert!(
        proposed.agent_id.is_none() && proposed.session_id.is_none(),
        "retiring slots count against admission until actual teardown"
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        2,
        "replacement cannot launch into a reserved retiring slot"
    );
    let undelivered = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Coordination,
                "mention-retiring-peer",
                vec![
                    text("This explicitly names the historical peer, not its replacement"),
                    SwarmBodySegment::MemberMention {
                        member_id: retiring.spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    let history = scenario.snapshot(&busy.id).await;
    let dispositions = history
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&undelivered.id))
        .collect::<Vec<_>>();
    assert_eq!(dispositions.len(), 1);
    assert!(dispositions[0].member_id == retiring.spec.id);
    assert_eq!(dispositions[0].state, SwarmDeliveryState::Undeliverable);
    assert!(dispositions[0].error.is_some());
    let retired_notification = dispositions[0].id.clone();
    let retained_caller = scenario
        .fixture
        .agent_control_caller(retained.agent_id.as_ref().expect("surviving live member"))
        .await;
    let before_teardown_describe: SwarmDescribe = tool_value(
        &tokio::time::timeout(
            Duration::from_secs(5),
            call_tool(&retained_caller, "tyde_swarm_describe", json!({})),
        )
        .await
        .expect("surviving member must authenticate and describe before teardown"),
    );
    assert!(
        before_teardown_describe.member_id == retained.spec.id
            && before_teardown_describe.swarm == history
    );
    let (mut retry_client, _) = scenario.fixture.connect_with_bootstrap().await;
    let occupied_before_teardown = scenario.fixture.agent_ids().await;
    finish_gate.release_one();
    scenario
        .wait_mock_turn(
            &retirement_teardown_gate,
            "actual retirement native teardown held before registry removal",
        )
        .await;
    // Fresh UI bootstrap asks every actor for usage; the closing actor cannot
    // service ReadUsageSnapshot while awaiting native shutdown. This existing
    // authenticated survivor reads canonical state over HTTP without that wait.
    let teardown_describe: SwarmDescribe = tool_value(
        &tokio::time::timeout(
            Duration::from_secs(5),
            call_tool(&retained_caller, "tyde_swarm_describe", json!({})),
        )
        .await
        .expect("canonical describe must not await the gated closing actor"),
    );
    assert!(teardown_describe.member_id == retained.spec.id);
    let teardown_state = teardown_describe.swarm;
    assert_eq!(teardown_state.lifecycle, SwarmLifecycle::Transitioning);
    assert!(
        teardown_state
            .members
            .iter()
            .any(|member| member.spec.id == retiring.spec.id
                && member.state == SwarmMemberState::Retiring
                && member.runtime_status == Some(AgentControlStatus::Idle)
                && member.agent_id == retiring.agent_id
                && member.session_id == retiring.session_id),
        "entered native shutdown is still an occupied Retiring binding until actual host registry teardown"
    );
    assert!(
        teardown_state
            .members
            .iter()
            .any(|member| member.spec.id == addition.id
                && member.state == SwarmMemberState::Proposed
                && member.agent_id.is_none()
                && member.session_id.is_none())
            && scenario.fixture.agent_ids().await == occupied_before_teardown,
        "the reviewed replacement must remain unbound while native retirement teardown is held"
    );
    // Retry cleanup used to classify a closing handle as terminated and call
    // registry.status directly, publishing Retired before returning Conflict.
    // Both entry points must reject without bypassing actual registry removal.
    for (phase, command) in [
        (
            "RetryMember during held retirement teardown",
            SwarmCommandPayload::RetryMember {
                swarm_id: busy.id.clone(),
                member_id: retiring.spec.id.clone(),
            },
        ),
        (
            "RetryNotification during held retirement teardown",
            SwarmCommandPayload::RetryNotification {
                swarm_id: busy.id.clone(),
                notification_id: retired_notification.clone(),
            },
        ),
    ] {
        eprintln!("Swarm sim capacity retry boundary begin: {phase}");
        retry_client
            .swarm_command(command)
            .await
            .expect("send retiring retry through independent real protocol client");
        let rejection = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = client_event(&mut retry_client, phase).await;
                if event.kind == FrameKind::SwarmErrorNotify {
                    let error: SwarmErrorNotifyPayload = event
                        .parse_payload()
                        .expect("parse retiring retry protocol rejection");
                    if error.swarm_id.as_ref() == Some(&busy.id) {
                        return error;
                    }
                }
                assert!(
                    event.kind != FrameKind::AgentClosed
                        || event
                            .parse_payload::<protocol::AgentClosedPayload>()
                            .expect("parse actual teardown close subject")
                            .agent_id
                            != *retiring
                                .agent_id
                                .as_ref()
                                .expect("occupied retiring runtime"),
                    "retry cannot close the occupied retiring runtime while native shutdown is held"
                );
            }
        })
        .await
        .expect("held teardown retry must reject without waiting for native shutdown");
        assert_eq!(rejection.code, SwarmErrorCode::Conflict);
        assert!(!rejection.message.is_empty());
        let after_retry_describe: SwarmDescribe = tool_value(
            &tokio::time::timeout(
                Duration::from_secs(5),
                call_tool(&retained_caller, "tyde_swarm_describe", json!({})),
            )
            .await
            .expect("post-rejection canonical describe must not await native teardown"),
        );
        assert!(
            after_retry_describe.member_id == retained.spec.id
                && after_retry_describe.swarm == teardown_state,
            "rejected retry must preserve the exact Retiring binding, session, pending replacement, causal rounds and delivery dispositions until actual registry removal"
        );
        assert!(
            scenario.fixture.agent_ids().await == occupied_before_teardown,
            "rejected retry cannot release the occupied retirement slot or admit the reviewed replacement before teardown completes"
        );
        assert_eq!(request_count(&retiring_control.requests().await), 1);
        eprintln!("Swarm sim capacity retry boundary rejected without mutation: {phase}");
    }
    drop(retry_client);
    retirement_teardown_gate.release_one();
    let completed = scenario
        .swarm(&busy.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
                && state.members.iter().any(|member| {
                    member.spec.id == retiring.spec.id && member.state == SwarmMemberState::Retired
                })
                && state.members.iter().any(|member| {
                    member.spec.id == addition.id
                        && member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Idle)
                })
        })
        .await;
    drop(addition_reservation);
    let retained_after = completed
        .members
        .iter()
        .find(|member| member.spec.id == retained.spec.id)
        .expect("retained member");
    assert!(
        retained_after.agent_id == retained.agent_id
            && retained_after.session_id == retained.session_id,
        "changing capacity must retain unaffected member identity and session"
    );
    assert_eq!(
        completed
            .members
            .iter()
            .filter(|member| matches!(
                member.state,
                SwarmMemberState::Reserved | SwarmMemberState::Live | SwarmMemberState::Retiring
            ))
            .count(),
        2
    );
    let retired_after = completed
        .members
        .iter()
        .find(|member| member.spec.id == retiring.spec.id)
        .expect("historical retired member");
    assert!(
        retired_after.session_id == retiring.session_id && retired_after.agent_id.is_none(),
        "retirement must retain history without a live activation binding"
    );
    eprintln!("Swarm sim capacity phase: retired-closed-caller-refused");
    let before_retired_refusal = scenario.snapshot(&busy.id).await;
    tool_error(&call_tool(&retiring_caller, "tyde_swarm_describe", json!({})).await);
    let mut retired_publication = final_handoff_publication;
    retired_publication.publication_id =
        SwarmPublicationId("closed-retired-caller-post".to_owned());
    tool_error(
        &call_tool(
            &retiring_caller,
            "tyde_swarm_post",
            serde_json::to_value(retired_publication)
                .expect("serialize valid closed-retired-caller post attempt"),
        )
        .await,
    );
    assert!(
        scenario.snapshot(&busy.id).await == before_retired_refusal,
        "the same authenticated credential after actual Retired/close must not describe, post or mutate durable causes and membership"
    );
    let persisted_handoff = scenario
        .thread(
            &busy.id,
            SwarmThreadRead {
                thread_id: pending_post.thread_id.clone(),
                after_cursor: None,
                limit: Some(10),
            },
        )
        .await;
    assert!(
        persisted_handoff.root == pending_post
            && persisted_handoff.posts == vec![final_handoff.post],
        "retirement must retain the permitted final handoff exactly once and reject any post from the closed historical caller"
    );
    scenario
        .send(SwarmCommandPayload::RetryMember {
            swarm_id: busy.id.clone(),
            member_id: retiring.spec.id.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    let retired_post = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Coordination,
                "mention-retired-peer",
                vec![
                    text("Historical author remains referenceable"),
                    SwarmBodySegment::MemberMention {
                        member_id: retiring.spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    let final_snapshot = scenario.snapshot(&busy.id).await;
    assert!(final_snapshot.notifications.iter().any(|intent| {
        intent.post_ids.contains(&retired_post.id)
            && intent.member_id == retiring.spec.id
            && intent.state == SwarmDeliveryState::Undeliverable
    }));
    assert!(
        !final_snapshot
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&retired_post.id)
                && intent.member_id == addition.id),
        "retired references must never redirect to a replacement"
    );
    assert!(retiring_control.violations().await.is_empty());

    let addition_live = final_snapshot
        .members
        .iter()
        .find(|member| member.spec.id == addition.id)
        .expect("active reviewed replacement")
        .clone();
    let addition_control = scenario
        .fixture
        .mock_by_id(
            addition_live
                .agent_id
                .as_ref()
                .expect("replacement runtime"),
        )
        .await;
    let before_retirement_handoff = request_count(&addition_control.requests().await);
    let admission_gate = scenario.fixture.install_swarm_admission_test_gate().await;
    let retiring_handoff_post = scenario
        .post(
            &busy.id,
            publication(
                SwarmBoard::Coordination,
                "retire-before-final-handoff",
                vec![
                    text("This prepared follow-up cannot outrun explicit FinishTurn retirement"),
                    SwarmBodySegment::MemberMention {
                        member_id: addition.id.clone(),
                    },
                ],
            ),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), admission_gate.wait_until_entered())
        .await
        .expect("replacement follow-up must reach final admission before native delivery");
    let reserved_handoff = scenario
        .swarm(&busy.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&retiring_handoff_post.id)
                    && intent.state == SwarmDeliveryState::Dispatching
            })
        })
        .await;
    let handoff_intent = reserved_handoff
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&retiring_handoff_post.id))
        .expect("durable replacement dispatch intent")
        .clone();
    let (mut lifecycle_client, _) = scenario.fixture.connect_with_bootstrap().await;
    lifecycle_client
        .swarm_command(SwarmCommandPayload::PreviewChange {
            swarm_id: busy.id.clone(),
            expected_revision: reserved_handoff.revision,
            constraints: scenario.constraints(1),
        })
        .await
        .expect("review capacity reduction on a second client while admission is gated");
    let handoff_preview_state = scenario
        .swarm(&busy.id, |state| state.change_preview.is_some())
        .await;
    let handoff_preview = handoff_preview_state
        .change_preview
        .expect("reviewed retirement before handoff");
    assert!(
        handoff_preview.conflicts.is_empty()
            && handoff_preview.retained == vec![retained.spec.id.clone()]
            && handoff_preview.retirements == vec![addition.id.clone()]
            && handoff_preview.additions.is_empty()
    );
    lifecycle_client
        .swarm_command(SwarmCommandPayload::ApplyChange {
            swarm_id: busy.id.clone(),
            preview_revision: handoff_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await
        .expect("commit explicit retirement before native follow-up admission");
    // The durable batch is still Reserved while this pre-enqueue gate is held.
    // FinishTurn retains that unresolved handoff as RetiringReserved until
    // defer resolves it; it cannot start native input or free the slot early.
    scenario
        .swarm(&busy.id, |state| {
            state.constraints.max_live_agents == 1
                && state.members.iter().any(|member| {
                    member.spec.id == addition.id
                        && member.state == SwarmMemberState::RetiringReserved
                })
        })
        .await;
    assert_eq!(
        request_count(&addition_control.requests().await),
        before_retirement_handoff
    );
    admission_gate.release_one();
    drop(admission_gate);
    let retired_handoff = scenario
        .swarm(&busy.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
                && state.members.iter().any(|member| {
                    member.spec.id == addition.id && member.state == SwarmMemberState::Retired
                })
                && state.notifications.iter().any(|intent| {
                    intent.id == handoff_intent.id
                        && intent.state == SwarmDeliveryState::Undeliverable
                })
        })
        .await;
    assert_eq!(
        request_count(&addition_control.requests().await),
        before_retirement_handoff,
        "releasing a prepared message after FinishTurn retirement cannot start another backend turn"
    );
    let historical_addition = retired_handoff
        .members
        .iter()
        .find(|member| member.spec.id == addition.id)
        .expect("retired reviewed replacement history");
    assert!(
        historical_addition.agent_id.is_none()
            && historical_addition.session_id == addition_live.session_id
    );
    assert!(
        retired_handoff
            .notifications
            .iter()
            .any(|intent| intent.id == handoff_intent.id
                && intent.member_id == addition.id
                && intent.post_ids == vec![retiring_handoff_post.id.clone()]
                && intent.error.is_some()),
        "retirement must preserve the exact undeliverable intent rather than redirect its prepared delivery"
    );
    let final_retained = retired_handoff
        .members
        .iter()
        .find(|member| member.spec.id == retained.spec.id)
        .expect("unaffected retained peer");
    assert!(
        final_retained.agent_id == retained.agent_id
            && final_retained.session_id == retained.session_id
    );
    let retiring_runtime = addition_live
        .agent_id
        .as_ref()
        .expect("replacement runtime");
    let at_retired = scenario.fixture.agent_ids().await;
    eprintln!(
        "Swarm sim retirement phase: canonical-retired; runtime count={}, retained present={}, retiring present={}",
        at_retired.len(),
        retained
            .agent_id
            .as_ref()
            .is_some_and(|id| at_retired.contains(id)),
        at_retired.contains(retiring_runtime),
    );
    assert_eq!(
        at_retired.len(),
        1,
        "Retired must free the actual host capacity slot, not merely clear the canonical binding before teardown"
    );
    assert!(
        !at_retired.contains(retiring_runtime)
            && retained
                .agent_id
                .as_ref()
                .is_some_and(|id| at_retired.contains(id))
    );
    // Require the close event as well as the exact Retired/runtime-count oracle.
    let _: protocol::AgentClosedPayload = scenario
        .wait(
            FrameKind::AgentClosed,
            "retired replacement runtime close",
            |closed: &protocol::AgentClosedPayload| &closed.agent_id == retiring_runtime,
        )
        .await;
    let after_close = scenario.fixture.agent_ids().await;
    eprintln!(
        "Swarm sim retirement phase: runtime-closed; runtime count={}, retained present={}, retiring present={}",
        after_close.len(),
        retained
            .agent_id
            .as_ref()
            .is_some_and(|id| after_close.contains(id)),
        after_close.contains(retiring_runtime),
    );
    assert_eq!(
        after_close.len(),
        1,
        "no prepared turn may keep or recreate a retired capacity slot"
    );
    assert!(addition_control.violations().await.is_empty());

    let private_draft = scenario.generate(scenario.constraints(2)).await;
    let private_send_gate = MockGateHandle::new();
    let board_finish_gate = MockGateHandle::new();
    let private_retirement_gate = MockGateHandle::new();
    let private_reservation = scenario
        .fixture
        .reserve_mock_launches(vec![
            (
                private_draft.members[0].name.clone(),
                MockScript::one(MockTurn::text("Unaffected private-boundary peer"))
                    .with_unbounded_echo(),
            ),
            (
                private_draft.members[1].name.clone(),
                MockScript::one(MockTurn::text("Idle private-boundary peer"))
                    .then(MockTurn::text(
                        "Private receipt followed by natural completion",
                    ))
                    .then(MockTurn::gated_text(
                        "Original pending board input",
                        &board_finish_gate,
                    ))
                    .then(MockTurn::gated_text(
                        "Retiring private turn completes naturally",
                        &private_retirement_gate,
                    ))
                    .with_send_gate(&private_send_gate),
            ),
        ])
        .await;
    let private_launch = scenario.launch(&private_draft).await;
    let private_live = scenario.swarm(&private_launch.id, ready).await;
    let private_member = private_live.members[1].clone();
    let private_agent = private_member
        .agent_id
        .as_ref()
        .expect("actual private handoff runtime");
    let private_control = scenario.fixture.mock_by_id(private_agent).await;
    let private_retained_control = scenario
        .fixture
        .mock_by_id(
            private_live.members[0]
                .agent_id
                .as_ref()
                .expect("nonretiring private-boundary peer"),
        )
        .await;
    let (mut private_client, private_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    let private_stream = private_bootstrap
        .agents
        .iter()
        .find(|agent| &agent.agent_id == private_agent)
        .expect("private input stream on its own protocol connection")
        .instance_stream
        .clone();
    private_client
        .send_message(
            &private_stream,
            "An admitted private turn precedes the durable board wake".to_owned(),
        )
        .await
        .expect("send actual idle private input");
    scenario
        .wait_mock_turn(
            &private_send_gate,
            "private checked enqueue awaiting native receipt",
        )
        .await;
    let private_reserved = scenario
        .swarm(&private_live.id, |state| {
            state.members.iter().any(|member| {
                member.spec.id == private_member.spec.id
                    && member.state == SwarmMemberState::Reserved
            })
        })
        .await;
    assert_eq!(request_count(&private_control.requests().await), 1);
    let behind_private = scenario.post(&private_live.id, publication(SwarmBoard::Coordination, "board-behind-private-receipt", vec![
        text("This original board intent waits behind the private receipt, then wakes without another human command"),
        SwarmBodySegment::MemberMention { member_id: private_member.spec.id.clone() },
    ])).await;
    let pending_private = scenario
        .swarm(&private_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&behind_private.id)
                    && intent.state == SwarmDeliveryState::Pending
            })
        })
        .await;
    let behind_intent = pending_private
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&behind_private.id))
        .expect("durable board intent behind reserved private admission")
        .clone();
    assert!(
        pending_private
            .members
            .iter()
            .any(|member| member.spec.id == private_member.spec.id
                && member.state == SwarmMemberState::Reserved
                && member.agent_id.as_ref() == Some(private_agent)
                && member.context_cursor == private_member.context_cursor)
    );
    assert!(
        private_reserved
            .members
            .iter()
            .any(|member| member.spec.id == private_member.spec.id
                && member.session_id == private_member.session_id)
    );
    private_send_gate.release_one();
    // No further host command rescues the pending wake: only native receipt and
    // natural completion release this private reservation back to the worker.
    scenario
        .wait_mock_turn(
            &private_send_gate,
            "pending board automatically dispatched after private completion",
        )
        .await;
    assert_eq!(request_count(&private_control.requests().await), 2);
    private_send_gate.release_one();
    scenario
        .wait_mock_turn(
            &board_finish_gate,
            "original board wake accepted behind private receipt",
        )
        .await;
    let accepted_private = scenario
        .swarm(&private_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == behind_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert_eq!(
        request_count(&private_control.requests().await),
        3,
        "private completion must reschedule the original pending board input exactly once, without a rescuing Pause/Resume or new post"
    );
    assert_eq!(
        accepted_private
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&behind_private.id))
            .count(),
        1
    );
    let (private_references, _) = last_dispatch_context(&private_control.requests().await);
    assert!(private_references == vec![behind_private.id.clone()]);
    assert!(
        accepted_private
            .members
            .iter()
            .any(|member| member.spec.id == private_member.spec.id
                && member.agent_id.as_ref() == Some(private_agent)
                && member.session_id == private_member.session_id)
    );
    board_finish_gate.release_one();
    let private_idle = scenario.swarm(&private_live.id, ready).await;
    let before_private_retirement = scenario.fixture.agent_ids().await;
    private_client
        .send_message(
            &private_stream,
            "This checked private receipt precedes approved FinishTurn retirement".to_owned(),
        )
        .await
        .expect("send private receipt before explicit retirement");
    scenario
        .wait_mock_turn(
            &private_send_gate,
            "private receipt held before FinishTurn retirement",
        )
        .await;
    let retirement_reserved = scenario
        .swarm(&private_live.id, |state| {
            // PrivateAdmission creates a fresh human round and reservation,
            // not a reviewed configuration revision (only ApplyChange does).
            state.revision == private_idle.revision
                && state.rounds.len() == private_idle.rounds.len() + 1
                && state.members.iter().any(|member| {
                    member.spec.id == private_member.spec.id
                        && member.state == SwarmMemberState::Reserved
                        && member.agent_id.as_ref() == Some(private_agent)
                        && member.session_id == private_member.session_id
                        && member.current_round_id != private_idle.members[1].current_round_id
                })
        })
        .await;
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: private_live.id.clone(),
            expected_revision: retirement_reserved.revision,
            constraints: scenario.constraints(1),
        })
        .await;
    let private_review = scenario
        .swarm(&private_live.id, |state| {
            state
                .change_preview
                .as_ref()
                .is_some_and(|preview| preview.base_revision == retirement_reserved.revision)
        })
        .await;
    let private_preview = private_review
        .change_preview
        .as_ref()
        .expect("reviewed retirement of outstanding private receipt");
    assert!(
        private_preview.conflicts.is_empty()
            && private_preview.retirements == vec![private_member.spec.id.clone()]
            && private_preview.retained == vec![private_live.members[0].spec.id.clone()]
            && private_preview.additions.is_empty()
    );
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: private_live.id.clone(),
            preview_revision: private_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let retiring_reserved = scenario
        .swarm(&private_live.id, |state| {
            state.constraints.max_live_agents == 1
                && state.members.iter().any(|member| {
                    member.spec.id == private_member.spec.id
                        && member.state == SwarmMemberState::RetiringReserved
                })
        })
        .await;
    assert_eq!(retiring_reserved.lifecycle, SwarmLifecycle::Transitioning);
    assert!(
        retiring_reserved
            .members
            .iter()
            .any(|member| member.spec.id == private_member.spec.id
                && member.agent_id.as_ref() == Some(private_agent)
                && member.session_id == private_member.session_id)
    );
    assert_eq!(
        request_count(&private_control.requests().await),
        3,
        "FinishTurn cannot claim native acceptance while the earlier private receipt is held"
    );
    assert!(
        scenario.fixture.agent_ids().await == before_private_retirement,
        "RetiringReserved must retain its real capacity slot while the private native receipt is unresolved"
    );
    assert!(
        !scenario
            .pending
            .iter()
            .any(|event| event.kind == FrameKind::AgentClosed
                && event
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .expect("pending close subject")
                    .agent_id
                    == *private_agent),
        "FinishTurn cannot close an outstanding checked private handoff before native receipt"
    );
    let implicit_briefing = scenario
        .post(
            &private_live.id,
            publication(
                SwarmBoard::Briefing,
                "implicit-briefing-during-private-retirement",
                vec![text(
                    "This unmentioned human root wakes only the nonretiring peer",
                )],
            ),
        )
        .await;
    let implicit_state = scenario
        .swarm(&private_live.id, |state| {
            state
                .notifications
                .iter()
                .any(|intent| intent.post_ids.contains(&implicit_briefing.id))
        })
        .await;
    let implicit_recipients = implicit_state
        .notifications
        .iter()
        .filter(|intent| intent.post_ids.contains(&implicit_briefing.id))
        .collect::<Vec<_>>();
    assert_eq!(
        implicit_recipients.len(),
        1,
        "unmentioned human Briefing must exclude every retiring member, including an outstanding RetiringReserved private receipt"
    );
    assert!(
        implicit_recipients[0].member_id == private_live.members[0].spec.id
            && implicit_state
                .members
                .iter()
                .any(|member| member.spec.id == private_member.spec.id
                    && member.state == SwarmMemberState::RetiringReserved
                    && member.agent_id.as_ref() == Some(private_agent)),
        "implicit publication must notify the exact nonretiring peer without consuming or replacing the retiring private handoff"
    );
    let implicit_intent_id = implicit_recipients[0].id.clone();
    assert_eq!(request_count(&private_control.requests().await), 3);
    private_send_gate.release_one();
    scenario
        .wait_mock_turn(
            &private_retirement_gate,
            "retiring private input accepted but current turn not completed",
        )
        .await;
    let retiring_turn = scenario
        .swarm(&private_live.id, |state| {
            state.members.iter().any(|member| {
                member.spec.id == private_member.spec.id
                    && member.state == SwarmMemberState::Retiring
                    && member.runtime_status == Some(AgentControlStatus::Thinking)
            })
        })
        .await;
    assert_eq!(retiring_turn.lifecycle, SwarmLifecycle::Transitioning);
    assert_eq!(
        request_count(&private_control.requests().await),
        4,
        "the one checked-enqueued private input may finish; no extra wake is admitted after retirement"
    );
    assert!(
        scenario.fixture.agent_ids().await == before_private_retirement,
        "receipt resolution changes RetiringReserved to Retiring, not Retired before actual native turn completion"
    );
    assert!(
        !private_control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt)),
        "FinishTurn must wait for the accepted private turn's natural completion, not cancel it"
    );
    private_retirement_gate.release_one();
    let private_retired = scenario
        .swarm(&private_live.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
                && state.members.iter().any(|member| {
                    member.spec.id == private_member.spec.id
                        && member.state == SwarmMemberState::Retired
                })
                && state.notifications.iter().any(|intent| {
                    intent.id == implicit_intent_id && intent.state == SwarmDeliveryState::Accepted
                })
        })
        .await;
    let private_history = private_retired
        .members
        .iter()
        .find(|member| member.spec.id == private_member.spec.id)
        .expect("completed private retirement history");
    assert!(
        private_history.agent_id.is_none()
            && private_history.runtime_status.is_none()
            && private_history.session_id == private_member.session_id
    );
    let after_private_retirement = scenario.fixture.agent_ids().await;
    assert!(
        after_private_retirement.len() + 1 == before_private_retirement.len()
            && !after_private_retirement.contains(private_agent)
            && before_private_retirement
                .iter()
                .filter(|agent| *agent != private_agent)
                .all(|agent| after_private_retirement.contains(agent)),
        "actual native completion must free exactly the retired private slot, never close an unrelated retained peer"
    );
    let _: protocol::AgentClosedPayload = scenario
        .wait(
            FrameKind::AgentClosed,
            "private FinishTurn runtime close",
            |closed: &protocol::AgentClosedPayload| &closed.agent_id == private_agent,
        )
        .await;
    assert_eq!(request_count(&private_control.requests().await), 4);
    assert_eq!(
        private_retired
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&implicit_briefing.id))
            .count(),
        1
    );
    assert_eq!(
        request_count(&private_retained_control.requests().await),
        2,
        "the unmentioned Briefing root must be accepted once by the nonretiring peer while the retiring peer receives no additional input"
    );
    let (implicit_references, _) =
        last_dispatch_context(&private_retained_control.requests().await);
    assert!(implicit_references == vec![implicit_briefing.id]);
    assert!(private_retained_control.violations().await.is_empty());
    assert!(private_control.violations().await.is_empty());
    drop(private_client);
    drop(private_reservation);
    scenario.pause(&private_live.id).await;

    scenario.pause(&busy.id).await;
    let idle_pair = scenario.launched(2).await;
    let pair_controls = controls(&scenario, &idle_pair).await;
    assert!(
        idle_pair
            .notifications
            .iter()
            .all(|intent| intent.state == SwarmDeliveryState::Accepted)
    );
    let mut constraints_only = idle_pair.constraints.clone();
    constraints_only
        .shared_guidance
        .push_str("\nKeep subsequent reviews concise");
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: idle_pair.id.clone(),
            expected_revision: idle_pair.revision,
            constraints: constraints_only.clone(),
        })
        .await;
    let guidance_review = scenario
        .swarm(&idle_pair.id, |state| {
            state
                .change_preview
                .as_ref()
                .is_some_and(|preview| preview.base_revision == idle_pair.revision)
        })
        .await;
    let guidance_preview = guidance_review
        .change_preview
        .as_ref()
        .expect("constraints-only reviewed change");
    assert!(
        guidance_preview.conflicts.is_empty()
            && guidance_preview.additions.is_empty()
            && guidance_preview.retirements.is_empty()
            && guidance_preview.retained.len() == 2
    );
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: idle_pair.id.clone(),
            preview_revision: guidance_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let idle_pair = scenario
        .swarm(&idle_pair.id, |state| state.constraints == constraints_only)
        .await;
    assert_eq!(
        idle_pair.lifecycle,
        SwarmLifecycle::Running,
        "constraints-only Apply with no Proposed/Reserved/Retiring member must return Running immediately, without relying on incidental status noise"
    );
    assert!(idle_pair.change_preview.is_none() && ready(&idle_pair));
    for control in &pair_controls {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "constraints-only review cannot mint an activation or redeliver an accepted opening"
        );
    }

    let (mut observer, observer_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        observer_bootstrap
            .swarms
            .iter()
            .any(|state| state == &idle_pair)
    );
    let ordinary_name = "Unrelated ordinary activity beside an idle swarm";
    let ordinary_first_gate = MockGateHandle::new();
    let ordinary_second_gate = MockGateHandle::new();
    let ordinary_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            ordinary_name,
            MockScript::one(MockTurn::gated_text(
                "Ordinary initial activity",
                &ordinary_first_gate,
            ))
            .then(MockTurn::gated_text(
                "Ordinary follow-up activity",
                &ordinary_second_gate,
            )),
        )
        .await;
    scenario
        .fixture
        .client
        .spawn_agent(protocol::SpawnAgentPayload {
            name: Some(ordinary_name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(scenario.project.id.clone()),
            params: protocol::SpawnAgentParams::New {
                workspace_roots: scenario
                    .project
                    .root_paths()
                    .iter()
                    .map(|root| root.0.clone())
                    .collect(),
                prompt: "Ordinary work has no swarm cause".to_owned(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: Some(idle_pair.members[0].spec.launch_profile_id.clone()),
                cost_hint: None,
                access_mode: protocol::BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        })
        .await
        .expect("spawn actual ordinary runtime beside idle swarm");
    let ordinary: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "ordinary noise control startup",
            |agent: &protocol::NewAgentPayload| agent.name == ordinary_name,
        )
        .await;
    assert!(
        ordinary.swarm_membership.is_none(),
        "ordinary startup cannot claim swarm ownership"
    );
    scenario
        .wait_mock_turn(&ordinary_first_gate, "ordinary startup while swarm idle")
        .await;
    let peer_new =
        client_event_without_swarm(&mut observer, "ordinary startup state isolation", |event| {
            event.kind == FrameKind::NewAgent
                && event
                    .parse_payload::<protocol::NewAgentPayload>()
                    .expect("observer ordinary startup")
                    .agent_id
                    == ordinary.agent_id
        })
        .await;
    let peer_ordinary: protocol::NewAgentPayload =
        peer_new.parse_payload().expect("observer ordinary stream");
    assert!(peer_ordinary.swarm_membership.is_none());
    client_event_without_swarm(
        &mut observer,
        "ordinary native-ready startup without swarm noise",
        |event| {
            // attach_subscriber_with_latest_output packages the start into
            // AgentBootstrap; it is not a separate initial AgentStart frame.
            event.kind == FrameKind::AgentBootstrap
                && event.stream == peer_ordinary.instance_stream
                && event
                    .parse_payload::<protocol::AgentBootstrapPayload>()
                    .expect("actual ordinary native-ready startup")
                    .events
                    .iter()
                    .any(|event| {
                        matches!(event,
                        protocol::AgentBootstrapEvent::AgentStart(start)
                            if start.agent_id == ordinary.agent_id && start.session_id.is_some()
                                && start.swarm_membership.is_none())
                    })
        },
    )
    .await;
    let ordinary_control = scenario.fixture.mock_by_id(&ordinary.agent_id).await;
    ordinary_first_gate.release_one();
    client_event_without_swarm(
        &mut observer,
        "ordinary first completion without swarm noise",
        |event| {
            event.kind == FrameKind::ChatEvent
                && event.stream == peer_ordinary.instance_stream
                && matches!(
                    event
                        .parse_payload::<protocol::ChatEvent>()
                        .expect("ordinary completion"),
                    protocol::ChatEvent::TypingStatusChanged(false)
                )
        },
    )
    .await;
    scenario
        .fixture
        .client
        .send_message(
            &ordinary.instance_stream,
            "An unrelated second native turn".to_owned(),
        )
        .await
        .expect("send ordinary follow-up through actual agent stream");
    scenario
        .wait_mock_turn(&ordinary_second_gate, "ordinary follow-up while swarm idle")
        .await;
    ordinary_second_gate.release_one();
    client_event_without_swarm(
        &mut observer,
        "ordinary second completion without swarm noise",
        |event| {
            event.kind == FrameKind::ChatEvent
                && event.stream == peer_ordinary.instance_stream
                && matches!(
                    event
                        .parse_payload::<protocol::ChatEvent>()
                        .expect("ordinary completion"),
                    protocol::ChatEvent::TypingStatusChanged(false)
                )
        },
    )
    .await;
    assert_eq!(
        request_count(&ordinary_control.requests().await),
        2,
        "both ordinary native turns must actually execute"
    );
    scenario
        .fixture
        .client
        .close_agent(&ordinary.instance_stream)
        .await
        .expect("close ordinary noise control");
    client_event_without_swarm(
        &mut observer,
        "ordinary close without swarm noise",
        |event| {
            event.kind == FrameKind::AgentClosed
                && event
                    .parse_payload::<protocol::AgentClosedPayload>()
                    .expect("observer ordinary close")
                    .agent_id
                    == ordinary.agent_id
        },
    )
    .await;
    let _: protocol::AgentClosedPayload = scenario
        .wait(
            FrameKind::AgentClosed,
            "ordinary runtime teardown",
            |closed: &protocol::AgentClosedPayload| closed.agent_id == ordinary.agent_id,
        )
        .await;
    scenario
        .board(&idle_pair.id, read_board(SwarmBoard::Briefing))
        .await;
    let observation_end = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut ordinary_frames = 0;
    while let Ok(result) = tokio::time::timeout_at(observation_end, observer.next_event()).await {
        let event = match result {
            Ok(Some(event)) => event,
            Ok(None) => panic!("ordinary noise observer closed"),
            Err(error) => panic!(
                "ordinary noise observer read failed: {}",
                frame_error_context(&error)
            ),
        };
        assert!(
            !matches!(
                event.kind,
                FrameKind::SwarmNotify
                    | FrameKind::SwarmErrorNotify
                    | FrameKind::AgentError
                    | FrameKind::CommandError
            ),
            "completed unrelated native activity cannot schedule or broadcast unchanged swarm state: {:?}",
            event.kind
        );
        ordinary_frames += 1;
    }
    eprintln!(
        "Swarm sim idle observation: ordinary native turns=2, swarm notifications=0, trailing frames={ordinary_frames}"
    );
    assert!(
        scenario.snapshot(&idle_pair.id).await == idle_pair,
        "ordinary startup, input, completion and close cannot change an idle swarm's canonical revision, intents, rounds or bindings"
    );
    assert!(ordinary_control.violations().await.is_empty());
    drop(observer);
    drop(ordinary_reservation);

    eprintln!(
        "Swarm sim recovery phase: two-idle-live-before-restart; accepted intents={}, retained sessions=2",
        idle_pair.notifications.len()
    );
    let idle_bootstrap = tokio::time::timeout(
        Duration::from_secs(5),
        scenario.fixture.relaunch_host_after_kill(),
    )
    .await
    .expect("two idle live members must restart without a native replay wait");
    scenario.pending.clear();
    assert!(idle_bootstrap.agents.is_empty() && scenario.fixture.agent_ids().await.is_empty());
    let dormant = idle_bootstrap
        .swarms
        .iter()
        .find(|state| state.id == idle_pair.id)
        .expect("retained idle swarm recovery");
    assert_eq!(dormant.lifecycle, SwarmLifecycle::AttentionRequired);
    assert!(dormant.notifications == idle_pair.notifications && dormant.members.len() == 2);
    for previous in &idle_pair.members {
        let recovered = dormant
            .members
            .iter()
            .find(|member| member.spec.id == previous.spec.id)
            .expect("exact retained peer");
        assert!(
            recovered.state == SwarmMemberState::Dormant
                && recovered.agent_id.is_none()
                && recovered.runtime_status.is_none()
                && recovered.session_id == previous.session_id
                && recovered.context_cursor == previous.context_cursor,
            "a previously Live idle peer is canonically Dormant, not a Proposed addition or a phantom bound actor"
        );
    }
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: idle_pair.id.clone(),
        })
        .await;
    let resumed_idle = scenario
        .swarm(&idle_pair.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
        })
        .await;
    assert!(
        resumed_idle
            .members
            .iter()
            .all(|member| member.state == SwarmMemberState::Dormant
                && member.agent_id.is_none()
                && member.runtime_status.is_none())
    );
    assert!(
        resumed_idle.notifications == idle_pair.notifications
            && scenario.fixture.agent_ids().await.is_empty(),
        "Resume without intended notifications must not instantiate either retained session or replay its opening"
    );
    let mut expanded_constraints = resumed_idle.constraints.clone();
    expanded_constraints.max_live_agents = 3;
    expanded_constraints.allocations[0].count = 3;
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: idle_pair.id.clone(),
            expected_revision: resumed_idle.revision,
            constraints: expanded_constraints,
        })
        .await;
    let dormant_expansion = scenario
        .swarm(&idle_pair.id, |state| {
            state
                .change_preview
                .as_ref()
                .is_some_and(|preview| preview.base_revision == resumed_idle.revision)
        })
        .await;
    let dormant_preview = dormant_expansion
        .change_preview
        .as_ref()
        .expect("reviewed C addition after idle recovery");
    assert!(
        dormant_preview.conflicts.is_empty()
            && dormant_preview.retirements.is_empty()
            && dormant_preview.additions.len() == 1
            && dormant_preview.retained.len() == 2
            && idle_pair
                .members
                .iter()
                .all(|member| dormant_preview.retained.contains(&member.spec.id))
    );
    let new_peer = dormant_preview.additions[0].clone();
    let new_peer_gate = MockGateHandle::new();
    let new_peer_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &new_peer.name,
            MockScript::one(MockTurn::gated_text(
                "Only the reviewed new peer starts",
                &new_peer_gate,
            )),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: idle_pair.id.clone(),
            preview_revision: dormant_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    scenario
        .wait_mock_turn(
            &new_peer_gate,
            "C startup beside two dormant retained peers",
        )
        .await;
    new_peer_gate.release_one();
    let settled_expansion = scenario
        .swarm(&idle_pair.id, |state| {
            state.lifecycle == SwarmLifecycle::Running
                && state.members.iter().any(|member| {
                    member.spec.id == new_peer.id
                        && member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Idle)
                })
        })
        .await;
    assert!(settled_expansion.change_preview.is_none() && settled_expansion.members.len() == 3);
    for previous in &idle_pair.members {
        let retained_peer = settled_expansion
            .members
            .iter()
            .find(|member| member.spec.id == previous.spec.id)
            .expect("unchanged dormant peer after C startup");
        assert!(
            retained_peer.state == SwarmMemberState::Dormant
                && retained_peer.agent_id.is_none()
                && retained_peer.runtime_status.is_none()
                && retained_peer.session_id == previous.session_id
                && retained_peer.context_cursor == previous.context_cursor
                && retained_peer.spec == previous.spec,
            "C completion must settle Running while old retained peers remain dormant, not wait forever on mistaken Proposed state"
        );
    }
    let c_agent = settled_expansion
        .members
        .iter()
        .find(|member| member.spec.id == new_peer.id)
        .and_then(|member| member.agent_id.as_ref())
        .expect("sole C runtime");
    assert!(
        scenario.fixture.agent_ids().await == vec![c_agent.clone()],
        "only the reviewed new peer may consume an actual runtime slot after idle Resume"
    );
    for old_intent in &idle_pair.notifications {
        assert!(
            settled_expansion
                .notifications
                .iter()
                .any(|intent| intent == old_intent),
            "C startup cannot replace or redeliver either retained peer's accepted original intent"
        );
    }
    let c_control = scenario.fixture.mock_by_id(c_agent).await;
    assert_eq!(request_count(&c_control.requests().await), 1);
    for control in &pair_controls {
        assert_eq!(
            request_count(&control.requests().await),
            1,
            "retained idle sessions receive no native input during restart, Resume or C startup"
        );
    }
    assert!(c_control.violations().await.is_empty());
    drop(new_peer_reservation);
}

#[tokio::test]
async fn explicit_legacy_conversion_requires_quiescence_and_preserves_sessions_without_transcript_posts()
 {
    let mut scenario = Scenario::new().await;
    let project_id = scenario.project.id.clone();
    let legacy_spec = |name: &str| protocol::TeamMemberCreateSpec {
        name: name.to_owned(),
        description: "Peer with an explicit shared project scope".to_owned(),
        profile: None,
        custom_agent_id: None,
        backend_kind: protocol::BackendKind::Claude,
        cost_hint: None,
        project_ids: vec![project_id.clone()],
    };
    scenario
        .fixture
        .client
        .team_create(protocol::TeamCreatePayload {
            name: "Legacy team pending explicit conversion".to_owned(),
            manager: legacy_spec("Legacy manager"),
        })
        .await
        .expect("create real legacy team over protocol");
    let team_event: protocol::TeamNotifyPayload = scenario
        .wait(FrameKind::TeamNotify, "legacy team creation", |event| {
            matches!(event, protocol::TeamNotifyPayload::Upsert { .. })
        })
        .await;
    let protocol::TeamNotifyPayload::Upsert { team } = team_event else {
        panic!("legacy creation must upsert a team")
    };
    let manager_event: protocol::TeamMemberNotifyPayload = scenario.wait(FrameKind::TeamMemberNotify, "legacy manager creation", |event| {
        matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member } if member.id == team.manager_member_id)
    }).await;
    let protocol::TeamMemberNotifyPayload::Upsert { member: manager } = manager_event else {
        panic!("legacy creation must upsert its manager");
    };
    scenario
        .fixture
        .client
        .team_member_create(protocol::TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: legacy_spec("Legacy report"),
            session_id: None,
        })
        .await
        .expect("create scoped legacy report");
    let report_event: protocol::TeamMemberNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberNotify,
            "legacy report creation",
            |event| {
                matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.team_id == team.id && member.id != manager.id)
            },
        )
        .await;
    let protocol::TeamMemberNotifyPayload::Upsert { member: report } = report_event else {
        panic!("legacy report creation must upsert its member");
    };
    scenario
        .fixture
        .client
        .team_member_activate(protocol::TeamMemberActivatePayload {
            member_id: manager.id.clone(),
            prompt: Some(
                "Private legacy conversation must not become shared board history".to_owned(),
            ),
            images: None,
        })
        .await
        .expect("activate legacy manager using mock backend");
    let active: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "legacy session activation",
            |event: &protocol::NewAgentPayload| event.team_member_id.as_ref() == Some(&manager.id),
        )
        .await;
    let session_event: protocol::TeamMemberNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberNotify,
            "legacy persisted session",
            |event| {
                matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.id == manager.id && member.session_id.is_some())
            },
        )
        .await;
    let protocol::TeamMemberNotifyPayload::Upsert {
        member: with_session,
    } = session_event
    else {
        panic!("legacy activation must persist its session");
    };
    let original_session = with_session.session_id.expect("legacy resumable session");
    let original_settings = scenario.settings_for_agent(&active.agent_id).await;
    let _: protocol::TeamMemberBindingNotifyPayload = scenario.wait(FrameKind::TeamMemberBindingNotify, "legacy live binding", |event| {
        matches!(event, protocol::TeamMemberBindingNotifyPayload::Upsert { binding }
            if binding.member_id == manager.id && binding.current_agent_id.as_ref() == Some(&active.agent_id))
    }).await;
    scenario
        .send(SwarmCommandPayload::PreviewMigration {
            team_id: team.id.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    scenario
        .fixture
        .client
        .close_agent(&active.instance_stream)
        .await
        .expect("close legacy agent before migration");
    let _: protocol::TeamMemberBindingNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberBindingNotify,
            "legacy quiescence",
            |event| {
                matches!(event, protocol::TeamMemberBindingNotifyPayload::Upsert { binding }
            if binding.member_id == manager.id && binding.current_agent_id.is_none())
            },
        )
        .await;
    scenario
        .send(SwarmCommandPayload::PreviewMigration {
            team_id: team.id.clone(),
        })
        .await;
    let draft_event: SwarmDraftNotifyPayload = scenario.wait(FrameKind::SwarmDraftNotify, "explicit legacy migration preview", |event| {
        matches!(event, SwarmDraftNotifyPayload::Upsert { draft } if draft.legacy_team_id.as_ref() == Some(&team.id))
    }).await;
    let SwarmDraftNotifyPayload::Upsert { draft } = draft_event else {
        panic!("migration preview must upsert a draft")
    };
    let draft = *draft;
    assert!(
        draft.conflicts.is_empty(),
        "compatible quiescent legacy members must have a reviewable conversion"
    );
    assert_eq!(draft.members.len(), 2);
    assert!(
        draft
            .members
            .iter()
            .any(|member| member.id.0 == manager.id.0)
    );
    assert!(
        draft
            .members
            .iter()
            .any(|member| member.id.0 == report.id.0)
    );
    assert!(
        draft
            .members
            .iter()
            .all(|member| member.project_id == scenario.project.id),
        "conversion cannot widen members' project scopes"
    );
    let (_, preview_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        preview_bootstrap.swarms.is_empty(),
        "preview must not silently convert or activate a legacy team"
    );
    assert!(
        preview_bootstrap
            .teams
            .iter()
            .any(|stored| stored.id == team.id)
    );
    assert!(
        preview_bootstrap
            .team_members
            .iter()
            .any(|stored| stored.id == manager.id
                && stored.session_id.as_ref() == Some(&original_session)),
        "legacy sessions remain owned by the team until apply"
    );
    assert!(
        scenario.fixture.agent_ids().await.is_empty(),
        "migration preview starts no work"
    );
    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: draft.id.clone(),
            expected_revision: Some(draft.revision),
            name: draft.name.clone(),
            opening_brief: "Human-reviewed opening brief for the converted peer group".to_owned(),
            constraints: draft.constraints.clone(),
        })
        .await;
    let mut reviewed = scenario.draft(&draft.id, draft.revision + 1).await;
    assert!(
        reviewed.members == draft.members,
        "reviewing the brief must preserve pinned migrated identities"
    );
    let mut changed_retained_selection = reviewed
        .members
        .iter()
        .find(|member| member.id.0 == manager.id.0)
        .expect("retained legacy manager selection")
        .clone();
    let selected_backend = changed_retained_selection.backend_kind;
    let schema_state = |entries: &[protocol::SessionSchemaEntry]| match entries
        .iter()
        .find(|entry| entry.backend_kind() == selected_backend)
    {
        Some(protocol::SessionSchemaEntry::Ready { .. }) => "ready",
        Some(protocol::SessionSchemaEntry::Pending { .. }) => "pending",
        Some(protocol::SessionSchemaEntry::Unavailable { .. }) => "unavailable",
        None => "missing",
    };
    eprintln!(
        "Swarm sim migration model-schema phase: initial={} after-activation={}",
        schema_state(&scenario.fixture.bootstrap.session_schemas),
        schema_state(&preview_bootstrap.session_schemas)
    );
    // The initial cold bootstrap can be Pending; successful activation resolves
    // discovery, so the fresh protocol projection owns the valid choices.
    let model_schema = preview_bootstrap
        .session_schemas
        .iter()
        .find_map(|entry| {
            entry
                .ready_schema()
                .filter(|schema| schema.backend_kind == changed_retained_selection.backend_kind)
        })
        .expect("mock backend must expose its canonical valid model schema");
    let model_field = model_schema
        .fields
        .iter()
        .find(|field| field.key == "model")
        .expect("mock schema must describe model selection");
    let protocol::SessionSettingFieldType::Select { options, .. } = &model_field.field_type else {
        panic!("model selection must expose canonical choices");
    };
    let alternate_model = options
        .iter()
        .map(|option| protocol::SessionSettingValue::String(option.value.clone()))
        .find(|value| {
            changed_retained_selection
                .session_settings
                .0
                .get(&model_field.key)
                != Some(value)
        })
        .expect("mock model catalog must provide a different valid selection");
    changed_retained_selection
        .session_settings
        .0
        .insert(model_field.key.clone(), alternate_model);
    scenario
        .send(SwarmCommandPayload::EditDraftMember {
            draft_id: reviewed.id.clone(),
            expected_revision: reviewed.revision,
            member: changed_retained_selection,
        })
        .await;
    let selection_error = scenario.error(SwarmErrorCode::Conflict).await;
    assert!(
        selection_error.draft_id.as_ref() == Some(&reviewed.id),
        "retained-session selection conflict must identify the draft requiring explicit retire-and-add"
    );
    let (_, after_selection_refusal) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        after_selection_refusal.swarm_drafts.contains(&reviewed)
            && after_selection_refusal.swarms.is_empty()
            && after_selection_refusal.agents.is_empty(),
        "a refused retained-session model override cannot alter the reviewed draft or start work"
    );
    scenario
        .send(SwarmCommandPayload::ApplyMigration {
            draft_id: reviewed.id.clone(),
            expected_revision: draft.revision,
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    let (mut legacy_client, _) = scenario.fixture.connect_with_bootstrap().await;
    let conversion_gate = scenario.fixture.install_swarm_conversion_test_gate().await;
    scenario
        .send(SwarmCommandPayload::ApplyMigration {
            draft_id: reviewed.id.clone(),
            expected_revision: reviewed.revision,
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), conversion_gate.wait_until_entered())
        .await
        .expect("conversion must reach its unlocked preliminary-validation boundary");
    legacy_client
        .team_member_create(protocol::TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: legacy_spec("Concurrent legacy roster addition"),
            session_id: None,
        })
        .await
        .expect("commit concurrent legacy roster edit from a second real client");
    let added_event: protocol::TeamMemberNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberNotify,
            "concurrent legacy roster commit",
            |event| {
                matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.team_id == team.id && member.name == "Concurrent legacy roster addition")
            },
        )
        .await;
    let protocol::TeamMemberNotifyPayload::Upsert {
        member: concurrent_member,
    } = added_event
    else {
        panic!("concurrent member creation must publish its committed record");
    };
    conversion_gate.release_one();
    drop(conversion_gate);
    let stale_roster = scenario.error(SwarmErrorCode::Conflict).await;
    assert!(stale_roster.draft_id.as_ref() == Some(&reviewed.id));
    let (_, after_roster_race) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        after_roster_race.swarms.is_empty()
            && after_roster_race.agents.is_empty()
            && after_roster_race.swarm_drafts.contains(&reviewed)
            && after_roster_race
                .team_members
                .iter()
                .any(|member| member.id == concurrent_member.id),
        "a roster mutation committed after prevalidation must defeat stale conversion without lost legacy records or partial swarm launch"
    );
    legacy_client
        .team_member_delete(protocol::TeamMemberDeletePayload {
            id: concurrent_member.id.clone(),
        })
        .await
        .expect("remove explicitly added legacy member before fresh review");
    let _: protocol::TeamMemberNotifyPayload = scenario.wait(FrameKind::TeamMemberNotify, "concurrent legacy roster cleanup", |event|
        matches!(event, protocol::TeamMemberNotifyPayload::Delete { member } if member.id == concurrent_member.id)).await;
    reviewed = scenario.refresh_migration(&reviewed).await;

    let conversion_gate = scenario.fixture.install_swarm_conversion_test_gate().await;
    scenario
        .send(SwarmCommandPayload::ApplyMigration {
            draft_id: reviewed.id.clone(),
            expected_revision: reviewed.revision,
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), conversion_gate.wait_until_entered())
        .await
        .expect("fresh conversion must expose the permission-mutation interleaving");
    let other_root = scenario
        .fixture
        .store_dir()
        .join("migration-other-workspace");
    std::fs::create_dir(&other_root).expect("create real alternate project scope");
    legacy_client
        .project_create(ProjectCreatePayload {
            name: "Concurrent alternate migration scope".to_owned(),
            roots: vec![ProjectRootPath(other_root.to_string_lossy().into_owned())],
        })
        .await
        .expect("create alternate scoped project over protocol");
    let other_event: ProjectNotifyPayload = scenario.wait(FrameKind::ProjectNotify, "alternate legacy scope creation", |event|
        matches!(event, ProjectNotifyPayload::Upsert { project } if project.name == "Concurrent alternate migration scope")).await;
    let ProjectNotifyPayload::Upsert {
        project: other_project,
    } = other_event
    else {
        panic!("alternate scope must be a real created project")
    };
    legacy_client
        .team_member_update(protocol::TeamMemberUpdatePayload {
            id: report.id.clone(),
            name: report.name.clone(),
            description: report.description.clone(),
            profile: None,
            project_ids: vec![other_project.id.clone()],
        })
        .await
        .expect("commit a valid permission-scope change while conversion is prepared");
    let _: protocol::TeamMemberNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberNotify,
            "concurrent legacy permission commit",
            |event| {
                matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.id == report.id && member.project_ids == vec![other_project.id.clone()])
            },
        )
        .await;
    conversion_gate.release_one();
    drop(conversion_gate);
    scenario.error(SwarmErrorCode::Conflict).await;
    let (_, after_scope_race) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        after_scope_race.swarms.is_empty()
            && after_scope_race.agents.is_empty()
            && after_scope_race.swarm_drafts.contains(&reviewed)
            && after_scope_race
                .team_members
                .iter()
                .any(|member| member.id == report.id
                    && member.project_ids == vec![other_project.id.clone()]),
        "commit-boundary conversion must revalidate permissions, not only roster counts and quiescence"
    );
    legacy_client
        .team_member_update(protocol::TeamMemberUpdatePayload {
            id: report.id.clone(),
            name: report.name.clone(),
            description: report.description.clone(),
            profile: None,
            project_ids: report.project_ids.clone(),
        })
        .await
        .expect("restore original scope for a new explicit migration review");
    let _: protocol::TeamMemberNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberNotify,
            "legacy permission cleanup",
            |event| {
                matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.id == report.id && member.project_ids == report.project_ids)
            },
        )
        .await;

    legacy_client
        .team_member_create(protocol::TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: legacy_spec("Concurrent sessionless legacy admission"),
            session_id: None,
        })
        .await
        .expect("create explicit sessionless member for admission race review");
    let admission_member_event: protocol::TeamMemberNotifyPayload = scenario.wait(FrameKind::TeamMemberNotify, "sessionless legacy race member", |event|
        matches!(event, protocol::TeamMemberNotifyPayload::Upsert { member }
            if member.team_id == team.id && member.name == "Concurrent sessionless legacy admission")).await;
    let protocol::TeamMemberNotifyPayload::Upsert {
        member: admission_member,
    } = admission_member_event
    else {
        panic!("sessionless legacy race must start with a committed member");
    };
    assert!(admission_member.session_id.is_none());
    let _: protocol::TeamMemberBindingNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberBindingNotify,
            "initial sessionless race binding",
            |event| {
                matches!(event, protocol::TeamMemberBindingNotifyPayload::Upsert { binding }
            if binding.member_id == admission_member.id && binding.current_agent_id.is_none())
            },
        )
        .await;
    reviewed = scenario.refresh_migration(&reviewed).await;
    assert_eq!(reviewed.members.len(), 3);
    let conversion_gate = scenario.fixture.install_swarm_conversion_test_gate().await;
    scenario
        .send(SwarmCommandPayload::ApplyMigration {
            draft_id: reviewed.id.clone(),
            expected_revision: reviewed.revision,
        })
        .await;
    tokio::time::timeout(Duration::from_secs(5), conversion_gate.wait_until_entered())
        .await
        .expect("conversion must remain unlocked while legacy admission can commit");
    legacy_client
        .team_member_activate(protocol::TeamMemberActivatePayload {
            member_id: admission_member.id.clone(),
            prompt: Some(
                "A concurrent legacy activation cannot coexist with stale conversion".to_owned(),
            ),
            images: None,
        })
        .await
        .expect("admit a sessionless legacy member through the real second-client protocol");
    let concurrent_active: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "concurrent legacy admission",
            |event: &protocol::NewAgentPayload| {
                event.team_member_id.as_ref() == Some(&admission_member.id)
            },
        )
        .await;
    let _: protocol::TeamMemberBindingNotifyPayload = scenario.wait(FrameKind::TeamMemberBindingNotify, "concurrent committed legacy binding", |event|
        matches!(event, protocol::TeamMemberBindingNotifyPayload::Upsert { binding }
            if binding.member_id == admission_member.id && binding.current_agent_id.as_ref() == Some(&concurrent_active.agent_id))).await;
    conversion_gate.release_one();
    drop(conversion_gate);
    scenario.error(SwarmErrorCode::Conflict).await;
    let (_, after_admission_race) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        after_admission_race.swarms.is_empty()
            && after_admission_race.swarm_drafts.contains(&reviewed)
            && after_admission_race
                .team_member_bindings
                .iter()
                .any(|binding| binding.member_id == admission_member.id
                    && binding.current_agent_id.as_ref() == Some(&concurrent_active.agent_id)),
        "accepted legacy admission and stale swarm conversion cannot both own or execute the same member"
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        1,
        "losing conversion cannot create any additional bound swarm agents"
    );
    scenario
        .fixture
        .client
        .close_agent(&concurrent_active.instance_stream)
        .await
        .expect("close explicitly admitted legacy race agent");
    let _: protocol::AgentClosedPayload = scenario
        .wait(
            FrameKind::AgentClosed,
            "concurrent legacy runtime close",
            |closed: &protocol::AgentClosedPayload| closed.agent_id == concurrent_active.agent_id,
        )
        .await;
    let _: protocol::TeamMemberBindingNotifyPayload = scenario
        .wait(
            FrameKind::TeamMemberBindingNotify,
            "concurrent legacy admission cleanup",
            |event| {
                matches!(event, protocol::TeamMemberBindingNotifyPayload::Upsert { binding }
            if binding.member_id == admission_member.id && binding.current_agent_id.is_none())
            },
        )
        .await;
    // CloseAgent is awaited by this connection's command actor. The delete
    // follows that completed close, not the creation's old unbound notification
    // or another client's independently processed teardown race.
    scenario
        .fixture
        .client
        .team_member_delete(protocol::TeamMemberDeletePayload {
            id: admission_member.id.clone(),
        })
        .await
        .expect("remove admission-race member before the final original-lineup review");
    let _: protocol::TeamMemberNotifyPayload = scenario.wait(FrameKind::TeamMemberNotify, "admission-race member cleanup", |event|
        matches!(event, protocol::TeamMemberNotifyPayload::Delete { member } if member.id == admission_member.id)).await;
    assert!(scenario.fixture.agent_ids().await.is_empty());
    reviewed = scenario.refresh_migration(&reviewed).await;
    assert_eq!(reviewed.members.len(), 2);
    assert!(
        reviewed
            .members
            .iter()
            .map(|member| &member.id)
            .eq(draft.members.iter().map(|member| &member.id)),
        "fresh migration review must preserve the original identities after concurrent edits and admission are explicitly resolved"
    );
    drop(legacy_client);
    scenario
        .send(SwarmCommandPayload::ApplyMigration {
            draft_id: reviewed.id.clone(),
            expected_revision: reviewed.revision,
        })
        .await;
    let launched_event: SwarmNotifyPayload = scenario
        .wait(
            FrameKind::SwarmNotify,
            "explicit conversion apply",
            |event: &SwarmNotifyPayload| event.swarm.source_draft_id.as_ref() == Some(&reviewed.id),
        )
        .await;
    let converted = scenario.swarm(&launched_event.swarm.id, ready).await;
    assert!(
        converted.legacy_team_id.as_ref() == Some(&team.id),
        "conversion must encode legacy ownership explicitly"
    );
    let migrated_manager = converted
        .members
        .iter()
        .find(|member| member.spec.id.0 == manager.id.0)
        .expect("stable migrated member identity");
    assert!(
        migrated_manager.session_id.as_ref() == Some(&original_session),
        "explicit conversion must reuse the existing provider session"
    );
    let resumed_selection = scenario
        .settings_for_agent(
            migrated_manager
                .agent_id
                .as_ref()
                .expect("migrated active binding"),
        )
        .await;
    assert!(
        resumed_selection.values == original_settings.values
            && resumed_selection.values == migrated_manager.spec.session_settings,
        "actual resumed activation must expose the original retained session selection, exactly matching the approved lineup"
    );
    let briefing = scenario
        .board(&converted.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        briefing.posts.len(),
        1,
        "legacy private transcript must never be reconstructed as shared posts"
    );
    assert!(briefing.posts[0].body == vec![text(&reviewed.opening_brief)]);
    assert!(briefing.posts[0].author == SwarmAuthor::Human);
    let coordination = scenario
        .board(&converted.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(
        coordination.posts.is_empty(),
        "conversion begins with an empty Coordination board"
    );
    scenario
        .send(SwarmCommandPayload::PreviewMigration {
            team_id: team.id.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    let reconnect = scenario.snapshot(&converted.id).await;
    assert!(
        reconnect.legacy_team_id == converted.legacy_team_id
            && reconnect.members == converted.members,
        "reconnect must retain explicit conversion and session ownership"
    );

    let live_agents = scenario.fixture.agent_ids().await;
    assert_eq!(live_agents.len(), 2);
    let legacy_path = scenario.fixture.store_dir().join("agent_teams.json");
    let preserved_legacy =
        std::fs::read(&legacy_path).expect("read preserved converted legacy records");
    scenario
        .fixture
        .client
        .team_member_activate(protocol::TeamMemberActivatePayload {
            member_id: report.id.clone(),
            prompt: Some(
                "Independent legacy activation must be refused after conversion".to_owned(),
            ),
            images: None,
        })
        .await
        .expect("attempt converted report activation over protocol");
    scenario
        .command_conflict(FrameKind::TeamMemberActivate)
        .await;
    scenario
        .fixture
        .client
        .team_member_activate(protocol::TeamMemberActivatePayload {
            member_id: manager.id.clone(),
            prompt: Some("The original team no longer owns this session".to_owned()),
            images: None,
        })
        .await
        .expect("attempt converted manager activation over protocol");
    scenario
        .command_conflict(FrameKind::TeamMemberActivate)
        .await;
    scenario
        .fixture
        .client
        .team_member_create(protocol::TeamMemberCreatePayload {
            team_id: team.id.clone(),
            member: legacy_spec("Unreviewed extra legacy member"),
            session_id: None,
        })
        .await
        .expect("attempt converted team member creation over protocol");
    scenario.command_conflict(FrameKind::TeamMemberCreate).await;
    scenario
        .fixture
        .client
        .team_member_update(protocol::TeamMemberUpdatePayload {
            id: report.id.clone(),
            name: "Unreviewed legacy member edit".to_owned(),
            // Router validation precedes the conversion ownership guard;
            // a nonempty description is required to exercise that Conflict.
            description: report.description.clone(),
            profile: None,
            project_ids: vec![project_id],
        })
        .await
        .expect("attempt converted member mutation over protocol");
    scenario.command_conflict(FrameKind::TeamMemberUpdate).await;
    scenario
        .fixture
        .client
        .team_set_manager(protocol::TeamSetManagerPayload {
            team_id: team.id.clone(),
            new_manager_member_id: report.id.clone(),
        })
        .await
        .expect("attempt converted team manager mutation over protocol");
    scenario.command_conflict(FrameKind::TeamSetManager).await;
    scenario
        .fixture
        .client
        .team_rename(protocol::TeamRenamePayload {
            id: team.id.clone(),
            name: "Unreviewed converted team rename".to_owned(),
        })
        .await
        .expect("attempt converted team rename over protocol");
    scenario.command_conflict(FrameKind::TeamRename).await;
    scenario
        .fixture
        .client
        .team_member_delete(protocol::TeamMemberDeletePayload {
            id: report.id.clone(),
        })
        .await
        .expect("attempt converted member deletion over protocol");
    scenario.command_conflict(FrameKind::TeamMemberDelete).await;
    scenario
        .fixture
        .client
        .team_delete(protocol::TeamDeletePayload {
            id: team.id.clone(),
        })
        .await
        .expect("attempt converted team deletion over protocol");
    scenario.command_conflict(FrameKind::TeamDelete).await;
    let live_after_legacy_commands = scenario.fixture.agent_ids().await;
    assert!(
        live_after_legacy_commands.len() == live_agents.len()
            && live_after_legacy_commands
                .iter()
                .all(|agent| live_agents.contains(agent)),
        "converted legacy activation must not create any extra active agent outside swarm capacity"
    );
    assert!(
        std::fs::read(&legacy_path)
            .expect("read historical legacy records after rejected mutations")
            == preserved_legacy,
        "explicit conversion must freeze rather than mutate or delete preserved legacy history"
    );
    let (_, frozen_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    assert!(
        !frozen_bootstrap
            .teams
            .iter()
            .any(|stored| stored.id == team.id)
            && !frozen_bootstrap
                .team_members
                .iter()
                .any(|stored| stored.team_id == team.id),
        "reconnect must not advertise converted records as a second live team"
    );
    assert!(
        !frozen_bootstrap
            .team_member_bindings
            .iter()
            .any(
                |binding| (binding.member_id == manager.id || binding.member_id == report.id)
                    && binding.current_agent_id.is_some()
            ),
        "converted legacy records cannot retain or create active legacy bindings"
    );
    let owned = frozen_bootstrap
        .swarms
        .iter()
        .find(|stored| stored.id == converted.id)
        .expect("converted swarm remains visible");
    assert!(
        owned == &converted,
        "legacy mutation rejection cannot change reviewed swarm ownership, sessions or capacity"
    );
    let unchanged_brief = scenario
        .board(&converted.id, read_board(SwarmBoard::Briefing))
        .await;
    assert!(
        unchanged_brief.posts == briefing.posts,
        "rejected legacy prompts must not become shared publications"
    );

    assert!(
        converted.members.iter().all(|member| member.spec.pinned),
        "conversion must begin with explicitly reviewed pinned legacy identities"
    );
    scenario
        .send(SwarmCommandPayload::PreviewChange {
            swarm_id: converted.id.clone(),
            expected_revision: converted.revision,
            constraints: scenario.constraints(1),
        })
        .await;
    let converted_reduction = scenario
        .swarm(&converted.id, |state| state.change_preview.is_some())
        .await;
    let converted_preview = converted_reduction
        .change_preview
        .expect("reviewed converted capacity reduction");
    assert!(
        converted_preview.conflicts.is_empty(),
        "draft migration pins cannot permanently veto an explicit live retirement approval"
    );
    assert!(
        converted_preview.retained == vec![migrated_manager.spec.id.clone()]
            && converted_preview.retirements == vec![protocol::SwarmMemberId(report.id.0.clone())]
            && converted_preview.additions.is_empty(),
        "reviewed converted capacity reduction must name the exact retained and retired identities"
    );
    scenario
        .send(SwarmCommandPayload::ApplyChange {
            swarm_id: converted.id.clone(),
            preview_revision: converted_preview.revision,
            retirement: SwarmRetirementPolicy::FinishTurn,
        })
        .await;
    let reduced_conversion = scenario
        .swarm(&converted.id, |state| {
            state.constraints.max_live_agents == 1
                && state.lifecycle == SwarmLifecycle::Running
                && state
                    .members
                    .iter()
                    .filter(|member| {
                        matches!(
                            member.state,
                            SwarmMemberState::Reserved
                                | SwarmMemberState::Live
                                | SwarmMemberState::Retiring
                        )
                    })
                    .count()
                    == 1
                && state.members.iter().any(|member| {
                    member.spec.id.0 == report.id.0 && member.state == SwarmMemberState::Retired
                })
        })
        .await;
    let retained_conversion = reduced_conversion
        .members
        .iter()
        .find(|member| member.spec.id == migrated_manager.spec.id)
        .expect("retained converted identity");
    assert!(
        retained_conversion.spec == migrated_manager.spec
            && retained_conversion.session_id == migrated_manager.session_id
            && retained_conversion.agent_id == migrated_manager.agent_id,
        "reducing a converted pinned swarm must preserve the retained reviewed identity and existing session/runtime"
    );
    assert_eq!(
        scenario.fixture.agent_ids().await.len(),
        1,
        "converted pins cannot strand an extra active agent above the newly approved capacity"
    );
}

#[tokio::test]
async fn interrupted_transport_acceptance_requires_explicit_notification_retry_after_restart() {
    let mut scenario = Scenario::new().await;
    let draft = scenario.generate(scenario.constraints(1)).await;
    let send_gate = MockGateHandle::new();
    let reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &draft.members[0].name,
            MockScript::one(MockTurn::text("Initial activation completed"))
                .with_unbounded_echo()
                .with_send_gate(&send_gate),
        )
        .await;
    let starting = scenario.launch(&draft).await;
    let live = scenario.swarm(&starting.id, ready).await;
    drop(reservation);
    let member = &live.members[0];
    let original_session = member.session_id.clone().expect("existing durable session");
    let original_control = scenario
        .fixture
        .mock_by_id(member.agent_id.as_ref().expect("live activation"))
        .await;
    let uncertain_publication = publication(
        SwarmBoard::Briefing,
        "uncertain-publication",
        vec![text("Delivery must not be silently repeated after a crash")],
    );
    let post = scenario.post(&live.id, uncertain_publication.clone()).await;
    send_gate.wait_until_entered().await;
    let dispatching = scenario
        .swarm(&live.id, |state| {
            state.notifications.iter().any(|notification| {
                notification.post_ids.contains(&post.id)
                    && notification.state == SwarmDeliveryState::Dispatching
            })
        })
        .await;
    let notification = dispatching
        .notifications
        .iter()
        .find(|notification| notification.post_ids.contains(&post.id))
        .expect("atomic persisted dispatch intent")
        .clone();
    assert_eq!(
        request_count(&original_control.requests().await),
        1,
        "gate must stop the follow-up before backend transport acceptance"
    );
    assert!(
        dispatching.members[0].context_cursor < post.cursor,
        "pending acceptance must not advance the context delivery cursor"
    );
    let restarted = scenario.fixture.relaunch_host_after_kill().await;
    scenario.pending.clear();
    let recovered = restarted
        .swarms
        .iter()
        .find(|state| state.id == live.id)
        .expect("recovered uncertain swarm");
    assert_eq!(recovered.lifecycle, SwarmLifecycle::AttentionRequired);
    let uncertain = recovered
        .notifications
        .iter()
        .find(|intent| intent.id == notification.id)
        .expect("durable uncertain notification");
    assert_eq!(
        uncertain.state,
        SwarmDeliveryState::Uncertain,
        "restart cannot claim a dispatch was accepted or blindly dispatch it again"
    );
    assert!(uncertain.error.is_some());
    assert!(recovered.members[0].session_id.as_ref() == Some(&original_session));
    assert!(recovered.members[0].agent_id.is_none());
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: live.id.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    let repeated = scenario.post(&live.id, uncertain_publication).await;
    assert!(
        repeated == post,
        "publication retry is not permission to retry uncertain execution"
    );
    let still_uncertain = scenario.snapshot(&live.id).await;
    assert!(still_uncertain.notifications.iter().any(
        |intent| intent.id == notification.id && intent.state == SwarmDeliveryState::Uncertain
    ));
    scenario
        .send(SwarmCommandPayload::RetryNotification {
            swarm_id: live.id.clone(),
            notification_id: protocol::SwarmNotificationId("foreign-notification".to_owned()),
        })
        .await;
    scenario.error(SwarmErrorCode::NotFound).await;
    let accepted_opening = still_uncertain
        .notifications
        .iter()
        .find(|intent| intent.state == SwarmDeliveryState::Accepted)
        .expect("opening delivery was already accepted");
    scenario
        .send(SwarmCommandPayload::RetryNotification {
            swarm_id: live.id.clone(),
            notification_id: accepted_opening.id.clone(),
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    scenario
        .send(SwarmCommandPayload::RetryNotification {
            swarm_id: live.id.clone(),
            notification_id: notification.id.clone(),
        })
        .await;
    let retry_authorized = scenario
        .swarm(&live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == notification.id && intent.state == SwarmDeliveryState::Pending
            })
        })
        .await;
    assert_eq!(
        retry_authorized.lifecycle,
        SwarmLifecycle::AttentionRequired,
        "delivery retry must not silently resume an attention-required swarm"
    );
    assert!(retry_authorized.members[0].agent_id.is_none());
    let finish_gate = MockGateHandle::new();
    let resumed_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &member.spec.name,
            MockScript::one(MockTurn::gated_text(
                "Explicitly retried context turn",
                &finish_gate,
            )),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: live.id.clone(),
        })
        .await;
    finish_gate.wait_until_entered().await;
    let accepted = scenario
        .swarm(&live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == notification.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert!(
        accepted.members[0].session_id.as_ref() == Some(&original_session),
        "notification retry must reuse the existing member session"
    );
    assert_eq!(
        accepted
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&post.id))
            .count(),
        1,
        "explicit retry must update the durable intent, not replace it with a new one"
    );
    let resumed_control = scenario
        .fixture
        .mock_by_id(
            accepted.members[0]
                .agent_id
                .as_ref()
                .expect("retried activation"),
        )
        .await;
    assert_eq!(request_count(&resumed_control.requests().await), 1);
    finish_gate.release_one();
    scenario
        .swarm(&live.id, |state| {
            ready(state) && state.members[0].context_cursor >= post.cursor
        })
        .await;
    drop(resumed_reservation);
    let board = scenario
        .board(&live.id, read_board(SwarmBoard::Briefing))
        .await;
    assert_eq!(
        board.posts.len(),
        2,
        "publication and notification recovery cannot duplicate durable posts"
    );
    assert!(board.posts[1].id == post.id);
    assert!(resumed_control.violations().await.is_empty());

    scenario.pause(&live.id).await;
    let acknowledgment_draft = scenario.generate(scenario.constraints(1)).await;
    let native_send_gate = MockGateHandle::new();
    let acknowledgment_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &acknowledgment_draft.members[0].name,
            MockScript::one(MockTurn::text("Native admission positive control"))
                // A completion gate cannot service native Interrupt. Held
                // remains active but interruptible after the SEND receipt.
                .then(MockTurn::held_text(
                    "Already-enqueued input remains admitted",
                ))
                .with_send_gate(&native_send_gate),
        )
        .await;
    let acknowledgment_launch = scenario.launch(&acknowledgment_draft).await;
    let acknowledgment_live = scenario.swarm(&acknowledgment_launch.id, ready).await;
    let acknowledgment_control = controls(&scenario, &acknowledgment_live).await.remove(0);
    let (mut lifecycle_peer, lifecycle_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    let acknowledgment_stream = lifecycle_bootstrap
        .agents
        .iter()
        .find(|agent| Some(&agent.agent_id) == acknowledgment_live.members[0].agent_id.as_ref())
        .expect("actual admitted runtime for cancellation observation")
        .instance_stream
        .clone();
    let admitted_post = scenario
        .post(
            &acknowledgment_live.id,
            publication(
                SwarmBoard::Briefing,
                "checked-enqueue-before-pause",
                vec![text("Pause follows this already-enqueued native input")],
            ),
        )
        .await;
    scenario
        .wait_mock_turn(
            &native_send_gate,
            "native acceptance held after checked mailbox enqueue",
        )
        .await;
    let waiting_ack = scenario
        .swarm(&acknowledgment_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&admitted_post.id)
                    && intent.state == SwarmDeliveryState::Dispatching
            })
        })
        .await;
    assert_eq!(
        request_count(&acknowledgment_control.requests().await),
        1,
        "actual native SEND gate must precede backend acceptance, not merely hold model completion"
    );
    lifecycle_peer
        .list_sessions(protocol::ListSessionsPayload::default())
        .await
        .expect("unrelated host read while native acknowledgment held");
    let session_read = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = client_event(
                &mut lifecycle_peer,
                "unrelated host progress during native send",
            )
            .await;
            assert!(
                !matches!(
                    event.kind,
                    FrameKind::CommandError | FrameKind::SwarmErrorNotify | FrameKind::AgentError
                ),
                "unrelated host read failed while native acceptance was held: {:?}",
                event.kind
            );
            if event.kind == FrameKind::SessionList {
                return event
                    .parse_payload::<protocol::SessionListPayload>()
                    .expect("actual host session list");
            }
        }
    })
    .await
    .expect("global host operations must not wait on backend acceptance");
    assert!(
        session_read
            .sessions
            .iter()
            .any(|session| Some(&session.id) == acknowledgment_live.members[0].session_id.as_ref()),
        "unrelated session listing must read the actual retained native session"
    );
    lifecycle_peer
        .swarm_command(SwarmCommandPayload::Pause {
            swarm_id: acknowledgment_live.id.clone(),
        })
        .await
        .expect("Pause after checked enqueue while native acknowledgment held");
    let pausing_ack = scenario
        .swarm(&acknowledgment_live.id, |state| {
            state.lifecycle == SwarmLifecycle::Pausing
        })
        .await;
    assert_eq!(request_count(&acknowledgment_control.requests().await), 1);
    assert!(
        pausing_ack.members[0].context_cursor == acknowledgment_live.members[0].context_cursor
            && pausing_ack
                .notifications
                .iter()
                .any(|intent| intent.post_ids.contains(&admitted_post.id)
                    && intent.state == SwarmDeliveryState::Dispatching),
        "Pausing can commit without holding the global lock, but cannot claim acknowledgment or completed cancellation while native SEND is held"
    );
    // Checked mailbox enqueue is admission's linearization point. This input
    // preceded Pause, so its later receipt is allowed; FIFO cancellation must
    // settle before Paused, unlike a preparation gate revoked before enqueue.
    native_send_gate.release_one();
    let paused_ack = scenario
        .swarm(&acknowledgment_live.id, |state| {
            state.lifecycle == SwarmLifecycle::Paused
        })
        .await;
    assert_eq!(
        request_count(&acknowledgment_control.requests().await),
        2,
        "only the already-admitted follow-up may cross native acceptance after committed Pausing"
    );
    assert!(
        acknowledgment_control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt)),
        "Pause must actually interrupt the already-admitted native turn before declaring Paused"
    );
    assert!(
        paused_ack.members[0].runtime_status == Some(AgentControlStatus::Idle)
            && paused_ack.members[0].session_id == acknowledgment_live.members[0].session_id
            && paused_ack.members[0].context_cursor >= admitted_post.cursor
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event =
                client_event(&mut lifecycle_peer, "already-admitted native cancellation").await;
            if event.stream == acknowledgment_stream
                && event.kind == FrameKind::ChatEvent
                && matches!(
                    event
                        .parse_payload::<protocol::ChatEvent>()
                        .expect("parse actual admitted turn activity"),
                    protocol::ChatEvent::TypingStatusChanged(false)
                )
            {
                return;
            }
        }
    })
    .await
    .expect("actual interrupted held turn must emit false typing, not just a swarm Pause label");
    let admitted_intent = waiting_ack
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&admitted_post.id))
        .expect("already-enqueued original intent");
    assert!(
        paused_ack
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&admitted_post.id))
            .eq([paused_ack
                .notifications
                .iter()
                .find(|intent| intent.id == admitted_intent.id
                    && intent.state == SwarmDeliveryState::Accepted)
                .expect("original transport receipt accepted once")]),
        "Pause after enqueue must settle the exact original intent once, without a new wake or duplicate cause"
    );
    assert!(acknowledgment_control.violations().await.is_empty());
    drop(lifecycle_peer);
    drop(acknowledgment_reservation);

    let busy_draft = scenario.generate(scenario.constraints(1)).await;
    let busy_send_gate = MockGateHandle::new();
    let deferred_retry_gate = MockGateHandle::new();
    let busy_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &busy_draft.members[0].name,
            MockScript::one(MockTurn::text(
                "Idle before native self-started busy response",
            ))
            .then(MockTurn::held_text("Native self-started busy turn"))
            .then(MockTurn::gated_text(
                "Explicit resumed original deferred input",
                &deferred_retry_gate,
            ))
            .with_send_gate(&busy_send_gate),
        )
        .await;
    let busy_launch = scenario.launch(&busy_draft).await;
    let busy_live = scenario.swarm(&busy_launch.id, ready).await;
    let busy_control = controls(&scenario, &busy_live).await.remove(0);
    let busy_post = scenario
        .post(
            &busy_live.id,
            publication(
                SwarmBoard::Briefing,
                "native-busy-refused-intent",
                vec![text(
                    "This exact intent must remain pending, not enter an ordinary input queue",
                )],
            ),
        )
        .await;
    scenario
        .wait_mock_turn(
            &busy_send_gate,
            "IdleOnly native send before self-started Busy",
        )
        .await;
    busy_send_gate.release_one_busy();
    let deferred_busy = scenario
        .swarm(&busy_live.id, |state| {
            state.members[0].state == SwarmMemberState::Live
                && state.members[0].runtime_status == Some(AgentControlStatus::Thinking)
                && state.notifications.iter().any(|intent| {
                    intent.post_ids.contains(&busy_post.id)
                        && intent.state == SwarmDeliveryState::Pending
                })
        })
        .await;
    let deferred_intent = deferred_busy
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&busy_post.id))
        .expect("original busy-refused intent")
        .clone();
    assert_eq!(
        request_count(&busy_control.requests().await),
        1,
        "self-started Busy is refusal, not native acceptance of the swarm message"
    );
    assert_eq!(
        deferred_busy.members[0].context_cursor,
        busy_live.members[0].context_cursor
    );
    let (mut queue_reader, queue_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    let busy_stream = queue_bootstrap
        .agents
        .iter()
        .find(|agent| Some(&agent.agent_id) == busy_live.members[0].agent_id.as_ref())
        .expect("actual busy runtime for queue inspection")
        .instance_stream
        .clone();
    let queued = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event =
                client_event(&mut queue_reader, "native Busy ordinary-queue exclusion").await;
            if event.kind == FrameKind::AgentBootstrap && event.stream == busy_stream {
                let agent: protocol::AgentBootstrapPayload =
                    event.parse_payload().expect("actual busy agent bootstrap");
                return agent
                    .events
                    .into_iter()
                    .find_map(|event| match event {
                        protocol::AgentBootstrapEvent::QueuedMessages(queue) => Some(queue),
                        _ => None,
                    })
                    .expect("canonical ordinary queue snapshot");
            }
        }
    })
    .await
    .expect("busy runtime must expose its canonical queue");
    assert!(
        queued.messages.is_empty(),
        "IdleOnly Busy cannot create a hidden ordinary queued input that later bypasses swarm Pause and accounting"
    );
    drop(queue_reader);
    let paused_busy = scenario.pause(&busy_live.id).await;
    assert!(
        paused_busy
            .notifications
            .iter()
            .any(|intent| intent == &deferred_intent)
            && paused_busy.members[0].context_cursor == busy_live.members[0].context_cursor,
        "actual self-started native cancellation must leave the exact refused notification Pending and context unadvanced"
    );
    assert_eq!(
        request_count(&busy_control.requests().await),
        1,
        "ending the self-started native turn during Pause must not drain an unaccounted ordinary input"
    );
    assert!(
        busy_control
            .requests()
            .await
            .iter()
            .any(|request| matches!(request, MockRequest::Interrupt)),
        "Pause must interrupt the held provider-owned Busy turn, not release normal text completion"
    );
    assert_eq!(
        paused_busy.members[0].runtime_status,
        Some(AgentControlStatus::Idle)
    );
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: busy_live.id.clone(),
        })
        .await;
    scenario
        .wait_mock_turn(
            &busy_send_gate,
            "explicit Resume of original native Busy refusal",
        )
        .await;
    busy_send_gate.release_one();
    scenario
        .wait_mock_turn(
            &deferred_retry_gate,
            "one accounted native retry after Busy refusal",
        )
        .await;
    let busy_accepted = scenario
        .swarm(&busy_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == deferred_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert_eq!(request_count(&busy_control.requests().await), 2);
    let (busy_references, _) = last_dispatch_context(&busy_control.requests().await);
    assert!(busy_references == vec![busy_post.id.clone()]);
    assert_eq!(
        busy_accepted
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&busy_post.id))
            .count(),
        1,
        "coalesced wake signals must retain the refused cause and accept it once after explicit Resume"
    );
    deferred_retry_gate.release_one();
    let busy_finished = scenario.swarm(&busy_live.id, ready).await;
    assert_eq!(request_count(&busy_control.requests().await), 2);

    let natural_reply_gate = MockGateHandle::new();
    busy_control
        .enqueue_all([
            MockTurn::text("Native self-started turn ends without human intervention"),
            MockTurn::gated_text(
                "Automatic acceptance of the original pending cause",
                &natural_reply_gate,
            ),
        ])
        .await;
    let interrupts_before_natural = busy_control
        .requests()
        .await
        .iter()
        .filter(|request| matches!(request, MockRequest::Interrupt))
        .count();
    let natural_post = scenario.post(&busy_live.id, publication(SwarmBoard::Briefing,
        "native-busy-natural-completion", vec![text("The original deferred intent must wake automatically when the native self-turn finishes")])).await;
    scenario
        .wait_mock_turn(
            &busy_send_gate,
            "native Busy before natural self-turn completion",
        )
        .await;
    assert_eq!(request_count(&busy_control.requests().await), 2);
    // Do not wait for a Thinking projection: the short self-turn is allowed to
    // coalesce Thinking->Idle at the status watcher. Owned activity must still
    // schedule dispatch even when its final canonical status is unchanged.
    busy_send_gate.release_one_busy();
    let naturally_deferred = scenario
        .swarm(&busy_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&natural_post.id)
                    && intent.state == SwarmDeliveryState::Pending
            })
        })
        .await;
    let natural_intent = naturally_deferred
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&natural_post.id))
        .expect("original naturally deferred intent")
        .clone();
    assert!(
        naturally_deferred.lifecycle == SwarmLifecycle::Running
            && naturally_deferred.members[0].context_cursor
                == busy_finished.members[0].context_cursor
            && naturally_deferred.members[0].agent_id == busy_finished.members[0].agent_id
            && naturally_deferred.members[0].session_id == busy_finished.members[0].session_id,
        "native Busy keeps the exact original intent pending without advancing or replacing its retained runtime/session"
    );
    scenario
        .wait_mock_turn(
            &busy_send_gate,
            "automatic dispatch after natural native self-turn completion without Pause or Resume",
        )
        .await;
    assert_eq!(
        request_count(&busy_control.requests().await),
        2,
        "the automatic retry still waits for actual native acceptance"
    );
    busy_send_gate.release_one();
    scenario
        .wait_mock_turn(
            &natural_reply_gate,
            "one native input automatically accepted after natural Busy completion",
        )
        .await;
    let naturally_accepted = scenario
        .swarm(&busy_live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == natural_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert_eq!(
        request_count(&busy_control.requests().await),
        3,
        "natural completion must automatically accept the pending input exactly once, without human retry or a lost wake"
    );
    let (natural_references, _) = last_dispatch_context(&busy_control.requests().await);
    assert!(natural_references == vec![natural_post.id.clone()]);
    assert_eq!(
        naturally_accepted
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&natural_post.id))
            .count(),
        1,
        "watch-channel coalescing cannot drop or duplicate the original durable notification"
    );
    assert!(
        naturally_accepted.members[0].agent_id == busy_finished.members[0].agent_id
            && naturally_accepted.members[0].session_id == busy_finished.members[0].session_id
    );
    natural_reply_gate.release_one();
    let natural_finished = scenario.swarm(&busy_live.id, ready).await;
    let natural_requests = busy_control.requests().await;
    assert_eq!(request_count(&natural_requests), 3);
    assert_eq!(
        natural_requests
            .iter()
            .filter(|request| matches!(request, MockRequest::Interrupt))
            .count(),
        interrupts_before_natural,
        "native self-turn completion and automatic pending-input dispatch must not require human Pause/Resume or an interrupt"
    );
    assert!(
        natural_finished
            .notifications
            .iter()
            .any(|intent| intent.id == natural_intent.id
                && intent.state == SwarmDeliveryState::Accepted)
            && natural_finished.members[0].context_cursor >= natural_post.cursor
    );
    assert!(busy_control.violations().await.is_empty());
    drop(busy_reservation);
    scenario.pause(&busy_live.id).await;

    let refusing = scenario.launched(1).await;
    let refusing_member = refusing.members[0].clone();
    let refusing_agent = refusing_member
        .agent_id
        .as_ref()
        .expect("live refusal runtime");
    let refusing_session = refusing_member
        .session_id
        .as_ref()
        .expect("live refusal session");
    let refusing_control = scenario.fixture.mock_by_id(refusing_agent).await;
    let before_refusal = request_count(&refusing_control.requests().await);
    assert_eq!(before_refusal, 1);
    let durable_idle = scenario
        .fixture
        .read_persisted_sessions()
        .into_iter()
        .find(|record| &record.id == refusing_session)
        .expect("actual idle session record");
    assert!(
        durable_idle.turn_recovery.is_none(),
        "completed native startup must have cleared its durable in-flight marker before refusal injection"
    );
    // A failed real SessionStore InFlight commit refuses acknowledged live
    // delivery before native input, but leaves the existing idle actor alive.
    let refused_commit = scenario.fixture.fail_next_session_commit();
    let refused_post = scenario
        .post(
            &refusing.id,
            publication(
                SwarmBoard::Briefing,
                "live-delivery-commit-refusal",
                vec![text(
                    "Retry this exact unaccepted notification after recovery",
                )],
            ),
        )
        .await;
    let failed_bound = scenario
        .swarm(&refusing.id, |state| {
            state.lifecycle == SwarmLifecycle::AttentionRequired
                && state.members[0].state == SwarmMemberState::Failed
                && state.notifications.iter().any(|intent| {
                    intent.post_ids.contains(&refused_post.id)
                        && intent.state == SwarmDeliveryState::Failed
                })
        })
        .await;
    drop(refused_commit);
    let refused_intent = failed_bound
        .notifications
        .iter()
        .find(|intent| intent.post_ids.contains(&refused_post.id))
        .expect("failed original live intent")
        .clone();
    assert!(
        failed_bound.members[0].agent_id.as_ref() == Some(refusing_agent)
            && failed_bound.members[0].session_id.as_ref() == Some(refusing_session)
            && failed_bound.members[0].runtime_status == Some(AgentControlStatus::Idle)
            && failed_bound.members[0].context_cursor == refusing_member.context_cursor
            && refused_intent.error.is_some(),
        "live completion refusal must retain its actual idle binding and unadvanced context; otherwise the restart regression is not exercised"
    );
    assert!(
        scenario.fixture.agent_ids().await.contains(refusing_agent),
        "the Failed binding must still occupy a real runtime before the crash"
    );
    assert_eq!(
        request_count(&refusing_control.requests().await),
        before_refusal,
        "durable admission rejection cannot deliver the refused body to the native backend"
    );
    eprintln!(
        "Swarm sim recovery phase: failed-live-binding-before-crash; failed intents=1, retained runtime=true"
    );
    let refusal_bootstrap = tokio::time::timeout(
        Duration::from_secs(5),
        scenario.fixture.relaunch_host_after_kill(),
    )
    .await
    .expect("failed live binding recovery must not strand host startup");
    scenario.pending.clear();
    assert!(
        refusal_bootstrap.agents.is_empty() && scenario.fixture.agent_ids().await.is_empty(),
        "restart must clear every failed/live runtime binding rather than restore an owned session as an ordinary capacity slot"
    );
    let recovered_refusal = refusal_bootstrap
        .swarms
        .iter()
        .find(|state| state.id == refusing.id)
        .expect("failed live intent remains visible after restart");
    assert!(
        recovered_refusal.members[0].state == SwarmMemberState::Failed
            && recovered_refusal.members[0].agent_id.is_none()
            && recovered_refusal.members[0].runtime_status.is_none()
            && recovered_refusal.members[0].session_id.as_ref() == Some(refusing_session)
            && recovered_refusal.members[0].context_cursor == refusing_member.context_cursor,
        "ALL runtime-only bindings must be cleared on load, including Failed members that retained a real actor before the crash"
    );
    assert!(
        recovered_refusal
            .notifications
            .iter()
            .any(|intent| intent == &refused_intent),
        "restart must retain the exact failed notification, not replace or silently accept it"
    );
    scenario
        .send(SwarmCommandPayload::RetryNotification {
            swarm_id: refusing.id.clone(),
            notification_id: refused_intent.id.clone(),
        })
        .await;
    let refused_retry = scenario
        .swarm(&refusing.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == refused_intent.id && intent.state == SwarmDeliveryState::Pending
            })
        })
        .await;
    assert_eq!(refused_retry.lifecycle, SwarmLifecycle::AttentionRequired);
    assert!(
        refused_retry.members[0].state == SwarmMemberState::Dormant
            && refused_retry.members[0].agent_id.is_none()
            && refused_retry.members[0].runtime_status.is_none()
            && scenario.fixture.agent_ids().await.is_empty(),
        "explicit retry makes a retained session dormant, not a phantom Live runtime that consumes its only capacity slot"
    );
    let retry_gate = MockGateHandle::new();
    let refused_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &refusing_member.spec.name,
            MockScript::one(MockTurn::gated_text(
                "Recovered refused native input",
                &retry_gate,
            )),
        )
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: refusing.id.clone(),
        })
        .await;
    let retry_accepted = scenario
        .swarm(&refusing.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.id == refused_intent.id && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    let retry_agent = retry_accepted.members[0]
        .agent_id
        .as_ref()
        .expect("one recovered retry runtime");
    scenario
        .wait_mock_turn(&retry_gate, "recovered refused live input")
        .await;
    let retry_control = scenario.fixture.mock_by_id(retry_agent).await;
    assert!(
        retry_accepted.members[0].session_id.as_ref() == Some(refusing_session)
            && retry_agent != refusing_agent
            && scenario.fixture.agent_ids().await == vec![retry_agent.clone()],
        "recovery must reuse the retained session in one fresh runtime without stale capacity or dual binding"
    );
    assert_eq!(request_count(&retry_control.requests().await), 1);
    let (retry_references, _) = last_dispatch_context(&retry_control.requests().await);
    assert!(
        retry_references == vec![refused_post.id.clone()],
        "retry must deliver only the exact original refused intent"
    );
    assert_eq!(
        retry_accepted
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&refused_post.id))
            .count(),
        1,
        "explicit retry updates the existing cause once, without duplicating intended delivery"
    );
    retry_gate.release_one();
    let refusal_finished = scenario.swarm(&refusing.id, ready).await;
    assert!(
        refusal_finished
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&refused_post.id))
            .all(|intent| intent.id == refused_intent.id
                && intent.state == SwarmDeliveryState::Accepted)
    );
    assert_eq!(request_count(&retry_control.requests().await), 1);
    assert!(retry_control.violations().await.is_empty());
    drop(refused_reservation);
    scenario.pause(&refusing.id).await;

    for session_persisted in [false, true] {
        let phase = if session_persisted {
            "owned-session-before-complete"
        } else {
            "owned-reservation-before-provider"
        };
        let boundary_draft = scenario.generate(scenario.constraints(1)).await;
        let startup_gate = if session_persisted {
            scenario
                .fixture
                .install_swarm_session_persistence_test_gate()
                .await
        } else {
            scenario
                .fixture
                .install_swarm_startup_reservation_test_gate()
                .await
        };
        let launch_reservation = scenario
            .fixture
            .reserve_next_mock_launch(
                &boundary_draft.members[0].name,
                MockScript::one(MockTurn::text("Uncompleted owned startup")),
            )
            .await;
        let boundary_launch = scenario.launch(&boundary_draft).await;
        eprintln!("Swarm sim startup phase begin: {phase}");
        tokio::time::timeout(Duration::from_secs(5), startup_gate.wait_until_entered())
            .await
            .expect("fresh owned startup must reach the selected real persistence boundary");
        let reserved = scenario
            .swarm(&boundary_launch.id, |state| {
                state.members[0].state == SwarmMemberState::Reserved
                    && state.members[0].agent_id.is_some()
            })
            .await;
        let opening_id = reserved
            .opening_post_id
            .as_ref()
            .expect("durable original opening cause");
        let original_intent = reserved
            .notifications
            .iter()
            .find(|intent| intent.post_ids.contains(opening_id))
            .expect("startup's exact durable dispatch intent")
            .clone();
        assert_eq!(original_intent.state, SwarmDeliveryState::Dispatching);
        assert!(
            reserved.members[0].session_id.is_none(),
            "the selected window must precede publication of the swarm session binding"
        );
        let at_boundary: protocol::SwarmStoreSnapshot = serde_json::from_slice(
            &std::fs::read(scenario.fixture.swarm_store_path())
                .expect("read actual gated startup snapshot"),
        )
        .expect("decode actual gated startup snapshot");
        let persisted_startup = at_boundary
            .swarms
            .iter()
            .find(|state| state.id == reserved.id)
            .expect("gated startup must be committed before a provider can start");
        assert!(
            persisted_startup.members[0].session_id.is_none()
                && persisted_startup.members[0].agent_id == reserved.members[0].agent_id
                && persisted_startup
                    .notifications
                    .iter()
                    .any(|intent| intent.id == original_intent.id
                        && intent.state == SwarmDeliveryState::Dispatching),
            "the actual held persistence window must precede canonical session mapping and acceptance, not merely reuse a buffered older state"
        );
        let membership = protocol::SwarmMembership {
            swarm_id: reserved.id.clone(),
            member_id: reserved.members[0].spec.id.clone(),
        };
        let persisted_owned = scenario
            .fixture
            .read_persisted_sessions()
            .into_iter()
            .filter(|record| record.swarm_membership.as_ref() == Some(&membership))
            .collect::<Vec<_>>();
        assert_eq!(
            persisted_owned.len(),
            usize::from(session_persisted),
            "the selected startup window must persist exact typed ownership in the FIRST session write, before publication"
        );
        let boundary_session = persisted_owned.first().map(|record| record.id.clone());
        let bootstrap = tokio::time::timeout(
            Duration::from_secs(5),
            scenario.fixture.relaunch_host_after_kill(),
        )
        .await
        .expect("killing a gated startup must not strand the replacement host");
        scenario.pending.clear();
        drop(startup_gate);
        drop(launch_reservation);
        eprintln!(
            "Swarm sim startup phase relaunched: {phase}; ordinary runtime count={}",
            bootstrap.agents.len()
        );
        assert!(
            bootstrap.agents.is_empty() && scenario.fixture.agent_ids().await.is_empty(),
            "owned startup reservations and newly persisted sessions cannot restore as ordinary agents"
        );
        assert!(
            bootstrap.agent_restoration_failures.is_empty(),
            "owned work belongs to explicit swarm recovery, not failed ordinary restoration"
        );
        let recovered = bootstrap
            .swarms
            .iter()
            .find(|state| state.id == reserved.id)
            .expect("interrupted owned startup must remain visible for human recovery");
        assert_eq!(recovered.lifecycle, SwarmLifecycle::AttentionRequired);
        assert_eq!(
            recovered.recovery_requirement,
            protocol::SwarmRecoveryRequirement::ExplicitResume
        );
        assert!(
            recovered.members[0].agent_id.is_none()
                && recovered.members[0].session_id == boundary_session
                && recovered.members[0].context_cursor == reserved.members[0].context_cursor
        );
        assert!(
            recovered
                .notifications
                .iter()
                .any(|intent| intent.id == original_intent.id
                    && intent.post_ids == original_intent.post_ids
                    && intent.state == SwarmDeliveryState::Uncertain)
        );
        let after_restore_owned = scenario
            .fixture
            .read_persisted_sessions()
            .into_iter()
            .filter(|record| record.swarm_membership.as_ref() == Some(&membership))
            .collect::<Vec<_>>();
        assert!(
            after_restore_owned
                .iter()
                .map(|record| &record.id)
                .eq(persisted_owned.iter().map(|record| &record.id)),
            "ordinary restoration cannot create another native session for interrupted owned work"
        );
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: reserved.id.clone(),
            })
            .await;
        scenario.error(SwarmErrorCode::Conflict).await;
        assert!(scenario.fixture.agent_ids().await.is_empty());
        scenario
            .send(SwarmCommandPayload::RetryNotification {
                swarm_id: reserved.id.clone(),
                notification_id: original_intent.id.clone(),
            })
            .await;
        let retry_required = scenario
            .swarm(&reserved.id, |state| {
                state.notifications.iter().any(|intent| {
                    intent.id == original_intent.id && intent.state == SwarmDeliveryState::Pending
                })
            })
            .await;
        assert_eq!(retry_required.lifecycle, SwarmLifecycle::AttentionRequired);
        assert_eq!(
            retry_required.recovery_requirement,
            protocol::SwarmRecoveryRequirement::ExplicitResume
        );
        let recovery_gate = MockGateHandle::new();
        let recovery_reservation = scenario
            .fixture
            .reserve_next_mock_launch(
                &boundary_draft.members[0].name,
                MockScript::one(MockTurn::gated_text(
                    "Explicit owned startup recovery",
                    &recovery_gate,
                )),
            )
            .await;
        scenario
            .send(SwarmCommandPayload::Resume {
                swarm_id: reserved.id.clone(),
            })
            .await;
        scenario.wait_mock_turn(&recovery_gate, phase).await;
        let accepted = scenario
            .swarm(&reserved.id, |state| {
                state.notifications.iter().any(|intent| {
                    intent.id == original_intent.id && intent.state == SwarmDeliveryState::Accepted
                })
            })
            .await;
        let recovered_agent = accepted.members[0]
            .agent_id
            .as_ref()
            .expect("explicitly recovered runtime");
        assert_eq!(scenario.fixture.agent_ids().await.len(), 1);
        if let Some(session) = boundary_session {
            assert!(accepted.members[0].session_id.as_ref() == Some(&session));
        }
        let start = scenario.start_for_agent(recovered_agent).await;
        assert!(
            start.swarm_membership.as_ref() == Some(&membership)
                && start.project_id.as_ref() == Some(&scenario.project.id)
                && start.parent_agent_id.is_none(),
            "explicit recovery must preserve canonical swarm ownership and project scope"
        );
        let native = scenario.fixture.mock_by_id(recovered_agent).await;
        assert_eq!(
            request_count(&native.requests().await),
            1,
            "explicit retry and Resume may deliver the original startup cause exactly once in the recovered runtime"
        );
        let (references, _) = last_dispatch_context(&native.requests().await);
        assert!(references == vec![opening_id.clone()]);
        let caller = scenario.fixture.agent_control_caller(recovered_agent).await;
        let described: SwarmDescribe =
            tool_value(&call_tool(&caller, "tyde_swarm_describe", json!({})).await);
        assert!(
            described.swarm.id == reserved.id && described.member_id == reserved.members[0].spec.id
        );
        tool_error(&call_tool(&caller, "tyde_workflow_targets", json!({})).await);
        tool_error(&call_tool(&caller, "tyde_list_agents", json!({})).await);
        recovery_gate.release_one();
        scenario.swarm(&reserved.id, ready).await;
        let persisted_brief = scenario
            .board(&reserved.id, read_board(SwarmBoard::Briefing))
            .await;
        assert_eq!(persisted_brief.posts.len(), 1);
        assert!(persisted_brief.posts[0].id == *opening_id);
        assert_eq!(
            scenario
                .snapshot(&reserved.id)
                .await
                .notifications
                .iter()
                .filter(|intent| intent.post_ids.contains(opening_id))
                .count(),
            1
        );
        assert!(native.violations().await.is_empty());
        scenario.pause(&reserved.id).await;
        drop(recovery_reservation);
    }
}

#[tokio::test]
async fn authenticated_peer_wakes_exhaust_one_causal_budget_without_losing_posts_or_resetting_on_board_changes()
 {
    let mut scenario = Scenario::new().await;
    let mut constraints = scenario.constraints(2);
    constraints.agent_wake_budget = 2;
    let draft = scenario.generate(constraints).await;
    let starting = scenario.launch(&draft).await;
    let live = scenario.swarm(&starting.id, ready).await;
    let mock_controls = controls(&scenario, &live).await;
    let first = &live.members[0];
    let second = &live.members[1];
    let first_caller = scenario
        .fixture
        .agent_control_caller(first.agent_id.as_ref().expect("first authenticated peer"))
        .await;
    let second_caller = scenario
        .fixture
        .agent_control_caller(second.agent_id.as_ref().expect("second authenticated peer"))
        .await;
    let opening = scenario
        .board(&live.id, read_board(SwarmBoard::Briefing))
        .await;
    let cause = opening.posts[0].round_id.clone();
    assert_eq!(live.rounds.len(), 1);
    assert_eq!(
        live.rounds[0].agent_activations_remaining, 2,
        "human-authorized initial member starts do not spend the agent-only wake allowance"
    );
    scenario.pause(&live.id).await;
    let coordination_publication = publication(
        SwarmBoard::Coordination,
        "same-round-coordination",
        vec![
            text("One useful review request"),
            SwarmBodySegment::MemberMention {
                member_id: second.spec.id.clone(),
            },
        ],
    );
    let coordination: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &first_caller,
            "tyde_swarm_post",
            serde_json::to_value(&coordination_publication).expect("serialize Coordination wake"),
        )
        .await,
    );
    let briefing: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &first_caller,
            "tyde_swarm_post",
            serde_json::to_value(publication(
                SwarmBoard::Briefing,
                "same-round-briefing",
                vec![
                    text("Related cross-board request in the same cause"),
                    SwarmBodySegment::PostLink {
                        post_id: coordination.post.id.clone(),
                    },
                    SwarmBodySegment::MemberMention {
                        member_id: second.spec.id.clone(),
                    },
                ],
            ))
            .expect("serialize Briefing wake"),
        )
        .await,
    );
    assert!(
        coordination.post.round_id == cause && briefing.post.round_id == cause,
        "board and thread changes cannot mint new agent causal rounds"
    );
    let duplicate: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &first_caller,
            "tyde_swarm_post",
            serde_json::to_value(coordination_publication)
                .expect("serialize idempotent wake retry"),
        )
        .await,
    );
    assert!(duplicate.duplicate && duplicate.post.id == coordination.post.id);
    let pending = scenario.snapshot(&live.id).await;
    assert_eq!(pending.rounds.len(), 1);
    assert_eq!(
        pending.rounds[0].agent_activations_remaining, 2,
        "publication alone must not spend activation allowance"
    );
    let same_cause_intents = pending
        .notifications
        .iter()
        .filter(|intent| {
            intent.post_ids.contains(&coordination.post.id)
                || intent.post_ids.contains(&briefing.post.id)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        same_cause_intents.len(),
        2,
        "retry must not duplicate a causal delivery intent"
    );
    assert!(
        same_cause_intents
            .iter()
            .all(|intent| intent.member_id == second.spec.id
                && intent.round_id == cause
                && intent.state == SwarmDeliveryState::Pending)
    );
    let first_wake_gate = MockGateHandle::new();
    mock_controls[1]
        .enqueue(MockTurn::gated_text(
            "First agent-triggered activation",
            &first_wake_gate,
        ))
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: live.id.clone(),
        })
        .await;
    first_wake_gate.wait_until_entered().await;
    let first_wake = scenario
        .swarm(&live.id, |state| {
            state
                .notifications
                .iter()
                .filter(|intent| {
                    intent.post_ids.contains(&coordination.post.id)
                        || intent.post_ids.contains(&briefing.post.id)
                })
                .all(|intent| intent.state == SwarmDeliveryState::Accepted)
                && state.members[1].runtime_status == Some(AgentControlStatus::Thinking)
        })
        .await;
    assert_eq!(first_wake.rounds.len(), 1);
    assert_eq!(
        first_wake.rounds[0].agent_activations_remaining, 1,
        "same-round coalescing charges one activation, not one per post and not zero"
    );
    for original_intent in &same_cause_intents {
        assert!(
            first_wake
                .notifications
                .iter()
                .any(|intent| intent.id == original_intent.id
                    && intent.state == SwarmDeliveryState::Accepted),
            "same-round coalescing must accept every original durable notification intent"
        );
    }
    assert!(first_wake.members[1].current_round_id.as_ref() == Some(&cause));
    assert_eq!(request_count(&mock_controls[0].requests().await), 1);
    let second_requests = mock_controls[1].requests().await;
    assert_eq!(
        request_count(&second_requests),
        2,
        "same-round pending posts must coalesce into one peer wake"
    );
    let (required_references, _) = last_dispatch_context(&second_requests);
    assert!(
        required_references == vec![coordination.post.id.clone(), briefing.post.id.clone()],
        "same-round coalescing must retain every durable cause in the explicit native notification-reference array"
    );
    assert!(
        second_requests.iter().any(|request| match request {
            MockRequest::Input(input) | MockRequest::Steer(input) =>
                input.message.contains(&coordination.post.id.0)
                    && input.message.contains(&briefing.post.id.0),
            _ => false,
        }),
        "same-round coalescing must retain both durable post references"
    );
    first_wake_gate.release_one();
    scenario
        .swarm(&live.id, |state| {
            ready(state) && state.members[1].context_cursor >= briefing.post.cursor
        })
        .await;

    let second_wake_gate = MockGateHandle::new();
    mock_controls[0]
        .enqueue(MockTurn::gated_text(
            "Second agent-triggered activation",
            &second_wake_gate,
        ))
        .await;
    let mut return_publication = publication(
        SwarmBoard::Briefing,
        "same-round-return",
        vec![
            text("Peer response continues the original causal round"),
            SwarmBodySegment::MemberMention {
                member_id: first.spec.id.clone(),
            },
        ],
    );
    return_publication.thread_id = Some(briefing.post.thread_id.clone());
    let return_post: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &second_caller,
            "tyde_swarm_post",
            serde_json::to_value(return_publication).expect("serialize return wake"),
        )
        .await,
    );
    assert!(
        return_post.post.round_id == cause,
        "peer response must inherit the actually delivered causal round"
    );
    second_wake_gate.wait_until_entered().await;
    let second_wake = scenario
        .swarm(&live.id, |state| {
            state.notifications.iter().any(|intent| {
                intent.post_ids.contains(&return_post.post.id)
                    && intent.state == SwarmDeliveryState::Accepted
            })
        })
        .await;
    assert_eq!(second_wake.rounds.len(), 1);
    assert_eq!(second_wake.rounds[0].agent_activations_remaining, 0);
    assert!(second_wake.members[0].current_round_id.as_ref() == Some(&cause));
    assert_eq!(request_count(&mock_controls[0].requests().await), 2);
    second_wake_gate.release_one();
    scenario
        .swarm(&live.id, |state| {
            ready(state) && state.members[0].context_cursor >= return_post.post.cursor
        })
        .await;

    let mut overflow_publication = publication(
        SwarmBoard::Coordination,
        "exhausted-thread-reply",
        vec![
            text("Durable reply after the automatic allowance is exhausted"),
            SwarmBodySegment::MemberMention {
                member_id: second.spec.id.clone(),
            },
        ],
    );
    overflow_publication.thread_id = Some(coordination.post.thread_id.clone());
    let overflow: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &first_caller,
            "tyde_swarm_post",
            serde_json::to_value(overflow_publication).expect("serialize exhausted wake"),
        )
        .await,
    );
    assert!(overflow.post.round_id == cause);
    let exhausted = scenario
        .swarm(&live.id, |state| {
            state.lifecycle == SwarmLifecycle::AttentionRequired
                && state.notifications.iter().any(|intent| {
                    intent.post_ids.contains(&overflow.post.id)
                        && intent.state == SwarmDeliveryState::Pending
                })
        })
        .await;
    assert!(
        exhausted.error.is_some(),
        "automatic loop exhaustion must explain why dispatch stopped"
    );
    assert_eq!(exhausted.rounds[0].agent_activations_remaining, 0);
    let cross_board_overflow: SwarmPublicationOutcome = tool_value(
        &call_tool(
            &second_caller,
            "tyde_swarm_post",
            serde_json::to_value(publication(
                SwarmBoard::Briefing,
                "exhausted-new-root",
                vec![
                    text("New root and other board must not reset the allowance"),
                    SwarmBodySegment::MemberMention {
                        member_id: first.spec.id.clone(),
                    },
                ],
            ))
            .expect("serialize cross-board exhausted wake"),
        )
        .await,
    );
    assert!(cross_board_overflow.post.round_id == cause);
    let human = scenario
        .post(
            &live.id,
            publication(
                SwarmBoard::Coordination,
                "fresh-human-context",
                vec![text(
                    "A new human cause does not silently resume attention-required execution",
                )],
            ),
        )
        .await;
    assert!(human.round_id != cause);
    let stopped = scenario.snapshot(&live.id).await;
    assert_eq!(stopped.lifecycle, SwarmLifecycle::AttentionRequired);
    for pending_post in [&overflow.post, &cross_board_overflow.post] {
        let intents = stopped
            .notifications
            .iter()
            .filter(|intent| intent.post_ids.contains(&pending_post.id))
            .collect::<Vec<_>>();
        assert_eq!(
            intents.len(),
            1,
            "loop-capped publications retain exactly one intended peer delivery"
        );
        assert_eq!(
            intents[0].state,
            SwarmDeliveryState::Pending,
            "exhausted delivery cannot be labeled accepted or silently removed"
        );
        assert!(intents[0].round_id == cause);
    }
    assert_eq!(
        stopped.rounds.len(),
        2,
        "only the explicit human publication creates a fresh cause"
    );
    assert_eq!(
        stopped
            .rounds
            .iter()
            .find(|round| round.id == cause)
            .expect("original cause")
            .agent_activations_remaining,
        0
    );
    assert_eq!(
        stopped
            .rounds
            .iter()
            .find(|round| round.id == human.round_id)
            .expect("human cause")
            .agent_activations_remaining,
        2
    );
    for control in &mock_controls {
        assert_eq!(
            request_count(&control.requests().await),
            2,
            "persisted posts, cross-board activity, and fresh human context cannot silently bypass the loop cap"
        );
        assert!(control.violations().await.is_empty());
    }
    let coordination_page = scenario
        .board(&live.id, read_board(SwarmBoard::Coordination))
        .await;
    assert!(
        coordination_page
            .posts
            .iter()
            .any(|post| post.id == overflow.post.id),
        "loop exhaustion must not drop the discussion that could not be dispatched"
    );
    let briefing_page = scenario
        .board(&live.id, read_board(SwarmBoard::Briefing))
        .await;
    assert!(
        briefing_page
            .posts
            .iter()
            .any(|post| post.id == cross_board_overflow.post.id)
    );

    let bootstrap = scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let recovered = bootstrap
        .swarms
        .iter()
        .find(|state| state.id == live.id)
        .expect("exhausted swarm restart snapshot");
    assert_eq!(recovered.lifecycle, SwarmLifecycle::AttentionRequired);
    assert!(
        recovered.rounds == stopped.rounds,
        "restart must not replenish the exhausted causal allowance"
    );
    assert!(
        recovered.notifications == stopped.notifications,
        "undispatched loop-cap intents remain durable across restart"
    );
    let gates = [MockGateHandle::new(), MockGateHandle::new()];
    let scripts = recovered
        .members
        .iter()
        .zip(&gates)
        .map(|(member, gate)| {
            (
                member.spec.name.clone(),
                MockScript::one(MockTurn::gated_text(
                    "Human-reauthorized bounded activation",
                    gate,
                )),
            )
        })
        .collect();
    let resumed_reservation = scenario.fixture.reserve_mock_launches(scripts).await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: live.id.clone(),
        })
        .await;
    for gate in &gates {
        gate.wait_until_entered().await;
    }
    let resumed = scenario
        .swarm(&live.id, |state| {
            state
                .notifications
                .iter()
                .filter(|intent| {
                    intent.post_ids.contains(&overflow.post.id)
                        || intent.post_ids.contains(&cross_board_overflow.post.id)
                })
                .all(|intent| intent.state == SwarmDeliveryState::Accepted)
                && state.members.iter().all(|member| {
                    member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Thinking)
                })
        })
        .await;
    assert_eq!(
        resumed
            .rounds
            .iter()
            .find(|round| round.id == cause)
            .expect("reauthorized original cause")
            .agent_activations_remaining,
        0,
        "explicit Resume restores only a finite allowance and both pending peer wakes consume it"
    );
    for original_intent in stopped.notifications.iter().filter(|intent| {
        intent.post_ids.contains(&overflow.post.id)
            || intent.post_ids.contains(&cross_board_overflow.post.id)
    }) {
        assert!(
            resumed
                .notifications
                .iter()
                .any(|intent| intent.id == original_intent.id
                    && intent.state == SwarmDeliveryState::Accepted),
            "human reauthorization updates the original intents without duplicating them"
        );
    }
    let (_, current_bootstrap) = scenario.fixture.connect_with_bootstrap().await;
    let current_swarm = current_bootstrap
        .swarms
        .iter()
        .find(|state| state.id == resumed.id)
        .expect("canonical loop recovery snapshot");
    eprintln!(
        "Swarm sim loop recovery native-control lookup; published_runtime_count={} lifecycle={:?}",
        current_bootstrap.agents.len(),
        current_swarm.lifecycle
    );
    for (index, member) in resumed.members.iter().enumerate() {
        let original = live
            .members
            .iter()
            .find(|old| old.spec.id == member.spec.id)
            .expect("stable member identity");
        assert!(member.session_id == original.session_id);
        let current_member = current_swarm
            .members
            .iter()
            .find(|current| current.spec.id == member.spec.id)
            .expect("canonical recovered member identity");
        eprintln!(
            "Swarm sim loop recovery member index={index}; state={:?} status={:?} binding_present={} same_observed_binding={} new_runtime_binding={} published_runtime_present={} session_retained={} error_present={}",
            current_member.state,
            current_member.runtime_status,
            current_member.agent_id.is_some(),
            current_member.agent_id == member.agent_id,
            member.agent_id != original.agent_id,
            current_bootstrap
                .agents
                .iter()
                .any(|agent| Some(&agent.agent_id) == member.agent_id.as_ref()),
            current_member.session_id == original.session_id,
            current_member.error.is_some()
        );
        let control = scenario
            .fixture
            .mock_by_id(
                member
                    .agent_id
                    .as_ref()
                    .expect("reauthorized runtime binding"),
            )
            .await;
        assert_eq!(request_count(&control.requests().await), 1);
        assert!(control.violations().await.is_empty());
    }
    for gate in &gates {
        gate.release_one();
    }
    scenario
        .swarm(&live.id, |state| {
            ready(state)
                && state
                    .members
                    .iter()
                    .all(|member| member.context_cursor >= human.cursor)
        })
        .await;
    drop(resumed_reservation);

    scenario.pause(&live.id).await;
    scenario
        .set_supervisor_setting("/supervisor/max_kicks_per_task", 1_u32, 3_u32)
        .await;
    let continue_marker = "__mock_supervisor_continue__";
    let ordinary_name = "Ordinary supervision positive control";
    let ordinary_start_gate = MockGateHandle::new();
    let ordinary_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            ordinary_name,
            MockScript::one(MockTurn::gated_text(
                "Ordinary task needs continuation",
                &ordinary_start_gate,
            ))
            .with_user_bubbles()
            .with_unbounded_echo(),
        )
        .await;
    scenario
        .fixture
        .client
        .spawn_agent(protocol::SpawnAgentPayload {
            name: Some(ordinary_name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id: Some(scenario.project.id.clone()),
            params: protocol::SpawnAgentParams::New {
                workspace_roots: scenario
                    .project
                    .root_paths()
                    .iter()
                    .map(|root| root.0.clone())
                    .collect(),
                prompt: continue_marker.to_owned(),
                images: None,
                backend_kind: protocol::BackendKind::Claude,
                launch_profile_id: Some(live.members[0].spec.launch_profile_id.clone()),
                cost_hint: None,
                access_mode: protocol::BackendAccessMode::ReadOnly,
                session_settings: None,
            },
        })
        .await
        .expect("spawn live ordinary supervisor control");
    let ordinary: protocol::NewAgentPayload = scenario
        .wait(
            FrameKind::NewAgent,
            "ordinary supervisor control startup",
            |agent: &protocol::NewAgentPayload| agent.name == ordinary_name,
        )
        .await;
    // NewAgent announces the actor before backend creation. The native first
    // turn and session-bearing AgentStart establish readiness before control.
    scenario
        .wait_mock_turn(&ordinary_start_gate, "ordinary supervisor native startup")
        .await;
    let ordinary_bootstrap: protocol::AgentBootstrapPayload = scenario
        .wait(
            FrameKind::AgentBootstrap,
            "ordinary supervisor session-bearing startup",
            |bootstrap: &protocol::AgentBootstrapPayload| {
                bootstrap.events.iter().any(|event| {
                    matches!(event,
                    protocol::AgentBootstrapEvent::AgentStart(start)
                        if start.agent_id == ordinary.agent_id && start.session_id.is_some())
                })
            },
        )
        .await;
    let ordinary_start = ordinary_bootstrap
        .events
        .into_iter()
        .find_map(|event| match event {
            protocol::AgentBootstrapEvent::AgentStart(start)
                if start.agent_id == ordinary.agent_id =>
            {
                Some(start)
            }
            _ => None,
        })
        .expect("actual session-bearing ordinary AgentStart inside native-ready bootstrap");
    assert!(ordinary_start.swarm_membership.is_none());
    let ordinary_control = scenario.fixture.mock_by_id(&ordinary.agent_id).await;
    ordinary_start_gate.release_one();
    drop(ordinary_reservation);
    let mut supervision_draft = scenario.generate(scenario.constraints(1)).await;
    scenario
        .send(SwarmCommandPayload::GenerateDraft {
            draft_id: supervision_draft.id.clone(),
            expected_revision: Some(supervision_draft.revision),
            name: supervision_draft.name.clone(),
            opening_brief: continue_marker.to_owned(),
            constraints: supervision_draft.constraints.clone(),
        })
        .await;
    supervision_draft = scenario
        .draft(&supervision_draft.id, supervision_draft.revision + 1)
        .await;
    let supervision_reservation = scenario
        .fixture
        .reserve_next_mock_launch(
            &supervision_draft.members[0].name,
            MockScript::one(MockTurn::text(
                "Swarm task needs canonical board authorization",
            ))
            .with_user_bubbles()
            .with_unbounded_echo(),
        )
        .await;
    let supervision_start = scenario.launch(&supervision_draft).await;
    let unsupervised = scenario.swarm(&supervision_start.id, ready).await;
    let member_control = controls(&scenario, &unsupervised).await.remove(0);
    drop(supervision_reservation);
    scenario
        .set_supervisor_setting("/supervisor/enabled", true, false)
        .await;
    // Match the existing supervisor suite's bounded quiet window (> its real
    // 3s debounce). Both tasks carry the identical Continue-ready marker; the
    // ordinary positive control proves the feature is actually enabled.
    scenario
        .observe_for(Duration::from_secs(8), "ordinary-continue-vs-owned-running")
        .await;
    let ordinary_requests = ordinary_control.requests().await;
    assert_eq!(
        request_count(&ordinary_requests),
        2,
        "ordinary agents must retain the actual autonomous supervised follow-up"
    );
    assert_eq!(
        ordinary_requests
            .iter()
            .filter(|request| matches!(request,
        MockRequest::Input(input) if input.origin == Some(protocol::MessageOrigin::Supervisor)))
            .count(),
        1
    );
    assert!(scenario.pending.iter().any(|envelope|
        envelope.kind == FrameKind::ChatEvent && envelope.stream == ordinary.instance_stream
            && matches!(envelope.parse_payload::<protocol::ChatEvent>().expect("parse visible ordinary supervision"),
                protocol::ChatEvent::MessageAdded(message) if matches!(message.sender, protocol::MessageSender::User)
                    && message.content.starts_with(protocol::SUPERVISOR_MESSAGE_PREFIX))),
        "the ordinary supervised continuation must also be visible over the real protocol");
    assert_eq!(
        request_count(&member_control.requests().await),
        1,
        "typed swarm ownership must suppress ordinary Continue admission even while Running"
    );
    let still_running = scenario.snapshot(&unsupervised.id).await;
    assert!(
        still_running.notifications == unsupervised.notifications
            && still_running.rounds == unsupervised.rounds
            && still_running.members[0].context_cursor == unsupervised.members[0].context_cursor
    );
    scenario.pause(&unsupervised.id).await;
    let paused_post = scenario
        .post(
            &unsupervised.id,
            publication(
                SwarmBoard::Coordination,
                "supervision-cannot-unpause",
                vec![
                    text(continue_marker),
                    SwarmBodySegment::MemberMention {
                        member_id: unsupervised.members[0].spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    scenario
        .set_supervisor_setting("/supervisor/enabled", false, true)
        .await;
    scenario
        .set_supervisor_setting("/supervisor/enabled", true, false)
        .await;
    let before_paused_supervision = scenario.snapshot(&unsupervised.id).await;
    scenario
        .observe_for(
            Duration::from_secs(8),
            "owned-paused-after-host-settings-edit",
        )
        .await;
    assert_eq!(request_count(&member_control.requests().await), 1);
    let after_paused_supervision = scenario.snapshot(&unsupervised.id).await;
    assert!(
        after_paused_supervision == before_paused_supervision,
        "ordinary host supervision settings cannot alter paused ownership, causal budgets or pending delivery"
    );
    assert!(
        after_paused_supervision
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&paused_post.id)
                && intent.state == SwarmDeliveryState::Pending)
    );
    // Admission can emit Accepted with the pre-turn Idle status before native
    // Thinking/Idle arrives. Establish the actual follow-up turn boundary so
    // the later equality assertion cannot compare an intermediate snapshot.
    let authorized_turn_gate = MockGateHandle::new();
    member_control
        .enqueue(MockTurn::gated_echo(&authorized_turn_gate))
        .await;
    scenario
        .send(SwarmCommandPayload::Resume {
            swarm_id: unsupervised.id.clone(),
        })
        .await;
    scenario
        .wait_mock_turn(
            &authorized_turn_gate,
            "authorized supervision control follow-up",
        )
        .await;
    scenario
        .swarm(&unsupervised.id, |state| {
            state.members[0].runtime_status == Some(AgentControlStatus::Thinking)
        })
        .await;
    authorized_turn_gate.release_one();
    let authorized_followup = scenario
        .swarm(&unsupervised.id, |state| {
            ready(state)
                && state.notifications.iter().any(|intent| {
                    intent.post_ids.contains(&paused_post.id)
                        && intent.state == SwarmDeliveryState::Accepted
                })
        })
        .await;
    assert_eq!(request_count(&member_control.requests().await), 2);
    scenario.fixture.fail_next_swarm_directory_sync().await;
    let uncertain_post = scenario
        .post(
            &unsupervised.id,
            publication(
                SwarmBoard::Coordination,
                "supervision-cannot-clear-attention",
                vec![
                    text(continue_marker),
                    SwarmBodySegment::MemberMention {
                        member_id: unsupervised.members[0].spec.id.clone(),
                    },
                ],
            ),
        )
        .await;
    scenario
        .error(SwarmErrorCode::CommittedDurabilityUncertain)
        .await;
    let before_attention_supervision = scenario
        .swarm(&unsupervised.id, |state| {
            state.lifecycle == SwarmLifecycle::AttentionRequired
        })
        .await;
    scenario
        .observe_for(
            Duration::from_secs(8),
            "owned-attention-after-authorized-followup",
        )
        .await;
    assert_eq!(
        request_count(&member_control.requests().await),
        2,
        "ordinary Continue cannot queue an unaccounted turn after swarm durability attention"
    );
    let after_attention_supervision = scenario.snapshot(&unsupervised.id).await;
    if after_attention_supervision != before_attention_supervision {
        eprintln!(
            "Swarm attention snapshot comparison: lifecycle_equal={} members_equal={} notifications_equal={} rounds_equal={} boards_equal={} revision_equal={} error_equal={} recovery_equal={}",
            after_attention_supervision.lifecycle == before_attention_supervision.lifecycle,
            after_attention_supervision.members == before_attention_supervision.members,
            after_attention_supervision.notifications == before_attention_supervision.notifications,
            after_attention_supervision.rounds == before_attention_supervision.rounds,
            after_attention_supervision.board_positions
                == before_attention_supervision.board_positions,
            after_attention_supervision.revision == before_attention_supervision.revision,
            after_attention_supervision.error == before_attention_supervision.error,
            after_attention_supervision.recovery_requirement
                == before_attention_supervision.recovery_requirement
        );
    }
    assert!(
        after_attention_supervision == before_attention_supervision
            && after_attention_supervision.members[0].context_cursor
                == authorized_followup.members[0].context_cursor
            && after_attention_supervision.recovery_requirement
                == protocol::SwarmRecoveryRequirement::ExplicitResume
    );
    assert!(
        after_attention_supervision
            .notifications
            .iter()
            .any(|intent| intent.post_ids.contains(&uncertain_post.id)
                && intent.state == SwarmDeliveryState::Pending)
    );
    assert_eq!(request_count(&ordinary_control.requests().await), 2);
    assert!(
        ordinary_control.violations().await.is_empty()
            && member_control.violations().await.is_empty()
    );
}

#[tokio::test]
async fn shared_images_are_durable_scoped_and_readable_by_authenticated_members() {
    use base64::Engine;
    use protocol::{
        ImageData, SwarmImage, SwarmImageId, SwarmImageNotifyPayload, SwarmImageOutcome,
        SwarmImageUpload,
    };

    async fn image_event(
        scenario: &mut Scenario,
        swarm: &SwarmId,
        id: &SwarmImageId,
    ) -> SwarmImageOutcome {
        let event: SwarmImageNotifyPayload = scenario
            .wait(
                FrameKind::SwarmImageNotify,
                "shared image outcome",
                |event: &SwarmImageNotifyPayload| event.swarm_id == *swarm && event.image_id == *id,
            )
            .await;
        event.outcome
    }
    async fn upload(
        scenario: &mut Scenario,
        swarm: &SwarmId,
        image: &SwarmImageUpload,
    ) -> SwarmImageOutcome {
        scenario
            .send(SwarmCommandPayload::UploadImage {
                swarm_id: swarm.clone(),
                image: image.clone(),
            })
            .await;
        image_event(scenario, swarm, &image.image_id).await
    }
    async fn read(
        scenario: &mut Scenario,
        swarm: &SwarmId,
        id: &SwarmImageId,
    ) -> SwarmImageOutcome {
        scenario
            .send(SwarmCommandPayload::ReadImage {
                swarm_id: swarm.clone(),
                image_id: id.clone(),
            })
            .await;
        image_event(scenario, swarm, id).await
    }
    fn rejected(outcome: SwarmImageOutcome, code: SwarmErrorCode) {
        let SwarmImageOutcome::Failed { error } = outcome else {
            panic!("invalid shared image must fail visibly")
        };
        assert_eq!(error.code, code);
        assert!(!error.message.is_empty());
    }

    let mut scenario = Scenario::new().await;
    let swarm = scenario.launched(1).await;
    scenario.pause(&swarm.id).await;
    let caller = scenario
        .fixture
        .agent_control_caller(
            swarm.members[0]
                .agent_id
                .as_ref()
                .expect("live authenticated member"),
        )
        .await;
    let mut uploads = Vec::new();
    let mut images = Vec::new();
    for (format, mime) in [
        (image::ImageFormat::Png, "image/png"),
        (image::ImageFormat::Jpeg, "image/jpeg"),
        (image::ImageFormat::Gif, "image/gif"),
        (image::ImageFormat::WebP, "image/webp"),
    ] {
        let pixels = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            12,
            image::Rgb([13, 180, 245]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels
            .write_to(&mut bytes, format)
            .expect("encode actual image fixture");
        let item = SwarmImageUpload {
            image_id: SwarmImageId(uuid::Uuid::new_v4().to_string()),
            name: format!("shared.{format:?}"),
            data: ImageData {
                media_type: mime.to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),
            },
        };
        if format == image::ImageFormat::Png {
            scenario.fixture.fail_next_swarm_directory_sync().await;
        }
        let SwarmImageOutcome::Ready { image, data: None } =
            upload(&mut scenario, &swarm.id, &item).await
        else {
            panic!("valid image upload must succeed")
        };
        assert!(image.width == 16 && image.height == 12 && image.byte_len > 0);
        assert_eq!(image.id, item.image_id);
        if format == image::ImageFormat::Png {
            scenario
                .error(SwarmErrorCode::CommittedDurabilityUncertain)
                .await;
            assert_eq!(
                scenario.snapshot(&swarm.id).await.recovery_requirement,
                protocol::SwarmRecoveryRequirement::ExplicitResume
            );
        }

        assert!(
            matches!(
                upload(&mut scenario, &swarm.id, &item).await,
                SwarmImageOutcome::Ready { data: None, .. }
            ),
            "same upload is idempotent"
        );
        assert!(
            matches!(read(&mut scenario, &swarm.id, &item.image_id).await, SwarmImageOutcome::Ready { image: loaded, data: Some(data) } if loaded == image && data == item.data)
        );
        swarm_tool_error(
            &call_tool(
                &caller,
                "tyde_swarm_read_image",
                json!({"image_id": item.image_id}),
            )
            .await,
            SwarmErrorCode::Unauthorized,
        );
        images.push(image);
        uploads.push(item);
    }
    let mut invalid = uploads[0].clone();
    invalid.name = "changed.png".into();
    rejected(
        upload(&mut scenario, &swarm.id, &invalid).await,
        SwarmErrorCode::Conflict,
    );
    for (id, mime, data) in [
        (
            "../escape".to_owned(),
            "image/png",
            uploads[0].data.data.clone(),
        ),
        (
            uuid::Uuid::new_v4().to_string(),
            "image/svg+xml",
            uploads[0].data.data.clone(),
        ),
        (
            uuid::Uuid::new_v4().to_string(),
            "image/jpeg",
            uploads[0].data.data.clone(),
        ),
        (
            uuid::Uuid::new_v4().to_string(),
            "image/png",
            "not base64".to_owned(),
        ),
        (
            uuid::Uuid::new_v4().to_string(),
            "image/png",
            base64::engine::general_purpose::STANDARD.encode(b"not image pixels"),
        ),
        (
            uuid::Uuid::new_v4().to_string(),
            "image/png",
            "A".repeat(protocol::SWARM_MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4),
        ),
    ] {
        rejected(
            upload(
                &mut scenario,
                &swarm.id,
                &SwarmImageUpload {
                    image_id: SwarmImageId(id),
                    name: "invalid.png".into(),
                    data: ImageData {
                        media_type: mime.into(),
                        data,
                    },
                },
            )
            .await,
            SwarmErrorCode::Invalid,
        );
    }
    let mut root = publication(SwarmBoard::Briefing, "image-only", Vec::new());
    root.images = images.iter().map(|image| image.id.clone()).collect();
    let posted = scenario.post(&swarm.id, root.clone()).await;
    assert!(posted.body.is_empty());
    assert_eq!(posted.images, images);
    let before = scenario.snapshot(&swarm.id).await.notifications.len();
    assert_eq!(scenario.post(&swarm.id, root.clone()).await, posted);
    assert_eq!(
        scenario
            .snapshot_after_commands(&swarm.id)
            .await
            .notifications
            .len(),
        before,
        "image post retry must not duplicate notification intents"
    );
    let mut conflict = root;
    conflict.images.reverse();
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: swarm.id.clone(),
            publication: conflict,
        })
        .await;
    scenario.error(SwarmErrorCode::Conflict).await;
    let mut reply = publication(
        SwarmBoard::Briefing,
        "image-reply",
        vec![text("More pictures")],
    );
    reply.thread_id = Some(posted.thread_id.clone());
    reply.images = vec![images[1].id.clone()];
    let replied = scenario.post(&swarm.id, reply).await;
    let thread = scenario
        .thread(
            &swarm.id,
            SwarmThreadRead {
                thread_id: posted.thread_id.clone(),
                after_cursor: None,
                limit: Some(50),
            },
        )
        .await;
    assert_eq!(thread.root, posted);
    assert!(thread.posts.contains(&replied));
    let mut coordination = publication(SwarmBoard::Coordination, "coordination-image", Vec::new());
    coordination.images = vec![images[2].id.clone()];
    let coord = scenario.post(&swarm.id, coordination).await;
    assert!(
        scenario
            .board(&swarm.id, read_board(SwarmBoard::Coordination))
            .await
            .posts
            .contains(&coord)
    );
    for (item, metadata) in uploads.iter().zip(&images) {
        let result = call_tool(
            &caller,
            "tyde_swarm_read_image",
            json!({"image_id": item.image_id}),
        )
        .await;
        assert_eq!(tool_value::<SwarmImage>(&result), *metadata);
        assert_eq!(
            result.content.len(),
            2,
            "MCP returns metadata and actual pixels, not just a filename"
        );
        let RawContent::Image(content) = &result.content[1].raw else {
            panic!("image tool must contain image content")
        };
        assert!(content.mime_type == item.data.media_type && content.data == item.data.data);
    }
    let other = scenario.launched(1).await;
    scenario.pause(&other.id).await;
    rejected(
        read(&mut scenario, &other.id, &images[0].id).await,
        SwarmErrorCode::NotFound,
    );
    rejected(
        upload(&mut scenario, &other.id, &uploads[0]).await,
        SwarmErrorCode::Conflict,
    );
    let mut foreign = publication(SwarmBoard::Briefing, "foreign-image", Vec::new());
    foreign.images = vec![images[0].id.clone()];
    scenario
        .send(SwarmCommandPayload::Post {
            swarm_id: other.id.clone(),
            publication: foreign,
        })
        .await;
    scenario.error(SwarmErrorCode::Unauthorized).await;
    let other_caller = scenario
        .fixture
        .agent_control_caller(other.members[0].agent_id.as_ref().expect("other member"))
        .await;
    swarm_tool_error(
        &call_tool(
            &other_caller,
            "tyde_swarm_read_image",
            json!({"image_id": images[0].id}),
        )
        .await,
        SwarmErrorCode::Unauthorized,
    );
    let image_path = scenario
        .fixture
        .swarm_store_path()
        .with_extension("images")
        .join(&images[0].id.0);
    let bytes = std::fs::read(&image_path).expect("host-owned image exists");
    std::fs::write(&image_path, vec![0u8; bytes.len()]).expect("simulate corrupt media");
    rejected(
        read(&mut scenario, &swarm.id, &images[0].id).await,
        SwarmErrorCode::Storage,
    );
    std::fs::write(&image_path, bytes).expect("restore actual bytes");
    scenario.fixture.restart_host().await;
    scenario.pending.clear();
    let restored = scenario
        .board(&swarm.id, read_board(SwarmBoard::Briefing))
        .await;
    assert!(restored.posts.contains(&posted) && restored.posts.contains(&replied));
    for item in &uploads {
        assert!(
            matches!(read(&mut scenario, &swarm.id, &item.image_id).await, SwarmImageOutcome::Ready { data: Some(data), .. } if data == item.data),
            "pixels must survive host restart"
        );
    }
    assert!(
        !std::path::Path::new(&scenario.project.root_paths()[0].0)
            .join("shared.Png")
            .exists(),
        "read-only project is never the upload store"
    );
}
