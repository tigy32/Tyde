mod fixture;

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentId, AgentOrigin, BackendAccessMode, BackendKind, CancelWorkflowPayload, ChatEvent,
    CommandErrorCode, CommandErrorPayload, Envelope, FrameKind, HostBootstrapPayload,
    NewAgentPayload, Project, ProjectAddRootPayload, ProjectCreatePayload,
    ProjectDeleteRootPayload, ProjectNotifyPayload, ProjectRootPath, SpawnAgentParams,
    SpawnAgentPayload, StreamPath, TriggerWorkflowPayload, WorkflowId, WorkflowNotifyPayload,
    WorkflowRunId, WorkflowRunNotifyPayload, WorkflowRunSnapshot, WorkflowRunSnapshotStatus,
    WorkflowSaveResponse, WorkflowStepRunSnapshotStatus, WorkflowTargetsResponse,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Value, json};

static GLOBAL_WORKFLOWS_ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

struct GlobalWorkflowsEnv {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
    dir: tempfile::TempDir,
}

impl GlobalWorkflowsEnv {
    async fn new() -> Self {
        let guard = GLOBAL_WORKFLOWS_ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let previous = std::env::var("TYDE_GLOBAL_WORKFLOWS_DIR").ok();
        let dir = tempfile::tempdir().expect("create global workflows tempdir");
        unsafe {
            std::env::set_var("TYDE_GLOBAL_WORKFLOWS_DIR", dir.path());
        }
        Self {
            _guard: guard,
            previous,
            dir,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for GlobalWorkflowsEnv {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("TYDE_GLOBAL_WORKFLOWS_DIR", previous);
            } else {
                std::env::remove_var("TYDE_GLOBAL_WORKFLOWS_DIR");
            }
        }
    }
}

async fn next_event(client: &mut client::Connection, context: &str) -> Envelope {
    loop {
        let env = match tokio::time::timeout(Duration::from_secs(5), client.next_event()).await {
            Ok(Ok(Some(env))) => env,
            Ok(Ok(None)) => panic!("connection closed before {context}"),
            Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
            Err(_) => panic!("timed out waiting for {context}"),
        };
        if fixture::is_builtin_team_custom_agent_notify(&env)
            || matches!(
                env.kind,
                FrameKind::SessionSchemas
                    | FrameKind::BackendSetup
                    | FrameKind::AgentsViewPreferencesNotify
                    | FrameKind::TeamPresetCatalogNotify
                    | FrameKind::SkillNotify
                    | FrameKind::CustomAgentNotify
                    | FrameKind::McpServerNotify
                    | FrameKind::SteeringNotify
            )
        {
            continue;
        }
        return env;
    }
}

async fn create_project(client: &mut client::Connection, root: &str) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: "Workflow Project".to_owned(),
            roots: vec![ProjectRootPath(root.to_owned())],
        })
        .await
        .expect("project_create failed");
    loop {
        let env = next_event(client, "project create").await;
        if env.kind == FrameKind::ProjectNotify {
            match env
                .parse_payload::<ProjectNotifyPayload>()
                .expect("ProjectNotify")
            {
                ProjectNotifyPayload::Upsert { project } => return project,
                ProjectNotifyPayload::Delete { .. } => continue,
            }
        }
    }
}

fn write_workflow(root: &std::path::Path, access_mode: &str) {
    let dir = root.join(".tyde/workflows");
    std::fs::create_dir_all(&dir).expect("create workflow dir");
    std::fs::write(
        dir.join("build.md"),
        workflow_markdown_with_access("build", "Build Project", "Run the build.", access_mode),
    )
    .expect("write workflow");
    std::fs::write(dir.join("bad.md"), "---\nid: bad\n").expect("write bad workflow");
}

fn workflow_markdown(id: &str, name: &str, body: &str) -> String {
    workflow_markdown_with_access(id, name, body, "read_only")
}

fn workflow_markdown_with_access(id: &str, name: &str, body: &str, access_mode: &str) -> String {
    format!(
        "---\nid: {id}\nname: {name}\ndescription: Compile and test\ncoordinator:\n  backend: codex\n  access_mode: {access_mode}\ndeclared_backends: [codex]\ntriggers: [global]\n---\n{body}\n"
    )
}

fn workflow_markdown_with_inputs(id: &str, name: &str) -> String {
    format!(
        "---\nid: {id}\nname: {name}\ndescription: Run with typed inputs\ncoordinator:\n  backend: codex\n  access_mode: read_only\ndeclared_backends: [codex]\ntriggers: [global]\ninputs:\n  - id: target\n    name: Target\n    required: true\n    control: text\n  - id: notes\n    control: multiline_text\n    default: \"Use defaults\"\n  - id: enabled\n    control: boolean\n    default: true\n  - id: retries\n    control: number\n    default: 3\n  - id: mode\n    control: select\n    options:\n      - value: fast\n        label: Fast\n      - value: safe\n        label: Safe\n    default: safe\n  - id: config_path\n    control: file_path\n---\nValidate the supplied inputs.\n"
    )
}

async fn wait_for_workflow_catalog(client: &mut client::Connection) {
    loop {
        let env = next_event(client, "workflow catalog").await;
        if env.kind == FrameKind::WorkflowNotify {
            break;
        }
    }
}

async fn wait_for_workflow_notify<F>(
    client: &mut client::Connection,
    context: &str,
    mut predicate: F,
) -> WorkflowNotifyPayload
where
    F: FnMut(&WorkflowNotifyPayload) -> bool,
{
    for _ in 0..30 {
        let env = next_event(client, context).await;
        if env.kind != FrameKind::WorkflowNotify {
            continue;
        }
        let payload: WorkflowNotifyPayload = env.parse_payload().expect("WorkflowNotify");
        if predicate(&payload) {
            return payload;
        }
    }
    panic!("timed out waiting for matching WorkflowNotify: {context}");
}

async fn wait_for_workflow_command_error(
    client: &mut client::Connection,
    context: &str,
) -> CommandErrorPayload {
    for _ in 0..20 {
        let env = next_event(client, context).await;
        match env.kind {
            FrameKind::CommandError => {
                return env.parse_payload().expect("CommandErrorPayload");
            }
            FrameKind::WorkflowRunNotify => {
                let payload: WorkflowRunNotifyPayload =
                    env.parse_payload().expect("WorkflowRunNotify");
                if payload.run.status == WorkflowRunSnapshotStatus::Running {
                    panic!(
                        "workflow run was created before expected command error {context}: {:?}",
                        payload.run
                    );
                }
            }
            _ => {}
        }
    }
    panic!("timed out waiting for workflow command error: {context}");
}

async fn collect_turn_delta_text(
    client: &mut client::Connection,
    stream: &StreamPath,
    context: &str,
) -> String {
    let mut text = String::new();
    let mut saw_turn = false;
    loop {
        let env = next_event(client, context).await;
        if env.stream != *stream || env.kind != FrameKind::ChatEvent {
            continue;
        }
        let event: ChatEvent = env.parse_payload().expect("ChatEvent");
        match event {
            ChatEvent::TypingStatusChanged(true) => saw_turn = true,
            ChatEvent::StreamDelta(delta) => text.push_str(&delta.text),
            ChatEvent::StreamEnd(end) => text.push_str(&end.message.content),
            ChatEvent::TypingStatusChanged(false) if saw_turn => return text,
            _ => {}
        }
    }
}

async fn trigger_workflow_and_wait_for_coordinator(
    client: &mut client::Connection,
    project_id: protocol::ProjectId,
) -> (WorkflowRunId, NewAgentPayload) {
    client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("build".to_owned()),
            project_id: Some(project_id),
            inputs: HashMap::new(),
        })
        .await
        .expect("trigger_workflow failed");

    let mut run_id = None;
    let mut coordinator = None;
    for _ in 0..20 {
        let env = next_event(client, "workflow coordinator").await;
        match env.kind {
            FrameKind::WorkflowRunNotify => {
                let payload: WorkflowRunNotifyPayload =
                    env.parse_payload().expect("WorkflowRunNotify");
                assert_eq!(payload.run.workflow_id, WorkflowId("build".to_owned()));
                if payload.run.status == WorkflowRunSnapshotStatus::Running {
                    run_id = Some(payload.run.id);
                }
            }
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
                if payload.origin == AgentOrigin::Workflow {
                    coordinator = Some(payload);
                }
            }
            _ => {}
        }
        if run_id.is_some() && coordinator.is_some() {
            break;
        }
    }

    (
        run_id.expect("workflow run notify missing"),
        coordinator.expect("workflow coordinator NewAgent missing"),
    )
}

async fn call_mcp_tool(
    base_url: &str,
    caller_agent_id: &AgentId,
    name: &str,
    arguments: Value,
) -> (bool, String) {
    call_mcp_tool_optional_caller(base_url, Some(caller_agent_id), name, arguments).await
}

async fn call_mcp_tool_optional_caller(
    base_url: &str,
    caller_agent_id: Option<&AgentId>,
    name: &str,
    arguments: Value,
) -> (bool, String) {
    let url = if let Some(caller_agent_id) = caller_agent_id {
        let separator = if base_url.contains('?') { '&' } else { '?' };
        format!("{base_url}{separator}agent_id={}", caller_agent_id.0)
    } else {
        base_url.to_owned()
    };
    let transport = StreamableHttpClientTransport::from_uri(url);
    let service = ().serve(transport).await.expect("connect to MCP");
    let arguments = arguments
        .as_object()
        .cloned()
        .unwrap_or_else(|| panic!("MCP arguments for {name} must be a JSON object"));
    let result = service
        .call_tool(CallToolRequestParams {
            meta: None,
            name: name.to_owned().into(),
            arguments: Some(arguments),
            task: None,
        })
        .await
        .unwrap_or_else(|err| panic!("call {name} failed: {err}"));
    let is_error = result.is_error.unwrap_or(false);
    let content = result
        .content
        .first()
        .unwrap_or_else(|| panic!("{name} result should include content"));
    let RawContent::Text(text) = &content.raw else {
        panic!(
            "expected text tool result for {name}, got {:?}",
            content.raw
        );
    };
    let body = text.text.clone();
    service.cancel().await.expect("cancel MCP client");
    (is_error, body)
}

async fn call_mcp_json_tool(
    base_url: &str,
    caller_agent_id: &AgentId,
    name: &str,
    arguments: Value,
) -> WorkflowRunSnapshot {
    let (is_error, body) = call_mcp_tool(base_url, caller_agent_id, name, arguments).await;
    assert!(!is_error, "MCP tool {name} returned error: {body}");
    serde_json::from_str(&body).unwrap_or_else(|err| panic!("parse {name} JSON result: {err}"))
}

async fn call_agent_control_json<T: serde::de::DeserializeOwned>(
    base_url: &str,
    caller_agent_id: &AgentId,
    name: &str,
    arguments: Value,
) -> T {
    let (is_error, body) = call_mcp_tool(base_url, caller_agent_id, name, arguments).await;
    assert!(!is_error, "MCP tool {name} returned error: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("parse {name} JSON result: {err}: {body}"))
}

async fn spawn_test_agent(
    client: &mut client::Connection,
    root: &Path,
    project_id: Option<protocol::ProjectId>,
    access_mode: BackendAccessMode,
    name: &str,
) -> NewAgentPayload {
    client
        .spawn_agent(SpawnAgentPayload {
            name: Some(name.to_owned()),
            custom_agent_id: None,
            parent_agent_id: None,
            project_id,
            params: SpawnAgentParams::New {
                workspace_roots: vec![root.display().to_string()],
                prompt: "workflow author".to_owned(),
                images: None,
                backend_kind: BackendKind::Codex,
                cost_hint: None,
                access_mode,
                session_settings: None,
            },
        })
        .await
        .expect("spawn_agent failed");

    loop {
        let env = next_event(client, "spawn test agent").await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if payload.name == name {
            return payload;
        }
    }
}

#[tokio::test]
async fn workflow_refresh_reports_catalog_and_diagnostics() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "read_only");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;

    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");

    loop {
        let env = next_event(&mut fixture.client, "workflow notify").await;
        if env.kind != FrameKind::WorkflowNotify {
            continue;
        }
        let payload: WorkflowNotifyPayload = env.parse_payload().expect("WorkflowNotify");
        assert_eq!(payload.summaries.len(), 1);
        assert_eq!(payload.summaries[0].id, WorkflowId("build".to_owned()));
        assert!(matches!(
            payload.summaries[0].source.scope,
            protocol::WorkflowSourceScope::Project { ref project_id, .. } if project_id == &project.id
        ));
        assert!(!payload.diagnostics.is_empty());
        return;
    }
}

#[tokio::test]
async fn trigger_workflow_spawns_workflow_origin_coordinator() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "read_only");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_catalog(&mut fixture.client).await;

    let (run_id, coordinator) =
        trigger_workflow_and_wait_for_coordinator(&mut fixture.client, project.id).await;
    let metadata = coordinator.workflow.expect("workflow metadata missing");
    assert_eq!(metadata.workflow_id, WorkflowId("build".to_owned()));
    assert_eq!(metadata.workflow_run_id, run_id);
}

#[tokio::test]
async fn workflow_trigger_input_validation_rejects_bad_runs() {
    let global = GlobalWorkflowsEnv::new().await;
    std::fs::write(
        global.path().join("input-flow.md"),
        workflow_markdown_with_inputs("input-flow", "Input Flow"),
    )
    .expect("write input workflow");
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_notify(&mut fixture.client, "input workflow catalog", |payload| {
        payload
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("input-flow".to_owned()))
    })
    .await;

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("input-flow".to_owned()),
            project_id: None,
            inputs: HashMap::new(),
        })
        .await
        .expect("send missing required trigger");
    let error = wait_for_workflow_command_error(&mut fixture.client, "missing input").await;
    assert_eq!(error.request_kind, FrameKind::TriggerWorkflow);
    assert_eq!(error.operation, "trigger_workflow");
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("missing required input \"target\""),
        "unexpected missing-input message: {}",
        error.message
    );

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("input-flow".to_owned()),
            project_id: None,
            inputs: HashMap::from([
                ("target".to_owned(), json!("src/lib.rs")),
                ("unexpected".to_owned(), json!("nope")),
            ]),
        })
        .await
        .expect("send unknown input trigger");
    let error = wait_for_workflow_command_error(&mut fixture.client, "unknown input").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("unknown input \"unexpected\""),
        "unexpected unknown-input message: {}",
        error.message
    );

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("input-flow".to_owned()),
            project_id: None,
            inputs: HashMap::from([
                ("target".to_owned(), json!("src/lib.rs")),
                ("enabled".to_owned(), json!("yes")),
            ]),
        })
        .await
        .expect("send type mismatch trigger");
    let error = wait_for_workflow_command_error(&mut fixture.client, "type mismatch").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("input \"enabled\"") && error.message.contains("must be a boolean"),
        "unexpected type-mismatch message: {}",
        error.message
    );

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("input-flow".to_owned()),
            project_id: None,
            inputs: HashMap::from([
                ("target".to_owned(), json!("src/lib.rs")),
                ("mode".to_owned(), json!("turbo")),
            ]),
        })
        .await
        .expect("send invalid select trigger");
    let error = wait_for_workflow_command_error(&mut fixture.client, "invalid select").await;
    assert_eq!(error.code, CommandErrorCode::InvalidInput);
    assert!(
        error.message.contains("input \"mode\"") && error.message.contains("select option values"),
        "unexpected select message: {}",
        error.message
    );

    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap.workflow_runs.is_empty(),
        "invalid triggers should not create workflow runs"
    );
}

#[tokio::test]
async fn workflow_trigger_applies_defaults_and_spawns_coordinator() {
    let global = GlobalWorkflowsEnv::new().await;
    std::fs::write(
        global.path().join("input-flow.md"),
        workflow_markdown_with_inputs("input-flow", "Input Flow"),
    )
    .expect("write input workflow");
    let mut fixture = Fixture::new().await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_notify(&mut fixture.client, "input workflow catalog", |payload| {
        payload
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("input-flow".to_owned()))
    })
    .await;

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("input-flow".to_owned()),
            project_id: None,
            inputs: HashMap::from([
                ("target".to_owned(), json!("src/lib.rs")),
                ("config_path".to_owned(), json!("/tmp/config.toml")),
            ]),
        })
        .await
        .expect("trigger workflow with valid inputs");

    let mut run = None;
    let mut coordinator = None;
    for _ in 0..30 {
        let env = next_event(&mut fixture.client, "valid input workflow").await;
        match env.kind {
            FrameKind::WorkflowRunNotify => {
                let payload: WorkflowRunNotifyPayload =
                    env.parse_payload().expect("WorkflowRunNotify");
                if payload.run.workflow_id == WorkflowId("input-flow".to_owned())
                    && payload.run.status == WorkflowRunSnapshotStatus::Running
                {
                    run = Some(payload.run);
                }
            }
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
                if payload.origin == AgentOrigin::Workflow && payload.name == "Workflow: Input Flow"
                {
                    coordinator = Some(payload);
                }
            }
            _ => {}
        }
        if run.is_some() && coordinator.is_some() {
            break;
        }
    }
    let run = run.expect("workflow run notify missing");
    assert_eq!(run.inputs.get("target"), Some(&json!("src/lib.rs")));
    assert_eq!(
        run.inputs.get("config_path"),
        Some(&json!("/tmp/config.toml"))
    );
    assert_eq!(run.inputs.get("notes"), Some(&json!("Use defaults")));
    assert_eq!(run.inputs.get("enabled"), Some(&json!(true)));
    assert_eq!(run.inputs.get("retries"), Some(&json!(3)));
    assert_eq!(run.inputs.get("mode"), Some(&json!("safe")));

    let coordinator = coordinator.expect("workflow coordinator missing");
    let response = collect_turn_delta_text(
        &mut fixture.client,
        &coordinator.instance_stream,
        "coordinator prompt with inputs",
    )
    .await;
    assert!(
        response.contains("\"target\": \"src/lib.rs\""),
        "coordinator prompt did not include supplied target: {response}"
    );
    assert!(
        response.contains("\"mode\": \"safe\""),
        "coordinator prompt did not include defaulted select: {response}"
    );
    assert!(
        response.contains("\"enabled\": true"),
        "coordinator prompt did not include defaulted boolean: {response}"
    );
}

#[tokio::test]
async fn workflow_progress_finish_and_reconnect_replay_run() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "read_only");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_catalog(&mut fixture.client).await;
    let (run_id, coordinator) =
        trigger_workflow_and_wait_for_coordinator(&mut fixture.client, project.id).await;

    let workflow_url = fixture.workflow_mcp_http_url().await;
    let step_snapshot = call_mcp_json_tool(
        &workflow_url,
        &coordinator.agent_id,
        "tyde_workflow_report_step",
        json!({
            "step_id": "compile",
            "title": "Compile",
            "status": "running",
            "message": "Compiling"
        }),
    )
    .await;
    assert_eq!(step_snapshot.id, run_id);
    assert_eq!(step_snapshot.steps.len(), 1);
    assert_eq!(
        step_snapshot.steps[0].status,
        WorkflowStepRunSnapshotStatus::Running
    );

    loop {
        let env = next_event(&mut fixture.client, "workflow step notify").await;
        if env.kind != FrameKind::WorkflowRunNotify {
            continue;
        }
        let payload: WorkflowRunNotifyPayload = env.parse_payload().expect("WorkflowRunNotify");
        if payload.run.id == step_snapshot.id && !payload.run.steps.is_empty() {
            assert_eq!(payload.run.steps[0].title, "Compile");
            break;
        }
    }

    let finished = call_mcp_json_tool(
        &workflow_url,
        &coordinator.agent_id,
        "tyde_workflow_finish",
        json!({
            "status": "completed",
            "summary": "Build completed"
        }),
    )
    .await;
    assert_eq!(finished.status, WorkflowRunSnapshotStatus::Completed);
    assert_eq!(finished.summary.as_deref(), Some("Build completed"));

    let (_reconnect, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrap
        .workflow_runs
        .iter()
        .find(|run| run.id == finished.id)
        .expect("bootstrap should replay workflow run");
    assert_eq!(replayed.status, WorkflowRunSnapshotStatus::Completed);
    assert_eq!(replayed.summary.as_deref(), Some("Build completed"));
}

#[tokio::test]
async fn workflow_cancel_marks_run_cancelled_and_replays_agent() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "read_only");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_catalog(&mut fixture.client).await;
    let (run_id, coordinator) =
        trigger_workflow_and_wait_for_coordinator(&mut fixture.client, project.id).await;

    fixture
        .client
        .cancel_workflow(CancelWorkflowPayload {
            run_id: run_id.clone(),
        })
        .await
        .expect("cancel_workflow failed");

    loop {
        let env = next_event(&mut fixture.client, "workflow cancel notify").await;
        if env.kind != FrameKind::WorkflowRunNotify {
            continue;
        }
        let payload: WorkflowRunNotifyPayload = env.parse_payload().expect("WorkflowRunNotify");
        if payload.run.id == run_id && payload.run.status == WorkflowRunSnapshotStatus::Cancelled {
            assert!(payload.run.agent_ids.contains(&coordinator.agent_id));
            break;
        }
    }

    let (_reconnect, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        bootstrap
            .agents
            .iter()
            .any(|agent| agent.agent_id == coordinator.agent_id),
        "cancelled workflow should keep coordinator agent for replay"
    );
}

#[tokio::test]
async fn late_workflow_finish_after_cancel_cannot_resurrect_run() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "read_only");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_catalog(&mut fixture.client).await;
    let (run_id, coordinator) =
        trigger_workflow_and_wait_for_coordinator(&mut fixture.client, project.id).await;

    fixture
        .client
        .cancel_workflow(CancelWorkflowPayload {
            run_id: run_id.clone(),
        })
        .await
        .expect("cancel_workflow failed");

    loop {
        let env = next_event(&mut fixture.client, "workflow cancel notify").await;
        if env.kind != FrameKind::WorkflowRunNotify {
            continue;
        }
        let payload: WorkflowRunNotifyPayload = env.parse_payload().expect("WorkflowRunNotify");
        if payload.run.id == run_id && payload.run.status == WorkflowRunSnapshotStatus::Cancelled {
            break;
        }
    }

    let workflow_url = fixture.workflow_mcp_http_url().await;
    let (is_error, body) = call_mcp_tool(
        &workflow_url,
        &coordinator.agent_id,
        "tyde_workflow_finish",
        json!({
            "status": "completed",
            "summary": "Too late"
        }),
    )
    .await;
    assert!(is_error, "late finish should be rejected: {body}");
    assert!(
        body.contains("already cancelled"),
        "unexpected late finish error: {body}"
    );

    let (_reconnect, bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = bootstrap
        .workflow_runs
        .iter()
        .find(|run| run.id == run_id)
        .expect("bootstrap should replay cancelled workflow run");
    assert_eq!(replayed.status, WorkflowRunSnapshotStatus::Cancelled);
    assert_ne!(replayed.summary.as_deref(), Some("Too late"));
}

#[tokio::test]
async fn workflow_child_agents_inherit_context_and_backend_allowlist() {
    let _global = GlobalWorkflowsEnv::new().await;
    let tmp = tempfile::tempdir().expect("create tempdir");
    write_workflow(tmp.path(), "unrestricted");
    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &tmp.path().display().to_string()).await;
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_catalog(&mut fixture.client).await;
    let (run_id, coordinator) =
        trigger_workflow_and_wait_for_coordinator(&mut fixture.client, project.id).await;
    assert_eq!(coordinator.backend_kind, BackendKind::Codex);

    let agent_control_url = fixture.agent_control_http_url().await;
    let (is_error, error_body) = call_mcp_tool(
        &agent_control_url,
        &coordinator.agent_id,
        "tyde_spawn_agent",
        json!({
            "workspace_roots": [tmp.path().display().to_string()],
            "prompt": "Use an undeclared backend.",
            "backend_kind": "claude",
            "name": "undeclared"
        }),
    )
    .await;
    assert!(
        is_error,
        "undeclared workflow child backend should be rejected: {error_body}"
    );
    assert!(
        error_body.contains("did not declare backend"),
        "unexpected backend rejection: {error_body}"
    );

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &coordinator.agent_id,
        "tyde_spawn_agent",
        json!({
            "workspace_roots": [tmp.path().display().to_string()],
            "prompt": "Use a declared backend.",
            "backend_kind": "codex",
            "name": "declared"
        }),
    )
    .await;
    assert!(!is_error, "declared workflow child backend failed: {body}");
    let result: Value = serde_json::from_str(&body).expect("parse spawn result JSON");
    let child_agent_id = AgentId(
        result
            .get("agent_id")
            .and_then(Value::as_str)
            .expect("spawn result missing agent_id")
            .to_owned(),
    );

    loop {
        let env = next_event(&mut fixture.client, "workflow child NewAgent").await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let child: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if child.agent_id != child_agent_id {
            continue;
        }
        assert_eq!(child.origin, AgentOrigin::Workflow);
        assert_eq!(child.parent_agent_id, Some(coordinator.agent_id.clone()));
        let metadata = child.workflow.expect("child workflow metadata missing");
        assert_eq!(metadata.workflow_id, WorkflowId("build".to_owned()));
        assert_eq!(metadata.workflow_run_id, run_id);
        break;
    }
}

#[tokio::test]
async fn workflow_save_mcp_notifies_and_triggers_without_refresh() {
    let _global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        &project_root.path().display().to_string(),
    )
    .await;
    let author = spawn_test_agent(
        &mut fixture.client,
        project_root.path(),
        Some(project.id.clone()),
        BackendAccessMode::Unrestricted,
        "workflow-author",
    )
    .await;
    let agent_control_url = fixture.agent_control_http_url().await;

    let targets: WorkflowTargetsResponse = call_agent_control_json(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_targets",
        json!({}),
    )
    .await;
    assert!(targets.targets.iter().any(|target| {
        matches!(&target.target, protocol::WorkflowSaveTarget::Global)
            && target.location.directory == _global.path().display().to_string()
    }));
    assert!(targets.targets.iter().any(|target| {
        matches!(
            &target.target,
            protocol::WorkflowSaveTarget::Project { project_id, root }
                if project_id == &project.id && root.0 == project_root.path().display().to_string()
        )
    }));

    let save: WorkflowSaveResponse = call_agent_control_json(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": {
                "kind": "project",
                "project_id": project.id.0.clone(),
                "root": project_root.path().display().to_string()
            },
            "mode": { "mode": "create" },
            "filename": "agent-build.md",
            "markdown": workflow_markdown("agent-build", "Agent Build", "Run agent-authored build.")
        }),
    )
    .await;
    assert!(save.created);
    assert_eq!(save.summary.id, WorkflowId("agent-build".to_owned()));
    assert!(save.path.ends_with("agent-build.md"));
    assert!(std::path::Path::new(&save.path).is_file());

    let notify =
        wait_for_workflow_notify(&mut fixture.client, "saved workflow notify", |payload| {
            payload
                .summaries
                .iter()
                .any(|summary| summary.id == WorkflowId("agent-build".to_owned()))
        })
        .await;
    assert!(!notify.locations.is_empty());

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("agent-build".to_owned()),
            project_id: Some(project.id.clone()),
            inputs: HashMap::new(),
        })
        .await
        .expect("trigger_workflow failed");

    let mut saw_run = false;
    let mut saw_coordinator = false;
    for _ in 0..20 {
        let env = next_event(&mut fixture.client, "saved workflow trigger").await;
        match env.kind {
            FrameKind::WorkflowRunNotify => {
                let payload: WorkflowRunNotifyPayload =
                    env.parse_payload().expect("WorkflowRunNotify");
                saw_run |= payload.run.workflow_id == WorkflowId("agent-build".to_owned());
            }
            FrameKind::NewAgent => {
                let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
                saw_coordinator |= payload.origin == AgentOrigin::Workflow
                    && payload.name == "Workflow: Agent Build";
            }
            _ => {}
        }
        if saw_run && saw_coordinator {
            return;
        }
    }
    panic!("saved workflow did not spawn a workflow-origin coordinator");
}

#[tokio::test]
async fn workflow_save_rejects_read_only_without_catalog_change() {
    let _global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let readonly = spawn_test_agent(
        &mut fixture.client,
        project_root.path(),
        None,
        BackendAccessMode::ReadOnly,
        "readonly-author",
    )
    .await;
    let agent_control_url = fixture.agent_control_http_url().await;
    let target_file = _global.path().join("readonly.md");

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &readonly.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": { "mode": "create" },
            "filename": "readonly.md",
            "markdown": workflow_markdown("readonly", "Readonly", "Should not save.")
        }),
    )
    .await;
    assert!(is_error, "read-only save should fail: {body}");
    assert!(
        body.contains("BackendAccessMode::ReadOnly rejects mutating MCP tool 'tyde_workflow_save'"),
        "unexpected read-only error: {body}"
    );
    assert!(!target_file.exists(), "read-only save wrote a file");

    let (_client, bootstrap) = fixture.connect_with_bootstrap().await;
    assert!(
        !bootstrap
            .workflow_summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("readonly".to_owned())),
        "read-only rejection changed the workflow catalog"
    );
}

#[tokio::test]
async fn workflow_save_create_replace_collision_semantics() {
    let _global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        &project_root.path().display().to_string(),
    )
    .await;
    let author = spawn_test_agent(
        &mut fixture.client,
        project_root.path(),
        Some(project.id.clone()),
        BackendAccessMode::Unrestricted,
        "collision-author",
    )
    .await;
    let agent_control_url = fixture.agent_control_http_url().await;

    let global_save: WorkflowSaveResponse = call_agent_control_json(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": { "mode": "create" },
            "filename": "collision.md",
            "markdown": workflow_markdown("collision", "Global Collision", "Global body.")
        }),
    )
    .await;
    assert!(global_save.created);

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": { "mode": "create" },
            "filename": "collision.md",
            "markdown": workflow_markdown("other-id", "Other", "Other body.")
        }),
    )
    .await;
    assert!(
        is_error && body.contains("already exists"),
        "unexpected filename collision: {body}"
    );

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": { "mode": "create" },
            "filename": "collision-copy.md",
            "markdown": workflow_markdown("collision", "Duplicate", "Duplicate body.")
        }),
    )
    .await;
    assert!(
        is_error && body.contains("same scope"),
        "unexpected id collision: {body}"
    );

    let project_shadow: WorkflowSaveResponse = call_agent_control_json(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": {
                "kind": "project",
                "project_id": project.id.0.clone(),
                "root": project_root.path().display().to_string()
            },
            "mode": { "mode": "create" },
            "filename": "collision.md",
            "markdown": workflow_markdown("collision", "Project Collision", "Project body.")
        }),
    )
    .await;
    assert!(project_shadow.created);
    assert!(
        project_shadow
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("shadows")),
        "project shadow save should return a warning diagnostic"
    );

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": {
                "mode": "replace",
                "existing_path": "/not/the/computed/path.md",
                "existing_id": "collision"
            },
            "filename": "collision.md",
            "markdown": workflow_markdown("collision", "Global Collision", "Updated body.")
        }),
    )
    .await;
    assert!(
        is_error && body.contains("does not match target path"),
        "unexpected path mismatch: {body}"
    );

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": {
                "mode": "replace",
                "existing_path": global_save.path,
                "existing_id": "not-current"
            },
            "filename": "collision.md",
            "markdown": workflow_markdown("not-current", "Wrong", "Wrong body.")
        }),
    )
    .await;
    assert!(
        is_error && body.contains("does not match current workflow id"),
        "unexpected id mismatch: {body}"
    );

    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": {
                "mode": "replace",
                "existing_path": _global.path().join("collision.md").display().to_string(),
                "existing_id": "collision"
            },
            "filename": "collision.md",
            "markdown": workflow_markdown("renamed", "Renamed", "Renamed body.")
        }),
    )
    .await;
    assert!(
        is_error && body.contains("cannot change workflow id"),
        "unexpected rename rejection: {body}"
    );

    let replaced: WorkflowSaveResponse = call_agent_control_json(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": {
                "mode": "replace",
                "existing_path": _global.path().join("collision.md").display().to_string(),
                "existing_id": "collision"
            },
            "filename": "collision.md",
            "markdown": workflow_markdown("collision", "Global Collision Updated", "Updated body.")
        }),
    )
    .await;
    assert!(!replaced.created);
    assert_eq!(replaced.summary.name, "Global Collision Updated");
}

#[tokio::test]
async fn workflow_watcher_auto_updates_direct_markdown_changes() {
    let _global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        &project_root.path().display().to_string(),
    )
    .await;
    let dir = project_root.path().join(".tyde/workflows");
    std::fs::create_dir_all(&dir).expect("create workflow dir");
    let path = dir.join("watched.md");
    std::fs::write(
        &path,
        workflow_markdown("watched", "Watched One", "First body."),
    )
    .expect("write watched workflow");

    wait_for_workflow_notify(&mut fixture.client, "watcher create", |payload| {
        payload.summaries.iter().any(|summary| {
            summary.id == WorkflowId("watched".to_owned()) && summary.name == "Watched One"
        })
    })
    .await;

    std::fs::write(
        &path,
        workflow_markdown("watched", "Watched Two", "Second body."),
    )
    .expect("modify watched workflow");
    wait_for_workflow_notify(&mut fixture.client, "watcher modify", |payload| {
        payload.summaries.iter().any(|summary| {
            summary.id == WorkflowId("watched".to_owned()) && summary.name == "Watched Two"
        })
    })
    .await;

    std::fs::remove_file(&path).expect("remove watched workflow");
    let removed = wait_for_workflow_notify(&mut fixture.client, "watcher remove", |payload| {
        !payload
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("watched".to_owned()))
    })
    .await;
    assert!(removed.summaries.iter().all(|summary| {
        !matches!(
            &summary.source.scope,
            protocol::WorkflowSourceScope::Project { project_id, .. } if project_id == &project.id
        ) || summary.id != WorkflowId("watched".to_owned())
    }));
}

#[tokio::test]
async fn workflow_project_shadowing_is_scoped_per_project() {
    let global = GlobalWorkflowsEnv::new().await;
    std::fs::write(
        global.path().join("build.md"),
        workflow_markdown("build", "Global Build", "Global body."),
    )
    .expect("write global workflow");
    let root_a = tempfile::tempdir().expect("create project A root");
    let root_b = tempfile::tempdir().expect("create project B root");
    let mut fixture = Fixture::new().await;
    let project_a = create_project(&mut fixture.client, &root_a.path().display().to_string()).await;
    let project_b = create_project(&mut fixture.client, &root_b.path().display().to_string()).await;
    let dir_a = root_a.path().join(".tyde/workflows");
    std::fs::create_dir_all(&dir_a).expect("create project A workflow dir");
    std::fs::write(
        dir_a.join("build.md"),
        workflow_markdown("build", "Project A Build", "Project A body."),
    )
    .expect("write project A workflow");

    let notify = wait_for_workflow_notify(&mut fixture.client, "shadowing notify", |payload| {
        payload
            .summaries
            .iter()
            .any(|summary| summary.name == "Global Build")
            && payload
                .summaries
                .iter()
                .any(|summary| summary.name == "Project A Build")
            && payload
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("shadows"))
    })
    .await;
    assert_eq!(
        notify
            .summaries
            .iter()
            .filter(|summary| summary.id == WorkflowId("build".to_owned()))
            .count(),
        2
    );

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("build".to_owned()),
            project_id: Some(project_a.id.clone()),
            inputs: HashMap::new(),
        })
        .await
        .expect("trigger project A workflow");
    wait_for_workflow_coordinator_name(&mut fixture.client, "Workflow: Project A Build").await;

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("build".to_owned()),
            project_id: Some(project_b.id.clone()),
            inputs: HashMap::new(),
        })
        .await
        .expect("trigger project B workflow");
    wait_for_workflow_coordinator_name(&mut fixture.client, "Workflow: Global Build").await;

    fixture
        .client
        .trigger_workflow(TriggerWorkflowPayload {
            workflow_id: WorkflowId("build".to_owned()),
            project_id: None,
            inputs: HashMap::new(),
        })
        .await
        .expect("trigger global workflow");
    wait_for_workflow_coordinator_name(&mut fixture.client, "Workflow: Global Build").await;
}

async fn wait_for_workflow_coordinator_name(client: &mut client::Connection, expected_name: &str) {
    for _ in 0..20 {
        let env = next_event(client, expected_name).await;
        if env.kind != FrameKind::NewAgent {
            continue;
        }
        let payload: NewAgentPayload = env.parse_payload().expect("NewAgent");
        if payload.origin == AgentOrigin::Workflow && payload.name == expected_name {
            return;
        }
    }
    panic!("missing workflow coordinator named {expected_name}");
}

#[tokio::test]
async fn workflow_strict_validator_and_legacy_diagnostics_are_visible() {
    let global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let author = spawn_test_agent(
        &mut fixture.client,
        project_root.path(),
        None,
        BackendAccessMode::Unrestricted,
        "validator-author",
    )
    .await;
    let agent_control_url = fixture.agent_control_http_url().await;

    let invalid_cases = [
        (
            "bad-yaml.md",
            "---\nid: bad-yaml\nname: Bad\ncoordinator: [\n---\nBody\n".to_owned(),
            "front matter",
        ),
        (
            "bad-slug.md",
            workflow_markdown("BadSlug", "Bad Slug", "Body."),
            "must match",
        ),
        (
            "empty-body.md",
            "---\nid: empty-body\nname: Empty Body\ncoordinator:\n  backend: codex\n---\n   \n".to_owned(),
            "body must not be empty",
        ),
        (
            "bad-trigger.md",
            "---\nid: bad-trigger\nname: Bad Trigger\ncoordinator:\n  backend: codex\ntriggers: [made_up]\n---\nBody\n".to_owned(),
            "unknown trigger",
        ),
        (
            "bad-input-kind.md",
            "---\nid: bad-input-kind\nname: Bad Input Kind\ncoordinator:\n  backend: codex\ninputs:\n  - id: target\n    control: mystery\n---\nBody\n".to_owned(),
            "unknown workflow input control kind",
        ),
        (
            "duplicate-input.md",
            "---\nid: duplicate-input\nname: Duplicate Input\ncoordinator:\n  backend: codex\ninputs:\n  - id: target\n  - id: target\n---\nBody\n".to_owned(),
            "duplicate workflow input id",
        ),
        (
            "default-mismatch.md",
            "---\nid: default-mismatch\nname: Default Mismatch\ncoordinator:\n  backend: codex\ninputs:\n  - id: enabled\n    control: boolean\n    default: nope\n---\nBody\n".to_owned(),
            "must be a boolean",
        ),
    ];

    for (filename, markdown, expected) in invalid_cases {
        let (is_error, body) = call_mcp_tool(
            &agent_control_url,
            &author.agent_id,
            "tyde_workflow_save",
            json!({
                "target": { "kind": "global" },
                "mode": { "mode": "create" },
                "filename": filename,
                "markdown": markdown
            }),
        )
        .await;
        assert!(
            is_error,
            "invalid workflow {filename} unexpectedly saved: {body}"
        );
        assert!(
            body.contains(expected),
            "invalid workflow {filename} error {body:?} did not contain {expected:?}"
        );
    }

    std::fs::write(
        global.path().join("legacy.md"),
        workflow_markdown("Legacy", "Legacy", "Legacy body."),
    )
    .expect("write legacy workflow");
    std::fs::write(
        global.path().join("unknown-control.md"),
        "---\nid: unknown-control\nname: Unknown Control\ncoordinator:\n  backend: codex\ninputs:\n  - id: target\n    control: mystery\n---\nBody\n",
    )
    .expect("write unknown-control workflow");
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    let notify = wait_for_workflow_notify(&mut fixture.client, "legacy diagnostic", |payload| {
        let has_legacy = payload.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .source
                .as_ref()
                .is_some_and(|source| source.path.ends_with("legacy.md"))
                && diagnostic.message.contains("must match")
                && diagnostic.severity == protocol::WorkflowDiagnosticSeverity::Warning
        });
        let has_unknown_control = payload.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .source
                .as_ref()
                .is_some_and(|source| source.path.ends_with("unknown-control.md"))
                && diagnostic
                    .message
                    .contains("unknown workflow input control kind")
                && diagnostic.severity == protocol::WorkflowDiagnosticSeverity::Warning
        });
        has_legacy && has_unknown_control
    })
    .await;
    assert!(
        !notify
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("Legacy".to_owned())),
        "legacy invalid workflow should be skipped"
    );
    assert!(
        notify.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .source
                .as_ref()
                .is_some_and(|source| source.path.ends_with("unknown-control.md"))
                && diagnostic
                    .message
                    .contains("unknown workflow input control kind")
                && diagnostic.severity == protocol::WorkflowDiagnosticSeverity::Warning
        }),
        "unknown control kind should produce a warning diagnostic"
    );
    assert!(
        !notify
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("unknown-control".to_owned())),
        "unknown-control workflow should be skipped"
    );
}

#[tokio::test]
async fn workflow_bootstrap_includes_catalog_locations() {
    let global = GlobalWorkflowsEnv::new().await;
    std::fs::create_dir_all(global.path()).expect("create global dir");
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let project = create_project(
        &mut fixture.client,
        &project_root.path().display().to_string(),
    )
    .await;
    std::fs::create_dir_all(project_root.path().join(".tyde/workflows"))
        .expect("create project workflow dir");
    fixture
        .client
        .workflow_refresh(protocol::WorkflowRefreshPayload::default())
        .await
        .expect("workflow_refresh failed");
    wait_for_workflow_notify(&mut fixture.client, "location refresh", |payload| {
        payload.locations.iter().any(|location| {
            matches!(
                &location.scope,
                protocol::WorkflowSourceScope::Project { project_id, root }
                    if project_id == &project.id
                        && root.0 == project_root.path().display().to_string()
            ) && location.exists
        })
    })
    .await;

    let (_client, bootstrap): (client::Connection, HostBootstrapPayload) =
        fixture.connect_with_bootstrap().await;
    assert!(bootstrap.workflow_locations.iter().any(|location| {
        matches!(location.scope, protocol::WorkflowSourceScope::Global)
            && location.directory == global.path().display().to_string()
            && location.exists
    }));
    assert!(bootstrap.workflow_locations.iter().any(|location| {
        matches!(
            &location.scope,
            protocol::WorkflowSourceScope::Project { project_id, root }
                if project_id == &project.id
                    && root.0 == project_root.path().display().to_string()
        ) && location.exists
    }));
}

#[tokio::test]
async fn workflow_project_root_updates_refresh_watcher_targets() {
    let _global = GlobalWorkflowsEnv::new().await;
    let root_one = tempfile::tempdir().expect("create root one");
    let root_two = tempfile::tempdir().expect("create root two");
    let root_two_workflows = root_two.path().join(".tyde/workflows");
    std::fs::create_dir_all(&root_two_workflows).expect("create root two workflow dir");
    std::fs::write(
        root_two_workflows.join("added-root.md"),
        workflow_markdown("added-root", "Added Root", "Added root body."),
    )
    .expect("write added-root workflow");

    let mut fixture = Fixture::new().await;
    let project = create_project(&mut fixture.client, &root_one.path().display().to_string()).await;
    fixture
        .client
        .project_add_root(ProjectAddRootPayload {
            id: project.id.clone(),
            root: ProjectRootPath(root_two.path().display().to_string()),
        })
        .await
        .expect("project_add_root failed");

    wait_for_workflow_notify(&mut fixture.client, "added root workflow", |payload| {
        payload
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("added-root".to_owned()))
    })
    .await;

    fixture
        .client
        .project_delete_root(ProjectDeleteRootPayload {
            id: project.id,
            root: ProjectRootPath(root_two.path().display().to_string()),
        })
        .await
        .expect("project_delete_root failed");

    wait_for_workflow_notify(&mut fixture.client, "deleted root workflow", |payload| {
        !payload
            .summaries
            .iter()
            .any(|summary| summary.id == WorkflowId("added-root".to_owned()))
    })
    .await;
}

#[tokio::test]
async fn workflow_save_rejects_stale_disk_duplicate_id_before_reload() {
    let global = GlobalWorkflowsEnv::new().await;
    let project_root = tempfile::tempdir().expect("create project root");
    let mut fixture = Fixture::new().await;
    let author = spawn_test_agent(
        &mut fixture.client,
        project_root.path(),
        None,
        BackendAccessMode::Unrestricted,
        "stale-duplicate-author",
    )
    .await;
    std::fs::write(
        global.path().join("stale-dupe.md"),
        workflow_markdown("stale-dupe", "Stale Dupe", "Already on disk."),
    )
    .expect("write stale duplicate workflow");

    let agent_control_url = fixture.agent_control_http_url().await;
    let (is_error, body) = call_mcp_tool(
        &agent_control_url,
        &author.agent_id,
        "tyde_workflow_save",
        json!({
            "target": { "kind": "global" },
            "mode": { "mode": "create" },
            "filename": "stale-dupe-copy.md",
            "markdown": workflow_markdown("stale-dupe", "Stale Dupe Copy", "Should not save.")
        }),
    )
    .await;
    assert!(is_error, "stale duplicate save should fail: {body}");
    assert!(
        body.contains("already exists in the same scope"),
        "unexpected stale duplicate error: {body}"
    );
    assert!(
        !global.path().join("stale-dupe-copy.md").exists(),
        "duplicate save wrote a second same-scope id"
    );
}
