use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use protocol::{
    AcpAdapterId, AcpAgentSpec, AgentInput, BackendKind, CapacityBucket, CapacityBucketId,
    CapacityCoverage, CapacityMeasure, CapacityReport, CapacityReset, CapacityScope,
    CapacitySource, CapacityUnavailableReason, CapacityWindow, ChatEvent, MessageTokenUsage,
    SessionId, SessionSettingField, SessionSettingFieldType, SessionSettingsSchema,
    SessionSettingsValues, SpawnCostHint, ToolExecutionOutcome, ToolExecutionResult,
    ToolRequestType, ToolUseData, ValueProvenance,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::backend::acp::adapter::{
    AcpRequestUsageRecord, AcpRequestUsageSnapshot, AcpRequestUsageWait,
};
use crate::backend::antigravity_stream::{
    AgyUsageBucket, AgyUsageReport, MCP_DISPATCH_TOOL, exit_code_from_result, mcp_inner_call,
    parse_frame, tool_request_type, usage_report_from_frame,
};
use crate::backend::skill_projection::{
    DescriptionPolicy, ProjectionPolicy, SkillRefusal, create_private_dir, create_private_root,
    discard_wrapper, inspect_skill, write_wrapper,
};
use crate::backend::{
    Backend, BackendCompactionAvailability, BackendCompactionCapability,
    BackendCompactionCapabilityEvidence, BackendCompactionCoordinator,
    BackendCompactionNotDispatchedReason, BackendCompactionRequest, BackendCompactionStart,
    BackendCompactionUnavailableReason, BackendDiscovery, BackendDiscoveryError, BackendSession,
    BackendSpawnConfig, BackendStartupError, EventStream, backend_fork_unsupported_message,
    resolve_settings as resolve_backend_settings,
};

const ANTIGRAVITY_AGENT_NAME: &str = "antigravity";
/// `/usage` answers locally in well under a second; this only bounds a probe
/// that has stopped answering.
const ANTIGRAVITY_CAPACITY_TIMEOUT: Duration = Duration::from_secs(30);
const ANTIGRAVITY_SKILLS_ROOT_PREFIX: &str = "tyde-antigravity-skills-";

const ANTIGRAVITY_SKILL_PROJECTION: ProjectionPolicy = ProjectionPolicy {
    refused_resource_names: &[],
    description: DescriptionPolicy::Required,
};

pub(crate) fn agent_spec() -> AcpAgentSpec {
    AcpAgentSpec {
        command: "agy".to_owned(),
        args: vec![
            "acp".to_owned(),
            "--dangerously-skip-permissions".to_owned(),
        ],
        cwd: None,
        env: Default::default(),
        adapter: AcpAdapterId::Antigravity,
    }
}

pub(crate) fn configure(mut config: BackendSpawnConfig) -> BackendSpawnConfig {
    config.acp_agent = Some(agent_spec());
    config
}

pub(crate) struct AntigravitySkillProjection {
    root: tempfile::TempDir,
}

impl AntigravitySkillProjection {
    pub(crate) fn path(&self) -> &Path {
        self.root.path()
    }
}

#[derive(Default)]
pub(crate) struct AntigravitySkillSetup {
    pub(crate) projection: Option<AntigravitySkillProjection>,
    pub(crate) degraded_notice: Option<String>,
}

pub(crate) fn prepare_antigravity_skills(
    skills: &[crate::backend::customization::ResolvedSkill],
) -> Result<AntigravitySkillSetup, String> {
    if skills.is_empty() {
        return Ok(AntigravitySkillSetup::default());
    }

    let mut inspected = Vec::new();
    let mut refusals = Vec::new();
    let mut claimed = BTreeSet::new();
    for skill in skills {
        match inspect_skill(skill, &mut claimed, ANTIGRAVITY_SKILL_PROJECTION) {
            Ok(entry) => inspected.push(entry),
            Err(reason) => refusals.push(SkillRefusal {
                name: skill.name.clone(),
                reason,
            }),
        }
    }

    let mut projection = None;
    if !inspected.is_empty() {
        let root = create_private_root(None, ANTIGRAVITY_SKILLS_ROOT_PREFIX)?;
        let skills_dir = root.path().join(".agents").join("skills");
        create_private_dir(&skills_dir)?;
        for entry in inspected {
            if let Err(reason) = write_wrapper(&skills_dir, &entry) {
                discard_wrapper(&skills_dir, &entry.projected.name);
                refusals.push(SkillRefusal {
                    name: entry.source_name,
                    reason,
                });
            }
        }
        if fs::read_dir(&skills_dir)
            .map_err(|err| format!("Failed to inspect Antigravity skill projection: {err}"))?
            .next()
            .is_some()
        {
            projection = Some(AntigravitySkillProjection { root });
        }
    }

    for refusal in &refusals {
        tracing::warn!("Antigravity skill projection: {}", refusal.describe());
    }
    let degraded_notice = (!refusals.is_empty()).then(|| {
        format!(
            "Antigravity started without some selected skills:\n{}",
            refusals
                .iter()
                .map(|refusal| format!("- {}", refusal.describe()))
                .collect::<Vec<_>>()
                .join("\n")
        )
    });

    Ok(AntigravitySkillSetup {
        projection,
        degraded_notice,
    })
}

pub(crate) fn prepare_antigravity_tool_policy(
    categories: &[protocol::ToolCategory],
) -> Result<Option<tempfile::TempDir>, String> {
    if categories.is_empty() {
        return Ok(None);
    }
    let root = create_private_root(None, "tyde-antigravity-tool-policy-")?;
    let customization_dir = root.path().join(".agents");
    create_private_dir(&customization_dir)?;
    let mut handlers = Vec::new();
    for category in categories {
        let tool = match category {
            protocol::ToolCategory::AskUser => "ask_question",
            protocol::ToolCategory::AgentDelegation => {
                return Err("Antigravity cannot enforce delegation exclusion".into());
            }
        };
        let response = json!({
            "decision": "deny",
            "reason": "This tool is unavailable because the session excludes user questions.",
        })
        .to_string();
        #[cfg(not(windows))]
        let command = format!("printf '%s' '{}'", response.replace('\'', r"'\''"));
        #[cfg(windows)]
        let command = format!("echo {response}");
        handlers.push(json!({
            "matcher": tool,
            "hooks": [{"type": "command", "command": command, "timeout": 10}],
        }));
    }
    crate::backend::skill_projection::write_private_file(
        &customization_dir.join("hooks.json"),
        &json!({"tyde-tool-category-policy": {"PreToolUse": handlers}}).to_string(),
    )?;
    tracing::info!(
        category_count = categories.len(),
        "Installed Antigravity native tool-category deny hooks"
    );
    Ok(Some(root))
}

// ── Capacity ────────────────────────────────────────────────────────────────

/// Reads the account's remaining quota with no conversation and no live agent.
///
/// `/usage` is answered by print mode without a model request or selection.
pub(crate) async fn read_capacity_out_of_band() -> protocol::BackendCapacityState {
    match read_antigravity_capacity().await {
        Ok(report) => protocol::BackendCapacityState::Known { report },
        Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
    }
}

async fn read_antigravity_capacity() -> Result<CapacityReport, CapacityUnavailableReason> {
    let mut command = crate::backend::subprocess::command("agy")
        .map_err(|_| CapacityUnavailableReason::SourceUnreachable)?;
    command.args(["--output-format", "stream-json", "-p", "/usage"]);
    command
        .stdin(crate::backend::subprocess::ProcessStdio::Null)
        .stdout(crate::backend::subprocess::ProcessStdio::Piped)
        .stderr(crate::backend::subprocess::ProcessStdio::Null);

    let output = tokio::time::timeout(ANTIGRAVITY_CAPACITY_TIMEOUT, command.output())
        .await
        .map_err(|_| CapacityUnavailableReason::SourceTimedOut)?
        .map_err(|_| CapacityUnavailableReason::SourceUnreachable)?;
    if !output.status.success() {
        return Err(CapacityUnavailableReason::SourceUnreachable);
    }
    let report = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| parse_frame(line.trim()).ok())
        .find_map(|frame| usage_report_from_frame(&frame))
        .ok_or(CapacityUnavailableReason::MalformedReport)?;
    map_antigravity_capacity(&report)
}

fn map_antigravity_capacity(
    report: &AgyUsageReport,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let buckets = report
        .groups
        .iter()
        .flat_map(|group| {
            group
                .buckets
                .iter()
                .map(|bucket| antigravity_capacity_bucket(&group.name, bucket))
        })
        .collect::<Vec<_>>();
    if buckets.is_empty() {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    Ok(CapacityReport {
        source: CapacitySource::AntigravityUsageCommand,
        observed_at_ms: Some(now_ms()),
        plan: None,
        buckets,
        coverage: CapacityCoverage::AllVendorBuckets,
    })
}

fn antigravity_capacity_bucket(group: &str, bucket: &AgyUsageBucket) -> CapacityBucket {
    let measure = match bucket.remaining_fraction {
        Some(remaining) => {
            let remaining_percent = (remaining * 100.0).round().clamp(0.0, 100.0) as u8;
            CapacityMeasure::UsedPercent {
                used_percent: 100_u8.saturating_sub(remaining_percent),
                remaining_percent,
                provenance: ValueProvenance {
                    vendor_reported: false,
                },
            }
        }
        None => CapacityMeasure::ReportedWithoutMagnitude,
    };
    CapacityBucket {
        id: CapacityBucketId::Antigravity {
            bucket: bucket.id.clone(),
        },
        label: bucket.name.clone(),
        measure,
        scope: CapacityScope::ModelFamily {
            name: group.to_string(),
        },
        window: match bucket.window.as_str() {
            "weekly" => CapacityWindow::Rolling {
                duration_minutes: 7 * 24 * 60,
            },
            "5h" => CapacityWindow::Rolling {
                duration_minutes: 5 * 60,
            },
            _ => CapacityWindow::NotReported,
        },
        reset: bucket
            .reset_time
            .as_deref()
            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
            .map(|stamp| CapacityReset::At {
                at_ms: stamp.timestamp_millis().max(0) as u64,
            })
            .unwrap_or(CapacityReset::NotReported),
        status: None,
    }
}

// ── Trajectory metadata, relocation, session enumeration ───────────────────

#[derive(Clone, PartialEq, prost::Message)]
struct AntigravityTrajectoryMetadata {
    #[prost(message, repeated, tag = "1")]
    workspaces: Vec<AntigravityWorkspace>,
    #[prost(string, repeated, tag = "7")]
    workspace_uris: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AntigravityWorkspace {
    #[prost(string, tag = "1")]
    uri: String,
}

pub(crate) fn replace_antigravity_metadata(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), String> {
    let mut connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|error| format!("Cannot open native conversation metadata: {error}"))?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("Cannot lock native conversation metadata: {error}"))?;
    let changed = transaction
        .execute(
            "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main' AND data = ?2",
            rusqlite::params![replacement, expected],
        )
        .map_err(|error| {
            format!("Cannot update native conversation workspace metadata: {error}")
        })?;
    if changed != 1 {
        return Err(
            "Native conversation metadata changed concurrently; relocation cannot proceed"
                .to_owned(),
        );
    }
    transaction
        .commit()
        .map_err(|error| format!("Cannot commit native conversation workspace metadata: {error}"))
}

fn relocate_antigravity_metadata(
    path: &Path,
    previous_root: &str,
    roots: &[String],
) -> Result<(Vec<u8>, Vec<u8>), String> {
    use prost::Message;
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| format!("Cannot read native conversation metadata: {error}"))?;
    let before: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("Cannot read native workspace metadata: {error}"))?;
    drop(connection);
    let stored = AntigravityTrajectoryMetadata::decode(before.as_slice())
        .map_err(|error| format!("Invalid native conversation workspace metadata: {error}"))?;
    let previous_uri = url::Url::from_file_path(previous_root)
        .map_err(|()| "Invalid previous workspace path".to_owned())?
        .to_string();
    let stored_primary = stored
        .workspace_uris
        .first()
        .or_else(|| stored.workspaces.first().map(|workspace| &workspace.uri));
    if stored_primary != Some(&previous_uri) {
        return Err(
            "Native conversation workspace differs from this runtime; refusing a stale relocation"
                .to_owned(),
        );
    }
    let uris = roots
        .iter()
        .map(|root| {
            url::Url::from_file_path(root)
                .map(|uri| uri.to_string())
                .map_err(|()| format!("Cannot encode workspace URI: {root}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let metadata = AntigravityTrajectoryMetadata {
        workspaces: uris
            .iter()
            .map(|uri| AntigravityWorkspace { uri: uri.clone() })
            .collect(),
        workspace_uris: uris,
    };
    let mut after = metadata.encode_to_vec();
    let mut remaining = before.as_slice();
    while !remaining.is_empty() {
        let field = remaining;
        let (tag, wire) = prost::encoding::decode_key(&mut remaining)
            .map_err(|error| format!("Invalid native workspace metadata: {error}"))?;
        prost::encoding::skip_field(
            wire,
            tag,
            &mut remaining,
            prost::encoding::DecodeContext::default(),
        )
        .map_err(|error| format!("Invalid native workspace metadata field: {error}"))?;
        if tag != 1 && tag != 7 {
            after.extend_from_slice(&field[..field.len() - remaining.len()]);
        }
    }
    replace_antigravity_metadata(path, &before, &after)?;
    tracing::info!(
        root_count = roots.len(),
        "Updated native Antigravity trajectory workspace fields without changing history"
    );
    Ok((before, after))
}

pub(crate) async fn relocate_workspace_session(
    configured_storage: Option<&Path>,
    session_id: &str,
    previous: &str,
    roots: &[String],
) -> Result<(PathBuf, Vec<u8>, Vec<u8>), String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    let previous_exec =
        tyde_process_env::launch::WorkspacePath::host(previous)?.execution_string()?;
    let replacement_roots = crate::backend::execution_workspace_roots(roots)?;
    let db_for_task = db_path.clone();
    let (before, after) = crate::backend::subprocess::spawn_blocking(move || {
        relocate_antigravity_metadata(&db_for_task, &previous_exec, &replacement_roots)
    })
    .await
    .map_err(|error| format!("Antigravity metadata relocation task failed: {error}"))??;
    Ok((db_path, before, after))
}

fn antigravity_session_workspace_roots(path: &Path) -> Result<Vec<String>, String> {
    use prost::Message;
    let connection = open_antigravity_db_readonly(path)?;
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            format!(
                "Cannot read Antigravity workspace metadata {}: {error}",
                path.display()
            )
        })?;
    let metadata = AntigravityTrajectoryMetadata::decode(bytes.as_slice()).map_err(|error| {
        format!(
            "Invalid Antigravity workspace metadata {}: {error}",
            path.display()
        )
    })?;
    metadata
        .workspaces
        .into_iter()
        .map(|workspace| {
            let uri = url::Url::parse(&workspace.uri)
                .map_err(|error| format!("Invalid Antigravity workspace URI: {error}"))?;
            let path = uri.to_file_path().map_err(|()| {
                format!(
                    "Antigravity workspace is not a local file URI: {}",
                    workspace.uri
                )
            })?;
            path.into_os_string()
                .into_string()
                .map_err(|_| "Antigravity workspace path is not UTF-8".to_owned())
        })
        .collect()
}

fn list_antigravity_sessions(
    conversations_dir: &Path,
    brain_dir: &Path,
) -> Result<Vec<BackendSession>, String> {
    let Ok(entries) = fs::read_dir(conversations_dir) else {
        return Ok(Vec::new());
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let session_id = SessionId(id.to_string());
        if !is_antigravity_native_session_id(&session_id) {
            continue;
        }
        let updated_at_ms = match entry.metadata() {
            Ok(metadata) => metadata.modified().ok().and_then(system_time_to_ms),
            Err(_) if !path.exists() => continue,
            Err(error) => {
                return Err(format!(
                    "Cannot inspect Antigravity session {}: {error}",
                    path.display()
                ));
            }
        };
        let workspace_roots = match antigravity_session_workspace_roots(&path) {
            Ok(roots) => roots,
            Err(_) if !path.exists() => continue,
            Err(error) => return Err(error),
        };
        let opening = antigravity_transcript_opening(brain_dir, id);
        sessions.push(BackendSession {
            id: session_id,
            backend_kind: BackendKind::Antigravity,
            workspace_roots,
            title: opening.as_ref().and_then(|opening| opening.title.clone()),
            token_count: None,
            created_at_ms: opening.as_ref().and_then(|opening| opening.created_at_ms),
            updated_at_ms,
            resumable: true,
        });
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
    Ok(sessions)
}

struct AntigravityTranscriptOpening {
    title: Option<String>,
    created_at_ms: Option<u64>,
}

fn antigravity_transcript_opening(
    brain_dir: &Path,
    conversation_id: &str,
) -> Option<AntigravityTranscriptOpening> {
    let path = brain_dir
        .join(conversation_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    let text = fs::read_to_string(path).ok()?;
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let value = serde_json::from_str::<Value>(first).ok()?;
    let created_at_ms = value
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .map(|stamp| stamp.timestamp_millis().max(0) as u64);
    Some(AntigravityTranscriptOpening {
        title: value
            .get("content")
            .and_then(Value::as_str)
            .and_then(antigravity_title_from_user_input),
        created_at_ms,
    })
}

fn antigravity_title_from_user_input(content: &str) -> Option<String> {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    let start = content.find(OPEN)? + OPEN.len();
    let end = content[start..].find(CLOSE)? + start;
    let request = crate::backend::workspace_prompt_user_text(content[start..end].trim());
    let first_line = request.lines().find(|line| !line.trim().is_empty())?.trim();
    if first_line.is_empty() {
        return None;
    }
    Some(first_line.chars().take(120).collect())
}

// ── Native trajectory protobuf decoding (per-request usage, MCP media, subagents) ─

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyStep {
    #[prost(int32, tag = "1")]
    kind: i32,
    #[prost(int32, tag = "4")]
    status: i32,
    #[prost(message, optional, tag = "5")]
    metadata: Option<NativeAgyMetadata>,
    #[prost(message, optional, tag = "20")]
    response: Option<NativeAgyResponse>,
    #[prost(message, optional, tag = "140")]
    generic: Option<NativeAgyGeneric>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyMetadata {
    #[prost(message, optional, tag = "1")]
    created_at: Option<NativeAgyTimestamp>,
    #[prost(message, optional, tag = "4")]
    tool_call: Option<NativeAgyToolCall>,
    #[prost(message, optional, tag = "9")]
    usage: Option<NativeAgyUsage>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyUsage {
    #[prost(uint64, tag = "2")]
    input: u64,
    #[prost(uint64, tag = "3")]
    output: u64,
    #[prost(uint64, tag = "4")]
    cache_write: u64,
    #[prost(uint64, tag = "5")]
    cache_read: u64,
    #[prost(uint64, tag = "9")]
    thinking: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyResponse {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "3")]
    thinking: String,
    #[prost(string, tag = "6")]
    message_id: String,
    #[prost(message, repeated, tag = "7")]
    tools: Vec<NativeAgyToolCall>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyToolCall {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    arguments: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyGeneric {
    #[prost(message, optional, tag = "2")]
    result: Option<NativeAgyGenericResult>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyGenericResult {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct NativeAgyExecutor {
    #[prost(enumeration = "NativeAgyTerminationReason", tag = "1")]
    termination_reason: i32,
    #[prost(bool, tag = "4")]
    proceeded_with_auto_continue: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum NativeAgyTerminationReason {
    Unspecified = 0,
    Error = 1,
    UserCanceled = 2,
    MaxInvocations = 3,
    NoToolCall = 4,
    MaxForcedInvocations = 6,
    EarlyContinue = 7,
    TerminalStepType = 8,
    TerminalCustomHook = 9,
    InjectedResponse = 10,
    MaxTokenBudgetExceeded = 11,
}

fn native_usage_to_protocol(usage: &NativeAgyUsage) -> protocol::TokenUsage {
    protocol::TokenUsage {
        input_tokens: usage.input,
        output_tokens: usage.output,
        total_tokens: usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write),
        cached_prompt_tokens: Some(usage.cache_read),
        cache_creation_input_tokens: Some(usage.cache_write),
        reasoning_tokens: Some(usage.thinking),
    }
}

fn open_antigravity_db_readonly(db_path: &Path) -> Result<rusqlite::Connection, String> {
    let connection =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| {
                format!(
                    "Cannot open Antigravity trajectory {}: {error}",
                    db_path.display()
                )
            })?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("Cannot configure Antigravity SQLite busy timeout: {error}"))?;
    Ok(connection)
}

async fn watch_antigravity_paths<T, F>(targets: Vec<PathBuf>, check: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnMut() -> Result<Option<T>, String> + Send + 'static,
{
    use notify::Watcher;
    let (updates, mut changed) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = updates.send(event);
    })
    .map_err(|error| format!("Cannot observe Antigravity storage updates: {error}"))?;

    let mut watched_dirs = BTreeSet::new();
    let mut interesting = Vec::new();
    for target in &targets {
        interesting.push(target.clone());
        if target.extension().and_then(|ext| ext.to_str()) == Some("db") {
            interesting.push(target.with_extension("db-wal"));
            interesting.push(target.with_extension("db-shm"));
        }
        let mut dir = target.parent();
        let mut recursive = false;
        while let Some(candidate) = dir {
            if candidate.is_dir() {
                if watched_dirs.insert((candidate.to_path_buf(), recursive)) {
                    let mode = if recursive {
                        notify::RecursiveMode::Recursive
                    } else {
                        notify::RecursiveMode::NonRecursive
                    };
                    watcher.watch(candidate, mode).map_err(|error| {
                        format!(
                            "Cannot watch Antigravity directory {}: {error}",
                            candidate.display()
                        )
                    })?;
                }
                break;
            }
            recursive = true;
            dir = candidate.parent();
        }
    }

    let check = std::sync::Arc::new(std::sync::Mutex::new(check));
    loop {
        let check_clone = std::sync::Arc::clone(&check);
        let outcome = crate::backend::subprocess::spawn_blocking(move || {
            let mut guard = check_clone.lock().expect("antigravity watch check mutex");
            guard()
        })
        .await
        .map_err(|error| format!("Antigravity storage check task failed: {error}"))??;
        if let Some(value) = outcome {
            return Ok(value);
        }
        loop {
            let event: notify::Result<notify::Event> = changed
                .recv()
                .await
                .ok_or_else(|| "Antigravity storage observer closed".to_owned())?;
            let event = event
                .map_err(|error| format!("Antigravity storage observation failed: {error}"))?;
            if matches!(
                event.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            ) && event.paths.iter().any(|path| {
                interesting
                    .iter()
                    .any(|target| path == target || target.starts_with(path))
            }) {
                break;
            }
        }
    }
}

fn extract_user_request_text(content: &str) -> &str {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    if let Some(start) = content.find(OPEN) {
        let body_start = start + OPEN.len();
        if let Some(rel_end) = content[body_start..].find(CLOSE) {
            return content[body_start..body_start + rel_end].trim();
        }
    }
    content.trim()
}

/// Reads original user-uploaded image attachments for a replayed `USER_INPUT`
/// step in `agy`'s `transcript_full.jsonl` matching `expected_text` at or after
/// `from_index`. Returns `(images, next_search_index)`.
pub(crate) async fn antigravity_user_message_images(
    configured_storage: Option<&Path>,
    session_id: &str,
    from_index: usize,
    expected_text: &str,
) -> Result<(Vec<protocol::ImageData>, usize), String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let brain_dir = antigravity_brain_dir(&conversations_dir);
    let session_id = session_id.to_owned();
    let expected_text = expected_text.to_owned();
    crate::backend::subprocess::spawn_blocking(move || {
        read_user_message_images_sync(&brain_dir, &session_id, from_index, &expected_text)
    })
    .await
    .map_err(|error| format!("Antigravity replay image lookup task failed: {error}"))?
}

fn read_user_message_images_sync(
    brain_dir: &Path,
    session_id: &str,
    from_index: usize,
    expected_text: &str,
) -> Result<(Vec<protocol::ImageData>, usize), String> {
    // `agy acp` (1.5.13) `session/load` omits image content blocks from
    // `user_message_chunk`; uploaded user image attachments are recorded only
    // on the matching `USER_INPUT` row in `transcript_full.jsonl`.
    let path = brain_dir
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    if !path.is_file() {
        return Err(format!(
            "Antigravity transcript missing for replayed session at {}",
            path.display()
        ));
    }
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "Cannot read Antigravity transcript {}: {error}",
            path.display()
        )
    })?;
    let expected_normalized = crate::backend::workspace_prompt_user_text(expected_text).trim();
    let mut user_rows = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row = serde_json::from_str::<Value>(trimmed).map_err(|error| {
            format!(
                "Invalid JSON line in Antigravity transcript {}: {error}",
                path.display()
            )
        })?;
        if row.get("type").and_then(Value::as_str) == Some("USER_INPUT") {
            user_rows.push(row);
        }
    }
    let matched = user_rows
        .iter()
        .enumerate()
        .skip(from_index)
        .find(|(_, row)| {
            row.get("content").and_then(Value::as_str).map(|content| {
                crate::backend::workspace_prompt_user_text(extract_user_request_text(content))
                    .trim()
            }) == Some(expected_normalized)
        });
    let Some((matched_idx, row)) = matched else {
        return Err(format!(
            "Antigravity transcript {} has no USER_INPUT row matching replayed message at index >= {from_index}",
            path.display()
        ));
    };
    let Some(media) = row.get("media").and_then(Value::as_array) else {
        return Ok((Vec::new(), matched_idx + 1));
    };
    let mut images = Vec::new();
    for item in media {
        let Some(mime) = item.get("mime_type").and_then(Value::as_str) else {
            continue;
        };
        if !mime.starts_with("image/") {
            continue;
        }
        let Some(uri) = item.get("uri").and_then(Value::as_str) else {
            return Err("Antigravity user image metadata omitted its file path".to_owned());
        };
        let media_path = PathBuf::from(uri);
        if !media_path.is_absolute() {
            return Err(format!(
                "Antigravity user image metadata contained non-absolute path: {uri}"
            ));
        }
        let bytes = fs::read(&media_path).map_err(|error| {
            format!(
                "Cannot read Antigravity uploaded image {}: {error}",
                media_path.display()
            )
        })?;
        images.push(protocol::ImageData {
            media_type: mime.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok((images, matched_idx + 1))
}

/// Reads completed metered model requests and terminal `PLANNER_RESPONSE`
/// progress (kind 15) from `agy`'s trajectory SQLite store for `session_id`.
/// When `wait` is `Some(...)`, watches SQLite WAL updates via `notify` until
/// the wait predicate (`ProviderMessageId` or `TextOnlyAtOrAfter`) is satisfied.
pub(crate) async fn antigravity_request_usages(
    configured_storage: Option<&Path>,
    session_id: &str,
    wait: Option<AcpRequestUsageWait>,
) -> Result<AcpRequestUsageSnapshot, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    let Some(wait) = wait else {
        return crate::backend::subprocess::spawn_blocking(move || {
            if !db_path.is_file() {
                return Ok(AcpRequestUsageSnapshot::default());
            }
            read_request_usages_sync(&db_path)
        })
        .await
        .map_err(|error| format!("Antigravity usage lookup task failed: {error}"))?;
    };
    let db_for_watch = db_path.clone();
    watch_antigravity_paths(vec![db_path], move || {
        if !db_for_watch.is_file() {
            return Ok(None);
        }
        let snapshot = read_request_usages_sync(&db_for_watch)?;
        let ready = match &wait {
            AcpRequestUsageWait::TextOnlyAtOrAfter(min_ordinal) => snapshot
                .terminal_text_only_ordinals
                .iter()
                .any(|&ordinal| ordinal >= *min_ordinal),
            AcpRequestUsageWait::ProviderMessageId(id) => {
                snapshot.terminal_provider_message_ids.contains(id)
            }
        };
        Ok(ready.then_some(snapshot))
    })
    .await
}

fn read_request_usages_sync(db_path: &Path) -> Result<AcpRequestUsageSnapshot, String> {
    use prost::Message;
    let connection = open_antigravity_db_readonly(db_path)?;
    let mut query = connection
        .prepare("SELECT step_format, step_payload FROM steps ORDER BY idx")
        .map_err(|error| format!("Cannot prepare Antigravity steps query: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("Cannot query Antigravity steps: {error}"))?;
    let mut snapshot = AcpRequestUsageSnapshot::default();
    for row in rows {
        let (format, payload) =
            row.map_err(|error| format!("Cannot read Antigravity step row: {error}"))?;
        if format != 0 {
            continue;
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|error| format!("Cannot decode Antigravity step protobuf: {error}"))?;
        if step.kind != 15 || !matches!(step.status, 3 | 6 | 7 | 12) {
            continue;
        }
        snapshot.terminal_responses += 1;
        let terminal_ordinal = snapshot.terminal_responses;
        let has_tool_calls = step
            .response
            .as_ref()
            .is_some_and(|response| !response.tools.is_empty());
        if !has_tool_calls {
            snapshot.terminal_text_only_ordinals.push(terminal_ordinal);
        }
        if let Some(id) = step
            .response
            .as_ref()
            .map(|r| r.message_id.as_str())
            .filter(|id| !id.is_empty())
        {
            snapshot.terminal_provider_message_ids.insert(id.to_owned());
        }
        if step.status != 3 {
            continue;
        }
        let response = step
            .response
            .ok_or_else(|| "Antigravity completed a planner response without payload".to_owned())?;
        if response.message_id.is_empty() {
            return Err("Antigravity completed a planner response without message_id".to_owned());
        }
        let usage = step.metadata.and_then(|meta| meta.usage).ok_or_else(|| {
            format!(
                "Antigravity completed planner response {} without token usage",
                response.message_id
            )
        })?;
        snapshot.records.push(AcpRequestUsageRecord {
            provider_message_id: response.message_id,
            usage: native_usage_to_protocol(&usage),
            terminal_ordinal,
            has_tool_calls,
        });
    }
    Ok(snapshot)
}

/// Looks up the provider `PLANNER_RESPONSE` `message_id` that declared
/// `tool_call_id` in `agy`'s trajectory SQLite store, waiting on SQLite WAL
/// updates via `notify` if the step has not flushed yet.
pub(crate) async fn antigravity_provider_message_id_for_tool(
    configured_storage: Option<&Path>,
    session_id: &str,
    tool_call_id: &str,
) -> Result<String, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    let tid = tool_call_id.to_owned();
    let db_for_watch = db_path.clone();
    watch_antigravity_paths(vec![db_path], move || {
        if !db_for_watch.is_file() {
            return Ok(None);
        }
        find_planner_message_id_for_tool_sync(&db_for_watch, &tid)
    })
    .await
}

fn find_planner_message_id_for_tool_sync(
    db_path: &Path,
    tool_call_id: &str,
) -> Result<Option<String>, String> {
    use prost::Message;
    let connection = open_antigravity_db_readonly(db_path)?;
    let mut query = connection
        .prepare("SELECT step_format, step_payload FROM steps ORDER BY idx DESC")
        .map_err(|error| format!("Cannot prepare Antigravity tool-owner query: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("Cannot query Antigravity tool owner: {error}"))?;
    for row in rows {
        let (format, payload) =
            row.map_err(|error| format!("Cannot read Antigravity step row: {error}"))?;
        if format != 0 {
            continue;
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|error| format!("Cannot decode Antigravity step protobuf: {error}"))?;
        if step.kind != 15 {
            continue;
        }
        let Some(response) = step.response else {
            continue;
        };
        if !response.message_id.is_empty()
            && response.tools.iter().any(|call| call.id == tool_call_id)
        {
            return Ok(Some(response.message_id));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AntigravityTurnBaseline {
    pub(crate) system_messages: usize,
    pub(crate) planner_responses: usize,
    pub(crate) terminal_responses: usize,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AntigravityContinuationSettled {
    pub(crate) expected_turn_text_non_ws: usize,
}

/// Counts `SYSTEM_MESSAGE` (kind 101) and `PLANNER_RESPONSE` (kind 15) steps
/// currently recorded in `agy`'s trajectory SQLite store for `session_id`.
pub(crate) async fn antigravity_turn_baseline(
    configured_storage: Option<&Path>,
    session_id: &str,
) -> Result<AntigravityTurnBaseline, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    crate::backend::subprocess::spawn_blocking(move || {
        if !db_path.is_file() {
            return Ok(AntigravityTurnBaseline::default());
        }
        let snap = read_continuation_snapshot_sync(&db_path, 0)?;
        Ok(AntigravityTurnBaseline {
            system_messages: snap.system_messages,
            planner_responses: snap.planner_responses,
            terminal_responses: snap.terminal_responses,
        })
    })
    .await
    .map_err(|error| format!("Antigravity continuation snapshot task failed: {error}"))?
}

/// Watches `agy`'s trajectory SQLite store via `notify` until a reactive-wakeup
/// continuation settles: no `RUNNING` steps remain, at least
/// `required_system_messages` `SYSTEM_MESSAGE` steps are recorded, and the
/// latest non-checkpoint step is a completed `PLANNER_RESPONSE` with no
/// outgoing tool calls.
pub(crate) async fn watch_antigravity_continuation_settled(
    configured_storage: Option<&Path>,
    session_id: &str,
    required_system_messages: usize,
    baseline_planner_responses: usize,
) -> Result<AntigravityContinuationSettled, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    let db_for_watch = db_path.clone();
    watch_antigravity_paths(vec![db_path], move || {
        if !db_for_watch.is_file() {
            return Ok(None);
        }
        let snap = read_continuation_snapshot_sync(&db_for_watch, baseline_planner_responses)?;
        let settled = !snap.has_running_step
            && snap.system_messages >= required_system_messages
            && snap.latest_step_is_terminal_response;
        Ok(settled.then_some(AntigravityContinuationSettled {
            expected_turn_text_non_ws: snap.expected_turn_text_non_ws,
        }))
    })
    .await
}

#[derive(Default)]
struct AntigravityContinuationSnapshot {
    system_messages: usize,
    planner_responses: usize,
    terminal_responses: usize,
    expected_turn_text_non_ws: usize,
    has_running_step: bool,
    latest_step_is_terminal_response: bool,
}

fn read_continuation_snapshot_sync(
    db_path: &Path,
    baseline_planner_responses: usize,
) -> Result<AntigravityContinuationSnapshot, String> {
    use prost::Message;
    let connection = open_antigravity_db_readonly(db_path)?;
    let mut query = connection
        .prepare("SELECT step_format, step_payload FROM steps ORDER BY idx")
        .map_err(|error| format!("Cannot prepare Antigravity continuation query: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("Cannot query Antigravity continuation steps: {error}"))?;
    let mut snapshot = AntigravityContinuationSnapshot::default();
    for row in rows {
        let (format, payload) =
            row.map_err(|error| format!("Cannot read Antigravity continuation row: {error}"))?;
        if format != 0 {
            continue;
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|error| format!("Cannot decode Antigravity continuation step: {error}"))?;
        if step.status == 2 {
            snapshot.has_running_step = true;
        }
        if step.kind == 101 {
            snapshot.system_messages += 1;
            snapshot.latest_step_is_terminal_response = false;
        } else if step.kind == 15 {
            let ordinal = snapshot.planner_responses;
            snapshot.planner_responses += 1;
            if matches!(step.status, 3 | 6 | 7 | 12) {
                snapshot.terminal_responses += 1;
            }
            if step.status == 3 {
                if ordinal >= baseline_planner_responses
                    && let Some(response) = step.response.as_ref()
                {
                    snapshot.expected_turn_text_non_ws =
                        snapshot.expected_turn_text_non_ws.saturating_add(
                            response
                                .text
                                .chars()
                                .filter(|ch| !ch.is_whitespace())
                                .count(),
                        );
                }
                snapshot.latest_step_is_terminal_response = step
                    .response
                    .as_ref()
                    .is_some_and(|response| response.tools.is_empty());
            } else {
                snapshot.latest_step_is_terminal_response = false;
            }
        } else if step.kind != 102 {
            snapshot.latest_step_is_terminal_response = false;
        }
    }
    Ok(snapshot)
}

fn find_tool_step_in_db(
    db_path: &Path,
    tool_call_id: &str,
) -> Result<Option<(u64, NativeAgyStep)>, String> {
    use prost::Message;
    let connection = open_antigravity_db_readonly(db_path)?;
    let mut query = connection
        .prepare("SELECT idx, step_format, step_payload FROM steps ORDER BY idx")
        .map_err(|error| format!("Cannot prepare Antigravity tool step query: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(|error| format!("Cannot query Antigravity tool steps: {error}"))?;
    for row in rows {
        let (idx, format, payload) =
            row.map_err(|error| format!("Cannot read Antigravity tool step row: {error}"))?;
        if format != 0 || idx < 0 {
            continue;
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|error| format!("Cannot decode Antigravity tool step protobuf: {error}"))?;
        if step
            .metadata
            .as_ref()
            .and_then(|meta| meta.tool_call.as_ref())
            .is_some_and(|call| call.id == tool_call_id)
        {
            return Ok(Some((idx as u64, step)));
        }
    }
    Ok(None)
}

fn read_transcript_step(
    brain_dir: &Path,
    session_id: &str,
    step_index: u64,
) -> Result<Option<Value>, String> {
    let path = brain_dir
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "Cannot read Antigravity transcript {}: {error}",
            path.display()
        )
    })?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row = serde_json::from_str::<Value>(trimmed).map_err(|error| {
            format!(
                "Invalid JSON line in Antigravity transcript {}: {error}",
                path.display()
            )
        })?;
        if row.get("step_index").and_then(Value::as_u64) == Some(step_index) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

fn strip_mcp_transcript_wrappers(content: &str) -> String {
    let lines: Vec<&str> = content
        .lines()
        .skip_while(|line| line.starts_with("Created At: ") || line.starts_with("Completed At: "))
        .filter(|line| !(line.starts_with("[Resource offloaded to file://") && line.ends_with(']')))
        .collect();
    lines.join("\n")
}

/// Recovers canonical MCP tool results (original server text, error text, and
/// offloaded image bytes) for an `agy acp` tool call, watching SQLite WAL and
/// `transcript_full.jsonl` via `notify` until the completed step is flushed.
pub(crate) async fn antigravity_mcp_tool_result(
    configured_storage: Option<&Path>,
    session_id: &str,
    tool_call_id: &str,
) -> Result<Value, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let brain_dir = antigravity_brain_dir(&conversations_dir);
    let db_path =
        antigravity_conversation_db_path(&SessionId(session_id.to_owned()), &conversations_dir);
    let transcript_path = brain_dir
        .join(session_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    let sid = session_id.to_owned();
    let tid = tool_call_id.to_owned();
    let db_for_watch = db_path.clone();

    watch_antigravity_paths(vec![db_path, transcript_path], move || {
        if !db_for_watch.is_file() {
            return Ok(None);
        }
        let Some((step_index, step)) = find_tool_step_in_db(&db_for_watch, &tid)? else {
            return Ok(None);
        };
        if !matches!(step.status, 3 | 6 | 7 | 12) {
            return Ok(None);
        }
        let Some(row) = read_transcript_step(&brain_dir, &sid, step_index)? else {
            return Ok(None);
        };
        let is_error =
            step.status == 7 || row.get("status").and_then(Value::as_str) == Some("ERROR");

        let mut content = Vec::new();
        let raw_text = if is_error {
            row.get("error")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!(
                        "Antigravity MCP error step {step_index} for tool call {tid} omitted transcript error text"
                    )
                })?
        } else {
            let text = row.get("content").and_then(Value::as_str).ok_or_else(|| {
                format!(
                    "Antigravity MCP step {step_index} for tool call {tid} omitted transcript content"
                )
            })?;
            strip_mcp_transcript_wrappers(text)
        };
        if !raw_text.is_empty() {
            content.push(json!({ "type": "text", "text": raw_text }));
        }
        if let Some(media) = row.get("media").and_then(Value::as_array) {
            for item in media {
                let Some(mime) = item.get("mime_type").and_then(Value::as_str) else {
                    continue;
                };
                if !mime.starts_with("image/") {
                    continue;
                }
                let Some(uri) = item.get("uri").and_then(Value::as_str) else {
                    return Err("Antigravity MCP media entry omitted its file URI".to_owned());
                };
                let parsed = url::Url::parse(uri).map_err(|error| {
                    format!("Invalid Antigravity MCP media URI {uri:?}: {error}")
                })?;
                let media_path = parsed.to_file_path().map_err(|()| {
                    format!("Antigravity MCP media URI is not a local file URI: {uri:?}")
                })?;
                let bytes = fs::read(&media_path).map_err(|error| {
                    format!(
                        "Cannot read Antigravity MCP image {}: {error}",
                        media_path.display()
                    )
                })?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                content.push(json!({
                    "type": "image",
                    "mimeType": mime,
                    "data": encoded,
                }));
            }
        }
        Ok(Some(json!({
            "content": content,
            "isError": is_error,
        })))
    })
    .await
}

enum NativeAgyChildCompletion {
    Completed(Vec<ChatEvent>),
    Failed {
        events: Vec<ChatEvent>,
        reason: NativeAgyTerminationReason,
    },
}

fn extract_child_conversation_id(text: &str) -> Result<String, String> {
    let start = text
        .find('{')
        .ok_or_else(|| "Antigravity invoke_subagent result contained no JSON payload".to_owned())?;
    let end = text
        .rfind('}')
        .filter(|&end| end >= start)
        .ok_or_else(|| "Antigravity invoke_subagent result had unterminated JSON".to_owned())?;
    let parsed: Value = serde_json::from_str(&text[start..=end])
        .map_err(|error| format!("Invalid JSON in Antigravity invoke_subagent result: {error}"))?;
    parsed
        .get("conversationId")
        .or_else(|| parsed.get("conversation_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Antigravity invoke_subagent result omitted conversationId".to_owned())
}

/// Reads the child session ID and display name from a completed
/// `invoke_subagent` step in the parent's trajectory SQLite store. Never waits
/// on the child's execution lifetime. Returns `Ok(None)` only for
/// `image-generator` invocations or non-succeeded steps.
pub(crate) async fn antigravity_native_child_descriptor(
    configured_storage: Option<&Path>,
    parent_session_id: &str,
    tool_call_id: &str,
) -> Result<Option<(SessionId, String)>, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let parent_db = antigravity_conversation_db_path(
        &SessionId(parent_session_id.to_owned()),
        &conversations_dir,
    );
    let tid = tool_call_id.to_owned();
    let db_for_watch = parent_db.clone();
    watch_antigravity_paths(vec![parent_db], move || {
        if !db_for_watch.is_file() {
            return Ok(None);
        }
        let Some((_, step)) = find_tool_step_in_db(&db_for_watch, &tid)? else {
            return Ok(None);
        };
        if !matches!(step.status, 3 | 6 | 7 | 12) {
            return Ok(None);
        }
        if step.status != 3 {
            return Ok(Some(None));
        }
        let raw_args = step
            .metadata
            .as_ref()
            .and_then(|meta| meta.tool_call.as_ref())
            .map(|call| call.arguments.as_str())
            .ok_or_else(|| {
                format!("Antigravity invoke_subagent step {tid} omitted tool_call arguments")
            })?;
        let parsed_args = serde_json::from_str::<Value>(raw_args).map_err(|error| {
            format!("Invalid JSON arguments for Antigravity invoke_subagent {tid}: {error}")
        })?;
        if parsed_args.get("TypeName").and_then(Value::as_str) == Some("image-generator") {
            return Ok(Some(None));
        }
        let text = step
            .generic
            .and_then(|g| g.result)
            .map(|r| r.text)
            .ok_or_else(|| {
                format!("Antigravity invoke_subagent step {tid} completed without result text")
            })?;
        let child_id = extract_child_conversation_id(&text)?;
        let name = parsed_args
            .get("Role")
            .or_else(|| parsed_args.get("TypeName"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                format!("Antigravity invoke_subagent step {tid} omitted Role and TypeName")
            })?;
        Ok(Some(Some((SessionId(child_id), name))))
    })
    .await
}

/// Watches an Antigravity native child trajectory DB until its executor reaches
/// a terminal state and returns its converted transcript events. Designed to
/// run in an async relay task outside the parent session's inbound loop.
pub(crate) async fn antigravity_wait_native_child_events(
    configured_storage: Option<&Path>,
    child_session_id: &SessionId,
) -> Result<Vec<ChatEvent>, String> {
    let conversations_dir = resolve_antigravity_conversations_dir(configured_storage)?;
    let child_db = antigravity_conversation_db_path(child_session_id, &conversations_dir);
    let events = match wait_for_antigravity_native_child(&child_db).await? {
        NativeAgyChildCompletion::Completed(events) => events,
        NativeAgyChildCompletion::Failed { mut events, reason } => {
            events.push(ChatEvent::MessageAdded(protocol::ChatMessage {
                origin: None,
                message_id: None,
                timestamp: crate::agent::now_ms(),
                sender: protocol::MessageSender::Error,
                content: format!("Antigravity native child executor terminated: {reason:?}"),
                reasoning: None,
                tool_calls: Vec::new(),
                model_info: None,
                token_usage: None,
                context_breakdown: None,
                images: None,
            }));
            events
        }
    };
    Ok(events)
}

async fn wait_for_antigravity_native_child(
    path: &Path,
) -> Result<NativeAgyChildCompletion, String> {
    use notify::Watcher;
    let (updates, mut changed) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = updates.send(event);
    })
    .map_err(|error| format!("Cannot observe native child completion: {error}"))?;
    let directory = path
        .parent()
        .ok_or("Native child trajectory has no directory")?;
    watcher
        .watch(directory, notify::RecursiveMode::NonRecursive)
        .map_err(|error| format!("Cannot subscribe to native child completion: {error}"))?;
    let wal = path.with_extension("db-wal");
    loop {
        if path.is_file()
            && let Some(events) = antigravity_native_child_history(path)?
        {
            return Ok(events);
        }
        loop {
            let event: notify::Result<notify::Event> = changed
                .recv()
                .await
                .ok_or("Native child completion observer closed")?;
            let event = event
                .map_err(|error| format!("Native child completion observation failed: {error}"))?;
            if matches!(
                event.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            ) && event
                .paths
                .iter()
                .any(|entry| entry == path || entry == &wal)
            {
                break;
            }
        }
    }
}

fn antigravity_native_child_history(
    path: &Path,
) -> Result<Option<NativeAgyChildCompletion>, String> {
    use prost::Message;
    use protocol::{
        ChatMessage, ChatMessageId, MessageSender, ReasoningData, ToolExecutionCompletedData,
        ToolRequest,
    };
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "Cannot open Antigravity native child trajectory".to_owned())?;
    use rusqlite::OptionalExtension;
    let executor = connection
        .query_row(
            "SELECT data FROM executor_metadata ORDER BY idx DESC LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|error| format!("Cannot read native child execution status: {error}"))?;
    let Some(executor) = executor else {
        return Ok(None);
    };
    let executor = NativeAgyExecutor::decode(executor.as_slice())
        .map_err(|_| "Invalid native child execution status".to_owned())?;
    let reason = NativeAgyTerminationReason::try_from(executor.termination_reason)
        .map_err(|_| "Unsupported Antigravity native child termination reason".to_owned())?;
    if executor.proceeded_with_auto_continue {
        return Ok(None);
    }
    let failed = match reason {
        NativeAgyTerminationReason::Unspecified | NativeAgyTerminationReason::EarlyContinue => {
            return Ok(None);
        }
        NativeAgyTerminationReason::Error
        | NativeAgyTerminationReason::UserCanceled
        | NativeAgyTerminationReason::MaxInvocations
        | NativeAgyTerminationReason::MaxForcedInvocations
        | NativeAgyTerminationReason::MaxTokenBudgetExceeded => true,
        NativeAgyTerminationReason::NoToolCall
        | NativeAgyTerminationReason::TerminalStepType
        | NativeAgyTerminationReason::TerminalCustomHook
        | NativeAgyTerminationReason::InjectedResponse => false,
    };
    let mut query = connection
        .prepare("SELECT step_format, step_payload FROM steps ORDER BY idx")
        .map_err(|error| format!("Cannot read native child steps: {error}"))?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("Cannot enumerate native child steps: {error}"))?;
    let mut events = Vec::new();
    let mut pending = BTreeMap::new();
    for row in rows {
        let (format, payload) =
            row.map_err(|error| format!("Cannot read native child step: {error}"))?;
        if format != 0 {
            return Err("Unsupported Antigravity native child step encoding".to_owned());
        }
        let step = NativeAgyStep::decode(payload.as_slice())
            .map_err(|_| "Invalid Antigravity native child step".to_owned())?;
        if matches!(step.kind, 14 | 101 | 102) {
            continue;
        }
        let metadata = step.metadata.ok_or("Native child step omitted metadata")?;
        match step.kind {
            15 => {
                if step.status != 3 {
                    return Err("Native child response was not completed".to_owned());
                }
                let response = step
                    .response
                    .ok_or("Native child response omitted payload")?;
                if response.message_id.is_empty() {
                    return Err("Native child response omitted identity".to_owned());
                }
                let mut declarations = Vec::new();
                let mut requests = Vec::new();
                for call in response.tools {
                    if call.id.is_empty() || call.name.is_empty() {
                        return Err("Native child tool omitted identity".to_owned());
                    }
                    let args: Value = serde_json::from_str(&call.arguments)
                        .map_err(|_| "Native child tool contained invalid arguments".to_owned())?;
                    let tool_type = tool_request_type(&call.name, &args, true);
                    let is_mcp = call.name == MCP_DISPATCH_TOOL;
                    let (name, args) = if is_mcp {
                        mcp_inner_call(&args)
                    } else {
                        (call.name, args)
                    };
                    if pending
                        .insert(call.id.clone(), (tool_type.clone(), is_mcp))
                        .is_some()
                    {
                        return Err("Native child repeated a tool identity".to_owned());
                    }
                    declarations.push(ToolUseData {
                        tool_call_id: call.id.clone(),
                        name: name.clone(),
                        arguments: args,
                        content_offset: None,
                    });
                    requests.push(ChatEvent::ToolRequest(ToolRequest {
                        tool_call_id: call.id,
                        tool_name: name,
                        tool_type,
                    }));
                }
                let message = ChatMessage {
                    origin: None,
                    message_id: Some(ChatMessageId(response.message_id)),
                    timestamp: metadata
                        .created_at
                        .map(|stamp| {
                            (stamp.seconds.max(0) as u64)
                                .saturating_mul(1000)
                                .saturating_add(stamp.nanos.max(0) as u64 / 1_000_000)
                        })
                        .unwrap_or_default(),
                    sender: MessageSender::Assistant {
                        agent: ANTIGRAVITY_AGENT_NAME.to_owned(),
                    },
                    content: response.text,
                    reasoning: (!response.thinking.is_empty()).then_some(ReasoningData {
                        text: response.thinking,
                        tokens: metadata.usage.as_ref().map(|usage| usage.thinking),
                        signature: None,
                        blob: None,
                    }),
                    tool_calls: declarations,
                    model_info: None,
                    token_usage: metadata.usage.map(|usage| {
                        MessageTokenUsage::request_known(native_usage_to_protocol(&usage))
                    }),
                    context_breakdown: None,
                    images: None,
                };
                events.push(ChatEvent::StreamStart(protocol::StreamStartData {
                    agent: ANTIGRAVITY_AGENT_NAME.to_owned(),
                    model: None,
                }));
                events.push(ChatEvent::StreamEnd(protocol::StreamEndData { message }));
                events.extend(requests);
            }
            132 => {
                let call = metadata
                    .tool_call
                    .ok_or("Native child tool result omitted identity")?;
                let (tool_type, is_mcp) = pending
                    .remove(&call.id)
                    .ok_or("Native child result has no declared tool")?;
                let result = step
                    .generic
                    .and_then(|generic| generic.result)
                    .ok_or("Native child tool result omitted payload")?;
                let outcome = match step.status {
                    3 => ToolExecutionOutcome::Succeeded {
                        result: child_tool_execution_result(&tool_type, &result.text, is_mcp),
                    },
                    7 => ToolExecutionOutcome::Failed {
                        message: result.text,
                        details: None,
                        normalization_failure: None,
                        result: None,
                    },
                    6 | 12 => ToolExecutionOutcome::Cancelled {
                        message: result.text,
                    },
                    _ => return Err("Native child tool has no terminal status".to_owned()),
                };
                events.push(ChatEvent::ToolExecutionCompleted(
                    ToolExecutionCompletedData {
                        tool_call_id: call.id,
                        outcome,
                    },
                ));
            }
            _ => {
                return Err(format!(
                    "Unsupported Antigravity native child step kind {}",
                    step.kind
                ));
            }
        }
    }
    if failed {
        for (tool_call_id, _) in pending {
            let message = format!("Native child executor terminated: {reason:?}");
            events.push(ChatEvent::ToolExecutionCompleted(
                ToolExecutionCompletedData {
                    tool_call_id,
                    outcome: if reason == NativeAgyTerminationReason::UserCanceled {
                        ToolExecutionOutcome::Cancelled { message }
                    } else {
                        ToolExecutionOutcome::Failed {
                            message,
                            details: None,
                            normalization_failure: None,
                            result: None,
                        }
                    },
                },
            ));
        }
        return Ok(Some(NativeAgyChildCompletion::Failed { events, reason }));
    }
    if !pending.is_empty() || events.is_empty() {
        return Err("Native child trajectory has no complete public response".to_owned());
    }
    events.push(ChatEvent::TypingStatusChanged(false));
    Ok(Some(NativeAgyChildCompletion::Completed(events)))
}

fn child_tool_execution_result(
    tool_type: &ToolRequestType,
    output: &str,
    is_mcp_dispatch: bool,
) -> ToolExecutionResult {
    if is_mcp_dispatch {
        return ToolExecutionResult::Other {
            result: Value::String(output.to_string()),
        };
    }
    match tool_type {
        ToolRequestType::ModifyFile { before, after, .. } => {
            let (lines_added, lines_removed) = crate::backend::estimate_line_delta(before, after);
            ToolExecutionResult::ModifyFile {
                lines_added,
                lines_removed,
            }
        }
        ToolRequestType::RunCommand { .. } => match exit_code_from_result(output) {
            Some(exit_code) => ToolExecutionResult::RunCommand {
                exit_code,
                stdout: output.to_string(),
                stderr: String::new(),
            },
            None => ToolExecutionResult::Other {
                result: json!({ "output": output }),
            },
        },
        ToolRequestType::ReadFiles { file_paths } => ToolExecutionResult::ReadFiles {
            files: file_paths
                .iter()
                .map(|path| protocol::FileInfo {
                    path: path.clone(),
                    bytes: None,
                })
                .collect(),
        },
        ToolRequestType::WebSearch { .. } => ToolExecutionResult::WebSearch,
        ToolRequestType::Sleep { .. } => ToolExecutionResult::Sleep,
        ToolRequestType::GenerateImage { .. } => ToolExecutionResult::GenerateImage {
            revised_prompt: None,
            image_count: 1,
        },
        _ => ToolExecutionResult::Other {
            result: json!({ "output": output }),
        },
    }
}

// ── Paths, settings, and Backend trait impl ────────────────────────────────

pub(crate) fn resolve_workspace_roots(
    workspace_roots: &[String],
) -> Result<(String, Vec<String>), String> {
    if workspace_roots.iter().all(|root| root.trim().is_empty()) {
        let no_root_cwd = antigravity_no_root_cwd()?;
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, &no_root_cwd)
    } else {
        resolve_workspace_roots_with_no_root_cwd(workspace_roots, Path::new(""))
    }
}

fn resolve_workspace_roots_with_no_root_cwd(
    workspace_roots: &[String],
    no_root_cwd: &Path,
) -> Result<(String, Vec<String>), String> {
    crate::paths::reject_ssh_workspace_roots(workspace_roots)?;
    let mut roots = workspace_roots
        .iter()
        .map(|root| root.trim())
        .filter(|root| !root.is_empty())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        fs::create_dir_all(no_root_cwd).map_err(|err| {
            format!(
                "Failed to create Antigravity no-root working directory {}: {err}",
                no_root_cwd.display()
            )
        })?;
        return Ok((no_root_cwd.to_string_lossy().to_string(), Vec::new()));
    }
    let primary = roots
        .first()
        .expect("empty roots returned above")
        .to_string();
    if !Path::new(&primary).is_dir() {
        return Err(format!(
            "Antigravity primary workspace root is not a directory: {primary}"
        ));
    }
    let extra = roots.drain(1..).map(str::to_string).collect::<Vec<_>>();
    Ok((primary, extra))
}

fn antigravity_no_root_cwd() -> Result<PathBuf, String> {
    Ok(crate::paths::home_dir()?
        .join(".tyde")
        .join("antigravity")
        .join("no-root"))
}

pub(crate) fn is_antigravity_native_session_id(session_id: &SessionId) -> bool {
    session_id.0.len() == 36 && Uuid::parse_str(&session_id.0).is_ok()
}

pub(crate) fn is_antigravity_session_resumable(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> bool {
    is_antigravity_native_session_id(session_id)
        && antigravity_conversation_db_path(session_id, conversations_dir).is_file()
}

fn ensure_antigravity_conversation_exists(
    session_id: &SessionId,
    conversations_dir: &Path,
) -> Result<(), String> {
    let path = antigravity_conversation_db_path(session_id, conversations_dir);
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "Antigravity conversation {session_id} does not exist at {}; refusing to resume without an exact native agy conversation",
            path.display()
        ))
    }
}

fn antigravity_conversation_db_path(session_id: &SessionId, conversations_dir: &Path) -> PathBuf {
    conversations_dir.join(format!("{}.db", session_id.0))
}

pub(crate) fn resolve_antigravity_conversations_dir(
    configured_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    match configured_dir {
        Some(path) => Ok(path.to_path_buf()),
        None => Ok(crate::paths::execution_home_dir()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")),
    }
}

fn antigravity_brain_dir(conversations_dir: &Path) -> PathBuf {
    match conversations_dir.parent() {
        Some(parent) => parent.join("brain"),
        None => conversations_dir.join("brain"),
    }
}

pub(crate) fn session_settings_schema() -> SessionSettingsSchema {
    SessionSettingsSchema {
        model_resolutions: Default::default(),
        backend_kind: BackendKind::Antigravity,
        fields: vec![
            SessionSettingField {
                key: "model".to_owned(),
                label: "Model".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: Vec::new(),
                    default: None,
                    nullable: true,
                },
            },
            SessionSettingField {
                key: "reasoning_effort".to_owned(),
                label: "Thinking Level".to_owned(),
                description: None,
                use_slider: true,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: Vec::new(),
                    default: None,
                    nullable: true,
                },
            },
            SessionSettingField {
                key: "mode".to_owned(),
                label: "Mode".to_owned(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: Vec::new(),
                    default: None,
                    nullable: true,
                },
            },
        ],
    }
}

pub(crate) fn antigravity_cost_hint_defaults(_cost_hint: SpawnCostHint) -> SessionSettingsValues {
    // Do not inject `reasoning_effort` unconditionally: not every Antigravity
    // model exposes a configurable `thought_level`, and each model advertises
    // its own supported thinking levels in `session/new`.
    SessionSettingsValues::default()
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    resolve_backend_settings(
        config,
        &session_settings_schema(),
        antigravity_cost_hint_defaults,
    )
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as u64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AntigravityBackend(crate::backend::acp::backend::KiroBackend);

impl Backend for AntigravityBackend {
    async fn relocate_stopped_session(
        session_id: &SessionId,
        relocation: &crate::backend::BackendSessionRelocation,
    ) -> Result<(), String> {
        let source_roots = relocation
            .source
            .scope
            .run_sync(|| crate::backend::execution_workspace_roots(&relocation.source.roots))?;
        let destination_roots = relocation.destination.scope.run_sync(|| {
            crate::backend::execution_workspace_roots(&relocation.destination.roots)
        })?;
        let Some(previous) = source_roots.first().cloned() else {
            return Ok(());
        };
        if source_roots == destination_roots {
            return Ok(());
        }
        let session_id = session_id.clone();
        relocation
            .destination
            .scope
            .run(async move {
                let conversations_dir = resolve_antigravity_conversations_dir(None)?;
                let db_path = antigravity_conversation_db_path(&session_id, &conversations_dir);
                crate::backend::subprocess::spawn_blocking(move || {
                    relocate_antigravity_metadata(&db_path, &previous, &destination_roots)
                        .map(|_| ())
                })
                .await
                .map_err(|error| format!("Antigravity relocation task failed: {error}"))?
            })
            .await
    }

    fn workspace_relocation_wake(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        self.0.workspace_relocation_wake()
    }

    fn native_session_id_is_valid(session_id: &SessionId) -> bool {
        is_antigravity_native_session_id(session_id)
    }

    fn resolve_storage_root(configured: Option<&Path>) -> Result<Option<PathBuf>, String> {
        resolve_antigravity_conversations_dir(configured).map(Some)
    }

    fn stored_session_is_resumable(
        session_id: &SessionId,
        stored_resumable: bool,
        has_parent: bool,
        superseded: bool,
        storage: &crate::backend::BackendStorage,
    ) -> bool {
        if superseded || (!stored_resumable && has_parent) {
            return false;
        }
        resolve_antigravity_conversations_dir(storage.root(BackendKind::Antigravity))
            .is_ok_and(|root| is_antigravity_session_resumable(session_id, &root))
    }

    fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
        resolve_session_settings(config)
    }

    fn startup_drop_cancels_workers() -> bool {
        false
    }

    fn session_is_resumable(
        session_id: &SessionId,
        _workspace_roots: &[String],
        _resolved: &crate::backend::customization::ResolvedSpawnConfig,
    ) -> bool {
        is_antigravity_native_session_id(session_id)
    }

    fn skill_delivery() -> crate::backend::customization::SkillDelivery {
        crate::backend::customization::SkillDelivery::NativeDiscovery
    }

    fn builtin_tier_config() -> settings_model::BackendTierConfig {
        settings_model::BackendTierConfig {
            low: antigravity_cost_hint_defaults(SpawnCostHint::Low),
            high: antigravity_cost_hint_defaults(SpawnCostHint::High),
        }
    }

    async fn read_capacity_out_of_band(
        _context: &crate::backend::BackendProbeContext,
    ) -> protocol::BackendCapacityState {
        read_capacity_out_of_band().await
    }

    fn validate_tool_categories(categories: &[protocol::ToolCategory]) -> Result<(), String> {
        for category in categories {
            match category {
                protocol::ToolCategory::AskUser => {}
                protocol::ToolCategory::AgentDelegation => {
                    return Err("Backend cannot enforce agent-delegation exclusion".into());
                }
            }
        }
        Ok(())
    }

    /// Capabilities verified against `agy acp`.
    ///
    /// `AsyncUserQuestionRequests` is intentionally omitted: unlike the legacy
    /// headless `stream-json` driver (which auto-skipped `ask_question` and kept
    /// running tools in the same turn), `agy acp` pauses the turn on
    /// `ask_question` (`stopReason: "end_turn"`) until the user replies in a
    /// follow-up turn.
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        use tyde_agent_adapter::BackendCapability as Cap;
        [
            Cap::ListSessions,
            Cap::ResumeSession,
            Cap::SetWorkspaceRoots,
            Cap::RestartInWorkspace,
            Cap::SetMultipleWorkspaceRoots,
            Cap::ImageInput,
            Cap::Interrupt,
            Cap::SessionSettings,
            Cap::StartupMcpServers,
            Cap::McpImageResults,
            Cap::McpOriginalContentFidelity,
            Cap::McpErrorResults,
            Cap::AgentControlTools,
            Cap::CapacityTelemetry,
            Cap::OutOfBandCapacity,
            Cap::WorkspaceInstructions,
            Cap::Customization,
            Cap::UserQuestionRequests,
            Cap::TurnUsageReported,
            Cap::CumulativeUsageReported,
            Cap::ModelRequestUsageReported,
            Cap::ContextUsageReported,
            Cap::ReasoningDeltas,
            Cap::Subagents,
            Cap::ForegroundSubagents,
            Cap::GenericModifyFile,
            Cap::GenericReadFiles,
            Cap::GenericWebSearch,
            Cap::GenericGenerateImage,
            Cap::GenericViewImage,
            Cap::GenericSleep,
            Cap::GenericOtherTool,
            Cap::SlashCommands,
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        session_settings_schema()
    }

    fn has_dynamic_session_schema() -> bool {
        true
    }

    async fn discover_in_workspace(
        context: &crate::backend::BackendProbeContext,
    ) -> Result<BackendDiscovery, BackendDiscoveryError> {
        let mut spec = context.launch.clone().unwrap_or_else(agent_spec);
        if let Some(program) = &context.program {
            spec.command.clone_from(program);
        }
        let mut schema = crate::backend::acp::backend::probe_session_settings_schema(
            &context.workspace_roots,
            None,
            Some(&spec),
        )
        .await?;
        schema.backend_kind = BackendKind::Antigravity;
        if !schema.fields.iter().any(|field| field.key == "model") {
            return Err("Antigravity reported no selectable models"
                .to_owned()
                .into());
        }
        tracing::info!("Discovered Antigravity settings from its native ACP catalog");
        Ok(BackendDiscovery {
            schema,
            launch_profiles: Vec::new(),
        })
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        BackendCompactionCapability {
            coordinator: BackendCompactionCoordinator::ContextOperation,
            availability: BackendCompactionAvailability::AutomaticOnly {
                reason: BackendCompactionUnavailableReason::ManualTriggerAbsent,
            },
            provider_version: None,
            protocol_version: None,
            evidence: BackendCompactionCapabilityEvidence::AdapterContract,
        }
    }

    async fn begin_compaction(&self, _request: BackendCompactionRequest) -> BackendCompactionStart {
        BackendCompactionStart::NotDispatched {
            reason: BackendCompactionNotDispatchedReason::NativeUnavailable(
                BackendCompactionUnavailableReason::ManualTriggerAbsent,
            ),
        }
    }

    async fn spawn_in_workspace(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let (backend, events) = crate::backend::acp::backend::KiroBackend::spawn(
            workspace_roots,
            configure(config),
            initial_input,
        )
        .await?;
        Ok((Self(backend), events))
    }

    async fn resume_in_workspace(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let conversations_dir = resolve_antigravity_conversations_dir(
            config.backend_storage.root(BackendKind::Antigravity),
        )?;
        ensure_antigravity_conversation_exists(&session_id, &conversations_dir)?;
        let (backend, events) = crate::backend::acp::backend::KiroBackend::resume(
            workspace_roots,
            configure(config),
            session_id,
        )
        .await?;
        Ok((Self(backend), events))
    }

    async fn fork_in_workspace(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Antigravity),
        ))
    }

    async fn list_sessions_in_workspace(
        _context: &crate::backend::BackendProbeContext,
    ) -> Result<Vec<BackendSession>, String> {
        let conversations_dir = resolve_antigravity_conversations_dir(None)?;
        list_antigravity_sessions(
            &conversations_dir,
            &antigravity_brain_dir(&conversations_dir),
        )
    }

    fn session_id(&self) -> SessionId {
        self.0.session_id()
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        self.0.update_session_settings(payload).await
    }

    fn set_workspace_roots(
        &mut self,
        workspace_roots: Vec<String>,
    ) -> impl std::future::Future<Output = Result<(), super::WorkspaceRelocationError>> + Send + 'static
    {
        self.0.set_workspace_roots(workspace_roots)
    }

    async fn read_session_settings(&self) -> Result<SessionSettingsValues, String> {
        self.0.read_session_settings().await
    }

    #[cfg(feature = "test-support")]
    async fn native_process_id_for_test(&self) -> Result<u32, String> {
        self.0.native_process_id_for_test().await
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.0.send(input).await
    }

    async fn interrupt(&self) -> bool {
        self.0.interrupt().await
    }

    async fn shutdown(self) {
        self.0.shutdown().await;
    }
}
