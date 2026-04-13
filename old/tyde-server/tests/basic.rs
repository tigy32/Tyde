use tyde_protocol::protocol::ChatEvent;
use tyde_server::backends::types::SessionCommand;
use tyde_server::mock_backend::MockBehavior;
use tyde_server::runtime_ops::execute_agent_command;

mod fixture;

#[tokio::test]
async fn create_agent_and_send_message() {
    let fixture = fixture::Fixture::new();

    let agent_id = fixture.create_agent().await;
    assert!(fixture.server.has_agent(&agent_id).await);

    execute_agent_command(
        &fixture.server,
        &agent_id,
        SessionCommand::SendMessage {
            message: "Hello from test".to_string(),
            images: None,
        },
    )
    .await
    .map_err(|f| f.error)
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let events = fixture.drain_chat_events(&agent_id);
    assert!(
        !events.is_empty(),
        "Expected chat events from mock backend, got none"
    );

    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.get("kind").and_then(|k| k.as_str()))
        .collect();
    assert!(
        kinds.contains(&"StreamStart"),
        "Expected StreamStart in events, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"StreamEnd"),
        "Expected StreamEnd in events, got: {kinds:?}"
    );
}

#[tokio::test]
async fn custom_events_behavior() {
    let fixture = fixture::Fixture::new();

    let (agent_id, controller) = fixture.create_agent_controlled().await;
    controller.set_behavior(MockBehavior::Events(vec![
        ChatEvent::TypingStatusChanged(true),
        ChatEvent::Error("custom".to_string()),
        ChatEvent::TypingStatusChanged(false),
    ]));

    execute_agent_command(
        &fixture.server,
        &agent_id,
        SessionCommand::SendMessage {
            message: "trigger custom".to_string(),
            images: None,
        },
    )
    .await
    .map_err(|f| f.error)
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let events = fixture.drain_chat_events(&agent_id);
    let has_error = events
        .iter()
        .any(|e| e.get("kind").and_then(|k| k.as_str()) == Some("Error"));
    assert!(has_error, "Expected Error in events: {events:?}");
}

#[tokio::test]
async fn close_agent_removes_it() {
    let fixture = fixture::Fixture::new();

    let agent_id = fixture.create_agent().await;
    assert!(fixture.server.has_agent(&agent_id).await);

    tyde_server::runtime_ops::close_agent(&fixture.server, &agent_id)
        .await
        .unwrap();

    assert!(!fixture.server.has_agent(&agent_id).await);
}

#[tokio::test]
async fn agent_lifecycle() {
    let fixture = fixture::Fixture::new();

    let agents = fixture.server.list_agents().await;
    assert!(agents.is_empty());

    let agent_id = fixture.create_agent().await;

    let agents = fixture.server.list_agents().await;
    assert_eq!(agents.len(), 1);

    let agent = fixture.server.get_agent(&agent_id).await.unwrap();
    assert_eq!(agent.name, "test-agent");
    assert!(agent.is_running);
}
