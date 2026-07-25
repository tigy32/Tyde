use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use url::{Host, Url};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevInstanceMutablePath {
    pub env: &'static str,
    pub relative_path: &'static str,
}

pub const WORKFLOW_RUN_STORE_PATH_ENV: &str = "TYDE_WORKFLOW_RUN_STORE_PATH";
pub const CONFIGURED_HOST_STORE_PATH_ENV: &str = "TYDE_CONFIGURED_HOST_STORE_PATH";
pub const DEV_INSTANCE_HOME_ENV: &str = "HOME";
pub const DEV_INSTANCE_HERMES_HOME_ENV: &str = "HERMES_HOME";
pub const DEV_INSTANCE_HERMES_HOME_RELATIVE_PATH: &str = "hermes-home/.hermes";
pub const DEV_INSTANCE_DENY_PROXY_URL: &str = "http://127.0.0.1:9";
pub const DEV_INSTANCE_PROVIDER_CREDENTIAL_ENVS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "CEREBRAS_API_KEY",
    "COHERE_API_KEY",
    "DEEPSEEK_API_KEY",
    "ELEVENLABS_API_KEY",
    "FIREWORKS_API_KEY",
    "GEMINI_API_KEY",
    "GITHUB_TOKEN",
    "GOOGLE_API_KEY",
    "GROQ_API_KEY",
    "HF_TOKEN",
    "HUGGINGFACE_API_KEY",
    "MISTRAL_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "PERPLEXITY_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
];

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisposableHermesEnvironment {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loopback_stub: Option<HermesLoopbackStub>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HermesLoopbackStub {
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevInstanceHermesNetworkPolicy {
    Inherited,
    LoopbackStubOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevInstanceHermesEnvironmentAttestation {
    pub home: String,
    pub resolved_home: String,
    pub hermes_home: String,
    pub resolved_hermes_home: String,
    pub home_ephemeral: bool,
    pub hermes_home_ephemeral: bool,
    pub profiles: Vec<String>,
    pub loopback_stub_url: Option<String>,
    pub network_policy: DevInstanceHermesNetworkPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDisposableHermesEnvironment {
    pub home: PathBuf,
    pub hermes_home: PathBuf,
    pub attestation: DevInstanceHermesEnvironmentAttestation,
}

pub fn prepare_disposable_hermes_environment(
    store_dir: &Path,
    input: &DisposableHermesEnvironment,
) -> Result<PreparedDisposableHermesEnvironment, String> {
    validate_profile_names(&input.profiles)?;
    let stub = input
        .loopback_stub
        .as_ref()
        .map(validate_loopback_stub)
        .transpose()?;

    let resolved_store = fs::canonicalize(store_dir).map_err(|error| {
        format!(
            "failed to resolve dev instance store {}: {error}",
            store_dir.display()
        )
    })?;
    let home = store_dir.join("hermes-home");
    fs::create_dir(&home).map_err(|error| {
        format!(
            "failed to create disposable HOME {}: {error}",
            home.display()
        )
    })?;
    let resolved_home = fs::canonicalize(&home).map_err(|error| {
        format!(
            "failed to resolve disposable HOME {}: {error}",
            home.display()
        )
    })?;
    if !resolved_home.starts_with(&resolved_store) {
        return Err(format!(
            "disposable HOME escaped dev instance store {}",
            resolved_store.display()
        ));
    }

    let hermes_home = store_dir.join(DEV_INSTANCE_HERMES_HOME_RELATIVE_PATH);
    fs::create_dir(&hermes_home).map_err(|error| {
        format!(
            "failed to create disposable HERMES_HOME {}: {error}",
            hermes_home.display()
        )
    })?;
    let resolved_hermes_home = fs::canonicalize(&hermes_home).map_err(|error| {
        format!(
            "failed to resolve disposable HERMES_HOME {}: {error}",
            hermes_home.display()
        )
    })?;
    if !resolved_hermes_home.starts_with(&resolved_home) {
        return Err(format!(
            "disposable HERMES_HOME escaped disposable HOME {}",
            resolved_home.display()
        ));
    }

    let profiles_dir = hermes_home.join("profiles");
    fs::create_dir(&profiles_dir).map_err(|error| {
        format!(
            "failed to create disposable Hermes profiles directory {}: {error}",
            profiles_dir.display()
        )
    })?;
    let mut config_homes = vec![hermes_home.clone()];
    for profile in &input.profiles {
        let profile_home = profiles_dir.join(profile);
        fs::create_dir(&profile_home).map_err(|error| {
            format!(
                "failed to create disposable Hermes profile {}: {error}",
                profile_home.display()
            )
        })?;
        let resolved_profile_home = fs::canonicalize(&profile_home).map_err(|error| {
            format!(
                "failed to resolve disposable Hermes profile {}: {error}",
                profile_home.display()
            )
        })?;
        if !resolved_profile_home.starts_with(&resolved_hermes_home) {
            return Err(format!(
                "disposable Hermes profile escaped HERMES_HOME {}",
                resolved_hermes_home.display()
            ));
        }
        config_homes.push(profile_home);
    }

    for config_home in &config_homes {
        write_disposable_hermes_config(config_home, stub.as_ref())?;
    }

    let loopback_stub_url = stub.as_ref().map(|(url, _)| url.to_string());
    Ok(PreparedDisposableHermesEnvironment {
        home: resolved_home.clone(),
        hermes_home: resolved_hermes_home.clone(),
        attestation: DevInstanceHermesEnvironmentAttestation {
            home: home.display().to_string(),
            resolved_home: resolved_home.display().to_string(),
            hermes_home: hermes_home.display().to_string(),
            resolved_hermes_home: resolved_hermes_home.display().to_string(),
            home_ephemeral: true,
            hermes_home_ephemeral: true,
            profiles: input.profiles.clone(),
            loopback_stub_url,
            network_policy: if stub.is_some() {
                DevInstanceHermesNetworkPolicy::LoopbackStubOnly
            } else {
                DevInstanceHermesNetworkPolicy::Inherited
            },
        },
    })
}

fn validate_profile_names(profiles: &[String]) -> Result<(), String> {
    if profiles.len() > 64 {
        return Err("at most 64 disposable Hermes profiles may be requested".to_string());
    }
    let mut unique = HashSet::new();
    for profile in profiles {
        let valid = (1..=64).contains(&profile.len())
            && profile
                .bytes()
                .enumerate()
                .all(|(index, byte)| match byte {
                    b'a'..=b'z' | b'0'..=b'9' => true,
                    b'_' | b'-' => index > 0,
                    _ => false,
                });
        if !valid || profile == "default" {
            return Err(format!("invalid disposable Hermes profile name '{profile}'"));
        }
        if !unique.insert(profile) {
            return Err(format!(
                "duplicate disposable Hermes profile name '{profile}'"
            ));
        }
    }
    Ok(())
}

fn validate_loopback_stub(stub: &HermesLoopbackStub) -> Result<(Url, String), String> {
    let url = Url::parse(stub.base_url.trim())
        .map_err(|error| format!("invalid Hermes loopback stub base URL: {error}"))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_none()
    {
        return Err(
            "Hermes loopback stub base URL must be plain HTTP with an explicit port, no credentials, query, or fragment"
                .to_string(),
        );
    }
    let is_supported_loopback = match url.host() {
        Some(Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        _ => false,
    };
    if !is_supported_loopback {
        return Err(
            "Hermes loopback stub base URL must use 127.0.0.1 or ::1".to_string(),
        );
    }
    let model = stub.model.trim();
    if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err("Hermes loopback stub model must be 1-256 printable characters".to_string());
    }
    Ok((url, model.to_owned()))
}

fn write_disposable_hermes_config(
    home: &Path,
    stub: Option<&(Url, String)>,
) -> Result<(), String> {
    let config = match stub {
        Some((base_url, model)) => serde_json::json!({
            "model": {
                "provider": "openai",
                "default": model,
                "base_url": base_url.as_str(),
            }
        }),
        None => serde_json::json!({}),
    };
    fs::write(
        home.join("config.yaml"),
        serde_json::to_vec_pretty(&config)
            .map_err(|error| format!("failed to serialize disposable Hermes config: {error}"))?,
    )
    .map_err(|error| {
        format!(
            "failed to write disposable Hermes config in {}: {error}",
            home.display()
        )
    })?;
    if stub.is_some() {
        fs::write(
            home.join(".env"),
            "OPENAI_API_KEY=tyde-local-loopback-stub\n",
        )
        .map_err(|error| {
            format!(
                "failed to write disposable Hermes stub environment in {}: {error}",
                home.display()
            )
        })?;
    }
    Ok(())
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
    fn disposable_hermes_environment_is_confined_and_seeds_loopback_stub() {
        let store = tempfile::tempdir().expect("store dir");
        let prepared = prepare_disposable_hermes_environment(
            store.path(),
            &DisposableHermesEnvironment {
                profiles: vec!["qa".to_owned()],
                loopback_stub: Some(HermesLoopbackStub {
                    base_url: "http://127.0.0.1:43123/v1".to_owned(),
                    model: "tyde-stub".to_owned(),
                }),
            },
        )
        .expect("prepare disposable Hermes environment");

        assert!(prepared.home.starts_with(store.path()));
        assert!(prepared.hermes_home.starts_with(&prepared.home));
        assert_eq!(
            prepared.attestation.network_policy,
            DevInstanceHermesNetworkPolicy::LoopbackStubOnly
        );
        assert!(prepared.attestation.home_ephemeral);
        assert!(prepared.attestation.hermes_home_ephemeral);
        assert_eq!(
            prepared.attestation.loopback_stub_url.as_deref(),
            Some("http://127.0.0.1:43123/v1")
        );
        assert_eq!(
            fs::read_to_string(prepared.hermes_home.join("config.yaml"))
                .expect("read default config"),
            fs::read_to_string(
                prepared
                    .hermes_home
                    .join("profiles")
                    .join("qa")
                    .join("config.yaml")
            )
            .expect("read named config")
        );
        assert!(
            fs::read_to_string(prepared.hermes_home.join("config.yaml"))
                .expect("read config")
                .contains("\"base_url\": \"http://127.0.0.1:43123/v1\"")
        );
        assert_eq!(
            fs::read_to_string(prepared.hermes_home.join(".env")).expect("read stub env"),
            "OPENAI_API_KEY=tyde-local-loopback-stub\n"
        );
        let attestation =
            serde_json::to_value(&prepared.attestation).expect("serialize attestation");
        assert_eq!(attestation["homeEphemeral"], Value::Bool(true));
        assert_eq!(attestation["hermesHomeEphemeral"], Value::Bool(true));
        assert_eq!(
            attestation["networkPolicy"],
            Value::String("loopback_stub_only".to_owned())
        );
    }

    #[test]
    fn disposable_hermes_environment_rejects_egress_and_profile_traversal() {
        let store = tempfile::tempdir().expect("store dir");
        let egress = prepare_disposable_hermes_environment(
            store.path(),
            &DisposableHermesEnvironment {
                profiles: Vec::new(),
                loopback_stub: Some(HermesLoopbackStub {
                    base_url: "https://api.openai.com/v1".to_owned(),
                    model: "paid-model".to_owned(),
                }),
            },
        )
        .expect_err("external provider URL must be rejected");
        assert!(egress.contains("plain HTTP"));

        let traversal = prepare_disposable_hermes_environment(
            store.path(),
            &DisposableHermesEnvironment {
                profiles: vec!["../real-home".to_owned()],
                loopback_stub: None,
            },
        )
        .expect_err("profile traversal must be rejected");
        assert!(traversal.contains("invalid disposable Hermes profile"));
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
