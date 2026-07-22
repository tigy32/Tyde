use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevInstanceMutablePath {
    pub env: &'static str,
    pub relative_path: &'static str,
}

pub const WORKFLOW_RUN_STORE_PATH_ENV: &str = "TYDE_WORKFLOW_RUN_STORE_PATH";
pub const CONFIGURED_HOST_STORE_PATH_ENV: &str = "TYDE_CONFIGURED_HOST_STORE_PATH";

pub const DEV_INSTANCE_MUTABLE_PATHS: &[DevInstanceMutablePath] = &[
    DevInstanceMutablePath {
        env: "TYDE_SESSION_STORE_PATH",
        relative_path: "sessions.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_PROJECT_STORE_PATH",
        relative_path: "projects.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_AGENT_TEAMS_STORE_PATH",
        relative_path: "agent_teams.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_REVIEW_STORE_PATH",
        relative_path: "reviews.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_SETTINGS_STORE_PATH",
        relative_path: "settings.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH",
        relative_path: "agents_view_preferences.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_CUSTOM_AGENTS_STORE_PATH",
        relative_path: "custom_agents.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_MCP_SERVERS_STORE_PATH",
        relative_path: "mcp_servers.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_STEERING_STORE_PATH",
        relative_path: "steering.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_SKILLS_STORE_PATH",
        relative_path: "skills.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_SKILLS_DIR_PATH",
        relative_path: "skills",
    },
    DevInstanceMutablePath {
        env: "TYDE_MOBILE_PAIRINGS_STORE_PATH",
        relative_path: "mobile_pairings.json",
    },
    DevInstanceMutablePath {
        env: WORKFLOW_RUN_STORE_PATH_ENV,
        relative_path: "workflow_runs.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_GLOBAL_WORKFLOWS_DIR",
        relative_path: "global_workflows",
    },
    DevInstanceMutablePath {
        env: CONFIGURED_HOST_STORE_PATH_ENV,
        relative_path: "configured_hosts.json",
    },
    DevInstanceMutablePath {
        env: "TYDE_TRACING_DIR_PATH",
        relative_path: "tracing",
    },
];

pub fn dev_instance_mutable_paths(
    store_dir: &Path,
) -> impl Iterator<Item = (&'static str, PathBuf)> + '_ {
    DEV_INSTANCE_MUTABLE_PATHS
        .iter()
        .map(|entry| (entry.env, store_dir.join(entry.relative_path)))
}

#[derive(Debug)]
pub struct BoundedDebugOutput {
    bytes: Vec<u8>,
    oldest_cursor: u64,
    next_cursor: u64,
    capacity: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DebugOutputSlice {
    pub cursor: u64,
    pub next_cursor: u64,
    pub oldest_cursor: u64,
    pub truncated: bool,
    pub output: String,
}

impl BoundedDebugOutput {
    pub fn new(capacity: usize) -> Self {
        Self {
            bytes: Vec::new(),
            oldest_cursor: 0,
            next_cursor: 0,
            capacity,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.next_cursor = self.next_cursor.saturating_add(bytes.len() as u64);
        if bytes.len() >= self.capacity {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len().saturating_sub(self.capacity)..]);
        } else {
            self.bytes.extend_from_slice(bytes);
            let overflow = self.bytes.len().saturating_sub(self.capacity);
            if overflow > 0 {
                self.bytes.drain(..overflow);
            }
        }
        self.oldest_cursor = self.next_cursor.saturating_sub(self.bytes.len() as u64);
    }

    pub fn read(&self, cursor: Option<u64>, max_bytes: usize) -> DebugOutputSlice {
        let requested = cursor.unwrap_or(self.oldest_cursor);
        let cursor = requested.clamp(self.oldest_cursor, self.next_cursor);
        let offset = cursor.saturating_sub(self.oldest_cursor) as usize;
        let end = offset.saturating_add(max_bytes).min(self.bytes.len());
        let next_cursor = cursor.saturating_add(end.saturating_sub(offset) as u64);
        DebugOutputSlice {
            cursor,
            next_cursor,
            oldest_cursor: self.oldest_cursor,
            truncated: requested < self.oldest_cursor,
            output: String::from_utf8_lossy(&self.bytes[offset..end]).into_owned(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn next_cursor(&self) -> u64 {
        self.next_cursor
    }
}

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
    use std::collections::HashSet;

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

    #[test]
    fn dev_instance_mutable_paths_are_unique_and_confined() {
        let expected = [
            ("TYDE_SESSION_STORE_PATH", "sessions.json"),
            ("TYDE_PROJECT_STORE_PATH", "projects.json"),
            ("TYDE_AGENT_TEAMS_STORE_PATH", "agent_teams.json"),
            ("TYDE_REVIEW_STORE_PATH", "reviews.json"),
            ("TYDE_SETTINGS_STORE_PATH", "settings.json"),
            (
                "TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH",
                "agents_view_preferences.json",
            ),
            ("TYDE_CUSTOM_AGENTS_STORE_PATH", "custom_agents.json"),
            ("TYDE_MCP_SERVERS_STORE_PATH", "mcp_servers.json"),
            ("TYDE_STEERING_STORE_PATH", "steering.json"),
            ("TYDE_SKILLS_STORE_PATH", "skills.json"),
            ("TYDE_SKILLS_DIR_PATH", "skills"),
            ("TYDE_MOBILE_PAIRINGS_STORE_PATH", "mobile_pairings.json"),
            ("TYDE_WORKFLOW_RUN_STORE_PATH", "workflow_runs.json"),
            ("TYDE_GLOBAL_WORKFLOWS_DIR", "global_workflows"),
            ("TYDE_CONFIGURED_HOST_STORE_PATH", "configured_hosts.json"),
            ("TYDE_TRACING_DIR_PATH", "tracing"),
        ];
        assert_eq!(
            DEV_INSTANCE_MUTABLE_PATHS
                .iter()
                .map(|entry| (entry.env, entry.relative_path))
                .collect::<Vec<_>>(),
            expected
        );

        let root = Path::new("/tmp/isolated-tyde");
        let paths = dev_instance_mutable_paths(root).collect::<Vec<_>>();
        assert_eq!(paths.len(), DEV_INSTANCE_MUTABLE_PATHS.len());
        assert!(paths.iter().all(|(_, path)| path.starts_with(root)));

        let envs = paths.iter().map(|(env, _)| *env).collect::<HashSet<_>>();
        assert_eq!(envs.len(), paths.len(), "environment keys must be unique");
        let disk_paths = paths.iter().map(|(_, path)| path).collect::<HashSet<_>>();
        assert_eq!(disk_paths.len(), paths.len(), "store paths must be unique");
    }

    #[test]
    fn bounded_debug_output_uses_monotonic_cursors_and_reports_loss() {
        let mut output = BoundedDebugOutput::new(8);
        output.append(b"abcd");
        let first = output.read(Some(0), 2);
        assert_eq!(first.output, "ab");
        assert_eq!(first.next_cursor, 2);

        output.append(b"efghij");
        let resumed = output.read(Some(first.next_cursor), 32);
        assert_eq!(resumed.oldest_cursor, 2);
        assert!(!resumed.truncated);
        assert_eq!(resumed.output, "cdefghij");
        assert_eq!(resumed.next_cursor, 10);

        output.append(b"klmnop");
        let stale = output.read(Some(resumed.next_cursor), 32);
        assert_eq!(stale.oldest_cursor, 8);
        assert!(!stale.truncated);
        assert_eq!(stale.output, "klmnop");
        assert_eq!(stale.next_cursor, 16);

        let lost = output.read(Some(0), 32);
        assert!(lost.truncated);
        assert_eq!(lost.cursor, 8);
        assert_eq!(lost.output, "ijklmnop");
    }
}
