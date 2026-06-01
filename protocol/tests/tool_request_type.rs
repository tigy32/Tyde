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

#[test]
fn exit_plan_mode_tool_request_round_trips() {
    let value = json!({
        "kind": "ExitPlanMode",
        "plan": "# Plan\n\nDo the thing.",
        "plan_path": "/repo/.claude/plans/plan.md"
    });

    let parsed: ToolRequestType = serde_json::from_value(value).expect("typed request");
    assert_eq!(
        parsed,
        ToolRequestType::ExitPlanMode {
            plan: Some("# Plan\n\nDo the thing.".to_string()),
            plan_path: Some("/repo/.claude/plans/plan.md".to_string()),
        }
    );

    let serialized = serde_json::to_value(parsed).expect("serialize typed request");
    assert_eq!(serialized["kind"], json!("ExitPlanMode"));
    assert_eq!(serialized["plan"], json!("# Plan\n\nDo the thing."));
    assert_eq!(
        serialized["plan_path"],
        json!("/repo/.claude/plans/plan.md")
    );
}
