use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiDebugRequest {
    Ping,
    Evaluate {
        expression: String,
        timeout_ms: Option<u64>,
    },
    CaptureScreenshot {
        max_dimension: Option<u32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiDebugResponse {
    Pong,
    EvaluateResult {
        value: Value,
    },
    CaptureScreenshotResult {
        png_base64: String,
        width: u32,
        height: u32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDebugRequestEvent {
    pub request_id: String,
    pub request: UiDebugRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDebugResponseSubmission {
    pub request_id: String,
    pub response: UiDebugResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDebugHealth {
    pub status: &'static str,
    pub ready: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let request = UiDebugRequest::Evaluate {
            expression: "return document.title;".to_string(),
            timeout_ms: Some(5_000),
        };

        let json = serde_json::to_string(&request).expect("serialize request");
        let decoded: UiDebugRequest = serde_json::from_str(&json).expect("deserialize request");

        match decoded {
            UiDebugRequest::Evaluate {
                expression,
                timeout_ms,
            } => {
                assert_eq!(expression, "return document.title;");
                assert_eq!(timeout_ms, Some(5_000));
            }
            other => panic!("unexpected variant after round trip: {other:?}"),
        }
    }

    #[test]
    fn response_round_trips() {
        let response = UiDebugResponse::CaptureScreenshotResult {
            png_base64: "ZmFrZQ==".to_string(),
            width: 640,
            height: 480,
        };

        let json = serde_json::to_string(&response).expect("serialize response");
        let decoded: UiDebugResponse = serde_json::from_str(&json).expect("deserialize response");

        match decoded {
            UiDebugResponse::CaptureScreenshotResult {
                png_base64,
                width,
                height,
            } => {
                assert_eq!(png_base64, "ZmFrZQ==");
                assert_eq!(width, 640);
                assert_eq!(height, 480);
            }
            other => panic!("unexpected variant after round trip: {other:?}"),
        }
    }
}
