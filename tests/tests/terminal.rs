mod fixture;

use std::fs;
use std::time::Duration;

use fixture::Fixture;
use protocol::{
    Envelope, FrameKind, NewTerminalPayload, Project, ProjectCreatePayload, ProjectNotifyPayload,
    ProjectRootPath, TerminalCreatePayload, TerminalErrorCode, TerminalErrorPayload,
    TerminalExitPayload, TerminalId, TerminalLaunchTarget, TerminalOutputPayload,
    TerminalResizePayload, TerminalSendPayload, TerminalStartPayload,
};
use tokio::time::timeout;
use uuid::Uuid;

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

async fn expect_next_event(client: &mut client::Connection, context: &str) -> Envelope {
    match timeout(EVENT_TIMEOUT, client.next_event()).await {
        Ok(Ok(Some(env))) => env,
        Ok(Ok(None)) => panic!("connection closed before {context}"),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
        Err(_) => panic!("timed out waiting for {context}"),
    }
}

async fn expect_no_event(client: &mut client::Connection, duration: Duration, context: &str) {
    match timeout(duration, client.next_event()).await {
        Err(_) => {}
        Ok(Ok(None)) => {}
        Ok(Ok(Some(env))) => panic!(
            "unexpected event before {context}: kind={} stream={}",
            env.kind, env.stream
        ),
        Ok(Err(err)) => panic!("next_event failed before {context}: {err:?}"),
    }
}

async fn create_project(
    client: &mut client::Connection,
    name: &str,
    roots: Vec<String>,
) -> Project {
    client
        .project_create(ProjectCreatePayload {
            name: name.to_owned(),
            roots,
        })
        .await
        .expect("project_create failed");

    let env = expect_next_event(client, "project create").await;
    assert_eq!(env.kind, FrameKind::ProjectNotify);
    match env
        .parse_payload()
        .expect("failed to parse ProjectNotifyPayload")
    {
        ProjectNotifyPayload::Upsert { project } => project,
        other => panic!("expected upsert project notification, got {other:?}"),
    }
}

async fn create_terminal(
    client: &mut client::Connection,
    payload: TerminalCreatePayload,
) -> (NewTerminalPayload, TerminalStartPayload) {
    client
        .terminal_create(payload)
        .await
        .expect("terminal_create failed");

    let new_terminal_env = expect_next_event(client, "new_terminal").await;
    assert_eq!(new_terminal_env.kind, FrameKind::NewTerminal);
    let new_terminal: NewTerminalPayload = new_terminal_env
        .parse_payload()
        .expect("failed to parse NewTerminalPayload");

    let start_env = expect_next_event(client, "terminal_start").await;
    assert_eq!(start_env.kind, FrameKind::TerminalStart);
    let start: TerminalStartPayload = start_env
        .parse_payload()
        .expect("failed to parse TerminalStartPayload");

    (new_terminal, start)
}

async fn wait_for_output_containing(
    client: &mut client::Connection,
    terminal_id: &TerminalId,
    needle: &str,
    context: &str,
) -> String {
    let mut combined = String::new();
    loop {
        let env = expect_next_event(client, context).await;
        assert_eq!(
            env.stream.0,
            format!("/terminal/{}", terminal_id),
            "unexpected stream while waiting for terminal output"
        );
        match env.kind {
            FrameKind::TerminalOutput => {
                let payload: TerminalOutputPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalOutputPayload");
                combined.push_str(&payload.data);
                if combined.contains(needle) {
                    return combined;
                }
            }
            FrameKind::TerminalExit => {
                let payload: TerminalExitPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalExitPayload");
                panic!(
                    "terminal exited before output contained '{needle}' during {context}: {payload:?}"
                );
            }
            FrameKind::TerminalError => {
                let payload: TerminalErrorPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalErrorPayload");
                panic!("terminal error before {context}: {payload:?}");
            }
            other => panic!("unexpected event kind {other} while waiting for {context}"),
        }
    }
}

async fn wait_for_terminal_exit(
    client: &mut client::Connection,
    terminal_id: &TerminalId,
    context: &str,
) -> TerminalExitPayload {
    loop {
        let env = expect_next_event(client, context).await;
        assert_eq!(
            env.stream.0,
            format!("/terminal/{}", terminal_id),
            "unexpected stream while waiting for terminal exit"
        );
        match env.kind {
            FrameKind::TerminalOutput => {
                let _: TerminalOutputPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalOutputPayload");
            }
            FrameKind::TerminalExit => {
                return env
                    .parse_payload()
                    .expect("failed to parse TerminalExitPayload");
            }
            FrameKind::TerminalError => {
                let payload: TerminalErrorPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalErrorPayload");
                panic!("unexpected terminal error before {context}: {payload:?}");
            }
            other => panic!("unexpected event kind {other} while waiting for {context}"),
        }
    }
}

async fn wait_for_terminal_error(
    client: &mut client::Connection,
    terminal_id: &TerminalId,
    context: &str,
) -> TerminalErrorPayload {
    loop {
        let env = expect_next_event(client, context).await;
        assert_eq!(
            env.stream.0,
            format!("/terminal/{}", terminal_id),
            "unexpected stream while waiting for terminal error"
        );
        match env.kind {
            FrameKind::TerminalOutput => {
                let _: TerminalOutputPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalOutputPayload");
            }
            FrameKind::TerminalError => {
                return env
                    .parse_payload()
                    .expect("failed to parse TerminalErrorPayload");
            }
            FrameKind::TerminalExit => {
                let payload: TerminalExitPayload = env
                    .parse_payload()
                    .expect("failed to parse TerminalExitPayload");
                panic!("unexpected terminal exit before {context}: {payload:?}");
            }
            other => panic!("unexpected event kind {other} while waiting for {context}"),
        }
    }
}

fn echo_command(token: &str) -> String {
    format!("echo {token}\n")
}

fn pwd_command() -> &'static str {
    "pwd\n"
}

fn resize_probe_command() -> &'static str {
    "stty size\n"
}

#[tokio::test]
async fn terminal_create_path_emits_start_and_streams_output() {
    let mut fixture = Fixture::new().await;
    let cwd = tempfile::tempdir().expect("create terminal cwd");

    let (new_terminal, start) = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Path {
                cwd: cwd.path().display().to_string(),
            },
            cols: 120,
            rows: 30,
        },
    )
    .await;

    assert_eq!(
        new_terminal.stream.0,
        format!("/terminal/{}", new_terminal.terminal_id)
    );
    assert_eq!(start.project_id, None);
    assert_eq!(start.root, None);
    assert_eq!(start.cwd, cwd.path().display().to_string());
    assert_eq!(start.cols, 120);
    assert_eq!(start.rows, 30);
    assert!(!start.shell.trim().is_empty(), "shell must not be empty");

    let token = format!("tyde-terminal-{}", Uuid::new_v4());
    fixture
        .client
        .terminal_send(
            &new_terminal.terminal_id,
            TerminalSendPayload {
                data: echo_command(&token),
            },
        )
        .await
        .expect("terminal_send failed");

    let output = wait_for_output_containing(
        &mut fixture.client,
        &new_terminal.terminal_id,
        &token,
        "terminal echo output",
    )
    .await;
    assert!(output.contains(&token));
}

#[tokio::test]
async fn project_terminal_sets_project_metadata_and_working_directory() {
    let mut fixture = Fixture::new().await;
    let root = tempfile::tempdir().expect("create project root");
    let nested = root.path().join("nested");
    fs::create_dir_all(&nested).expect("create nested cwd");

    let project = create_project(
        &mut fixture.client,
        "Terminal Project",
        vec![root.path().display().to_string()],
    )
    .await;

    let (new_terminal, start) = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Project {
                project_id: project.id.clone(),
                root: ProjectRootPath(root.path().display().to_string()),
                relative_cwd: Some("nested".to_owned()),
            },
            cols: 100,
            rows: 25,
        },
    )
    .await;

    assert_eq!(start.project_id, Some(project.id));
    assert_eq!(
        start.root,
        Some(ProjectRootPath(root.path().display().to_string()))
    );
    assert_eq!(start.cwd, nested.display().to_string());

    fixture
        .client
        .terminal_send(
            &new_terminal.terminal_id,
            TerminalSendPayload {
                data: pwd_command().to_owned(),
            },
        )
        .await
        .expect("terminal_send pwd failed");

    let output = wait_for_output_containing(
        &mut fixture.client,
        &new_terminal.terminal_id,
        &nested.display().to_string(),
        "pwd output",
    )
    .await;
    assert!(output.contains(&nested.display().to_string()));
}

#[tokio::test]
async fn terminal_resize_is_observable_from_shell() {
    if cfg!(target_os = "windows") {
        eprintln!("SKIPPED: resize probe uses stty");
        return;
    }

    let mut fixture = Fixture::new().await;
    let cwd = tempfile::tempdir().expect("create terminal cwd");

    let (new_terminal, _start) = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Path {
                cwd: cwd.path().display().to_string(),
            },
            cols: 80,
            rows: 24,
        },
    )
    .await;

    fixture
        .client
        .terminal_resize(
            &new_terminal.terminal_id,
            TerminalResizePayload {
                cols: 132,
                rows: 41,
            },
        )
        .await
        .expect("terminal_resize failed");

    fixture
        .client
        .terminal_send(
            &new_terminal.terminal_id,
            TerminalSendPayload {
                data: resize_probe_command().to_owned(),
            },
        )
        .await
        .expect("terminal_send stty failed");

    let output = wait_for_output_containing(
        &mut fixture.client,
        &new_terminal.terminal_id,
        "41 132",
        "stty size output",
    )
    .await;
    assert!(output.contains("41 132"));
}

#[tokio::test]
async fn terminal_close_emits_exit() {
    let mut fixture = Fixture::new().await;
    let cwd = tempfile::tempdir().expect("create terminal cwd");

    let (new_terminal, _start) = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Path {
                cwd: cwd.path().display().to_string(),
            },
            cols: 90,
            rows: 28,
        },
    )
    .await;

    fixture
        .client
        .terminal_close(&new_terminal.terminal_id)
        .await
        .expect("terminal_close failed");

    let exit = wait_for_terminal_exit(
        &mut fixture.client,
        &new_terminal.terminal_id,
        "terminal close exit",
    )
    .await;
    assert!(
        exit.exit_code.is_some() || exit.signal.is_some(),
        "exit payload should include an exit code or signal"
    );
}

#[tokio::test]
async fn send_after_terminal_exit_emits_not_running_error() {
    let mut fixture = Fixture::new().await;
    let cwd = tempfile::tempdir().expect("create terminal cwd");

    let (new_terminal, _start) = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Path {
                cwd: cwd.path().display().to_string(),
            },
            cols: 100,
            rows: 30,
        },
    )
    .await;

    fixture
        .client
        .terminal_send(
            &new_terminal.terminal_id,
            TerminalSendPayload {
                data: "exit 7\n".to_owned(),
            },
        )
        .await
        .expect("terminal_send exit failed");

    let exit =
        wait_for_terminal_exit(&mut fixture.client, &new_terminal.terminal_id, "shell exit").await;
    assert_eq!(exit.exit_code, Some(7));

    fixture
        .client
        .terminal_send(
            &new_terminal.terminal_id,
            TerminalSendPayload {
                data: echo_command("after-exit"),
            },
        )
        .await
        .expect("terminal_send after exit failed");

    let error = wait_for_terminal_error(
        &mut fixture.client,
        &new_terminal.terminal_id,
        "not running error",
    )
    .await;
    assert_eq!(error.code, TerminalErrorCode::NotRunning);
    assert!(!error.fatal);
}

#[tokio::test]
async fn late_joining_client_does_not_receive_existing_terminals() {
    let mut fixture = Fixture::new().await;
    let cwd = tempfile::tempdir().expect("create terminal cwd");

    let _ = create_terminal(
        &mut fixture.client,
        TerminalCreatePayload {
            target: TerminalLaunchTarget::Path {
                cwd: cwd.path().display().to_string(),
            },
            cols: 80,
            rows: 24,
        },
    )
    .await;

    let mut late_client = fixture.connect().await;
    expect_no_event(
        &mut late_client,
        Duration::from_millis(300),
        "terminal replay to late-joining client",
    )
    .await;
}
