mod fixture;

use std::collections::HashMap;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    AgentId, AgentOrigin, BackendKind, CancelWorkflowPayload, Envelope, FrameKind, NewAgentPayload,
    Project, ProjectCreatePayload, ProjectNotifyPayload, ProjectRootPath, TriggerWorkflowPayload,
    WorkflowId, WorkflowNotifyPayload, WorkflowRunId, WorkflowRunNotifyPayload,
    WorkflowRunSnapshot, WorkflowRunSnapshotStatus, WorkflowStepRunSnapshotStatus,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Value, json};

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
        format!(
            "---\nid: build\nname: Build Project\ndescription: Compile and test\ncoordinator:\n  backend: codex\n  access_mode: {access_mode}\ndeclared_backends: [codex]\ntriggers: [global]\n---\nRun the build.\n"
        ),
    )
    .expect("write workflow");
    std::fs::write(dir.join("bad.md"), "---\nid: bad\n").expect("write bad workflow");
}

async fn wait_for_workflow_catalog(client: &mut client::Connection) {
    loop {
        let env = next_event(client, "workflow catalog").await;
        if env.kind == FrameKind::WorkflowNotify {
            break;
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
    let separator = if base_url.contains('?') { '&' } else { '?' };
    let url = format!("{base_url}{separator}agent_id={}", caller_agent_id.0);
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

#[tokio::test]
async fn workflow_refresh_reports_catalog_and_diagnostics() {
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
async fn workflow_progress_finish_and_reconnect_replay_run() {
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
