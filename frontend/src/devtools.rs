use crate::state::{AppState, ChatRowContent, ToolRequestEntry};
use devtools_protocol::{
    UiDebugRequest, UiDebugResponse, UiFileOwners, UiMemoryOwners, UiPayloadBytes,
    UiTranscriptOwners,
};
use js_sys::Promise;
use leptos::prelude::*;
use protocol::{ToolExecutionOutcome, ToolExecutionResult};
use serde_json::Value;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::bridge::{
    self, SubmitUiDebugResponseRequest, listen_ui_debug_request, mark_ui_debug_ready,
    submit_ui_debug_response,
};

pub async fn install_listener(state: AppState) -> Result<bridge::UnlistenHandle, String> {
    let handle = listen_ui_debug_request(move |event| {
        let state = state.clone();
        spawn_local(async move {
            let response = handle_request(&state, event.request).await;
            let submit = SubmitUiDebugResponseRequest {
                request_id: event.request_id,
                response,
            };
            if let Err(err) = submit_ui_debug_response(submit).await {
                log::error!("failed to submit ui debug response: {err}");
            }
        });
    })
    .await?;

    mark_ui_debug_ready().await?;
    Ok(handle)
}

async fn handle_request(state: &AppState, request: UiDebugRequest) -> UiDebugResponse {
    match request {
        UiDebugRequest::Ping => UiDebugResponse::Pong,
        UiDebugRequest::MemoryOwners => UiDebugResponse::MemoryOwners {
            snapshot: Box::new(memory_owners(state)),
        },
        UiDebugRequest::Evaluate { expression, .. } => match evaluate_expression(&expression).await
        {
            Ok(value) => UiDebugResponse::EvaluateResult { value },
            Err(message) => UiDebugResponse::Error { message },
        },
        UiDebugRequest::CaptureScreenshot { .. } => UiDebugResponse::Error {
            message: "capture_screenshot is handled by the desktop shell".to_string(),
        },
    }
}

fn buffer_bytes(bytes: &mut UiPayloadBytes, value: &String) {
    bytes.len_bytes += value.len();
    bytes.capacity_bytes += value.capacity();
}

fn json_bytes(bytes: &mut UiPayloadBytes, value: &Value) {
    match value {
        Value::String(text) => buffer_bytes(bytes, text),
        Value::Array(values) => values.iter().for_each(|value| json_bytes(bytes, value)),
        Value::Object(values) => {
            for (key, value) in values {
                buffer_bytes(bytes, key);
                json_bytes(bytes, value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn image_bytes(bytes: &mut UiPayloadBytes, image: &protocol::ImageData) {
    buffer_bytes(bytes, &image.data);
}

fn result_bytes(bytes: &mut UiPayloadBytes, outcome: &ToolExecutionOutcome) {
    let result = match outcome {
        ToolExecutionOutcome::Succeeded { result } => Some(result),
        ToolExecutionOutcome::Failed {
            message,
            details,
            result,
            ..
        } => {
            buffer_bytes(bytes, message);
            if let Some(details) = details {
                buffer_bytes(bytes, details);
            }
            result.as_ref()
        }
        ToolExecutionOutcome::Cancelled { message } => {
            buffer_bytes(bytes, message);
            None
        }
    };
    let Some(result) = result else {
        return;
    };
    match result {
        ToolExecutionResult::RunCommand { stdout, stderr, .. } => {
            buffer_bytes(bytes, stdout);
            buffer_bytes(bytes, stderr);
        }
        ToolExecutionResult::NativeContent { text, images } => {
            buffer_bytes(bytes, text);
            for image in images {
                image_bytes(bytes, image);
            }
        }
        ToolExecutionResult::Other { result } => json_bytes(bytes, result),
        ToolExecutionResult::GetTypeDocs { documentation } => buffer_bytes(bytes, documentation),
        ToolExecutionResult::SearchTypes { types } => {
            for text in types {
                buffer_bytes(bytes, text);
            }
        }
        ToolExecutionResult::ViewImage {
            image,
            preview_error,
        } => {
            if let Some(image) = image {
                image_bytes(bytes, image);
            }
            if let Some(error) = preview_error {
                buffer_bytes(bytes, error);
            }
        }
        ToolExecutionResult::GenerateImage { revised_prompt, .. } => {
            if let Some(prompt) = revised_prompt {
                buffer_bytes(bytes, prompt);
            }
        }
        ToolExecutionResult::ExitPlanMode { feedback, plan, .. } => {
            if let Some(feedback) = feedback {
                buffer_bytes(bytes, feedback);
            }
            if let Some(plan) = plan {
                buffer_bytes(bytes, plan);
            }
        }
        ToolExecutionResult::AskUserQuestion { answers } => {
            for answer in answers {
                match &answer.answer {
                    protocol::UserQuestionAnswerValue::Selected { labels, note } => {
                        for label in labels {
                            buffer_bytes(bytes, label);
                        }
                        if let Some(note) = note {
                            buffer_bytes(bytes, note);
                        }
                    }
                    protocol::UserQuestionAnswerValue::FreeText { text, note } => {
                        buffer_bytes(bytes, text);
                        if let Some(note) = note {
                            buffer_bytes(bytes, note);
                        }
                    }
                }
            }
        }
        ToolExecutionResult::ModifyFile { .. }
        | ToolExecutionResult::ReadFiles { .. }
        | ToolExecutionResult::TydeSendAgentMessage
        | ToolExecutionResult::TydeAwaitAgents { .. }
        | ToolExecutionResult::WebSearch
        | ToolExecutionResult::Sleep => {}
    }
}

fn tool_bytes(owners: &mut UiTranscriptOwners, tool: &ToolRequestEntry) {
    owners.tool_requests += 1;
    if let Some(result) = &tool.result {
        owners.attached_results += 1;
        result_bytes(&mut owners.result_payloads, &result.outcome);
    }
}

fn file_bytes<'a>(
    owners: &mut UiFileOwners,
    files: impl Iterator<Item = &'a crate::state::OpenFile>,
    diffs: impl Iterator<Item = &'a crate::state::DiffViewState>,
) {
    for file in files {
        owners.files += 1;
        if let Some(contents) = &file.contents {
            buffer_bytes(&mut owners.contents, contents);
        }
    }
    for diff in diffs {
        owners.diffs += 1;
        owners.diff_files += diff.files.len();
        for file in &diff.files {
            for hunk in &file.hunks {
                owners.diff_lines += hunk.lines.len();
                for line in &hunk.lines {
                    buffer_bytes(&mut owners.diff_text, &line.text);
                }
            }
        }
    }
}

pub(crate) fn memory_owners(state: &AppState) -> UiMemoryOwners {
    let mut snapshot = UiMemoryOwners::default();
    state.chat_rows.with_untracked(|agents| {
        let owners = &mut snapshot.transcript;
        owners.agents = agents.len();
        for rows in agents.values() {
            owners.rows += rows.len();
            owners.row_slots_capacity_bytes +=
                rows.capacity() * std::mem::size_of::<crate::state::ChatRowHandle>();
            for row in rows {
                match &row.content {
                    ChatRowContent::Message(entry) => entry.with_untracked(|entry| {
                        owners.messages += 1;
                        buffer_bytes(&mut owners.message_text, &entry.message.content);
                        if let Some(reasoning) = &entry.message.reasoning {
                            buffer_bytes(&mut owners.reasoning_text, &reasoning.text);
                            if let Some(signature) = &reasoning.signature {
                                buffer_bytes(&mut owners.reasoning_text, signature);
                            }
                            if let Some(blob) = &reasoning.blob {
                                owners.reasoning_blobs.len_bytes += blob.len();
                                owners.reasoning_blobs.capacity_bytes += blob.capacity();
                            }
                        }
                        if let Some(images) = &entry.message.images {
                            for image in images {
                                image_bytes(&mut owners.message_images, image);
                            }
                        }
                        for tool in &entry.tool_requests {
                            tool_bytes(owners, tool);
                        }
                    }),
                    ChatRowContent::Streaming(_)
                    | ChatRowContent::ContextCompaction(_)
                    | ChatRowContent::Notice(_) => {}
                }
            }
        }
    });
    state.streaming_text.with_untracked(|agents| {
        let owners = &mut snapshot.streaming;
        owners.agents = agents.len();
        for stream in agents.values() {
            stream
                .text
                .with_untracked(|text| buffer_bytes(&mut owners.message_text, text));
            stream
                .reasoning
                .with_untracked(|text| buffer_bytes(&mut owners.reasoning_text, text));
            stream.tool_requests.with_untracked(|tools| {
                for tool in tools {
                    tool.entry.with_untracked(|entry| tool_bytes(owners, entry));
                }
            });
        }
    });
    state.completed_tool_results.with_untracked(|results| {
        snapshot.completed_results = results.len();
        for result in results.values() {
            result_bytes(&mut snapshot.completed_result_payloads, &result.outcome);
        }
    });
    snapshot.tool_row_index_entries = state
        .chat_tool_rows
        .with_untracked(|agents| agents.values().map(|rows| rows.len()).sum());
    snapshot.message_row_index_entries = state
        .chat_message_rows
        .with_untracked(|agents| agents.values().map(|rows| rows.len()).sum());
    snapshot.pending_questions = state
        .pending_user_questions
        .with_untracked(|agents| agents.values().map(|questions| questions.len()).sum());
    snapshot.progress_entries = state.tool_progress.with_untracked(|entries| entries.len());
    state.open_files.with_untracked(|files| {
        state.diff_contents.with_untracked(|diffs| {
            file_bytes(&mut snapshot.active_files, files.values(), diffs.values())
        })
    });
    state.project_view_memory.with_untracked(|projects| {
        snapshot.saved_projects = projects.len();
        for project in projects.values() {
            file_bytes(
                &mut snapshot.saved_files,
                project.open_files.values(),
                project.diff_contents.values(),
            );
        }
    });
    snapshot
}

async fn evaluate_expression(expression: &str) -> Result<Value, String> {
    let script = format!("(async () => {{ {expression} }})()");
    let value = js_sys::eval(&script).map_err(format_js_error)?;
    let result = JsFuture::from(Promise::resolve(&value))
        .await
        .map_err(format_js_error)?;
    js_value_to_json(result)
}

fn js_value_to_json(value: JsValue) -> Result<Value, String> {
    if value.is_null() || value.is_undefined() {
        return Ok(Value::Null);
    }

    serde_wasm_bindgen::from_value(value)
        .map_err(|err| format!("failed to serialize JS value: {err}"))
}

fn format_js_error(value: JsValue) -> String {
    if let Some(message) = value.as_string() {
        return message;
    }

    match serde_wasm_bindgen::from_value::<Value>(value) {
        Ok(json) => json.to_string(),
        Err(_) => "JavaScript evaluation failed".to_string(),
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::wasm_test_support::mount_to;
    use leptos::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    #[derive(serde::Serialize)]
    struct DebugEventEnvelope {
        payload: devtools_protocol::UiDebugRequestEvent,
    }

    // Capture the production listener's outbound JS arguments, not native IPC.
    // No Tauri serialization or native command deserialization is simulated.
    struct BrowserTransport(JsValue);

    impl BrowserTransport {
        fn new() -> Self {
            Self(js_sys::eval(r#"
                (() => {
                    const existed = Object.hasOwn(window, '__TAURI__');
                    const previous = window.__TAURI__;
                    let handler, ready = false, submit;
                    window.__TAURI__ = {
                        event: { listen(name, callback) {
                            if (name !== 'tyde://ui-debug-request') throw new Error('unexpected event');
                            handler = callback;
                            return Promise.resolve(() => { handler = null; });
                        } },
                        core: { async invoke(command, args) {
                            if (command === 'mark_ui_debug_ready') ready = true;
                            else if (command === 'submit_ui_debug_response') submit(args);
                            else throw new Error('unexpected command');
                        } }
                    };
                    return {
                        dispatch(event) {
                            if (!ready || !handler) throw new Error('debug listener not ready');
                            return new Promise(resolve => { submit = resolve; handler(event); });
                        },
                        restore() {
                            if (existed) window.__TAURI__ = previous;
                            else delete window.__TAURI__;
                        }
                    };
                })()
            "#).expect("install browser transport capture"))
        }

        async fn evaluate(&self, expression: &str, expected: &str) {
            let envelope = DebugEventEnvelope {
                payload: devtools_protocol::UiDebugRequestEvent {
                    request_id: "browser-evaluation".to_owned(),
                    request: UiDebugRequest::Evaluate {
                        expression: expression.to_owned(),
                        timeout_ms: None,
                    },
                },
            };
            let event = js_sys::JSON::parse(
                &serde_json::to_string(&envelope).expect("serialize typed debug event"),
            )
            .expect("browser event");
            let dispatch: js_sys::Function =
                js_sys::Reflect::get(&self.0, &JsValue::from_str("dispatch"))
                    .expect("dispatch")
                    .dyn_into()
                    .expect("dispatch function");
            let pending: Promise = dispatch
                .call1(&self.0, &event)
                .expect("dispatch debug event")
                .dyn_into()
                .expect("submission promise");
            let args = JsFuture::from(pending).await.expect("outbound submission");
            assert_eq!(
                js_sys::Reflect::get(&args, &JsValue::from_str("requestId"))
                    .expect("request identity")
                    .as_string()
                    .as_deref(),
                Some("browser-evaluation")
            );
            let response = js_sys::Reflect::get(&args, &JsValue::from_str("response"))
                .expect("outbound response");
            let expected: Value = serde_json::from_str(expected).expect("expected JSON value");
            if expected.get("value").is_some() {
                let value = js_sys::Reflect::get(&response, &JsValue::from_str("value"))
                    .expect("outbound evaluation value");
                // Deserialization accepts undefined as null; inspect the real
                // JS argument first so a dropped IPC value cannot pass.
                assert!(!value.is_undefined(), "evaluation value must be defined");
            }
            let actual: Value =
                serde_wasm_bindgen::from_value(response).expect("outbound browser argument values");
            assert_eq!(actual, expected);
        }
    }

    impl Drop for BrowserTransport {
        fn drop(&mut self) {
            let restore: js_sys::Function =
                js_sys::Reflect::get(&self.0, &JsValue::from_str("restore"))
                    .expect("restore")
                    .dyn_into()
                    .expect("restore function");
            restore.call0(&self.0).expect("restore browser transport");
        }
    }

    #[wasm_bindgen_test]
    async fn evaluated_dom_results_preserve_json_values_at_browser_bridge() {
        let document = web_sys::window()
            .expect("window")
            .document()
            .expect("document");
        let root: web_sys::HtmlElement = document
            .create_element("div")
            .expect("fixture root")
            .dyn_into()
            .expect("HTML root");
        document
            .body()
            .expect("body")
            .append_child(&root)
            .expect("mount root");
        let handle = mount_to(root.clone(), || {
            view! {
                <output data-debug-null-mount="1">"Mounted"</output>
            }
        });
        let mount = crate::wasm_test_support::Mounted::new(
            handle,
            crate::wasm_test_support::ScopedRoot::new(root, "debug-null-response"),
        );
        assert_eq!(mount.text_content().as_deref(), Some("Mounted"));
        let transport = BrowserTransport::new();
        let listener = install_listener(AppState::new())
            .await
            .expect("install production listener");

        transport.evaluate(
            "await Promise.resolve(); document.querySelector('[data-debug-null-mount]').textContent = 'Evaluated'",
            r#"{"kind":"evaluate_result","value":null}"#,
        ).await;
        assert_eq!(mount.text_content().as_deref(), Some("Evaluated"));

        for (expression, expected) in [
            ("return null", r#"{"kind":"evaluate_result","value":null}"#),
            (
                "return undefined",
                r#"{"kind":"evaluate_result","value":null}"#,
            ),
            (
                "return { nested: null, items: [null, 1] }",
                r#"{"kind":"evaluate_result","value":{"items":[null,1],"nested":null}}"#,
            ),
            (
                "return { ok: true, label: document.querySelector('[data-debug-null-mount]').textContent }",
                r#"{"kind":"evaluate_result","value":{"label":"Evaluated","ok":true}}"#,
            ),
            (
                r#"return JSON.parse('{"__proto__":{"tag":1},"nested":{"__proto__":null},"items":[{"__proto__":"literal"}],"__TAURI_TO_IPC_KEY__":"ordinary","constructor":{"prototype":null},"ok":2}')"#,
                r#"{"kind":"evaluate_result","value":{"__proto__":{"tag":1},"nested":{"__proto__":null},"items":[{"__proto__":"literal"}],"__TAURI_TO_IPC_KEY__":"ordinary","constructor":{"prototype":null},"ok":2}}"#,
            ),
            (
                "return [null, 0, false, '', { nested: null }]",
                r#"{"kind":"evaluate_result","value":[null,0,false,"",{"nested":null}]}"#,
            ),
            ("return 7", r#"{"kind":"evaluate_result","value":7}"#),
            ("return 0", r#"{"kind":"evaluate_result","value":0}"#),
            (
                "return false",
                r#"{"kind":"evaluate_result","value":false}"#,
            ),
            ("return ''", r#"{"kind":"evaluate_result","value":""}"#),
            (
                "throw 'debug-eval-failed'",
                r#"{"kind":"error","message":"debug-eval-failed"}"#,
            ),
        ] {
            transport.evaluate(expression, expected).await;
        }
        listener.remove();
    }
}
