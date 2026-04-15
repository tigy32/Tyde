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
