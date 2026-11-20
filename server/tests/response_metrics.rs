mod fixture;

use fixture::Fixture;
use protocol::{ChatEvent, ChatMessage, MessageSender, MessageTokenUsage, ModelInfo, TokenUsage};
use server::backend::mock::{MockScript, MockTurn};

const MOCK_OUTPUT_TOKENS: i64 = 340;

#[tokio::test]
async fn responses_and_turns_are_recorded_per_backend_and_settings() {
    let mut fixture = Fixture::new().await;
    let unstreamed = ChatMessage {
        origin: None,
        message_id: None,
        timestamp: 0,
        sender: MessageSender::Assistant {
            agent: "mock".to_owned(),
        },
        content: "unstreamed reply".to_owned(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: Some(ModelInfo {
            model: "mock".to_owned(),
        }),
        token_usage: Some(MessageTokenUsage::request_known(TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            total_tokens: 18,
            cached_prompt_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
        })),
        context_breakdown: None,
        images: None,
    };
    let agent = fixture
        .spawn_scripted(
            "metrics",
            MockScript::one(MockTurn::text("streamed reply"))
                .then(MockTurn::text_with_late_usage("late usage reply"))
                .then(MockTurn::chat_events(vec![ChatEvent::MessageAdded(
                    unstreamed,
                )]))
                .then(MockTurn::held_text("interrupted reply")),
        )
        .await;
    fixture.finish_turn(&agent).await;
    for prompt in ["late", "unstreamed"] {
        fixture
            .client
            .send_message(&agent.stream, prompt.to_owned())
            .await
            .expect("send metrics turn");
        fixture.finish_turn(&agent).await;
    }
    fixture
        .client
        .send_message(&agent.stream, "hold".to_owned())
        .await
        .expect("send held turn");
    fixture
        .next_chat_event_matching(&agent, "held stream end", |event| {
            matches!(event, ChatEvent::StreamEnd(_))
        })
        .await;
    fixture
        .client
        .interrupt(&agent.stream)
        .await
        .expect("interrupt held turn");
    fixture
        .next_chat_event_matching(&agent, "held turn idle", |event| {
            matches!(event, ChatEvent::TypingStatusChanged(false))
        })
        .await;

    let path = fixture
        .host_for_test()
        .flush_response_metrics_for_test()
        .await;
    let db = rusqlite::Connection::open(&path).expect("open metrics database");

    type ResponseRow = (
        String,
        Option<String>,
        Option<String>,
        String,
        bool,
        Option<i64>,
        Option<i64>,
        bool,
        Option<i64>,
        Option<f64>,
        String,
    );
    let responses: Vec<ResponseRow> = db
        .prepare(
            "SELECT backend, model, settings_key, agent_id, streamed, ttft_from_input_ms,
                generation_ms, usage_known, output_tokens, output_tokens_per_second, outcome
             FROM response_samples ORDER BY id",
        )
        .expect("prepare responses")
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
            ))
        })
        .expect("query responses")
        .collect::<Result<_, _>>()
        .expect("read responses");
    assert_eq!(responses.len(), 4, "one row per model response");
    for row in &responses {
        assert_eq!(row.0, "claude", "backend label");
        assert_eq!(row.1.as_deref(), Some("mock"), "model label");
        assert!(row.2.is_some(), "settings key recorded");
        assert_eq!(row.3, agent.new_agent.agent_id.0, "agent id");
    }
    for (index, row) in responses[..2].iter().enumerate() {
        assert!(row.4, "response {index} streamed");
        assert!(row.5.is_some(), "response {index} time to first token");
        assert!(row.7, "response {index} usage attached");
        assert_eq!(row.8, Some(MOCK_OUTPUT_TOKENS), "response {index} output");
        let generation_ms = row.6.expect("generation time");
        match row.9 {
            Some(rate) => assert!(
                generation_ms > 0
                    && (rate - MOCK_OUTPUT_TOKENS as f64 * 1_000.0 / generation_ms as f64).abs()
                        < 1e-6,
                "response {index} tokens per second follows output over generation"
            ),
            None => assert_eq!(
                generation_ms, 0,
                "response {index} rate omitted only at 0ms"
            ),
        }
        assert_eq!(row.10, "completed");
    }
    let unstreamed = &responses[2];
    assert!(!unstreamed.4, "unstreamed response");
    assert_eq!(unstreamed.5, None, "no first token for unstreamed");
    assert_eq!(unstreamed.8, Some(7));
    assert_eq!(unstreamed.9, None, "no rate for unstreamed");
    let held = &responses[3];
    assert!(held.4, "held response streamed");
    assert!(!held.7, "held response never reported usage");
    assert_eq!(held.10, "completed");

    let turns: Vec<(i64, bool, i64, String)> = db
        .prepare(
            "SELECT responses, usage_complete, output_tokens, outcome
             FROM turn_samples ORDER BY id",
        )
        .expect("prepare turns")
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query turns")
        .collect::<Result<_, _>>()
        .expect("read turns");
    assert_eq!(
        turns,
        vec![
            (1, true, MOCK_OUTPUT_TOKENS, "completed".to_owned()),
            (1, true, MOCK_OUTPUT_TOKENS, "completed".to_owned()),
            (1, true, 7, "completed".to_owned()),
            (1, false, 0, "cancelled".to_owned()),
        ]
    );

    let rollup: (i64, i64, i64, i64) = db
        .query_row(
            "SELECT SUM(responses), SUM(streamed_responses), SUM(usage_known_responses),
                SUM(output_tokens_sum)
             FROM daily_rollups WHERE backend = 'claude' AND model = 'mock'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read rollup");
    assert_eq!(rollup, (4, 3, 3, 2 * MOCK_OUTPUT_TOKENS + 7));
}
