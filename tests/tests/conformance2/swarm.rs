//! Real providers using the production swarm host, MCP router, and durable board.

#[path = "../../../server/tests/fixture.rs"]
pub mod protocol_fixture;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use futures_util::FutureExt;
use protocol::*;
use rmcp::model::{CallToolResult, RawContent};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::time::Instant;

const EVENT_TIMEOUT: Duration = Duration::from_secs(180);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(35);
const AUDIT_PASSES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Opening,
    Exchange,
    Busy,
    Replay,
    Resumed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BoardTool {
    Describe,
    ReadBoard,
    ReadThread,
    ReadImage,
    Post,
}

impl BoardTool {
    fn from_name(name: &str) -> Option<Self> {
        [
            ("tyde_swarm_describe", Self::Describe),
            ("tyde_swarm_read_board", Self::ReadBoard),
            ("tyde_swarm_read_thread", Self::ReadThread),
            ("tyde_swarm_read_image", Self::ReadImage),
            ("tyde_swarm_post", Self::Post),
        ]
        .into_iter()
        .find_map(|(suffix, tool)| name.ends_with(suffix).then_some(tool))
    }
}

enum BoardResult {
    Describe(Box<SwarmDescribe>),
    Board(Box<SwarmBoardPage>),
    Thread(Box<SwarmThreadPage>),
    Post(Box<SwarmPublicationOutcome>),
    Image(Box<SwarmImage>, ImageData),
}

struct ToolEvidence {
    membership: SwarmMembership,
    phase: Phase,
    round_id: Option<SwarmRoundId>,
    tool: Option<BoardTool>,
    request: ToolRequest,
    request_seq: u64,
    request_observation: u64,
    completion_seq: Option<u64>,
    result: Option<BoardResult>,
    code_mode_disabled: bool,
}

impl ToolEvidence {
    fn is_code_mode(&self) -> bool {
        self.tool.is_none() && matches!(self.request.tool_name.as_str(), "exec" | "functions.exec")
    }

    fn input<T: DeserializeOwned>(&self) -> T {
        let ToolRequestType::Other { args } = &self.request.tool_type else {
            panic!(
                "board request lost its canonical arguments; phase={:?}; tool={:?}",
                self.phase, self.tool
            );
        };
        decode(args, "canonical board request")
    }

    fn complete(&mut self, completion: ToolExecutionCompletedData, seq: u64) {
        assert!(
            self.completion_seq.is_none(),
            "duplicate completion on the same real member stream; phase={:?}; tool={:?}",
            self.phase,
            self.tool
        );
        let result = match completion.outcome {
            ToolExecutionOutcome::Succeeded { result } => result,
            ToolExecutionOutcome::Failed {
                message,
                details,
                normalization_failure,
            } => {
                let error_category = if message
                    .contains("MCP tool call requires approval, but approval policy is never")
                {
                    "McpApprovalRequiredUnderNever"
                } else if message.contains("MCP tool call requires approval") {
                    "McpApprovalRequired"
                } else {
                    "Unclassified"
                };
                panic!(
                    "real tool failed; phase={:?}; tool={:?}; error_category={error_category}; details_present={}; normalization={normalization_failure:?}",
                    self.phase,
                    self.tool,
                    details.is_some()
                );
            }
            ToolExecutionOutcome::Cancelled { .. } => panic!(
                "real tool cancelled; phase={:?}; tool={:?}",
                self.phase, self.tool
            ),
        };
        self.completion_seq = Some(seq);
        let Some(tool) = self.tool else {
            if self.is_code_mode()
                && let ToolExecutionResult::Other { result } = &result
            {
                self.code_mode_disabled = contains_code_mode_refusal(result);
            }
            return;
        };
        let ToolExecutionResult::Other { result } = result else {
            panic!(
                "board completion lost canonical MCP result; phase={:?}; tool={tool:?}",
                self.phase
            );
        };
        let mcp: CallToolResult = decode(&result, "canonical MCP completion");
        assert!(
            mcp.is_error == Some(false),
            "board completion must report successful MCP execution; phase={:?}; tool={tool:?}",
            self.phase
        );
        if tool == BoardTool::ReadImage {
            // Claude's native transcript adds a source-path text annotation.
            // Require one typed metadata block and one actual image; extra
            // native text must not substitute for either or add extra pixels.
            let mut metadata = Vec::new();
            let mut pixels = Vec::new();
            for content in &mcp.content {
                match &content.raw {
                    RawContent::Text(text) => {
                        if let Ok(image) = serde_json::from_str::<SwarmImage>(&text.text) {
                            metadata.push(image);
                        }
                    }
                    RawContent::Image(image) => pixels.push(image),
                    _ => panic!("shared image result contains unsupported content"),
                }
            }
            assert_eq!(
                metadata.len(),
                1,
                "image result must carry exactly one canonical metadata block"
            );
            assert_eq!(
                pixels.len(),
                1,
                "image result must carry exactly one actual image block"
            );
            let image = metadata.remove(0);
            let pixels = pixels[0];
            let input: SwarmImageRead = self.input();
            assert!(image.id == input.image_id && image.media_type == pixels.mime_type);
            self.result = Some(BoardResult::Image(
                Box::new(image),
                ImageData {
                    media_type: pixels.mime_type.clone(),
                    data: pixels.data.clone(),
                },
            ));
            return;
        }
        let [content] = mcp.content.as_slice() else {
            panic!(
                "board completion must carry exactly one canonical JSON payload; phase={:?}; tool={tool:?}; parts={}",
                self.phase,
                mcp.content.len()
            );
        };
        let RawContent::Text(content) = &content.raw else {
            panic!(
                "board completion must carry canonical JSON text; phase={:?}; tool={tool:?}",
                self.phase
            );
        };
        let value: Value = serde_json::from_str(&content.text).unwrap_or_else(|_| {
            panic!(
                "invalid board JSON completion; phase={:?}; tool={tool:?}",
                self.phase
            )
        });
        self.result = Some(match tool {
            BoardTool::ReadImage => unreachable!("image content handled before JSON-only results"),
            BoardTool::Describe => {
                let result: SwarmDescribe = decode(&value, "swarm describe completion");
                assert!(
                    result.swarm.id == self.membership.swarm_id
                        && result.member_id == self.membership.member_id,
                    "describe result must identify the authenticated stream's exact swarm/member"
                );
                BoardResult::Describe(Box::new(result))
            }
            BoardTool::ReadBoard => {
                let query: SwarmBoardRead = self.input();
                let result: SwarmBoardPage = decode(&value, "swarm board completion");
                assert!(
                    result.swarm_id == self.membership.swarm_id
                        && result.board == query.board
                        && result
                            .posts
                            .iter()
                            .all(|post| post.swarm_id == result.swarm_id
                                && post.board == query.board),
                    "board result must match this member's authenticated swarm and requested board"
                );
                BoardResult::Board(Box::new(result))
            }
            BoardTool::ReadThread => {
                let query: SwarmThreadRead = self.input();
                let result: SwarmThreadPage = decode(&value, "swarm thread completion");
                assert!(
                    result.swarm_id == self.membership.swarm_id
                        && result.thread_id == query.thread_id
                        && result.root.swarm_id == result.swarm_id
                        && result.root.thread_id == query.thread_id
                        && result.root.thread_id.0 == result.root.id.0
                        && result
                            .posts
                            .iter()
                            .all(|post| post.swarm_id == result.swarm_id
                                && post.thread_id == query.thread_id),
                    "thread result must contain the requested root/replies in the authenticated swarm"
                );
                BoardResult::Thread(Box::new(result))
            }
            BoardTool::Post => {
                let publication: SwarmPublication = self.input();
                let result: SwarmPublicationOutcome =
                    decode(&value, "swarm publication completion");
                assert!(
                    result.post.swarm_id == self.membership.swarm_id
                        && result.post.author
                            == (SwarmAuthor::Member {
                                member_id: self.membership.member_id.clone()
                            })
                        && result.post.publication_id == publication.publication_id
                        && result.post.board == publication.board
                        && result.post.body == publication.body
                        && result.post.attachments == publication.attachments,
                    "successful publication must retain exact authenticated author, publication identity, and content"
                );
                assert!(
                    match &publication.thread_id {
                        Some(thread) => result.post.thread_id == *thread,
                        None => result.post.thread_id.0 == result.post.id.0,
                    },
                    "publication completion must preserve requested root/reply relationship"
                );
                BoardResult::Post(Box::new(result))
            }
        });
    }
}

fn decode<T: DeserializeOwned>(value: &Value, context: &str) -> T {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| {
        panic!(
            "invalid typed payload during {context}; category={:?}; line={}; column={}",
            error.classify(),
            error.line(),
            error.column()
        )
    })
}

fn contains_code_mode_refusal(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            let text = text.to_ascii_lowercase();
            text.contains("disabled")
                && ["code mode", "code-mode", "code_mode"]
                    .iter()
                    .any(|label| text.contains(label))
        }
        Value::Array(values) => values.iter().any(contains_code_mode_refusal),
        Value::Object(values) => values.values().any(contains_code_mode_refusal),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

struct BusyObservation {
    swarm: Swarm,
    audit_reads_started: HashSet<(SwarmMemberId, SwarmRoundId, SwarmThreadId)>,
    observation: u64,
}

struct BoardClient {
    connection: client::Connection,
    pending: Vec<Envelope>,
    phase: Phase,
    streams: HashMap<StreamPath, SwarmMembership>,
    activities: HashMap<StreamPath, AgentActivity>,
    tools: HashMap<(StreamPath, String), ToolEvidence>,
    swarms: HashMap<SwarmId, Swarm>,
    delivery_edges: HashMap<SwarmNotificationId, (SwarmDeliveryState, usize)>,
    busy_observations: Vec<BusyObservation>,
    post_count: usize,
    observation: u64,
    command_kind: &'static str,
}

impl BoardClient {
    async fn connect(
        host: server::HostHandle,
        phase: Phase,
    ) -> (Self, settings_model::HostBootstrapPayload) {
        let (connection, bootstrap) =
            tokio::time::timeout(CONNECTION_TIMEOUT, protocol_fixture::connect_host(host))
                .await
                .expect("real swarm connection/bootstrap exceeded its bounded deadline");
        let client = Self {
            connection,
            pending: Vec::new(),
            phase,
            streams: HashMap::new(),
            activities: HashMap::new(),
            tools: HashMap::new(),
            swarms: bootstrap
                .swarms
                .iter()
                .map(|swarm| (swarm.id.clone(), swarm.clone()))
                .collect(),
            delivery_edges: HashMap::new(),
            busy_observations: Vec::new(),
            post_count: 0,
            observation: 0,
            command_kind: "none",
        };
        (client, bootstrap)
    }

    async fn send(&mut self, command: SwarmCommandPayload) {
        self.command_kind = match &command {
            SwarmCommandPayload::GenerateDraft { .. } => "generate_draft",
            SwarmCommandPayload::EditDraftMember { .. } => "edit_draft_member",
            SwarmCommandPayload::Launch { .. } => "launch",
            SwarmCommandPayload::Post { .. } => "post",
            SwarmCommandPayload::Pause { .. } => "pause",
            SwarmCommandPayload::Resume { .. } => "resume",
            SwarmCommandPayload::ReadBoard { .. } => "read_board",
            _ => "other",
        };
        tokio::time::timeout(CONNECTION_TIMEOUT, self.connection.swarm_command(command))
            .await
            .expect("real swarm command send exceeded its bounded deadline")
            .unwrap_or_else(|_| {
                panic!(
                    "real swarm command transport failed; phase={:?}",
                    self.phase
                )
            });
    }

    fn register_stream(&mut self, stream: &StreamPath, start: AgentStartPayload) {
        let membership = start
            .swarm_membership
            .expect("real swarm agent stream must expose canonical member ownership");
        if let Some(existing) = self.streams.insert(stream.clone(), membership.clone()) {
            assert!(
                existing == membership,
                "agent instance stream changed authenticated swarm membership"
            );
        }
    }

    fn observe_chat(&mut self, stream: &StreamPath, seq: u64, phase: Phase, event: ChatEvent) {
        match event {
            ChatEvent::ToolRequest(request) => {
                let membership = self
                    .streams
                    .get(stream)
                    .expect("real tool request must follow an owned agent stream bootstrap")
                    .clone();
                let round_id = self
                    .swarms
                    .get(&membership.swarm_id)
                    .and_then(|swarm| {
                        swarm
                            .members
                            .iter()
                            .find(|member| member.spec.id == membership.member_id)
                    })
                    .and_then(|member| member.current_round_id.clone());
                let key = (stream.clone(), request.tool_call_id.clone());
                let tool = BoardTool::from_name(&request.tool_name);
                let previous = self.tools.insert(
                    key,
                    ToolEvidence {
                        membership,
                        phase,
                        round_id,
                        tool,
                        request,
                        request_seq: seq,
                        request_observation: self.observation,
                        completion_seq: None,
                        result: None,
                        code_mode_disabled: false,
                    },
                );
                assert!(
                    previous.is_none(),
                    "duplicate real tool request on the same member stream; phase={phase:?}; tool={tool:?}"
                );
            }
            ChatEvent::ToolExecutionCompleted(completion) => {
                let key = (stream.clone(), completion.tool_call_id.clone());
                let Some(tool) = self.tools.get_mut(&key) else {
                    // Bootstrap history is a bounded message tail and may omit
                    // an older request. Unpaired replay is never execution proof.
                    assert!(
                        phase == Phase::Replay,
                        "real live tool completion must correlate to a request on the SAME instance stream"
                    );
                    return;
                };
                if tool.phase == Phase::Replay && phase != Phase::Replay {
                    tool.phase = phase;
                }
                tool.complete(completion, seq);
            }
            ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
                panic!("real member emitted a provider error; phase={phase:?}");
            }
            _ => {}
        }
    }

    fn observe(&mut self, event: &Envelope) {
        match event.kind {
            FrameKind::CommandError => {
                let error: CommandErrorPayload = decode(&event.payload, "command rejection");
                panic!(
                    "real swarm command rejected; phase={:?}; code={:?}; request_kind={:?}",
                    self.phase, error.code, error.request_kind
                );
            }
            FrameKind::SwarmErrorNotify => {
                let error: SwarmErrorNotifyPayload = decode(&event.payload, "swarm rejection");
                panic!(
                    "real swarm rejected; phase={:?}; command={}; code={:?}; draft_present={}; swarm_present={}; publication_present={}; session_settings_rejected={}; model_field={}; effort_field={}; reasoning_field={}",
                    self.phase,
                    self.command_kind,
                    error.code,
                    error.draft_id.is_some(),
                    error.swarm_id.is_some(),
                    error.publication_id.is_some(),
                    error.message.contains("session settings"),
                    error.message.contains("'model'"),
                    error.message.contains("'effort'"),
                    error.message.contains("'reasoning_effort'")
                );
            }
            FrameKind::AgentError => {
                let error: AgentErrorPayload = decode(&event.payload, "provider failure");
                panic!(
                    "real member failed; phase={:?}; code={:?}; fatal={}",
                    self.phase, error.code, error.fatal
                );
            }
            FrameKind::AgentStart => {
                self.register_stream(&event.stream, decode(&event.payload, "member start"))
            }
            FrameKind::AgentBootstrap => {
                let bootstrap: AgentBootstrapPayload = decode(&event.payload, "member bootstrap");
                for entry in &bootstrap.events {
                    if let AgentBootstrapEvent::AgentStart(start) = entry {
                        self.register_stream(&event.stream, start.clone());
                    }
                }
                // First activation replay contains only this opening turn. On
                // reconnect historical requests/completions are not resume proof.
                let phase = if self.phase == Phase::Opening {
                    Phase::Opening
                } else {
                    Phase::Replay
                };
                for entry in bootstrap.events {
                    match entry {
                        AgentBootstrapEvent::ChatEvent(chat) => {
                            self.observe_chat(&event.stream, event.seq, phase, chat)
                        }
                        AgentBootstrapEvent::AgentError(error) => panic!(
                            "real member bootstrap contains provider failure; phase={phase:?}; code={:?}; fatal={}",
                            error.code, error.fatal
                        ),
                        _ => {}
                    }
                }
                self.activities
                    .insert(event.stream.clone(), bootstrap.activity);
            }
            FrameKind::AgentActivityChanged => {
                let activity: AgentActivityChangedPayload =
                    decode(&event.payload, "member activity");
                self.activities
                    .insert(event.stream.clone(), activity.activity);
            }
            FrameKind::ChatEvent => self.observe_chat(
                &event.stream,
                event.seq,
                self.phase,
                decode(&event.payload, "real provider event"),
            ),
            FrameKind::SwarmNotify => {
                let payload: SwarmNotifyPayload =
                    decode(&event.payload, "canonical swarm transition");
                assert!(
                    payload
                        .swarm
                        .members
                        .iter()
                        .all(|member| member.state != SwarmMemberState::Failed
                            && member.error.is_none()),
                    "real member activation/runtime failed; phase={:?}; member_count={}; failure-facts={:?}",
                    self.phase,
                    payload.swarm.members.len(),
                    payload
                        .swarm
                        .members
                        .iter()
                        .map(|member| (
                            member.state,
                            member.runtime_status,
                            member.agent_id.is_some(),
                            member.session_id.is_some(),
                            member
                                .error
                                .as_ref()
                                .is_some_and(|error| error.contains("session settings"))
                        ))
                        .collect::<Vec<_>>()
                );
                for notification in &payload.swarm.notifications {
                    let edge = self
                        .delivery_edges
                        .entry(notification.id.clone())
                        .or_insert((SwarmDeliveryState::Pending, 0));
                    if notification.state == SwarmDeliveryState::Accepted
                        && edge.0 != SwarmDeliveryState::Accepted
                    {
                        edge.1 += 1;
                    }
                    edge.0 = notification.state;
                    assert!(
                        edge.1 <= 1,
                        "one durable notification was accepted more than once; phase={:?}",
                        self.phase
                    );
                }
                if self.phase == Phase::Busy {
                    let audit_reads_started = self
                        .tools
                        .values()
                        .filter(|tool| {
                            tool.phase == Phase::Busy
                                && tool.tool == Some(BoardTool::ReadThread)
                                && tool.membership.swarm_id == payload.swarm.id
                        })
                        .filter_map(|tool| {
                            let query: SwarmThreadRead = tool.input();
                            tool.round_id.clone().map(|round| {
                                (tool.membership.member_id.clone(), round, query.thread_id)
                            })
                        })
                        .collect();
                    self.busy_observations.push(BusyObservation {
                        swarm: payload.swarm.clone(),
                        audit_reads_started,
                        observation: self.observation,
                    });
                }
                self.swarms.insert(payload.swarm.id.clone(), payload.swarm);
            }
            FrameKind::SwarmPostNotify => self.post_count += 1,
            _ => {}
        }
    }

    async fn next(&mut self, deadline: Instant, context: &str) -> Envelope {
        let event = tokio::time::timeout_at(deadline, self.connection.next_event()).await.unwrap_or_else(|_| {
            let state = self.swarms.values().map(|swarm| (swarm.lifecycle, swarm.members.iter().map(|member| (member.state, member.runtime_status, member.error.is_some())).collect::<Vec<_>>())).collect::<Vec<_>>();
            let mut tools = self.tools.values().collect::<Vec<_>>();
            tools.sort_by_key(|tool| tool.request_observation);
            let tool_facts = tools.iter().map(|tool| (tool.phase, tool.tool, tool.is_code_mode(), tool.round_id.is_some(), tool.completion_seq.is_some(), tool.result.is_some(), tool.code_mode_disabled)).collect::<Vec<_>>();
            panic!("real swarm timed out during {context}; phase={:?}; command={}; state-facts={state:?}; posts={}; requests={}; completions={}; tool-facts(phase,board_tool,code_mode,round,completed,board_result,code_mode_disabled)={tool_facts:?}", self.phase, self.command_kind, self.post_count, self.tools.len(), self.tools.values().filter(|tool| tool.completion_seq.is_some()).count());
        }).unwrap_or_else(|error| {
            let fact = match error {
                FrameError::Io(error) => format!("FrameIo(kind={:?},os={:?})", error.kind(), error.raw_os_error()),
                FrameError::Json(error) => format!("FrameJson(category={:?},line={},column={})", error.classify(), error.line(), error.column()),
                FrameError::Protocol(reason) => format!("FrameProtocol(invalid_magic={},sequence_mismatch={})", reason == "invalid TYD2 record magic", reason.contains("sequence")),
            };
            panic!("real swarm protocol read failed during {context}; phase={:?}; error={fact}", self.phase);
        }).expect("real swarm connection remains open");
        self.observation += 1;
        self.observe(&event);
        event
    }

    fn buffer(&mut self, event: Envelope) {
        if matches!(
            event.kind,
            FrameKind::SwarmDraftNotify
                | FrameKind::SwarmPostNotify
                | FrameKind::SwarmBoardNotify
                | FrameKind::SwarmThreadNotify
                | FrameKind::ProjectNotify
                | FrameKind::LaunchProfileCatalogNotify
                | FrameKind::SessionSchemas
        ) {
            self.pending.push(event);
        }
    }

    async fn wait<T: DeserializeOwned>(
        &mut self,
        kind: FrameKind,
        context: &str,
        predicate: impl Fn(&T) -> bool,
    ) -> T {
        if let Some(index) = self
            .pending
            .iter()
            .position(|event| event.kind == kind && predicate(&decode(&event.payload, context)))
        {
            return decode(&self.pending.remove(index).payload, context);
        }
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let event = self.next(deadline, context).await;
            if event.kind == kind {
                let payload = decode(&event.payload, context);
                if predicate(&payload) {
                    return payload;
                }
            }
            self.buffer(event);
        }
    }

    async fn swarm(&mut self, id: &SwarmId, predicate: impl Fn(&Swarm) -> bool) -> Swarm {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            // Do not consume an older buffered snapshot as a new transition.
            if let Some(swarm) = self.swarms.get(id).filter(|swarm| predicate(swarm)) {
                return swarm.clone();
            }
            let event = self.next(deadline, "runtime transition").await;
            self.buffer(event);
        }
    }

    async fn quiescent(&mut self, id: &SwarmId) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if self.swarms.get(id).is_some_and(|swarm| {
                swarm.members.iter().all(|member| {
                    member.state == SwarmMemberState::Live
                        && member.runtime_status == Some(AgentControlStatus::Idle)
                        && self.streams.iter().any(|(stream, ownership)| {
                            ownership.swarm_id == *id
                                && ownership.member_id == member.spec.id
                                && self.activities.get(stream) == Some(&AgentActivity::Idle)
                        })
                })
            }) && self
                .tools
                .values()
                .all(|tool| tool.completion_seq.is_some())
            {
                return;
            }
            let event = self
                .next(deadline, "real member turns and tool completions settling")
                .await;
            self.buffer(event);
        }
    }

    async fn require_tool(
        &mut self,
        member: &SwarmMemberId,
        phase: Phase,
        tool: BoardTool,
        predicate: impl Fn(&BoardResult) -> bool,
    ) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if self.tools.values().any(|evidence| {
                evidence.membership.member_id == *member
                    && evidence.phase == phase
                    && evidence.tool == Some(tool)
                    && evidence.result.as_ref().is_some_and(&predicate)
            }) {
                return;
            }
            let event = self
                .next(deadline, "successful correlated canonical board completion")
                .await;
            self.buffer(event);
        }
    }

    async fn member_post(
        &mut self,
        member: &SwarmMemberId,
        board: SwarmBoard,
        marker: &str,
        cause: &SwarmPost,
        after_cursor: u64,
        thread: Option<&SwarmThreadId>,
    ) -> SwarmPost {
        let event: SwarmPostNotifyPayload = self.wait(FrameKind::SwarmPostNotify, "causal authenticated model publication", |event: &SwarmPostNotifyPayload| {
            let same_swarm = event.post.swarm_id == cause.swarm_id;
            let same_board = event.post.board == board;
            let intended_author = event.post.author == (SwarmAuthor::Member { member_id: member.clone() });
            let same_cause = event.post.round_id == cause.round_id;
            let after_trigger = event.post.cursor > after_cursor;
            let intended_thread = match thread { Some(thread) => event.post.thread_id == *thread, None => event.post.thread_id.0 == event.post.id.0 };
            let intended_marker = event.post.body.iter().any(|segment| matches!(segment, SwarmBodySegment::Text { text } if text.contains(marker)));
            eprintln!("real swarm publication predicate facts: same_swarm={same_swarm}; same_board={same_board}; intended_author={intended_author}; same_cause={same_cause}; after_trigger={after_trigger}; intended_thread={intended_thread}; intended_marker={intended_marker}");
            same_swarm && same_board && intended_author && same_cause && after_trigger && intended_thread && intended_marker
        }).await;
        event.post
    }

    async fn post(&mut self, id: &SwarmId, members: &[&SwarmMemberId], text: &str) -> SwarmPost {
        let publication_id = SwarmPublicationId(uuid::Uuid::new_v4().to_string());
        let mut body = members
            .iter()
            .map(|member| SwarmBodySegment::MemberMention {
                member_id: (*member).clone(),
            })
            .collect::<Vec<_>>();
        body.push(SwarmBodySegment::Text {
            text: text.to_owned(),
        });
        self.send(SwarmCommandPayload::Post {
            swarm_id: id.clone(),
            publication: SwarmPublication {
                images: Vec::new(),
                board: SwarmBoard::Briefing,
                publication_id: publication_id.clone(),
                body: body.clone(),
                thread_id: None,
                attachments: Vec::new(),
            },
        })
        .await;
        let event: SwarmPostNotifyPayload = self
            .wait(
                FrameKind::SwarmPostNotify,
                "human publication",
                |event: &SwarmPostNotifyPayload| {
                    event.post.swarm_id == *id
                        && event.post.publication_id == publication_id
                        && event.post.author == SwarmAuthor::Human
                },
            )
            .await;
        assert!(
            event.post.body == body
                && event.post.board == SwarmBoard::Briefing
                && event.post.thread_id.0 == event.post.id.0,
            "human trigger must retain exact intended publication/root"
        );
        event.post
    }

    fn assert_accepted_once(&self, swarm: &Swarm, member: &SwarmMemberId, post: &SwarmPost) {
        let intents = swarm
            .notifications
            .iter()
            .filter(|intent| intent.member_id == *member && intent.post_ids.contains(&post.id))
            .collect::<Vec<_>>();
        assert_eq!(
            intents.len(),
            1,
            "one trigger must create exactly one intent for the target member"
        );
        let intent = intents[0];
        assert!(
            intent.state == SwarmDeliveryState::Accepted && intent.round_id == post.round_id,
            "accepted delivery must retain the exact trigger round"
        );
        assert!(
            self.delivery_edges
                .get(&intent.id)
                .is_some_and(|(_, accepted)| *accepted == 1),
            "the real event stream must show one acceptance transition, not repeated snapshot counting"
        );
        assert!(
            swarm
                .members
                .iter()
                .any(|current| current.spec.id == *member && current.context_cursor >= post.cursor),
            "acceptance must advance the target's canonical inline-context cursor"
        );
    }

    async fn busy_pending(
        &mut self,
        member: &SwarmMemberId,
        audit: &SwarmPost,
        queued: &SwarmPost,
    ) -> (SwarmNotificationId, u64) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            for observed in &self.busy_observations {
                if observed.swarm.id == audit.swarm_id
                    && observed.audit_reads_started.contains(&(
                        member.clone(),
                        audit.round_id.clone(),
                        audit.thread_id.clone(),
                    ))
                    && observed.swarm.members.iter().any(|current| {
                        current.spec.id == *member
                            && current.state == SwarmMemberState::Live
                            && current.runtime_status == Some(AgentControlStatus::Thinking)
                            && current.current_round_id.as_ref() == Some(&audit.round_id)
                            && current.context_cursor < queued.cursor
                    })
                    && let Some(intent) = observed.swarm.notifications.iter().find(|intent| {
                        intent.member_id == *member
                            && intent.round_id == queued.round_id
                            && intent.post_ids.contains(&queued.id)
                            && intent.state == SwarmDeliveryState::Pending
                    })
                {
                    eprintln!(
                        "real swarm busy evidence: phase=Busy; provider_audit_started=true; runtime=Thinking; intent=Pending; context_before_trigger=true"
                    );
                    return (intent.id.clone(), observed.observation);
                }
            }
            let event = self
                .next(
                    deadline,
                    "pending delivery during the real provider's current audit turn",
                )
                .await;
            self.buffer(event);
        }
    }
}

fn spawn_host(store: &Path, backend: BackendKind) -> server::HostHandle {
    let settings_path = store.join("settings.json");
    let settings = server::store::settings::HostSettingsStore::load(settings_path.clone())
        .expect("load isolated real-host settings");
    let mut values = settings.get().expect("read isolated real-host settings");
    values.enabled_backends = vec![backend];
    settings
        .replace(values)
        .expect("enable the real provider under test");
    server::spawn_host_with_store_paths_and_runtime_config(
        store.join("sessions.json"),
        store.join("projects.json"),
        settings_path,
        server::HostRuntimeConfig::default(),
    )
    .expect("start real swarm host")
}

fn backend_profile(catalog: &LaunchProfileCatalog, backend: BackendKind) -> Option<LaunchProfile> {
    catalog.entries.iter().find_map(|entry| match entry {
        LaunchProfileEntry::Ready { profile }
            if profile.backend_kind == backend
                && profile.kind == LaunchProfileKind::BackendDefault =>
        {
            Some(profile.clone())
        }
        _ => None,
    })
}

fn backend_schema(
    entries: &[SessionSchemaEntry],
    backend: BackendKind,
) -> Option<SessionSettingsSchema> {
    match entries.iter().find(|entry| entry.backend_kind() == backend) {
        Some(SessionSchemaEntry::Ready { schema }) => Some(schema.clone()),
        Some(SessionSchemaEntry::Unavailable { .. }) => {
            panic!("real host model catalog unavailable; backend={backend:?}");
        }
        Some(SessionSchemaEntry::Pending { .. }) | None => None,
    }
}

fn setting_key_fact(key: &str) -> &'static str {
    match key {
        "model" => "model",
        "profile" => "profile",
        "mode" => "mode",
        "effort" => "effort",
        "reasoning_effort" => "reasoning_effort",
        "speed" => "speed",
        _ => "other",
    }
}

fn catalog_settings(
    schema: &SessionSettingsSchema,
    requested: SessionSettingsValues,
) -> SessionSettingsValues {
    assert!(
        !requested.0.is_empty(),
        "real swarm requires an explicit conformance model/profile selection"
    );
    let mut unsupported = Vec::new();
    let mut settings = requested.0.iter().collect::<Vec<_>>();
    settings.sort_by_key(|(key, _)| *key);
    for (key, value) in settings {
        let key_fact = setting_key_fact(key);
        let field = schema.fields.iter().find(|field| field.key == *key)
            .unwrap_or_else(|| panic!("conformance setting absent from real host catalog; backend={:?}; field={key_fact}", schema.backend_kind));
        let valid = match (value, &field.field_type) {
            (
                SessionSettingValue::String(value),
                SessionSettingFieldType::Select { nullable, .. },
            ) => {
                let options = field.select_options(&requested).unwrap_or_else(|| panic!("real model catalog lacks selected dependency; backend={:?}; field={key_fact}", schema.backend_kind));
                let advertised = options.iter().any(|option| option.value == *value);
                let unavailable = options.iter().any(|option| {
                    option.value == *value && option.label.contains(SCHEMA_UNAVAILABLE_MARKER)
                });
                eprintln!(
                    "real swarm catalog evidence: backend={:?}; field={key_fact}; options={}; requested_advertised={advertised}; unavailable={unavailable}; nullable={nullable}; dependent={}",
                    schema.backend_kind,
                    options.len(),
                    field.select_options_by_setting.is_some()
                );
                // Direct-backend profiles can request effort on a model whose
                // native host catalog explicitly offers no effort overrides.
                // Null is advertised here; replacing the model is not.
                if !advertised
                    && *nullable
                    && options.is_empty()
                    && field.select_options_by_setting.is_some()
                    && !matches!(key.as_str(), "model" | "profile")
                {
                    unsupported.push(key.clone());
                    true
                } else {
                    advertised && !unavailable
                }
            }
            (SessionSettingValue::Bool(_), SessionSettingFieldType::Toggle { .. }) => true,
            (
                SessionSettingValue::Integer(value),
                SessionSettingFieldType::Integer { min, max, .. },
            ) => (*min..=*max).contains(value),
            (SessionSettingValue::Null, SessionSettingFieldType::Select { nullable, .. }) => {
                *nullable
            }
            (
                SessionSettingValue::Null,
                SessionSettingFieldType::Toggle { .. } | SessionSettingFieldType::Integer { .. },
            ) => true,
            _ => false,
        };
        assert!(
            valid,
            "conformance setting invalid in real host catalog; backend={:?}; field={key_fact}",
            schema.backend_kind
        );
    }
    let mut resolved = requested.clone();
    for key in unsupported {
        eprintln!(
            "real swarm catalog selection: backend={:?}; field={}; selection=ExplicitNull; native_options_empty=true",
            schema.backend_kind,
            setting_key_fact(&key)
        );
        resolved.0.insert(key, SessionSettingValue::Null);
    }
    assert!(
        resolved.0.get("model") == requested.0.get("model")
            && resolved.0.get("profile") == requested.0.get("profile"),
        "real catalog setup must preserve the intended conformance model/profile"
    );
    resolved
}

async fn require_reads(
    client: &mut BoardClient,
    member: &SwarmMemberId,
    phase: Phase,
    root: &SwarmPost,
    reply: Option<&SwarmPost>,
) {
    client.require_tool(member, phase, BoardTool::ReadBoard, |result| matches!(result, BoardResult::Board(page) if page.swarm_id == root.swarm_id && page.board == root.board && page.posts.contains(root) && reply.is_none_or(|reply| page.posts.contains(reply)))).await;
    client.require_tool(member, phase, BoardTool::ReadThread, |result| matches!(result, BoardResult::Thread(page) if page.swarm_id == root.swarm_id && page.thread_id == root.thread_id && page.root == *root && reply.is_none_or(|reply| page.posts.contains(reply)))).await;
}

async fn require_publication(
    client: &mut BoardClient,
    member: &SwarmMemberId,
    phase: Phase,
    post: &SwarmPost,
) {
    client.require_tool(member, phase, BoardTool::Post, |result| matches!(result, BoardResult::Post(outcome) if outcome.post == *post && !outcome.duplicate && outcome.commit_status == SwarmCommitStatus::Durable)).await;
}

pub async fn run(backend: BackendKind, workspace: &Path, settings: SessionSettingsValues) {
    let store = tempfile::tempdir().expect("create isolated real swarm store");
    let mut host = spawn_host(store.path(), backend);
    let result = std::panic::AssertUnwindSafe(async {
        let (mut client, bootstrap) = BoardClient::connect(host.clone(), Phase::Opening).await;
        let profile = match backend_profile(&bootstrap.launch_profile_catalog, backend) {
            Some(profile) => profile,
            None => {
                let payload: LaunchProfileCatalogPayload = client.wait(FrameKind::LaunchProfileCatalogNotify, "real backend readiness", |payload: &LaunchProfileCatalogPayload| backend_profile(&payload.catalog, backend).is_some()).await;
                backend_profile(&payload.catalog, backend).expect("ready real launch profile")
            }
        };
        let schema = match backend_schema(&bootstrap.session_schemas, backend) {
            Some(schema) => schema,
            None => {
                let payload: SessionSchemasPayload = client.wait(FrameKind::SessionSchemas, "real host model catalog", |payload: &SessionSchemasPayload| backend_schema(&payload.schemas, backend).is_some()).await;
                backend_schema(&payload.schemas, backend).expect("ready real host model catalog")
            }
        };
        let settings = catalog_settings(&schema, settings);
        tokio::time::timeout(CONNECTION_TIMEOUT, client.connection.project_create(ProjectCreatePayload { name: "Real swarm conformance".to_owned(), roots: vec![ProjectRootPath(workspace.to_string_lossy().into_owned())] })).await.expect("project command send exceeded its bounded deadline").unwrap_or_else(|_| panic!("real swarm project command transport failed"));
        let event: ProjectNotifyPayload = client.wait(FrameKind::ProjectNotify, "project registration", |event| matches!(event, ProjectNotifyPayload::Upsert { .. })).await;
        let ProjectNotifyPayload::Upsert { project } = event else { unreachable!() };
        let draft_id = SwarmDraftId(uuid::Uuid::new_v4().to_string());
        let guidance = "Use only the four tyde_swarm tools directly, never another agent tool. Every result must be a durable board post, not a private final answer. Discover your identity and peers using tyde_swarm_describe, then read Briefing using tyde_swarm_read_board. Read the exact opening thread using tyde_swarm_read_thread. For every root publication, omit thread_id or set it to null; supply thread_id only for the explicitly designated peer reply, using the peer request's exact thread ID. Initially publish one Briefing root SWARM_READY and finish your turn. Later, Initiator receiving BEGIN_EXCHANGE must publish a Coordination root containing PEER_REQUEST and a typed member_mention for Responder. Responder receiving PEER_REQUEST must read Coordination using tyde_swarm_read_board and then read that exact Coordination thread and reply in the SAME thread with PEER_REPLY and a typed member_mention for Initiator. Initiator receiving PEER_REPLY must read Coordination using tyde_swarm_read_board and then read that exact thread including the reply and publish a Briefing root PEER_DONE without mentions. Do not respond to any other agent's Briefing posts. Only Initiator handles BUSY_AUDIT: perform exactly four audit passes, each calling tyde_swarm_read_board for Briefing THEN tyde_swarm_read_thread for the exact human BUSY_AUDIT root. Execute all eight reads sequentially, awaiting each result before issuing the next; do not batch or parallelize them. Then publish a Briefing root AUDIT_DONE without mentions and finish that turn. If BUSY_FOLLOWUP appears in a read result during the audit, do not act on it in the audit round: handle it only when separately delivered in your next notification context. Only Initiator receiving that delivered BUSY_FOLLOWUP reads Briefing and its exact human trigger thread, publishes a Briefing root BUSY_CONFIRMED without mentions, and finishes. Responder does nothing on BUSY_AUDIT or BUSY_FOLLOWUP and finishes immediately. If a human asks RESUME_CHECK, both peers read Briefing and that exact human trigger thread, then each publishes one Briefing root RESUME_CONFIRMED without mentions. Do not create or edit files. Every publication must have a fresh publication_id; obtain exact swarm/member/thread/post IDs from describe/read tools or delivered notification context. Never type an @name instead of a member_mention segment. Handle only the explicitly delivered trigger, not an older matching marker in board history.";
        let provider_instructions = match backend {
            // Retained native evidence: exec discovery returned code-mode-disabled
            // refusals, not results from any of the four actual board tools.
            BackendKind::Codex => "Invoke the four named MCP tools DIRECTLY from the tyde-agent-control server using their advertised native tool-call interfaces. Do not use functions.exec, exec, JavaScript, ALL_TOOLS, tool-discovery code, or any code-mode wrapper: code mode is disabled by the read-only policy. The discovery and opening board/thread primer below applies only to the initial opening notification: start by directly calling tyde_swarm_describe, then perform those opening reads and publication. On later notifications, do not repeat the opening primer; execute only the current phase's prescribed reads and publication. A private response is not a substitute for publishing.",
            BackendKind::Claude => "Invoke the four named MCP tools directly from the tyde-agent-control server. For the initial Opening notification only, perform exactly this four-tool sequence, awaiting each successful result: (1) tyde_swarm_describe; (2) tyde_swarm_read_board for Briefing; (3) tyde_swarm_read_thread for the exact opening thread identified by those results; (4) tyde_swarm_post one SWARM_READY Briefing root, omitting thread_id or setting it to null. Calls 2 and 3 must be actual MCP tool invocations even when the inline notification, visible context, or Describe result already contains the opening text and IDs: none substitutes for the two real read results. Do not publish or finish the opening turn before both reads succeed. On later notifications, follow only the corresponding phase instructions below, without repeating this opening primer.",
            _ => "Invoke the four named MCP tools directly from the tyde-agent-control server. Start by calling tyde_swarm_describe, then perform the required real board/thread reads and publication.",
        };
        let guidance = format!("{provider_instructions}\n\n{guidance}");
        client.send(SwarmCommandPayload::GenerateDraft { draft_id: draft_id.clone(), expected_revision: None, name: "Real peer coordination".to_owned(), opening_brief: guidance.to_owned(), constraints: SwarmConstraints {
            project_id: project.id, workspace_policy: SwarmWorkspacePolicy::ReadOnly, max_live_agents: 2,
            allocations: vec![SwarmBackendAllocation { backend_kind: backend, launch_profile_id: profile.id, session_settings: settings, count: 2 }], shared_guidance: guidance.to_owned(), agent_wake_budget: 16,
        }}).await;
        let event: SwarmDraftNotifyPayload = client.wait(FrameKind::SwarmDraftNotify, "generated draft", |event| matches!(event, SwarmDraftNotifyPayload::Upsert { draft } if draft.id == draft_id)).await;
        let SwarmDraftNotifyPayload::Upsert { draft } = event else { unreachable!() };
        let mut draft = *draft;
        for (index, name) in ["Initiator", "Responder"].into_iter().enumerate() {
            let mut member = draft.members[index].clone();
            member.name = name.to_owned();
            member.pinned = true;
            client.send(SwarmCommandPayload::EditDraftMember { draft_id: draft.id.clone(), expected_revision: draft.revision, member }).await;
            let event: SwarmDraftNotifyPayload = client.wait(FrameKind::SwarmDraftNotify, "reviewed peer identity", |event| matches!(event, SwarmDraftNotifyPayload::Upsert { draft: updated } if updated.id == draft.id && updated.revision > draft.revision)).await;
            let SwarmDraftNotifyPayload::Upsert { draft: updated } = event else { unreachable!() };
            draft = *updated;
        }
        assert!(draft.conflicts.is_empty(), "real swarm draft must be launchable");
        let initiator = draft.members[0].id.clone();
        let responder = draft.members[1].id.clone();
        client.send(SwarmCommandPayload::Launch { draft_id: draft.id.clone(), expected_revision: draft.revision }).await;
        let event: SwarmNotifyPayload = client.wait(FrameKind::SwarmNotify, "real swarm launch", |event: &SwarmNotifyPayload| event.swarm.source_draft_id.as_ref() == Some(&draft.id)).await;
        let swarm_id = event.swarm.id;
        let opening_id = event.swarm.opening_post_id.expect("launch publishes its canonical opening root");
        let opening: SwarmPostNotifyPayload = client.wait(FrameKind::SwarmPostNotify, "canonical opening post", |event: &SwarmPostNotifyPayload| event.post.swarm_id == swarm_id && event.post.id == opening_id).await;
        let opening = opening.post;
        assert!(opening.author == SwarmAuthor::Human && opening.board == SwarmBoard::Briefing && opening.thread_id.0 == opening.id.0, "opening must be the intended human Briefing root");
        let ready_first = client.member_post(&initiator, SwarmBoard::Briefing, "SWARM_READY", &opening, opening.cursor, None).await;
        let ready_second = client.member_post(&responder, SwarmBoard::Briefing, "SWARM_READY", &opening, opening.cursor, None).await;
        for (member, ready) in [(&initiator, &ready_first), (&responder, &ready_second)] {
            client.require_tool(member, Phase::Opening, BoardTool::Describe, |result| matches!(result, BoardResult::Describe(describe) if describe.swarm.id == swarm_id && describe.member_id == *member)).await;
            require_reads(&mut client, member, Phase::Opening, &opening, None).await;
            require_publication(&mut client, member, Phase::Opening, ready).await;
        }
        client.quiescent(&swarm_id).await;

        client.phase = Phase::Exchange;
        let begin = client.post(&swarm_id, &[&initiator], "BEGIN_EXCHANGE. Coordinate with Responder as described in the opening brief.").await;
        assert!(begin.cursor > ready_first.cursor.max(ready_second.cursor) && begin.round_id != opening.round_id, "human exchange must start a new round after both opening turns");
        let request = client.member_post(&initiator, SwarmBoard::Coordination, "PEER_REQUEST", &begin, begin.cursor, None).await;
        assert!(request.body.contains(&SwarmBodySegment::MemberMention { member_id: responder.clone() }), "peer request must route by typed member identity");
        let reply = client.member_post(&responder, SwarmBoard::Coordination, "PEER_REPLY", &begin, request.cursor, Some(&request.thread_id)).await;
        assert!(reply.thread_id == request.thread_id && reply.id != request.id, "peer response must be an actual reply, not a similarly marked root");
        assert!(reply.body.contains(&SwarmBodySegment::MemberMention { member_id: initiator.clone() }), "peer reply must route to the exact original peer");
        let done = client.member_post(&initiator, SwarmBoard::Briefing, "PEER_DONE", &begin, reply.cursor, None).await;
        require_reads(&mut client, &responder, Phase::Exchange, &request, None).await;
        require_reads(&mut client, &initiator, Phase::Exchange, &request, Some(&reply)).await;
        require_publication(&mut client, &initiator, Phase::Exchange, &request).await;
        require_publication(&mut client, &responder, Phase::Exchange, &reply).await;
        require_publication(&mut client, &initiator, Phase::Exchange, &done).await;
        client.quiescent(&swarm_id).await;

        client.phase = Phase::Busy;
        tokio::time::timeout(EVENT_TIMEOUT, async {
            let audit_prompt = match backend {
                BackendKind::Codex => "BUSY_AUDIT. Initiator: this phase requires exactly eight read-tool calls, not eight total tool calls. Do not repeat discovery or any opening/primer reads. Perform this numbered sequence exactly once, awaiting each result before the next call: (1) tyde_swarm_read_board for Briefing; (2) tyde_swarm_read_thread for this exact human trigger thread; (3) tyde_swarm_read_board for Briefing; (4) tyde_swarm_read_thread for this same human trigger thread; (5) tyde_swarm_read_board for Briefing; (6) tyde_swarm_read_thread for this same human trigger thread; (7) tyde_swarm_read_board for Briefing; (8) tyde_swarm_read_thread for this same human trigger thread. These are the four audit passes: do not add, skip, or repeat a read. Only after call8 succeeds, publish one AUDIT_DONE Briefing root without mentions, omitting thread_id or setting it to null, and finish. Responder: finish without acting.",
                _ => "BUSY_AUDIT. Initiator: perform the four sequential audit passes on Briefing and this exact human thread, then post AUDIT_DONE and finish. Responder: finish without acting.",
            };
            let audit = client.post(&swarm_id, &[&initiator], audit_prompt).await;
            assert!(audit.cursor > done.cursor && audit.round_id != begin.round_id, "busy audit must be a fresh causal phase");
            let deadline = Instant::now() + EVENT_TIMEOUT;
            loop {
                if client.tools.values().any(|tool| tool.phase == Phase::Busy && tool.membership.member_id == initiator && tool.round_id.as_ref() == Some(&audit.round_id) && tool.tool == Some(BoardTool::ReadThread) && tool.completion_seq.is_none() && tool.input::<SwarmThreadRead>().thread_id == audit.thread_id) { break; }
                let event = client.next(deadline, "real provider starting the audit thread read").await;
                client.buffer(event);
            }
            let queued = client.post(&swarm_id, &[&initiator], "BUSY_FOLLOWUP. Initiator: only when this post is delivered as a new notification, read Briefing and this exact human thread, publish BUSY_CONFIRMED, and finish. Do not handle it in the audit round. Responder: finish without acting.").await;
            assert!(queued.cursor > audit.cursor && queued.round_id != audit.round_id, "busy follow-up must have its own human-triggered round");
            let (pending_id, pending_observation) = client.busy_pending(&initiator, &audit, &queued).await;
            let audited = client.member_post(&initiator, SwarmBoard::Briefing, "AUDIT_DONE", &audit, queued.cursor, None).await;
            require_publication(&mut client, &initiator, Phase::Busy, &audited).await;
            let mut reads = client.tools.values().filter(|tool| tool.phase == Phase::Busy && tool.membership.member_id == initiator && tool.round_id.as_ref() == Some(&audit.round_id) && matches!(tool.tool, Some(BoardTool::ReadBoard | BoardTool::ReadThread))).collect::<Vec<_>>();
            reads.sort_by_key(|tool| tool.request_seq);
            assert_eq!(reads.len(), AUDIT_PASSES * 2, "busy phase must actually execute all requested provider audit reads");
            assert!(reads.as_chunks::<2>().0.iter().all(|pair| matches!(&pair[0].result, Some(BoardResult::Board(page)) if page.board == SwarmBoard::Briefing && page.posts.contains(&audit)) && matches!(&pair[1].result, Some(BoardResult::Thread(page)) if page.root == audit)), "each real audit pass must successfully read the exact board and human thread");
            assert!(reads.windows(2).all(|pair| pair[0].completion_seq.is_some_and(|completed| completed < pair[1].request_seq)), "the real provider must finish each audit read before starting the next, not fake a long-running parallel batch");
            assert!(reads.iter().any(|tool| tool.request_observation > pending_observation), "real sequential audit work must continue AFTER the canonical busy/Pending snapshot, not merely precede the queued trigger");
            let confirmed = client.member_post(&initiator, SwarmBoard::Briefing, "BUSY_CONFIRMED", &queued, audited.cursor, None).await;
            require_reads(&mut client, &initiator, Phase::Busy, &queued, None).await;
            require_publication(&mut client, &initiator, Phase::Busy, &confirmed).await;
            let accepted = client.swarm(&swarm_id, |swarm| swarm.notifications.iter().any(|intent| intent.id == pending_id && intent.state == SwarmDeliveryState::Accepted)).await;
            client.assert_accepted_once(&accepted, &initiator, &queued);
            client.quiescent(&swarm_id).await;
            client.send(SwarmCommandPayload::ReadBoard { swarm_id: swarm_id.clone(), query: SwarmBoardRead {
                view: protocol::SwarmBoardView::Posts, board: SwarmBoard::Briefing, after_cursor: None, limit: Some(100) } }).await;
            let history: SwarmBoardNotifyPayload = client.wait(FrameKind::SwarmBoardNotify, "busy follow-up publication cardinality", |event: &SwarmBoardNotifyPayload| event.page.swarm_id == swarm_id && event.page.board == SwarmBoard::Briefing && event.page.posts.contains(&confirmed)).await;
            let confirmations = history.page.posts.iter().filter(|post| post.author == (SwarmAuthor::Member { member_id: initiator.clone() }) && post.round_id == queued.round_id && post.body.iter().any(|segment| matches!(segment, SwarmBodySegment::Text { text } if text.contains("BUSY_CONFIRMED")))).count();
            assert_eq!(confirmations, 1, "one busy notification must produce exactly one causal confirmation after native acceptance");
        }).await.expect("real busy-delivery phase exceeded its shared bounded deadline; no qualifying provider audit/busy/pending/acceptance proof");

        client.send(SwarmCommandPayload::Pause { swarm_id: swarm_id.clone() }).await;
        let paused = client.swarm(&swarm_id, |swarm| swarm.lifecycle == SwarmLifecycle::Paused).await;
        let sessions: HashMap<_, _> = paused.members.iter().map(|member| (member.spec.id.clone(), member.session_id.clone().expect("real member owns a session"))).collect();
        // Explicit mentions restrict human Briefing recipients; prose asking
        // both peers does not notify an unmentioned retained session.
        let queued = client.post(&swarm_id, &[&initiator, &responder], "RESUME_CHECK. Both peers: after this notification is delivered, read Briefing and this exact human thread, then each publish one RESUME_CONFIRMED root without mentions.").await;
        let pending = client.swarm(&swarm_id, |swarm| swarm.lifecycle == SwarmLifecycle::Paused && [&initiator, &responder].into_iter().all(|member| swarm.notifications.iter().any(|intent| intent.member_id == *member && intent.post_ids == vec![queued.id.clone()] && intent.round_id == queued.round_id && intent.state == SwarmDeliveryState::Pending))).await;
        assert_eq!(pending.notifications.iter().filter(|intent| intent.post_ids.contains(&queued.id)).count(), 2, "the one explicit human resume trigger must create exactly one Pending intent for EACH peer");
        assert!(pending.members.iter().all(|member| member.context_cursor < queued.cursor), "paused trigger must not be accepted into either private session");
        tokio::time::timeout(SHUTDOWN_TIMEOUT, host.shutdown_for_restart()).await.expect("real swarm restart shutdown exceeded its bounded deadline");
        drop(client);
        host = spawn_host(store.path(), backend);
        let (mut client, bootstrap) = BoardClient::connect(host.clone(), Phase::Replay).await;
        let restored = bootstrap.swarms.iter().find(|swarm| swarm.id == swarm_id).expect("durable swarm appears in reconnect bootstrap");
        assert_eq!(restored.lifecycle, SwarmLifecycle::Paused, "restart must not execute paused communication");
        assert!([&initiator, &responder].into_iter().all(|member| restored.notifications.iter().any(|intent| intent.member_id == *member && intent.post_ids == vec![queued.id.clone()] && intent.round_id == queued.round_id && intent.state == SwarmDeliveryState::Pending)), "BOTH exact paused notifications survive actual host restart with their original cause");
        assert_eq!(restored.notifications.iter().filter(|intent| intent.post_ids.contains(&queued.id)).count(), 2, "restart must preserve exactly the original two intended deliveries");
        assert!(restored.members.iter().all(|member| member.context_cursor < queued.cursor), "restart must not silently accept queued communication");
        if backend_profile(&bootstrap.launch_profile_catalog, backend).is_none() {
            let _: LaunchProfileCatalogPayload = client.wait(FrameKind::LaunchProfileCatalogNotify, "resumed real backend readiness", |payload: &LaunchProfileCatalogPayload| backend_profile(&payload.catalog, backend).is_some()).await;
        }
        client.phase = Phase::Resumed;
        client.send(SwarmCommandPayload::Resume { swarm_id: swarm_id.clone() }).await;
        let mut confirmations = Vec::new();
        for member in [&initiator, &responder] {
            let confirmed = client.member_post(member, SwarmBoard::Briefing, "RESUME_CONFIRMED", &queued, queued.cursor, None).await;
            require_reads(&mut client, member, Phase::Resumed, &queued, None).await;
            require_publication(&mut client, member, Phase::Resumed, &confirmed).await;
            confirmations.push(confirmed);
        }
        let resumed = client.swarm(&swarm_id, |swarm| [&initiator, &responder].into_iter().all(|member| swarm.notifications.iter().any(|intent| intent.member_id == *member && intent.post_ids.contains(&queued.id) && intent.round_id == queued.round_id && intent.state == SwarmDeliveryState::Accepted))).await;
        for member in [&initiator, &responder] { client.assert_accepted_once(&resumed, member, &queued); }
        assert!(resumed.members.iter().all(|member| sessions.get(&member.spec.id) == member.session_id.as_ref()), "real restart must resume existing private sessions, not create replacements");
        client.quiescent(&swarm_id).await;
        client.send(SwarmCommandPayload::ReadBoard { swarm_id: swarm_id.clone(), query: SwarmBoardRead {
                view: protocol::SwarmBoardView::Posts, board: SwarmBoard::Briefing, after_cursor: None, limit: Some(100) } }).await;
        let history: SwarmBoardNotifyPayload = client.wait(FrameKind::SwarmBoardNotify, "resumed confirmation cardinality", |event: &SwarmBoardNotifyPayload| event.page.swarm_id == swarm_id && event.page.board == SwarmBoard::Briefing && confirmations.iter().all(|post| event.page.posts.contains(post))).await;
        for member in [&initiator, &responder] {
            let count = history.page.posts.iter().filter(|post| post.author == (SwarmAuthor::Member { member_id: member.clone() }) && post.round_id == queued.round_id && post.body.iter().any(|segment| matches!(segment, SwarmBodySegment::Text { text } if text.contains("RESUME_CONFIRMED")))).count();
            assert_eq!(count, 1, "one restored notification must produce exactly one new causal confirmation per real member");
        }
        client.send(SwarmCommandPayload::ReadBoard { swarm_id: swarm_id.clone(), query: SwarmBoardRead {
                view: protocol::SwarmBoardView::Posts, board: SwarmBoard::Coordination, after_cursor: None, limit: Some(100) } }).await;
        let history: SwarmBoardNotifyPayload = client.wait(FrameKind::SwarmBoardNotify, "durable coordination after resume", |event: &SwarmBoardNotifyPayload| event.page.swarm_id == swarm_id && event.page.board == SwarmBoard::Coordination && event.page.high_water >= reply.cursor).await;
        assert!(history.page.posts.contains(&request) && history.page.posts.contains(&reply), "exact real peer root and reply survive host restart");
    }).catch_unwind().await;
    let cleanup = std::panic::AssertUnwindSafe(tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        host.shutdown_for_restart(),
    ))
    .catch_unwind()
    .await;
    if let Err(original) = result {
        if !matches!(cleanup, Ok(Ok(()))) {
            eprintln!(
                "real swarm cleanup also failed or exceeded its bounded deadline; preserving original scenario panic"
            );
        }
        std::panic::resume_unwind(original);
    }
    match cleanup {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("real swarm final shutdown exceeded its bounded deadline"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

pub async fn run_images(
    backend: BackendKind,
    workspace: &Path,
    settings: SessionSettingsValues,
    pixels: ImageData,
    answer: &str,
) {
    let store = tempfile::tempdir().expect("create isolated image swarm store");
    let host = spawn_host(store.path(), backend);
    let result = std::panic::AssertUnwindSafe(async {
        let (mut client, bootstrap) = BoardClient::connect(host.clone(), Phase::Opening).await;
        let profile = match backend_profile(&bootstrap.launch_profile_catalog, backend) {
            Some(profile) => profile,
            None => {
                let payload: LaunchProfileCatalogPayload = client.wait(FrameKind::LaunchProfileCatalogNotify, "image provider readiness", |payload: &LaunchProfileCatalogPayload| backend_profile(&payload.catalog, backend).is_some()).await;
                backend_profile(&payload.catalog, backend).expect("ready real launch profile")
            }
        };
        let schema = match backend_schema(&bootstrap.session_schemas, backend) {
            Some(schema) => schema,
            None => {
                let payload: SessionSchemasPayload = client.wait(FrameKind::SessionSchemas, "image model catalog", |payload: &SessionSchemasPayload| backend_schema(&payload.schemas, backend).is_some()).await;
                backend_schema(&payload.schemas, backend).expect("ready real model catalog")
            }
        };
        let settings = catalog_settings(&schema, settings);
        tokio::time::timeout(CONNECTION_TIMEOUT, client.connection.project_create(ProjectCreatePayload { name:"Shared image conformance".into(), roots:vec![ProjectRootPath(workspace.to_string_lossy().into_owned())] })).await.expect("bounded project registration").expect("project transport");
        let event: ProjectNotifyPayload = client.wait(FrameKind::ProjectNotify, "image project registration", |event| matches!(event, ProjectNotifyPayload::Upsert { .. })).await;
        let ProjectNotifyPayload::Upsert { project } = event else { unreachable!() };
        let prompt = match backend {
            BackendKind::Codex => "Use the named tyde-agent-control MCP tools DIRECTLY through their native interfaces. Do not use functions.exec, exec, code-mode wrappers or tool-discovery code: code mode is disabled. ",
            BackendKind::Claude => "Use the named tyde-agent-control MCP tools directly. Perform exactly these three actual tool calls in order, awaiting each successful result: (1) tyde_swarm_read_board for Briefing; (2) tyde_swarm_read_image for its shared attachment; (3) tyde_swarm_post the result. The board read is mandatory even when inline notification context already contains the human post and image ID; inline context is not a substitute for the actual read. Do not publish before both reads succeed. ",
            _ => "Use the named tyde-agent-control MCP tools directly. ",
        }.to_owned() + "Read the shared Briefing board with tyde_swarm_read_board. Find the human post with an image attachment. Call tyde_swarm_read_image with its exact image_id to view the actual pixels; metadata and filenames do not contain the answer. The image contains three equal vertical solid-color bands. Publish exactly one concise human-facing reply using tyde_swarm_post in the original human image request thread, with its exact thread_id, with one text segment containing IMAGE_RESULT followed by a space and the three lowercase CSS color names from left to right separated by colons. Use a fresh publication_id. Do not guess before reading the image. Do not create files or use any agent tools. A private final answer is not a board publication. Finish after posting.";
        let draft_id = SwarmDraftId(uuid::Uuid::new_v4().to_string());
        client.send(SwarmCommandPayload::GenerateDraft { draft_id:draft_id.clone(), expected_revision:None, name:"Shared pixels".into(), opening_brief:String::new(), constraints:SwarmConstraints {
            project_id:project.id, workspace_policy:SwarmWorkspacePolicy::ReadOnly, max_live_agents:1,
            allocations:vec![SwarmBackendAllocation {backend_kind:backend,launch_profile_id:profile.id,session_settings:settings,count:1}],shared_guidance:String::new(),agent_wake_budget:2,
        }}).await;
        let event: SwarmDraftNotifyPayload = client.wait(FrameKind::SwarmDraftNotify, "image draft", |event| matches!(event, SwarmDraftNotifyPayload::Upsert {draft} if draft.id == draft_id)).await;
        let SwarmDraftNotifyPayload::Upsert {draft} = event else { unreachable!() };
        assert!(draft.conflicts.is_empty());
        let member = draft.members[0].id.clone();
        client.send(SwarmCommandPayload::Launch {draft_id:draft.id.clone(),expected_revision:draft.revision}).await;
        let event: SwarmNotifyPayload = client.wait(FrameKind::SwarmNotify, "image swarm launch", |event: &SwarmNotifyPayload| event.swarm.source_draft_id.as_ref() == Some(&draft.id)).await;
        let swarm_id = event.swarm.id;
        assert!(event.swarm.opening_post_id.is_none(), "upload precedes the first provider wake");
        let image_id = SwarmImageId(uuid::Uuid::new_v4().to_string());
        client.send(SwarmCommandPayload::UploadImage {swarm_id:swarm_id.clone(),image:SwarmImageUpload {image_id:image_id.clone(),name:"shared-reference.png".into(),data:pixels.clone()}}).await;
        let uploaded: SwarmImageNotifyPayload = client.wait(FrameKind::SwarmImageNotify, "actual image upload", |event: &SwarmImageNotifyPayload| event.swarm_id == swarm_id && event.image_id == image_id).await;
        let SwarmImageOutcome::Ready {image, data:None} = uploaded.outcome else {panic!("valid real image must be durably stored")};
        let publication_id = SwarmPublicationId(uuid::Uuid::new_v4().to_string());
        client.send(SwarmCommandPayload::Post {swarm_id:swarm_id.clone(),publication:SwarmPublication {publication_id:publication_id.clone(),board:SwarmBoard::Briefing,thread_id:None,body:vec![SwarmBodySegment::Text {text:prompt}],attachments:Vec::new(),images:vec![image_id.clone()]}}).await;
        let trigger: SwarmPostNotifyPayload = client.wait(FrameKind::SwarmPostNotify, "shared image trigger", |event: &SwarmPostNotifyPayload| event.post.publication_id == publication_id && event.post.author == SwarmAuthor::Human).await;
        let posted = client.member_post(&member, SwarmBoard::Briefing, "IMAGE_RESULT", &trigger.post, trigger.post.cursor, Some(&trigger.post.thread_id)).await;
        let text = posted.body.iter().map(|segment| match segment {SwarmBodySegment::Text {text} => text.as_str(), _ => panic!("image result must be ordinary text")}).collect::<String>().trim().to_ascii_lowercase().replace("fuchsia", "magenta");
        assert!(text == format!("image_result {answer}"), "real member must identify the actual shared pixels correctly; backend={backend:?}");
        client.require_tool(&member, Phase::Opening, BoardTool::ReadImage, |result| matches!(result, BoardResult::Image(metadata, data) if **metadata == image && *data == pixels)).await;
        client.require_tool(&member, Phase::Opening, BoardTool::ReadBoard, |result| matches!(result, BoardResult::Board(page) if page.posts.contains(&trigger.post))).await;
        require_publication(&mut client, &member, Phase::Opening, &posted).await;
        client.quiescent(&swarm_id).await;
        eprintln!("Shared image conformance passed: backend={backend:?}; actual image read and authenticated pixel answer published");
    }).catch_unwind().await;
    let cleanup = tokio::time::timeout(SHUTDOWN_TIMEOUT, host.shutdown_for_restart()).await;
    if let Err(panic) = result {
        if cleanup.is_err() {
            eprintln!("image host cleanup exceeded deadline; preserving scenario failure");
        }
        std::panic::resume_unwind(panic);
    }
    cleanup.expect("bounded image host shutdown");
}
