use protocol::{AskUserQuestion, AskUserQuestionOption, ToolRequestType};
use serde_json::json;

#[test]
fn ask_user_question_tool_request_round_trips() {
    let value = json!({
        "kind": "AskUserQuestion",
        "questions": [
            {
                "id": "language",
                "question": "Which language?",
                "header": "Language",
                "options": [
                    { "label": "Rust", "description": "Systems lang" },
                    { "label": "Python" }
                ],
                "multiSelect": true
            }
        ]
    });

    let parsed: ToolRequestType = serde_json::from_value(value).expect("typed request");
    assert_eq!(
        parsed,
        ToolRequestType::AskUserQuestion {
            questions: vec![AskUserQuestion {
                id: Some("language".to_string()),
                question: "Which language?".to_string(),
                header: Some("Language".to_string()),
                options: vec![
                    AskUserQuestionOption {
                        label: "Rust".to_string(),
                        description: Some("Systems lang".to_string()),
                    },
                    AskUserQuestionOption {
                        label: "Python".to_string(),
                        description: None,
                    },
                ],
                multi_select: true,
            }]
        }
    );

    let serialized = serde_json::to_value(parsed).expect("serialize typed request");
    assert_eq!(serialized["kind"], json!("AskUserQuestion"));
    assert_eq!(serialized["questions"][0]["multiSelect"], json!(true));
    assert!(
        serialized["questions"][0]["options"][1]
            .get("description")
            .is_none()
    );
}
