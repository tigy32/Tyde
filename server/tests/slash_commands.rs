mod fixture;

use fixture::{Fixture, TestAgent, next_logical_frame_matching_on};
use protocol::{ChatEvent, FrameKind, SlashCommand, SlashCommandCatalog};
use server::backend::mock::{MockRequest, MockScript, MockTurn};

fn catalog() -> SlashCommandCatalog {
    SlashCommandCatalog {
        commands: vec![
            SlashCommand {
                name: "compact".to_owned(),
                description: Some("Summarize the conversation".to_owned()),
                input_hint: Some("focus".to_owned()),
            },
            SlashCommand {
                name: "review".to_owned(),
                description: None,
                input_hint: None,
            },
        ],
    }
}

async fn send_message(fixture: &mut Fixture, agent: &TestAgent, message: &str) {
    let seq = fixture
        .client
        .outgoing_seq
        .get_mut(&agent.stream)
        .expect("agent sequence");
    let envelope = protocol::Envelope {
        stream: agent.stream.clone(),
        seq: *seq,
        kind: FrameKind::SendMessage,
        payload: serde_json::json!({ "message": message }),
    };
    *seq += 1;
    protocol::write_envelope(&mut fixture.client.writer, &envelope)
        .await
        .expect("send message");
}

fn slash_commands_event(env: &protocol::Envelope) -> Option<SlashCommandCatalog> {
    if env.kind != FrameKind::ChatEvent {
        return None;
    }
    match env.parse_payload::<ChatEvent>() {
        Ok(ChatEvent::SlashCommandsChanged(catalog)) => Some(catalog),
        _ => None,
    }
}

/// The backend's slash-command set is session state rather than transcript:
/// a live subscriber sees it as soon as the backend publishes it, a subscriber
/// that attaches later receives the latest set in its bootstrap, and a message
/// that invokes one of the commands reaches the backend exactly as typed.
#[tokio::test]
async fn slash_command_catalog_reaches_live_and_late_subscribers() {
    let mut fixture = Fixture::new().await;
    let agent = fixture
        .spawn_scripted(
            "slash-commands",
            MockScript::one(MockTurn::text("ready"))
                .then(MockTurn::text("compacted"))
                .with_slash_commands(catalog().commands),
        )
        .await;

    let published = fixture
        .next_chat_event_matching(&agent, "live slash command set", |event| {
            matches!(event, ChatEvent::SlashCommandsChanged(_))
        })
        .await;
    match published {
        ChatEvent::SlashCommandsChanged(published) => assert_eq!(published, catalog()),
        other => panic!("expected the slash command set, got {other:?}"),
    }
    let launch = fixture.finish_turn(&agent).await;
    launch.assert_stream_end_contains("ready");

    let (mut late, _bootstrap) = fixture.connect_with_bootstrap().await;
    let replayed = next_logical_frame_matching_on(&mut late, "late subscriber bootstrap", |env| {
        slash_commands_event(env).is_some()
    })
    .await;
    assert_eq!(
        slash_commands_event(&replayed),
        Some(catalog()),
        "a subscriber attaching after the publication must receive the same set"
    );

    send_message(&mut fixture, &agent, "/compact the auth work").await;
    let turn = fixture.finish_turn(&agent).await;
    turn.assert_stream_end_contains("compacted");
    let delivered = fixture
        .mock(&agent)
        .await
        .requests()
        .await
        .into_iter()
        .filter_map(|request| match request {
            MockRequest::Input(payload) => Some(payload.message),
            _ => None,
        })
        .next_back();
    assert_eq!(
        delivered.as_deref(),
        Some("/compact the auth work"),
        "the invoking message must reach the backend exactly as typed"
    );
}
