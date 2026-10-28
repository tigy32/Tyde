use devtools_protocol::{UiDebugRequest, UiDebugResponse};
use js_sys::Promise;
use serde_json::Value;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::bridge::{
    self, SubmitUiDebugResponseRequest, listen_ui_debug_request, mark_ui_debug_ready,
    submit_ui_debug_response,
};

pub async fn install_listener() -> Result<bridge::UnlistenHandle, String> {
    let handle = listen_ui_debug_request(move |event| {
        spawn_local(async move {
            let response = handle_request(event.request).await;
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

async fn handle_request(request: UiDebugRequest) -> UiDebugResponse {
    match request {
        UiDebugRequest::Ping => UiDebugResponse::Pong,
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
        let listener = install_listener()
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
