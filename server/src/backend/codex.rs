use std::collections::{HashMap, HashSet, VecDeque, hash_map::Entry};
use std::ffi::OsString;
use std::io::{Read, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use indexmap::IndexMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use protocol::{
    AgentControlAgentRef, AgentControlProgress, AgentControlProgressKind, BackendAccessMode,
    CapacityBucket, CapacityBucketId, CapacityCoverage, CapacityMeasure, CapacityPlanLabel,
    CapacityReport, CapacityReset, CapacityScope, CapacitySource, CapacityUnavailableReason,
    CapacityWindow, ChatMessageId, CodexLimitSlot, CurrentContextUsage, ImageData,
    MessageMetadataUpdateData, MessageTokenUsage, ModelInfo, ModelRequestId,
    ModelRequestTokenUsage, ModelTurnId, ReasoningData, TokenUsage, TokenUsageScope,
    TokenUsageUnavailableReason, ToolExecutionMode, ToolExecutionNormalizationFailure,
    ToolExecutionOutcome, ToolExecutionResult, ToolProgressData, ToolProgressUpdate,
    ToolRequestType, ToolUseData, ValueProvenance,
};

use crate::agent::customization::{ResolvedSkill, SkillSelection};
use crate::agent_control_mcp::{
    AGENT_CONTROL_AWAIT_MCP_SERVER_NAME, AGENT_CONTROL_MCP_SERVER_NAME,
};
use crate::backend::agent_control_progress::{
    PendingToolNormalizationFailure, is_tyde_agent_control_await_tool_name,
    is_tyde_agent_control_send_message_tool_name, is_tyde_agent_control_spawn_tool_name,
    normalize_tyde_chat_event, tyde_tool_result,
};
use crate::backend::turn_emitter::{
    AgentName, ResponseHandle, RetryAttemptPayload, StreamEndPayload, TurnEmitter,
};
use crate::backend::{
    BackendExecutionMode, BackendStartupError, SessionCommand, StartupMcpServer,
    StartupMcpTransport, normalize_mcp_call_tool_result, render_combined_spawn_instructions,
};
use crate::process_env;
use crate::review_mcp::REVIEW_FEEDBACK_MCP_SERVER_NAME;
use crate::sub_agent::SubAgentEmitter;
use crate::subprocess::ImageAttachment;

const CODEX_AGENT_NAME: &str = "codex";
const CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT: u64 = 200_000;
// The entire GPT-5 family (gpt-5, gpt-5.x, their -codex and -mini variants)
// ships a 400k context window per OpenAI's model docs. `codex-mini-latest` is
// the lone exception at 200k. This is only a pre-first-turn fallback — once a
// turn reports `context_window` in token usage we use that instead.
const CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY: u64 = 400_000;
const CODEX_FORCED_APPROVAL_POLICY: &str = "never";
const CODEX_INFERENCE_APPROVAL_POLICY: &str = "untrusted";
const CODEX_UNRESTRICTED_SANDBOX: &str = "danger-full-access";
const CODEX_INFERENCE_SANDBOX: &str = "read-only";
const CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS: bool = true;
const CODEX_REASONING_SUMMARY_LEVEL: &str = "detailed";
const CODEX_MAX_GENERATED_IMAGE_BYTES: usize = 25 * 1024 * 1024;
/// How often a thread is asked which of its command executions are still
/// running as background terminals. A matched yielded-session raw result
/// classifies root work; polling then reconciles its liveness. The loop runs
/// only while a `commandExecution` item is outstanding, so an idle thread
/// issues no requests.
const CODEX_BACKGROUND_TERMINAL_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Consecutive polls a background terminal may be missing from
/// `thread/backgroundTerminals/list` before it is reported as stopped. The
/// slack lets the authoritative `item/completed` — which carries the exit
/// code — win the race when a process exits between polls.
const MAX_CODEX_RAW_NOTIFICATION_METHODS: usize = 32;
const MAX_CODEX_PROVIDER_SUPERSESSIONS_PER_TURN: u8 = 1;
const MAX_CODEX_PROVIDER_ITEM_TOMBSTONES: usize = 8;
const MAX_CODEX_TERMINATED_TURNS: usize = 8;
const MAX_CODEX_LATE_SUPERSEDED_EVENTS: u8 = 32;
const MAX_CODEX_LATE_SUPERSEDED_BYTES: usize = 64 * 1024;
const CODEX_SUPERSESSION_WARNING: &str = "Codex restarted part of its response mid-turn. \
The partial output above was kept and the turn continued.";
const CODEX_SKILLS_ROOT_PREFIX: &str = "tyde-codex-skills-";
const CODEX_ROLLOUT_TRACE_ROOT_PREFIX: &str = "tyde-codex-rollout-trace-";
const CODEX_ROLLOUT_TRACE_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CODEX_SKILL_MANIFEST_MAX_ENTRIES: usize = 100_000;
const CODEX_SKILL_MANIFEST_MAX_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(feature = "test-support")]
const CODEX_LEGACY_DYNAMIC_AWAIT_MARKER: &str = ".tyde-conformance-legacy-dynamic-await";
const CODEX_RAW_EVENTS_UNAVAILABLE_WARNING: &str = "This resumed or forked Codex thread cannot expose per-response raw boundaries with the installed app-server. Tyde is retaining the legacy tool-container projection for this thread; start a new Codex session for strict provider-response message identity.";

fn emit_codex_raw_events_warning_if_needed(emitter: &TurnEmitter, strict: bool) {
    if !strict {
        emitter.warning_message(CODEX_RAW_EVENTS_UNAVAILABLE_WARNING);
    }
}

fn codex_command() -> Command {
    Command::new("codex")
}

#[derive(Clone)]
pub struct CodexCommandHandle {
    inner: Arc<CodexInner>,
}

impl CodexCommandHandle {
    pub async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        self.inner.execute(command).await
    }

    async fn update_runtime_settings(&self, settings: Value) -> Result<(), String> {
        self.inner.update_runtime_settings(settings).await
    }

    async fn try_reserve_user_turn(&self) -> bool {
        let mut state = self.inner.state.lock().await;
        if state.active_turn_id.is_some()
            || state.awaiting_root_turn_start
            || state.background_wake_request_in_flight
        {
            return false;
        }
        state.awaiting_root_turn_start = true;
        true
    }

    async fn release_user_turn_reservation(&self) {
        let mut state = self.inner.state.lock().await;
        if state.active_turn_id.is_none() {
            state.awaiting_root_turn_start = false;
        }
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.inner
            .rpc
            .compaction_capability
            .lock()
            .expect("Codex compaction capability mutex poisoned")
            .clone()
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        self.inner.begin_compaction(request).await
    }
}

struct CodexSkillProjection {
    root: tempfile::TempDir,
}

impl CodexSkillProjection {
    fn new(skills: &[ResolvedSkill]) -> Result<Option<Self>, String> {
        if skills.is_empty() {
            return Ok(None);
        }

        let mut names = HashSet::new();
        for skill in skills {
            if !names.insert(skill.name.as_str()) {
                return Err(format!(
                    "Codex native skill selection contains duplicate name '{}'",
                    skill.name
                ));
            }
        }

        create_codex_skill_tempdir().map(|root| Some(Self { root }))
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn remove(self) {
        let path = self.path().to_path_buf();
        if let Err(err) = self.root.close() {
            tracing::warn!(
                "Failed to remove Codex session skill root {}: {err}",
                path.display()
            );
        }
    }
}

fn create_codex_skill_tempdir() -> Result<tempfile::TempDir, String> {
    let root = tempfile::Builder::new()
        .prefix(CODEX_SKILLS_ROOT_PREFIX)
        .tempdir()
        .map_err(|err| format!("Failed to create Codex session skill root: {err}"))?;
    restrict_codex_directory(root.path())?;
    Ok(root)
}

fn materialize_codex_skill(
    projection_root: &Path,
    skill: &ResolvedSkill,
    ordinal: usize,
) -> Result<PathBuf, String> {
    let wrapper_dir = projection_root.join(format!("skill-{ordinal:05}"));
    std::fs::create_dir(&wrapper_dir).map_err(|err| {
        format!(
            "Failed to create Codex wrapper for selected skill '{}': {err}",
            skill.name
        )
    })?;
    restrict_codex_directory(&wrapper_dir)?;

    let original = skill.load_body()?;
    let parsed = parse_codex_skill_md(&original)
        .map_err(|err| format!("Selected Codex skill '{}': {err}", skill.name))?;
    let wrapper_body = render_codex_skill_md(skill, parsed)
        .map_err(|err| format!("Selected Codex skill '{}': {err}", skill.name))?;
    let wrapper_skill_md = wrapper_dir.join("SKILL.md");
    std::fs::write(&wrapper_skill_md, wrapper_body).map_err(|err| {
        format!(
            "Failed to write Codex wrapper for selected skill '{}': {err}",
            skill.name
        )
    })?;
    restrict_codex_file(&wrapper_skill_md)?;

    let source_dir = std::fs::canonicalize(&skill.source_dir).map_err(|err| {
        format!(
            "Failed to inspect selected skill resources {}: {err}",
            skill.source_dir.display()
        )
    })?;
    for entry in std::fs::read_dir(&source_dir).map_err(|err| {
        format!(
            "Failed to read selected skill resources {}: {err}",
            source_dir.display()
        )
    })? {
        let entry = entry.map_err(|err| {
            format!(
                "Failed to read selected skill resources {}: {err}",
                source_dir.display()
            )
        })?;
        if entry.file_name() == std::ffi::OsStr::new("SKILL.md") {
            continue;
        }
        create_codex_resource_link(&entry.path(), &wrapper_dir.join(entry.file_name())).map_err(
            |err| {
                format!(
                    "Failed to link resource for selected Codex skill '{}': {err}",
                    skill.name
                )
            },
        )?;
    }

    std::fs::canonicalize(&wrapper_skill_md).map_err(|err| {
        format!(
            "Failed to inspect Codex wrapper for selected skill '{}': {err}",
            skill.name
        )
    })
}

fn discard_codex_skill_wrapper(projection_root: &Path, ordinal: usize) -> Result<(), String> {
    let wrapper_dir = projection_root.join(format!("skill-{ordinal:05}"));
    match std::fs::remove_dir_all(&wrapper_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!(
            "Failed to remove incomplete Codex skill wrapper {}: {err}",
            wrapper_dir.display()
        )),
    }
}

#[derive(Debug)]
struct ParsedCodexSkillMd<'a> {
    frontmatter: serde_yaml::Mapping,
    description: Option<String>,
    body: &'a str,
}

fn parse_codex_skill_md(raw: &str) -> Result<ParsedCodexSkillMd<'_>, String> {
    let detected = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = detected.split_inclusive('\n');
    let Some(opening) = lines.next() else {
        return Ok(ParsedCodexSkillMd {
            frontmatter: serde_yaml::Mapping::new(),
            description: None,
            body: raw,
        });
    };
    if codex_skill_line_contents(opening).trim() != "---" {
        return Ok(ParsedCodexSkillMd {
            frontmatter: serde_yaml::Mapping::new(),
            description: None,
            body: raw,
        });
    }

    let frontmatter_start = opening.len();
    let mut closing_start = frontmatter_start;
    let mut closing_end = None;
    for line in lines {
        let end = closing_start + line.len();
        if codex_skill_line_contents(line).trim() == "---" {
            closing_end = Some(end);
            break;
        }
        closing_start = end;
    }
    let Some(closing_end) = closing_end else {
        return Err("its SKILL.md opens a '---' frontmatter block that is never closed".to_owned());
    };

    let frontmatter = &detected[frontmatter_start..closing_start];
    let value: serde_yaml::Value = serde_yaml::from_str(frontmatter)
        .map_err(|err| format!("its SKILL.md frontmatter is not valid YAML: {err}"))?;
    let mapping = match value {
        serde_yaml::Value::Mapping(mapping) => mapping,
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        _ => return Err("its SKILL.md frontmatter is not a YAML mapping".to_owned()),
    };
    validate_codex_skill_frontmatter(&mapping)?;
    let description = mapping
        .get("description")
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                "its SKILL.md frontmatter field 'description' must be a YAML string".to_owned()
            })
        })
        .transpose()?;
    if description
        .as_deref()
        .is_some_and(|description| codex_single_line(description).is_empty())
    {
        return Err("its SKILL.md frontmatter field 'description' must not be empty".to_owned());
    }

    Ok(ParsedCodexSkillMd {
        frontmatter: mapping,
        description,
        body: &detected[closing_end..],
    })
}

fn codex_skill_line_contents(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn validate_codex_skill_frontmatter(mapping: &serde_yaml::Mapping) -> Result<(), String> {
    if mapping.keys().any(|key| {
        matches!(
            key,
            serde_yaml::Value::Sequence(_) | serde_yaml::Value::Mapping(_)
        )
    }) {
        return Err(
            "its SKILL.md frontmatter has a collection-valued key that Codex cannot load"
                .to_owned(),
        );
    }
    if let Some(metadata) = mapping.get("metadata") {
        let serde_yaml::Value::Mapping(metadata) = metadata else {
            return Err(
                "its SKILL.md frontmatter field 'metadata' must be a YAML mapping".to_owned(),
            );
        };
        if let Some(short_description) = metadata.get("short-description")
            && !matches!(
                short_description,
                serde_yaml::Value::String(_) | serde_yaml::Value::Null
            )
        {
            return Err(
                "its SKILL.md frontmatter field 'metadata.short-description' must be a YAML string"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn render_codex_skill_md(
    skill: &ResolvedSkill,
    mut parsed: ParsedCodexSkillMd<'_>,
) -> Result<String, String> {
    parsed.frontmatter.remove("name");
    parsed.frontmatter.insert(
        serde_yaml::Value::String("name".to_owned()),
        serde_yaml::Value::String(skill.name.clone()),
    );
    if parsed.description.is_none() {
        let description = skill
            .description
            .as_deref()
            .filter(|description| !codex_single_line(description).is_empty())
            .unwrap_or(&skill.name);
        parsed.frontmatter.insert(
            serde_yaml::Value::String("description".to_owned()),
            serde_yaml::Value::String(description.to_owned()),
        );
    }

    let frontmatter =
        canonicalize_codex_skill_yaml(serde_yaml::Value::Mapping(parsed.frontmatter))?;
    let mut rendered = serde_yaml::to_string(&frontmatter)
        .map_err(|err| format!("its SKILL.md frontmatter could not be serialized: {err}"))?;
    if rendered.lines().any(|line| line.trim() == "---") {
        return Err(
            "its SKILL.md frontmatter serialized an ambiguous Codex '---' delimiter".to_owned(),
        );
    }
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(format!("---\n{rendered}---\n{}", parsed.body))
}

fn canonicalize_codex_skill_yaml(value: serde_yaml::Value) -> Result<serde_yaml::Value, String> {
    match value {
        serde_yaml::Value::Sequence(values) => values
            .into_iter()
            .map(canonicalize_codex_skill_yaml)
            .collect::<Result<Vec<_>, _>>()
            .map(serde_yaml::Value::Sequence),
        serde_yaml::Value::Mapping(mapping) => {
            let mut entries = Vec::with_capacity(mapping.len());
            for (key, value) in mapping {
                let key = canonicalize_codex_skill_yaml(key)?;
                let value = canonicalize_codex_skill_yaml(value)?;
                let sort_key = serde_yaml::to_string(&key).map_err(|err| {
                    format!("its SKILL.md frontmatter key could not be serialized: {err}")
                })?;
                entries.push((sort_key, key, value));
            }
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_yaml::Mapping::with_capacity(entries.len());
            for (_, key, value) in entries {
                canonical.insert(key, value);
            }
            Ok(serde_yaml::Value::Mapping(canonical))
        }
        serde_yaml::Value::Tagged(mut tagged) => {
            tagged.value = canonicalize_codex_skill_yaml(tagged.value)?;
            Ok(serde_yaml::Value::Tagged(tagged))
        }
        scalar => Ok(scalar),
    }
}

fn codex_single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(unix)]
fn restrict_codex_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|err| {
        format!(
            "Failed to make Codex skill directory private {}: {err}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_codex_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn restrict_codex_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
        format!(
            "Failed to make Codex skill file private {}: {err}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn restrict_codex_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn create_codex_resource_link(source: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, link)
            .map_err(|err| format!("{} -> {}: {err}", link.display(), source.display()))
    }
    #[cfg(windows)]
    {
        let metadata = std::fs::metadata(source)
            .map_err(|err| format!("Failed to inspect {}: {err}", source.display()))?;
        let result = if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(source, link)
        } else {
            std::os::windows::fs::symlink_file(source, link)
        };
        result.map_err(|err| format!("{} -> {}: {err}", link.display(), source.display()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(format!(
            "{} -> {}: directory and file symlinks are unsupported on this platform",
            link.display(),
            source.display()
        ))
    }
}

#[derive(Clone, Debug)]
struct CodexListedSkill {
    name: String,
    locator: Option<String>,
    enabled: Option<bool>,
}

#[derive(Clone, Debug)]
struct CodexSkillCatalogError {
    name: Option<String>,
    locator: Option<String>,
    message: String,
}

impl CodexSkillCatalogError {
    fn display(&self) -> String {
        match self.locator.as_deref() {
            Some(locator) => format!("{locator}: {}", self.message),
            None => self.message.clone(),
        }
    }

    fn relates_to(&self, name: &str, expected_skill_md: Option<&Path>) -> bool {
        self.name.as_deref() == Some(name)
            || self.locator.as_deref().is_some_and(|locator| {
                expected_skill_md.is_some_and(|expected| Path::new(locator) == expected)
            })
    }
}

#[derive(Clone, Debug)]
struct CodexSkillCatalog {
    skills: Vec<CodexListedSkill>,
    errors: Vec<CodexSkillCatalogError>,
}

fn parse_codex_skill_catalog(response: &Value) -> Result<CodexSkillCatalog, String> {
    let entries = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Codex skills/list response missing data array")?;
    let mut skills = Vec::new();
    let mut errors = Vec::new();

    for entry in entries {
        let listed = entry
            .get("skills")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for skill in listed {
            let locator = skill.get("path").and_then(Value::as_str).map(str::to_owned);
            let Some(name) = skill.get("name").and_then(Value::as_str) else {
                errors.push(CodexSkillCatalogError {
                    name: locator.as_deref().and_then(codex_skill_name_from_locator),
                    locator,
                    message: "Codex skills/list entry omitted or malformed its name".to_owned(),
                });
                continue;
            };
            let enabled = skill.get("enabled").and_then(Value::as_bool);
            if enabled.is_none() {
                errors.push(CodexSkillCatalogError {
                    name: Some(name.to_owned()),
                    locator: locator.clone(),
                    message: format!(
                        "Codex skills/list entry '{name}' omitted or malformed its enabled state"
                    ),
                });
            }
            skills.push(CodexListedSkill {
                name: name.to_owned(),
                locator,
                enabled,
            });
        }
        if let Some(listed_errors) = entry.get("errors").and_then(Value::as_array) {
            for error in listed_errors {
                let path = error.get("path").and_then(Value::as_str).map(str::to_owned);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown discovery error");
                errors.push(CodexSkillCatalogError {
                    name: path.as_deref().and_then(codex_skill_name_from_locator),
                    locator: path,
                    message: message.to_owned(),
                });
            }
        }
    }

    Ok(CodexSkillCatalog { skills, errors })
}

fn codex_skill_name_from_locator(locator: &str) -> Option<String> {
    let path = Path::new(locator);
    if !path.is_absolute() {
        return None;
    }
    let parent = if path.file_name() == Some(std::ffi::OsStr::new("SKILL.md")) {
        path.parent()?
    } else {
        path
    };
    parent.file_name()?.to_str().map(str::to_owned)
}

fn codex_listed_filesystem_skill_md(skill: &CodexListedSkill) -> Result<Option<PathBuf>, String> {
    let Some(locator) = skill.locator.as_deref() else {
        return Ok(None);
    };
    let path = Path::new(locator);
    if !path.is_absolute() {
        return Ok(None);
    }
    let skill_md = std::fs::canonicalize(path).map_err(|err| {
        format!(
            "Failed to inspect native Codex skill '{}' at {}: {err}",
            skill.name,
            path.display()
        )
    })?;
    if !skill_md.is_file() {
        return Err(format!(
            "Native Codex skill '{}' path is not a regular file: {}",
            skill.name,
            skill_md.display()
        ));
    }
    Ok(Some(skill_md))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexSkillTreeManifest {
    digest: [u8; 32],
    entries: usize,
    bytes: u64,
}

struct CodexSkillTreeManifestBuilder {
    hasher: Sha256,
    entries: usize,
    bytes: u64,
}

impl CodexSkillTreeManifestBuilder {
    fn build(root: &Path) -> Result<(PathBuf, CodexSkillTreeManifest), String> {
        let root = std::fs::canonicalize(root)
            .map_err(|err| format!("Failed to inspect skill tree {}: {err}", root.display()))?;
        let mut builder = Self {
            hasher: Sha256::new(),
            entries: 1,
            bytes: 0,
        };
        builder.visit(&root, Path::new(""))?;
        let digest: [u8; 32] = builder.hasher.finalize().into();
        Ok((
            root,
            CodexSkillTreeManifest {
                digest,
                entries: builder.entries,
                bytes: builder.bytes,
            },
        ))
    }

    fn visit(&mut self, path: &Path, relative: &Path) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|err| format!("Failed to inspect skill entry {}: {err}", path.display()))?;
        codex_hash_relative_path(&mut self.hasher, relative);
        self.hasher
            .update(codex_skill_permission_bits(&metadata).to_le_bytes());
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            self.hasher.update(b"d");
            for name in self.sorted_entry_names(path)? {
                self.visit(&path.join(&name), &relative.join(name))?;
            }
            return Ok(());
        }
        if file_type.is_symlink() {
            self.hasher.update(b"l");
            let target = std::fs::read_link(path)
                .map_err(|err| format!("Failed to read skill link {}: {err}", path.display()))?;
            codex_hash_os_string(&mut self.hasher, target.as_os_str());
            return Ok(());
        }
        if !file_type.is_file() {
            return Err(format!(
                "Skill tree contains unsupported non-file entry {}",
                path.display()
            ));
        }

        self.hasher.update(b"f");
        let size = metadata.len();
        self.bytes = self.bytes.checked_add(size).ok_or_else(|| {
            format!(
                "Skill tree byte count overflowed while reading {}",
                path.display()
            )
        })?;
        if self.bytes > CODEX_SKILL_MANIFEST_MAX_BYTES {
            return Err(format!(
                "Skill tree {} exceeds the Codex collision-check limit of {} bytes",
                path.display(),
                CODEX_SKILL_MANIFEST_MAX_BYTES
            ));
        }
        self.hasher.update(size.to_le_bytes());
        let mut file = std::fs::File::open(path)
            .map_err(|err| format!("Failed to read skill file {}: {err}", path.display()))?;
        let mut buffer = [0u8; 64 * 1024];
        let mut read = 0u64;
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|err| format!("Failed to read skill file {}: {err}", path.display()))?;
            if count == 0 {
                break;
            }
            read = read.saturating_add(count as u64);
            if read > size {
                return Err(format!(
                    "Skill file changed while computing collision manifest: {}",
                    path.display()
                ));
            }
            self.hasher.update(&buffer[..count]);
        }
        if read != size {
            return Err(format!(
                "Skill file changed while computing collision manifest: {}",
                path.display()
            ));
        }
        Ok(())
    }

    fn sorted_entry_names(&mut self, path: &Path) -> Result<Vec<OsString>, String> {
        let remaining = CODEX_SKILL_MANIFEST_MAX_ENTRIES - self.entries;
        let names = codex_sorted_entry_names(path, remaining)?;
        self.entries += names.len();
        Ok(names)
    }
}

fn codex_sorted_entry_names(path: &Path, limit: usize) -> Result<Vec<OsString>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|err| format!("Failed to read skill directory {}: {err}", path.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        if names.len() == limit {
            return Err(format!(
                "Skill tree {} exceeds the Codex collision-check limit of {} entries",
                path.display(),
                CODEX_SKILL_MANIFEST_MAX_ENTRIES
            ));
        }
        names.push(
            entry
                .map_err(|err| format!("Failed to read skill directory {}: {err}", path.display()))?
                .file_name(),
        );
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn codex_skill_permission_bits(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn codex_skill_permission_bits(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

fn codex_hash_relative_path(hasher: &mut Sha256, path: &Path) {
    let components = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => Some(component),
            _ => None,
        })
        .collect::<Vec<_>>();
    hasher.update((components.len() as u64).to_le_bytes());
    for component in components {
        codex_hash_os_string(hasher, component);
    }
}

fn codex_hash_os_string(hasher: &mut Sha256, value: &std::ffi::OsStr) {
    let bytes = value.as_encoded_bytes();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

async fn codex_list_skills(rpc: &CodexRpc, cwd: &str) -> Result<CodexSkillCatalog, String> {
    let response = rpc
        .request(
            "skills/list",
            json!({
                "cwds": [cwd],
                "forceReload": true
            }),
        )
        .await?;
    parse_codex_skill_catalog(&response)
}

async fn initialize_codex_rpc(
    rpc: &CodexRpc,
    installed_provider_version: Option<&str>,
) -> Result<(), String> {
    let response = rpc
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": CODEX_APP_SERVER_CLIENT_NAME,
                    "title": Value::Null,
                    "version": "0.1"
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )
        .await?;
    let user_agent = response
        .get("userAgent")
        .or_else(|| response.get("user_agent"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let capability = codex_compaction_capability_with_installed_version(
        user_agent.as_deref(),
        installed_provider_version,
    );
    *rpc.compaction_capability
        .lock()
        .expect("Codex compaction capability mutex poisoned") = capability;
    Ok(())
}

const CODEX_APP_SERVER_CLIENT_NAME: &str = "tyde";

fn codex_version_from_user_agent(user_agent: &str) -> Result<Option<&str>, ()> {
    let prefix = format!("{CODEX_APP_SERVER_CLIENT_NAME}/");
    let mut matches = user_agent
        .split_whitespace()
        .filter_map(|part| part.strip_prefix(prefix.as_str()));
    let Some(version) = matches.next() else {
        return Ok(None);
    };
    if version.is_empty() || matches.next().is_some() {
        return Err(());
    }
    Ok(Some(version))
}

fn codex_compaction_capability_with_installed_version(
    user_agent: Option<&str>,
    installed_provider_version: Option<&str>,
) -> BackendCompactionCapability {
    let user_agent = user_agent.map(str::trim).filter(|value| !value.is_empty());
    let installed_provider_version = installed_provider_version
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let provider_version = user_agent
        .and_then(|value| codex_version_from_user_agent(value).ok().flatten())
        .map(str::to_owned)
        .or_else(|| installed_provider_version.map(str::to_owned));
    BackendCompactionCapability::native(
        BackendCompactionMechanism::JsonRpcRequest,
        provider_version,
        BackendCompactionCapabilityEvidence::CodexMethodProbe,
    )
}

#[derive(Default)]
struct CodexSkillSetup {
    exposed_names: Vec<String>,
    diagnostics: Vec<String>,
}

#[derive(Clone)]
enum CodexExpectedSkillVisibility {
    Native(PathBuf),
    Projected(PathBuf),
}

impl CodexExpectedSkillVisibility {
    fn skill_md(&self) -> &Path {
        match self {
            Self::Native(path) | Self::Projected(path) => path,
        }
    }

    fn is_projected(&self) -> bool {
        matches!(self, Self::Projected(_))
    }
}

#[derive(Clone)]
struct CodexPreparedSkill {
    ordinal: usize,
    skill: ResolvedSkill,
    visibility: CodexExpectedSkillVisibility,
}

struct CodexSkillPreparation {
    prepared: Vec<CodexPreparedSkill>,
    diagnostics: Vec<String>,
}

fn prepare_codex_selected_skill(
    selected: &ResolvedSkill,
    ordinal: usize,
    baseline: &CodexSkillCatalog,
    projection_root: &Path,
    manifests: &mut HashMap<PathBuf, CodexSkillTreeManifest>,
) -> Result<CodexExpectedSkillVisibility, String> {
    let related_errors = baseline
        .errors
        .iter()
        .filter(|error| error.relates_to(&selected.name, None))
        .map(CodexSkillCatalogError::display)
        .collect::<Vec<_>>();
    if !related_errors.is_empty() {
        return Err(format!(
            "Selected Tyde skill '{}' is ambiguous because Codex reported related discovery errors: {}",
            selected.name,
            related_errors.join("; ")
        ));
    }
    let unknown = baseline
        .skills
        .iter()
        .filter(|skill| skill.name == selected.name && skill.enabled.is_none())
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!(
            "Selected Tyde skill '{}' conflicts with {} same-name Codex catalog entry or entries whose enabled state is unknown",
            selected.name,
            unknown.len()
        ));
    }
    let same_name = baseline
        .skills
        .iter()
        .filter(|skill| skill.enabled == Some(true) && skill.name == selected.name)
        .collect::<Vec<_>>();
    let mut equivalent_visible = None;
    for native in same_name {
        let Some(native_skill_md) = codex_listed_filesystem_skill_md(native)? else {
            return Err(format!(
                "Selected Tyde skill '{}' conflicts with enabled non-filesystem Codex skill locator {}; native invocation would be ambiguous",
                selected.name,
                native.locator.as_deref().unwrap_or("<opaque>")
            ));
        };
        let native_dir = native_skill_md.parent().ok_or_else(|| {
            format!(
                "Native Codex skill '{}' path has no parent: {}",
                native.name,
                native_skill_md.display()
            )
        })?;
        let (_, selected_manifest) = codex_cached_skill_manifest(manifests, &selected.source_dir)?;
        let (_, native_manifest) = codex_cached_skill_manifest(manifests, native_dir)?;
        if selected_manifest == native_manifest {
            equivalent_visible = Some(native_skill_md);
        } else {
            return Err(format!(
                "Selected Tyde skill '{}' at {} conflicts with a different enabled Codex skill at {}; rename or reconcile one of the complete skill trees",
                selected.name,
                selected.source_dir.display(),
                native_dir.display()
            ));
        }
    }

    match equivalent_visible {
        Some(native_skill_md) => Ok(CodexExpectedSkillVisibility::Native(native_skill_md)),
        None => materialize_codex_skill(projection_root, selected, ordinal)
            .map(CodexExpectedSkillVisibility::Projected),
    }
}

fn prepare_codex_skills_blocking(
    selected_skills: Vec<ResolvedSkill>,
    baseline: CodexSkillCatalog,
    projection_root: PathBuf,
    selection: SkillSelection,
) -> Result<CodexSkillPreparation, String> {
    let mut manifests = HashMap::<PathBuf, CodexSkillTreeManifest>::new();
    let mut prepared = Vec::new();
    let mut diagnostics = Vec::new();
    for (ordinal, selected) in selected_skills.into_iter().enumerate() {
        match prepare_codex_selected_skill(
            &selected,
            ordinal,
            &baseline,
            &projection_root,
            &mut manifests,
        ) {
            Ok(visibility) => prepared.push(CodexPreparedSkill {
                ordinal,
                skill: selected,
                visibility,
            }),
            // The selection type does not decide this. An explicitly selected
            // skill Tyde cannot project is one capability the session does not
            // get; refusing to start would cost it all the others too. It does
            // decide how loudly to say it: a skill a custom agent named by hand
            // going missing is worth more than one of everything-installed.
            Err(err) => {
                discard_codex_skill_wrapper(&projection_root, ordinal)?;
                tracing::warn!("Codex skill selection degraded: {err}");
                diagnostics.push(format!(
                    "Codex session omitted Tyde skill '{}' ({}): {err}",
                    selected.name,
                    codex_selection_label(selection)
                ));
            }
        }
    }
    Ok(CodexSkillPreparation {
        prepared,
        diagnostics,
    })
}

/// How to describe a dropped skill: the selection type is no longer a policy
/// input, but it is still what tells the user whether their agent asked for this
/// skill by name or simply had everything installed.
fn codex_selection_label(selection: SkillSelection) -> &'static str {
    match selection {
        SkillSelection::Explicit => "explicitly selected",
        SkillSelection::AllInstalled => "installed",
    }
}

/// What the post-`extraRoots` catalog says about one selected skill.
///
/// The distinction that matters is *reachable* versus *not there*. Codex, like
/// Claude, resolves a name collision in favour of a skill the user already had;
/// the selection is then still satisfied — by someone else's body — and dropping
/// it would take away a capability the session does have. Only `Unresolved`
/// means the model would find nothing under that name.
enum CodexSkillVerdict {
    Verified,
    Superseded(String),
    Unresolved(String),
}

fn verify_codex_prepared_skills_blocking(
    prepared: &[CodexPreparedSkill],
    catalog: &CodexSkillCatalog,
) -> Vec<(usize, CodexSkillVerdict)> {
    prepared
        .iter()
        .enumerate()
        .filter_map(
            |(index, prepared)| match verify_codex_prepared_skill(prepared, catalog) {
                CodexSkillVerdict::Verified => None,
                verdict => Some((index, verdict)),
            },
        )
        .collect()
}

fn verify_codex_prepared_skill(
    prepared: &CodexPreparedSkill,
    catalog: &CodexSkillCatalog,
) -> CodexSkillVerdict {
    let selected = &prepared.skill;
    let expected_skill_md = prepared.visibility.skill_md();
    let related_errors = catalog
        .errors
        .iter()
        .filter(|error| error.relates_to(&selected.name, Some(expected_skill_md)))
        .map(CodexSkillCatalogError::display)
        .collect::<Vec<_>>();
    if !related_errors.is_empty() {
        return CodexSkillVerdict::Unresolved(format!(
            "Selected Codex skill '{}' is ambiguous because final discovery reported related errors: {}",
            selected.name,
            related_errors.join("; ")
        ));
    }
    let same_name = catalog
        .skills
        .iter()
        .filter(|skill| skill.name == selected.name)
        .collect::<Vec<_>>();
    if same_name.iter().any(|skill| skill.enabled.is_none()) {
        return CodexSkillVerdict::Unresolved(format!(
            "Selected Codex skill '{}' is ambiguous because a same-name catalog entry has an unknown enabled state",
            selected.name
        ));
    }
    let enabled = same_name
        .iter()
        .copied()
        .filter(|skill| skill.enabled == Some(true))
        .collect::<Vec<_>>();
    let describe_matches = || {
        same_name
            .iter()
            .map(|skill| {
                format!(
                    "{} ({})",
                    skill.locator.as_deref().unwrap_or("<opaque>"),
                    match skill.enabled {
                        Some(true) => "enabled",
                        Some(false) => "disabled",
                        None => "unknown",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if enabled.is_empty() {
        return CodexSkillVerdict::Unresolved(format!(
            "Selected Codex skill '{}' must resolve to exactly one enabled entry after \
             skills/extraRoots/set; found 0 [{}]",
            selected.name,
            describe_matches()
        ));
    }
    if enabled.len() > 1 {
        // More than one enabled entry under the name: the skill resolves, but
        // not necessarily to Tyde's copy. Reachable, so it stays; ambiguous, so
        // it is reported.
        return CodexSkillVerdict::Superseded(format!(
            "Selected Codex skill '{}' must resolve to exactly one enabled entry after \
             skills/extraRoots/set; found {} [{}], so Codex — not Tyde — decides which body runs. \
             Rename the Tyde skill to give this session its own copy.",
            selected.name,
            enabled.len(),
            describe_matches()
        ));
    }
    let visible_skill_md = match codex_listed_filesystem_skill_md(enabled[0]) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return CodexSkillVerdict::Superseded(format!(
                "Selected Codex skill '{}' resolved to non-filesystem locator {}, which is not the \
                 copy Tyde projected",
                selected.name,
                enabled[0].locator.as_deref().unwrap_or("<opaque>")
            ));
        }
        Err(err) => return CodexSkillVerdict::Unresolved(err),
    };
    if visible_skill_md != expected_skill_md {
        return CodexSkillVerdict::Superseded(format!(
            "Selected Codex skill '{}' resolved to {} instead of the copy Tyde projected at {}, so \
             a skill of the same name that was already installed won the name. The skill is \
             available; rename the Tyde skill to use Tyde's copy instead.",
            selected.name,
            visible_skill_md.display(),
            expected_skill_md.display()
        ));
    }
    CodexSkillVerdict::Verified
}

async fn set_codex_skill_extra_roots(
    rpc: &CodexRpc,
    projection_root: &Path,
    prepared: &[CodexPreparedSkill],
) -> Result<(), String> {
    let extra_roots = if prepared.iter().any(|skill| skill.visibility.is_projected()) {
        vec![projection_root.to_string_lossy().into_owned()]
    } else {
        Vec::new()
    };
    if let Err(err) = rpc
        .request(
            "skills/extraRoots/set",
            json!({ "extraRoots": extra_roots }),
        )
        .await
    {
        if is_codex_skills_extra_roots_unsupported_error(&err) {
            return Err(codex_skills_extra_roots_unsupported_message());
        }
        return Err(format!(
            "Codex skills/extraRoots/set failed before thread creation: {err}"
        ));
    }
    Ok(())
}

async fn configure_codex_native_skills(
    rpc: &CodexRpc,
    cwd: &str,
    projection: &CodexSkillProjection,
    selected_skills: &[ResolvedSkill],
    selection: SkillSelection,
) -> Result<CodexSkillSetup, String> {
    let baseline = codex_list_skills(rpc, cwd).await?;
    let mut diagnostics = codex_skill_catalog_diagnostics("baseline", &baseline.errors);
    let selected = selected_skills.to_vec();
    let projection_root = projection.path().to_path_buf();
    let preparation = tokio::task::spawn_blocking(move || {
        prepare_codex_skills_blocking(selected, baseline, projection_root, selection)
    })
    .await
    .map_err(|err| format!("Codex skill preparation task failed: {err}"))??;
    diagnostics.extend(preparation.diagnostics);
    let mut prepared = preparation.prepared;

    if prepared.is_empty() {
        // Nothing was projected, whatever the selection asked for. There is no
        // root worth registering, and the diagnostics above already say why.
        tracing::debug!("Codex selection exposed no Tyde skills; skipping skills/extraRoots/set");
        return Ok(CodexSkillSetup {
            exposed_names: Vec::new(),
            diagnostics,
        });
    }

    set_codex_skill_extra_roots(rpc, projection.path(), &prepared).await?;
    let maximum_verifications = selected_skills.len() + 1;
    for verification in 0..maximum_verifications {
        let final_catalog = codex_list_skills(rpc, cwd).await?;
        for diagnostic in codex_skill_catalog_diagnostics("final", &final_catalog.errors) {
            if !diagnostics.contains(&diagnostic) {
                diagnostics.push(diagnostic);
            }
        }
        let prepared_for_verification = prepared.clone();
        let catalog_for_verification = final_catalog.clone();
        let verdicts = tokio::task::spawn_blocking(move || {
            verify_codex_prepared_skills_blocking(
                &prepared_for_verification,
                &catalog_for_verification,
            )
        })
        .await
        .map_err(|err| format!("Codex skill verification task failed: {err}"))?;

        // A superseded skill still resolves, so it is kept and reported. Only
        // an unresolved one is dropped and re-verified — retrying a supersession
        // would loop until the retry budget ran out and then drop a skill the
        // session actually has.
        let mut failures = Vec::new();
        for (index, verdict) in verdicts {
            match verdict {
                CodexSkillVerdict::Verified => {}
                CodexSkillVerdict::Superseded(note) => {
                    if !diagnostics.contains(&note) {
                        tracing::warn!("{note}");
                        diagnostics.push(note);
                    }
                }
                CodexSkillVerdict::Unresolved(reason) => failures.push((index, reason)),
            }
        }
        if failures.is_empty() {
            break;
        }

        // The selection type does not change the outcome. A custom agent that
        // named its skills is still that agent with one fewer, and refusing to
        // start would cost it every skill that *did* resolve.
        let last_pass = verification + 1 == maximum_verifications;
        let failed_indices = failures
            .iter()
            .map(|(index, _)| *index)
            .collect::<HashSet<_>>();
        for (index, err) in &failures {
            let selected = &prepared[*index].skill;
            let label = codex_selection_label(selection);
            let diagnostic = if last_pass {
                format!(
                    "Codex session omitted Tyde skill '{}' ({label}) after \
                     {maximum_verifications} catalog checks did not converge: {err}",
                    selected.name
                )
            } else {
                format!(
                    "Codex session omitted Tyde skill '{}' ({label}): {err}",
                    selected.name
                )
            };
            tracing::warn!("{diagnostic}");
            diagnostics.push(diagnostic);
        }
        let projection_root = projection.path().to_path_buf();
        let failed_wrappers = failures
            .iter()
            .filter_map(|(index, _)| {
                let prepared = &prepared[*index];
                prepared
                    .visibility
                    .is_projected()
                    .then_some(prepared.ordinal)
            })
            .collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            for ordinal in failed_wrappers {
                discard_codex_skill_wrapper(&projection_root, ordinal)?;
            }
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("Codex skill cleanup task failed: {err}"))??;
        prepared = prepared
            .into_iter()
            .enumerate()
            .filter_map(|(index, prepared)| (!failed_indices.contains(&index)).then_some(prepared))
            .collect();
        set_codex_skill_extra_roots(rpc, projection.path(), &prepared).await?;
    }
    let exposed_names = prepared
        .into_iter()
        .map(|prepared| prepared.skill.name)
        .collect::<Vec<_>>();
    tracing::debug!(
        "Codex session exposed {} selected skill(s): {}",
        exposed_names.len(),
        exposed_names.join(", ")
    );
    Ok(CodexSkillSetup {
        exposed_names,
        diagnostics,
    })
}

/// Turn a failure to configure skills into a session that starts without them.
///
/// Every way skill configuration can fail — an app-server that rejects
/// `skills/extraRoots/set`, a catalog Tyde cannot read, a projection it cannot
/// write — costs the session its Tyde skills and nothing else. The session
/// starts, and this is the notice that says what it is missing.
fn codex_skills_unavailable(selected_skills: &[ResolvedSkill], reason: &str) -> CodexSkillSetup {
    let names = selected_skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let diagnostic = format!(
        "Tyde started this Codex session without its {} selected skill(s) [{names}]: {reason}",
        selected_skills.len()
    );
    tracing::warn!("{diagnostic}");
    CodexSkillSetup {
        exposed_names: Vec::new(),
        diagnostics: vec![diagnostic],
    }
}

fn codex_skill_catalog_diagnostics(phase: &str, errors: &[CodexSkillCatalogError]) -> Vec<String> {
    errors
        .iter()
        .map(|error| format!("Codex {phase} skills/list diagnostic: {}", error.display()))
        .collect()
}

fn codex_cached_skill_manifest(
    cache: &mut HashMap<PathBuf, CodexSkillTreeManifest>,
    root: &Path,
) -> Result<(PathBuf, CodexSkillTreeManifest), String> {
    let canonical = std::fs::canonicalize(root)
        .map_err(|err| format!("Failed to inspect skill tree {}: {err}", root.display()))?;
    if let Some(manifest) = cache.get(&canonical) {
        return Ok((canonical, manifest.clone()));
    }
    let (_, manifest) = CodexSkillTreeManifestBuilder::build(&canonical)?;
    cache.insert(canonical.clone(), manifest.clone());
    Ok((canonical, manifest))
}

fn is_codex_skills_extra_roots_unsupported_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("does not support native selected skills")
        || (normalized.contains("skills/extraroots/set")
            && (normalized.contains("-32601")
                || normalized.contains("method not found")
                || normalized.contains("unknown method")
                || normalized.contains("unknown request")
                || normalized.contains("unsupported method")))
}

fn codex_skills_extra_roots_unsupported_message() -> String {
    "Installed Codex CLI does not expose native selected skills (app-server method `skills/extraRoots/set`). Update Codex CLI and try again."
        .to_owned()
}

fn codex_selected_skills_ssh_notice(
    ssh_host: Option<&str>,
    selected_skills: &[ResolvedSkill],
) -> Option<String> {
    let host = ssh_host.filter(|_| !selected_skills.is_empty())?;
    Some(format!(
        "Tyde started this Codex session without its {} selected skill(s): Tyde will not send local skill paths to the app-server on SSH host '{host}', which cannot read this machine's disk",
        selected_skills.len()
    ))
}

fn codex_selected_skills_remote_workspace_notice(
    workspace_roots: &[String],
    selected_skills: &[ResolvedSkill],
) -> Option<String> {
    if selected_skills.is_empty() {
        return None;
    }
    let detail = codex_ssh_workspace_detail(workspace_roots)?;
    Some(format!(
        "Tyde started this Codex session without its {} selected skill(s): Tyde will not send local skill paths to a remote app-server {detail}",
        selected_skills.len()
    ))
}

/// The one reason a Codex session cannot have Tyde's skills that is known before
/// the app-server starts: it is running somewhere that cannot read this
/// machine's disk. The skills are dropped, not the session.
fn codex_remote_skill_notice(
    ssh_host: Option<&str>,
    workspace_roots: &[String],
    selected_skills: &[ResolvedSkill],
) -> Option<String> {
    codex_selected_skills_ssh_notice(ssh_host, selected_skills)
        .or_else(|| codex_selected_skills_remote_workspace_notice(workspace_roots, selected_skills))
}

fn codex_ssh_workspace_detail(workspace_roots: &[String]) -> Option<String> {
    if !workspace_roots
        .iter()
        .any(|root| root.trim_start().starts_with("ssh://"))
    {
        return None;
    }
    Some(
        match crate::remote::parse_remote_workspace_roots(workspace_roots) {
            Ok(Some((host, _))) => format!("for SSH host '{host}'"),
            Ok(None) => "for malformed SSH workspace roots".to_owned(),
            Err(err) => format!("for SSH workspace roots ({err})"),
        },
    )
}

pub struct CodexSession {
    inner: Arc<CodexInner>,
    // The thread/start or thread/fork response is the authoritative source for
    // this value. Keep it outside the event-state lock so parent-session
    // publication cannot be delayed by an early raw child notification.
    session_id: SessionId,
}

struct CodexThreadResponseConfig<'a> {
    startup_mcp_servers: &'a [StartupMcpServer],
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
}

struct CodexSelectedSkillContext<'a> {
    skills: &'a [ResolvedSkill],
    selection: SkillSelection,
    installed_provider_version: Option<&'a str>,
}

impl CodexSelectedSkillContext<'_> {
    fn empty() -> Self {
        Self {
            skills: &[],
            selection: SkillSelection::Explicit,
            installed_provider_version: None,
        }
    }
}

struct CodexThreadResources {
    steering_tempfile: Option<PathBuf>,
    skill_projection: Option<CodexSkillProjection>,
    skill_setup: CodexSkillSetup,
}

struct CodexSessionSpawnOptions<'a> {
    ephemeral: bool,
    access_mode: BackendAccessMode,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    execution_mode: BackendExecutionMode,
    installed_provider_version: Option<&'a str>,
    selected_skills: &'a [ResolvedSkill],
    skill_selection: SkillSelection,
}

impl CodexSession {
    pub async fn spawn(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: false,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
                installed_provider_version: None,
                selected_skills: &[],
                skill_selection: SkillSelection::Explicit,
            },
        )
        .await
    }

    pub async fn spawn_ephemeral(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: true,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
                installed_provider_version: None,
                selected_skills: &[],
                skill_selection: SkillSelection::Explicit,
            },
        )
        .await
    }

    pub async fn spawn_admin(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::spawn_with_mode(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            CodexSessionSpawnOptions {
                ephemeral: true,
                access_mode,
                subagent_emitter: None,
                execution_mode: BackendExecutionMode::Agent,
                installed_provider_version: None,
                selected_skills: &[],
                skill_selection: SkillSelection::Explicit,
            },
        )
        .await
    }

    async fn spawn_with_mode(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        options: CodexSessionSpawnOptions<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let CodexSessionSpawnOptions {
            ephemeral,
            access_mode,
            subagent_emitter,
            execution_mode,
            installed_provider_version,
            selected_skills,
            skill_selection,
        } = options;
        // A session that cannot reach Tyde's skills runs without them. Dropping
        // the selection here — rather than refusing the spawn — is what keeps a
        // remote workspace usable to a skill-bearing agent.
        let mut skill_notices: Vec<String> = Vec::new();
        let mut selected_skills = selected_skills;
        if let Some(notice) =
            codex_remote_skill_notice(ssh_host.as_deref(), workspace_roots, selected_skills)
        {
            tracing::warn!("{notice}");
            skill_notices.push(notice);
            selected_skills = &[];
        }
        let mut skill_projection = match CodexSkillProjection::new(selected_skills) {
            Ok(projection) => projection,
            Err(err) => {
                let unavailable = codex_skills_unavailable(selected_skills, &err);
                skill_notices.extend(unavailable.diagnostics);
                selected_skills = &[];
                None
            }
        };
        let steering_tempfile = match steering_content {
            Some(content) if !content.trim().is_empty() => {
                Some(write_codex_session_steering_tempfile(content)?)
            }
            _ => None,
        };
        #[cfg(feature = "test-support")]
        let legacy_dynamic_await = workspace_roots.iter().any(|root| {
            Path::new(root)
                .join(CODEX_LEGACY_DYNAMIC_AWAIT_MARKER)
                .is_file()
        });
        #[cfg(feature = "test-support")]
        let conformance_mcp_servers = legacy_dynamic_await.then(|| {
            startup_mcp_servers
                .iter()
                .filter(|server| server.name != AGENT_CONTROL_AWAIT_MCP_SERVER_NAME)
                .cloned()
                .collect::<Vec<_>>()
        });
        #[cfg(feature = "test-support")]
        let startup_mcp_servers = conformance_mcp_servers
            .as_deref()
            .unwrap_or(startup_mcp_servers);
        let (rpc, inbound_rx) = match CodexRpc::spawn(
            ssh_host.as_deref(),
            startup_mcp_servers,
            steering_tempfile.as_deref(),
            access_mode,
            execution_mode,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                remove_codex_skill_projection(&mut skill_projection);
                remove_codex_steering_tempfile(&steering_tempfile);
                return Err(err);
            }
        };

        if let Err(err) = initialize_codex_rpc(&rpc, installed_provider_version).await {
            cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
            return Err(err);
        }

        let cwd = if ssh_host.is_some() {
            // For remote sessions, extract the remote path (host already stripped)
            match crate::remote::parse_remote_workspace_roots(workspace_roots) {
                Ok(Some((_, paths))) => match paths.into_iter().next() {
                    Some(path) => path,
                    None => {
                        cleanup_codex_startup_failure(
                            rpc,
                            &mut skill_projection,
                            &steering_tempfile,
                        )
                        .await;
                        return Err("No remote workspace root found".to_owned());
                    }
                },
                Ok(None) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err("Expected remote workspace roots for SSH session".to_owned());
                }
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err(err);
                }
            }
        } else {
            match pick_workspace_root(workspace_roots) {
                Ok(root) => root,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err(err);
                }
            }
        };
        let skill_setup = if let Some(projection) = skill_projection.as_ref() {
            match configure_codex_native_skills(
                &rpc,
                &cwd,
                projection,
                selected_skills,
                skill_selection,
            )
            .await
            {
                Ok(setup) => setup,
                // Configuring skills never fails the spawn. Whatever went wrong
                // — a rejected `skills/extraRoots/set`, an unreadable catalog —
                // costs this session its Tyde skills and nothing else, and the
                // notice says which ones and why.
                Err(err) => {
                    remove_codex_skill_projection(&mut skill_projection);
                    codex_skills_unavailable(selected_skills, &err)
                }
            }
        } else {
            CodexSkillSetup::default()
        };
        // Notices decided before the app-server started ride along with the
        // ones it produced, so the user sees every reason in one place.
        let mut skill_setup = skill_setup;
        skill_setup.diagnostics.splice(0..0, skill_notices);

        let mut thread_start_params = json!({
            "cwd": cwd,
            "sandbox": codex_sandbox_mode(access_mode, execution_mode),
            "approvalPolicy": codex_approval_policy(execution_mode),
            "ephemeral": ephemeral || execution_mode == BackendExecutionMode::InferenceOnly,
            "experimentalRawEvents": CODEX_ENABLE_EXPERIMENTAL_RAW_EVENTS,
            "persistExtendedHistory": false
        });
        #[cfg(feature = "test-support")]
        if legacy_dynamic_await {
            thread_start_params["dynamicTools"] = json!([{
                "type": "function",
                "name": "tyde_await_agents",
                "description": "Wait until any supplied direct child Tyde agent becomes idle or failed.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "agent_ids": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "type": "string", "minLength": 1 }
                        }
                    },
                    "required": ["agent_ids"]
                }
            }]);
            tracing::info!("Projected the legacy Codex dynamic await tool for conformance");
        }
        if execution_mode == BackendExecutionMode::InferenceOnly {
            match codex_inference_thread_config(&rpc, &cwd).await {
                Ok(config) => thread_start_params["config"] = config,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err(err);
                }
            }
        }
        let thread_started = match rpc.request("thread/start", thread_start_params).await {
            Ok(response) => response,
            Err(err) => {
                cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
                return Err(err);
            }
        };
        if thread_started
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .is_none()
        {
            cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
            return Err("Codex thread/start response missing thread.id".to_owned());
        }

        Self::from_thread_response(
            rpc,
            inbound_rx,
            CodexThreadResources {
                steering_tempfile,
                skill_projection,
                skill_setup,
            },
            CodexThreadResponseConfig {
                startup_mcp_servers,
                access_mode,
                execution_mode,
            },
            thread_started,
            "thread/start",
            subagent_emitter,
        )
        .await
    }

    pub async fn fork(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
        from_thread_id: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        Self::fork_with_selected_skills(
            workspace_roots,
            ssh_host,
            startup_mcp_servers,
            steering_content,
            access_mode,
            from_thread_id,
            CodexSelectedSkillContext::empty(),
        )
        .await
    }

    async fn fork_with_selected_skills(
        workspace_roots: &[String],
        ssh_host: Option<String>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_content: Option<&str>,
        access_mode: BackendAccessMode,
        from_thread_id: &str,
        selected_skill_context: CodexSelectedSkillContext<'_>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let CodexSelectedSkillContext {
            skills: selected_skills,
            selection: skill_selection,
            installed_provider_version,
        } = selected_skill_context;
        // A session that cannot reach Tyde's skills runs without them. Dropping
        // the selection here — rather than refusing the spawn — is what keeps a
        // remote workspace usable to a skill-bearing agent.
        let mut skill_notices: Vec<String> = Vec::new();
        let mut selected_skills = selected_skills;
        if let Some(notice) =
            codex_remote_skill_notice(ssh_host.as_deref(), workspace_roots, selected_skills)
        {
            tracing::warn!("{notice}");
            skill_notices.push(notice);
            selected_skills = &[];
        }
        let mut skill_projection = match CodexSkillProjection::new(selected_skills) {
            Ok(projection) => projection,
            Err(err) => {
                let unavailable = codex_skills_unavailable(selected_skills, &err);
                skill_notices.extend(unavailable.diagnostics);
                selected_skills = &[];
                None
            }
        };
        let steering_tempfile = match steering_content {
            Some(content) if !content.trim().is_empty() => {
                Some(write_codex_session_steering_tempfile(content)?)
            }
            _ => None,
        };
        let (rpc, inbound_rx) = match CodexRpc::spawn(
            ssh_host.as_deref(),
            startup_mcp_servers,
            steering_tempfile.as_deref(),
            access_mode,
            BackendExecutionMode::Agent,
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                remove_codex_skill_projection(&mut skill_projection);
                remove_codex_steering_tempfile(&steering_tempfile);
                return Err(err);
            }
        };

        if let Err(err) = initialize_codex_rpc(&rpc, installed_provider_version).await {
            cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
            return Err(err);
        }

        let cwd = if ssh_host.is_some() {
            let parsed = match crate::remote::parse_remote_workspace_roots(workspace_roots) {
                Ok(parsed) => parsed,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err(err);
                }
            };
            let Some((_, paths)) = parsed else {
                cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
                return Err("Expected remote workspace roots for SSH session".to_string());
            };
            let Some(path) = paths.into_iter().next() else {
                cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
                return Err("No remote workspace root found".to_string());
            };
            path
        } else {
            match pick_workspace_root(workspace_roots) {
                Ok(root) => root,
                Err(err) => {
                    cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile)
                        .await;
                    return Err(err);
                }
            }
        };
        let skill_setup = if let Some(projection) = skill_projection.as_ref() {
            match configure_codex_native_skills(
                &rpc,
                &cwd,
                projection,
                selected_skills,
                skill_selection,
            )
            .await
            {
                Ok(setup) => setup,
                // Configuring skills never fails the spawn. Whatever went wrong
                // — a rejected `skills/extraRoots/set`, an unreadable catalog —
                // costs this session its Tyde skills and nothing else, and the
                // notice says which ones and why.
                Err(err) => {
                    remove_codex_skill_projection(&mut skill_projection);
                    codex_skills_unavailable(selected_skills, &err)
                }
            }
        } else {
            CodexSkillSetup::default()
        };
        // Notices decided before the app-server started ride along with the
        // ones it produced, so the user sees every reason in one place.
        let mut skill_setup = skill_setup;
        skill_setup.diagnostics.splice(0..0, skill_notices);

        let mut fork_params = json!({
            "threadId": from_thread_id,
            "cwd": cwd.clone(),
            "sandbox": codex_sandbox_mode(access_mode, BackendExecutionMode::Agent),
            "approvalPolicy": CODEX_FORCED_APPROVAL_POLICY,
            "ephemeral": false,
            "persistExtendedHistory": false
        });
        fork_params["runtimeWorkspaceRoots"] =
            json!(codex_runtime_workspace_roots(workspace_roots, &cwd));

        let thread_forked = match rpc.request("thread/fork", fork_params).await {
            Ok(value) => value,
            Err(err) => {
                cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
                return Err(format!("Codex thread/fork failed: {err}"));
            }
        };
        if thread_forked
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .is_none()
        {
            cleanup_codex_startup_failure(rpc, &mut skill_projection, &steering_tempfile).await;
            return Err("Codex thread/fork response missing thread.id".to_string());
        }

        Self::from_thread_response(
            rpc,
            inbound_rx,
            CodexThreadResources {
                steering_tempfile,
                skill_projection,
                skill_setup,
            },
            CodexThreadResponseConfig {
                startup_mcp_servers,
                access_mode,
                execution_mode: BackendExecutionMode::Agent,
            },
            thread_forked,
            "thread/fork",
            None,
        )
        .await
    }

    async fn from_thread_response(
        rpc: CodexRpc,
        inbound_rx: mpsc::UnboundedReceiver<CodexInbound>,
        resources: CodexThreadResources,
        config: CodexThreadResponseConfig<'_>,
        thread_response: Value,
        method: &str,
        subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let CodexThreadResources {
            steering_tempfile,
            skill_projection,
            skill_setup,
        } = resources;
        let thread_id = thread_response
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Codex {method} response missing thread.id"))?
            .to_string();
        let session_id = SessionId(thread_id.clone());
        let strict_response_splitting = method == "thread/start";
        let mut response_splitters = HashMap::new();
        response_splitters.insert(
            thread_id.clone(),
            CodexResponseSplitter::new(&thread_id, strict_response_splitting),
        );

        let model = thread_response
            .get("model")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            event_tx,
            AgentName(CODEX_AGENT_NAME),
        ));

        let initial_capacity_emitter = subagent_emitter.clone();
        let inner = Arc::new(CodexInner {
            rpc,
            emitter,
            state: Mutex::new(initial_codex_state(
                thread_id,
                response_splitters,
                model,
                config.access_mode,
                config.execution_mode,
                codex_has_http_mcp_servers(config.startup_mcp_servers),
                subagent_emitter,
            )),
            steering_tempfile,
            skill_projection: std::sync::Mutex::new(skill_projection),
        });

        if !skill_setup.exposed_names.is_empty() {
            tracing::debug!(
                "Codex session retained exposed skills: {}",
                skill_setup.exposed_names.join(", ")
            );
        }
        for diagnostic in skill_setup.diagnostics {
            inner
                .emitter
                .subprocess_stderr(&format!("Codex warning: {diagnostic}"));
        }
        emit_codex_raw_events_warning_if_needed(inner.emitter.as_ref(), strict_response_splitting);

        let forward_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut rx = inbound_rx;
            let mut nested_batches = HashMap::<String, CodexNestedGenericBatch>::new();
            while let Some(inbound) = rx.recv().await {
                if let Some((turn_id, call_count)) = codex_raw_nested_batch_declaration(&inbound) {
                    tracing::debug!(
                        turn_id,
                        call_count,
                        "Codex declared a nested generic tool batch"
                    );
                    assert!(
                        nested_batches
                            .insert(turn_id, CodexNestedGenericBatch::new(call_count))
                            .is_none(),
                        "Codex declared overlapping nested generic batches for one turn"
                    );
                    forward_inner.handle_inbound(inbound).await;
                    continue;
                }
                let Some((method, params, _, item_id)) =
                    codex_nested_generic_tool_notification(&inbound)
                else {
                    forward_inner.handle_inbound(inbound).await;
                    continue;
                };
                let Some(turn_id) = extract_turn_id(params) else {
                    forward_inner.handle_inbound(inbound).await;
                    continue;
                };
                let Some(batch) = nested_batches.get_mut(&turn_id) else {
                    forward_inner.handle_inbound(inbound).await;
                    continue;
                };
                match method {
                    "item/started" => {
                        tracing::debug!(
                            turn_id,
                            item_id,
                            "Codex admitted a nested generic tool request"
                        );
                        batch.observe_start(item_id);
                        forward_inner.handle_inbound(inbound).await;
                        if batch.starts_remaining == 0 {
                            for completion in batch.pending_completions.drain(..) {
                                forward_inner.handle_inbound(completion).await;
                            }
                        }
                    }
                    "item/completed" => {
                        batch.observe_completion(item_id);
                        if batch.starts_remaining == 0 {
                            forward_inner.handle_inbound(inbound).await;
                        } else {
                            tracing::debug!(
                                turn_id,
                                item_id,
                                starts_remaining = batch.starts_remaining,
                                "Holding a nested generic completion until every request is admitted"
                            );
                            batch.pending_completions.push(inbound);
                        }
                    }
                    _ => forward_inner.handle_inbound(inbound).await,
                }
                if batch.completions_remaining == 0 {
                    assert!(
                        batch.pending_completions.is_empty(),
                        "Codex nested batch ended before every request was admitted"
                    );
                    nested_batches.remove(&turn_id);
                }
            }
        });

        if let Some(emitter) = initial_capacity_emitter {
            inner.spawn_capacity_refresh(emitter);
        }

        Ok((Self { inner, session_id }, event_rx))
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id.clone()
    }

    pub fn command_handle(&self) -> CodexCommandHandle {
        CodexCommandHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    pub async fn list_mcp_server_statuses(&self) -> Result<Value, String> {
        self.inner
            .rpc
            .request(
                "mcpServerStatus/list",
                json!({
                    "detail": "toolsAndAuthOnly",
                    "limit": 100
                }),
            )
            .await
    }

    pub async fn call_mcp_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Option<Value>,
        meta: Option<Value>,
    ) -> Result<Value, String> {
        let thread_id = {
            let state = self.inner.state.lock().await;
            state.thread_id.clone()
        };

        self.inner
            .rpc
            .request(
                "mcpServer/tool/call",
                json!({
                    "threadId": thread_id,
                    "server": server,
                    "tool": tool,
                    "arguments": arguments,
                    "_meta": meta
                }),
            )
            .await
    }

    pub(crate) async fn set_subagent_emitter(
        &self,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(), String> {
        let mut state = self.inner.state.lock().await;
        state.subagent_emitter = Some(emitter.clone());
        drop(state);
        self.inner.spawn_capacity_refresh(emitter);
        Ok(())
    }

    pub async fn shutdown(self) {
        self.inner.terminate_background_terminals().await;
        self.inner.drain_background_commands().await;
        self.inner.complete_all_codex_subagents().await;
        self.inner.rpc.shutdown().await;
        remove_codex_skill_projection_guard(&self.inner.skill_projection);
        remove_codex_steering_tempfile(&self.inner.steering_tempfile);
    }
}

async fn cleanup_codex_startup_failure(
    rpc: CodexRpc,
    skill_projection: &mut Option<CodexSkillProjection>,
    steering_tempfile: &Option<std::path::PathBuf>,
) {
    rpc.shutdown().await;
    remove_codex_skill_projection(skill_projection);
    remove_codex_steering_tempfile(steering_tempfile);
}

fn remove_codex_skill_projection(projection: &mut Option<CodexSkillProjection>) {
    if let Some(projection) = projection.take() {
        projection.remove();
    }
}

fn remove_codex_skill_projection_guard(
    projection: &std::sync::Mutex<Option<CodexSkillProjection>>,
) {
    let mut projection = projection
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    remove_codex_skill_projection(&mut projection);
}

fn remove_codex_steering_tempfile(steering_tempfile: &Option<std::path::PathBuf>) {
    if let Some(path) = steering_tempfile
        && let Err(e) = std::fs::remove_file(path)
    {
        tracing::warn!(
            "Failed to remove steering temp file {}: {e}",
            path.display()
        );
    }
}

/// Maps only the verified passive app-server notification. This deliberately
/// consumes the already-open app-server notification.
pub(crate) fn map_passive_rate_limits_updated(
    params: &Value,
) -> Result<CapacityReport, CapacityUnavailableReason> {
    let snapshot = params.get("rateLimits").unwrap_or(params);
    if !snapshot.is_object() {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    if let Some(limit_id) = snapshot.get("limitId")
        && !matches!(limit_id, Value::Null)
        && !limit_id.as_str().is_some_and(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
    {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    let reached_scope = match snapshot.get("rateLimitReachedType") {
        None | Some(Value::Null) => CapacityScope::NotReported,
        Some(Value::String(value))
            if value.len() <= 128 && !value.chars().any(char::is_control) =>
        {
            if value.starts_with("workspace_") {
                CapacityScope::Workspace
            } else if value.starts_with("organization_") {
                CapacityScope::OrganizationSpend
            } else {
                CapacityScope::NotReported
            }
        }
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let limit_name = match snapshot.get("limitName") {
        None | Some(Value::Null) => None,
        Some(Value::String(value))
            if !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control) =>
        {
            Some(value.as_str())
        }
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let window_scope = match snapshot.get("individualLimit") {
        None | Some(Value::Null) => CapacityScope::NotReported,
        Some(Value::Bool(true)) => CapacityScope::Individual,
        Some(Value::Bool(false)) => CapacityScope::Account,
        _ => return Err(CapacityUnavailableReason::MalformedReport),
    };
    let mut buckets = Vec::new();
    for (field, slot, label) in [
        ("primary", CodexLimitSlot::Primary, "primary limit"),
        ("secondary", CodexLimitSlot::Secondary, "secondary limit"),
    ] {
        let Some(window_value) = snapshot.get(field) else {
            continue;
        };
        if window_value.is_null() {
            continue;
        }
        let window = window_value
            .as_object()
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let used = window
            .get("usedPercent")
            .and_then(Value::as_u64)
            .filter(|value| *value <= 100)
            .ok_or(CapacityUnavailableReason::MalformedReport)? as u8;
        let duration_minutes = window
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let reset = match window.get("resetsAt") {
            None | Some(Value::Null) => CapacityReset::NotReported,
            Some(value) => value
                .as_u64()
                .and_then(|seconds| seconds.checked_mul(1000))
                .map(|at_ms| CapacityReset::At { at_ms })
                .ok_or(CapacityUnavailableReason::MalformedReport)?,
        };
        buckets.push(CapacityBucket {
            id: CapacityBucketId::Codex { slot },
            label: limit_name.map_or_else(|| label.to_string(), |name| format!("{name} {label}")),
            measure: CapacityMeasure::UsedPercent {
                used_percent: used,
                remaining_percent: 100 - used,
                provenance: ValueProvenance {
                    vendor_reported: true,
                },
            },
            scope: window_scope.clone(),
            window: CapacityWindow::Rolling { duration_minutes },
            reset,
            status: None,
        });
    }
    if let Some(credits_value) = snapshot.get("credits")
        && !credits_value.is_null()
    {
        let credits = credits_value
            .as_object()
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let has_credits = credits
            .get("hasCredits")
            .and_then(Value::as_bool)
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let unlimited = credits
            .get("unlimited")
            .and_then(Value::as_bool)
            .ok_or(CapacityUnavailableReason::MalformedReport)?;
        let balance = match credits.get("balance") {
            None | Some(Value::Null) => None,
            Some(Value::String(value))
                if !value.is_empty()
                    && value.len() <= 64
                    && !value.chars().any(char::is_control) =>
            {
                Some(value.clone())
            }
            Some(Value::Number(value)) => {
                let value = value.to_string();
                Some(
                    (value.len() <= 64)
                        .then_some(value)
                        .ok_or(CapacityUnavailableReason::MalformedReport)?,
                )
            }
            Some(_) => return Err(CapacityUnavailableReason::MalformedReport),
        };
        buckets.push(CapacityBucket {
            id: CapacityBucketId::Codex {
                slot: CodexLimitSlot::Credits,
            },
            label: "credits".to_string(),
            measure: CapacityMeasure::Credits {
                has_credits,
                unlimited,
                balance,
            },
            scope: reached_scope,
            window: CapacityWindow::NotReported,
            reset: CapacityReset::NotReported,
            status: None,
        });
    }
    if buckets.is_empty() {
        return Err(CapacityUnavailableReason::MalformedReport);
    }
    let complete_windows = snapshot.get("primary").is_some_and(Value::is_object)
        && snapshot.get("secondary").is_some_and(Value::is_object);
    Ok(CapacityReport {
        source: CapacitySource::CodexAccountRateLimitsUpdated,
        observed_at_ms: None,
        plan: match snapshot.get("planType") {
            None | Some(Value::Null) => None,
            Some(Value::String(label))
                if !label.is_empty()
                    && label.len() <= 128
                    && !label.chars().any(char::is_control) =>
            {
                Some(CapacityPlanLabel {
                    label: label.clone(),
                })
            }
            _ => return Err(CapacityUnavailableReason::MalformedReport),
        },
        buckets,
        coverage: if complete_windows {
            CapacityCoverage::AllVendorBuckets
        } else {
            CapacityCoverage::RepresentativeBucketOnly
        },
    })
}

/// Route the verified passive notification through the emitter supplied by the
/// owning agent session. The adapter never discovers a host globally.
pub(crate) fn forward_passive_rate_limits_updated(params: &Value, emitter: &dyn SubAgentEmitter) {
    let state = match map_passive_rate_limits_updated(params) {
        Ok(report) => protocol::BackendCapacityState::Known { report },
        Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
    };
    emitter.on_backend_capacity(protocol::BackendKind::Codex, state);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexCapacityAccountMode {
    ChatGpt,
    Unsupported,
    Unauthenticated,
}

fn codex_capacity_account_mode(account_response: &Value) -> CodexCapacityAccountMode {
    match account_response
        .get("account")
        .and_then(Value::as_object)
        .and_then(|account| account.get("type"))
        .and_then(Value::as_str)
    {
        Some("chatgpt" | "personalAccessToken") => CodexCapacityAccountMode::ChatGpt,
        Some(_) => CodexCapacityAccountMode::Unsupported,
        None => CodexCapacityAccountMode::Unauthenticated,
    }
}

pub(crate) async fn probe_session_settings_schema(
    program: Option<&str>,
) -> Result<SessionSettingsSchema, String> {
    let (rpc, _inbound_rx) = CodexRpc::spawn_with_local_program(
        None,
        &[],
        None,
        BackendAccessMode::Unrestricted,
        BackendExecutionMode::Agent,
        program,
    )
    .await
    .map_err(|err| format!("Codex model discovery failed to spawn app-server: {err}"))?;

    if let Err(err) = rpc
        .request(
            "initialize",
            json!({
                "clientInfo": { "name": "tyde", "title": Value::Null, "version": "0.1" },
                "capabilities": { "experimentalApi": true }
            }),
        )
        .await
    {
        return codex_probe_result_with_cleanup(
            Err(format!("Codex model discovery initialize failed: {err}")),
            rpc.terminate().await,
        );
    }

    let response = rpc
        .request("model/list", json!({ "includeHidden": false }))
        .await;
    let response = codex_probe_result_with_cleanup(
        response.map_err(|err| format!("Codex model discovery model/list RPC failed: {err}")),
        rpc.terminate().await,
    )?;

    let raw_models = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            format!("Codex model discovery model/list response missing data array: {response}")
        })?;

    let models = codex_model_metadata_from_raw(raw_models);

    if models.is_empty() {
        return Err("Codex model discovery model/list returned no usable models".to_string());
    }

    Ok(codex_session_settings_schema(models))
}

fn codex_probe_result_with_cleanup<T>(
    operation: Result<T, String>,
    cleanup: Result<(), String>,
) -> Result<T, String> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(format!(
            "Codex model discovery app-server cleanup failed: {cleanup_error}"
        )),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; Codex app-server cleanup also failed: {cleanup_error}"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexModelMetadata {
    option: protocol::SelectOption,
    reasoning_options: Vec<protocol::SelectOption>,
    is_default: bool,
}

fn codex_model_metadata_from_raw(raw_models: &[Value]) -> Vec<CodexModelMetadata> {
    let mut models = raw_models
        .iter()
        .filter_map(codex_model_metadata_entry_from_raw)
        .collect::<Vec<_>>();

    models.sort_by(|a, b| compare_codex_model_ids_for_display(&a.option.value, &b.option.value));
    models.dedup_by(|a, b| a.option.value.eq_ignore_ascii_case(&b.option.value));
    models
}

fn codex_model_metadata_entry_from_raw(model: &Value) -> Option<CodexModelMetadata> {
    let id = model
        .get("model")
        .or_else(|| model.get("id"))
        .and_then(Value::as_str)?
        .trim();
    if id.is_empty() {
        return None;
    }

    let mut reasoning_options = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(codex_reasoning_option_from_raw)
        .collect::<Vec<_>>();
    reasoning_options.dedup_by(|a, b| a.value == b.value);

    Some(CodexModelMetadata {
        option: protocol::SelectOption {
            value: id.to_string(),
            // Codex's displayName casing is not currently normalized across entries
            // (for example, `gpt-...` and `GPT-...` can appear in one response).
            // The model id is the canonical value we send back to Codex, so use it as
            // the label too and normalize only display casing.
            label: codex_model_label_from_id(id),
        },
        reasoning_options,
        is_default: model
            .get("isDefault")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn codex_reasoning_option_from_raw(value: &Value) -> Option<protocol::SelectOption> {
    let effort = value.get("reasoningEffort").and_then(Value::as_str)?.trim();
    if effort.is_empty() {
        return None;
    }
    Some(protocol::SelectOption {
        value: effort.to_string(),
        label: match effort {
            "xhigh" => "XHigh".to_string(),
            _ => effort
                .split(['-', '_'])
                .filter(|part| !part.is_empty())
                .map(|part| {
                    let mut chars = part.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_uppercase().collect::<String>()
                            + &chars.as_str().to_ascii_lowercase()
                    })
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
    })
}

fn codex_model_label_from_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

fn compare_codex_model_ids_for_display(a: &str, b: &str) -> std::cmp::Ordering {
    let a_numbers = numeric_components(a);
    let b_numbers = numeric_components(b);

    for (a_number, b_number) in a_numbers.iter().zip(b_numbers.iter()) {
        match b_number.cmp(a_number) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }

    match a_numbers.len().cmp(&b_numbers.len()) {
        // More numeric components are treated as a more specific/newer version:
        // e.g. `gpt-5.1` sorts before `gpt-5`.
        std::cmp::Ordering::Greater => return std::cmp::Ordering::Less,
        std::cmp::Ordering::Less => return std::cmp::Ordering::Greater,
        std::cmp::Ordering::Equal => {}
    }

    let a_normalized = a.to_ascii_lowercase();
    let b_normalized = b.to_ascii_lowercase();
    match a_normalized.cmp(&b_normalized) {
        std::cmp::Ordering::Equal => a.cmp(b),
        ordering => ordering,
    }
}

fn numeric_components(value: &str) -> Vec<u64> {
    let mut components = Vec::new();
    let mut current: Option<u64> = None;

    for byte in value.bytes() {
        if byte.is_ascii_digit() {
            let digit = u64::from(byte - b'0');
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit),
            );
        } else if let Some(number) = current.take() {
            components.push(number);
        }
    }

    if let Some(number) = current {
        components.push(number);
    }

    components
}

fn codex_generated_identity_epoch(thread_id: &str) -> u64 {
    thread_id.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProviderStreamConflict {
    MissingMessageId,
    ForeignActiveMessageId,
    MismatchedEndMessageId,
    DuplicateTerminalMessageId,
    ConflictingDuplicateCompletion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProviderResponseOrigin {
    IdlessProviderResponseItem,
    IdlessReasoning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexProviderResponseIdentity {
    origin: CodexProviderResponseOrigin,
    stream_epoch: u64,
    item_ordinal: u64,
}

impl CodexProviderResponseIdentity {
    fn message_id(&self) -> ChatMessageId {
        let origin = match self.origin {
            CodexProviderResponseOrigin::IdlessProviderResponseItem => "response",
            CodexProviderResponseOrigin::IdlessReasoning => "reasoning",
        };
        ChatMessageId(format!(
            "codex-provider:{origin}:{}:{}",
            self.stream_epoch, self.item_ordinal
        ))
    }
}

fn codex_stream_identity_violation_message(violation: CodexProviderStreamConflict) -> &'static str {
    match violation {
        CodexProviderStreamConflict::MissingMessageId => {
            "Stream identity violation: missing message id"
        }
        CodexProviderStreamConflict::ForeignActiveMessageId => {
            "Stream identity violation: foreign active message id"
        }
        CodexProviderStreamConflict::MismatchedEndMessageId => {
            "Stream identity violation: mismatched end message id"
        }
        CodexProviderStreamConflict::DuplicateTerminalMessageId => {
            "Stream identity violation: duplicate terminal message id"
        }
        CodexProviderStreamConflict::ConflictingDuplicateCompletion => {
            "Stream identity violation: conflicting duplicate completion"
        }
    }
}

#[derive(Clone)]
struct PendingRequest {
    request_id: Value,
    tool_call_id: String,
    kind: PendingRequestKind,
}

#[derive(Clone)]
enum PendingRequestKind {
    CommandApproval,
    FileChangeApproval,
    ExecCommandApproval,
    ApplyPatchApproval,
    UserInput { questions: Vec<String> },
}

#[derive(Clone)]
struct ActiveStreamState {
    turn_id: String,
    message_id: ChatMessageId,
    generated_identity: Option<CodexProviderResponseIdentity>,
    text: String,
    reasoning: String,
    reasoning_only: bool,
    stream_published: bool,
    images: Vec<ImageData>,
}

#[derive(Clone)]
struct BufferedCodexToolRequest {
    turn_id: Option<String>,
    provider_item_id: Option<String>,
    provider_call_id: Option<String>,
    tool_call_id: String,
    tool_name: String,
    arguments: Value,
    tool_type: Value,
    /// How much of the response's text had arrived when the card was declared.
    /// `stream_end` re-declares the call and must reuse this, or the two
    /// declarations disagree and the emitter rejects the pair as conflicting.
    /// `None` for a request recovered from a raw item, which is never declared
    /// early and so has no streaming position to preserve.
    content_offset: Option<u32>,
}

struct OpenCodexProviderResponse {
    identity: CodexProviderResponseIdentity,
    turn_id: String,
    /// The provider's own id for this response, taken from
    /// `rawResponse/completed`. Absent on a resumed thread, which gets no raw
    /// events at all.
    response_id: Option<String>,
    text: String,
    reasoning: String,
    item_text: HashMap<String, String>,
    item_reasoning: HashMap<String, String>,
    typed_item_ids: HashSet<String>,
    raw_item_ids: HashSet<String>,
    raw_call_ids: HashSet<String>,
    pending_typed_tool_item_id: Option<String>,
    pending_typed_tool_call_id: Option<String>,
    tool_requests: Vec<BufferedCodexToolRequest>,
    raw_tool_requests: Vec<BufferedCodexToolRequest>,
}

struct ClosedCodexProviderResponse {
    message_id: ChatMessageId,
    response_id: Option<String>,
    usage: Option<Value>,
    failed: bool,
}

struct CodexResponseSplitter {
    enabled: bool,
    stream_epoch: u64,
    next_ordinal: u64,
    open: Option<OpenCodexProviderResponse>,
    closed: VecDeque<ClosedCodexProviderResponse>,
    provider_typed_tool_item_ids: HashSet<String>,
    execution_only_typed_tool_item_ids: HashSet<String>,
    execution_only_typed_tool_owners: HashMap<String, BufferedCodexToolRequest>,
    claimed_raw_tool_calls: HashSet<String>,
    /// Provider `call_id`s a typed item has taken over, so the raw output for
    /// the same call neither completes the card a second time nor reports its
    /// missing owner as a loss. Unlike `claimed_raw_tool_calls` this is written
    /// only by `observe_typed_item_started` and never cleared, because the raw
    /// output routinely arrives *after* the typed item has completed and
    /// unparked its owner.
    typed_owned_call_ids: HashSet<String>,
    /// Raw declarations normally hidden because a richer typed item renders
    /// the call. The runtime can reject a nested command before creating that
    /// typed item, so retain the declaration until its raw output proves
    /// whether the typed owner actually existed.
    suppressed_raw_tool_requests: IndexMap<String, BufferedCodexToolRequest>,
    completed_raw_tool_call_ids: HashSet<String>,
    pending_raw_tool_owners: IndexMap<String, BufferedCodexToolRequest>,
    last_token_usage: Option<Value>,
}

struct CodexResponseDelta {
    delta: String,
}

struct FinalizedCodexProviderResponse {
    message_id: ChatMessageId,
    turn_id: String,
    content: String,
    reasoning: Option<String>,
    tool_requests: Vec<BufferedCodexToolRequest>,
    evicted_tool_requests: Vec<BufferedCodexToolRequest>,
    response_id: Option<String>,
    usage: Option<Value>,
    failed: bool,
}

impl CodexResponseSplitter {
    fn new(thread_id: &str, enabled: bool) -> Self {
        Self {
            enabled,
            stream_epoch: codex_generated_identity_epoch(thread_id),
            next_ordinal: 1,
            open: None,
            closed: VecDeque::new(),
            provider_typed_tool_item_ids: HashSet::new(),
            execution_only_typed_tool_item_ids: HashSet::new(),
            execution_only_typed_tool_owners: HashMap::new(),
            claimed_raw_tool_calls: HashSet::new(),
            typed_owned_call_ids: HashSet::new(),
            suppressed_raw_tool_requests: IndexMap::new(),
            completed_raw_tool_call_ids: HashSet::new(),
            pending_raw_tool_owners: IndexMap::new(),
            last_token_usage: None,
        }
    }

    /// The provider-response boundary for a thread with no raw events. The
    /// notification itself repeats — a resumed turn sent two consecutive ones
    /// carrying byte-identical totals — so its arrival means nothing and only
    /// movement in the reported usage marks a completed request.
    fn token_usage_boundary_reached(&mut self, usage: Option<&Value>) -> bool {
        if self.last_token_usage.as_ref() == usage {
            return false;
        }
        self.last_token_usage = usage.cloned();
        self.open.is_some()
    }

    fn ensure_open(
        &mut self,
        turn_id: Option<&str>,
    ) -> Option<(Option<CodexProviderResponseIdentity>, ChatMessageId)> {
        if !self.enabled {
            return None;
        }
        let opened = if self.open.is_none() {
            let identity = CodexProviderResponseIdentity {
                origin: CodexProviderResponseOrigin::IdlessProviderResponseItem,
                stream_epoch: self.stream_epoch,
                item_ordinal: self.next_ordinal,
            };
            self.next_ordinal = self.next_ordinal.saturating_add(1);
            self.open = Some(OpenCodexProviderResponse {
                identity: identity.clone(),
                turn_id: turn_id.unwrap_or("turn").to_owned(),
                response_id: None,
                text: String::new(),
                reasoning: String::new(),
                item_text: HashMap::new(),
                item_reasoning: HashMap::new(),
                typed_item_ids: HashSet::new(),
                raw_item_ids: HashSet::new(),
                raw_call_ids: HashSet::new(),
                pending_typed_tool_item_id: None,
                pending_typed_tool_call_id: None,
                tool_requests: Vec::new(),
                raw_tool_requests: Vec::new(),
            });
            Some(identity)
        } else {
            None
        };
        let message_id = self
            .open
            .as_ref()
            .expect("Codex response splitter opened a response")
            .identity
            .message_id();
        Some((opened, message_id))
    }

    fn observe_typed_item_started(
        &mut self,
        turn_id: Option<&str>,
        item_id: Option<&str>,
        call_id: Option<&str>,
        item_type: &str,
    ) -> Option<(Option<CodexProviderResponseIdentity>, ChatMessageId)> {
        let tool_item = is_codex_provider_tool_item_type(item_type);
        if tool_item {
            let item_id = item_id.filter(|item_id| !item_id.trim().is_empty())?;
            // A typed item owns its call from the moment it starts, whether or
            // not a raw declaration has been seen yet. The raw one can arrive
            // after this item's card has completed and its response closed —
            // measured, an MCP call whose typed card completed, then a raw
            // declaration for the same call id one response later — and
            // without the ownership recorded here that late declaration opened
            // a second card that nothing ever completes.
            self.typed_owned_call_ids.insert(item_id.to_owned());
            if let Some(call_id) = call_id.filter(|call_id| !call_id.trim().is_empty()) {
                self.typed_owned_call_ids.insert(call_id.to_owned());
            }
            let owner = self.claim_raw_tool_owner(item_id, call_id);
            tracing::debug!(
                probe = "typed",
                item_id,
                call_id = call_id.unwrap_or("<none>"),
                item_type,
                claimed_owner = owner.is_some(),
                "PROBE typed item recorded"
            );
            if let Some(owner) = owner {
                // The typed item's own id *is* the provider `call_id` the raw
                // output will carry, but record the owner's copy too for the
                // shapes where the two differ.
                self.typed_owned_call_ids.insert(item_id.to_owned());
                if let Some(call_id) = owner.provider_call_id.clone() {
                    self.typed_owned_call_ids.insert(call_id);
                }
                // The raw declaration parked as this item's owner describes
                // the same call the typed item does, so it must not also be
                // buffered as a card of its own. Declaring it and claiming it
                // are two different jobs; only the drop was ever meant to go.
                if let Some(response) = self.open.as_mut() {
                    response
                        .raw_tool_requests
                        .retain(|raw| raw.tool_call_id != owner.tool_call_id);
                }
                self.execution_only_typed_tool_item_ids
                    .insert(item_id.to_owned());
                self.execution_only_typed_tool_owners
                    .insert(item_id.to_owned(), owner);
                return None;
            }
            let Some(response) = self.open.as_mut() else {
                self.execution_only_typed_tool_item_ids
                    .insert(item_id.to_owned());
                return None;
            };
            let matches_raw = response.raw_item_ids.contains(item_id)
                || response.raw_call_ids.contains(item_id)
                || call_id.is_some_and(|call_id| {
                    response.raw_item_ids.contains(call_id)
                        || response.raw_call_ids.contains(call_id)
                });
            if !matches_raw {
                self.execution_only_typed_tool_item_ids
                    .insert(item_id.to_owned());
                return None;
            }
            response.typed_item_ids.insert(item_id.to_owned());
            response.pending_typed_tool_item_id = Some(item_id.to_owned());
            response.pending_typed_tool_call_id = call_id
                .filter(|call_id| !call_id.trim().is_empty())
                .map(str::to_owned);
            self.provider_typed_tool_item_ids.insert(item_id.to_owned());
            return Some((None, response.identity.message_id()));
        }
        let opened = self.ensure_open(turn_id)?;
        if let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) {
            let response = self.open.as_mut().expect("open Codex response");
            response.typed_item_ids.insert(item_id.to_owned());
        }
        Some(opened)
    }

    fn claim_raw_tool_owner(
        &mut self,
        item_id: &str,
        call_id: Option<&str>,
    ) -> Option<BufferedCodexToolRequest> {
        let matches_identity = |owner: &&BufferedCodexToolRequest| {
            [Some(item_id), call_id]
                .into_iter()
                .flatten()
                .any(|identity| {
                    owner.provider_item_id.as_deref() == Some(identity)
                        || owner.provider_call_id.as_deref() == Some(identity)
                })
        };
        let mut owners = self
            .open
            .as_ref()
            .into_iter()
            .flat_map(|response| response.raw_tool_requests.iter())
            .chain(self.pending_raw_tool_owners.values())
            .filter(matches_identity)
            .filter(|owner| !self.claimed_raw_tool_calls.contains(&owner.tool_call_id))
            .cloned();
        let owner = owners.next()?;
        if owners.next().is_some() {
            return None;
        }
        self.claimed_raw_tool_calls
            .insert(owner.tool_call_id.clone());
        Some(owner)
    }

    fn observe_raw_item(
        &mut self,
        turn_id: Option<&str>,
        item: &Value,
    ) -> Option<(Option<CodexProviderResponseIdentity>, ChatMessageId)> {
        if !is_raw_codex_provider_output_item(item) {
            return None;
        }
        let opened = self.ensure_open(turn_id)?;
        let Some(item_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|item_id| !item_id.trim().is_empty())
        else {
            return Some(opened);
        };
        let response = self.open.as_mut().expect("open Codex response");
        if !response.raw_item_ids.insert(item_id.to_owned()) {
            return Some(opened);
        }
        if let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("callId"))
            .and_then(Value::as_str)
            .filter(|call_id| !call_id.trim().is_empty())
        {
            response.raw_call_ids.insert(call_id.to_owned());
        }
        // A raw declaration owns a card only when nothing else renders the
        // call. Where a typed item exists it stays the owner — it carries the
        // status, exit code and output, and matching the two id spaces (which
        // share no key) is the guesswork this avoids. The call ids are recorded
        // as typed-owned so the raw output answering them is known to belong to
        // the typed item's card rather than reported as a lost result.
        //
        // Where no typed item exists the raw declaration is all there is, and
        // discarding it dropped the call outright — see
        // `codex_raw_call_is_rendered_elsewhere`.
        if let Some(request) = raw_codex_tool_request(item_id, item) {
            let typed_owns_call = self.typed_item_owns_call(item_id)
                || request
                    .provider_call_id
                    .as_deref()
                    .is_some_and(|call_id| self.typed_item_owns_call(call_id));
            tracing::debug!(
                probe = "raw",
                item_id,
                provider_call_id = request.provider_call_id.as_deref().unwrap_or("<none>"),
                tool_call_id = request.tool_call_id,
                tool_name = request.tool_name,
                typed_owns_call,
                "PROBE raw declaration"
            );
            if typed_owns_call {
                self.typed_owned_call_ids.insert(item_id.to_owned());
                if let Some(call_id) = request.provider_call_id {
                    self.typed_owned_call_ids.insert(call_id);
                }
            } else if codex_raw_call_is_rendered_elsewhere(&request.tool_name, &request.arguments) {
                if let Some(call_id) = request.provider_call_id.clone() {
                    self.suppressed_raw_tool_requests.insert(
                        call_id,
                        BufferedCodexToolRequest {
                            turn_id: turn_id.map(str::to_owned),
                            ..request
                        },
                    );
                    while self.suppressed_raw_tool_requests.len() > 256 {
                        self.suppressed_raw_tool_requests.shift_remove_index(0);
                    }
                }
            } else {
                let request = BufferedCodexToolRequest {
                    turn_id: turn_id.map(str::to_owned),
                    ..request
                };
                let response = self.open.as_mut().expect("open Codex response");
                if !response
                    .raw_tool_requests
                    .iter()
                    .any(|existing| existing.tool_call_id == request.tool_call_id)
                {
                    response.raw_tool_requests.push(request);
                }
            }
        }
        Some(opened)
    }

    /// Take the provider's id for the response now open.
    ///
    /// `rawResponse/completed` is the only event that carries it — and it is
    /// *not* the end of the response. Codex reports the typed item for every
    /// tool the response called after it: measured against 0.146.0, 73ms later
    /// for an instant command and 202ms for one that sleeps 8s, and the same
    /// for a file edit. Closing here left nothing open when the tool arrived,
    /// so `buffer_tool_request` opened a second, id-less response for it and
    /// every tool became its own chat message — billed a second time with the
    /// request usage its other half already reported.
    ///
    /// The response ends at `thread/tokenUsage/updated`, which lands after the
    /// last of those items, and which is the boundary a resumed thread already
    /// used for want of any raw events.
    fn observe_raw_response_completed(&mut self, turn_id: Option<&str>, response_id: String) {
        let Some(response) = self.open.as_mut() else {
            return;
        };
        if response.turn_id == "turn"
            && let Some(turn_id) = turn_id
        {
            response.turn_id = turn_id.to_owned();
        }
        response.response_id = Some(response_id);
    }

    fn observe_delta(
        &mut self,
        turn_id: Option<&str>,
        item_id: Option<&str>,
        delta: &str,
        reasoning: bool,
    ) -> Option<CodexResponseDelta> {
        self.ensure_open(turn_id)?;
        let response = self.open.as_mut().expect("open Codex response");
        let item_id = item_id
            .filter(|item_id| !item_id.trim().is_empty())
            .unwrap_or(if reasoning {
                "<idless-reasoning>"
            } else {
                "<idless-text>"
            });
        response.typed_item_ids.insert(item_id.to_owned());
        if reasoning {
            response
                .item_reasoning
                .entry(item_id.to_owned())
                .or_default()
                .push_str(delta);
            response.reasoning.push_str(delta);
        } else {
            response
                .item_text
                .entry(item_id.to_owned())
                .or_default()
                .push_str(delta);
            response.text.push_str(delta);
        }
        Some(CodexResponseDelta {
            delta: delta.to_owned(),
        })
    }

    fn observe_item_completed(
        &mut self,
        turn_id: Option<&str>,
        item_id: Option<&str>,
        completed: &str,
        reasoning: bool,
    ) -> Option<CodexResponseDelta> {
        if !contains_non_whitespace(completed)
            && self.open.as_ref().is_none_or(|response| {
                !contains_non_whitespace(&response.text)
                    && !contains_non_whitespace(&response.reasoning)
            })
        {
            return None;
        }
        self.ensure_open(turn_id)?;
        let response = self.open.as_mut().expect("open Codex response");
        let item_id = item_id
            .filter(|item_id| !item_id.trim().is_empty())
            .unwrap_or(if reasoning {
                "<idless-reasoning>"
            } else {
                "<idless-text>"
            });
        response.typed_item_ids.insert(item_id.to_owned());
        let observed = if reasoning {
            response
                .item_reasoning
                .entry(item_id.to_owned())
                .or_default()
        } else {
            response.item_text.entry(item_id.to_owned()).or_default()
        };
        let missing = completed
            .strip_prefix(observed.as_str())
            .unwrap_or_default()
            .to_owned();
        if missing.is_empty() {
            return Some(CodexResponseDelta {
                delta: String::new(),
            });
        }
        observed.push_str(&missing);
        if reasoning {
            response.reasoning.push_str(&missing);
        } else {
            response.text.push_str(&missing);
        }
        Some(CodexResponseDelta { delta: missing })
    }

    fn buffer_tool_request(
        &mut self,
        turn_id: Option<&str>,
        tool_call_id: &str,
        tool_name: &str,
        arguments: Value,
        tool_type: Value,
    ) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        // Opens the response rather than requiring one: a resumed thread gets no
        // raw events, so nothing else has opened it, and bailing here is what
        // sent every tool down the one-card-per-tool path.
        self.ensure_open(turn_id)?;
        let response = self.open.as_mut().expect("open Codex response");
        if response.turn_id == "turn"
            && let Some(turn_id) = turn_id
        {
            response.turn_id = turn_id.to_owned();
        }
        let content_offset = u32::try_from(response.text.chars().count()).unwrap_or(u32::MAX);
        let provider_item_id = response.pending_typed_tool_item_id.clone();
        let provider_call_id = response.pending_typed_tool_call_id.clone();
        response.raw_tool_requests.retain(|raw| {
            let raw_item_id = raw.provider_item_id.as_deref().unwrap_or_default();
            let typed_matches_raw_item = provider_item_id.as_ref().is_some_and(|typed_item_id| {
                typed_item_id == raw_item_id || raw.provider_call_id.as_ref() == Some(typed_item_id)
            });
            let typed_call_matches_raw = provider_call_id.as_ref().is_some_and(|typed_call_id| {
                typed_call_id == raw_item_id || raw.provider_call_id.as_ref() == Some(typed_call_id)
            });
            !typed_matches_raw_item && !typed_call_matches_raw
        });
        if let Some(existing) = response
            .tool_requests
            .iter_mut()
            .find(|request| request.tool_call_id == tool_call_id)
        {
            existing.turn_id = turn_id.map(str::to_owned);
            existing.provider_item_id = provider_item_id;
            existing.provider_call_id = provider_call_id;
            existing.tool_call_id = tool_call_id.to_owned();
            existing.tool_name = tool_name.to_owned();
            existing.arguments = arguments;
            existing.tool_type = tool_type;
            return Some(*existing.content_offset.get_or_insert(content_offset));
        }
        response.tool_requests.push(BufferedCodexToolRequest {
            turn_id: turn_id.map(str::to_owned),
            provider_item_id,
            provider_call_id,
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            arguments,
            tool_type,
            content_offset: Some(content_offset),
        });
        Some(content_offset)
    }

    fn finalize(
        &mut self,
        turn_id: Option<&str>,
        usage: Option<Value>,
        failed: bool,
    ) -> Option<FinalizedCodexProviderResponse> {
        let mut response = self.open.take()?;
        if response.turn_id == "turn"
            && let Some(turn_id) = turn_id
        {
            response.turn_id = turn_id.to_owned();
        }
        // Off the response, not the closing event: only `rawResponse/completed`
        // ever carries an id, and that is no longer the event that closes it.
        let response_id = response.response_id;
        let mut tool_requests = response.tool_requests;
        for raw in response.raw_tool_requests {
            if !tool_requests.iter().any(|request| {
                request.tool_call_id == raw.tool_call_id
                    || request.provider_item_id == raw.provider_item_id
                    || request.provider_item_id == raw.provider_call_id
                    || request.provider_call_id == raw.provider_item_id
                    || (request.provider_call_id.is_some()
                        && request.provider_call_id == raw.provider_call_id)
            }) {
                tool_requests.push(raw);
            }
        }
        // Park *every* owner under its provider `call_id`. Codex reports one
        // shell tool twice: a raw `function_call` item (`id: fc_…`, `call_id:
        // call_…`) which is what declares the card below, and a
        // `commandExecution` item whose `id` *is* that `call_id`. The
        // execution routinely starts after this response has finalized, and
        // once `self.open` is gone this map is the only place its owner can
        // still be found.
        //
        // This used to be gated on a `raw_completes_tool` flag that meant
        // `item_type == "custom_tool_call"`, so `function_call` owners — every
        // shell command — were dropped here. The execution then found nothing,
        // minted a *second* card for the same command and completed that one,
        // leaving the declared card open until the idle sweep cancelled it.
        for request in &tool_requests {
            if !failed && let Some(call_id) = request.provider_call_id.as_ref() {
                if !self.completed_raw_tool_call_ids.remove(call_id) {
                    self.pending_raw_tool_owners
                        .insert(call_id.clone(), request.clone());
                }
            } else if let Some(call_id) = request.provider_call_id.as_ref() {
                self.completed_raw_tool_call_ids.remove(call_id);
            }
        }
        let mut evicted_tool_requests = Vec::new();
        while self.pending_raw_tool_owners.len() > 256 {
            if let Some((_, owner)) = self.pending_raw_tool_owners.shift_remove_index(0) {
                self.claimed_raw_tool_calls.remove(&owner.tool_call_id);
                evicted_tool_requests.push(owner);
            }
        }
        let message_id = response.identity.message_id();
        self.closed.push_back(ClosedCodexProviderResponse {
            message_id: message_id.clone(),
            response_id: response_id.clone(),
            usage: usage.clone(),
            failed,
        });
        while self.closed.len() > 128 {
            self.closed.pop_front();
        }
        if let Some(record) = self.closed.back() {
            tracing::debug!(
                message_id = record.message_id.0,
                response_id = ?record.response_id,
                usage = ?record.usage,
                failed = record.failed,
                "Retained Codex response identity and exact usage"
            );
        }
        Some(FinalizedCodexProviderResponse {
            message_id,
            turn_id: response.turn_id,
            content: response.text,
            reasoning: contains_non_whitespace(&response.reasoning).then_some(response.reasoning),
            tool_requests,
            evicted_tool_requests,
            response_id,
            usage,
            failed,
        })
    }

    fn take_execution_only_typed_tool_owner(
        &mut self,
        item_id: Option<&str>,
    ) -> Option<BufferedCodexToolRequest> {
        let item_id = item_id.filter(|item_id| !item_id.trim().is_empty())?;
        self.execution_only_typed_tool_owners.get(item_id).cloned()
    }

    fn finish_typed_tool(
        &mut self,
        item_id: Option<&str>,
    ) -> Option<(bool, Option<BufferedCodexToolRequest>)> {
        let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) else {
            return Some((false, None));
        };
        let execution_only = self.execution_only_typed_tool_item_ids.remove(item_id);
        let owner = self.execution_only_typed_tool_owners.remove(item_id);
        let provider_owned = self.provider_typed_tool_item_ids.remove(item_id);
        (execution_only || provider_owned).then_some((provider_owned, owner))
    }

    fn raw_tool_owner(&self, call_id: &str) -> Option<BufferedCodexToolRequest> {
        self.pending_raw_tool_owners.get(call_id).cloned()
    }

    /// The card a raw output belongs to, including one the open response has
    /// declared but not yet published.
    ///
    /// Only for completing that output. A `write_stdin` output arrives about
    /// 2ms *before* the `thread/tokenUsage/updated` that closes the response,
    /// so its owner is not parked yet and the card would stay open until the
    /// idle sweep cancelled it. Deliberately not folded into
    /// [`Self::raw_tool_owner`]: that one also answers "which command execution
    /// yielded this session", and an interaction with a running process is not
    /// a command execution — widening it there made the yielded-session
    /// correlation ambiguous and reported every poll as an uncorrelated
    /// session.
    fn raw_tool_owner_for_completion(&self, call_id: &str) -> Option<BufferedCodexToolRequest> {
        self.raw_tool_owner(call_id).or_else(|| {
            self.open.as_ref().and_then(|response| {
                response
                    .raw_tool_requests
                    .iter()
                    .find(|owner| owner.provider_call_id.as_deref() == Some(call_id))
                    .cloned()
            })
        })
    }

    fn pending_raw_owner_count_for_turn(&self, turn_id: &str) -> usize {
        self.pending_raw_tool_owners
            .values()
            .filter(|owner| owner.turn_id.as_deref() == Some(turn_id))
            .count()
    }

    fn claim_raw_tool_call(&mut self, tool_call_id: &str) {
        self.claimed_raw_tool_calls.insert(tool_call_id.to_owned());
    }

    fn typed_item_owns_call(&self, call_id: &str) -> bool {
        self.typed_owned_call_ids.contains(call_id)
    }

    fn suppressed_raw_tool_request(&self, call_id: &str) -> Option<BufferedCodexToolRequest> {
        self.suppressed_raw_tool_requests.get(call_id).cloned()
    }

    fn remove_suppressed_raw_tool_request(
        &mut self,
        call_id: &str,
    ) -> Option<BufferedCodexToolRequest> {
        self.suppressed_raw_tool_requests.shift_remove(call_id)
    }

    fn suppressed_web_search_query(&self) -> Option<String> {
        let mut queries = self
            .suppressed_raw_tool_requests
            .values()
            .filter_map(|request| request.arguments.as_str())
            .filter_map(codex_web_search_query_from_source);
        let query = queries.next()?;
        queries.next().is_none().then_some(query)
    }

    fn remove_raw_tool_owner(&mut self, call_id: &str) -> Option<BufferedCodexToolRequest> {
        let owner = self.pending_raw_tool_owners.shift_remove(call_id);
        if let Some(owner) = owner.as_ref() {
            self.claimed_raw_tool_calls.remove(&owner.tool_call_id);
        }
        owner
    }

    fn complete_raw_tool_call(&mut self, call_id: &str) {
        if self.remove_raw_tool_owner(call_id).is_none() {
            self.completed_raw_tool_call_ids.insert(call_id.to_owned());
        }
    }

    fn remove_raw_tool_owner_by_tool_call_id(&mut self, tool_call_id: &str) {
        let call_id = self
            .pending_raw_tool_owners
            .iter()
            .find(|(_, owner)| owner.tool_call_id == tool_call_id)
            .map(|(call_id, _)| call_id.clone());
        if let Some(call_id) = call_id {
            self.remove_raw_tool_owner(&call_id);
        }
    }
}

impl ActiveStreamState {
    fn is_replaceable_provider_reservation(&self) -> bool {
        self.generated_identity.is_none()
            && !self.stream_published
            && self.text.is_empty()
            && self.reasoning.is_empty()
            && self.images.is_empty()
    }
}

struct InterruptedPublishedStream {
    response: ResponseHandle,
    content: String,
    reasoning: Option<String>,
    images: Vec<ImageData>,
}

#[derive(Clone)]
struct CompletedCodexAgentMessage {
    reported_text: String,
    reported_reasoning: Option<String>,
    completion_text: String,
    completion_reasoning: Option<String>,
}

impl CompletedCodexAgentMessage {
    fn matches_replay(&self, text: &str, reasoning: &Option<String>) -> bool {
        (self.reported_text == text && &self.reported_reasoning == reasoning)
            || (self.completion_text == text && &self.completion_reasoning == reasoning)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProviderItemKind {
    AgentMessage,
    Reasoning,
}

enum CodexProviderStreamFinalization {
    Completed {
        text: String,
        reasoning: Option<String>,
    },
    Superseded,
    TurnAborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProviderOpenCause {
    ItemStarted,
    Delta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexProviderItemDisposition {
    Superseded,
    TurnTerminated,
    Completed,
}

struct CodexProviderItemTombstone {
    owner_thread_id: String,
    turn_id: String,
    message_id: ChatMessageId,
    kind: CodexProviderItemKind,
    disposition: CodexProviderItemDisposition,
    accepted_text: String,
    accepted_reasoning: String,
    late_text: String,
    late_reasoning: String,
    late_event_count: u8,
    late_bytes: usize,
}

#[derive(Clone)]
struct TerminatedCodexTurn {
    turn_id: String,
}

enum CodexLateProviderEvent {
    Delta {
        kind: CodexProviderItemKind,
        content: String,
    },
    Completion {
        kind: CodexProviderItemKind,
        text: String,
        reasoning: Option<String>,
    },
    Started {
        kind: CodexProviderItemKind,
    },
}

enum CodexLateProviderEventOutcome {
    NotFound,
    Absorbed {
        first: bool,
        turn_id: String,
        disposition: CodexProviderItemDisposition,
    },
    Contradiction {
        affected_turn_is_live: bool,
        turn_id: String,
        disposition: CodexProviderItemDisposition,
    },
}

fn push_codex_provider_item_tombstone(
    tombstones: &mut VecDeque<CodexProviderItemTombstone>,
    tombstone: CodexProviderItemTombstone,
) {
    if let Some(index) = tombstones.iter().position(|existing| {
        existing.owner_thread_id == tombstone.owner_thread_id
            && existing.message_id == tombstone.message_id
    }) {
        tombstones.remove(index);
    }
    tombstones.push_back(tombstone);
    while tombstones.len() > MAX_CODEX_PROVIDER_ITEM_TOMBSTONES {
        if let Some(evicted) = tombstones.pop_front() {
            // Completed tombstones are pushed on every normal finalize, so
            // their eviction is routine churn; the loss-recording dispositions
            // stay loud.
            if evicted.disposition == CodexProviderItemDisposition::Completed {
                tracing::debug!(
                    owner_thread_id = evicted.owner_thread_id,
                    turn_id = evicted.turn_id,
                    provider_item_id = evicted.message_id.0,
                    disposition = ?evicted.disposition,
                    "Evicted bounded Codex provider-item tombstone"
                );
            } else {
                tracing::warn!(
                    owner_thread_id = evicted.owner_thread_id,
                    turn_id = evicted.turn_id,
                    provider_item_id = evicted.message_id.0,
                    disposition = ?evicted.disposition,
                    "Evicted bounded Codex provider-item tombstone"
                );
            }
        }
    }
}

fn push_codex_terminated_turn(turns: &mut VecDeque<TerminatedCodexTurn>, turn_id: String) -> bool {
    if turns.iter().any(|turn| turn.turn_id == turn_id) {
        return false;
    }
    turns.push_back(TerminatedCodexTurn { turn_id });
    while turns.len() > MAX_CODEX_TERMINATED_TURNS {
        turns.pop_front();
    }
    true
}

fn classify_codex_late_provider_event(
    tombstones: &mut VecDeque<CodexProviderItemTombstone>,
    owner_thread_id: &str,
    active_turn_id: Option<&str>,
    message_id: &ChatMessageId,
    event: &CodexLateProviderEvent,
) -> CodexLateProviderEventOutcome {
    let Some(tombstone) = tombstones.iter_mut().rev().find(|tombstone| {
        tombstone.owner_thread_id == owner_thread_id && tombstone.message_id == *message_id
    }) else {
        return CodexLateProviderEventOutcome::NotFound;
    };
    // Duplicate completions of a normally-completed item stay with the
    // completed_agent_messages replay check: it still holds the reported
    // payload variants a tombstone does not retain, so idempotent replays
    // stay quiet and conflicting ones stay loud.
    if tombstone.disposition == CodexProviderItemDisposition::Completed
        && matches!(event, CodexLateProviderEvent::Completion { .. })
    {
        return CodexLateProviderEventOutcome::NotFound;
    }
    let (kind, event_bytes, compatible) = match event {
        CodexLateProviderEvent::Delta { kind, content } => {
            (*kind, content.len(), *kind == tombstone.kind)
        }
        CodexLateProviderEvent::Completion {
            kind,
            text,
            reasoning,
        } => {
            let compatible = if *kind != tombstone.kind {
                false
            } else {
                match kind {
                    CodexProviderItemKind::AgentMessage => {
                        text.starts_with(&tombstone.accepted_text)
                    }
                    CodexProviderItemKind::Reasoning => true,
                }
            };
            (
                *kind,
                text.len()
                    .saturating_add(reasoning.as_ref().map_or(0, String::len)),
                compatible,
            )
        }
        CodexLateProviderEvent::Started { kind } => (
            *kind,
            0,
            // A replayed start of a normally-completed item is a benign
            // straggler; for loss-recording dispositions a restart still
            // contradicts the recorded loss.
            *kind == tombstone.kind
                && tombstone.disposition == CodexProviderItemDisposition::Completed,
        ),
    };
    let affected_turn_is_live = active_turn_id == Some(tombstone.turn_id.as_str());
    let would_exceed_bounds = tombstone.late_event_count >= MAX_CODEX_LATE_SUPERSEDED_EVENTS
        || tombstone.late_bytes.saturating_add(event_bytes) > MAX_CODEX_LATE_SUPERSEDED_BYTES;
    if !compatible || would_exceed_bounds {
        return CodexLateProviderEventOutcome::Contradiction {
            affected_turn_is_live,
            turn_id: tombstone.turn_id.clone(),
            disposition: tombstone.disposition,
        };
    }
    let first = tombstone.late_event_count == 0;
    tombstone.late_event_count = tombstone.late_event_count.saturating_add(1);
    tombstone.late_bytes = tombstone.late_bytes.saturating_add(event_bytes);
    if let CodexLateProviderEvent::Delta { content, .. } = event {
        match kind {
            CodexProviderItemKind::AgentMessage => tombstone.late_text.push_str(content),
            CodexProviderItemKind::Reasoning => tombstone.late_reasoning.push_str(content),
        }
    }
    CodexLateProviderEventOutcome::Absorbed {
        first,
        turn_id: tombstone.turn_id.clone(),
        disposition: tombstone.disposition,
    }
}

struct FinalizedCodexProviderItem {
    turn_id: String,
    message_id: ChatMessageId,
    kind: CodexProviderItemKind,
    content: String,
    reasoning: Option<String>,
    emitted: bool,
}

enum CodexAgentMessageOpen {
    Open,
    Existing,
    Retired,
    Terminal,
    Foreign,
    Superseded(Box<ActiveStreamState>),
}

enum CodexSubAgentMessageOpen {
    Open,
    Existing,
    Retired,
    Terminal,
    Foreign,
    Superseded(Box<FinalizedCodexSubAgentProviderItem>),
}

struct CodexProviderNotificationOwner<'a> {
    thread_id: Option<&'a str>,
    turn_id: Option<&'a str>,
}

#[derive(Clone)]
struct PendingCodexMessageMetadata {
    turn_id: String,
    message_id: ChatMessageId,
    model: String,
}

#[derive(Clone, Default)]
struct CodexTurnTokenUsage {
    request_count: u32,
    latest_request: Option<TokenUsage>,
    turn: TokenUsage,
    cumulative: Option<TokenUsage>,
    model_context_window: Option<u64>,
}

struct CodexSubAgentStream {
    emitter: Arc<TurnEmitter>,
    agent_id: protocol::AgentId,
    spawn_item_id: String,
    activity_item_id: Option<String>,
    agent_path: String,
    agent_name: String,
    name_update_tx: Option<mpsc::UnboundedSender<String>>,
    sender_thread_id: String,
    active_turn_id: Option<String>,
    current_message_id: Option<ChatMessageId>,
    current_generated_identity: Option<CodexProviderResponseIdentity>,
    current_reasoning_only: bool,
    current_stream_published: bool,
    current_response: Option<ResponseHandle>,
    current_text: String,
    current_reasoning: String,
    current_tool_call_ids: Vec<String>,
    current_images: Vec<ImageData>,
    tool_container: Option<ChatMessageId>,
    pending_tool_call_ids: HashSet<String>,
    tool_container_images: Vec<protocol::ImageData>,
    completed_agent_messages: HashMap<ChatMessageId, CompletedCodexAgentMessage>,
    retired_unpublished_message_ids: HashSet<ChatMessageId>,
    provider_supersessions_this_turn: u8,
    supersession_warning_emitted: bool,
    provider_item_tombstones: VecDeque<CodexProviderItemTombstone>,
    terminated_turns: VecDeque<TerminatedCodexTurn>,
    terminated_turn_awaiting_replacement: Option<String>,
    pending_spawn_terminal_status: Option<String>,
    background_work_failed: bool,
    generated_identity_epoch: u64,
    next_generated_identity_ordinal: u64,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
    token_usage_by_turn: HashMap<String, Value>,
    model_token_usage_by_turn: HashMap<String, CodexTurnTokenUsage>,
    provider_usage_baseline: Option<TokenUsage>,
}

impl CodexSubAgentStream {
    fn has_replaceable_provider_reservation(&self) -> bool {
        self.current_message_id.is_some()
            && self.current_generated_identity.is_none()
            && !self.current_stream_published
            && self.current_text.is_empty()
            && self.current_reasoning.is_empty()
            && self.current_images.is_empty()
    }

    fn retire_replaceable_provider_reservation(&mut self) -> bool {
        if !self.has_replaceable_provider_reservation() {
            return false;
        }
        let message_id = self
            .current_message_id
            .take()
            .expect("replaceable child reservation has an id");
        self.retired_unpublished_message_ids.insert(message_id);
        self.current_generated_identity = None;
        self.current_reasoning_only = false;
        self.current_stream_published = false;
        self.current_response = None;
        self.current_text.clear();
        self.current_reasoning.clear();
        self.current_tool_call_ids.clear();
        self.current_images.clear();
        true
    }
}

struct CompletedCodexSubAgentStream {
    emitter: Arc<TurnEmitter>,
    agent_id: protocol::AgentId,
    spawn_item_id: String,
    activity_item_id: Option<String>,
    agent_path: String,
    agent_name: String,
    name_update_tx: Option<mpsc::UnboundedSender<String>>,
    sender_thread_id: String,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
    model_token_usage_by_turn: HashMap<String, CodexTurnTokenUsage>,
    provider_usage_baseline: Option<TokenUsage>,
    provider_item_tombstones: VecDeque<CodexProviderItemTombstone>,
    owner_terminated: bool,
}

struct FinalizedCodexSubAgentProviderItem {
    emitter: Arc<TurnEmitter>,
    turn_id: String,
    message_id: ChatMessageId,
    kind: CodexProviderItemKind,
    response: Option<ResponseHandle>,
    emitted: bool,
    content: String,
    reasoning: Option<String>,
    token_usage: Option<Value>,
    unavailable_reason: Option<TokenUsageUnavailableReason>,
    images: Vec<ImageData>,
}

fn completed_codex_subagent_stream(
    stream: CodexSubAgentStream,
    owner_terminated: bool,
) -> CompletedCodexSubAgentStream {
    CompletedCodexSubAgentStream {
        emitter: stream.emitter,
        agent_id: stream.agent_id,
        spawn_item_id: stream.spawn_item_id,
        activity_item_id: stream.activity_item_id,
        agent_path: stream.agent_path,
        agent_name: stream.agent_name,
        name_update_tx: stream.name_update_tx,
        sender_thread_id: stream.sender_thread_id,
        pending_message_metadata: stream.pending_message_metadata,
        model_token_usage_by_turn: stream.model_token_usage_by_turn,
        provider_usage_baseline: stream.provider_usage_baseline,
        provider_item_tombstones: stream.provider_item_tombstones,
        owner_terminated,
    }
}

#[derive(Clone)]
struct CodexSubAgentSpawnInfo {
    item_id: String,
    tool_name: String,
    name: String,
    prompt: Option<String>,
    agent_type: String,
    receiver_thread_id: String,
    sender_thread_id: String,
}

struct CodexSubAgentActivity {
    item_id: Option<String>,
    agent_thread_id: String,
    agent_path: String,
    kind: String,
}

/// A command execution Codex reports as a live background terminal.
struct CodexBackgroundCommand {
    tool_call_id: String,
    task_id: String,
    description: Option<String>,
}

struct CodexBackgroundWake {
    tool_call_id: String,
    task_id: String,
    description: Option<String>,
    exit_code: i32,
    output: String,
}

struct CodexBackgroundCommandResult {
    task_id: String,
    description: Option<String>,
    exit_code: i32,
    output: String,
}

/// A `commandExecution` item between `item/started` and `item/completed`.
///
/// Every command is tracked until a yielded unified-exec result proves that
/// root work escaped into a live session, or the item completes.
#[derive(Clone)]
struct CodexOutstandingCommand {
    tool_call_id: String,
    command: Option<String>,
    process_id: Option<String>,
    turn_id: String,
}

#[derive(Clone)]
struct CodexUnownedCommand {
    command: Option<String>,
    process_id: String,
    turn_id: String,
}

/// A tool outcome that arrived before its card existed.
///
/// Codex does not order a `commandExecution`'s `item/completed` against the
/// `rawResponse/completed` that declares the owning card, so a fast command
/// (`printf`, `cat`) can finish first. Emitting then is a
/// `completion_without_request` violation; dropping it leaves the card
/// spinning until the idle sweep cancels it. Hold the outcome here and flush
/// it in `finalize_strict_response` once the card has been declared.
struct CodexDeferredToolCompletion {
    thread_id: String,
    tool_call_id: String,
    outcome: ToolExecutionOutcome,
}

enum CodexUnlinkedRawToolResolution {
    OrdinaryCompletion,
    Correlated(String),
    Failed,
}

/// One row of a `thread/backgroundTerminals/list` result.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexBackgroundTerminalRow {
    item_id: String,
    process_id: String,
    command: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct CodexRawContractDriftWarning {
    thread_id: String,
    observed_notification_methods: Vec<String>,
    methods_truncated: bool,
}

struct CodexCommandTermination {
    thread_id: String,
    tool_call_id: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CodexCodeCellKey {
    thread_id: String,
    turn_id: String,
    runtime_cell_id: String,
}

impl CodexBackgroundCommand {
    fn progress(&self) -> ToolProgressData {
        ToolProgressData {
            tool_call_id: self.tool_call_id.clone(),
            execution_mode: ToolExecutionMode::Background,
            cancellable: true,
            update: ToolProgressUpdate::Other {
                payload: json!({
                    "task_id": self.task_id,
                    "description": self.description,
                }),
            },
        }
    }
}

enum CodexNotificationOwner {
    Parent {
        thread_id: String,
    },
    LiveChild {
        thread_id: String,
    },
    CompletedChild {
        thread_id: String,
    },
    Descendant {
        thread_id: String,
        ancestor_thread_id: String,
    },
    Unknown {
        thread_id: Option<String>,
    },
}

struct CodexState {
    thread_id: String,
    response_splitters: HashMap<String, CodexResponseSplitter>,
    pending_resume_thread_id: Option<String>,
    effective_model: Option<String>,
    model_override: Option<String>,
    reasoning_effort_override: Option<String>,
    approval_policy: Option<String>,
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
    turn_network_access: bool,
    active_turn_id: Option<String>,
    foreground_response_completed: bool,
    awaiting_root_turn_start: bool,
    /// Set when the user cancels while no root turn is tracked. A root turn
    /// that starts while this is set raced the cancel — the user has already
    /// been told the agent is idle, so that turn is interrupted and
    /// tombstoned instead of being adopted.
    interrupt_next_root_turn: bool,
    pending_compaction: Option<PendingCodexCompaction>,
    active_stream: Option<ActiveStreamState>,
    tool_container: Option<ChatMessageId>,
    notification_sequence: u64,
    completed_agent_messages: HashMap<ChatMessageId, CompletedCodexAgentMessage>,
    retired_unpublished_message_ids: HashSet<ChatMessageId>,
    provider_supersessions_this_turn: u8,
    supersession_warning_emitted: bool,
    provider_item_tombstones: VecDeque<CodexProviderItemTombstone>,
    terminated_turns: VecDeque<TerminatedCodexTurn>,
    terminated_turn_awaiting_replacement: Option<String>,
    generated_identity_epoch: u64,
    next_generated_identity_ordinal: u64,
    pending_tool_call_ids: HashSet<String>,
    tool_call_identities: CodexToolCallIdentities,
    background_commands: HashMap<(String, String), CodexBackgroundCommand>,
    background_command_results: HashMap<(String, String), Vec<CodexBackgroundCommandResult>>,
    background_command_owner_active: bool,
    /// `commandExecution` items that have started and not yet completed, keyed
    /// like `background_commands`. Codex holds the item open for as long as
    /// the process lives, so this is exactly the set of commands that could
    /// still be — or become — a background terminal.
    outstanding_command_executions: HashMap<(String, String), CodexOutstandingCommand>,
    unowned_command_executions: HashMap<(String, String), CodexUnownedCommand>,
    /// Outcomes that outran the response finalize which declares their card.
    /// Drained by `flush_deferred_tool_completions`.
    deferred_tool_completions: Vec<CodexDeferredToolCompletion>,
    /// Whether the background-terminal poll loop is already running.
    background_terminal_poll_active: bool,
    pending_background_wakes: VecDeque<CodexBackgroundWake>,
    background_wake_request_in_flight: bool,
    experimental_raw_events_requested: bool,
    raw_response_item_completed_seen: bool,
    first_background_list_thread_id: Option<String>,
    observed_notification_methods: HashSet<String>,
    raw_notification_methods_truncated: bool,
    raw_contract_drift_warned: bool,
    tool_container_images: Vec<protocol::ImageData>,
    cancelled_tool_call_ids: HashSet<String>,
    code_cell_tools: HashMap<CodexCodeCellKey, HashSet<String>>,
    code_cell_by_tool: HashMap<String, CodexCodeCellKey>,
    /// A provider turn reached `turn/completed` while explicitly foreground
    /// tool requests were still open. The presentation remains active until
    /// those real requests complete, unless another turn starts first and
    /// assumes ownership of the active presentation.
    close_active_stream_when_tools_idle: bool,
    pending_message_metadata: Option<PendingCodexMessageMetadata>,
    completed_message_metadata_by_turn: HashMap<String, PendingCodexMessageMetadata>,
    token_usage_by_turn: HashMap<String, Value>,
    model_token_usage_by_turn: HashMap<String, CodexTurnTokenUsage>,
    file_change_call_ids: HashMap<String, Vec<String>>,
    pending_raw_modify_calls: HashMap<(String, String), PendingRawCodexModify>,
    pending_request: Option<PendingRequest>,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
    capacity_refresh_in_flight: bool,
    pending_subagent_spawns: HashMap<String, CodexSubAgentSpawnInfo>,
    native_subagent_tool_call_ids: HashSet<String>,
    conflicting_subagent_threads: HashMap<String, String>,
    registering_subagent_threads: HashSet<String>,
    unknown_owner_notifications: HashSet<String>,
    descendant_owner_threads: HashMap<String, String>,
    subagent_streams: HashMap<String, CodexSubAgentStream>,
    completed_subagent_streams: HashMap<String, CompletedCodexSubAgentStream>,
}

fn initial_codex_state(
    thread_id: String,
    response_splitters: HashMap<String, CodexResponseSplitter>,
    model: Option<String>,
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
    turn_network_access: bool,
    subagent_emitter: Option<Arc<dyn SubAgentEmitter>>,
) -> CodexState {
    let generated_identity_epoch = codex_generated_identity_epoch(&thread_id);
    let strict_response_splitting = response_splitters
        .get(&thread_id)
        .is_some_and(|splitter| splitter.enabled);
    CodexState {
        thread_id,
        response_splitters,
        pending_resume_thread_id: None,
        effective_model: model,
        model_override: None,
        reasoning_effort_override: None,
        approval_policy: None,
        access_mode,
        execution_mode,
        turn_network_access,
        active_turn_id: None,
        foreground_response_completed: false,
        awaiting_root_turn_start: false,
        interrupt_next_root_turn: false,
        pending_compaction: None,
        active_stream: None,
        tool_container: None,
        notification_sequence: 0,
        completed_agent_messages: HashMap::new(),
        retired_unpublished_message_ids: HashSet::new(),
        provider_supersessions_this_turn: 0,
        supersession_warning_emitted: false,
        provider_item_tombstones: VecDeque::new(),
        terminated_turns: VecDeque::new(),
        terminated_turn_awaiting_replacement: None,
        generated_identity_epoch,
        next_generated_identity_ordinal: 1,
        pending_tool_call_ids: HashSet::new(),
        tool_call_identities: CodexToolCallIdentities::default(),
        background_commands: HashMap::new(),
        background_command_results: HashMap::new(),
        background_command_owner_active: true,
        outstanding_command_executions: HashMap::new(),
        unowned_command_executions: HashMap::new(),
        deferred_tool_completions: Vec::new(),
        background_terminal_poll_active: false,
        pending_background_wakes: VecDeque::new(),
        background_wake_request_in_flight: false,
        experimental_raw_events_requested: strict_response_splitting,
        raw_response_item_completed_seen: false,
        first_background_list_thread_id: None,
        observed_notification_methods: HashSet::new(),
        raw_notification_methods_truncated: false,
        raw_contract_drift_warned: false,
        tool_container_images: Vec::new(),
        cancelled_tool_call_ids: HashSet::new(),
        code_cell_tools: HashMap::new(),
        code_cell_by_tool: HashMap::new(),
        close_active_stream_when_tools_idle: false,
        pending_message_metadata: None,
        completed_message_metadata_by_turn: HashMap::new(),
        token_usage_by_turn: HashMap::new(),
        model_token_usage_by_turn: HashMap::new(),
        file_change_call_ids: HashMap::new(),
        pending_raw_modify_calls: HashMap::new(),
        pending_request: None,
        subagent_emitter,
        capacity_refresh_in_flight: false,
        pending_subagent_spawns: HashMap::new(),
        native_subagent_tool_call_ids: HashSet::new(),
        conflicting_subagent_threads: HashMap::new(),
        registering_subagent_threads: HashSet::new(),
        unknown_owner_notifications: HashSet::new(),
        descendant_owner_threads: HashMap::new(),
        subagent_streams: HashMap::new(),
        completed_subagent_streams: HashMap::new(),
    }
}

struct PendingCodexCompaction {
    request: BackendCompactionRequest,
    terminal_tx: Option<oneshot::Sender<BackendCompactionResult>>,
    accepted: bool,
    turn_id: Option<String>,
    item_id: Option<String>,
    item_started: bool,
    item_completed: bool,
    turn_status: Option<String>,
    deprecated_notification_seen: bool,
    started_at: std::time::Instant,
}

#[derive(Clone)]
struct PendingRawCodexModify {
    turn_id: String,
    tool_call_id: String,
    file_path: String,
    before: String,
    after: String,
    failed_output: Option<String>,
}

#[derive(Default)]
struct CodexToolCallIdentities {
    occurrence_counts: HashMap<(String, String, String), u64>,
    pending: HashMap<(String, String), VecDeque<CodexToolCallOccurrence>>,
    completed: HashMap<(String, String, String), String>,
}

struct CodexToolCallOccurrence {
    canonical_id: String,
    turn_id: String,
    tool_name: String,
}

impl CodexToolCallIdentities {
    fn started(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        provider_item_id: &str,
        tool_name: &str,
        base_canonical_id: &str,
    ) -> String {
        let key = (thread_id.to_owned(), provider_item_id.to_owned());
        let queue = self.pending.entry(key).or_default();
        if let Some(existing) = queue.back()
            && existing.turn_id == turn_id
            && existing.tool_name == tool_name
        {
            return existing.canonical_id.clone();
        }
        let occurrence = self
            .occurrence_counts
            .entry((
                thread_id.to_owned(),
                turn_id.to_owned(),
                provider_item_id.to_owned(),
            ))
            .or_default();
        *occurrence = occurrence.saturating_add(1);
        let canonical_id = if *occurrence == 1 {
            base_canonical_id.to_owned()
        } else {
            format!("{base_canonical_id}:occurrence-{occurrence}")
        };
        queue.push_back(CodexToolCallOccurrence {
            canonical_id: canonical_id.clone(),
            turn_id: turn_id.to_owned(),
            tool_name: tool_name.to_owned(),
        });
        canonical_id
    }

    fn completed(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        provider_item_id: &str,
        tool_name: &str,
        base_canonical_id: &str,
    ) -> String {
        let key = (thread_id.to_owned(), provider_item_id.to_owned());
        if let Some(queue) = self.pending.get_mut(&key) {
            let position = queue
                .iter()
                .position(|occurrence| {
                    occurrence.turn_id == turn_id && occurrence.tool_name == tool_name
                })
                .or_else(|| {
                    queue
                        .iter()
                        .position(|occurrence| occurrence.tool_name == tool_name)
                });
            if let Some(position) = position {
                let occurrence = queue
                    .remove(position)
                    .expect("pending Codex tool occurrence disappeared");
                if queue.is_empty() {
                    self.pending.remove(&key);
                }
                self.completed.insert(
                    (
                        thread_id.to_owned(),
                        provider_item_id.to_owned(),
                        tool_name.to_owned(),
                    ),
                    occurrence.canonical_id.clone(),
                );
                return occurrence.canonical_id;
            }
        }
        if let Some(canonical_id) = self.completed.get(&(
            thread_id.to_owned(),
            provider_item_id.to_owned(),
            tool_name.to_owned(),
        )) {
            return canonical_id.clone();
        }
        let canonical_id = self.started(
            thread_id,
            turn_id,
            provider_item_id,
            tool_name,
            base_canonical_id,
        );
        if let Some(queue) = self.pending.get_mut(&key)
            && let Some(occurrence) = queue.pop_back()
        {
            if queue.is_empty() {
                self.pending.remove(&key);
            }
            self.completed.insert(
                (
                    thread_id.to_owned(),
                    provider_item_id.to_owned(),
                    tool_name.to_owned(),
                ),
                occurrence.canonical_id.clone(),
            );
        }
        canonical_id
    }
}

struct CodexInner {
    rpc: CodexRpc,
    emitter: Arc<TurnEmitter>,
    state: Mutex<CodexState>,
    steering_tempfile: Option<std::path::PathBuf>,
    skill_projection: std::sync::Mutex<Option<CodexSkillProjection>>,
}
impl CodexInner {
    async fn begin_compaction(
        self: &Arc<Self>,
        request: BackendCompactionRequest,
    ) -> BackendCompactionStart {
        let capability = self
            .rpc
            .compaction_capability
            .lock()
            .expect("Codex compaction capability mutex poisoned")
            .clone();
        if let Some(start) = super::compaction::not_dispatched_for_capability(&capability) {
            return start;
        }

        let (terminal_tx, terminal) = oneshot::channel();
        let thread_id = {
            let mut state = self.state.lock().await;
            if state.pending_compaction.is_some() {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::AnotherCompactionActive,
                };
            }
            if state.active_turn_id.is_some()
                || state.active_stream.is_some()
                || state.pending_request.is_some()
            {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::ActiveTurn,
                };
            }
            if state.tool_container.is_some() || !state.pending_tool_call_ids.is_empty() {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::ToolLifecycleActive,
                };
            }
            if !state.background_commands.is_empty()
                || !state.pending_subagent_spawns.is_empty()
                || !state.registering_subagent_threads.is_empty()
                || !state.subagent_streams.is_empty()
            {
                return BackendCompactionStart::Deferred {
                    reason: BackendCompactionDeferredReason::BackgroundMutationActive,
                };
            }
            let thread_id = state.thread_id.clone();
            state.pending_compaction = Some(PendingCodexCompaction {
                request: request.clone(),
                terminal_tx: Some(terminal_tx),
                accepted: false,
                turn_id: None,
                item_id: None,
                item_started: false,
                item_completed: false,
                turn_status: None,
                deprecated_notification_seen: false,
                started_at: std::time::Instant::now(),
            });
            thread_id
        };

        self.emitter
            .compaction_event(&BackendCompactionEvent::Progress(
                BackendCompactionProgress {
                    operation_id: request.operation_id.clone(),
                    stage: CompactionStage::Dispatching,
                    elapsed_ms: None,
                },
            ));

        let response = self
            .rpc
            .request_typed(
                "thread/compact/start",
                json!({
                    "threadId": thread_id
                }),
            )
            .await;
        match response {
            Ok(value) if value.as_object().is_some_and(serde_json::Map::is_empty) => {
                if let Some(pending) = self.state.lock().await.pending_compaction.as_mut() {
                    pending.accepted = true;
                }
            }
            Ok(value) => {
                self.finish_compaction_failure(
                    BackendCompactionFailureKind::ProtocolViolation,
                    format!("Codex thread/compact/start returned an unexpected response: {value}"),
                )
                .await;
            }
            Err(error) => {
                let rejected_before_dispatch =
                    error.rejected_before_dispatch("thread/compact/start");
                if error.code.is_some()
                    && !rejected_before_dispatch
                    && let Some(pending) = self.state.lock().await.pending_compaction.as_mut()
                {
                    pending.accepted = true;
                }
                cache_codex_manual_trigger_absent(
                    &self.rpc.compaction_capability,
                    &capability,
                    "thread/compact/start",
                    &error,
                );
                self.finish_compaction_failure_with_dispatch(
                    BackendCompactionFailureKind::ProviderRejected,
                    error.to_string(),
                    rejected_before_dispatch.then_some(BackendCompactionDispatchState::Rejected),
                )
                .await;
            }
        }

        if self.state.lock().await.pending_compaction.is_some() {
            let inner = Arc::clone(self);
            let operation_id = request.operation_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(300)).await;
                let matches = inner
                    .state
                    .lock()
                    .await
                    .pending_compaction
                    .as_ref()
                    .is_some_and(|pending| pending.request.operation_id == operation_id);
                if matches {
                    inner
                        .finish_compaction_failure(
                            BackendCompactionFailureKind::TimedOut,
                            "Codex compaction timed out before correlated completion".to_string(),
                        )
                        .await;
                }
            });
        }

        BackendCompactionStart::Accepted(super::BackendAcceptedCompaction {
            operation_id: request.operation_id,
            terminal,
        })
    }

    async fn observe_codex_notification_contract(&self, method: &str) {
        let mut state = self.state.lock().await;
        if method == "rawResponseItem/completed" {
            state.raw_response_item_completed_seen = true;
        }
        if state.observed_notification_methods.contains(method) {
            return;
        }
        if state.observed_notification_methods.len() < MAX_CODEX_RAW_NOTIFICATION_METHODS {
            state
                .observed_notification_methods
                .insert(method.to_owned());
        } else {
            state.raw_notification_methods_truncated = true;
        }
    }

    async fn correlate_yielded_command_owners(&self, params: &Value) -> Vec<String> {
        let declared_session_ids = codex_yielded_session_ids(params);
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return Vec::new();
        };
        let Some(turn_id) = extract_turn_id(params) else {
            return Vec::new();
        };
        let Some(call_id) = params
            .pointer("/item/call_id")
            .or_else(|| params.pointer("/item/callId"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        let mut state = self.state.lock().await;
        let owner = state
            .response_splitters
            .get(&thread_id)
            .and_then(|splitter| splitter.raw_tool_owner(call_id));
        let Some(owner) = owner else {
            return Vec::new();
        };
        // Which *command execution* yielded this session. An interaction with an
        // already-running process is not one: it reports the session its target
        // yielded, which belongs to the execution's card, not to the
        // interaction's. Before interactions owned cards at all they had no
        // owner and fell out one line above; without this they reach the
        // correlation and report every poll as an uncorrelated session.
        if !codex_raw_call_is_rendered_elsewhere(&owner.tool_name, &owner.arguments) {
            return Vec::new();
        }
        let session_ids = if declared_session_ids.is_empty() {
            state
                .outstanding_command_executions
                .iter()
                .filter(|((owner_thread_id, _), command)| {
                    owner_thread_id == &thread_id
                        && command.turn_id == turn_id
                        && command.tool_call_id == owner.tool_call_id
                })
                .filter_map(|(_, command)| command.process_id.clone())
                .fold(Vec::new(), |mut session_ids, session_id| {
                    if !session_ids.contains(&session_id) {
                        session_ids.push(session_id);
                    }
                    session_ids
                })
        } else {
            declared_session_ids
        };
        if session_ids.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::with_capacity(session_ids.len());
        for session_id in &session_ids {
            let candidates = state
                .outstanding_command_executions
                .iter()
                .filter(|((owner_thread_id, _), command)| {
                    owner_thread_id == &thread_id
                        && command.turn_id == turn_id
                        && command.tool_call_id == owner.tool_call_id
                        && command.process_id.as_deref() == Some(session_id)
                })
                .map(|(key, _)| (key.clone(), None))
                .chain(
                    state
                        .unowned_command_executions
                        .iter()
                        .filter(|((owner_thread_id, _), command)| {
                            owner_thread_id == &thread_id
                                && command.turn_id == turn_id
                                && command.process_id == *session_id
                        })
                        .map(|(key, command)| (key.clone(), Some(command.clone()))),
                )
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                return Vec::new();
            }
            matches.push(
                candidates
                    .into_iter()
                    .next()
                    .expect("one yielded command candidate"),
            );
        }
        for (key, command) in matches {
            if let Some(command) = command {
                state.unowned_command_executions.remove(&key);
                state.outstanding_command_executions.insert(
                    key,
                    CodexOutstandingCommand {
                        tool_call_id: owner.tool_call_id.clone(),
                        command: command.command,
                        process_id: Some(command.process_id),
                        turn_id: command.turn_id,
                    },
                );
            }
        }
        if let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
            splitter.claim_raw_tool_call(&owner.tool_call_id);
        }
        session_ids
    }

    async fn resolve_unlinked_raw_tool_output(
        &self,
        params: &Value,
    ) -> CodexUnlinkedRawToolResolution {
        if !codex_yielded_session_ids(params).is_empty() {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        }
        let Some(item) = params.get("item") else {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        };
        if item.get("type").and_then(Value::as_str) != Some("custom_tool_call_output") {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        }
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        };
        let Some(turn_id) = extract_turn_id(params) else {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        };
        let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("callId"))
            .and_then(Value::as_str)
        else {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        };
        let snapshot = {
            let state = self.state.lock().await;
            let owner = state
                .response_splitters
                .get(&thread_id)
                .and_then(|splitter| splitter.raw_tool_owner(call_id));
            let owner_count = state
                .response_splitters
                .get(&thread_id)
                .map(|splitter| splitter.pending_raw_owner_count_for_turn(&turn_id))
                .unwrap_or(0);
            let candidates = state
                .unowned_command_executions
                .iter()
                .filter(|((owner_thread_id, _), command)| {
                    owner_thread_id == &thread_id && command.turn_id == turn_id
                })
                .map(|(key, command)| (key.clone(), command.clone()))
                .collect::<Vec<_>>();
            (owner, owner_count, candidates)
        };
        let (Some(owner), owner_count, candidates) = snapshot else {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        };
        if !owner.tool_name.eq_ignore_ascii_case("exec") {
            return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
        }
        if owner_count == 1 && candidates.len() == 1 {
            let (key, command) = candidates
                .into_iter()
                .next()
                .expect("one unowned command candidate");
            let process_id = command.process_id.clone();
            let mut state = self.state.lock().await;
            state.unowned_command_executions.remove(&key);
            state.outstanding_command_executions.insert(
                key,
                CodexOutstandingCommand {
                    tool_call_id: owner.tool_call_id.clone(),
                    command: command.command,
                    process_id: Some(command.process_id),
                    turn_id: command.turn_id,
                },
            );
            if let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
                splitter.claim_raw_tool_call(&owner.tool_call_id);
            }
            return CodexUnlinkedRawToolResolution::Correlated(process_id);
        }

        let (listed, list_authoritative) = match self
            .rpc
            .request(
                "thread/backgroundTerminals/list",
                json!({ "threadId": thread_id }),
            )
            .await
        {
            Ok(result) => (parse_codex_background_terminals(&result), true),
            Err(error) => {
                tracing::warn!(
                    thread_id,
                    turn_id,
                    %error,
                    "Could not verify an unlinked Codex nested execution"
                );
                (Vec::new(), false)
            }
        };
        let resolution = {
            let mut state = self.state.lock().await;
            let owner_count = state
                .response_splitters
                .get(&thread_id)
                .map(|splitter| splitter.pending_raw_owner_count_for_turn(&turn_id))
                .unwrap_or(0);
            let candidates = state
                .unowned_command_executions
                .iter()
                .filter(|((owner_thread_id, _), command)| {
                    owner_thread_id == &thread_id && command.turn_id == turn_id
                })
                .map(|(key, command)| (key.clone(), command.clone()))
                .collect::<Vec<_>>();
            let live_candidates = candidates
                .iter()
                .filter(|(key, command)| {
                    listed
                        .iter()
                        .any(|row| row.item_id == key.1 && row.process_id == command.process_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            let unknown_live_row_count = listed
                .iter()
                .filter(|row| {
                    let key = (thread_id.clone(), row.item_id.clone());
                    !state.background_commands.contains_key(&key)
                        && !state.outstanding_command_executions.contains_key(&key)
                        && !candidates.iter().any(|(candidate_key, command)| {
                            candidate_key == &key && command.process_id == row.process_id
                        })
                })
                .count();
            if owner_count == 1 && live_candidates.len() == 1 && unknown_live_row_count == 0 {
                let (key, command) = live_candidates
                    .into_iter()
                    .next()
                    .expect("one live unowned command candidate");
                let process_id = command.process_id.clone();
                state.unowned_command_executions.remove(&key);
                state.outstanding_command_executions.insert(
                    key,
                    CodexOutstandingCommand {
                        tool_call_id: owner.tool_call_id.clone(),
                        command: command.command,
                        process_id: Some(command.process_id),
                        turn_id: command.turn_id,
                    },
                );
                if let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
                    splitter.claim_raw_tool_call(&owner.tool_call_id);
                }
                Some(process_id)
            } else if list_authoritative
                && live_candidates.is_empty()
                && unknown_live_row_count == 0
            {
                return CodexUnlinkedRawToolResolution::OrdinaryCompletion;
            } else {
                state
                    .unowned_command_executions
                    .retain(|(owner_thread_id, _), command| {
                        owner_thread_id != &thread_id || command.turn_id != turn_id
                    });
                None
            }
        };
        if let Some(process_id) = resolution {
            return CodexUnlinkedRawToolResolution::Correlated(process_id);
        }

        let message = "Codex could not uniquely correlate a live nested command execution";
        if let Some((emitter, _)) = self.response_projection_target(&thread_id).await
            && !emitter.fail_pending_tool(&owner.tool_call_id, message)
        {
            emitter.backend_error(message);
        }
        if let Some(splitter) = self
            .state
            .lock()
            .await
            .response_splitters
            .get_mut(&thread_id)
        {
            splitter.remove_raw_tool_owner(call_id);
        }
        CodexUnlinkedRawToolResolution::Failed
    }

    async fn promote_command(&self, params: &Value, session_id: &str) {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            tracing::debug!(
                yielded_session_id = session_id,
                "Ignoring Codex yielded session result without thread identity"
            );
            return;
        };
        let Some(turn_id) = extract_turn_id(params) else {
            tracing::debug!(
                thread_id,
                yielded_session_id = session_id,
                "Ignoring Codex yielded session result without turn identity"
            );
            return;
        };
        let promoted = {
            let mut state = self.state.lock().await;
            let owner_is_live = if thread_id == state.thread_id {
                state.active_turn_id.as_deref() == Some(turn_id.as_str())
                    && !state
                        .terminated_turns
                        .iter()
                        .any(|terminated| terminated.turn_id == turn_id)
                    && state.terminated_turn_awaiting_replacement.is_none()
            } else {
                state
                    .subagent_streams
                    .get(&thread_id)
                    .is_some_and(|stream| {
                        stream.active_turn_id.as_deref() == Some(turn_id.as_str())
                            && !stream
                                .terminated_turns
                                .iter()
                                .any(|terminated| terminated.turn_id == turn_id)
                            && stream.terminated_turn_awaiting_replacement.is_none()
                    })
            };
            if !state.background_command_owner_active || !owner_is_live {
                None
            } else {
                let matches = state
                    .outstanding_command_executions
                    .iter()
                    .filter(|((owner_thread_id, _), command)| {
                        owner_thread_id == &thread_id
                            && command.turn_id == turn_id
                            && command.process_id.as_deref() == Some(session_id)
                    })
                    .map(|(key, command)| (key.clone(), command.clone()))
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    None
                } else {
                    let (key, outstanding) = matches
                        .into_iter()
                        .next()
                        .expect("one yielded command match");
                    match state.background_commands.entry(key) {
                        Entry::Occupied(_) => None,
                        Entry::Vacant(entry) => {
                            let tool_call_id = outstanding.tool_call_id.clone();
                            let command = CodexBackgroundCommand {
                                tool_call_id: outstanding.tool_call_id,
                                task_id: session_id.to_owned(),
                                description: outstanding.command,
                            };
                            let progress = command.progress();
                            entry.insert(command);
                            Some((progress, tool_call_id))
                        }
                    }
                }
            }
        };
        if let Some((progress, tool_call_id)) = promoted {
            let Some(emitter) = self.background_progress_emitter(&thread_id).await else {
                return;
            };
            let _ = tool_call_id;
            emitter.tool_progress(&progress);
        } else {
            tracing::debug!(
                thread_id,
                turn_id,
                yielded_session_id = session_id,
                "Codex yielded session result did not match one live command"
            );
        }
    }

    async fn handle_raw_modify_completion(&self, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return;
        };
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("custom_tool_call") => {
                let Some(input) = item.get("input").and_then(Value::as_str) else {
                    return;
                };
                let Some((file_path, before, after)) = parse_raw_codex_apply_patch(input) else {
                    return;
                };
                let Some(turn_id) = extract_turn_id(params) else {
                    return;
                };
                let tool_call_id = format!("codex:{thread_id}:{turn_id}:raw:{call_id}");
                self.state.lock().await.pending_raw_modify_calls.insert(
                    (thread_id, call_id.to_owned()),
                    PendingRawCodexModify {
                        turn_id,
                        tool_call_id,
                        file_path,
                        before,
                        after,
                        failed_output: None,
                    },
                );
            }
            Some("custom_tool_call_output") => {
                let output = raw_custom_tool_output_text(item);
                if !output.starts_with("Script failed") {
                    self.state
                        .lock()
                        .await
                        .pending_raw_modify_calls
                        .remove(&(thread_id, call_id.to_owned()));
                    return;
                }
                if let Some(pending) = self
                    .state
                    .lock()
                    .await
                    .pending_raw_modify_calls
                    .get_mut(&(thread_id, call_id.to_owned()))
                {
                    pending.failed_output = Some(output);
                }
            }
            _ => {}
        }
    }

    async fn flush_raw_modify_failures(&self, turn_id: Option<&str>) {
        let pending = {
            let mut state = self.state.lock().await;
            let keys = state
                .pending_raw_modify_calls
                .iter()
                .filter(|(_, pending)| {
                    pending.failed_output.is_some()
                        && turn_id.is_none_or(|turn_id| pending.turn_id == turn_id)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.pending_raw_modify_calls.remove(&key))
                .collect::<Vec<_>>()
        };
        for pending in pending {
            let output = pending
                .failed_output
                .expect("flushed raw modify failure must have output");
            self.emit_modify_file_request(
                &pending.tool_call_id,
                &pending.file_path,
                &pending.before,
                &pending.after,
            )
            .await;
            self.emit_tool_execution_completed(
                &pending.tool_call_id,
                "modify_file",
                false,
                json!({
                    "kind": "Error",
                    "short_message": "File modification failed",
                    "detailed_message": output,
                }),
                Some(output),
            )
            .await;
        }
    }

    async fn promote_root_commands_before_agent_response(&self, params: &Value) {
        let thread_id = extract_notification_thread_id(params);
        let turn_id = extract_turn_id(params);
        let promoted = {
            let mut state = self.state.lock().await;
            let thread_id = thread_id.unwrap_or_else(|| state.thread_id.clone());
            let turn_id = turn_id.or_else(|| state.active_turn_id.clone());
            if thread_id != state.thread_id || !state.background_command_owner_active {
                Vec::new()
            } else {
                let keys = state
                    .outstanding_command_executions
                    .iter()
                    .filter(|(key, command)| {
                        key.0 == thread_id
                            && turn_id
                                .as_ref()
                                .is_none_or(|turn_id| command.turn_id == *turn_id)
                            && !state.background_commands.contains_key(*key)
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                keys.into_iter()
                    .filter_map(|key| {
                        let outstanding = state.outstanding_command_executions.get(&key)?.clone();
                        let command = CodexBackgroundCommand {
                            tool_call_id: outstanding.tool_call_id.clone(),
                            task_id: outstanding
                                .process_id
                                .clone()
                                .unwrap_or_else(|| key.1.clone()),
                            description: outstanding.command,
                        };
                        let progress = command.progress();
                        state.background_commands.insert(key, command);
                        Some((progress, outstanding.tool_call_id))
                    })
                    .collect()
            }
        };
        for (progress, tool_call_id) in promoted {
            tracing::info!(
                tool_call_id,
                "Promoting Codex command because agent response began before command completion"
            );
            let _ = tool_call_id;
            self.emitter.tool_progress(&progress);
        }
    }

    async fn warn_codex_raw_contract_drift_once_if_needed(&self) {
        let warning = {
            let mut state = self.state.lock().await;
            take_codex_raw_contract_drift_warning(&mut state)
        };
        let Some(warning) = warning else {
            return;
        };
        tracing::warn!(
            thread_id = warning.thread_id.as_str(),
            observed_notification_methods = ?warning.observed_notification_methods,
            notification_methods_truncated = warning.methods_truncated,
            "Codex experimental raw-event contract may have drifted"
        );
    }

    async fn finish_compaction_failure(&self, kind: BackendCompactionFailureKind, message: String) {
        self.finish_compaction_failure_with_dispatch(kind, message, None)
            .await;
    }

    async fn finish_compaction_failure_with_dispatch(
        &self,
        kind: BackendCompactionFailureKind,
        message: String,
        dispatch_override: Option<BackendCompactionDispatchState>,
    ) {
        let (pending, thread_id) = {
            let mut state = self.state.lock().await;
            (state.pending_compaction.take(), state.thread_id.clone())
        };
        let Some(mut pending) = pending else {
            return;
        };
        let mutation = if pending.item_started || pending.item_completed {
            BackendCompactionMutationState::MayHaveMutated
        } else {
            BackendCompactionMutationState::NotObserved
        };
        let dispatch = dispatch_override.unwrap_or(if pending.accepted {
            BackendCompactionDispatchState::Accepted
        } else {
            BackendCompactionDispatchState::MayHaveReachedProvider
        });
        let metrics = CompactionMetrics {
            duration_ms: Some(pending.started_at.elapsed().as_millis() as u64),
            ..CompactionMetrics::default()
        };
        let result = BackendCompactionResult {
            operation_id: pending.request.operation_id.clone(),
            dispatch,
            mutation,
            outcome: Err(BackendCompactionFailure { kind, message }),
            provider_session_id: Some(SessionId(thread_id.clone())),
            metrics,
            post_context_tokens: PostCompactionTokenCount::Unknown,
            evidence: BackendCompactionTerminalEvidence::Codex {
                thread_id,
                turn_id: pending.turn_id,
                item_id: pending.item_id,
                deprecated_notification_seen: pending.deprecated_notification_seen,
            },
        };
        if let Some(tx) = pending.terminal_tx.take() {
            let _ = tx.send(result);
        }
    }

    async fn finish_codex_compaction_from_turn(&self) {
        let Some(mut pending) = self.state.lock().await.pending_compaction.take() else {
            return;
        };
        let thread_id = self.state.lock().await.thread_id.clone();
        eprintln!(
            "TYDE CODEX COMPACTION TERMINAL thread_id={thread_id:?} turn_id={:?} item_id={:?} accepted={} item_started={} item_completed={} turn_status={:?}",
            pending.turn_id,
            pending.item_id,
            pending.accepted,
            pending.item_started,
            pending.item_completed,
            pending.turn_status,
        );
        let completed = pending.accepted
            && pending.item_started
            && pending.item_completed
            && pending.turn_status.as_deref() == Some("completed")
            && pending.turn_id.is_some()
            && pending.item_id.is_some();
        let metrics = CompactionMetrics {
            duration_ms: Some(pending.started_at.elapsed().as_millis() as u64),
            ..CompactionMetrics::default()
        };
        let observation =
            completed.then(|| {
                let turn_id = pending
                    .turn_id
                    .as_ref()
                    .expect("completed Codex compaction has a turn id");
                let item_id = pending
                    .item_id
                    .as_ref()
                    .expect("completed Codex compaction has an item id");
                BackendObservedCompaction {
                    observation_id: super::compaction::stable_observation_id(
                        "codex",
                        &thread_id,
                        &format!("{turn_id}:{item_id}"),
                    ),
                    trigger: CompactionTrigger::BackendObservedManual,
                    method: CompactionMethod::NativeRpc,
                    provider_session_id: Some(SessionId(thread_id.clone())),
                    metrics: metrics.clone(),
                    source: BackendCompactionObservationSource::CodexItem {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id: item_id.clone(),
                    },
                    user_focus: pending.request.focus.clone().map(|text| {
                        BackendCompactionUserFocus {
                            text,
                            provenance: BackendCompactionUserFocusProvenance::TydeRequest,
                        }
                    }),
                }
            });
        let result = BackendCompactionResult {
            operation_id: pending.request.operation_id.clone(),
            dispatch: BackendCompactionDispatchState::Accepted,
            mutation: if pending.item_completed {
                BackendCompactionMutationState::Completed
            } else if pending.item_started {
                BackendCompactionMutationState::MayHaveMutated
            } else {
                BackendCompactionMutationState::NotObserved
            },
            outcome: if completed {
                Ok(BackendCompactionSuccess {
                    mechanism: CompactionMethod::NativeRpc,
                })
            } else {
                Err(BackendCompactionFailure {
                    kind: BackendCompactionFailureKind::ProviderFailed,
                    message: format!(
                        "Codex compaction ended without a completed contextCompaction item (turn status {:?})",
                        pending.turn_status
                    ),
                })
            },
            provider_session_id: Some(SessionId(thread_id.clone())),
            metrics,
            post_context_tokens: PostCompactionTokenCount::Unknown,
            evidence: BackendCompactionTerminalEvidence::Codex {
                thread_id,
                turn_id: pending.turn_id,
                item_id: pending.item_id,
                deprecated_notification_seen: pending.deprecated_notification_seen,
            },
        };
        if let Some(observation) = observation {
            self.emitter
                .compaction_event(&BackendCompactionEvent::Observed(Box::new(observation)));
        }
        if let Some(tx) = pending.terminal_tx.take() {
            let _ = tx.send(result);
        }
    }

    async fn intercept_compaction_notification(&self, method: &str, params: &Value) -> bool {
        let thread_id = extract_notification_thread_id(params);
        let item = params.get("item");
        let item_type = item
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str);
        let turn_id = extract_turn_id(params).or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

        let mut finish = false;
        let mut progress = None;
        let mut observed = None;
        {
            let mut state = self.state.lock().await;
            let belongs_to_root = thread_id.as_ref().is_none_or(|id| id == &state.thread_id);
            if !belongs_to_root {
                return false;
            }
            if let Some(pending) = state.pending_compaction.as_mut() {
                match method {
                    "turn/started" => {
                        if pending.turn_id.is_none() {
                            pending.turn_id = turn_id.clone();
                        }
                        progress = Some(BackendCompactionProgress {
                            operation_id: pending.request.operation_id.clone(),
                            stage: CompactionStage::Compacting,
                            elapsed_ms: None,
                        });
                    }
                    "item/started" if item_type == Some("contextCompaction") => {
                        if pending.turn_id.is_none() {
                            pending.turn_id = turn_id.clone();
                        }
                        pending.item_started = true;
                        pending.item_id = item
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        progress = Some(BackendCompactionProgress {
                            operation_id: pending.request.operation_id.clone(),
                            stage: CompactionStage::Compacting,
                            elapsed_ms: None,
                        });
                    }
                    "item/completed" if item_type == Some("contextCompaction") => {
                        if pending.turn_id.is_none() {
                            pending.turn_id = turn_id.clone();
                        }
                        pending.item_started = true;
                        pending.item_completed = true;
                        pending.item_id = item
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or_else(|| pending.item_id.clone());
                        progress = Some(BackendCompactionProgress {
                            operation_id: pending.request.operation_id.clone(),
                            stage: CompactionStage::Finalizing,
                            elapsed_ms: None,
                        });
                    }
                    "thread/compacted" => {
                        pending.deprecated_notification_seen = true;
                    }
                    "turn/completed"
                        if pending.turn_id.is_some()
                            && turn_id.as_ref() == pending.turn_id.as_ref() =>
                    {
                        pending.turn_status = params
                            .get("turn")
                            .and_then(|turn| turn.get("status"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        finish = true;
                    }
                    _ => {}
                }
                if matches!(
                    method,
                    "turn/started"
                        | "item/started"
                        | "item/completed"
                        | "rawResponse/completed"
                        | "thread/tokenUsage/updated"
                        | "turn/completed"
                ) && (pending.turn_id.as_ref() == turn_id.as_ref()
                    || item_type == Some("contextCompaction"))
                {
                    drop(state);
                    if let Some(progress) = progress {
                        self.emitter
                            .compaction_event(&BackendCompactionEvent::Progress(progress));
                    }
                    if finish {
                        self.finish_codex_compaction_from_turn().await;
                    }
                    return true;
                }
            } else if method == "item/completed" && item_type == Some("contextCompaction") {
                let turn_id = turn_id.unwrap_or_else(|| "unknown-turn".to_string());
                let item_id = item
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown-item")
                    .to_string();
                observed = Some(BackendObservedCompaction {
                    observation_id: super::compaction::stable_observation_id(
                        "codex",
                        &state.thread_id,
                        &format!("{turn_id}:{item_id}"),
                    ),
                    trigger: CompactionTrigger::BackendAutomatic,
                    method: CompactionMethod::BackendAutomatic,
                    provider_session_id: Some(SessionId(state.thread_id.clone())),
                    metrics: CompactionMetrics::default(),
                    source: BackendCompactionObservationSource::CodexItem {
                        thread_id: state.thread_id.clone(),
                        turn_id,
                        item_id,
                    },
                    user_focus: None,
                });
            }
        }
        if let Some(observed) = observed {
            self.emitter
                .compaction_event(&BackendCompactionEvent::Observed(Box::new(observed)));
            return true;
        }
        false
    }

    /// Record a command execution that has started but not finished, and arm
    /// the poll that reconciles confirmed background work.
    async fn track_command_execution(
        self: &Arc<Self>,
        params: &Value,
        provider_item_id: &str,
        tool_call_id: &str,
        item: &Value,
    ) {
        {
            let mut state = self.state.lock().await;
            if !state.background_command_owner_active {
                return;
            }
            let thread_id =
                extract_notification_thread_id(params).unwrap_or_else(|| state.thread_id.clone());
            let turn_id = extract_turn_id(params)
                .or_else(|| {
                    if thread_id == state.thread_id {
                        state.active_turn_id.clone()
                    } else {
                        state
                            .subagent_streams
                            .get(&thread_id)
                            .and_then(|stream| stream.active_turn_id.clone())
                    }
                })
                .unwrap_or_else(|| "turn".to_owned());
            state.outstanding_command_executions.insert(
                (thread_id, provider_item_id.to_owned()),
                CodexOutstandingCommand {
                    tool_call_id: tool_call_id.to_owned(),
                    command: codex_command_text(item),
                    process_id: codex_process_id(item),
                    turn_id,
                },
            );
        }
        self.spawn_background_terminal_poll();
    }

    /// Emit a tool outcome, or hold it if its card has not been declared yet.
    ///
    /// See [`CodexDeferredToolCompletion`]. `has_known_tool_request` is the
    /// discriminator: a card Codex has told us about but that no response has
    /// declared is unknown to the emitter, and completing it would be dropped
    /// as `completion_without_request`.
    async fn emit_or_defer_tool_completion(
        &self,
        thread_id: &str,
        emitter: &Arc<TurnEmitter>,
        tool_call_id: &str,
        outcome: ToolExecutionOutcome,
    ) {
        if emitter.has_known_tool_request(tool_call_id) {
            emitter.tool_completed(tool_call_id, outcome);
            return;
        }
        tracing::debug!(
            thread_id,
            tool_call_id,
            "deferring Codex tool completion until its provider response declares the card"
        );
        self.state
            .lock()
            .await
            .deferred_tool_completions
            .push(CodexDeferredToolCompletion {
                thread_id: thread_id.to_owned(),
                tool_call_id: tool_call_id.to_owned(),
                outcome,
            });
    }

    /// Emit every held outcome whose card the just-finalized response declared.
    ///
    /// Anything still undeclared stays held: a later response in the same turn
    /// may still declare it. `finalize_incomplete_strict_response` clears the
    /// remainder so nothing survives its turn.
    async fn flush_deferred_tool_completions(&self, thread_id: &str, emitter: &Arc<TurnEmitter>) {
        let ready = {
            let mut state = self.state.lock().await;
            let mut ready = Vec::new();
            state.deferred_tool_completions.retain(|deferred| {
                if deferred.thread_id != thread_id
                    || !emitter.has_pending_tool_request(&deferred.tool_call_id)
                {
                    return true;
                }
                ready.push((deferred.tool_call_id.clone(), deferred.outcome.clone()));
                false
            });
            ready
        };
        for (tool_call_id, outcome) in ready {
            tracing::debug!(
                thread_id,
                tool_call_id,
                "emitting a Codex tool completion that outran its card declaration"
            );
            emitter.tool_completed(&tool_call_id, outcome);
        }
    }

    /// Drop held outcomes for a thread, reporting each one that never found a
    /// card. A silent drop here is exactly how a tool card ends up spinning
    /// until the idle sweep cancels it, so it is logged rather than ignored.
    async fn discard_deferred_tool_completions(&self, thread_id: &str) {
        let discarded = {
            let mut state = self.state.lock().await;
            let mut discarded = Vec::new();
            state.deferred_tool_completions.retain(|deferred| {
                if deferred.thread_id != thread_id {
                    return true;
                }
                discarded.push(deferred.tool_call_id.clone());
                false
            });
            discarded
        };
        for tool_call_id in discarded {
            tracing::error!(
                thread_id,
                tool_call_id,
                "Codex tool completion was never claimed by a declared card"
            );
        }
    }

    async fn forget_command_execution(
        &self,
        params: &Value,
        provider_item_id: &str,
    ) -> Option<CodexOutstandingCommand> {
        let mut state = self.state.lock().await;
        let thread_id =
            extract_notification_thread_id(params).unwrap_or_else(|| state.thread_id.clone());
        let outstanding = state
            .outstanding_command_executions
            .remove(&(thread_id.clone(), provider_item_id.to_owned()));
        state
            .unowned_command_executions
            .remove(&(thread_id, provider_item_id.to_owned()));
        outstanding
    }

    /// Start watching `thread/backgroundTerminals/list` if nothing is watching
    /// it yet. The loop ends as soon as no command execution is outstanding,
    /// so an idle thread costs nothing.
    ///
    /// Holds a `Weak` reference: a process the user leaves running must not
    /// keep the session — and its app-server child — alive after teardown.
    fn spawn_background_terminal_poll(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let mut state = inner.state.lock().await;
                if state.background_terminal_poll_active {
                    return;
                }
                state.background_terminal_poll_active = true;
            }
            loop {
                tokio::time::sleep(CODEX_BACKGROUND_TERMINAL_POLL_INTERVAL).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if !inner.poll_background_terminals_once().await {
                    inner.state.lock().await.background_terminal_poll_active = false;
                    return;
                }
            }
        });
    }

    /// Stop the processes behind the foreground cards a user interrupt is
    /// about to report as cancelled.
    ///
    /// Interrupt is the soft operation: detached work outlives it by design
    /// (`ChatEvent`, "Cancellation ordering"), so this targets exactly the open
    /// *foreground* cards and leaves `background_commands` alone — which is why
    /// it cannot reuse `terminate_background_terminals`, whose whole job is to
    /// take everything down at shutdown.
    ///
    /// A command Codex never gave a `processId` for has no handle to kill, and
    /// per the chosen semantics that case stays quiet rather than reporting a
    /// stop Tyde cannot perform.
    async fn terminate_foreground_commands(&self) {
        let foreground = self
            .emitter
            .open_foreground_tool_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        if foreground.is_empty() {
            return;
        }
        let targets = {
            let state = self.state.lock().await;
            state
                .outstanding_command_executions
                .iter()
                .filter(|(_, command)| foreground.contains(&command.tool_call_id))
                .filter_map(|((thread_id, _), command)| {
                    command.process_id.as_ref().map(|process_id| {
                        (
                            thread_id.clone(),
                            process_id.clone(),
                            command.tool_call_id.clone(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        for (thread_id, process_id, tool_call_id) in targets {
            tracing::debug!(
                thread_id,
                process_id,
                tool_call_id,
                "terminating a Codex foreground command for a user interrupt"
            );
            self.rpc.spawn_request(
                "thread/backgroundTerminals/terminate",
                json!({ "threadId": thread_id, "processId": process_id }),
            );
            // Measured: killing the process makes Codex report the exec as
            // `Failed { "Command failed with exit code -1" }`, and that real
            // completion closes the card first — blaming the tool for what the
            // user did. Close it as cancelled here and record the id so
            // `suppress_cancelled_tool_completion` drops the late failure,
            // which leaves exactly one completion and the right one.
            self.emitter
                .cancel_pending_tool(&tool_call_id, "Tool execution was cancelled by user");
            let mut state = self.state.lock().await;
            state.pending_tool_call_ids.remove(&tool_call_id);
            state.cancelled_tool_call_ids.insert(tool_call_id);
        }
    }

    /// Stop one background command the user cancelled from its card.
    ///
    /// Scoped to that card: `terminate_background_terminals` kills everything
    /// at shutdown and `terminate_foreground_commands` kills what an interrupt
    /// covers, but neither can express "this one". A background command is
    /// tracked either as a task (`background_commands`, keyed by its Codex task
    /// id) or as a still-running execution (`outstanding_command_executions`,
    /// keyed by its process id); both are addressed by the same terminate RPC.
    async fn cancel_background_task(&self, tool_call_id: &str) -> bool {
        let target = {
            let state = self.state.lock().await;
            state
                .background_commands
                .iter()
                .find(|(_, command)| command.tool_call_id == tool_call_id)
                .map(|((thread_id, _), command)| (thread_id.clone(), command.task_id.clone()))
                .or_else(|| {
                    state
                        .outstanding_command_executions
                        .iter()
                        .find(|(_, command)| command.tool_call_id == tool_call_id)
                        .and_then(|((thread_id, _), command)| {
                            command
                                .process_id
                                .as_ref()
                                .map(|process_id| (thread_id.clone(), process_id.clone()))
                        })
                })
        };
        let Some((thread_id, process_id)) = target else {
            tracing::warn!(
                tool_call_id,
                "no running Codex background command matches the cancelled card"
            );
            return false;
        };
        if let Err(error) = self
            .rpc
            .request(
                "thread/backgroundTerminals/terminate",
                json!({ "threadId": thread_id, "processId": process_id }),
            )
            .await
        {
            tracing::error!(
                thread_id,
                process_id,
                tool_call_id,
                error = %error,
                "failed to terminate a cancelled Codex background command"
            );
            return false;
        }
        // Killing the process makes Codex report the exec as failed, which
        // would blame the command for what the user did. Close the card as
        // cancelled and record the id so the late failure is dropped.
        self.emitter
            .cancel_pending_tool(tool_call_id, "Tool execution was cancelled by user");
        let mut state = self.state.lock().await;
        state.pending_tool_call_ids.remove(tool_call_id);
        state
            .cancelled_tool_call_ids
            .insert(tool_call_id.to_owned());
        true
    }

    async fn terminate_background_terminals(&self) {
        let terminals =
            {
                let state = self.state.lock().await;
                state
                    .background_commands
                    .iter()
                    .map(|((thread_id, _), command)| (thread_id.clone(), command.task_id.clone()))
                    .chain(state.outstanding_command_executions.iter().filter_map(
                        |((thread_id, _), command)| {
                            command
                                .process_id
                                .as_ref()
                                .map(|process_id| (thread_id.clone(), process_id.clone()))
                        },
                    ))
                    .chain(state.unowned_command_executions.iter().map(
                        |((thread_id, _), command)| (thread_id.clone(), command.process_id.clone()),
                    ))
                    .collect::<HashSet<_>>()
            };
        for (thread_id, process_id) in terminals {
            if let Err(error) = self
                .rpc
                .request(
                    "thread/backgroundTerminals/terminate",
                    json!({ "threadId": thread_id, "processId": process_id }),
                )
                .await
            {
                tracing::warn!(
                    thread_id,
                    process_id,
                    error = %error,
                    "Failed to terminate Codex background terminal during shutdown"
                );
            }
        }
    }

    /// One reconcile pass. Returns whether the loop should keep polling.
    async fn poll_background_terminals_once(&self) -> bool {
        // Native children are separate threads on the same app-server, and
        // each keeps its own background terminals, so every thread with an
        // outstanding command needs its own snapshot.
        let thread_ids = {
            let state = self.state.lock().await;
            state
                .outstanding_command_executions
                .keys()
                .map(|(thread_id, _)| thread_id.clone())
                .collect::<HashSet<_>>()
        };
        if thread_ids.is_empty() {
            return false;
        }
        for thread_id in thread_ids {
            let result = match self
                .rpc
                .request(
                    "thread/backgroundTerminals/list",
                    json!({ "threadId": thread_id }),
                )
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    // The app-server is gone or wedged. Nothing authoritative
                    // is left to report, so stop rather than guess at liveness.
                    tracing::debug!("Codex background terminal poll failed: {err}");
                    return false;
                }
            };
            if result
                .get("nextCursor")
                .is_some_and(|cursor| !cursor.is_null())
            {
                tracing::warn!(
                    "Codex reported more background terminals than one page; \
                     only the first page is tracked"
                );
            }
            let listed = parse_codex_background_terminals(&result);
            let (progress, cancelled) = {
                let mut state = self.state.lock().await;
                if !listed.is_empty() && state.first_background_list_thread_id.is_none() {
                    state.first_background_list_thread_id = Some(thread_id.clone());
                }
                reconcile_codex_background_terminals(&mut state, &thread_id, &listed)
            };
            if !progress.is_empty() || !cancelled.is_empty() {
                let Some(emitter) = self.background_progress_emitter(&thread_id).await else {
                    // The child's stream is gone; its rows went with it.
                    continue;
                };
                for update in &progress {
                    emitter.tool_progress(update);
                }
                for tool_call_id in cancelled {
                    emitter.cancel_pending_tool(
                        &tool_call_id,
                        "Codex background command disappeared before reporting completion",
                    );
                }
            }
            let deferred_terminal = {
                let mut state = self.state.lock().await;
                let still_running = state
                    .background_commands
                    .keys()
                    .chain(state.outstanding_command_executions.keys())
                    .any(|(owner_thread_id, _)| owner_thread_id == &thread_id);
                (!still_running)
                    .then(|| {
                        state
                            .subagent_streams
                            .get_mut(&thread_id)
                            .and_then(|stream| stream.pending_spawn_terminal_status.take())
                    })
                    .flatten()
            };
            if let Some(status) = deferred_terminal {
                self.terminalize_codex_subagent_spawn(&thread_id, &status)
                    .await;
            }
        }
        true
    }

    /// Background rows belong to the thread that owns the command: the root
    /// session, or a native child's own stream.
    async fn background_progress_emitter(&self, thread_id: &str) -> Option<Arc<TurnEmitter>> {
        let state = self.state.lock().await;
        if state.thread_id == thread_id {
            return Some(Arc::clone(&self.emitter));
        }
        state
            .subagent_streams
            .get(thread_id)
            .map(|stream| Arc::clone(&stream.emitter))
    }

    async fn take_background_command(
        &self,
        params: &Value,
        provider_item_id: &str,
    ) -> Option<CodexBackgroundCommand> {
        let mut state = self.state.lock().await;
        let thread_id =
            extract_notification_thread_id(params).unwrap_or_else(|| state.thread_id.clone());
        state
            .background_commands
            .remove(&(thread_id, provider_item_id.to_owned()))
    }

    async fn complete_background_command_group(
        &self,
        thread_id: &str,
        command: &CodexBackgroundCommand,
        exit_code: i32,
        output: &str,
    ) -> Option<ToolExecutionOutcome> {
        let mut state = self.state.lock().await;
        let group_key = (thread_id.to_owned(), command.tool_call_id.clone());
        state
            .background_command_results
            .entry(group_key.clone())
            .or_default()
            .push(CodexBackgroundCommandResult {
                task_id: command.task_id.clone(),
                description: command.description.clone(),
                exit_code,
                output: output.to_owned(),
            });
        let still_running =
            state
                .background_commands
                .iter()
                .any(|((owner_thread_id, _), pending)| {
                    owner_thread_id == thread_id && pending.tool_call_id == command.tool_call_id
                })
                || state.outstanding_command_executions.iter().any(
                    |((owner_thread_id, _), pending)| {
                        owner_thread_id == thread_id && pending.tool_call_id == command.tool_call_id
                    },
                );
        if still_running {
            return None;
        }
        state
            .background_command_results
            .remove(&group_key)
            .map(codex_background_command_group_outcome)
    }

    async fn enqueue_background_wake(
        self: &Arc<Self>,
        command: CodexBackgroundCommand,
        exit_code: i32,
        output: String,
    ) {
        {
            let mut state = self.state.lock().await;
            if !state.background_command_owner_active {
                return;
            }
            state
                .pending_background_wakes
                .push_back(CodexBackgroundWake {
                    tool_call_id: command.tool_call_id,
                    task_id: command.task_id,
                    description: command.description,
                    exit_code,
                    output,
                });
        }
        self.spawn_pending_background_wake();
    }

    fn spawn_pending_background_wake(self: &Arc<Self>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let Some((thread_id, wakes, model, effort, approval_policy, sandbox_policy)) = ({
                let mut state = inner.state.lock().await;
                if state.pending_background_wakes.is_empty()
                    || state.background_wake_request_in_flight
                    || state.active_turn_id.is_some()
                    || state.awaiting_root_turn_start
                    || !state.background_command_owner_active
                {
                    None
                } else {
                    state.background_wake_request_in_flight = true;
                    let wakes = state.pending_background_wakes.drain(..).collect::<Vec<_>>();
                    let (model, effort) = match state.execution_mode {
                        BackendExecutionMode::Agent => (
                            state.model_override.clone(),
                            state.reasoning_effort_override.clone(),
                        ),
                        BackendExecutionMode::InferenceOnly => (None, None),
                    };
                    let approval_policy = state
                        .approval_policy
                        .clone()
                        .unwrap_or_else(|| codex_approval_policy(state.execution_mode).to_string());
                    let sandbox_policy = codex_sandbox_policy(
                        state.access_mode,
                        state.turn_network_access,
                        state.execution_mode,
                    );
                    Some((
                        state.thread_id.clone(),
                        wakes,
                        model,
                        effort,
                        approval_policy,
                        sandbox_policy,
                    ))
                }
            }) else {
                return;
            };

            let notification = codex_background_wake_notification(&wakes);
            let mut params = json!({
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": notification,
                    "text_elements": []
                }],
                "summary": CODEX_REASONING_SUMMARY_LEVEL,
                "approvalPolicy": approval_policy,
                "sandboxPolicy": sandbox_policy
            });
            if let Some(model) = model {
                params["model"] = Value::String(model);
            }
            if let Some(effort) = effort {
                params["effort"] = Value::String(effort);
            }

            if let Err(error) = inner.rpc.request("turn/start", params).await {
                let mut state = inner.state.lock().await;
                state.background_wake_request_in_flight = false;
                for wake in wakes.into_iter().rev() {
                    state.pending_background_wakes.push_front(wake);
                }
                drop(state);
                tracing::warn!(%error, "Failed to start Codex background completion turn");
                inner
                    .emitter
                    .warning_message("Codex could not resume after background work completed");
            }
        });
    }

    async fn drain_background_commands(&self) {
        let commands = {
            let mut state = self.state.lock().await;
            state.background_command_owner_active = false;
            state.pending_background_wakes.clear();
            state.background_wake_request_in_flight = false;
            let commands = take_all_codex_commands(&mut state);
            state.background_command_results.clear();
            for command in &commands {
                state.pending_tool_call_ids.remove(&command.tool_call_id);
                state
                    .cancelled_tool_call_ids
                    .insert(command.tool_call_id.clone());
            }
            commands
        };
        for command in commands {
            let Some(emitter) = self.background_progress_emitter(&command.thread_id).await else {
                continue;
            };
            if emitter.has_pending_tool_request(&command.tool_call_id) {
                eprintln!(
                    "TYDE CODEX TOOL TERMINAL source=shutdown-drain thread_id={} tool_call_id={} outcome=cancelled",
                    command.thread_id, command.tool_call_id
                );
                emitter.tool_completed(
                    &command.tool_call_id,
                    ToolExecutionOutcome::Cancelled {
                        message: "Background command owner exited".to_string(),
                    },
                );
            }
        }
        self.warn_codex_raw_contract_drift_once_if_needed().await;
    }

    async fn tool_call_started_id(
        &self,
        params: &Value,
        provider_item_id: &str,
        tool_name: &str,
    ) -> String {
        let mut state = self.state.lock().await;
        let thread_id =
            extract_notification_thread_id(params).unwrap_or_else(|| state.thread_id.clone());
        let turn_id = extract_turn_id(params)
            .or_else(|| state.active_turn_id.clone())
            .unwrap_or_else(|| "turn".to_owned());
        let base_canonical_id = codex_scoped_tool_call_id(params, provider_item_id);
        state.tool_call_identities.started(
            &thread_id,
            &turn_id,
            provider_item_id,
            tool_name,
            &base_canonical_id,
        )
    }

    async fn tool_call_completed_id(
        &self,
        params: &Value,
        provider_item_id: &str,
        tool_name: &str,
    ) -> String {
        let mut state = self.state.lock().await;
        let thread_id =
            extract_notification_thread_id(params).unwrap_or_else(|| state.thread_id.clone());
        let turn_id = extract_turn_id(params)
            .or_else(|| state.active_turn_id.clone())
            .unwrap_or_else(|| "turn".to_owned());
        let base_canonical_id = codex_scoped_tool_call_id(params, provider_item_id);
        state.tool_call_identities.completed(
            &thread_id,
            &turn_id,
            provider_item_id,
            tool_name,
            &base_canonical_id,
        )
    }

    fn spawn_capacity_refresh(self: &Arc<Self>, emitter: Arc<dyn SubAgentEmitter>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            {
                let mut state = inner.state.lock().await;
                if state.capacity_refresh_in_flight {
                    return;
                }
                state.capacity_refresh_in_flight = true;
            }
            let capacity = inner.read_backend_capacity().await;
            inner.state.lock().await.capacity_refresh_in_flight = false;
            emitter.on_backend_capacity(protocol::BackendKind::Codex, capacity);
        });
    }

    async fn read_backend_capacity(&self) -> protocol::BackendCapacityState {
        let account = match self
            .rpc
            .request("account/read", json!({ "refreshToken": false }))
            .await
        {
            Ok(account) => account,
            Err(_) => {
                return protocol::BackendCapacityState::Unavailable {
                    reason: protocol::CapacityUnavailableReason::SourceUnreachable,
                };
            }
        };
        match codex_capacity_account_mode(&account) {
            CodexCapacityAccountMode::ChatGpt => {}
            CodexCapacityAccountMode::Unsupported => {
                return protocol::BackendCapacityState::Unsupported {
                    reason: protocol::CapacityUnsupportedReason::AccountTypeNotReported,
                };
            }
            CodexCapacityAccountMode::Unauthenticated => {
                return protocol::BackendCapacityState::AuthError {
                    detail: protocol::CapacityErrorDetail {
                        summary: "Codex account information is unavailable".to_string(),
                        code: protocol::CapacityErrorCode::NotAuthenticated,
                    },
                };
            }
        }

        match self.rpc.request("account/rateLimits/read", json!({})).await {
            Ok(snapshot) => match map_passive_rate_limits_updated(&snapshot) {
                Ok(report) => protocol::BackendCapacityState::Known { report },
                Err(reason) => protocol::BackendCapacityState::Unavailable { reason },
            },
            Err(_) => protocol::BackendCapacityState::Unavailable {
                reason: protocol::CapacityUnavailableReason::SourceUnreachable,
            },
        }
    }

    async fn apply_local_settings(&self, settings: &Value) {
        let Some(obj) = settings.as_object() else {
            return;
        };
        let mut state = self.state.lock().await;

        if let Some(model_value) = obj.get("model") {
            if model_value.is_null() {
                state.model_override = None;
            } else if let Some(model) = model_value.as_str() {
                let normalized = model.trim();
                state.model_override = if normalized.is_empty() {
                    None
                } else {
                    Some(normalized.to_string())
                };
                if state.model_override.is_some() {
                    state.effective_model = state.model_override.clone();
                }
            }
        }

        if let Some(effort_value) = obj
            .get("reasoning_effort")
            .or_else(|| obj.get("reasoningEffort"))
        {
            if effort_value.is_null() {
                state.reasoning_effort_override = None;
            } else if let Some(raw) = effort_value.as_str() {
                state.reasoning_effort_override = normalize_reasoning_effort(raw);
            }
        }

        if obj.contains_key("approval_policy") || obj.contains_key("approvalPolicy") {
            state.approval_policy = Some(CODEX_FORCED_APPROVAL_POLICY.to_string());
        }
    }

    async fn update_runtime_settings(&self, settings: Value) -> Result<(), String> {
        let thread_id = self.state.lock().await.thread_id.clone();
        let params = codex_thread_settings_update_params(&thread_id, &settings)?;
        self.rpc.request("thread/settings/update", params).await?;
        self.apply_local_settings(&settings).await;
        Ok(())
    }

    async fn open_agent_message_item(
        &self,
        message_id: ChatMessageId,
        cause: CodexProviderOpenCause,
        notification_thread_id: Option<&str>,
        notification_turn_id: Option<&str>,
    ) -> CodexAgentMessageOpen {
        let tool_container_was_open = self.state.lock().await.tool_container.is_some();
        self.close_tool_container_if_open().await;
        let mut state = self.state.lock().await;
        if state.completed_agent_messages.contains_key(&message_id) {
            return CodexAgentMessageOpen::Terminal;
        }
        if state.retired_unpublished_message_ids.contains(&message_id) {
            return CodexAgentMessageOpen::Retired;
        }
        if let Some(stream) = state.active_stream.as_ref() {
            if stream.message_id == message_id {
                return if stream.reasoning_only {
                    CodexAgentMessageOpen::Foreign
                } else {
                    CodexAgentMessageOpen::Existing
                };
            }
            if stream.is_replaceable_provider_reservation() {
                let retired_id = stream.message_id.clone();
                state.active_stream = None;
                state.retired_unpublished_message_ids.insert(retired_id);
            } else {
                let same_owner =
                    notification_thread_id.is_none_or(|thread_id| thread_id == state.thread_id);
                let same_turn = state.active_turn_id.as_ref().is_some_and(|turn_id| {
                    stream.turn_id == *turn_id
                        && notification_turn_id.is_none_or(|incoming| incoming == turn_id)
                });
                let tool_ownership_is_clear = !tool_container_was_open
                    && state.pending_tool_call_ids.is_empty()
                    && state.tool_container_images.is_empty()
                    && !state.close_active_stream_when_tools_idle;
                let can_supersede = cause == CodexProviderOpenCause::ItemStarted
                    && same_owner
                    && same_turn
                    && stream.generated_identity.is_none()
                    && tool_ownership_is_clear
                    && state.provider_supersessions_this_turn
                        < MAX_CODEX_PROVIDER_SUPERSESSIONS_PER_TURN;
                if !can_supersede {
                    return CodexAgentMessageOpen::Foreign;
                }
                let previous = state
                    .active_stream
                    .take()
                    .expect("eligible Codex provider stream disappeared");
                state.provider_supersessions_this_turn =
                    state.provider_supersessions_this_turn.saturating_add(1);
                let turn_id = state
                    .active_turn_id
                    .clone()
                    .unwrap_or_else(|| message_id.0.clone());
                let images = std::mem::take(&mut state.tool_container_images);
                state.active_stream = Some(ActiveStreamState {
                    turn_id,
                    message_id: message_id.clone(),
                    generated_identity: None,
                    text: String::new(),
                    reasoning: String::new(),
                    reasoning_only: false,
                    stream_published: false,
                    images,
                });
                return CodexAgentMessageOpen::Superseded(Box::new(previous));
            }
        }

        let turn_id = state
            .active_turn_id
            .clone()
            .unwrap_or_else(|| message_id.0.clone());
        let images = std::mem::take(&mut state.tool_container_images);
        state.active_stream = Some(ActiveStreamState {
            turn_id,
            message_id: message_id.clone(),
            generated_identity: None,
            text: String::new(),
            reasoning: String::new(),
            reasoning_only: false,
            stream_published: false,
            images,
        });
        CodexAgentMessageOpen::Open
    }

    async fn open_reasoning_message_item(
        &self,
        provider_message_id: Option<ChatMessageId>,
        cause: CodexProviderOpenCause,
        notification_thread_id: Option<&str>,
        notification_turn_id: Option<&str>,
    ) -> CodexAgentMessageOpen {
        let tool_container_was_open = self.state.lock().await.tool_container.is_some();
        self.close_tool_container_if_open().await;
        let mut state = self.state.lock().await;
        if provider_message_id
            .as_ref()
            .is_some_and(|message_id| state.retired_unpublished_message_ids.contains(message_id))
        {
            return CodexAgentMessageOpen::Retired;
        }
        if provider_message_id
            .as_ref()
            .is_some_and(|message_id| state.completed_agent_messages.contains_key(message_id))
        {
            return CodexAgentMessageOpen::Terminal;
        }
        if let Some(stream) = state.active_stream.as_ref() {
            let same_identity = match provider_message_id.as_ref() {
                Some(message_id) => stream.reasoning_only && stream.message_id == *message_id,
                None => {
                    stream.reasoning_only
                        && stream.generated_identity.as_ref().is_some_and(|identity| {
                            identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                        })
                }
            };
            if same_identity {
                return CodexAgentMessageOpen::Existing;
            }
            if provider_message_id.is_some() && stream.is_replaceable_provider_reservation() {
                let retired_id = stream.message_id.clone();
                state.active_stream = None;
                state.retired_unpublished_message_ids.insert(retired_id);
            } else {
                let same_owner =
                    notification_thread_id.is_none_or(|thread_id| thread_id == state.thread_id);
                let same_turn = state.active_turn_id.as_ref().is_some_and(|turn_id| {
                    stream.turn_id == *turn_id
                        && notification_turn_id.is_none_or(|incoming| incoming == turn_id)
                });
                let tool_ownership_is_clear = !tool_container_was_open
                    && state.pending_tool_call_ids.is_empty()
                    && state.tool_container_images.is_empty()
                    && !state.close_active_stream_when_tools_idle;
                let can_supersede = cause == CodexProviderOpenCause::ItemStarted
                    && provider_message_id.is_some()
                    && same_owner
                    && same_turn
                    && stream.generated_identity.is_none()
                    && tool_ownership_is_clear
                    && state.provider_supersessions_this_turn
                        < MAX_CODEX_PROVIDER_SUPERSESSIONS_PER_TURN;
                if !can_supersede {
                    return CodexAgentMessageOpen::Foreign;
                }
                let previous = state
                    .active_stream
                    .take()
                    .expect("eligible Codex provider stream disappeared");
                state.provider_supersessions_this_turn =
                    state.provider_supersessions_this_turn.saturating_add(1);
                let message_id = provider_message_id
                    .clone()
                    .expect("eligible Codex reasoning supersession has a provider id");
                let turn_id = state
                    .active_turn_id
                    .clone()
                    .unwrap_or_else(|| message_id.0.clone());
                let images = std::mem::take(&mut state.tool_container_images);
                state.active_stream = Some(ActiveStreamState {
                    turn_id,
                    message_id,
                    generated_identity: None,
                    text: String::new(),
                    reasoning: String::new(),
                    reasoning_only: true,
                    stream_published: false,
                    images,
                });
                return CodexAgentMessageOpen::Superseded(Box::new(previous));
            }
        }
        let generated_identity = provider_message_id.is_none().then(|| {
            let identity = CodexProviderResponseIdentity {
                origin: CodexProviderResponseOrigin::IdlessReasoning,
                stream_epoch: state.generated_identity_epoch,
                item_ordinal: state.next_generated_identity_ordinal,
            };
            state.next_generated_identity_ordinal =
                state.next_generated_identity_ordinal.saturating_add(1);
            identity
        });
        let message_id = provider_message_id.unwrap_or_else(|| {
            generated_identity
                .as_ref()
                .expect("generated identity")
                .message_id()
        });
        if state.completed_agent_messages.contains_key(&message_id) {
            return CodexAgentMessageOpen::Terminal;
        }
        let turn_id = state
            .active_turn_id
            .clone()
            .unwrap_or_else(|| message_id.0.clone());
        let images = std::mem::take(&mut state.tool_container_images);
        state.active_stream = Some(ActiveStreamState {
            turn_id,
            message_id: message_id.clone(),
            generated_identity: generated_identity.clone(),
            text: String::new(),
            reasoning: String::new(),
            reasoning_only: true,
            stream_published: false,
            images,
        });
        CodexAgentMessageOpen::Open
    }

    async fn finalize_root_provider_stream(
        &self,
        stream: ActiveStreamState,
        finalization: CodexProviderStreamFinalization,
    ) -> FinalizedCodexProviderItem {
        let kind = if stream.reasoning_only {
            CodexProviderItemKind::Reasoning
        } else {
            CodexProviderItemKind::AgentMessage
        };
        let provider_completed = matches!(
            &finalization,
            CodexProviderStreamFinalization::Completed { .. }
        );
        let (reported_text, reported_reasoning, content, reasoning) = match (kind, finalization) {
            (
                CodexProviderItemKind::AgentMessage,
                CodexProviderStreamFinalization::Completed { text, reasoning },
            ) => {
                let content = if contains_non_whitespace(&text) {
                    text.clone()
                } else {
                    stream.text.clone()
                };
                let resolved_reasoning = if stream.reasoning.trim().is_empty() {
                    reasoning.clone()
                } else {
                    Some(stream.reasoning.clone())
                }
                .filter(|reasoning| contains_non_whitespace(reasoning));
                (text, reasoning, content, resolved_reasoning)
            }
            (
                CodexProviderItemKind::Reasoning,
                CodexProviderStreamFinalization::Completed { reasoning, .. },
            ) => {
                let resolved_reasoning = reasoning.clone().or_else(|| {
                    contains_non_whitespace(&stream.reasoning).then_some(stream.reasoning.clone())
                });
                (String::new(), reasoning, String::new(), resolved_reasoning)
            }
            (_, CodexProviderStreamFinalization::Superseded)
            | (_, CodexProviderStreamFinalization::TurnAborted) => {
                let content = if kind == CodexProviderItemKind::AgentMessage {
                    stream.text.clone()
                } else {
                    String::new()
                };
                let reasoning =
                    contains_non_whitespace(&stream.reasoning).then_some(stream.reasoning.clone());
                (content.clone(), reasoning.clone(), content, reasoning)
            }
        };
        let images = stream.images;
        let renderable =
            codex_message_is_renderable(&content, reasoning.as_deref(), 0, images.len());
        let completion = CompletedCodexAgentMessage {
            reported_text,
            reported_reasoning,
            completion_text: content.clone(),
            completion_reasoning: reasoning.clone(),
        };
        let model = {
            let mut state = self.state.lock().await;
            if kind == CodexProviderItemKind::AgentMessage {
                state.close_active_stream_when_tools_idle = false;
                if provider_completed {
                    state.foreground_response_completed = true;
                }
            }
            let model = state
                .effective_model
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            if renderable && self.emitter.open_response().is_none() {
                let response = self.emitter.ensure_open_response(Some(&model));
                if let Some(reasoning) = reasoning.as_deref() {
                    self.emitter.stream_reasoning_delta(&response, reasoning);
                }
            }
            let metadata = match kind {
                CodexProviderItemKind::AgentMessage if renderable => {
                    metadata_target_for_visible_message(
                        stream.turn_id.clone(),
                        self.emitter
                            .open_response()
                            .expect("renderable response")
                            .message_id(),
                        &content,
                        reasoning.as_deref(),
                        model.clone(),
                    )
                }
                CodexProviderItemKind::Reasoning => reasoning.as_ref().and_then(|_| {
                    metadata_target_for_visible_message(
                        stream.turn_id.clone(),
                        self.emitter
                            .open_response()
                            .expect("renderable response")
                            .message_id(),
                        "",
                        reasoning.as_deref(),
                        model.clone(),
                    )
                }),
                CodexProviderItemKind::AgentMessage => None,
            };
            if kind == CodexProviderItemKind::Reasoning {
                state.pending_message_metadata = metadata;
            } else if let Some(metadata) = metadata {
                state.pending_message_metadata = Some(metadata);
            }
            state
                .completed_agent_messages
                .insert(stream.message_id.clone(), completion);
            // The Superseded/TurnAborted callers install their own tombstones;
            // normal completion installs one here so delayed starts and deltas
            // for this id are absorbed instead of tripping the duplicate-
            // terminal violation against an unrelated live turn.
            if provider_completed {
                let owner_thread_id = state.thread_id.clone();
                push_codex_provider_item_tombstone(
                    &mut state.provider_item_tombstones,
                    CodexProviderItemTombstone {
                        owner_thread_id,
                        turn_id: stream.turn_id.clone(),
                        message_id: stream.message_id.clone(),
                        kind,
                        disposition: CodexProviderItemDisposition::Completed,
                        accepted_text: content.clone(),
                        accepted_reasoning: reasoning.clone().unwrap_or_default(),
                        late_text: String::new(),
                        late_reasoning: String::new(),
                        late_event_count: 0,
                        late_bytes: 0,
                    },
                );
            }
            model
        };
        if !renderable && !stream.stream_published {
            match kind {
                CodexProviderItemKind::AgentMessage => tracing::debug!(
                    provider_item_id = stream.message_id.0.as_str(),
                    item_type = "agentMessage",
                    text_bytes = content.len(),
                    reasoning_bytes = reasoning.as_ref().map_or(0, String::len),
                    "Suppressed contentless Codex completion"
                ),
                CodexProviderItemKind::Reasoning => tracing::debug!(
                    provider_item_id = stream.message_id.0.as_str(),
                    item_type = "reasoning",
                    reasoning_bytes = 0,
                    "Suppressed contentless Codex completion"
                ),
            }
            return FinalizedCodexProviderItem {
                turn_id: stream.turn_id,
                message_id: stream.message_id,
                kind,
                content,
                reasoning,
                emitted: false,
            };
        }
        self.trace_terminal_emission("stream_end", Some(&stream.message_id.0))
            .await;
        self.emitter.stream_end(
            self.emitter
                .open_response()
                .expect("renderable Codex response"),
            StreamEndPayload {
                content: content.clone(),
                model_info: Some(ModelInfo { model }),
                reasoning: reasoning.clone().map(reasoning_data),
                tool_calls: Vec::new(),
                context_breakdown: None,
                images,
                ..StreamEndPayload::default()
            },
        );
        FinalizedCodexProviderItem {
            turn_id: stream.turn_id,
            message_id: stream.message_id,
            kind,
            content,
            reasoning,
            emitted: true,
        }
    }

    async fn finalize_root_provider_supersession(
        &self,
        previous: ActiveStreamState,
        incoming_message_id: &ChatMessageId,
        incoming_kind: CodexProviderItemKind,
    ) {
        let finalized = self
            .finalize_root_provider_stream(previous, CodexProviderStreamFinalization::Superseded)
            .await;
        let (thread_id, recovery_ordinal, emit_warning) = {
            let mut state = self.state.lock().await;
            let thread_id = state.thread_id.clone();
            push_codex_provider_item_tombstone(
                &mut state.provider_item_tombstones,
                CodexProviderItemTombstone {
                    owner_thread_id: thread_id.clone(),
                    turn_id: finalized.turn_id.clone(),
                    message_id: finalized.message_id.clone(),
                    kind: finalized.kind,
                    disposition: CodexProviderItemDisposition::Superseded,
                    accepted_text: finalized.content.clone(),
                    accepted_reasoning: finalized.reasoning.clone().unwrap_or_default(),
                    late_text: String::new(),
                    late_reasoning: String::new(),
                    late_event_count: 0,
                    late_bytes: 0,
                },
            );
            let emit_warning = !state.supersession_warning_emitted;
            state.supersession_warning_emitted = true;
            (
                thread_id,
                state.provider_supersessions_this_turn,
                emit_warning,
            )
        };
        tracing::warn!(
            thread_id,
            turn_id = finalized.turn_id.as_str(),
            superseded_item_id = finalized.message_id.0.as_str(),
            incoming_item_id = incoming_message_id.0.as_str(),
            superseded_kind = ?finalized.kind,
            incoming_kind = ?incoming_kind,
            recovery_ordinal,
            accepted_text_bytes = finalized.content.len(),
            accepted_reasoning_bytes = finalized.reasoning.as_ref().map_or(0, String::len),
            stream_terminal_emitted = finalized.emitted,
            "Recovered Codex provider-item supersession"
        );
        if emit_warning {
            self.emitter.warning_message(CODEX_SUPERSESSION_WARNING);
        }
    }

    async fn handle_root_late_provider_event(
        &self,
        message_id: &ChatMessageId,
        event: CodexLateProviderEvent,
        method: &str,
    ) -> bool {
        let (thread_id, outcome, accepted_text_bytes, accepted_reasoning_bytes) = {
            let mut state = self.state.lock().await;
            let thread_id = state.thread_id.clone();
            let active_turn_id = state.active_turn_id.clone();
            let outcome = classify_codex_late_provider_event(
                &mut state.provider_item_tombstones,
                &thread_id,
                active_turn_id.as_deref(),
                message_id,
                &event,
            );
            let lengths = state
                .provider_item_tombstones
                .iter()
                .rev()
                .find(|tombstone| {
                    tombstone.owner_thread_id == thread_id && tombstone.message_id == *message_id
                })
                .map(|tombstone| {
                    (
                        tombstone.accepted_text.len(),
                        tombstone.accepted_reasoning.len(),
                    )
                })
                .unwrap_or_default();
            (thread_id, outcome, lengths.0, lengths.1)
        };
        match outcome {
            CodexLateProviderEventOutcome::NotFound => false,
            CodexLateProviderEventOutcome::Absorbed {
                first,
                turn_id,
                disposition,
            } => {
                if first && disposition != CodexProviderItemDisposition::Completed {
                    tracing::warn!(
                        thread_id,
                        turn_id,
                        provider_item_id = message_id.0.as_str(),
                        codex_method = method,
                        ?disposition,
                        accepted_text_bytes,
                        accepted_reasoning_bytes,
                        "Absorbing bounded late event for terminalized Codex provider item"
                    );
                } else {
                    tracing::debug!(
                        thread_id,
                        turn_id,
                        provider_item_id = message_id.0.as_str(),
                        codex_method = method,
                        ?disposition,
                        "Absorbing repeated late event for terminalized Codex provider item"
                    );
                }
                true
            }
            CodexLateProviderEventOutcome::Contradiction {
                affected_turn_is_live,
                turn_id,
                disposition,
            } => {
                tracing::warn!(
                    thread_id,
                    turn_id,
                    provider_item_id = message_id.0.as_str(),
                    codex_method = method,
                    ?disposition,
                    affected_turn_is_live,
                    "Contradictory late Codex provider-item event"
                );
                if affected_turn_is_live {
                    self.reject_agent_message_identity(
                        CodexProviderStreamConflict::ConflictingDuplicateCompletion,
                        method,
                        Some(&message_id.0),
                    )
                    .await;
                }
                true
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_subagent_provider_stream(
        stream_key: &str,
        stream: &mut CodexSubAgentStream,
        turn_id_from_params: Option<String>,
        message_id: ChatMessageId,
        kind: CodexProviderItemKind,
        model: &str,
        finalization: CodexProviderStreamFinalization,
    ) -> FinalizedCodexSubAgentProviderItem {
        let provider_completed = matches!(
            &finalization,
            CodexProviderStreamFinalization::Completed { .. }
        );
        let stream_published = stream.current_stream_published;
        let mut response = stream.current_response.take();
        let (reported_text, reported_reasoning, content, reasoning) = match (kind, finalization) {
            (
                CodexProviderItemKind::AgentMessage,
                CodexProviderStreamFinalization::Completed {
                    text,
                    reasoning: reported_reasoning,
                },
            ) => {
                let content = if contains_non_whitespace(&text) {
                    text.clone()
                } else {
                    stream.current_text.clone()
                };
                let reasoning = if stream.current_reasoning.trim().is_empty() {
                    reported_reasoning.clone()
                } else {
                    Some(stream.current_reasoning.clone())
                }
                .filter(|reasoning| contains_non_whitespace(reasoning));
                (text, reported_reasoning, content, reasoning)
            }
            (
                CodexProviderItemKind::Reasoning,
                CodexProviderStreamFinalization::Completed {
                    reasoning: reported_reasoning,
                    ..
                },
            ) => {
                let reasoning = reported_reasoning.clone().or_else(|| {
                    contains_non_whitespace(&stream.current_reasoning)
                        .then_some(stream.current_reasoning.clone())
                });
                (String::new(), reported_reasoning, String::new(), reasoning)
            }
            (_, CodexProviderStreamFinalization::Superseded)
            | (_, CodexProviderStreamFinalization::TurnAborted) => {
                let content = if kind == CodexProviderItemKind::AgentMessage {
                    stream.current_text.clone()
                } else {
                    String::new()
                };
                let reasoning = contains_non_whitespace(&stream.current_reasoning)
                    .then_some(stream.current_reasoning.clone());
                (content.clone(), reasoning.clone(), content, reasoning)
            }
        };
        let renderable = codex_message_is_renderable(
            &content,
            reasoning.as_deref(),
            stream.current_tool_call_ids.len(),
            stream.current_images.len(),
        );
        let completion = CompletedCodexAgentMessage {
            reported_text,
            reported_reasoning,
            completion_text: content.clone(),
            completion_reasoning: reasoning.clone(),
        };
        let turn_id = turn_id_from_params
            .or_else(|| stream.active_turn_id.clone())
            .unwrap_or_else(|| "turn".to_string());
        stream.current_message_id = None;
        stream.current_generated_identity = None;
        stream.current_reasoning_only = false;
        stream.current_stream_published = false;
        stream.current_text.clear();
        stream.current_reasoning.clear();
        stream.current_tool_call_ids.clear();
        let images = std::mem::take(&mut stream.current_images);
        stream
            .completed_agent_messages
            .insert(message_id.clone(), completion);
        // Mirrors the root finalize: the Superseded/TurnAborted callers push
        // their own tombstones, while normal completion records one here so
        // delayed starts and deltas do not terminate a live child turn.
        if provider_completed {
            push_codex_provider_item_tombstone(
                &mut stream.provider_item_tombstones,
                CodexProviderItemTombstone {
                    owner_thread_id: stream_key.to_string(),
                    turn_id: turn_id.clone(),
                    message_id: message_id.clone(),
                    kind,
                    disposition: CodexProviderItemDisposition::Completed,
                    accepted_text: content.clone(),
                    accepted_reasoning: reasoning.clone().unwrap_or_default(),
                    late_text: String::new(),
                    late_reasoning: String::new(),
                    late_event_count: 0,
                    late_bytes: 0,
                },
            );
        }
        if !renderable && !stream_published {
            match kind {
                CodexProviderItemKind::AgentMessage => tracing::debug!(
                    provider_item_id = message_id.0.as_str(),
                    item_type = "agentMessage",
                    text_bytes = content.len(),
                    reasoning_bytes = reasoning.as_ref().map_or(0, String::len),
                    "Suppressed contentless Codex child completion"
                ),
                CodexProviderItemKind::Reasoning => tracing::debug!(
                    provider_item_id = message_id.0.as_str(),
                    item_type = "reasoning",
                    reasoning_bytes = 0,
                    "Suppressed contentless Codex child completion"
                ),
            }
            return FinalizedCodexSubAgentProviderItem {
                emitter: Arc::clone(&stream.emitter),
                turn_id,
                message_id,
                kind,
                response,
                emitted: false,
                content,
                reasoning,
                token_usage: None,
                unavailable_reason: None,
                images,
            };
        }
        if response.is_none() {
            response = Some(stream.emitter.stream_start(Some(model)));
            if let Some(reasoning) = reasoning.as_deref() {
                stream.emitter.stream_reasoning_delta(
                    response.as_ref().expect("renderable child response"),
                    reasoning,
                );
            }
        }
        let presentation_message_id = response
            .as_ref()
            .expect("renderable child response")
            .message_id();
        let (token_usage, unavailable_reason) = if provider_completed {
            let token_usage = stream.token_usage_by_turn.remove(&turn_id);
            let unavailable_reason = if token_usage.is_some() {
                None
            } else {
                stream.pending_message_metadata = Some(PendingCodexMessageMetadata {
                    turn_id: turn_id.clone(),
                    message_id: presentation_message_id.clone(),
                    model: model.to_string(),
                });
                Some(TokenUsageUnavailableReason::BackendDidNotReport)
            };
            (token_usage, unavailable_reason)
        } else {
            stream.pending_message_metadata = Some(PendingCodexMessageMetadata {
                turn_id: turn_id.clone(),
                message_id: presentation_message_id,
                model: model.to_string(),
            });
            (None, None)
        };
        FinalizedCodexSubAgentProviderItem {
            emitter: Arc::clone(&stream.emitter),
            turn_id,
            message_id,
            kind,
            response,
            emitted: true,
            content,
            reasoning,
            token_usage,
            unavailable_reason,
            images,
        }
    }

    fn emit_finalized_subagent_provider_item(
        mut finalized: FinalizedCodexSubAgentProviderItem,
        model: &str,
    ) {
        if !finalized.emitted {
            return;
        }
        if finalized.response.is_none() {
            finalized.response = Some(finalized.emitter.stream_start(Some(model)));
            if let Some(reasoning) = finalized.reasoning.as_deref() {
                finalized.emitter.stream_reasoning_delta(
                    finalized.response.as_ref().expect("response"),
                    reasoning,
                );
            }
        }
        let token_usage = finalized
            .token_usage
            .as_ref()
            .and_then(codex_token_usage)
            .map(MessageTokenUsage::request_known)
            .or_else(|| {
                finalized
                    .unavailable_reason
                    .map(MessageTokenUsage::unavailable)
            });
        finalized.emitter.stream_end(
            finalized
                .response
                .take()
                .expect("renderable child response"),
            StreamEndPayload {
                content: finalized.content,
                model_info: Some(ModelInfo {
                    model: model.to_owned(),
                }),
                token_usage,
                reasoning: finalized.reasoning.map(reasoning_data),
                tool_calls: Vec::new(),
                images: finalized.images,
                ..StreamEndPayload::default()
            },
        );
    }

    async fn finalize_subagent_provider_supersession(
        &self,
        stream_key: &str,
        finalized: FinalizedCodexSubAgentProviderItem,
        incoming_message_id: &ChatMessageId,
        incoming_kind: CodexProviderItemKind,
        model: &str,
    ) {
        let emit_warning = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            push_codex_provider_item_tombstone(
                &mut stream.provider_item_tombstones,
                CodexProviderItemTombstone {
                    owner_thread_id: stream_key.to_string(),
                    turn_id: finalized.turn_id.clone(),
                    message_id: finalized.message_id.clone(),
                    kind: finalized.kind,
                    disposition: CodexProviderItemDisposition::Superseded,
                    accepted_text: finalized.content.clone(),
                    accepted_reasoning: finalized.reasoning.clone().unwrap_or_default(),
                    late_text: String::new(),
                    late_reasoning: String::new(),
                    late_event_count: 0,
                    late_bytes: 0,
                },
            );
            let emit_warning = !stream.supersession_warning_emitted;
            stream.supersession_warning_emitted = true;
            emit_warning
        };
        tracing::warn!(
            child_thread_id = stream_key,
            turn_id = finalized.turn_id.as_str(),
            superseded_item_id = finalized.message_id.0.as_str(),
            incoming_item_id = incoming_message_id.0.as_str(),
            superseded_kind = ?finalized.kind,
            incoming_kind = ?incoming_kind,
            accepted_text_bytes = finalized.content.len(),
            accepted_reasoning_bytes = finalized.reasoning.as_ref().map_or(0, String::len),
            "Recovered Codex child provider-item supersession"
        );
        let emitter = Arc::clone(&finalized.emitter);
        Self::emit_finalized_subagent_provider_item(finalized, model);
        if emit_warning {
            emitter.warning_message(CODEX_SUPERSESSION_WARNING);
        }
    }

    async fn handle_subagent_late_provider_event(
        &self,
        stream_key: &str,
        message_id: &ChatMessageId,
        event: CodexLateProviderEvent,
        method: &str,
    ) -> bool {
        let outcome = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return false;
            };
            classify_codex_late_provider_event(
                &mut stream.provider_item_tombstones,
                stream_key,
                stream.active_turn_id.as_deref(),
                message_id,
                &event,
            )
        };
        match outcome {
            CodexLateProviderEventOutcome::NotFound => false,
            CodexLateProviderEventOutcome::Absorbed {
                first,
                turn_id,
                disposition,
            } => {
                if first && disposition != CodexProviderItemDisposition::Completed {
                    tracing::warn!(
                        child_thread_id = stream_key,
                        turn_id,
                        provider_item_id = message_id.0.as_str(),
                        codex_method = method,
                        ?disposition,
                        "Absorbing bounded late event for terminalized Codex child item"
                    );
                } else {
                    tracing::debug!(
                        child_thread_id = stream_key,
                        turn_id,
                        provider_item_id = message_id.0.as_str(),
                        codex_method = method,
                        ?disposition,
                        "Absorbing repeated late event for terminalized Codex child item"
                    );
                }
                true
            }
            CodexLateProviderEventOutcome::Contradiction {
                affected_turn_is_live,
                turn_id,
                disposition,
            } => {
                tracing::warn!(
                    child_thread_id = stream_key,
                    turn_id,
                    provider_item_id = message_id.0.as_str(),
                    codex_method = method,
                    ?disposition,
                    affected_turn_is_live,
                    "Contradictory late Codex child provider-item event"
                );
                if affected_turn_is_live {
                    self.reject_subagent_message_identity(
                        stream_key,
                        CodexProviderStreamConflict::ConflictingDuplicateCompletion,
                        method,
                    )
                    .await;
                }
                true
            }
        }
    }

    async fn open_subagent_provider_item(
        &self,
        stream_key: &str,
        provider_message_id: Option<ChatMessageId>,
        kind: CodexProviderItemKind,
        cause: CodexProviderOpenCause,
        notification_owner: CodexProviderNotificationOwner<'_>,
        model: &str,
    ) -> CodexSubAgentMessageOpen {
        let tool_container_was_open = {
            let state = self.state.lock().await;
            state
                .subagent_streams
                .get(stream_key)
                .is_some_and(|stream| stream.tool_container.is_some())
        };
        self.close_subagent_tool_container_if_open(stream_key).await;
        let mut state = self.state.lock().await;
        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
            return CodexSubAgentMessageOpen::Foreign;
        };
        if provider_message_id
            .as_ref()
            .is_some_and(|message_id| stream.retired_unpublished_message_ids.contains(message_id))
        {
            return CodexSubAgentMessageOpen::Retired;
        }
        if provider_message_id
            .as_ref()
            .is_some_and(|message_id| stream.completed_agent_messages.contains_key(message_id))
        {
            return CodexSubAgentMessageOpen::Terminal;
        }
        let same_identity = stream
            .current_message_id
            .as_ref()
            .is_some_and(|active_message_id| match provider_message_id.as_ref() {
                Some(message_id) => {
                    active_message_id == message_id
                        && stream.current_reasoning_only
                            == (kind == CodexProviderItemKind::Reasoning)
                }
                None => {
                    kind == CodexProviderItemKind::Reasoning
                        && stream.current_reasoning_only
                        && stream
                            .current_generated_identity
                            .as_ref()
                            .is_some_and(|identity| {
                                identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                            })
                }
            });
        if same_identity {
            return CodexSubAgentMessageOpen::Existing;
        }
        if stream.current_message_id.is_some() {
            let retired =
                provider_message_id.is_some() && stream.retire_replaceable_provider_reservation();
            if !retired {
                let same_owner = notification_owner
                    .thread_id
                    .is_none_or(|thread_id| thread_id == stream_key);
                // Child turn boundaries clear current_message_id, so any live
                // child item is owned by active_turn_id without a second item
                // turn field to compare as the root path does.
                let same_turn = stream.active_turn_id.as_ref().is_some_and(|turn_id| {
                    notification_owner
                        .turn_id
                        .is_none_or(|incoming| incoming == turn_id)
                });
                let tool_ownership_is_clear = !tool_container_was_open
                    && stream.pending_tool_call_ids.is_empty()
                    && stream.tool_container_images.is_empty();
                let can_supersede = cause == CodexProviderOpenCause::ItemStarted
                    && provider_message_id.is_some()
                    && same_owner
                    && same_turn
                    && stream.current_generated_identity.is_none()
                    && tool_ownership_is_clear
                    && stream.provider_supersessions_this_turn
                        < MAX_CODEX_PROVIDER_SUPERSESSIONS_PER_TURN;
                if !can_supersede {
                    return CodexSubAgentMessageOpen::Foreign;
                }
                let previous_message_id = stream
                    .current_message_id
                    .clone()
                    .expect("eligible Codex child provider stream has an id");
                let previous_kind = if stream.current_reasoning_only {
                    CodexProviderItemKind::Reasoning
                } else {
                    CodexProviderItemKind::AgentMessage
                };
                let finalized = Self::finalize_subagent_provider_stream(
                    stream_key,
                    stream,
                    None,
                    previous_message_id,
                    previous_kind,
                    model,
                    CodexProviderStreamFinalization::Superseded,
                );
                stream.provider_supersessions_this_turn =
                    stream.provider_supersessions_this_turn.saturating_add(1);
                let message_id = provider_message_id
                    .clone()
                    .expect("eligible Codex child supersession has a provider id");
                stream.current_message_id = Some(message_id);
                stream.current_response = None;
                stream.current_generated_identity = None;
                stream.current_reasoning_only = kind == CodexProviderItemKind::Reasoning;
                stream.current_stream_published = false;
                stream.current_text.clear();
                stream.current_reasoning.clear();
                stream.current_tool_call_ids.clear();
                stream.current_images.clear();
                return CodexSubAgentMessageOpen::Superseded(Box::new(finalized));
            }
        }
        let generated_identity = provider_message_id.is_none().then(|| {
            let identity = CodexProviderResponseIdentity {
                origin: CodexProviderResponseOrigin::IdlessReasoning,
                stream_epoch: stream.generated_identity_epoch,
                item_ordinal: stream.next_generated_identity_ordinal,
            };
            stream.next_generated_identity_ordinal =
                stream.next_generated_identity_ordinal.saturating_add(1);
            identity
        });
        let message_id = provider_message_id.unwrap_or_else(|| {
            generated_identity
                .as_ref()
                .expect("idless child provider item must be reasoning")
                .message_id()
        });
        if stream.completed_agent_messages.contains_key(&message_id) {
            return CodexSubAgentMessageOpen::Terminal;
        }
        stream.current_message_id = Some(message_id);
        stream.current_response = None;
        stream.current_generated_identity = generated_identity;
        stream.current_reasoning_only = kind == CodexProviderItemKind::Reasoning;
        stream.current_stream_published = false;
        stream.current_text.clear();
        stream.current_reasoning.clear();
        stream.current_tool_call_ids.clear();
        stream.current_images.clear();
        CodexSubAgentMessageOpen::Open
    }

    async fn append_text_to_active_stream(
        &self,
        message_id: &ChatMessageId,
        delta: &str,
    ) -> Option<(ResponseHandle, String)> {
        let mut state = self.state.lock().await;
        let model = state
            .effective_model
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        let stream = state
            .active_stream
            .as_mut()
            .filter(|stream| &stream.message_id == message_id)?;
        let was_published = stream.stream_published;
        stream.text.push_str(delta);
        if !stream.stream_published && contains_non_whitespace(&stream.text) {
            stream.stream_published = true;
        }
        if !stream.stream_published {
            return None;
        }
        let emitted = if was_published {
            delta.to_string()
        } else {
            stream.text.clone()
        };
        let response = self.emitter.ensure_open_response(Some(&model));
        Some((response, emitted))
    }

    async fn reject_agent_message_identity(
        &self,
        violation: CodexProviderStreamConflict,
        method: &str,
        provider_item_id: Option<&str>,
    ) {
        let (
            thread_id,
            turn_id,
            provider_turn_id,
            active_stream,
            active_item_id,
            active_buffer_len,
            stream_published,
            terminated_background_commands,
        ) = {
            let mut state = self.state.lock().await;
            let active_stream = state.active_stream.take();
            let provider_turn_id = state
                .active_turn_id
                .clone()
                .or_else(|| active_stream.as_ref().map(|stream| stream.turn_id.clone()));
            let turn_id = provider_turn_id
                .clone()
                .unwrap_or_else(|| "<no-active-turn>".to_string());
            if !push_codex_terminated_turn(&mut state.terminated_turns, turn_id.clone()) {
                state.active_stream = active_stream;
                return;
            }
            state.terminated_turn_awaiting_replacement = Some(turn_id.clone());
            let active_item_id = active_stream
                .as_ref()
                .map(|stream| stream.message_id.clone());
            let active_buffer_len = active_stream.as_ref().map_or(0, |stream| {
                stream.text.len().saturating_add(stream.reasoning.len())
            });
            let stream_published = active_stream
                .as_ref()
                .is_some_and(|stream| stream.stream_published);
            let interrupted_tool_call_ids = state
                .pending_tool_call_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            state
                .cancelled_tool_call_ids
                .extend(interrupted_tool_call_ids);
            state.active_turn_id = None;
            state.tool_container = None;
            state.pending_tool_call_ids.clear();
            state.tool_container_images.clear();
            state.close_active_stream_when_tools_idle = false;
            state.pending_request = None;
            state.file_change_call_ids.clear();
            state.pending_message_metadata = None;
            let root_thread_id = state.thread_id.clone();
            let terminated_background_commands =
                take_codex_commands_for_turn(&mut state, &root_thread_id, &turn_id);
            (
                root_thread_id,
                turn_id,
                provider_turn_id,
                active_stream,
                active_item_id,
                active_buffer_len,
                stream_published,
                terminated_background_commands,
            )
        };
        if let Some(stream) = active_stream {
            let finalized = self
                .finalize_root_provider_stream(stream, CodexProviderStreamFinalization::TurnAborted)
                .await;
            let mut state = self.state.lock().await;
            push_codex_provider_item_tombstone(
                &mut state.provider_item_tombstones,
                CodexProviderItemTombstone {
                    owner_thread_id: thread_id.clone(),
                    turn_id: finalized.turn_id,
                    message_id: finalized.message_id,
                    kind: finalized.kind,
                    disposition: CodexProviderItemDisposition::TurnTerminated,
                    accepted_text: finalized.content,
                    accepted_reasoning: finalized.reasoning.unwrap_or_default(),
                    late_text: String::new(),
                    late_reasoning: String::new(),
                    late_event_count: 0,
                    late_bytes: 0,
                },
            );
            state.pending_message_metadata = None;
        }
        for command in terminated_background_commands {
            self.emitter.cancel_pending_tool(
                &command.tool_call_id,
                "Codex background command was cancelled after a stream identity violation",
            );
        }
        tracing::warn!(
            codex_method = method,
            thread_id = thread_id.as_str(),
            turn_id = turn_id.as_str(),
            ?provider_item_id,
            ?active_item_id,
            active_buffer_len,
            stream_published,
            ?violation,
            "Terminating Codex turn after provider-item identity violation"
        );
        self.emitter
            .backend_error(codex_stream_identity_violation_message(violation));
        self.emitter
            .operation_cancelled("Stream identity violation");
        if let Some(provider_turn_id) = provider_turn_id {
            self.rpc.spawn_request(
                "turn/interrupt",
                json!({
                    "threadId": thread_id,
                    "turnId": provider_turn_id,
                }),
            );
        }
    }

    async fn reject_impossible_delta_supersession(
        &self,
        previous: ActiveStreamState,
        method: &str,
        provider_item_id: Option<&str>,
    ) {
        let previous_item_id = previous.message_id.clone();
        let displaced_item_id = {
            let mut state = self.state.lock().await;
            let displaced = state.active_stream.replace(previous);
            state.provider_supersessions_this_turn =
                state.provider_supersessions_this_turn.saturating_sub(1);
            displaced.map(|stream| stream.message_id)
        };
        tracing::error!(
            codex_method = method,
            ?provider_item_id,
            previous_item_id = previous_item_id.0.as_str(),
            ?displaced_item_id,
            "Restored accepted Codex stream after impossible delta supersession"
        );
        self.reject_agent_message_identity(
            CodexProviderStreamConflict::ForeignActiveMessageId,
            method,
            provider_item_id,
        )
        .await;
    }

    async fn append_reasoning_to_active_stream(&self, reasoning: &str) {
        let emission = {
            let mut state = self.state.lock().await;
            let model = state
                .effective_model
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            if let Some(stream) = state.active_stream.as_mut() {
                if stream.reasoning.split('\n').any(|line| line == reasoning) {
                    None
                } else {
                    let was_published = stream.stream_published;
                    if !stream.reasoning.is_empty() && !stream.reasoning.ends_with('\n') {
                        stream.reasoning.push('\n');
                    }
                    stream.reasoning.push_str(reasoning);
                    if !stream.stream_published && contains_non_whitespace(&stream.reasoning) {
                        stream.stream_published = true;
                    }
                    stream.stream_published.then(|| {
                        let response = self.emitter.ensure_open_response(Some(&model));
                        let reasoning = if was_published {
                            reasoning.to_string()
                        } else {
                            stream.reasoning.clone()
                        };
                        (response, reasoning)
                    })
                }
            } else {
                None
            }
        };
        if let Some((response, reasoning)) = emission {
            self.emitter.stream_reasoning_delta(&response, &reasoning);
        }
    }

    async fn track_tool_requests(&self, tool_call_ids: impl IntoIterator<Item = String>) {
        let mut state = self.state.lock().await;
        for tool_call_id in tool_call_ids {
            state.cancelled_tool_call_ids.remove(&tool_call_id);
            state.pending_tool_call_ids.insert(tool_call_id);
        }
    }

    async fn suppress_cancelled_tool_completion(&self, tool_call_id: &str) -> bool {
        let suppressed = {
            let mut state = self.state.lock().await;
            !state.pending_tool_call_ids.contains(tool_call_id)
                && state.cancelled_tool_call_ids.remove(tool_call_id)
        };
        if suppressed {
            tracing::debug!(
                tool_call_id,
                "Ignoring late Codex tool completion from an interrupted turn"
            );
        }
        suppressed
    }

    async fn close_tool_container_if_open(&self) {
        let mut state = self.state.lock().await;
        state.tool_container = None;
        if state.active_stream.is_some() {
            let images = std::mem::take(&mut state.tool_container_images);
            state
                .active_stream
                .as_mut()
                .expect("active Codex response")
                .images
                .extend(images);
        }
    }

    async fn mark_tool_completed(&self, tool_call_id: &str) {
        self.mark_tool_completed_with_images(tool_call_id, Vec::new())
            .await;
    }

    async fn mark_tool_completed_with_images(
        &self,
        tool_call_id: &str,
        images: Vec<protocol::ImageData>,
    ) {
        let emit_deferred_idle = {
            let mut state = self.state.lock().await;
            state.tool_container_images.extend(images);
            state.pending_tool_call_ids.remove(tool_call_id);
            if std::env::var_os("TYDE_CODEX_TRACE_TOOL_STATE").is_some() {
                tracing::debug!(
                    tool_call_id,
                    pending = state.pending_tool_call_ids.len(),
                    container = state.tool_container.is_some(),
                    active_stream = state.active_stream.is_some(),
                    images = state.tool_container_images.len(),
                    "Codex tool completed"
                );
            }
            if state.pending_tool_call_ids.is_empty() {
                state.tool_container = None;
                if state.active_stream.is_some() {
                    let images = std::mem::take(&mut state.tool_container_images);
                    state
                        .active_stream
                        .as_mut()
                        .expect("active Codex stream disappeared")
                        .images
                        .extend(images);
                }
            }
            let emit_deferred_idle = state.pending_tool_call_ids.is_empty()
                && state.close_active_stream_when_tools_idle
                && state.active_turn_id.is_none()
                && !state.awaiting_root_turn_start;
            if emit_deferred_idle {
                // Keep the state transition and event emission atomic with
                // `turn/started`: a completion from the previous turn must not
                // publish stale idle after a newer turn has become active.
                state.close_active_stream_when_tools_idle = false;
                self.emitter.typing_status_changed(false);
            }
            emit_deferred_idle
        };
        if emit_deferred_idle {
            tracing::debug!(
                tool_call_id,
                "Codex foreground tools completed after their provider turn"
            );
        }
    }

    async fn execute(&self, command: SessionCommand) -> Result<(), String> {
        match command {
            SessionCommand::SendMessage { message, images } => {
                self.emit_user_message_added(&message, images.as_deref());
                // UI contract: show typing immediately when a user turn is submitted,
                // without waiting for Codex to acknowledge turn/started.
                self.emitter.typing_status_changed(true);

                if self.respond_pending_request(&message).await? {
                    return Ok(());
                }

                let (
                    thread_id,
                    model_override,
                    effort_override,
                    approval_policy_override,
                    access_mode,
                    execution_mode,
                    turn_network_access,
                ) = {
                    let mut state = self.state.lock().await;
                    // This send supersedes any cancel that raced an earlier
                    // turn start; the next turn/started belongs to it.
                    state.interrupt_next_root_turn = false;
                    state.awaiting_root_turn_start = true;
                    let (model_override, effort_override) = match state.execution_mode {
                        BackendExecutionMode::Agent => (
                            state.model_override.clone(),
                            state.reasoning_effort_override.clone(),
                        ),
                        BackendExecutionMode::InferenceOnly => (None, None),
                    };
                    (
                        state.thread_id.clone(),
                        model_override,
                        effort_override,
                        state.approval_policy.clone(),
                        state.access_mode,
                        state.execution_mode,
                        state.turn_network_access,
                    )
                };

                let mut input_items = vec![json!({
                    "type": "text",
                    "text": message,
                    "text_elements": []
                })];

                if let Some(imgs) = images {
                    for image in imgs {
                        let path = persist_temp_image(&image).await?;
                        input_items.push(json!({
                            "type": "localImage",
                            "path": path
                        }));
                    }
                }

                let mut params = json!({
                    "threadId": thread_id,
                    "input": input_items
                });

                if let Some(model) = model_override {
                    params["model"] = Value::String(model);
                }
                if let Some(effort) = effort_override {
                    params["effort"] = Value::String(effort);
                }
                params["summary"] = Value::String(CODEX_REASONING_SUMMARY_LEVEL.to_string());
                let approval_policy = approval_policy_override
                    .unwrap_or_else(|| codex_approval_policy(execution_mode).to_string());
                params["approvalPolicy"] = Value::String(approval_policy);
                params["sandboxPolicy"] =
                    codex_sandbox_policy(access_mode, turn_network_access, execution_mode);

                let turn_start = self.rpc.request("turn/start", params).await;
                eprintln!("TYDE CODEX TURN START RESPONSE result={turn_start:?}");
                if let Err(err) = turn_start {
                    self.state.lock().await.awaiting_root_turn_start = false;
                    self.emitter.typing_status_changed(false);
                    return Err(err);
                }
                Ok(())
            }
            SessionCommand::CancelConversation => {
                let compaction_start_pending = {
                    let state = self.state.lock().await;
                    state.active_turn_id.is_none()
                        && state
                            .pending_compaction
                            .as_ref()
                            .is_some_and(|pending| pending.turn_id.is_none())
                };
                if compaction_start_pending {
                    tokio::time::timeout(Duration::from_secs(30), async {
                        loop {
                            let start_resolved = {
                                let state = self.state.lock().await;
                                state
                                    .pending_compaction
                                    .as_ref()
                                    .is_none_or(|pending| pending.turn_id.is_some())
                            };
                            if start_resolved {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .map_err(|_| {
                        "Codex compaction did not publish a turn to interrupt".to_string()
                    })?;
                }
                let (
                    thread_id,
                    turn_id_opt,
                    interrupting_compaction,
                    foreground_ended_with_background_work,
                ) = {
                    let state = self.state.lock().await;
                    let active_turn_id = state.active_turn_id.clone();
                    let compaction_turn_id = state
                        .pending_compaction
                        .as_ref()
                        .and_then(|pending| pending.turn_id.clone());
                    (
                        state.thread_id.clone(),
                        active_turn_id
                            .clone()
                            .or_else(|| compaction_turn_id.clone()),
                        active_turn_id.is_none() && compaction_turn_id.is_some(),
                        (!state.background_commands.is_empty()
                            || !state.subagent_streams.is_empty())
                            && (state.active_turn_id.is_none()
                                || state.foreground_response_completed),
                    )
                };
                eprintln!(
                    "TYDE CODEX CANCEL STATE turn_id={turn_id_opt:?} interrupting_compaction={interrupting_compaction} compaction_start_pending={compaction_start_pending}"
                );
                if foreground_ended_with_background_work {
                    self.emitter.interrupt_acknowledged(
                        "Codex foreground turn already ended; background work continues.",
                    );
                    return Ok(());
                }
                let Some(turn_id) = turn_id_opt else {
                    // No tracked turn to interrupt. A turn may still be
                    // spooling (turn/start dispatched, turn/started not yet
                    // delivered), so latch an interrupt for the next root
                    // turn and resolve the cancel now — returning a silent
                    // Ok here leaves the client stuck on "Cancelling…"
                    // forever waiting for events that will never come.
                    // operation_cancelled ends with TypingStatusChanged(false).
                    let mut state = self.state.lock().await;
                    state.interrupt_next_root_turn = true;
                    state.awaiting_root_turn_start = false;
                    drop(state);
                    self.emitter.operation_cancelled("Operation cancelled");
                    return Ok(());
                };
                // Before the interrupt, so the kill goes out while Codex still
                // tracks the process. The cards for these commands are marked
                // cancelled when the interrupted turn completes; killing here
                // is what makes that report true.
                self.terminate_foreground_commands().await;
                match self
                    .rpc
                    .request_typed(
                        "turn/interrupt",
                        json!({
                            "threadId": thread_id,
                            "turnId": turn_id
                        }),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(error) if error.no_active_turn_to_interrupt() => {
                        eprintln!(
                            "TYDE CODEX INTERRUPT PROVIDER ALREADY TERMINAL turn_id={turn_id}"
                        );
                    }
                    Err(error) => return Err(error.to_string()),
                }
                tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        let terminal = {
                            let state = self.state.lock().await;
                            if interrupting_compaction {
                                state
                                    .pending_compaction
                                    .as_ref()
                                    .and_then(|pending| pending.turn_id.as_deref())
                                    != Some(turn_id.as_str())
                            } else {
                                state.active_turn_id.as_deref() != Some(turn_id.as_str())
                            }
                        };
                        if terminal {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .map_err(|_| format!("Codex turn {turn_id} did not terminalize after interrupt"))?;
                if interrupting_compaction {
                    self.emitter.operation_cancelled("Operation cancelled");
                }
                Ok(())
            }
            SessionCommand::CancelBackgroundTask { tool_call_id } => {
                if self.cancel_background_task(&tool_call_id).await {
                    Ok(())
                } else {
                    Err(format!(
                        "no running Codex background command for card {tool_call_id}"
                    ))
                }
            }
            SessionCommand::GetSettings => {
                // Phase 6 handles config/settings parity. Keep non-failing no-op for now.
                Ok(())
            }
            SessionCommand::ListSessions => self.list_sessions().await,
            SessionCommand::ResumeSession { session_id } => self.resume_session(session_id).await,
            SessionCommand::DeleteSession { session_id } => self.delete_session(session_id).await,
            SessionCommand::ListProfiles => {
                // Phase 6 handles profiles parity.
                Ok(())
            }
            SessionCommand::SwitchProfile { profile_name: _ } => {
                // Phase 6 handles profile switching parity.
                Ok(())
            }
            SessionCommand::GetModuleSchemas => {
                // Phase 6 handles module schema parity.
                Ok(())
            }
            SessionCommand::ListModels => self.list_models().await,
            SessionCommand::UpdateSettings {
                settings,
                persist: _,
            } => {
                self.apply_local_settings(&settings).await;
                Ok(())
            }
        }
    }

    async fn list_sessions(&self) -> Result<(), String> {
        let mut cursor: Option<String> = None;
        let mut sessions: Vec<Value> = Vec::new();

        for _ in 0..20 {
            let mut params = json!({ "limit": 100 });
            if let Some(cur) = cursor.as_ref() {
                params["cursor"] = Value::String(cur.clone());
            }

            let response = self.rpc.request("thread/list", params).await?;
            let page = response
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            if page.is_empty() {
                break;
            }

            for thread in page {
                if let Some(metadata) = codex_thread_to_session_metadata(&thread) {
                    sessions.push(metadata);
                }
            }

            cursor = response
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            if cursor.is_none() || sessions.len() >= 1000 {
                break;
            }
        }

        self.emitter.sessions_list(sessions);
        Ok(())
    }

    async fn resume_session(&self, session_id: String) -> Result<(), String> {
        self.state.lock().await.pending_resume_thread_id = Some(session_id.clone());
        let resumed = async {
            let developer_instructions = self
                .steering_tempfile
                .as_ref()
                .map(std::fs::read_to_string)
                .transpose()
                .map_err(|error| format!("Failed to read Codex resume steering: {error}"))?;
            tracing::debug!(
                steering_bytes = developer_instructions.as_deref().map_or(0, str::len),
                "Reapplying Tyde steering to resumed Codex thread"
            );
            let mut params = json!({ "threadId": session_id });
            if let Some(developer_instructions) = developer_instructions {
                params["developerInstructions"] = Value::String(developer_instructions);
            }
            let response = self
                .rpc
                // Deliberately *not* passing `experimentalRawEvents` here.
                // `ThreadResumeParams` does carry the field (codex 0.146.0), but
                // sending it changes nothing: a resumed thread still emits only
                // typed `item/*` notifications and never a single `rawResponse*`
                // one. Measured, not assumed — see the splitter below.
                .request("thread/resume", params)
                .await?;

            let thread = response
                .get("thread")
                .ok_or("Codex thread/resume response missing thread")?;
            let resumed_thread_id = thread
                .get("id")
                .and_then(Value::as_str)
                .ok_or("Codex thread/resume response missing thread.id")?
                .to_string();
            let resumed_model = response
                .get("model")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            let turns = thread
                .get("turns")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| "Codex resume response missing 'turns' array".to_string())?;
            Ok::<_, String>((resumed_thread_id, resumed_model, turns))
        }
        .await;
        let (resumed_thread_id, resumed_model, turns) = match resumed {
            Ok(resumed) => resumed,
            Err(error) => {
                self.state.lock().await.pending_resume_thread_id = None;
                return Err(error);
            }
        };

        self.drain_background_commands().await;
        self.complete_all_codex_subagents().await;

        {
            let mut state = self.state.lock().await;
            state.pending_resume_thread_id = None;
            state.thread_id = resumed_thread_id;
            let resumed_thread_id = state.thread_id.clone();
            state.response_splitters.clear();
            // A resumed thread gets no `rawResponse*` of any kind — Codex
            // 0.146.0 accepts `experimentalRawEvents` on `thread/resume` and
            // ignores it (openai/codex#34353). Splitting still runs, because the
            // boundary it finalizes on is a `thread/tokenUsage/updated` change,
            // which a resumed thread does emit. Leaving it off is what gave each
            // tool call its own chat message.
            state.response_splitters.insert(
                resumed_thread_id.clone(),
                CodexResponseSplitter::new(&resumed_thread_id, true),
            );
            state.experimental_raw_events_requested = false;
            if let Some(model) = resumed_model.clone() {
                state.effective_model = Some(model);
            }
            state.active_turn_id = None;
            state.active_stream = None;
            state.provider_supersessions_this_turn = 0;
            state.supersession_warning_emitted = false;
            state.provider_item_tombstones.clear();
            state.terminated_turns.clear();
            state.terminated_turn_awaiting_replacement = None;
            state.pending_message_metadata = None;
            state.token_usage_by_turn.clear();
            state.model_token_usage_by_turn.clear();
            state.file_change_call_ids.clear();
            state.background_command_owner_active = true;
            state.pending_background_wakes.clear();
            state.background_wake_request_in_flight = false;
            state.pending_request = None;
        }

        self.emitter.conversation_cleared();
        emit_codex_raw_events_warning_if_needed(self.emitter.as_ref(), false);
        self.emitter.typing_status_changed(false);

        let model = resumed_model.unwrap_or_else(|| "codex".to_string());
        self.emit_resumed_thread_history(&turns, &model).await;

        Ok(())
    }

    async fn delete_session(&self, session_id: String) -> Result<(), String> {
        match self
            .rpc
            .request(
                "thread/archive",
                json!({
                    "threadId": session_id
                }),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let normalized = err.to_ascii_lowercase();
                if normalized.contains("no rollout found")
                    || normalized.contains("thread not found")
                    || normalized.contains("not found")
                {
                    return Ok(());
                }
                Err(err)
            }
        }
    }

    async fn list_models(&self) -> Result<(), String> {
        let response = self
            .rpc
            .request("model/list", json!({ "includeHidden": false }))
            .await?;

        let raw_models = response
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let models: Vec<Value> = raw_models
            .iter()
            .filter_map(|m| {
                let id = m
                    .get("model")
                    .or_else(|| m.get("id"))
                    .and_then(Value::as_str)?;
                let display_name = m.get("displayName").and_then(Value::as_str).unwrap_or(id);
                let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
                Some(json!({
                    "id": id,
                    "displayName": display_name,
                    "isDefault": is_default,
                }))
            })
            .collect();

        self.emitter.models_list(models);
        Ok(())
    }

    async fn emit_resumed_thread_history(&self, turns: &[Value], model: &str) {
        for turn in turns {
            let turn_id = turn
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown-turn");
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };

            for item in items {
                let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();

                match item_type {
                    "contextCompaction" => {
                        let item_id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown-item");
                        let thread_id = self.state.lock().await.thread_id.clone();
                        self.emitter
                            .compaction_event(&BackendCompactionEvent::Observed(Box::new(
                                BackendObservedCompaction {
                                    observation_id: super::compaction::stable_observation_id(
                                        "codex",
                                        &thread_id,
                                        &format!("{turn_id}:{item_id}"),
                                    ),
                                    trigger: CompactionTrigger::BackendAutomatic,
                                    method: CompactionMethod::BackendAutomatic,
                                    provider_session_id: Some(SessionId(thread_id.clone())),
                                    metrics: CompactionMetrics::default(),
                                    source: BackendCompactionObservationSource::CodexItem {
                                        thread_id,
                                        turn_id: turn_id.to_string(),
                                        item_id: item_id.to_string(),
                                    },
                                    user_focus: None,
                                },
                            )));
                    }
                    "userMessage" => {
                        let text = extract_codex_item_text(item);
                        if text.trim().is_empty() {
                            continue;
                        }
                        self.emitter.user_message(&text, None);
                    }
                    "agentMessage" => {
                        let text = extract_codex_item_text(item);
                        let reasoning = extract_codex_item_reasoning(item)
                            .filter(|reasoning| contains_non_whitespace(reasoning));
                        if !codex_message_is_renderable(&text, reasoning.as_deref(), 0, 0) {
                            continue;
                        }
                        self.emitter.replay_assistant_message(
                            crate::backend::turn_emitter::AssistantMessagePayload {
                                message_id: item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|message_id| !message_id.trim().is_empty())
                                    .map(|message_id| ChatMessageId(message_id.to_owned())),
                                content: text,
                                reasoning: reasoning.map(reasoning_data),
                                tool_calls: Vec::new(),
                                model_info: Some(ModelInfo {
                                    model: model.to_owned(),
                                }),
                                token_usage: None,
                                context_breakdown: None,
                                images: Vec::new(),
                            },
                        );
                    }
                    "reasoning" => {
                        let Some(reasoning) = extract_codex_item_reasoning(item)
                            .filter(|reasoning| contains_non_whitespace(reasoning))
                        else {
                            continue;
                        };
                        self.emitter.replay_assistant_message(
                            crate::backend::turn_emitter::AssistantMessagePayload {
                                message_id: item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|message_id| !message_id.trim().is_empty())
                                    .map(|message_id| ChatMessageId(message_id.to_owned())),
                                content: String::new(),
                                reasoning: Some(reasoning_data(reasoning)),
                                tool_calls: Vec::new(),
                                model_info: Some(ModelInfo {
                                    model: model.to_owned(),
                                }),
                                token_usage: None,
                                context_breakdown: None,
                                images: Vec::new(),
                            },
                        );
                    }
                    "imageGeneration" => {
                        let Ok(image) = parse_codex_generated_image(item) else {
                            continue;
                        };
                        self.emitter.replay_assistant_message(
                            crate::backend::turn_emitter::AssistantMessagePayload {
                                message_id: item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|message_id| !message_id.trim().is_empty())
                                    .map(|message_id| ChatMessageId(message_id.to_owned())),
                                content: String::new(),
                                reasoning: None,
                                tool_calls: Vec::new(),
                                model_info: Some(ModelInfo {
                                    model: model.to_owned(),
                                }),
                                token_usage: None,
                                context_breakdown: None,
                                images: vec![image],
                            },
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    async fn respond_pending_request(&self, message: &str) -> Result<bool, String> {
        let pending = {
            let mut state = self.state.lock().await;
            state.pending_request.take()
        };

        let Some(pending) = pending else {
            return Ok(false);
        };

        match pending.kind {
            PendingRequestKind::CommandApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::FileChangeApproval => {
                let decision = parse_approval_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "file_change_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::ExecCommandApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "exec_command_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::ApplyPatchApproval => {
                let decision = parse_review_decision(message);
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "decision": decision
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "apply_patch_approval",
                    true,
                    json!({"kind": "Other", "result": {"decision": decision}}),
                    None,
                )
                .await;
            }
            PendingRequestKind::UserInput { questions } => {
                let normalized = if message.trim().is_empty() {
                    String::new()
                } else {
                    message.trim().to_string()
                };
                let mut answers = serde_json::Map::new();
                for q in &questions {
                    answers.insert(q.clone(), json!({ "answers": [normalized] }));
                }
                self.rpc
                    .respond(
                        pending.request_id.clone(),
                        json!({
                            "answers": answers
                        }),
                    )
                    .await?;
                self.emit_tool_execution_completed(
                    &pending.tool_call_id,
                    "ask_user_question",
                    true,
                    json!({"kind": "Other", "result": {"answered": true}}),
                    None,
                )
                .await;
            }
        }

        Ok(true)
    }

    async fn handle_inbound(self: &Arc<Self>, inbound: CodexInbound) {
        match inbound {
            CodexInbound::Stderr(line) => {
                if let Some((attempt, max_retries)) = parse_codex_reconnecting_attempt(&line) {
                    self.emitter.retry_attempt(RetryAttemptPayload {
                        attempt,
                        max_retries,
                        error: &line,
                        backoff_ms: 250u64
                            .saturating_mul(1u64 << attempt.saturating_sub(1))
                            .min(4_000),
                    });
                } else {
                    self.emitter.subprocess_stderr(&line);
                }
            }
            CodexInbound::RolloutTrace(event) => {
                self.handle_rollout_trace_event(event).await;
            }
            CodexInbound::RolloutTraceError(error) => {
                tracing::error!(error, "Codex rollout trace reader stopped");
                self.emitter.subprocess_stderr(&format!(
                    "Codex rollout trace is unavailable; exact code-mode tool ownership cannot be reconciled: {error}"
                ));
            }
            CodexInbound::Closed { exit_code } => {
                self.finalize_all_incomplete_strict_responses(
                    "Codex transport closed before rawResponse/completed",
                )
                .await;
                self.finish_compaction_failure(
                    BackendCompactionFailureKind::TransportClosed,
                    "Codex app-server exited during compaction".to_string(),
                )
                .await;
                self.drain_background_commands().await;
                self.complete_all_codex_subagents().await;
                self.emitter.subprocess_exit(exit_code);
                // The app-server exited on its own; reap it now rather than
                // leaving a zombie until session teardown (CodexRpc::Drop won't
                // fire while the forwarder still holds Arc<CodexInner>).
                self.rpc.reap_after_exit().await;
            }
            CodexInbound::Notification { method, params } => {
                if method.starts_with("codex/event/") {
                    self.handle_legacy_codex_event(&method, &params).await;
                    return;
                }
                self.handle_notification(&method, &params).await;
            }
            CodexInbound::ServerRequest { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
        }
    }

    async fn handle_rollout_trace_event(&self, event: CodexRolloutTraceEvent) {
        match event {
            CodexRolloutTraceEvent::ToolStarted {
                owner,
                tool_call_id,
            } => {
                let mut state = self.state.lock().await;
                if let Some(previous) = state
                    .code_cell_by_tool
                    .insert(tool_call_id.clone(), owner.clone())
                    && previous != owner
                {
                    tracing::error!(
                        tool_call_id,
                        "Codex rollout trace reassigned a live tool to another code cell"
                    );
                    return;
                }
                state
                    .code_cell_tools
                    .entry(owner)
                    .or_default()
                    .insert(tool_call_id);
            }
            CodexRolloutTraceEvent::ToolEnded { tool_call_id } => {
                let mut state = self.state.lock().await;
                let Some(owner) = state.code_cell_by_tool.remove(&tool_call_id) else {
                    return;
                };
                if let Some(tools) = state.code_cell_tools.get_mut(&owner) {
                    tools.remove(&tool_call_id);
                    if tools.is_empty() {
                        state.code_cell_tools.remove(&owner);
                    }
                }
            }
            CodexRolloutTraceEvent::CodeCellEnded { owner } => {
                let abandoned = {
                    let mut state = self.state.lock().await;
                    let Some(tool_call_ids) = state.code_cell_tools.remove(&owner) else {
                        return;
                    };
                    tool_call_ids
                        .into_iter()
                        .map(|provider_tool_call_id| {
                            state.code_cell_by_tool.remove(&provider_tool_call_id);
                            let tool_call_id = format!(
                                "codex:{}:{}:{}",
                                owner.thread_id, owner.turn_id, provider_tool_call_id
                            );
                            state.pending_tool_call_ids.remove(&tool_call_id);
                            state.cancelled_tool_call_ids.insert(tool_call_id.clone());
                            tool_call_id
                        })
                        .collect::<Vec<_>>()
                };
                let Some(emitter) = self.background_progress_emitter(&owner.thread_id).await else {
                    return;
                };
                for tool_call_id in abandoned {
                    tracing::warn!(
                        thread_id = owner.thread_id,
                        turn_id = owner.turn_id,
                        runtime_cell_id = owner.runtime_cell_id,
                        tool_call_id,
                        "Codex code-mode program exited with an owned tool still open"
                    );
                    self.emit_or_defer_tool_completion(
                        &owner.thread_id,
                        &emitter,
                        &tool_call_id,
                        ToolExecutionOutcome::Cancelled {
                            message:
                                "Tool call was abandoned when its owning code-mode program exited"
                                    .to_owned(),
                        },
                    )
                    .await;
                    self.mark_tool_completed(&tool_call_id).await;
                }
            }
        }
    }

    /// The emitter and model for a thread, for code that projects *provider
    /// responses*. Strict-splitting only: returns `None` for a thread whose
    /// splitter is disabled, because there are no provider-response boundaries
    /// to project there.
    ///
    /// Do not use this to emit a tool card. Tool cards exist on every thread,
    /// including the resumed and forked ones this returns `None` for — use
    /// [`Self::tool_projection_target`].
    async fn response_projection_target(
        &self,
        thread_id: &str,
    ) -> Option<(Arc<TurnEmitter>, String)> {
        if !self
            .state
            .lock()
            .await
            .response_splitters
            .get(thread_id)
            .is_some_and(|splitter| splitter.enabled)
        {
            return None;
        }
        self.tool_projection_target(thread_id).await
    }

    /// The emitter and model for a thread, with no strict-splitting condition.
    ///
    /// A resumed or forked thread has its splitter disabled — Codex sends it no
    /// `rawResponse*` notifications — but it still runs tools, and those tool
    /// cards still belong to the user's chat.
    async fn tool_projection_target(&self, thread_id: &str) -> Option<(Arc<TurnEmitter>, String)> {
        let state = self.state.lock().await;
        let model = state
            .effective_model
            .clone()
            .unwrap_or_else(|| "codex".to_owned());
        if thread_id == state.thread_id {
            return Some((Arc::clone(&self.emitter), model));
        }
        state
            .subagent_streams
            .get(thread_id)
            .map(|stream| (Arc::clone(&stream.emitter), model.clone()))
            .or_else(|| {
                state
                    .completed_subagent_streams
                    .get(thread_id)
                    .map(|stream| (Arc::clone(&stream.emitter), model))
            })
    }

    async fn ensure_strict_response_handle(
        &self,
        thread_id: &str,
        emitter: &TurnEmitter,
        model: &str,
    ) -> Option<ResponseHandle> {
        // The splitter answers *whether* a provider response is open; the
        // emitter answers which chat response that is. A second copy of the
        // handle here could only ever disagree with the emitter, which retires
        // its response at `stream_end` and drops it if the turn goes idle —
        // neither of which is observable from the splitter.
        let state = self.state.lock().await;
        state
            .response_splitters
            .get(thread_id)
            .and_then(|splitter| splitter.open.as_ref())?;
        Some(emitter.ensure_open_response(Some(model)))
    }

    async fn observe_strict_response_item_started(&self, params: &Value) -> (bool, bool) {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return (false, false);
        };
        if self.response_projection_target(&thread_id).await.is_none() {
            return (false, false);
        }
        let Some(item) = params.get("item") else {
            return (false, false);
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if !is_codex_provider_output_item_type(item_type) {
            return (false, false);
        }
        let tool_item = is_codex_provider_tool_item_type(item_type);
        let item_id = item.get("id").and_then(Value::as_str);
        let call_id = item
            .get("callId")
            .or_else(|| item.get("call_id"))
            .and_then(Value::as_str);
        let turn_id = extract_turn_id(params);
        {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(&thread_id)
                .and_then(|splitter| {
                    splitter.observe_typed_item_started(
                        turn_id.as_deref(),
                        item_id,
                        call_id,
                        item_type,
                    )
                });
        }
        let provider_tool = !tool_item || {
            let state = self.state.lock().await;
            item_id.is_some_and(|item_id| {
                state
                    .response_splitters
                    .get(&thread_id)
                    .is_some_and(|splitter| splitter.provider_typed_tool_item_ids.contains(item_id))
            })
        };
        (true, provider_tool)
    }

    async fn strict_execution_only_tool_owner(
        &self,
        params: &Value,
    ) -> Option<BufferedCodexToolRequest> {
        let thread_id = extract_notification_thread_id(params)?;
        let item_id = params.pointer("/item/id").and_then(Value::as_str);
        let mut state = self.state.lock().await;
        state
            .response_splitters
            .get_mut(&thread_id)?
            .take_execution_only_typed_tool_owner(item_id)
    }

    async fn handle_strict_execution_only_tool_started(self: &Arc<Self>, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let provider_item_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("tool-call");
        let thread_id =
            extract_notification_thread_id(params).unwrap_or_else(|| "<unknown-thread>".to_owned());
        let owner = self.strict_execution_only_tool_owner(params).await;
        match item_type {
            "commandExecution" => {
                tracing::debug!(
                    thread_id,
                    owner = ?owner.as_ref().map(|owner| (
                        owner.tool_call_id.as_str(),
                        owner.tool_name.as_str(),
                        &owner.tool_type,
                    )),
                    "Codex command execution started"
                );
                let tool_call_id = if let Some(owner) = owner {
                    owner.tool_call_id
                } else {
                    let tool_call_id = self
                        .tool_call_started_id(params, provider_item_id, "run_command")
                        .await;
                    self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                        .await;
                    self.emit_tool_request_for_thread(
                        &thread_id,
                        &tool_call_id,
                        "run_command",
                        CodexToolRequest::from_item(
                            item,
                            json!({
                                "kind": "RunCommand",
                                "command": codex_command_text(item).unwrap_or_default(),
                                "working_directory": item
                                    .get("cwd")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            }),
                        ),
                    )
                    .await;
                    tool_call_id
                };
                self.track_command_execution(params, provider_item_id, &tool_call_id, item)
                    .await;
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let canonical_item_id = if let Some(owner) = owner.as_ref() {
                    owner.tool_call_id.clone()
                } else if !parse_codex_subagent_collabs(item).is_empty() {
                    let tool_name = item
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("spawnAgent");
                    let item_id = self
                        .tool_call_started_id(params, provider_item_id, tool_name)
                        .await;
                    self.track_tool_requests(std::iter::once(item_id.clone()))
                        .await;
                    self.emit_tool_request_for_thread(
                        &thread_id,
                        &item_id,
                        tool_name,
                        codex_public_tool_request(tool_name, item),
                    )
                    .await;
                    item_id
                } else {
                    codex_scoped_tool_call_id(params, provider_item_id)
                };
                self.record_codex_subagent_spawn_metadata_if_needed(
                    Some(&canonical_item_id),
                    Some(params),
                    item,
                )
                .await;
            }
            _ => {}
        }
    }

    async fn handle_strict_response_delta(&self, method: &str, params: &Value) -> bool {
        let (reasoning, delta) = if method == "item/agentMessage/delta" {
            (
                false,
                params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            )
        } else if is_reasoning_notification_method(method) {
            let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                return false;
            };
            (true, delta)
        } else {
            return false;
        };
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return false;
        };
        let Some((emitter, model)) = self.response_projection_target(&thread_id).await else {
            return false;
        };
        if delta.is_empty() {
            return true;
        }
        let item_id = params
            .get("itemId")
            .or_else(|| params.get("item_id"))
            .and_then(Value::as_str);
        let turn_id = extract_turn_id(params);
        let emission = {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(&thread_id)
                .and_then(|splitter| {
                    splitter.observe_delta(turn_id.as_deref(), item_id, &delta, reasoning)
                })
        };
        let Some(emission) = emission else {
            return false;
        };
        let Some(response) = self
            .ensure_strict_response_handle(&thread_id, emitter.as_ref(), &model)
            .await
        else {
            return false;
        };
        if reasoning {
            emitter.stream_reasoning_delta(&response, &emission.delta);
        } else {
            emitter.stream_delta(&response, &emission.delta);
        }
        true
    }

    async fn handle_strict_response_item_completed(&self, params: &Value) -> bool {
        let Some(item) = params.get("item") else {
            return false;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if !matches!(item_type, "agentMessage" | "reasoning") {
            return false;
        }
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return false;
        };
        let Some((emitter, model)) = self.response_projection_target(&thread_id).await else {
            return false;
        };
        let reasoning = item_type == "reasoning";
        let completed = if reasoning {
            extract_codex_item_reasoning(item).unwrap_or_default()
        } else {
            item.get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| extract_codex_item_text(item))
        };
        let item_id = item.get("id").and_then(Value::as_str);
        let turn_id = extract_turn_id(params);
        let emission = {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(&thread_id)
                .and_then(|splitter| {
                    splitter.observe_item_completed(
                        turn_id.as_deref(),
                        item_id,
                        &completed,
                        reasoning,
                    )
                })
        };
        let Some(emission) = emission else {
            return false;
        };
        let Some(response) = self
            .ensure_strict_response_handle(&thread_id, emitter.as_ref(), &model)
            .await
        else {
            return false;
        };
        if !emission.delta.is_empty() {
            if reasoning {
                emitter.stream_reasoning_delta(&response, &emission.delta);
            } else {
                emitter.stream_delta(&response, &emission.delta);
            }
        }
        true
    }

    async fn finish_strict_typed_tool(
        &self,
        params: &Value,
    ) -> Option<(bool, Option<BufferedCodexToolRequest>)> {
        let item = params.get("item")?;
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if !is_codex_provider_tool_item_type(item_type) {
            return None;
        }
        let thread_id = extract_notification_thread_id(params)?;
        let item_id = item.get("id").and_then(Value::as_str);
        let mut state = self.state.lock().await;
        let splitter = state.response_splitters.get_mut(&thread_id)?;
        splitter
            .enabled
            .then(|| splitter.finish_typed_tool(item_id))?
    }

    async fn complete_strict_raw_tool_output(&self, params: &Value) -> bool {
        let Some(item) = params.get("item") else {
            return false;
        };
        if !is_raw_codex_tool_output_item_type(item.get("type").and_then(Value::as_str)) {
            return false;
        }
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return false;
        };
        let Some(call_id) = item
            .get("call_id")
            .or_else(|| item.get("callId"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        let (owner, suppressed_owner, typed_owns_call) = {
            let state = self.state.lock().await;
            let splitter = state.response_splitters.get(&thread_id);
            (
                splitter.and_then(|splitter| splitter.raw_tool_owner_for_completion(call_id)),
                splitter.and_then(|splitter| splitter.suppressed_raw_tool_request(call_id)),
                splitter.is_some_and(|splitter| splitter.typed_item_owns_call(call_id)),
            )
        };
        let owner = if owner.is_some() {
            owner
        } else if let Some(suppressed_owner) = suppressed_owner {
            let output = raw_custom_tool_output_text(item);
            let nested_command_never_started = suppressed_owner
                .tool_type
                .get("kind")
                .and_then(Value::as_str)
                == Some("RunCommand")
                && output.starts_with("Script failed")
                && output.contains("exec_command failed");
            if !nested_command_never_started {
                if let Some(splitter) = self
                    .state
                    .lock()
                    .await
                    .response_splitters
                    .get_mut(&thread_id)
                {
                    splitter.remove_suppressed_raw_tool_request(call_id);
                }
                return false;
            }
            eprintln!(
                "TYDE CODEX RAW TOOL FALLBACK thread_id={thread_id} call_id={call_id} tool_call_id={}",
                suppressed_owner.tool_call_id
            );
            let buffered = self
                .buffer_strict_tool_request(
                    &thread_id,
                    &suppressed_owner.tool_call_id,
                    &suppressed_owner.tool_name,
                    suppressed_owner.arguments.clone(),
                    suppressed_owner.tool_type.clone(),
                )
                .await;
            if !buffered {
                tracing::error!(
                    thread_id,
                    call_id,
                    tool_call_id = suppressed_owner.tool_call_id,
                    "Codex could not declare a rejected nested command"
                );
                return false;
            }
            Some(suppressed_owner)
        } else {
            None
        };
        let Some(owner) = owner else {
            if typed_owns_call {
                // The ordinary path for a shell call: the typed item completed
                // the card and unparked its owner before this arrived. Reporting
                // it as a loss put an ERROR in the log for every healthy command.
                tracing::debug!(
                    thread_id,
                    call_id,
                    "Codex raw tool output followed the typed item that already completed its card"
                );
                return false;
            }
            // No parked owner for this `call_id`, so this raw output cannot be
            // attached to the card the model declared for it. Downstream that
            // shows up as a tool that never completes.
            tracing::error!(
                thread_id,
                call_id,
                "Codex raw tool output had no parked owner to complete"
            );
            return false;
        };
        let Some((emitter, _)) = self.response_projection_target(&thread_id).await else {
            tracing::error!(
                thread_id,
                call_id,
                tool_call_id = owner.tool_call_id,
                "Codex raw tool output had no emitter to report it on"
            );
            return false;
        };
        // Nothing to do only when the emitter *knows* the card and it is no
        // longer pending — it was declared and already completed. A card the
        // emitter has never heard of is the opposite case: a raw output can
        // outrun its declaration, because the output lands about 2ms before the
        // `thread/tokenUsage/updated` that closes the response and declares the
        // cards. Treating that as "already handled" swallowed the outcome and
        // left the card open until the idle sweep cancelled it.
        if !emitter.has_pending_tool_request(&owner.tool_call_id)
            && emitter.has_known_tool_request(&owner.tool_call_id)
        {
            if let Some(splitter) = self
                .state
                .lock()
                .await
                .response_splitters
                .get_mut(&thread_id)
            {
                splitter.complete_raw_tool_call(call_id);
            }
            return true;
        }
        let native_subagent = {
            let mut state = self.state.lock().await;
            let native_subagent = state
                .native_subagent_tool_call_ids
                .contains(&owner.tool_call_id);
            if native_subagent && let Some(splitter) = state.response_splitters.get_mut(&thread_id)
            {
                splitter.complete_raw_tool_call(call_id);
            }
            native_subagent
        };
        if native_subagent {
            return true;
        }
        let yielded_session_ids = codex_yielded_session_ids(params);
        let correlated_background = {
            let mut state = self.state.lock().await;
            let correlated =
                state
                    .background_commands
                    .iter()
                    .any(|((owner_thread_id, _), command)| {
                        owner_thread_id == &thread_id && command.tool_call_id == owner.tool_call_id
                    });
            if correlated && let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
                splitter.complete_raw_tool_call(call_id);
            }
            correlated
        };
        if correlated_background {
            return true;
        }
        // Only a command execution can yield its session to the background.
        // An interaction with an already-running process reports the session it
        // polled, which belongs to that process's own card — running it through
        // the background correlation below reports a healthy foreground poll as
        // an uncorrelated yield and puts an Error card in front of the user.
        if !yielded_session_ids.is_empty()
            && codex_raw_call_is_rendered_elsewhere(&owner.tool_name, &owner.arguments)
        {
            let correlated = {
                let mut state = self.state.lock().await;
                let correlated = yielded_session_ids.iter().all(|session_id| {
                    state
                        .background_commands
                        .iter()
                        .any(|((owner_thread_id, _), command)| {
                            owner_thread_id == &thread_id && command.task_id == *session_id
                        })
                });
                if correlated && let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
                    splitter.complete_raw_tool_call(call_id);
                }
                correlated
            };
            if correlated {
                return true;
            }
            let message = format!(
                "Codex yielded sessions {yielded_session_ids:?} without uniquely correlated command executions"
            );
            if !emitter.fail_pending_tool(&owner.tool_call_id, &message) {
                emitter.backend_error(&message);
            }
            let turn_id = extract_turn_id(params);
            let mut state = self.state.lock().await;
            state
                .unowned_command_executions
                .retain(|(owner_thread_id, _), command| {
                    owner_thread_id != &thread_id
                        || !yielded_session_ids.contains(&command.process_id)
                        || turn_id
                            .as_ref()
                            .is_some_and(|turn_id| command.turn_id != *turn_id)
                });
            state
                .outstanding_command_executions
                .retain(|(owner_thread_id, _), command| {
                    owner_thread_id != &thread_id
                        || command.tool_call_id != owner.tool_call_id
                        || command
                            .process_id
                            .as_ref()
                            .is_none_or(|process_id| !yielded_session_ids.contains(process_id))
                });
            if let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
                splitter.complete_raw_tool_call(call_id);
            }
            return true;
        }
        // Codex reports one shell call twice, and only the typed
        // `commandExecution` carries `exit_code`/`stdout`. Completing the card
        // from here as well left the rendered outcome decided by whichever of
        // the two landed first — a millisecond apart on the wire — and a
        // disagreement surfaced as `conflicting_duplicate_completion`. A card a
        // typed item has taken belongs to that item alone.
        if typed_owns_call {
            if let Some(splitter) = self
                .state
                .lock()
                .await
                .response_splitters
                .get_mut(&thread_id)
            {
                splitter.complete_raw_tool_call(call_id);
            }
            return true;
        }
        let output = raw_custom_tool_output_text(item);
        let success = !output.starts_with("Script failed");
        let error = (!success).then(|| output.clone());
        let tool_result = if success {
            json!({ "kind": "Other", "result": output })
        } else {
            json!({
                "kind": "Error",
                "short_message": format!("{} failed", owner.tool_name),
                "detailed_message": output,
            })
        };
        self.emit_or_defer_tool_completion(
            &thread_id,
            &emitter,
            &owner.tool_call_id,
            codex_tool_execution_outcome(tool_result, success, error, None),
        )
        .await;
        if let Some(splitter) = self
            .state
            .lock()
            .await
            .response_splitters
            .get_mut(&thread_id)
        {
            splitter.complete_raw_tool_call(call_id);
            splitter.remove_suppressed_raw_tool_request(call_id);
        }
        true
    }

    async fn handle_strict_execution_only_tool_completed(
        self: &Arc<Self>,
        params: &Value,
        owner: Option<BufferedCodexToolRequest>,
    ) {
        let Some(item) = params.get("item") else {
            return;
        };
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let provider_item_id = item.get("id").and_then(Value::as_str).unwrap_or("item");
        match item_type {
            "commandExecution" => {
                tracing::debug!(
                    thread_id,
                    owner = ?owner
                        .as_ref()
                        .map(|owner| (owner.tool_call_id.as_str(), owner.tool_name.as_str())),
                    "Codex command execution completed"
                );
                let background_command =
                    self.take_background_command(params, provider_item_id).await;
                let outstanding_command = self
                    .forget_command_execution(params, provider_item_id)
                    .await;
                let Some(tool_call_id) = background_command
                    .as_ref()
                    .map(|command| command.tool_call_id.clone())
                    .or_else(|| {
                        outstanding_command
                            .as_ref()
                            .map(|command| command.tool_call_id.clone())
                    })
                else {
                    // Neither a background group nor an outstanding execution
                    // owns this item, so there is nobody to complete. The card
                    // Codex opened for it stays pending until the idle sweep
                    // cancels it, which is a user-visible stuck tool — report
                    // it rather than returning in silence.
                    tracing::error!(
                        thread_id,
                        provider_item_id,
                        "Codex command execution completed with no correlated tool call"
                    );
                    return;
                };
                let Some(emitter) = self.background_progress_emitter(&thread_id).await else {
                    tracing::error!(
                        thread_id,
                        tool_call_id,
                        "Codex command execution completed with no emitter to report it on"
                    );
                    return;
                };
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let success = exit_code == 0;
                eprintln!(
                    "TYDE CODEX COMMAND TERMINAL thread_id={thread_id} provider_item_id={provider_item_id} exit_code={exit_code} background_owner={} outstanding_owner={} child={} ",
                    background_command.is_some(),
                    outstanding_command.is_some(),
                    thread_id != self.state.lock().await.thread_id,
                );
                let outcome = if let Some(command) = background_command.as_ref() {
                    self.complete_background_command_group(&thread_id, command, exit_code, &output)
                        .await
                } else {
                    Some(codex_tool_execution_outcome(
                        json!({
                            "kind": "RunCommand",
                            "exit_code": exit_code,
                            "stdout": output.clone(),
                            "stderr": "",
                        }),
                        success,
                        (!success).then(|| format!("Command failed with exit code {exit_code}")),
                        None,
                    ))
                };
                if let Some(outcome) = outcome {
                    self.emit_or_defer_tool_completion(
                        &thread_id,
                        &emitter,
                        &tool_call_id,
                        outcome,
                    )
                    .await;
                    if thread_id == self.state.lock().await.thread_id {
                        self.mark_tool_completed(&tool_call_id).await;
                    }
                }
                if thread_id == self.state.lock().await.thread_id {
                    if let Some(command) = background_command {
                        self.enqueue_background_wake(command, exit_code, output)
                            .await;
                    }
                } else {
                    let pending_spawn_terminal = {
                        let mut state = self.state.lock().await;
                        if !success && let Some(stream) = state.subagent_streams.get_mut(&thread_id)
                        {
                            stream.background_work_failed = true;
                            if stream.pending_spawn_terminal_status.is_some() {
                                stream.pending_spawn_terminal_status = Some("failed".to_owned());
                            }
                        }
                        let still_running = state
                            .background_commands
                            .keys()
                            .chain(state.outstanding_command_executions.keys())
                            .any(|(owner_thread_id, _)| owner_thread_id == &thread_id);
                        (!still_running)
                            .then(|| {
                                state
                                    .subagent_streams
                                    .get_mut(&thread_id)
                                    .and_then(|stream| stream.pending_spawn_terminal_status.take())
                            })
                            .flatten()
                    };
                    if let Some(status) = pending_spawn_terminal {
                        self.terminalize_codex_subagent_spawn(&thread_id, &status)
                            .await;
                    }
                }
                if let Some(splitter) = self
                    .state
                    .lock()
                    .await
                    .response_splitters
                    .get_mut(&thread_id)
                {
                    splitter.remove_raw_tool_owner_by_tool_call_id(&tool_call_id);
                }
            }
            "imageGeneration" => {
                let Ok(image) = parse_codex_generated_image(item) else {
                    return;
                };
                let mut state = self.state.lock().await;
                if thread_id == state.thread_id {
                    state.tool_container_images.push(image);
                } else if let Some(stream) = state.subagent_streams.get_mut(&thread_id) {
                    stream.tool_container_images.push(image);
                }
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let canonical_item_id = if let Some(owner) = owner.as_ref() {
                    owner.tool_call_id.clone()
                } else {
                    self.tool_call_completed_id(
                        params,
                        provider_item_id,
                        item.get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("collab_tool"),
                    )
                    .await
                };
                self.record_codex_subagent_spawn_metadata_if_needed(
                    Some(&canonical_item_id),
                    Some(params),
                    item,
                )
                .await;
            }
            _ => {}
        }
    }

    async fn observe_strict_raw_response_item(&self, params: &Value) {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return;
        };
        if self.response_projection_target(&thread_id).await.is_none() {
            return;
        }
        let Some(item) = params.get("item") else {
            return;
        };
        let turn_id = extract_turn_id(params);
        {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(&thread_id)
                .and_then(|splitter| splitter.observe_raw_item(turn_id.as_deref(), item));
        }
    }

    async fn buffer_strict_tool_request(
        &self,
        thread_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        arguments: Value,
        tool_type: Value,
    ) -> bool {
        let Some((emitter, model)) = self.response_projection_target(thread_id).await else {
            return false;
        };
        let turn_id = {
            let state = self.state.lock().await;
            if thread_id == state.thread_id {
                state.active_turn_id.clone()
            } else {
                state
                    .subagent_streams
                    .get(thread_id)
                    .and_then(|stream| stream.active_turn_id.clone())
            }
        };
        let declaration = ToolUseData {
            tool_call_id: tool_call_id.to_owned(),
            name: tool_name.to_owned(),
            arguments: arguments.clone(),
            content_offset: None,
        };
        let request_type = codex_tool_request_type(tool_type.clone());
        let content_offset = {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(thread_id)
                .and_then(|splitter| {
                    splitter.buffer_tool_request(
                        turn_id.as_deref(),
                        tool_call_id,
                        tool_name,
                        arguments,
                        tool_type,
                    )
                })
        };
        let Some(content_offset) = content_offset else {
            return false;
        };
        let Some(response) = self
            .ensure_strict_response_handle(thread_id, emitter.as_ref(), &model)
            .await
        else {
            return false;
        };
        // Declare now rather than at the response boundary. That boundary is a
        // `tokenUsage` change, which only lands once the tools have finished
        // running, so waiting for it would leave every card invisible for the
        // whole duration of the command.
        emitter.declare_streaming_tools(
            &response,
            vec![ToolUseData {
                content_offset: Some(content_offset),
                ..declaration
            }],
        );
        emitter.tool_request(tool_call_id, request_type);
        true
    }

    async fn prepare_strict_response_bookkeeping(
        &self,
        thread_id: &str,
        finalized: &FinalizedCodexProviderResponse,
        model: &str,
        presentation_message_id: ChatMessageId,
    ) -> Vec<ImageData> {
        let mut state = self.state.lock().await;
        if thread_id == state.thread_id {
            if !finalized.failed
                && finalized.tool_requests.is_empty()
                && contains_non_whitespace(&finalized.content)
            {
                state.foreground_response_completed = true;
            }
            state.close_active_stream_when_tools_idle = false;
            if let Some(metadata) = metadata_target_for_visible_message(
                finalized.turn_id.clone(),
                presentation_message_id,
                &finalized.content,
                finalized.reasoning.as_deref(),
                model.to_owned(),
            ) {
                state.pending_message_metadata = Some(metadata);
            }
            return if finalized.failed {
                Vec::new()
            } else {
                std::mem::take(&mut state.tool_container_images)
            };
        }
        let Some(stream) = state.subagent_streams.get_mut(thread_id) else {
            return Vec::new();
        };
        if let Some(metadata) = metadata_target_for_visible_message(
            finalized.turn_id.clone(),
            presentation_message_id,
            &finalized.content,
            finalized.reasoning.as_deref(),
            model.to_owned(),
        ) {
            stream.pending_message_metadata = Some(metadata);
        }
        if finalized.failed {
            Vec::new()
        } else {
            let mut images = std::mem::take(&mut stream.current_images);
            images.append(&mut stream.tool_container_images);
            images
        }
    }

    async fn finalize_strict_response(&self, params: &Value, failed: bool) -> bool {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return false;
        };
        let Some((emitter, model)) = self.response_projection_target(&thread_id).await else {
            return false;
        };
        let usage = params.get("usage").and_then(|usage| {
            normalize_token_usage_with_envelope(usage, Some(params), Some(&model))
        });
        let turn_id = extract_turn_id(params);
        let (finalized, retained_raw_owners, claimed_raw_calls) = {
            let mut state = self.state.lock().await;
            let Some(splitter) = state.response_splitters.get_mut(&thread_id) else {
                return false;
            };
            let finalized = splitter.finalize(turn_id.as_deref(), usage, failed);
            (
                finalized,
                splitter.pending_raw_tool_owners.len(),
                splitter.claimed_raw_tool_calls.len(),
            )
        };
        let Some(finalized) = finalized else {
            return false;
        };
        eprintln!(
            "TYDE CODEX RAW OWNER RETENTION thread_id={thread_id} retained={retained_raw_owners} claimed={claimed_raw_calls}"
        );
        tracing::debug!(
            thread_id,
            turn_id = finalized.turn_id,
            response_id = ?finalized.response_id,
            tools = ?finalized
                .tool_requests
                .iter()
                .map(|request| (
                    request.tool_call_id.as_str(),
                    request.tool_name.as_str(),
                    &request.tool_type,
                ))
                .collect::<Vec<_>>(),
            "Finalizing a Codex provider response"
        );
        for request in &finalized.evicted_tool_requests {
            if !emitter.fail_pending_tool(
                &request.tool_call_id,
                "Codex discarded an expired tool owner before completion",
            ) {
                emitter.backend_error(&format!(
                    "Codex lost the owner for pending tool '{}'",
                    request.tool_call_id
                ));
            }
        }
        let renderable = contains_non_whitespace(&finalized.content)
            || finalized
                .reasoning
                .as_deref()
                .is_some_and(contains_non_whitespace)
            || !finalized.tool_requests.is_empty();
        let response = emitter
            .open_response()
            .or_else(|| renderable.then(|| emitter.ensure_open_response(Some(&model))));
        let Some(response) = response else {
            tracing::debug!(
                thread_id,
                turn_id = finalized.turn_id,
                response_id = ?finalized.response_id,
                "Suppressed an empty Codex provider response"
            );
            return true;
        };
        let images = self
            .prepare_strict_response_bookkeeping(
                &thread_id,
                &finalized,
                &model,
                response.message_id(),
            )
            .await;
        let content_offset = u32::try_from(finalized.content.chars().count()).unwrap_or(u32::MAX);
        let tool_calls = finalized
            .tool_requests
            .iter()
            .map(|request| ToolUseData {
                tool_call_id: request.tool_call_id.clone(),
                name: request.tool_name.clone(),
                arguments: request.arguments.clone(),
                content_offset: Some(request.content_offset.unwrap_or(content_offset)),
            })
            .collect();
        let token_usage = finalized
            .usage
            .as_ref()
            .and_then(codex_token_usage)
            .map(MessageTokenUsage::request_known)
            .or_else(|| {
                Some(MessageTokenUsage::unavailable(
                    TokenUsageUnavailableReason::BackendDidNotReport,
                ))
            });
        emitter.stream_end(
            response,
            StreamEndPayload {
                content: finalized.content,
                model_info: Some(ModelInfo {
                    model: model.clone(),
                }),
                token_usage,
                reasoning: finalized.reasoning.map(reasoning_data),
                tool_calls,
                images,
                ..StreamEndPayload::default()
            },
        );
        for request in &finalized.tool_requests {
            emitter.tool_request(
                &request.tool_call_id,
                codex_tool_request_type(request.tool_type.clone()),
            );
            if finalized.failed {
                emitter.fail_pending_tool(
                    &request.tool_call_id,
                    "Codex provider response ended before the tool completed",
                );
            }
        }
        // The cards this response declares are open only now, so this is the
        // first moment any outcome that outran the declaration can be emitted.
        self.flush_deferred_tool_completions(&thread_id, &emitter)
            .await;
        let retained_completed_owners = {
            let state = self.state.lock().await;
            state
                .response_splitters
                .get(&thread_id)
                .into_iter()
                .flat_map(|splitter| splitter.pending_raw_tool_owners.values())
                .filter(|owner| {
                    emitter.has_known_tool_request(&owner.tool_call_id)
                        && !emitter.has_pending_tool_request(&owner.tool_call_id)
                })
                .map(|owner| owner.tool_call_id.clone())
                .collect::<Vec<_>>()
        };
        for tool_call_id in retained_completed_owners {
            emitter.backend_error(&format!(
                "Codex retained the owner for completed tool '{tool_call_id}'"
            ));
        }
        tracing::info!(
            thread_id,
            turn_id = finalized.turn_id,
            message_id = finalized.message_id.0,
            response_id = ?finalized.response_id,
            failed = finalized.failed,
            "Finalized Codex provider response boundary"
        );
        true
    }

    /// Record the provider's id for the open response. See
    /// [`CodexResponseSplitter::observe_raw_response_completed`] for why this
    /// notification names a response without ending it.
    async fn observe_raw_response_completed(&self, params: &Value) {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return;
        };
        let Some(response_id) = params
            .get("responseId")
            .or_else(|| params.get("response_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let turn_id = extract_turn_id(params);
        let mut state = self.state.lock().await;
        if let Some(splitter) = state.response_splitters.get_mut(&thread_id) {
            splitter.observe_raw_response_completed(turn_id.as_deref(), response_id);
        }
    }

    /// Closes the open provider response when reported usage moves — the single
    /// boundary, for threads with raw events and without.
    ///
    /// It is the only notification that arrives after *everything* the response
    /// produced, including the typed items for the tools it called, which is
    /// why `rawResponse/completed` cannot serve. Measured on 0.146.0, its
    /// `last` is byte-identical to that event's `usage` on every response, so
    /// nothing is lost by reading the number here instead.
    async fn finalize_strict_response_at_token_usage(&self, params: &Value) {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return;
        };
        let usage = params.pointer("/tokenUsage").cloned();
        let boundary = {
            let mut state = self.state.lock().await;
            state
                .response_splitters
                .get_mut(&thread_id)
                .is_some_and(|splitter| splitter.token_usage_boundary_reached(usage.as_ref()))
        };
        if !boundary {
            return;
        }
        let mut params = params.clone();
        if let Some(last) = params.pointer("/tokenUsage/last").cloned() {
            params["usage"] = last;
        }
        self.finalize_strict_response(&params, false).await;
    }

    /// Close a response the turn left open, reporting `reason` unless it is
    /// `None`.
    ///
    /// `None` is for the turns that end this way by construction rather than by
    /// fault — see [`incomplete_turn_response_error`]. Those still close the
    /// response and still drop its held completions; they just say nothing.
    async fn finalize_incomplete_strict_response(
        &self,
        params: &Value,
        reason: Option<&str>,
    ) -> bool {
        let Some(thread_id) = extract_notification_thread_id(params) else {
            return false;
        };
        let target = self.response_projection_target(&thread_id).await;
        if !self.finalize_strict_response(params, true).await {
            return false;
        }
        if let Some((emitter, _)) = target
            && let Some(reason) = reason
        {
            emitter.backend_error(reason);
        }
        // The response never completed, so it declared nothing; anything still
        // held for this thread has lost its only chance at a card.
        self.discard_deferred_tool_completions(&thread_id).await;
        true
    }

    async fn finalize_all_incomplete_strict_responses(&self, reason: &str) {
        let open_responses = {
            let state = self.state.lock().await;
            state
                .response_splitters
                .iter()
                .filter_map(|(thread_id, splitter)| {
                    splitter
                        .open
                        .as_ref()
                        .map(|response| (thread_id.clone(), response.turn_id.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (thread_id, turn_id) in open_responses {
            self.finalize_incomplete_strict_response(
                &json!({ "threadId": thread_id, "turnId": turn_id }),
                Some(reason),
            )
            .await;
        }
    }

    async fn handle_notification(self: &Arc<Self>, method: &str, params: &Value) {
        self.observe_codex_notification_contract(method).await;
        if matches!(method, "turn/started" | "turn/completed" | "error") {
            let state = self.state.lock().await;
            eprintln!(
                "TYDE CODEX ROOT NOTIFICATION method={method} active_turn_id={:?} awaiting_root_turn_start={} interrupt_next_root_turn={} params={params}",
                state.active_turn_id,
                state.awaiting_root_turn_start,
                state.interrupt_next_root_turn,
            );
        }
        if self.state.lock().await.pending_compaction.is_some() {
            eprintln!("TYDE CODEX COMPACTION NOTIFICATION method={method} params={params}");
        }
        if self.intercept_compaction_notification(method, params).await {
            return;
        }
        self.trace_notification_structure(method, params).await;
        if method == "mcpServer/startupStatus/updated"
            && params.get("name").and_then(Value::as_str)
                == Some(AGENT_CONTROL_AWAIT_MCP_SERVER_NAME)
            && params.get("status").and_then(Value::as_str) == Some("ready")
        {
            let inner = Arc::clone(self);
            tokio::spawn(async move {
                match inner
                    .rpc
                    .request(
                        "mcpServerStatus/list",
                        json!({"detail": "toolsAndAuthOnly", "limit": 100}),
                    )
                    .await
                {
                    Ok(statuses) => {
                        let await_status =
                            statuses
                                .get("data")
                                .and_then(Value::as_array)
                                .and_then(|servers| {
                                    servers.iter().find(|server| {
                                        server.get("name").and_then(Value::as_str)
                                            == Some(AGENT_CONTROL_AWAIT_MCP_SERVER_NAME)
                                    })
                                });
                        tracing::info!(?await_status, "Codex await MCP inventory after ready");
                    }
                    Err(error) => tracing::warn!(%error, "failed to inspect Codex MCP inventory"),
                }
            });
        }
        self.trace_agent_message_identity_event(method, params)
            .await;
        if method == "rawResponse/completed" {
            self.observe_raw_response_completed(params).await;
            return;
        }
        if method == "rawResponseItem/completed" {
            if matches!(
                params
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str),
                Some("custom_tool_call" | "custom_tool_call_output")
            ) {
                eprintln!("TYDE CODEX RAW TOOL RESPONSE ITEM params={params}");
            }
            self.observe_strict_raw_response_item(params).await;
            let unlinked_resolution = if !codex_yielded_session_ids(params).is_empty() {
                let session_ids = self.correlate_yielded_command_owners(params).await;
                for session_id in session_ids {
                    self.promote_command(params, &session_id).await;
                }
                CodexUnlinkedRawToolResolution::OrdinaryCompletion
            } else {
                let session_ids = self.correlate_yielded_command_owners(params).await;
                if session_ids.is_empty() {
                    let resolution = self.resolve_unlinked_raw_tool_output(params).await;
                    if let CodexUnlinkedRawToolResolution::Correlated(process_id) = &resolution {
                        self.promote_command(params, process_id).await;
                    }
                    resolution
                } else {
                    for session_id in session_ids {
                        self.promote_command(params, &session_id).await;
                    }
                    CodexUnlinkedRawToolResolution::OrdinaryCompletion
                }
            };
            let strict_raw_completion = match unlinked_resolution {
                CodexUnlinkedRawToolResolution::Failed => true,
                CodexUnlinkedRawToolResolution::OrdinaryCompletion
                | CodexUnlinkedRawToolResolution::Correlated(_) => {
                    self.complete_strict_raw_tool_output(params).await
                }
            };
            if matches!(
                params
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str),
                Some("custom_tool_call" | "custom_tool_call_output")
            ) {
                tracing::debug!(?params, "Codex raw custom tool completion");
            }
            if !strict_raw_completion {
                self.handle_raw_modify_completion(params).await;
            }
            return;
        }
        if self.handle_strict_response_delta(method, params).await {
            return;
        }
        if method == "error"
            && params
                .get("willRetry")
                .or_else(|| params.get("will_retry"))
                .and_then(Value::as_bool)
                == Some(true)
        {
            self.finalize_strict_response(params, true).await;
        }
        if method == "turn/completed" {
            self.finalize_incomplete_strict_response(
                params,
                incomplete_turn_response_error(params),
            )
            .await;
        }
        if matches!(method, "subAgentActivity" | "sub_agent_activity") {
            let mut item = params
                .get("item")
                .cloned()
                .unwrap_or_else(|| params.clone());
            if item.get("type").is_none() {
                item["type"] = Value::String("subAgentActivity".to_string());
            }
            self.register_codex_subagent_activity_if_needed(&item).await;
            return;
        }
        let suppress_root_response_before_routing = if is_codex_response_side_notification(method) {
            let notification_thread_id = extract_notification_thread_id(params);
            let state = self.state.lock().await;
            let belongs_to_root = notification_thread_id
                .as_ref()
                .is_none_or(|thread_id| thread_id == &state.thread_id);
            let notification_turn_id = extract_turn_id(params);
            let provider_item_id = params
                .get("item")
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                .or_else(|| params.get("itemId").and_then(Value::as_str))
                .or_else(|| params.get("item_id").and_then(Value::as_str));
            // Only loss-recording tombstones may bypass the post-violation
            // quiet window; a routine Completed tombstone re-routing events
            // here would let a repeated conflicting completion target
            // "<no-active-turn>" and emit a fresh cancellation.
            let is_tombstoned_item = provider_item_id.is_some_and(|message_id| {
                state.provider_item_tombstones.iter().any(|tombstone| {
                    tombstone.message_id.0 == message_id
                        && tombstone.disposition != CodexProviderItemDisposition::Completed
                })
            });
            let is_explicitly_terminated_turn =
                notification_turn_id.as_ref().is_some_and(|turn_id| {
                    state
                        .terminated_turns
                        .iter()
                        .any(|terminated| terminated.turn_id == *turn_id)
                });
            let awaiting_new_turn_after_termination = notification_turn_id.is_none()
                && state.active_turn_id.is_none()
                && state
                    .terminated_turn_awaiting_replacement
                    .as_ref()
                    .is_some_and(|awaiting_turn_id| {
                        state
                            .terminated_turns
                            .iter()
                            .any(|terminated| terminated.turn_id == *awaiting_turn_id)
                    });
            let suppress = belongs_to_root
                && !matches!(method, "turn/started" | "turn/completed")
                && !is_tombstoned_item
                && (is_explicitly_terminated_turn || awaiting_new_turn_after_termination);
            if suppress
                && method == "item/completed"
                && let Some(tool_call_id) = params
                    .get("item")
                    .and_then(|item| item.get("id"))
                    .and_then(Value::as_str)
            {
                drop(state);
                self.state
                    .lock()
                    .await
                    .cancelled_tool_call_ids
                    .remove(tool_call_id);
            }
            suppress
        } else {
            false
        };
        if suppress_root_response_before_routing {
            if method == "item/completed"
                && let Some(item) = params.get("item")
                && item.get("type").and_then(Value::as_str) == Some("commandExecution")
                && let Some(provider_item_id) = item.get("id").and_then(Value::as_str)
            {
                let tool_call_id = self
                    .tool_call_completed_id(params, provider_item_id, "run_command")
                    .await;
                let command = self.take_background_command(params, provider_item_id).await;
                let _ = self
                    .forget_command_execution(params, provider_item_id)
                    .await;
                self.warn_codex_raw_contract_drift_once_if_needed().await;
                self.state
                    .lock()
                    .await
                    .cancelled_tool_call_ids
                    .remove(&tool_call_id);
                if let Some(command) = command {
                    let exit_code = item
                        .get("exitCode")
                        .or_else(|| item.get("exit_code"))
                        .and_then(Value::as_i64)
                        .unwrap_or(-1);
                    let success = exit_code == 0;
                    self.emit_tool_execution_completed(
                        &command.tool_call_id,
                        "run_command",
                        success,
                        json!({
                            "kind": "RunCommand",
                            "exit_code": exit_code,
                            "stdout": item
                                .get("aggregatedOutput")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                            "stderr": "",
                        }),
                        (!success).then(|| format!("Command failed with exit code {exit_code}")),
                    )
                    .await;
                }
            }
            tracing::debug!(
                codex_method = method,
                "Ignoring late Codex response notification for terminated root turn"
            );
            return;
        }
        if self
            .handle_subagent_notification_if_needed(method, params)
            .await
        {
            return;
        }
        match method {
            "account/rateLimits/updated" => {
                let emitter = self.state.lock().await.subagent_emitter.clone();
                if let Some(emitter) = emitter {
                    let capacity = self.read_backend_capacity().await;
                    emitter.on_backend_capacity(protocol::BackendKind::Codex, capacity);
                }
            }
            "thread/settings/updated" => {
                if let Some(model) = params
                    .get("threadSettings")
                    .and_then(|settings| settings.get("model"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                {
                    self.state.lock().await.effective_model = Some(model.to_string());
                }
            }
            "turn/started" => {
                self.close_tool_container_if_open().await;
                let turn_id = params
                    .get("turn")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn")
                    .to_string();
                let provider_initiated = {
                    let mut state = self.state.lock().await;
                    if state
                        .terminated_turns
                        .iter()
                        .any(|turn| turn.turn_id == turn_id)
                    {
                        tracing::debug!(
                            turn_id,
                            "Ignoring restarted Codex root turn after local termination"
                        );
                        return;
                    }
                    if state.active_turn_id.as_ref() == Some(&turn_id) {
                        tracing::debug!(turn_id, "Ignoring duplicate Codex root turn start");
                        return;
                    }
                    if state.interrupt_next_root_turn {
                        state.interrupt_next_root_turn = false;
                        push_codex_terminated_turn(&mut state.terminated_turns, turn_id.clone());
                        state.terminated_turn_awaiting_replacement = Some(turn_id.clone());
                        let thread_id = state.thread_id.clone();
                        drop(state);
                        tracing::info!(turn_id, "Interrupting Codex root turn that raced a cancel");
                        self.rpc.spawn_request(
                            "turn/interrupt",
                            json!({
                                "threadId": thread_id,
                                "turnId": turn_id,
                            }),
                        );
                        return;
                    }
                    state.terminated_turn_awaiting_replacement = None;
                    let provider_initiated = !state.awaiting_root_turn_start;
                    state.awaiting_root_turn_start = false;
                    state.background_wake_request_in_flight = false;
                    state.active_turn_id = Some(turn_id.clone());
                    state.foreground_response_completed = false;
                    state.active_stream = None;
                    state.retired_unpublished_message_ids.clear();
                    state.provider_supersessions_this_turn = 0;
                    state.supersession_warning_emitted = false;
                    state.pending_tool_call_ids.clear();
                    state.tool_container_images.clear();
                    state.close_active_stream_when_tools_idle = false;
                    state.pending_message_metadata = None;
                    provider_initiated
                };
                if provider_initiated {
                    self.emitter.typing_status_changed(true);
                }
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let Some(message_id) = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()))
                else {
                    self.reject_agent_message_identity(
                        CodexProviderStreamConflict::MissingMessageId,
                        method,
                        None,
                    )
                    .await;
                    return;
                };
                if delta.is_empty() {
                    return;
                }
                if self
                    .handle_root_late_provider_event(
                        &message_id,
                        CodexLateProviderEvent::Delta {
                            kind: CodexProviderItemKind::AgentMessage,
                            content: delta.clone(),
                        },
                        method,
                    )
                    .await
                {
                    return;
                }
                let notification_thread_id = extract_notification_thread_id(params);
                let notification_turn_id = extract_turn_id(params);
                match self
                    .open_agent_message_item(
                        message_id.clone(),
                        CodexProviderOpenCause::Delta,
                        notification_thread_id.as_deref(),
                        notification_turn_id.as_deref(),
                    )
                    .await
                {
                    CodexAgentMessageOpen::Open => {}
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::DuplicateTerminalMessageId,
                            method,
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Retired | CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            method,
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Superseded(previous) => {
                        self.reject_impossible_delta_supersession(
                            *previous,
                            method,
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                }
                if let Some((response, emitted)) =
                    self.append_text_to_active_stream(&message_id, &delta).await
                {
                    self.emitter.stream_delta(&response, &emitted);
                }
            }
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                    return;
                };
                let provider_item_id = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                if let Some(message_id) = provider_item_id.as_ref()
                    && self
                        .handle_root_late_provider_event(
                            message_id,
                            CodexLateProviderEvent::Delta {
                                kind: CodexProviderItemKind::Reasoning,
                                content: delta.clone(),
                            },
                            method,
                        )
                        .await
                {
                    return;
                }
                let notification_thread_id = extract_notification_thread_id(params);
                let notification_turn_id = extract_turn_id(params);
                match self
                    .open_reasoning_message_item(
                        provider_item_id.clone(),
                        CodexProviderOpenCause::Delta,
                        notification_thread_id.as_deref(),
                        notification_turn_id.as_deref(),
                    )
                    .await
                {
                    CodexAgentMessageOpen::Open => {}
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::DuplicateTerminalMessageId,
                            method,
                            provider_item_id.as_ref().map(|item_id| item_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Retired | CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            method,
                            provider_item_id.as_ref().map(|item_id| item_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                    CodexAgentMessageOpen::Superseded(previous) => {
                        self.reject_impossible_delta_supersession(
                            *previous,
                            method,
                            provider_item_id.as_ref().map(|item_id| item_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                }
                self.append_reasoning_to_active_stream(&delta).await;
            }
            "item/started" => {
                tracing::debug!(
                    kind = "started",
                    item_type = params
                        .get("item")
                        .and_then(|i| i.get("type"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    id = params
                        .get("item")
                        .and_then(|i| i.get("id"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    call_id = params
                        .get("item")
                        .and_then(|i| i.get("callId").or_else(|| i.get("call_id")))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    "PROBE typed item event"
                );
                self.handle_item_started(params).await;
            }
            "item/completed" => {
                tracing::debug!(
                    kind = "completed",
                    item_type = params
                        .get("item")
                        .and_then(|i| i.get("type"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    id = params
                        .get("item")
                        .and_then(|i| i.get("id"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    call_id = params
                        .get("item")
                        .and_then(|i| i.get("callId").or_else(|| i.get("call_id")))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<none>"),
                    "PROBE typed item event"
                );
                self.handle_item_completed(params).await;
                // A foreground completion can be the event that releases a
                // deferred idle boundary. Only after that boundary may queued
                // background results start their provider wake turn.
                self.spawn_pending_background_wake();
            }
            "turn/plan/updated" => {
                self.handle_plan_update(params);
            }
            "thread/tokenUsage/updated" => {
                self.handle_root_token_usage_updated(params).await;
                let turn_id = extract_turn_id(params);
                self.flush_raw_modify_failures(turn_id.as_deref()).await;
                self.finalize_strict_response_at_token_usage(params).await;
            }
            "model/rerouted" => {
                if let Some(model) = params.get("toModel").and_then(Value::as_str) {
                    let mut state = self.state.lock().await;
                    state.effective_model = Some(model.to_string());
                }
            }
            "turn/completed" => {
                self.handle_turn_completed(params).await;
            }
            "error" => {
                self.handle_error_notification(params).await;
            }
            _ => {}
        }
    }

    async fn trace_agent_message_identity_event(&self, method: &str, params: &Value) {
        let item = params.get("item");
        let item_type = item
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .or_else(|| (method == "item/agentMessage/delta").then_some("agentMessage"));
        if item_type != Some("agentMessage") {
            return;
        }

        let provider_item_id = item
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .or_else(|| params.get("itemId").and_then(Value::as_str))
            .or_else(|| params.get("item_id").and_then(Value::as_str));
        let thread_id = params
            .get("threadId")
            .or_else(|| params.get("thread_id"))
            .and_then(Value::as_str);
        let turn_id = extract_turn_id(params);
        let (active_turn_id, active_item_id, active_buffer_len) = {
            let state = self.state.lock().await;
            (
                state.active_turn_id.clone(),
                state
                    .active_stream
                    .as_ref()
                    .map(|stream| stream.message_id.clone()),
                state
                    .active_stream
                    .as_ref()
                    .map_or(0, |stream| stream.text.len()),
            )
        };
        tracing::debug!(
            codex_method = method,
            ?thread_id,
            ?turn_id,
            ?provider_item_id,
            provider_item_type = item_type,
            ?active_turn_id,
            ?active_item_id,
            active_buffer_len,
            "Codex agentMessage identity event"
        );
    }

    /// Per-notification trace of everything arriving from the Codex app-server.
    ///
    /// Two levels, deliberately:
    ///
    /// - `debug` — the *derived* view: method, item id/type/status, the turn and
    ///   thread this was attributed to, and a monotonic sequence number.
    /// - `trace` — the **verbatim JSON** Codex sent, same sequence number.
    ///
    /// The `trace` level exists because every other log in this file reports
    /// what Tyde *concluded*, not what the provider *said*. When a tool card
    /// never completes, the question is almost always "did the item carry a
    /// correlation id we failed to read, or did Codex genuinely not send one?"
    /// — and a derived log cannot answer it. Field values are only formatted if
    /// a subscriber is interested, so the raw dump costs nothing when off.
    ///
    /// # Seeing it
    ///
    /// Scope the filter to this module; `trace` across the whole crate is
    /// unreadable. Pair the sequence numbers to line a raw payload up against
    /// the `debug` line and the `TYDE CODEX STRICT …` lines for the same event.
    ///
    /// ```text
    /// TYDE_RUN_REAL_AI_TESTS=1 TYDE_REAL_BACKENDS=codex \
    ///   RUST_LOG=server::backend::codex=trace \
    ///   cargo nextest run -p tests --test conformance --run-ignored all \
    ///     -E 'test(=real_conversation)' --no-capture
    /// ```
    ///
    /// `tests/tests/conformance.rs` installs a subscriber over
    /// `EnvFilter::from_default_env()`, so `RUST_LOG` is all that is required —
    /// without it the events are compiled in but discarded.
    ///
    /// Keep the model on a cheap pin. The suites run `gpt-5.6-luna`; setting
    /// `TYDE_CODEX_TEST_MODEL` to another one is how you tell model-specific
    /// drift from a real defect.
    async fn trace_notification_structure(&self, method: &str, params: &Value) {
        if method == "mcpServer/startupStatus/updated" {
            tracing::info!(?params, "Codex MCP startup status");
        }
        let item = params.get("item");
        let item_id = item
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .or_else(|| params.get("itemId").and_then(Value::as_str))
            .or_else(|| params.get("item_id").and_then(Value::as_str));
        let item_type = item
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str);
        let item_status = item
            .and_then(|item| item.get("status"))
            .and_then(Value::as_str);
        let tool_name = item
            .and_then(|item| item.get("tool"))
            .and_then(Value::as_str);
        let (sequence, active_turn_id, active_item_id, tool_container_id, pending_tool_count) = {
            let mut state = self.state.lock().await;
            state.notification_sequence = state.notification_sequence.saturating_add(1);
            (
                state.notification_sequence,
                state.active_turn_id.clone(),
                state
                    .active_stream
                    .as_ref()
                    .map(|stream| stream.message_id.clone()),
                state.tool_container.clone(),
                state.pending_tool_call_ids.len(),
            )
        };
        tracing::debug!(
            codex_notification_sequence = sequence,
            codex_method = method,
            thread_id = ?extract_notification_thread_id(params),
            turn_id = ?extract_turn_id(params),
            ?item_id,
            ?item_type,
            ?item_status,
            ?active_turn_id,
            ?active_item_id,
            ?tool_container_id,
            pending_tool_count,
            "Codex notification structure"
        );
        // Ground truth. Same sequence number as the line above, so the two can
        // be paired; `%params` is only formatted when a subscriber is enabled.
        tracing::trace!(
            codex_notification_sequence = sequence,
            codex_method = method,
            params = %params,
            "Codex notification payload"
        );
        if tool_name.is_some_and(|tool_name| {
            is_tyde_agent_control_spawn_tool_name(tool_name)
                || is_tyde_agent_control_await_tool_name(tool_name)
        }) {
            tracing::info!(
                codex_notification_sequence = sequence,
                codex_method = method,
                thread_id = ?extract_notification_thread_id(params),
                turn_id = ?extract_turn_id(params),
                ?item_id,
                ?item_type,
                ?item_status,
                ?tool_name,
                ?active_turn_id,
                ?tool_container_id,
                pending_tool_count,
                "Observed Codex Tyde agent-control notification"
            );
        }
    }

    async fn trace_terminal_emission(&self, terminal: &'static str, message_id: Option<&str>) {
        let (sequence, active_turn_id, active_item_id, tool_container_id, pending_tool_count) = {
            let state = self.state.lock().await;
            (
                state.notification_sequence,
                state.active_turn_id.clone(),
                state
                    .active_stream
                    .as_ref()
                    .map(|stream| stream.message_id.clone()),
                state.tool_container.clone(),
                state.pending_tool_call_ids.len(),
            )
        };
        tracing::debug!(
            codex_notification_sequence = sequence,
            tyde_terminal = terminal,
            ?message_id,
            ?active_turn_id,
            ?active_item_id,
            ?tool_container_id,
            pending_tool_count,
            "Codex terminal emission structure"
        );
    }

    async fn handle_root_token_usage_updated(&self, params: &Value) {
        let (metadata_update, model_usage) = {
            let mut state = self.state.lock().await;
            let model = state.effective_model.clone();
            let model_usage = extract_model_request_token_usage(params, model.as_deref()).and_then(
                |(turn_id, request, cumulative, context_window)| {
                    let usage = record_model_request_token_usage(
                        &mut state.model_token_usage_by_turn,
                        turn_id,
                        request,
                        cumulative,
                        context_window,
                    )?;
                    Some(usage)
                },
            );
            let Some((turn_id, token_usage)) = extract_turn_token_usage(params, model.as_deref())
            else {
                return;
            };

            let metadata_update =
                if let Some(pending) = state.completed_message_metadata_by_turn.remove(&turn_id) {
                    let model_token_usage = state.model_token_usage_by_turn.get(&turn_id).cloned();
                    Some((pending, token_usage, model_token_usage, None))
                } else if state.active_turn_id.as_deref() == Some(turn_id.as_str())
                    || state
                        .active_stream
                        .as_ref()
                        .is_some_and(|stream| stream.turn_id == turn_id)
                    || state
                        .pending_message_metadata
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == turn_id)
                {
                    state.token_usage_by_turn.insert(turn_id, token_usage);
                    None
                } else {
                    None
                };
            (metadata_update, model_usage)
        };

        if let Some(usage) = model_usage {
            self.emitter.model_request_token_usage(&usage);
        }
        if let Some((pending, token_usage, model_token_usage, context_breakdown)) = metadata_update
        {
            emit_codex_message_metadata_update(
                &self.emitter,
                pending,
                Some(token_usage),
                model_token_usage.as_ref(),
                context_breakdown,
            );
        }
    }

    async fn handle_subagent_notification_if_needed(
        self: &Arc<Self>,
        method: &str,
        params: &Value,
    ) -> bool {
        let explicitly_thread_scoped_control = matches!(method, "model/rerouted" | "error")
            && extract_notification_thread_id(params).is_some();
        if !is_thread_scoped_codex_notification(method) && !explicitly_thread_scoped_control {
            return false;
        }
        let owner = {
            let state = self.state.lock().await;
            classify_codex_notification_owner(&state, params)
        };

        match owner {
            CodexNotificationOwner::Parent { thread_id } => {
                tracing::debug!(method, thread_id, "Codex notification ownership: parent");
                false
            }
            CodexNotificationOwner::LiveChild { thread_id } => {
                self.register_codex_descendant_from_notification(&thread_id, params)
                    .await;
                let model = self
                    .state
                    .lock()
                    .await
                    .effective_model
                    .clone()
                    .unwrap_or_else(|| "codex".to_string());
                tracing::info!(
                    method,
                    thread_id,
                    "Codex notification ownership: live child"
                );
                self.handle_subagent_notification(method, params, &thread_id, &model)
                    .await;
                true
            }
            CodexNotificationOwner::CompletedChild { thread_id } => {
                self.register_codex_descendant_from_notification(&thread_id, params)
                    .await;
                let model = self
                    .state
                    .lock()
                    .await
                    .effective_model
                    .clone()
                    .unwrap_or_else(|| "codex".to_string());
                tracing::warn!(
                    method,
                    thread_id,
                    "Codex notification ownership: completed child"
                );
                self.handle_completed_subagent_notification(method, params, &thread_id, &model)
                    .await;
                true
            }
            CodexNotificationOwner::Descendant {
                thread_id,
                ancestor_thread_id,
            } => {
                self.register_codex_descendant_from_notification(&ancestor_thread_id, params)
                    .await;
                tracing::info!(
                    method,
                    thread_id,
                    ancestor_thread_id,
                    "Codex notification ownership: nested descendant"
                );
                true
            }
            CodexNotificationOwner::Unknown { thread_id } => {
                let thread_id = thread_id.unwrap_or_else(|| "<missing>".to_string());
                let key = format!("{method}:{thread_id}");
                let (first_observation, known_child_count) = {
                    let mut state = self.state.lock().await;
                    let first_observation = state.unknown_owner_notifications.insert(key);
                    let known_child_count =
                        state.subagent_streams.len() + state.completed_subagent_streams.len();
                    (first_observation, known_child_count)
                };
                if first_observation {
                    let message = format!(
                        "Codex ownership invariant failed: thread-scoped notification '{method}' belongs to unregistered thread '{thread_id}'"
                    );
                    tracing::error!(method, thread_id, known_child_count, "{message}");
                    self.emitter.backend_error(&message);
                } else {
                    tracing::debug!(
                        method,
                        thread_id,
                        "Repeated unknown Codex thread notification suppressed"
                    );
                }
                true
            }
        }
    }

    async fn register_codex_descendant_from_notification(
        &self,
        ancestor_thread_id: &str,
        params: &Value,
    ) {
        let Some(item) = params.get("item") else {
            return;
        };
        let Some(activity) = parse_codex_subagent_activity(item) else {
            return;
        };
        if activity.kind != "started" {
            return;
        }
        let descendant_thread_id = activity.agent_thread_id;
        let mut state = self.state.lock().await;
        match state.descendant_owner_threads.get(&descendant_thread_id) {
            Some(existing) if existing != ancestor_thread_id => {
                let message = format!(
                    "Codex ownership invariant failed: descendant thread '{descendant_thread_id}' names both '{existing}' and '{ancestor_thread_id}' as its direct-child ancestor"
                );
                tracing::error!(
                    descendant_thread_id,
                    existing_ancestor_thread_id = existing,
                    ancestor_thread_id,
                    "{message}"
                );
                drop(state);
                self.emitter.backend_error(&message);
            }
            Some(_) => {}
            None => {
                tracing::info!(
                    descendant_thread_id,
                    ancestor_thread_id,
                    agent_path = activity.agent_path,
                    "Registered Codex nested descendant ownership"
                );
                state
                    .descendant_owner_threads
                    .insert(descendant_thread_id, ancestor_thread_id.to_owned());
            }
        }
    }

    async fn handle_subagent_notification(
        self: &Arc<Self>,
        method: &str,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        eprintln!(
            "TYDE CODEX CHILD EVENT thread={stream_key} method={method} turn_id={:?} turn_status={:?} will_retry={:?} message={:?}",
            extract_turn_id(params),
            params
                .get("turn")
                .and_then(|turn| turn.get("status"))
                .and_then(Value::as_str),
            params
                .get("willRetry")
                .or_else(|| params.get("will_retry"))
                .and_then(Value::as_bool),
            params.get("message").and_then(Value::as_str).or_else(|| {
                params
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
            })
        );
        {
            let suppress_terminated_response = {
                let state = self.state.lock().await;
                state
                    .subagent_streams
                    .get(stream_key)
                    .is_some_and(|stream| {
                        let provider_item_id = params
                            .get("item")
                            .and_then(|item| item.get("id"))
                            .and_then(Value::as_str)
                            .or_else(|| params.get("itemId").and_then(Value::as_str))
                            .or_else(|| params.get("item_id").and_then(Value::as_str));
                        // Mirrors the root gate: only loss-recording
                        // tombstones bypass the post-violation quiet window.
                        let is_tombstoned_item = provider_item_id.is_some_and(|message_id| {
                            stream.provider_item_tombstones.iter().any(|tombstone| {
                                tombstone.message_id.0 == message_id
                                    && tombstone.disposition
                                        != CodexProviderItemDisposition::Completed
                            })
                        });
                        let notification_turn_id = extract_turn_id(params);
                        let is_explicitly_terminated_turn =
                            notification_turn_id.as_ref().is_some_and(|turn_id| {
                                stream
                                    .terminated_turns
                                    .iter()
                                    .any(|terminated| terminated.turn_id == *turn_id)
                            });
                        let awaiting_new_turn_after_termination = notification_turn_id.is_none()
                            && stream.active_turn_id.is_none()
                            && stream
                                .terminated_turn_awaiting_replacement
                                .as_ref()
                                .is_some_and(|awaiting_turn_id| {
                                    stream
                                        .terminated_turns
                                        .iter()
                                        .any(|terminated| terminated.turn_id == *awaiting_turn_id)
                                });
                        !matches!(method, "turn/started" | "turn/completed")
                            && !is_tombstoned_item
                            && (is_explicitly_terminated_turn
                                || awaiting_new_turn_after_termination)
                    })
            };
            if suppress_terminated_response {
                if method == "item/completed"
                    && let Some(item) = params.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("commandExecution")
                    && let Some(provider_item_id) = item.get("id").and_then(Value::as_str)
                {
                    let command = self.take_background_command(params, provider_item_id).await;
                    let _ = self
                        .forget_command_execution(params, provider_item_id)
                        .await;
                    self.warn_codex_raw_contract_drift_once_if_needed().await;
                    if let Some(command) = command
                        && let Some(emitter) = self.codex_subagent_emitter(stream_key).await
                    {
                        let exit_code = item
                            .get("exitCode")
                            .or_else(|| item.get("exit_code"))
                            .and_then(Value::as_i64)
                            .unwrap_or(-1);
                        let output = item
                            .get("aggregatedOutput")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let success = exit_code == 0;
                        emitter.tool_completed(
                            &command.tool_call_id,
                            codex_tool_execution_outcome(
                                json!({
                                    "kind": "RunCommand",
                                    "exit_code": exit_code,
                                    "stdout": output,
                                    "stderr": ""
                                }),
                                success,
                                (!success)
                                    .then(|| format!("Command failed with exit code {exit_code}")),
                                None,
                            ),
                        );
                    }
                }
                tracing::debug!(
                    child_thread_id = stream_key,
                    codex_method = method,
                    "Ignoring late Codex response notification for terminated child turn"
                );
                return;
            }
        }
        match method {
            "turn/started" => {
                self.close_subagent_tool_container_if_open(stream_key).await;
                let turn_id = extract_turn_id(params).unwrap_or_else(|| "turn".to_string());
                let (terminated, duplicate) = {
                    let state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get(stream_key) else {
                        return;
                    };
                    (
                        stream
                            .terminated_turns
                            .iter()
                            .any(|turn| turn.turn_id == turn_id),
                        stream.active_turn_id.as_ref() == Some(&turn_id),
                    )
                };
                if terminated || duplicate {
                    tracing::debug!(
                        child_thread_id = stream_key,
                        turn_id,
                        terminated,
                        "Ignoring duplicate or locally terminated Codex child turn start"
                    );
                    return;
                }
                let Some(emitter) = self
                    .update_codex_subagent_stream(stream_key, |stream| {
                        stream.terminated_turn_awaiting_replacement = None;
                        stream.active_turn_id = Some(turn_id.clone());
                        stream.current_message_id = None;
                        stream.current_generated_identity = None;
                        stream.current_reasoning_only = false;
                        stream.current_stream_published = false;
                        stream.current_response = None;
                        stream.current_text.clear();
                        stream.current_reasoning.clear();
                        stream.current_tool_call_ids.clear();
                        stream.current_images.clear();
                        stream.pending_tool_call_ids.clear();
                        stream.tool_container_images.clear();
                        stream.retired_unpublished_message_ids.clear();
                        stream.provider_supersessions_this_turn = 0;
                        stream.supersession_warning_emitted = false;
                        stream.pending_message_metadata = None;
                    })
                    .await
                else {
                    return;
                };
                emitter.typing_status_changed(true);
            }
            "item/agentMessage/delta" => {
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if delta.is_empty() {
                    return;
                }
                let Some(message_id) = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|message_id| !message_id.trim().is_empty())
                    .map(|message_id| ChatMessageId(message_id.to_string()))
                else {
                    self.reject_subagent_message_identity(
                        stream_key,
                        CodexProviderStreamConflict::MissingMessageId,
                        method,
                    )
                    .await;
                    return;
                };
                if self
                    .handle_subagent_late_provider_event(
                        stream_key,
                        &message_id,
                        CodexLateProviderEvent::Delta {
                            kind: CodexProviderItemKind::AgentMessage,
                            content: delta.clone(),
                        },
                        method,
                    )
                    .await
                {
                    return;
                }
                self.close_subagent_tool_container_if_open(stream_key).await;
                let (emitter, _publish, emitted, violation) = {
                    let mut state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                        return;
                    };
                    if stream.completed_agent_messages.contains_key(&message_id) {
                        (
                            Arc::clone(&stream.emitter),
                            false,
                            None,
                            Some(CodexProviderStreamConflict::DuplicateTerminalMessageId),
                        )
                    } else if stream.retired_unpublished_message_ids.contains(&message_id) {
                        (
                            Arc::clone(&stream.emitter),
                            false,
                            None,
                            Some(CodexProviderStreamConflict::ForeignActiveMessageId),
                        )
                    } else if let Some(active_id) = stream.current_message_id.as_ref() {
                        if active_id == &message_id && !stream.current_reasoning_only {
                            let was_published = stream.current_stream_published;
                            stream.current_text.push_str(&delta);
                            if !stream.current_stream_published
                                && contains_non_whitespace(&stream.current_text)
                            {
                                stream.current_stream_published = true;
                            }
                            let emitted = stream.current_stream_published.then(|| {
                                if was_published {
                                    delta.clone()
                                } else {
                                    stream.current_text.clone()
                                }
                            });
                            (
                                Arc::clone(&stream.emitter),
                                !was_published && stream.current_stream_published,
                                emitted,
                                None,
                            )
                        } else if stream.retire_replaceable_provider_reservation() {
                            stream.current_message_id = Some(message_id.clone());
                            stream.current_response = None;
                            stream.current_generated_identity = None;
                            stream.current_reasoning_only = false;
                            stream.current_text = delta.clone();
                            stream.current_stream_published =
                                contains_non_whitespace(&stream.current_text);
                            let published = stream.current_stream_published;
                            let emitted = published.then(|| stream.current_text.clone());
                            (Arc::clone(&stream.emitter), published, emitted, None)
                        } else {
                            (
                                Arc::clone(&stream.emitter),
                                false,
                                None,
                                Some(CodexProviderStreamConflict::ForeignActiveMessageId),
                            )
                        }
                    } else {
                        stream.current_message_id = Some(message_id.clone());
                        stream.current_response = None;
                        stream.current_generated_identity = None;
                        stream.current_reasoning_only = false;
                        stream.current_stream_published = false;
                        stream.current_text.clear();
                        stream.current_reasoning.clear();
                        stream.current_tool_call_ids.clear();
                        stream.current_images.clear();
                        stream.current_text.push_str(&delta);
                        stream.current_stream_published =
                            contains_non_whitespace(&stream.current_text);
                        let published = stream.current_stream_published;
                        let emitted = published.then(|| stream.current_text.clone());
                        (Arc::clone(&stream.emitter), published, emitted, None)
                    }
                };
                if let Some(violation) = violation {
                    self.reject_subagent_message_identity(stream_key, violation, method)
                        .await;
                    return;
                }
                if let Some(emitted) = emitted {
                    let response = {
                        let mut state = self.state.lock().await;
                        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                            return;
                        };
                        stream
                            .current_response
                            .get_or_insert_with(|| emitter.stream_start(Some(model)))
                            .clone()
                    };
                    emitter.stream_delta(&response, &emitted);
                }
            }
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                let Some(delta) = extract_codex_reasoning_delta_text(params) else {
                    return;
                };
                let provider_item_id = params
                    .get("itemId")
                    .or_else(|| params.get("item_id"))
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                if let Some(message_id) = provider_item_id.as_ref()
                    && self
                        .handle_subagent_late_provider_event(
                            stream_key,
                            message_id,
                            CodexLateProviderEvent::Delta {
                                kind: CodexProviderItemKind::Reasoning,
                                content: delta.clone(),
                            },
                            method,
                        )
                        .await
                {
                    return;
                }
                self.close_subagent_tool_container_if_open(stream_key).await;
                let (emitter, _message_id, _generated_identity, _publish, emitted, violation) = {
                    let mut state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                        return;
                    };
                    if provider_item_id.as_ref().is_some_and(|message_id| {
                        stream.retired_unpublished_message_ids.contains(message_id)
                    }) {
                        (
                            Arc::clone(&stream.emitter),
                            None,
                            None,
                            false,
                            None,
                            Some(CodexProviderStreamConflict::ForeignActiveMessageId),
                        )
                    } else if let Some(active_message_id) = stream.current_message_id.clone() {
                        let matches_idless_reasoning = stream.current_reasoning_only
                            && stream
                                .current_generated_identity
                                .as_ref()
                                .is_some_and(|identity| {
                                    identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                                });
                        let matches = match provider_item_id.as_ref() {
                            Some(item_id) => {
                                item_id == &active_message_id && stream.current_reasoning_only
                            }
                            None => matches_idless_reasoning,
                        };
                        if !matches
                            && provider_item_id.is_some()
                            && stream.retire_replaceable_provider_reservation()
                        {
                            let message_id = provider_item_id
                                .clone()
                                .expect("provider reasoning replacement has an id");
                            stream.current_message_id = Some(message_id.clone());
                            stream.current_response = None;
                            stream.current_generated_identity = None;
                            stream.current_reasoning_only = true;
                            stream.current_reasoning = delta.clone();
                            stream.current_stream_published =
                                contains_non_whitespace(&stream.current_reasoning);
                            let published = stream.current_stream_published;
                            let emitted = published.then(|| stream.current_reasoning.clone());
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                None,
                                published,
                                emitted,
                                None,
                            )
                        } else if !matches {
                            (
                                Arc::clone(&stream.emitter),
                                None,
                                None,
                                false,
                                None,
                                Some(CodexProviderStreamConflict::ForeignActiveMessageId),
                            )
                        } else {
                            let was_published = stream.current_stream_published;
                            stream.current_reasoning.push_str(&delta);
                            if !stream.current_stream_published
                                && contains_non_whitespace(&stream.current_reasoning)
                            {
                                stream.current_stream_published = true;
                            }
                            let emitted = stream.current_stream_published.then(|| {
                                if was_published {
                                    delta.clone()
                                } else {
                                    stream.current_reasoning.clone()
                                }
                            });
                            (
                                Arc::clone(&stream.emitter),
                                Some(active_message_id),
                                stream.current_generated_identity.clone(),
                                !was_published && stream.current_stream_published,
                                emitted,
                                None,
                            )
                        }
                    } else {
                        let generated_identity = provider_item_id.is_none().then(|| {
                            let identity = CodexProviderResponseIdentity {
                                origin: CodexProviderResponseOrigin::IdlessReasoning,
                                stream_epoch: stream.generated_identity_epoch,
                                item_ordinal: stream.next_generated_identity_ordinal,
                            };
                            stream.next_generated_identity_ordinal =
                                stream.next_generated_identity_ordinal.saturating_add(1);
                            identity
                        });
                        let message_id = provider_item_id.clone().unwrap_or_else(|| {
                            generated_identity
                                .as_ref()
                                .expect("generated child reasoning identity")
                                .message_id()
                        });
                        if stream.completed_agent_messages.contains_key(&message_id) {
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                generated_identity,
                                false,
                                None,
                                Some(CodexProviderStreamConflict::DuplicateTerminalMessageId),
                            )
                        } else {
                            stream.current_message_id = Some(message_id.clone());
                            stream.current_response = None;
                            stream.current_generated_identity = generated_identity.clone();
                            stream.current_reasoning_only = true;
                            stream.current_stream_published = false;
                            stream.current_text.clear();
                            stream.current_reasoning = delta.clone();
                            stream.current_tool_call_ids.clear();
                            stream.current_images.clear();
                            stream.current_stream_published =
                                contains_non_whitespace(&stream.current_reasoning);
                            let published = stream.current_stream_published;
                            let emitted = published.then(|| stream.current_reasoning.clone());
                            (
                                Arc::clone(&stream.emitter),
                                Some(message_id),
                                generated_identity,
                                published,
                                emitted,
                                None,
                            )
                        }
                    }
                };
                if let Some(violation) = violation {
                    self.reject_subagent_message_identity(stream_key, violation, method)
                        .await;
                    return;
                }
                if let Some(emitted) = emitted {
                    let response = {
                        let mut state = self.state.lock().await;
                        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                            return;
                        };
                        stream
                            .current_response
                            .get_or_insert_with(|| emitter.stream_start(Some(model)))
                            .clone()
                    };
                    emitter.stream_reasoning_delta(&response, &emitted);
                }
            }
            "item/started" => {
                let item_type = params
                    .pointer("/item/type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (strict_response, provider_tool) =
                    self.observe_strict_response_item_started(params).await;
                let provider_kind = match item_type {
                    "agentMessage" => Some(CodexProviderItemKind::AgentMessage),
                    "reasoning" => Some(CodexProviderItemKind::Reasoning),
                    _ => None,
                };
                if strict_response && provider_kind.is_some() {
                    return;
                }
                if strict_response && is_codex_provider_tool_item_type(item_type) && !provider_tool
                {
                    self.handle_strict_execution_only_tool_started(params).await;
                    return;
                }
                if let Some(provider_kind) = provider_kind {
                    let provider_message_id = params
                        .pointer("/item/id")
                        .and_then(Value::as_str)
                        .filter(|message_id| !message_id.trim().is_empty())
                        .map(|message_id| ChatMessageId(message_id.to_string()));
                    if provider_kind == CodexProviderItemKind::AgentMessage
                        && provider_message_id.is_none()
                    {
                        self.reject_subagent_message_identity(
                            stream_key,
                            CodexProviderStreamConflict::MissingMessageId,
                            method,
                        )
                        .await;
                        return;
                    }
                    if let Some(message_id) = provider_message_id.as_ref()
                        && self
                            .handle_subagent_late_provider_event(
                                stream_key,
                                message_id,
                                CodexLateProviderEvent::Started {
                                    kind: provider_kind,
                                },
                                method,
                            )
                            .await
                    {
                        return;
                    }
                    let notification_thread_id = extract_notification_thread_id(params);
                    let notification_turn_id = extract_turn_id(params);
                    let result = self
                        .open_subagent_provider_item(
                            stream_key,
                            provider_message_id.clone(),
                            provider_kind,
                            CodexProviderOpenCause::ItemStarted,
                            CodexProviderNotificationOwner {
                                thread_id: notification_thread_id.as_deref(),
                                turn_id: notification_turn_id.as_deref(),
                            },
                            model,
                        )
                        .await;
                    match result {
                        CodexSubAgentMessageOpen::Open
                        | CodexSubAgentMessageOpen::Existing
                        | CodexSubAgentMessageOpen::Retired => {}
                        CodexSubAgentMessageOpen::Superseded(finalized) => {
                            self.finalize_subagent_provider_supersession(
                                stream_key,
                                *finalized,
                                provider_message_id
                                    .as_ref()
                                    .expect("superseded child item has provider id"),
                                provider_kind,
                                model,
                            )
                            .await;
                        }
                        CodexSubAgentMessageOpen::Terminal => {
                            self.reject_subagent_message_identity(
                                stream_key,
                                CodexProviderStreamConflict::DuplicateTerminalMessageId,
                                method,
                            )
                            .await;
                        }
                        CodexSubAgentMessageOpen::Foreign => {
                            self.reject_subagent_message_identity(
                                stream_key,
                                CodexProviderStreamConflict::ForeignActiveMessageId,
                                method,
                            )
                            .await;
                        }
                    }
                    return;
                }
                if !matches!(
                    item_type,
                    "commandExecution"
                        | "fileChange"
                        | "imageGeneration"
                        | "webSearch"
                        | "imageView"
                        | "sleep"
                        | "collabToolCall"
                        | "collabAgentToolCall"
                        | "mcpToolCall"
                        | "dynamicToolCall"
                ) {
                    return;
                }
                let boundary = {
                    let mut state = self.state.lock().await;
                    let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                        return;
                    };
                    if stream.current_message_id.is_some()
                        && stream.current_stream_published
                        && stream.current_reasoning_only
                    {
                        Err(CodexProviderStreamConflict::ForeignActiveMessageId)
                    } else {
                        Ok(Arc::clone(&stream.emitter))
                    }
                };
                let emitter = match boundary {
                    Ok(boundary) => boundary,
                    Err(violation) => {
                        self.reject_subagent_message_identity(stream_key, violation, method)
                            .await;
                        return;
                    }
                };
                let (container, tool_call_ids) = self
                    .handle_subagent_item_started(params, stream_key, emitter.as_ref())
                    .await;
                if container.is_some() || !tool_call_ids.is_empty() {
                    let ownership_violation = {
                        let mut state = self.state.lock().await;
                        state
                            .subagent_streams
                            .get_mut(stream_key)
                            .and_then(|stream| {
                                stream
                                    .pending_tool_call_ids
                                    .extend(tool_call_ids.iter().cloned());
                                if let Some(container) = container {
                                    if stream.current_stream_published
                                        || stream.tool_container.is_some()
                                    {
                                        return Some(Arc::clone(&stream.emitter));
                                    }
                                    if let Some(message_id) = stream.current_message_id.as_ref() {
                                        tracing::debug!(
                                            child_thread_id = stream_key,
                                            provider_item_id = message_id.0.as_str(),
                                            tool_container_id = container.0.as_str(),
                                            "Opening a Codex child tool container beside an unpublished provider reservation"
                                        );
                                    }
                                    stream.tool_container = Some(container);
                                } else if stream.tool_container.is_none()
                                    && stream.current_message_id.is_some()
                                {
                                    for tool_call_id in tool_call_ids {
                                        if !stream.current_tool_call_ids.contains(&tool_call_id) {
                                            stream.current_tool_call_ids.push(tool_call_id);
                                        }
                                    }
                                }
                                None
                            })
                    };
                    if ownership_violation.is_some() {
                        self.reject_subagent_message_identity(
                            stream_key,
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            method,
                        )
                        .await;
                    }
                }
            }
            "item/completed" => {
                if let Some((provider_owned, owner)) = self.finish_strict_typed_tool(params).await
                    && !provider_owned
                {
                    self.handle_strict_execution_only_tool_completed(params, owner)
                        .await;
                    return;
                }
                if self.handle_strict_response_item_completed(params).await {
                    return;
                }
                self.handle_subagent_item_completed(params, stream_key, model)
                    .await;
            }
            "turn/plan/updated" => {
                let tasks = codex_plan_update_task_list_from_params(params).unwrap_or_else(|| {
                    protocol::TaskList {
                        title: "Plan".to_string(),
                        tasks: Vec::new(),
                    }
                });
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                emitter.task_update(&tasks);
            }
            "thread/tokenUsage/updated" => {
                self.handle_subagent_token_usage_updated(params, stream_key, model)
                    .await;
                self.finalize_strict_response_at_token_usage(params).await;
            }
            "turn/completed" => {
                self.handle_subagent_turn_completed(params, stream_key, model)
                    .await;
            }
            "error" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        params
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("Codex error")
                    .to_string();
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                if let Some(tool_call_id) = codex_error_tool_call_id(params)
                    && emitter.fail_pending_tool(&tool_call_id, &message)
                {
                    self.complete_subagent_tool_calls(stream_key, &[tool_call_id])
                        .await;
                    return;
                }
                emitter.backend_error(&message);
                emitter.typing_status_changed(false);
            }
            _ => {}
        }
    }

    async fn handle_completed_subagent_notification(
        &self,
        method: &str,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        if matches!(method, "item/started" | "item/completed")
            && params
                .pointer("/item/type")
                .and_then(Value::as_str)
                .is_some_and(|item_type| item_type.eq_ignore_ascii_case("subAgentActivity"))
        {
            tracing::info!(
                child_thread_id = stream_key,
                codex_method = method,
                "Accepted nested activity from completed Codex child"
            );
            return;
        }
        if self
            .state
            .lock()
            .await
            .completed_subagent_streams
            .get(stream_key)
            .is_some_and(|stream| stream.owner_terminated)
        {
            tracing::debug!(
                child_thread_id = stream_key,
                codex_method = method,
                "Dropping late Codex child event after owner termination"
            );
            return;
        }
        if method == "item/completed"
            && let Some(item) = params.get("item")
            && item.get("type").and_then(Value::as_str) == Some("commandExecution")
            && let Some(provider_item_id) = item.get("id").and_then(Value::as_str)
        {
            let command = self.take_background_command(params, provider_item_id).await;
            let _ = self
                .forget_command_execution(params, provider_item_id)
                .await;
            self.warn_codex_raw_contract_drift_once_if_needed().await;
            if let Some(command) = command {
                let emitter = {
                    let state = self.state.lock().await;
                    state
                        .completed_subagent_streams
                        .get(stream_key)
                        .map(|stream| Arc::clone(&stream.emitter))
                };
                if let Some(emitter) = emitter {
                    let exit_code = item
                        .get("exitCode")
                        .or_else(|| item.get("exit_code"))
                        .and_then(Value::as_i64)
                        .unwrap_or(-1);
                    let output = item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let success = exit_code == 0;
                    emitter.tool_completed(
                        &command.tool_call_id,
                        codex_tool_execution_outcome(
                            json!({
                                "kind": "RunCommand",
                                "exit_code": exit_code,
                                "stdout": output,
                                "stderr": ""
                            }),
                            success,
                            (!success)
                                .then(|| format!("Command failed with exit code {exit_code}")),
                            None,
                        ),
                    );
                }
            }
            tracing::debug!(
                child_thread_id = stream_key,
                provider_item_id,
                "Cleaned late command completion for completed Codex child"
            );
            return;
        }
        let provider_item_id = params
            .get("item")
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .or_else(|| params.get("itemId").and_then(Value::as_str))
            .or_else(|| params.get("item_id").and_then(Value::as_str))
            .filter(|message_id| !message_id.trim().is_empty())
            .map(|message_id| ChatMessageId(message_id.to_string()));
        let late_event = match method {
            "item/agentMessage/delta" => Some(CodexLateProviderEvent::Delta {
                kind: CodexProviderItemKind::AgentMessage,
                content: params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            reasoning_method if is_reasoning_notification_method(reasoning_method) => {
                extract_codex_reasoning_delta_text(params).map(|content| {
                    CodexLateProviderEvent::Delta {
                        kind: CodexProviderItemKind::Reasoning,
                        content,
                    }
                })
            }
            "item/started" => params
                .pointer("/item/type")
                .and_then(Value::as_str)
                .and_then(|item_type| match item_type {
                    "agentMessage" => Some(CodexProviderItemKind::AgentMessage),
                    "reasoning" => Some(CodexProviderItemKind::Reasoning),
                    _ => None,
                })
                .map(|kind| CodexLateProviderEvent::Started { kind }),
            "item/completed" => {
                let item = params.get("item");
                item.and_then(|item| {
                    item.get("type")
                        .and_then(Value::as_str)
                        .and_then(|item_type| match item_type {
                            "agentMessage" => Some(CodexLateProviderEvent::Completion {
                                kind: CodexProviderItemKind::AgentMessage,
                                text: item
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| extract_codex_item_text(item)),
                                reasoning: extract_codex_item_reasoning(item),
                            }),
                            "reasoning" => Some(CodexLateProviderEvent::Completion {
                                kind: CodexProviderItemKind::Reasoning,
                                text: String::new(),
                                reasoning: extract_codex_item_reasoning(item),
                            }),
                            _ => None,
                        })
                })
            }
            _ => None,
        };
        if let (Some(provider_item_id), Some(late_event)) =
            (provider_item_id.as_ref(), late_event.as_ref())
        {
            let outcome = {
                let mut state = self.state.lock().await;
                state
                    .completed_subagent_streams
                    .get_mut(stream_key)
                    .map(|stream| {
                        classify_codex_late_provider_event(
                            &mut stream.provider_item_tombstones,
                            stream_key,
                            None,
                            provider_item_id,
                            late_event,
                        )
                    })
                    .unwrap_or(CodexLateProviderEventOutcome::NotFound)
            };
            if !matches!(outcome, CodexLateProviderEventOutcome::NotFound) {
                tracing::warn!(
                    child_thread_id = stream_key,
                    codex_method = method,
                    provider_item_id = provider_item_id.0.as_str(),
                    "Dropping late provider-item event for completed Codex child"
                );
                return;
            }
        }
        match method {
            "thread/tokenUsage/updated" | "turn/completed" => {
                self.handle_completed_subagent_token_usage(params, stream_key, model)
                    .await;
            }
            _ => {
                let emitter = {
                    let state = self.state.lock().await;
                    state
                        .completed_subagent_streams
                        .get(stream_key)
                        .map(|stream| Arc::clone(&stream.emitter))
                };
                if let Some(emitter) = emitter {
                    let message = format!(
                        "Codex ownership invariant failed: late child content '{method}' arrived after child thread '{stream_key}' completed"
                    );
                    tracing::error!(method, thread_id = stream_key, "{message}");
                    emitter.backend_error(&message);
                }
            }
        }
    }

    async fn codex_subagent_emitter(&self, stream_key: &str) -> Option<Arc<TurnEmitter>> {
        let state = self.state.lock().await;
        state
            .subagent_streams
            .get(stream_key)
            .map(|stream| Arc::clone(&stream.emitter))
    }

    async fn update_codex_subagent_stream(
        &self,
        stream_key: &str,
        update: impl FnOnce(&mut CodexSubAgentStream),
    ) -> Option<Arc<TurnEmitter>> {
        let mut state = self.state.lock().await;
        let stream = state.subagent_streams.get_mut(stream_key)?;
        update(stream);
        Some(Arc::clone(&stream.emitter))
    }

    async fn close_subagent_tool_container_if_open(&self, stream_key: &str) {
        let mut state = self.state.lock().await;
        let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
            return;
        };
        stream.tool_container = None;
        if stream.current_message_id.is_some() {
            stream
                .current_images
                .append(&mut stream.tool_container_images);
        }
    }

    async fn complete_subagent_tool_calls(&self, stream_key: &str, tool_call_ids: &[String]) {
        self.complete_subagent_tool_calls_with_images(stream_key, tool_call_ids, Vec::new())
            .await;
    }

    async fn complete_subagent_tool_calls_with_images(
        &self,
        stream_key: &str,
        tool_call_ids: &[String],
        images: Vec<protocol::ImageData>,
    ) {
        {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            stream.tool_container_images.extend(images);
            for tool_call_id in tool_call_ids {
                stream.pending_tool_call_ids.remove(tool_call_id);
            }
            if stream.pending_tool_call_ids.is_empty() {
                stream.tool_container = None;
                let images = std::mem::take(&mut stream.tool_container_images);
                stream.current_images.extend(images);
            }
        }
    }

    async fn reject_subagent_message_identity(
        &self,
        stream_key: &str,
        violation: CodexProviderStreamConflict,
        method: &str,
    ) {
        let (emitter, turn_id, provider_turn_id, finalized, model, terminated_background_commands) = {
            let mut state = self.state.lock().await;
            let model = state
                .effective_model
                .clone()
                .unwrap_or_else(|| "codex".to_string());
            let (emitter, turn_id, provider_turn_id, finalized) = {
                let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                    return;
                };
                let provider_turn_id = stream.active_turn_id.clone();
                let turn_id = provider_turn_id
                    .clone()
                    .unwrap_or_else(|| "<no-active-turn>".to_string());
                if !push_codex_terminated_turn(&mut stream.terminated_turns, turn_id.clone()) {
                    return;
                }
                stream.terminated_turn_awaiting_replacement = Some(turn_id.clone());
                let finalized = stream.current_message_id.clone().map(|message_id| {
                    let kind = if stream.current_reasoning_only {
                        CodexProviderItemKind::Reasoning
                    } else {
                        CodexProviderItemKind::AgentMessage
                    };
                    Self::finalize_subagent_provider_stream(
                        stream_key,
                        stream,
                        None,
                        message_id,
                        kind,
                        &model,
                        CodexProviderStreamFinalization::TurnAborted,
                    )
                });
                if let Some(finalized) = finalized.as_ref() {
                    push_codex_provider_item_tombstone(
                        &mut stream.provider_item_tombstones,
                        CodexProviderItemTombstone {
                            owner_thread_id: stream_key.to_string(),
                            turn_id: finalized.turn_id.clone(),
                            message_id: finalized.message_id.clone(),
                            kind: finalized.kind,
                            disposition: CodexProviderItemDisposition::TurnTerminated,
                            accepted_text: finalized.content.clone(),
                            accepted_reasoning: finalized.reasoning.clone().unwrap_or_default(),
                            late_text: String::new(),
                            late_reasoning: String::new(),
                            late_event_count: 0,
                            late_bytes: 0,
                        },
                    );
                }
                stream.active_turn_id = None;
                stream.tool_container = None;
                stream.pending_tool_call_ids.clear();
                stream.tool_container_images.clear();
                stream.pending_message_metadata = None;
                (
                    Arc::clone(&stream.emitter),
                    turn_id,
                    provider_turn_id,
                    finalized,
                )
            };
            let terminated_background_commands =
                take_codex_commands_for_turn(&mut state, stream_key, &turn_id);
            (
                emitter,
                turn_id,
                provider_turn_id,
                finalized,
                model,
                terminated_background_commands,
            )
        };
        tracing::warn!(
            child_thread_id = stream_key,
            turn_id = turn_id.as_str(),
            codex_method = method,
            ?violation,
            "Terminating Codex child turn after provider-item identity violation"
        );
        if let Some(finalized) = finalized {
            Self::emit_finalized_subagent_provider_item(finalized, &model);
        }
        for command in terminated_background_commands {
            emitter.cancel_pending_tool(
                &command.tool_call_id,
                "Codex background command was cancelled after a stream identity violation",
            );
        }
        emitter.backend_error(codex_stream_identity_violation_message(violation));
        emitter.operation_cancelled("Stream identity violation");
        if let Some(provider_turn_id) = provider_turn_id {
            self.rpc.spawn_request(
                "turn/interrupt",
                json!({
                    "threadId": stream_key,
                    "turnId": provider_turn_id,
                }),
            );
        }
    }

    async fn handle_subagent_item_started(
        self: &Arc<Self>,
        params: &Value,
        stream_key: &str,
        emitter: &TurnEmitter,
    ) -> (Option<ChatMessageId>, Vec<String>) {
        let Some(item) = params.get("item") else {
            return (None, Vec::new());
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let tool_name = match item_type {
            "imageGeneration" => "generate_image",
            "webSearch" => "web_search",
            "imageView" => "view_image",
            "sleep" => "sleep",
            "commandExecution" => "run_command",
            "fileChange" => "file_change",
            "collabToolCall" | "collabAgentToolCall" | "mcpToolCall" | "dynamicToolCall" => item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or(item_type),
            _ => return (None, Vec::new()),
        };
        let item_id = self
            .tool_call_started_id(
                params,
                item.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("tool-call"),
                tool_name,
            )
            .await;

        match item_type {
            "imageGeneration" => {
                let prompt = item
                    .get("revisedPrompt")
                    .and_then(Value::as_str)
                    .filter(|prompt| !prompt.trim().is_empty())
                    .map(str::to_owned);
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        "generate_image",
                        CodexToolRequest::from_item(
                            item,
                            serde_json::to_value(protocol::ToolRequestType::GenerateImage {
                                prompt,
                            })
                            .expect("serialize Codex image generation request"),
                        ),
                    )
                    .await;
                (container, vec![item_id])
            }
            "webSearch" => {
                let query = item
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        "web_search",
                        CodexToolRequest::from_item(
                            item,
                            serde_json::to_value(protocol::ToolRequestType::WebSearch { query })
                                .expect("serialize Codex web search request"),
                        ),
                    )
                    .await;
                (container, vec![item_id])
            }
            "imageView" => {
                let path = item
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        "view_image",
                        CodexToolRequest::from_item(
                            item,
                            serde_json::to_value(protocol::ToolRequestType::ViewImage { path })
                                .expect("serialize Codex image view request"),
                        ),
                    )
                    .await;
                (container, vec![item_id])
            }
            "sleep" => {
                let duration_ms = item.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        "sleep",
                        CodexToolRequest::from_item(
                            item,
                            serde_json::to_value(protocol::ToolRequestType::Sleep { duration_ms })
                                .expect("serialize Codex sleep request"),
                        ),
                    )
                    .await;
                (container, vec![item_id])
            }
            "commandExecution" => {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        "run_command",
                        CodexToolRequest::from_item(
                            item,
                            json!({
                                "kind": "RunCommand",
                                "command": command,
                                "working_directory": cwd
                            }),
                        ),
                    )
                    .await;
                // Children background processes exactly like the root thread,
                // and the poll is keyed by thread id, so the same tracking
                // gives the child's own tray its rows.
                self.track_command_execution(
                    params,
                    item.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("tool-call"),
                    &item_id,
                    item,
                )
                .await;
                (container, vec![item_id])
            }
            "fileChange" => {
                let file_changes = parse_codex_file_changes(item);
                if file_changes.is_empty() {
                    return (None, Vec::new());
                }
                let total = file_changes.len();
                let mut container = None;
                let mut call_ids = Vec::with_capacity(total);
                for (idx, change) in file_changes.iter().enumerate() {
                    let call_id = codex_file_change_call_id(&item_id, idx, total);
                    container = self
                        .emit_subagent_tool_request(
                            stream_key,
                            emitter,
                            &call_id,
                            "modify_file",
                            CodexToolRequest::from_item(
                                item,
                                json!({
                                    "kind": "ModifyFile",
                                    "file_path": change.path,
                                    "before": change.before,
                                    "after": change.after,
                                }),
                            ),
                        )
                        .await
                        .or(container);
                    call_ids.push(call_id);
                }
                (container, call_ids)
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        &tool_name,
                        codex_public_tool_request(&tool_name, item),
                    )
                    .await;
                (container, vec![item_id])
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                let container = self
                    .emit_subagent_tool_request(
                        stream_key,
                        emitter,
                        &item_id,
                        &tool_name,
                        CodexToolRequest::other(json!(codex_generic_tool_arguments(item))),
                    )
                    .await;
                (container, vec![item_id])
            }
            _ => (None, Vec::new()),
        }
    }

    async fn emit_subagent_tool_request(
        &self,
        stream_key: &str,
        emitter: &TurnEmitter,
        tool_call_id: &str,
        tool_name: &str,
        request: CodexToolRequest,
    ) -> Option<ChatMessageId> {
        if self
            .buffer_strict_tool_request(
                stream_key,
                tool_call_id,
                tool_name,
                request.arguments.clone(),
                request.tool_type.clone(),
            )
            .await
        {
            None
        } else {
            let model = self
                .state
                .lock()
                .await
                .effective_model
                .clone()
                .unwrap_or_else(|| "codex".to_owned());
            let response = emitter.stream_start(Some(&model));
            emitter.stream_end(
                response,
                StreamEndPayload {
                    model_info: Some(ModelInfo { model }),
                    tool_calls: vec![ToolUseData {
                        tool_call_id: tool_call_id.to_owned(),
                        name: tool_name.to_owned(),
                        arguments: request.arguments,
                        content_offset: Some(0),
                    }],
                    ..StreamEndPayload::default()
                },
            );
            emitter.tool_request(tool_call_id, codex_tool_request_type(request.tool_type));
            None
        }
    }

    async fn handle_subagent_item_completed(&self, params: &Value, stream_key: &str, model: &str) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let provider_item_id = item.get("id").and_then(Value::as_str).unwrap_or("item");
        if let Some(message_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|message_id| !message_id.trim().is_empty())
            .map(|message_id| ChatMessageId(message_id.to_string()))
        {
            let late_event = match item_type {
                "agentMessage" => Some(CodexLateProviderEvent::Completion {
                    kind: CodexProviderItemKind::AgentMessage,
                    text: item
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| extract_codex_item_text(item)),
                    reasoning: extract_codex_item_reasoning(item),
                }),
                "reasoning" => Some(CodexLateProviderEvent::Completion {
                    kind: CodexProviderItemKind::Reasoning,
                    text: String::new(),
                    reasoning: extract_codex_item_reasoning(item)
                        .filter(|reasoning| contains_non_whitespace(reasoning)),
                }),
                _ => None,
            };
            if let Some(late_event) = late_event
                && self
                    .handle_subagent_late_provider_event(
                        stream_key,
                        &message_id,
                        late_event,
                        "item/completed",
                    )
                    .await
            {
                return;
            }
        }

        match item_type {
            "agentMessage" => {
                let Some(provider_item_id) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|item_id| !item_id.trim().is_empty())
                else {
                    self.reject_subagent_message_identity(
                        stream_key,
                        CodexProviderStreamConflict::MissingMessageId,
                        "item/completed",
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(provider_item_id.to_string());
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| extract_codex_item_text(item));
                let reasoning = extract_codex_item_reasoning(item);
                let turn_id_from_params = extract_turn_id(params);
                let Some(finalized) = self
                    .complete_subagent_message(
                        stream_key,
                        turn_id_from_params,
                        message_id.clone(),
                        model.to_string(),
                        text,
                        reasoning,
                    )
                    .await
                else {
                    return;
                };
                Self::emit_finalized_subagent_provider_item(finalized, model);
            }
            "reasoning" => {
                self.complete_subagent_reasoning_item(
                    stream_key,
                    extract_turn_id(params),
                    item.get("id")
                        .and_then(Value::as_str)
                        .filter(|item_id| !item_id.trim().is_empty())
                        .map(|item_id| ChatMessageId(item_id.to_string())),
                    model,
                    extract_codex_item_reasoning(item)
                        .filter(|reasoning| contains_non_whitespace(reasoning)),
                )
                .await;
            }
            "imageGeneration" => {
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, "generate_image")
                    .await;
                self.complete_subagent_image_generation(stream_key, &item_id, item)
                    .await;
            }
            "webSearch" | "imageView" | "sleep" => {
                let tool_name = codex_native_tool_completion(item_type)
                    .map(|(tool_name, _)| tool_name)
                    .unwrap_or(item_type);
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, tool_name)
                    .await;
                self.complete_subagent_native_tool(stream_key, &item_id, item_type)
                    .await;
            }
            "commandExecution" => {
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, "run_command")
                    .await;
                self.take_background_command(params, provider_item_id).await;
                let _ = self
                    .forget_command_execution(params, provider_item_id)
                    .await;
                self.warn_codex_raw_contract_drift_once_if_needed().await;
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let success = exit_code == 0;
                let error_message = if success {
                    None
                } else {
                    Some(format!("Command failed with exit code {exit_code}"))
                };
                emitter.tool_completed(
                    &item_id,
                    codex_tool_execution_outcome(
                        json!({
                            "kind": "RunCommand",
                            "exit_code": exit_code,
                            "stdout": output,
                            "stderr": ""
                        }),
                        success,
                        error_message,
                        None,
                    ),
                );
                self.complete_subagent_tool_calls(stream_key, std::slice::from_ref(&item_id))
                    .await;
                let pending_spawn_terminal = {
                    let mut state = self.state.lock().await;
                    if !success && let Some(stream) = state.subagent_streams.get_mut(stream_key) {
                        stream.background_work_failed = true;
                        if stream.pending_spawn_terminal_status.is_some() {
                            stream.pending_spawn_terminal_status = Some("failed".to_owned());
                        }
                    }
                    let still_running = state
                        .background_commands
                        .keys()
                        .chain(state.outstanding_command_executions.keys())
                        .any(|(thread_id, _)| thread_id == stream_key);
                    if still_running {
                        None
                    } else {
                        state
                            .subagent_streams
                            .get_mut(stream_key)
                            .and_then(|stream| stream.pending_spawn_terminal_status.take())
                    }
                };
                if let Some(status) = pending_spawn_terminal {
                    self.terminalize_codex_subagent_spawn(stream_key, &status)
                        .await;
                }
            }
            "fileChange" => {
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, "file_change")
                    .await;
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let success = item.get("status").and_then(Value::as_str) == Some("completed");
                let file_changes = parse_codex_file_changes(item);
                let err_str = if success {
                    None
                } else {
                    Some("File changes were not applied")
                };
                if file_changes.is_empty() {
                    let request_was_emitted = {
                        let state = self.state.lock().await;
                        state
                            .subagent_streams
                            .get(stream_key)
                            .is_some_and(|stream| stream.pending_tool_call_ids.contains(&item_id))
                    };
                    if !request_was_emitted {
                        // An empty fileChange never emitted a request at
                        // item/started; completing it here would fabricate a
                        // "completion without a pending request" card.
                        tracing::debug!(
                            child_thread_id = stream_key,
                            tool_call_id = item_id.as_str(),
                            "Skipping Codex child fileChange completion with no changes and no emitted request"
                        );
                        return;
                    }
                    emitter.tool_completed(
                        &item_id,
                        codex_tool_execution_outcome(
                            json!({
                                "kind": "Other",
                                "result": item
                            }),
                            success,
                            err_str.map(str::to_owned),
                            None,
                        ),
                    );
                    self.complete_subagent_tool_calls(stream_key, std::slice::from_ref(&item_id))
                        .await;
                    return;
                }
                let total = file_changes.len();
                let mut completed_call_ids = Vec::with_capacity(total);
                for (idx, change) in file_changes.iter().enumerate() {
                    let call_id = codex_file_change_call_id(&item_id, idx, total);
                    let tool_result = if success {
                        json!({
                            "kind": "ModifyFile",
                            "lines_added": change.lines_added,
                            "lines_removed": change.lines_removed
                        })
                    } else {
                        json!({
                            "kind": "Error",
                            "short_message": "File changes were not applied",
                            "detailed_message": item.to_string()
                        })
                    };
                    emitter.tool_completed(
                        &call_id,
                        codex_tool_execution_outcome(
                            tool_result,
                            success,
                            err_str.map(str::to_owned),
                            None,
                        ),
                    );
                    completed_call_ids.push(call_id);
                }
                self.complete_subagent_tool_calls(stream_key, &completed_call_ids)
                    .await;
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, tool_name)
                    .await;
                let provider_success = item.get("status").and_then(Value::as_str)
                    == Some("completed")
                    || item.get("success").and_then(Value::as_bool) == Some(true);
                let (tool_result, success, error_message) = if item_type == "mcpToolCall" {
                    let normalized = normalize_mcp_call_tool_result(item);
                    let success = normalized.success && provider_success;
                    let error = normalized
                        .error
                        .or_else(|| (!success).then(|| format!("{tool_name} failed")));
                    (normalized.tool_result, success, error)
                } else {
                    let error = (!provider_success).then(|| format!("{tool_name} failed"));
                    (
                        codex_public_generic_tool_result(tool_name, item, provider_success),
                        provider_success,
                        error,
                    )
                };
                emitter.tool_completed(
                    &item_id,
                    codex_tool_execution_outcome(tool_result, success, error_message, None),
                );
                self.complete_subagent_tool_calls(stream_key, std::slice::from_ref(&item_id))
                    .await;
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
                    return;
                };
                // The started side registered this identity under the item
                // type when no `tool` field was present; a different default
                // here would miss the pending occurrence and mint a phantom
                // occurrence-2 id.
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, tool_name)
                    .await;
                let success = codex_item_success(item);
                let error_message = if success {
                    None
                } else {
                    Some(format!("{tool_name} failed"))
                };
                let tool_result = codex_public_collaboration_result(item, success);
                emitter.tool_completed(
                    &item_id,
                    codex_tool_execution_outcome(tool_result, success, error_message, None),
                );
                self.complete_subagent_tool_calls(stream_key, std::slice::from_ref(&item_id))
                    .await;
            }
            _ => {}
        }
    }

    async fn complete_subagent_image_generation(
        &self,
        stream_key: &str,
        item_id: &str,
        item: &Value,
    ) {
        let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
            return;
        };
        let revised_prompt = codex_image_generation_prompt(item);
        let image = parse_codex_generated_image(item);
        let (success, tool_result, error, images) = match image {
            Ok(image) => (
                true,
                serde_json::to_value(protocol::ToolExecutionResult::GenerateImage {
                    revised_prompt,
                    image_count: 1,
                })
                .expect("serialize Codex image generation result"),
                None,
                vec![image],
            ),
            Err(error) => (
                false,
                json!({
                    "kind": "Error",
                    "short_message": "Image generation failed",
                    "detailed_message": error,
                }),
                Some(error),
                Vec::new(),
            ),
        };
        emitter.tool_completed(
            item_id,
            codex_tool_execution_outcome(tool_result, success, error, None),
        );
        self.complete_subagent_tool_calls_with_images(stream_key, &[item_id.to_owned()], images)
            .await;
    }

    async fn complete_subagent_native_tool(
        &self,
        stream_key: &str,
        item_id: &str,
        item_type: &str,
    ) {
        let Some((tool_name, tool_result)) = codex_native_tool_completion(item_type) else {
            return;
        };
        let Some(emitter) = self.codex_subagent_emitter(stream_key).await else {
            return;
        };
        let _ = tool_name;
        emitter.tool_completed(
            item_id,
            codex_tool_execution_outcome(tool_result, true, None, None),
        );
        self.complete_subagent_tool_calls(stream_key, &[item_id.to_owned()])
            .await;
    }

    async fn complete_subagent_message(
        &self,
        stream_key: &str,
        turn_id_from_params: Option<String>,
        message_id: ChatMessageId,
        model: String,
        completion_text: String,
        completion_reasoning: Option<String>,
    ) -> Option<FinalizedCodexSubAgentProviderItem> {
        self.close_subagent_tool_container_if_open(stream_key).await;
        let result = {
            let mut state = self.state.lock().await;
            let stream = state.subagent_streams.get_mut(stream_key)?;
            if stream.retired_unpublished_message_ids.contains(&message_id) {
                if contains_non_whitespace(&completion_text)
                    || completion_reasoning
                        .as_deref()
                        .is_some_and(contains_non_whitespace)
                {
                    Err(CodexProviderStreamConflict::ForeignActiveMessageId)
                } else {
                    tracing::debug!(
                        child_thread_id = stream_key,
                        provider_item_id = message_id.0.as_str(),
                        "Ignoring contentless completion for retired Codex child reservation"
                    );
                    return None;
                }
            } else if let Some(previous) = stream.completed_agent_messages.get(&message_id) {
                if previous.matches_replay(&completion_text, &completion_reasoning) {
                    Ok(None)
                } else {
                    Err(CodexProviderStreamConflict::ConflictingDuplicateCompletion)
                }
            } else if stream
                .current_message_id
                .as_ref()
                .is_some_and(|active_message_id| {
                    active_message_id != &message_id || stream.current_reasoning_only
                })
            {
                Err(CodexProviderStreamConflict::ForeignActiveMessageId)
            } else {
                Ok(Some(Self::finalize_subagent_provider_stream(
                    stream_key,
                    stream,
                    turn_id_from_params,
                    message_id,
                    CodexProviderItemKind::AgentMessage,
                    &model,
                    CodexProviderStreamFinalization::Completed {
                        text: completion_text,
                        reasoning: completion_reasoning,
                    },
                )))
            }
        };
        match result {
            Ok(Some(result)) if result.emitted => Some(result),
            Ok(_) => None,
            Err(violation) => {
                self.reject_subagent_message_identity(stream_key, violation, "item/completed")
                    .await;
                None
            }
        }
    }

    async fn complete_subagent_reasoning_item(
        &self,
        stream_key: &str,
        turn_id_from_params: Option<String>,
        provider_message_id: Option<ChatMessageId>,
        model: &str,
        completion_reasoning: Option<String>,
    ) {
        self.close_subagent_tool_container_if_open(stream_key).await;
        let result = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            if provider_message_id.as_ref().is_some_and(|message_id| {
                stream.retired_unpublished_message_ids.contains(message_id)
            }) {
                if completion_reasoning
                    .as_deref()
                    .is_some_and(contains_non_whitespace)
                {
                    Err(CodexProviderStreamConflict::ForeignActiveMessageId)
                } else {
                    tracing::debug!(
                        child_thread_id = stream_key,
                        provider_item_id = provider_message_id
                            .as_ref()
                            .expect("retired provider reasoning id")
                            .0
                            .as_str(),
                        "Ignoring contentless completion for retired Codex child reservation"
                    );
                    return;
                }
            } else {
                let matches_active = stream.current_reasoning_only
                    && match provider_message_id.as_ref() {
                        Some(message_id) => stream.current_message_id.as_ref() == Some(message_id),
                        None => {
                            stream
                                .current_generated_identity
                                .as_ref()
                                .is_some_and(|identity| {
                                    identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                                })
                        }
                    };
                if stream.current_message_id.is_some() && !matches_active {
                    Err(CodexProviderStreamConflict::ForeignActiveMessageId)
                } else {
                    let generated_identity = if stream.current_message_id.is_some() {
                        stream.current_generated_identity.clone()
                    } else {
                        provider_message_id.is_none().then(|| {
                            let identity = CodexProviderResponseIdentity {
                                origin: CodexProviderResponseOrigin::IdlessReasoning,
                                stream_epoch: stream.generated_identity_epoch,
                                item_ordinal: stream.next_generated_identity_ordinal,
                            };
                            stream.next_generated_identity_ordinal =
                                stream.next_generated_identity_ordinal.saturating_add(1);
                            identity
                        })
                    };
                    let message_id = stream.current_message_id.clone().unwrap_or_else(|| {
                        provider_message_id.clone().unwrap_or_else(|| {
                            generated_identity
                                .as_ref()
                                .expect("generated child reasoning identity")
                                .message_id()
                        })
                    });
                    let reported_reasoning = completion_reasoning.clone();
                    if let Some(previous) = stream.completed_agent_messages.get(&message_id) {
                        if previous.matches_replay("", &reported_reasoning) {
                            return;
                        }
                        Err(CodexProviderStreamConflict::ConflictingDuplicateCompletion)
                    } else {
                        Ok(Some(Self::finalize_subagent_provider_stream(
                            stream_key,
                            stream,
                            turn_id_from_params,
                            message_id,
                            CodexProviderItemKind::Reasoning,
                            model,
                            CodexProviderStreamFinalization::Completed {
                                text: String::new(),
                                reasoning: completion_reasoning,
                            },
                        )))
                    }
                }
            }
        };
        let finalized = match result {
            Ok(Some(result)) if result.emitted => result,
            Ok(Some(_)) => return,
            Ok(None) => return,
            Err(violation) => {
                self.reject_subagent_message_identity(stream_key, violation, "item/completed")
                    .await;
                return;
            }
        };
        Self::emit_finalized_subagent_provider_item(finalized, model);
    }

    async fn handle_subagent_token_usage_updated(
        &self,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        self.emit_subagent_model_request_usage(params, stream_key, model, false)
            .await;
        let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model)) else {
            return;
        };
        if let Some((emitter, pending, token_usage, context_breakdown)) = self
            .record_subagent_token_usage(stream_key, turn_id, token_usage)
            .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
    }

    async fn record_subagent_token_usage(
        &self,
        stream_key: &str,
        turn_id: String,
        token_usage: Value,
    ) -> Option<(
        Arc<TurnEmitter>,
        PendingCodexMessageMetadata,
        Value,
        Option<Value>,
    )> {
        let mut state = self.state.lock().await;
        let stream = state.subagent_streams.get_mut(stream_key)?;
        stream
            .token_usage_by_turn
            .insert(turn_id.clone(), token_usage.clone());
        let pending_ready = stream
            .pending_message_metadata
            .as_ref()
            .is_some_and(|pending| pending.turn_id == turn_id);
        if !pending_ready {
            return None;
        }
        let pending = stream.pending_message_metadata.take()?;
        let token_usage = stream.token_usage_by_turn.remove(&turn_id)?;
        Some((Arc::clone(&stream.emitter), pending, token_usage, None))
    }

    async fn handle_completed_subagent_token_usage(
        &self,
        params: &Value,
        stream_key: &str,
        model: &str,
    ) {
        self.emit_subagent_model_request_usage(params, stream_key, model, true)
            .await;
        let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model)) else {
            return;
        };
        if let Some((emitter, pending, token_usage, context_breakdown)) = self
            .record_completed_subagent_token_usage(stream_key, turn_id, token_usage)
            .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
    }

    async fn record_completed_subagent_token_usage(
        &self,
        stream_key: &str,
        turn_id: String,
        token_usage: Value,
    ) -> Option<(
        Arc<TurnEmitter>,
        PendingCodexMessageMetadata,
        Value,
        Option<Value>,
    )> {
        let mut state = self.state.lock().await;
        let stream = state.completed_subagent_streams.get_mut(stream_key)?;
        let pending_ready = stream
            .pending_message_metadata
            .as_ref()
            .is_some_and(|pending| pending.turn_id == turn_id);
        if !pending_ready {
            return None;
        }
        let pending = stream.pending_message_metadata.take()?;
        Some((Arc::clone(&stream.emitter), pending, token_usage, None))
    }

    async fn handle_subagent_turn_completed(&self, params: &Value, stream_key: &str, model: &str) {
        let completed_turn_id = extract_turn_id(params);
        let consumed_terminated_turn = {
            let mut state = self.state.lock().await;
            let Some(stream) = state.subagent_streams.get_mut(stream_key) else {
                return;
            };
            match completed_turn_id.as_ref() {
                Some(completed_turn_id)
                    if stream
                        .terminated_turns
                        .iter()
                        .any(|turn| turn.turn_id == *completed_turn_id) =>
                {
                    stream.token_usage_by_turn.remove(completed_turn_id);
                    stream.model_token_usage_by_turn.remove(completed_turn_id);
                    true
                }
                _ => false,
            }
        };
        if consumed_terminated_turn {
            tracing::debug!(
                child_thread_id = stream_key,
                ?completed_turn_id,
                "Consumed completion for terminated Codex child turn"
            );
            return;
        }
        self.emit_subagent_model_request_usage(params, stream_key, model, false)
            .await;
        self.close_subagent_tool_container_if_open(stream_key).await;
        let turn_status = params
            .get("turn")
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        let open_item_requires_termination = {
            let state = self.state.lock().await;
            state
                .subagent_streams
                .get(stream_key)
                .is_some_and(|stream| {
                    let matching_turn = completed_turn_id
                        .as_ref()
                        .is_none_or(|turn_id| stream.active_turn_id.as_ref() == Some(turn_id));
                    let durable_idless_reasoning = stream.current_reasoning_only
                        && stream.current_stream_published
                        && stream
                            .current_generated_identity
                            .as_ref()
                            .is_some_and(|identity| {
                                identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                            })
                        && contains_non_whitespace(&stream.current_reasoning);
                    stream.current_message_id.is_some()
                        && (!matching_turn
                            || (turn_status != "interrupted" && !durable_idless_reasoning))
                })
        };
        if open_item_requires_termination {
            self.reject_subagent_message_identity(
                stream_key,
                CodexProviderStreamConflict::MismatchedEndMessageId,
                "turn/completed",
            )
            .await;
            return;
        }
        if let Some((turn_id, token_usage)) = extract_turn_token_usage(params, Some(model))
            && let Some((emitter, pending, token_usage, context_breakdown)) = self
                .record_subagent_token_usage(stream_key, turn_id, token_usage)
                .await
        {
            emit_codex_message_metadata_update(
                emitter.as_ref(),
                pending,
                Some(token_usage),
                None,
                context_breakdown,
            );
        }
        let Some((
            emitter,
            interrupted_published_stream,
            partial_idless_reasoning,
            terminated_turn_id,
        )) = ({
            let mut state = self.state.lock().await;
            state.subagent_streams.get_mut(stream_key).map(|stream| {
                let open_item_published = stream.current_stream_published;
                let mut interrupted_published_stream = None;
                let mut partial_idless_reasoning = None;
                let mut terminated_turn_id = None;
                if extract_turn_id(params)
                    .as_ref()
                    .is_none_or(|turn_id| stream.active_turn_id.as_ref() == Some(turn_id))
                {
                    let locally_completed_turn_id = completed_turn_id
                        .clone()
                        .or_else(|| stream.active_turn_id.clone());
                    let reasoning = contains_non_whitespace(&stream.current_reasoning)
                        .then(|| stream.current_reasoning.clone());
                    if turn_status == "interrupted"
                        && open_item_published
                        && let Some(message_id) = stream.current_message_id.clone()
                    {
                        let content = stream.current_text.clone();
                        stream.completed_agent_messages.insert(
                            message_id.clone(),
                            CompletedCodexAgentMessage {
                                reported_text: content.clone(),
                                reported_reasoning: reasoning.clone(),
                                completion_text: content.clone(),
                                completion_reasoning: reasoning.clone(),
                            },
                        );
                        interrupted_published_stream = Some(InterruptedPublishedStream {
                            response: stream
                                .current_response
                                .take()
                                .expect("published child response"),
                            content,
                            reasoning,
                            images: std::mem::take(&mut stream.current_images),
                        });
                    } else {
                        let durable_idless_reasoning = stream.current_reasoning_only
                            && stream
                                .current_generated_identity
                                .as_ref()
                                .is_some_and(|identity| {
                                    identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                                })
                            && reasoning.is_some();
                        if durable_idless_reasoning
                            && open_item_published
                            && let Some(message_id) = stream.current_message_id.clone()
                        {
                            let reasoning = reasoning.expect("durable child reasoning");
                            stream.completed_agent_messages.insert(
                                message_id.clone(),
                                CompletedCodexAgentMessage {
                                    reported_text: String::new(),
                                    reported_reasoning: Some(reasoning.clone()),
                                    completion_text: String::new(),
                                    completion_reasoning: Some(reasoning.clone()),
                                },
                            );
                            partial_idless_reasoning = Some((
                                stream
                                    .current_response
                                    .take()
                                    .expect("published child reasoning response"),
                                reasoning,
                            ));
                        }
                    }
                    if turn_status == "interrupted" || partial_idless_reasoning.is_some() {
                        stream.pending_message_metadata = None;
                        if let Some(turn_id) = locally_completed_turn_id {
                            terminated_turn_id = Some(turn_id.clone());
                            push_codex_terminated_turn(
                                &mut stream.terminated_turns,
                                turn_id.clone(),
                            );
                            stream.terminated_turn_awaiting_replacement = Some(turn_id);
                        }
                    }
                    stream.active_turn_id = None;
                    stream.current_message_id = None;
                    stream.current_generated_identity = None;
                    stream.current_reasoning_only = false;
                    stream.current_stream_published = false;
                    stream.current_response = None;
                    stream.current_text.clear();
                    stream.current_reasoning.clear();
                    stream.current_tool_call_ids.clear();
                    stream.current_images.clear();
                }
                (
                    Arc::clone(&stream.emitter),
                    interrupted_published_stream,
                    partial_idless_reasoning,
                    terminated_turn_id,
                )
            })
        })
        else {
            return;
        };

        if let Some(turn_id) = terminated_turn_id {
            let commands = {
                let mut state = self.state.lock().await;
                take_codex_commands_for_turn(&mut state, stream_key, &turn_id)
            };
            for command in commands {
                emitter.cancel_pending_tool(
                    &command.tool_call_id,
                    "Codex background command was cancelled with its interrupted turn",
                );
            }
        }
        if let Some(stream) = interrupted_published_stream {
            emitter.stream_end(
                stream.response,
                StreamEndPayload {
                    content: stream.content,
                    model_info: Some(ModelInfo {
                        model: model.to_owned(),
                    }),
                    reasoning: stream.reasoning.map(reasoning_data),
                    images: stream.images,
                    ..StreamEndPayload::default()
                },
            );
            emitter.operation_cancelled("Operation cancelled");
            return;
        }

        if let Some((response, reasoning)) = partial_idless_reasoning {
            emitter.stream_end(
                response,
                StreamEndPayload {
                    model_info: Some(ModelInfo {
                        model: model.to_owned(),
                    }),
                    reasoning: Some(reasoning_data(reasoning)),
                    ..StreamEndPayload::default()
                },
            );
            emitter.operation_cancelled("Codex child turn ended before reasoning item completion");
            return;
        }

        if turn_status == "interrupted" {
            emitter.operation_cancelled("Operation cancelled");
        } else {
            emitter.typing_status_changed(false);
            if turn_status == "failed" {
                let message = params
                    .get("turn")
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex turn failed")
                    .to_string();
                emitter.backend_error(&message);
            }
        }
        tracing::debug!(
            thread_id = stream_key,
            status = turn_status,
            "Codex child turn completed; retaining ownership for possible follow-up"
        );
        let deferred_for_background_work = {
            let mut state = self.state.lock().await;
            let has_background_work = state
                .background_commands
                .keys()
                .chain(state.outstanding_command_executions.keys())
                .any(|(thread_id, _)| thread_id == stream_key);
            let terminal_status = if state
                .subagent_streams
                .get(stream_key)
                .is_some_and(|stream| stream.background_work_failed)
            {
                "failed"
            } else {
                turn_status.as_str()
            }
            .to_owned();
            eprintln!(
                "TYDE CODEX CHILD TURN TERMINAL thread_id={stream_key} provider_status={turn_status} background_work_failed={} has_background_work={has_background_work} terminal_status={terminal_status}",
                state
                    .subagent_streams
                    .get(stream_key)
                    .is_some_and(|stream| stream.background_work_failed),
            );
            if has_background_work && turn_status == "completed" {
                if let Some(stream) = state.subagent_streams.get_mut(stream_key) {
                    stream.pending_spawn_terminal_status = Some(terminal_status);
                }
                true
            } else {
                false
            }
        };
        if deferred_for_background_work {
            tracing::info!(
                child_thread_id = stream_key,
                "Deferring native spawn terminal until child background work completes"
            );
            return;
        }
        let terminal_status = {
            let state = self.state.lock().await;
            if state
                .subagent_streams
                .get(stream_key)
                .is_some_and(|stream| stream.background_work_failed)
            {
                "failed"
            } else {
                turn_status.as_str()
            }
            .to_owned()
        };
        self.terminalize_codex_subagent_spawn(stream_key, &terminal_status)
            .await;
    }

    async fn terminalize_codex_subagent_spawn(&self, stream_key: &str, turn_status: &str) {
        let terminal = {
            let mut state = self.state.lock().await;
            let terminal = state.subagent_streams.get(stream_key).map(|stream| {
                (
                    stream.spawn_item_id.clone(),
                    stream.agent_id.clone(),
                    stream.agent_name.clone(),
                    stream.current_tool_call_ids.len() as u64,
                )
            });
            if let Some((tool_call_id, ..)) = terminal.as_ref() {
                state.native_subagent_tool_call_ids.remove(tool_call_id);
            }
            terminal
        };
        if let Some((recorded_tool_call_id, agent_id, agent_name, tool_calls)) = terminal {
            let tool_call_id = if self
                .emitter
                .has_pending_tool_request(&recorded_tool_call_id)
            {
                recorded_tool_call_id.clone()
            } else {
                format!("codex-native-spawn:{recorded_tool_call_id}")
            };
            eprintln!(
                "TYDE CODEX CHILD SPAWN TERMINALIZE stream_key={stream_key} recorded_tool_call_id={recorded_tool_call_id} resolved_tool_call_id={tool_call_id} pending={} status={turn_status}",
                self.emitter.has_pending_tool_request(&tool_call_id),
            );
            if !self.emitter.has_pending_tool_request(&tool_call_id) {
                return;
            }
            tracing::info!(
                child_thread_id = stream_key,
                tool_call_id,
                status = turn_status,
                "Terminalizing Codex native background spawn"
            );
            let success = turn_status == "completed";
            let cancelled = matches!(
                turn_status,
                "interrupted" | "cancelled" | "canceled" | "stopped"
            );
            let status = if success {
                protocol::SubAgentProgressStatus::Completed
            } else if cancelled {
                protocol::SubAgentProgressStatus::Stopped
            } else {
                protocol::SubAgentProgressStatus::Failed
            };
            self.emitter.tool_progress(&ToolProgressData {
                tool_call_id: tool_call_id.clone(),
                execution_mode: ToolExecutionMode::Background,
                cancellable: false,
                update: ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
                    agent_id,
                    agent_name,
                    last_tool_name: None,
                    tool_calls,
                    completed: true,
                    status,
                }),
            });
            if cancelled {
                self.emitter.cancel_pending_tool(
                    &tool_call_id,
                    "Codex child was cancelled before it completed",
                );
                self.mark_tool_completed(&tool_call_id).await;
            } else {
                let tool_name = self
                    .emitter
                    .tool_request_name(&tool_call_id)
                    .unwrap_or_else(|| "spawnAgent".to_owned());
                self.emit_tool_execution_completed(
                    &tool_call_id,
                    &tool_name,
                    success,
                    json!({ "kind": "Other", "result": { "status": turn_status } }),
                    (!success)
                        .then(|| format!("Codex child turn ended with status '{turn_status}'")),
                )
                .await;
            }
        }
    }

    async fn emit_subagent_model_request_usage(
        &self,
        params: &Value,
        stream_key: &str,
        model: &str,
        completed: bool,
    ) {
        let Some((turn_id, request, cumulative, context_window)) =
            extract_model_request_token_usage(params, Some(model))
        else {
            return;
        };
        let recorded = {
            let mut state = self.state.lock().await;
            if completed {
                state
                    .completed_subagent_streams
                    .get_mut(stream_key)
                    .and_then(|stream| {
                        let cumulative = normalize_subagent_cumulative_usage(
                            &mut stream.provider_usage_baseline,
                            &request,
                            cumulative,
                        );
                        let usage = record_model_request_token_usage(
                            &mut stream.model_token_usage_by_turn,
                            turn_id,
                            request,
                            cumulative,
                            context_window,
                        )?;
                        Some((Arc::clone(&stream.emitter), usage))
                    })
            } else {
                state
                    .subagent_streams
                    .get_mut(stream_key)
                    .and_then(|stream| {
                        let cumulative = normalize_subagent_cumulative_usage(
                            &mut stream.provider_usage_baseline,
                            &request,
                            cumulative,
                        );
                        let usage = record_model_request_token_usage(
                            &mut stream.model_token_usage_by_turn,
                            turn_id,
                            request,
                            cumulative,
                            context_window,
                        )?;
                        Some((Arc::clone(&stream.emitter), usage))
                    })
            }
        };
        if let Some((emitter, usage)) = recorded {
            emitter.model_request_token_usage(&usage);
        }
    }

    async fn handle_legacy_codex_event(&self, method: &str, params: &Value) {
        if let Some(retry) = extract_legacy_codex_retry_attempt(method, params) {
            self.emitter.retry_attempt(retry);
            return;
        }
        let Some(delta) = extract_reasoning_delta_from_legacy_codex_event(method, params) else {
            return;
        };
        self.emit_reasoning_delta(delta).await;
    }

    async fn emit_reasoning_delta(&self, delta: String) {
        match self
            .open_reasoning_message_item(None, CodexProviderOpenCause::Delta, None, None)
            .await
        {
            CodexAgentMessageOpen::Open => {}
            CodexAgentMessageOpen::Existing => {}
            CodexAgentMessageOpen::Retired => return,
            CodexAgentMessageOpen::Terminal => {
                self.reject_agent_message_identity(
                    CodexProviderStreamConflict::DuplicateTerminalMessageId,
                    "codex/event/reasoning",
                    None,
                )
                .await;
                return;
            }
            CodexAgentMessageOpen::Foreign => {
                self.reject_agent_message_identity(
                    CodexProviderStreamConflict::ForeignActiveMessageId,
                    "codex/event/reasoning",
                    None,
                )
                .await;
                return;
            }
            CodexAgentMessageOpen::Superseded(previous) => {
                self.reject_impossible_delta_supersession(*previous, "codex/event/reasoning", None)
                    .await;
                return;
            }
        }
        self.append_reasoning_to_active_stream(&delta).await;
    }

    async fn handle_error_notification(&self, params: &Value) {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("Codex error")
            .to_string();
        if let Some(tool_call_id) = codex_error_tool_call_id(params)
            && self.emitter.fail_pending_tool(&tool_call_id, &message)
        {
            self.mark_tool_completed(&tool_call_id).await;
            return;
        }
        let terminal = {
            let state = self.state.lock().await;
            is_terminal_codex_error_notification(&state, params)
        };
        if terminal {
            self.complete_all_codex_subagents().await;
            self.emitter.backend_error(&message);
            self.emitter.typing_status_changed(false);
            return;
        }

        self.emitter
            .subprocess_stderr(&format!("Codex warning: {message}"));
    }

    async fn handle_server_request(self: &Arc<Self>, id: Value, method: &str, params: &Value) {
        let inference_only =
            self.state.lock().await.execution_mode == BackendExecutionMode::InferenceOnly;
        if inference_only && is_codex_tool_server_request(method) {
            let response = match method {
                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                    json!({ "decision": "decline" })
                }
                "execCommandApproval" | "applyPatchApproval" => {
                    json!({ "decision": "denied" })
                }
                "mcpServer/elicitation/request" => json!({ "action": "cancel" }),
                "item/tool/requestUserInput" => json!({ "answers": {} }),
                "item/tool/call" => json!({
                    "success": false,
                    "contentItems": [{
                        "type": "inputText",
                        "text": "Transient inference does not permit tools."
                    }]
                }),
                _ => json!({ "decision": "decline" }),
            };
            if let Err(err) = self.rpc.respond(id, response).await {
                self.emitter.backend_error(&format!(
                    "Codex transient inference failed to reject tool request '{method}': {err}"
                ));
            } else {
                self.emitter.backend_error(&format!(
                    "Codex transient inference rejected tool request '{method}'"
                ));
            }
            self.emitter.typing_status_changed(false);
            return;
        }

        match method {
            "item/commandExecution/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("approval")
                    .to_string();
                let tool_call_id =
                    format!("approval-{}", codex_scoped_tool_call_id(params, &item_id));
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        params
                            .get("command")
                            .and_then(Value::as_str)
                            .map(|cmd| format!("Approve command: {cmd}"))
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::CommandApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    CodexToolRequest::other(json!({
                        "question": question,
                        "type": "command_approval"
                    })),
                )
                .await;
            }
            "item/fileChange/requestApproval" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("file-approval")
                    .to_string();
                let tool_call_id = format!(
                    "file-approval-{}",
                    codex_scoped_tool_call_id(params, &item_id)
                );
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::FileChangeApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    CodexToolRequest::other(json!({
                        "question": question,
                        "type": "file_change_approval"
                    })),
                )
                .await;
            }
            "execCommandApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("exec-approval")
                    .to_string();
                let tool_call_id = format!(
                    "exec-approval-{}",
                    codex_scoped_tool_call_id(params, &call_id)
                );
                let command_text = params
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
                    .or_else(|| {
                        if command_text.is_empty() {
                            None
                        } else {
                            Some(format!("Approve command: {command_text}"))
                        }
                    })
                    .unwrap_or_else(|| "Approve pending command?".to_string());

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ExecCommandApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    CodexToolRequest::other(json!({
                        "question": question,
                        "type": "command_approval"
                    })),
                )
                .await;
            }
            "applyPatchApproval" => {
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("patch-approval")
                    .to_string();
                let tool_call_id = format!(
                    "patch-approval-{}",
                    codex_scoped_tool_call_id(params, &call_id)
                );
                let question = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Approve pending file changes?")
                    .to_string();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::ApplyPatchApproval,
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    CodexToolRequest::other(json!({
                        "question": question,
                        "type": "file_change_approval"
                    })),
                )
                .await;
            }
            "item/tool/requestUserInput" => {
                let item_id = params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .unwrap_or("request-user-input")
                    .to_string();
                let tool_call_id = format!(
                    "request-user-input-{}",
                    codex_scoped_tool_call_id(params, &item_id)
                );
                let questions = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let question_ids = questions
                    .iter()
                    .filter_map(|q| q.get("id").and_then(Value::as_str).map(|s| s.to_string()))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state.pending_request = Some(PendingRequest {
                        request_id: id,
                        tool_call_id: tool_call_id.clone(),
                        kind: PendingRequestKind::UserInput {
                            questions: question_ids,
                        },
                    });
                }

                self.emitter.typing_status_changed(false);
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    "ask_user_question",
                    CodexToolRequest::other(json!({
                        "questions": questions,
                        "type": "request_user_input"
                    })),
                )
                .await;
            }
            "mcpServer/elicitation/request" => {
                let result = codex_mcp_elicitation_result(params);
                if let Err(err) = self.rpc.respond(id, result).await {
                    self.emitter.subprocess_stderr(&format!(
                        "Failed to resolve Codex MCP elicitation request: {err}"
                    ));
                }
            }
            "item/tool/call" => {
                let tool_name = params
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("dynamic_tool");
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .map(|call_id| codex_scoped_tool_call_id(params, call_id))
                    .unwrap_or_else(|| codex_scoped_tool_call_id(params, "dynamic-tool-call"));
                tracing::info!(
                    tool_call_id = call_id,
                    tool_name,
                    "Rejecting unsupported Codex dynamic client tool request"
                );
                let response_payload = json!({
                    "success": false,
                    "contentItems": [
                        {
                            "type": "inputText",
                            "text": "Dynamic client tool calls are not yet supported in Tyde."
                        }
                    ]
                });
                let _ = self.rpc.respond(id, response_payload).await;
            }
            _ => {
                let _ = self
                    .rpc
                    .respond(
                        id,
                        json!({"ignored": true, "reason": "unsupported_server_request"}),
                    )
                    .await;
            }
        }
    }

    async fn handle_item_started(self: &Arc<Self>, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item.get("id").and_then(Value::as_str);
        let notification_thread_id = extract_notification_thread_id(params);
        let notification_turn_id = extract_turn_id(params);
        self.state.lock().await.foreground_response_completed = false;
        let (strict_response, provider_tool) =
            self.observe_strict_response_item_started(params).await;
        if strict_response && matches!(item_type, "agentMessage" | "reasoning") {
            // The response beginning while a command is still running is what
            // makes that command a background one, and it is true whether or
            // not the splitter owns this message. The `agentMessage` arm below
            // is unreachable for a strict response, so promoting there alone
            // left a still-running command marked foreground until the turn
            // went idle underneath it.
            if item_type == "agentMessage" {
                self.promote_root_commands_before_agent_response(params)
                    .await;
            }
            return;
        }
        let _ = provider_tool;

        match item_type {
            "agentMessage" => {
                self.promote_root_commands_before_agent_response(params)
                    .await;
                let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) else {
                    self.reject_agent_message_identity(
                        CodexProviderStreamConflict::MissingMessageId,
                        "item/started",
                        None,
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(item_id.to_string());
                if self
                    .handle_root_late_provider_event(
                        &message_id,
                        CodexLateProviderEvent::Started {
                            kind: CodexProviderItemKind::AgentMessage,
                        },
                        "item/started",
                    )
                    .await
                {
                    return;
                }
                match self
                    .open_agent_message_item(
                        message_id.clone(),
                        CodexProviderOpenCause::ItemStarted,
                        notification_thread_id.as_deref(),
                        notification_turn_id.as_deref(),
                    )
                    .await
                {
                    CodexAgentMessageOpen::Open => {}
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Retired => {}
                    CodexAgentMessageOpen::Superseded(previous) => {
                        self.finalize_root_provider_supersession(
                            *previous,
                            &message_id,
                            CodexProviderItemKind::AgentMessage,
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::DuplicateTerminalMessageId,
                            "item/started",
                            Some(&message_id.0),
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            "item/started",
                            Some(&message_id.0),
                        )
                        .await;
                    }
                }
            }
            "reasoning" => {
                let provider_message_id = item_id
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                if let Some(message_id) = provider_message_id.as_ref()
                    && self
                        .handle_root_late_provider_event(
                            message_id,
                            CodexLateProviderEvent::Started {
                                kind: CodexProviderItemKind::Reasoning,
                            },
                            "item/started",
                        )
                        .await
                {
                    return;
                }
                match self
                    .open_reasoning_message_item(
                        provider_message_id.clone(),
                        CodexProviderOpenCause::ItemStarted,
                        notification_thread_id.as_deref(),
                        notification_turn_id.as_deref(),
                    )
                    .await
                {
                    CodexAgentMessageOpen::Open => {}
                    CodexAgentMessageOpen::Existing => {}
                    CodexAgentMessageOpen::Retired => {}
                    CodexAgentMessageOpen::Superseded(previous) => {
                        self.finalize_root_provider_supersession(
                            *previous,
                            provider_message_id
                                .as_ref()
                                .expect("superseded reasoning has provider id"),
                            CodexProviderItemKind::Reasoning,
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Terminal => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::DuplicateTerminalMessageId,
                            "item/started",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                    }
                    CodexAgentMessageOpen::Foreign => {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            "item/started",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                    }
                }
            }
            "imageGeneration" => {
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), "generate_image")
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                let prompt = item
                    .get("revisedPrompt")
                    .and_then(Value::as_str)
                    .filter(|prompt| !prompt.trim().is_empty())
                    .map(str::to_owned);
                self.emit_tool_request(
                    &item_id,
                    "generate_image",
                    CodexToolRequest::from_item(
                        item,
                        serde_json::to_value(protocol::ToolRequestType::GenerateImage { prompt })
                            .expect("serialize Codex image generation request"),
                    ),
                )
                .await;
            }
            "webSearch" => {
                tracing::debug!(?item, "Codex started a native webSearch item");
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), "web_search")
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                let query = item
                    .get("query")
                    .and_then(Value::as_str)
                    .filter(|query| !query.trim().is_empty())
                    .map(str::to_owned);
                let query = match query {
                    Some(query) => query,
                    None => {
                        // Code-mode emits an empty typed webSearch item after a
                        // raw web__run request whose JavaScript source owns the query.
                        let state = self.state.lock().await;
                        notification_thread_id
                            .as_deref()
                            .and_then(|thread_id| state.response_splitters.get(thread_id))
                            .and_then(CodexResponseSplitter::suppressed_web_search_query)
                            .unwrap_or_default()
                    }
                };
                self.emit_tool_request(
                    &item_id,
                    "web_search",
                    CodexToolRequest::from_item(
                        item,
                        serde_json::to_value(protocol::ToolRequestType::WebSearch { query })
                            .expect("serialize Codex web search request"),
                    ),
                )
                .await;
            }
            "imageView" => {
                tracing::debug!(?item, "Codex started a native imageView item");
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), "view_image")
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                let path = item
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.emit_tool_request(
                    &item_id,
                    "view_image",
                    CodexToolRequest::from_item(
                        item,
                        serde_json::to_value(protocol::ToolRequestType::ViewImage { path })
                            .expect("serialize Codex image view request"),
                    ),
                )
                .await;
            }
            "sleep" => {
                tracing::debug!(?item, "Codex started a native sleep item");
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), "sleep")
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                let duration_ms = item.get("durationMs").and_then(Value::as_u64).unwrap_or(0);
                self.emit_tool_request(
                    &item_id,
                    "sleep",
                    CodexToolRequest::from_item(
                        item,
                        serde_json::to_value(protocol::ToolRequestType::Sleep { duration_ms })
                            .expect("serialize Codex sleep request"),
                    ),
                )
                .await;
            }
            "commandExecution" => {
                let provider_item_id = item_id.unwrap_or("tool-call");
                let item_id = self
                    .tool_call_started_id(params, provider_item_id, "run_command")
                    .await;
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cwd = item
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                self.emit_tool_request(
                    &item_id,
                    "run_command",
                    CodexToolRequest::from_item(
                        item,
                        json!({
                            "kind": "RunCommand",
                            "command": command,
                            "working_directory": cwd
                        }),
                    ),
                )
                .await;
                self.track_command_execution(params, provider_item_id, &item_id, item)
                    .await;
            }
            "fileChange" => {
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), "file_change")
                    .await;
                let file_changes = parse_codex_file_changes(item);
                if file_changes.is_empty() {
                    return;
                }

                let total = file_changes.len();
                let call_ids = file_changes
                    .iter()
                    .enumerate()
                    .map(|(idx, _)| codex_file_change_call_id(&item_id, idx, total))
                    .collect::<Vec<_>>();

                {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .insert(item_id.clone(), call_ids.clone());
                }

                self.track_tool_requests(call_ids.clone()).await;
                for (change, call_id) in file_changes.into_iter().zip(call_ids) {
                    self.emit_modify_file_request(
                        &call_id,
                        &change.path,
                        &change.before,
                        &change.after,
                    )
                    .await;
                }
            }
            "collabToolCall" | "collabAgentToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("collab_tool")
                    .to_string();
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), &tool_name)
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                self.emit_tool_request(
                    &item_id,
                    &tool_name,
                    codex_public_tool_request(&tool_name, item),
                )
                .await;
                if is_tyde_agent_control_spawn_tool_name(&tool_name)
                    || is_tyde_agent_control_await_tool_name(&tool_name)
                {
                    tracing::info!(
                        tool_call_id = item_id,
                        tool_name,
                        "Emitted Codex Tyde agent-control request"
                    );
                }
                self.emit_agent_control_await_progress_if_needed(
                    &item_id,
                    &tool_name,
                    item,
                    protocol::AgentControlProgressStatus::Running,
                )
                .await;
                self.record_codex_subagent_spawn_metadata_if_needed(
                    Some(&item_id),
                    Some(params),
                    item,
                )
                .await;
            }
            "subAgentActivity" | "sub_agent_activity" => {
                self.register_codex_subagent_activity_if_needed(item).await;
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type)
                    .to_string();
                let item_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), &tool_name)
                    .await;
                self.track_tool_requests(std::iter::once(item_id.clone()))
                    .await;
                self.emit_tool_request(
                    &item_id,
                    &tool_name,
                    codex_public_tool_request(&tool_name, item),
                )
                .await;
                if is_tyde_agent_control_spawn_tool_name(&tool_name)
                    || is_tyde_agent_control_await_tool_name(&tool_name)
                {
                    tracing::info!(
                        tool_call_id = item_id,
                        tool_name,
                        "Emitted Codex Tyde agent-control request"
                    );
                }
                self.emit_agent_control_await_progress_if_needed(
                    &item_id,
                    &tool_name,
                    item,
                    protocol::AgentControlProgressStatus::Running,
                )
                .await;
            }
            // A tool item this build has no mapping for. It still ran, so it
            // still gets a card, carrying the provider's own JSON: the
            // alternative is a tool the user never sees, and a card left open
            // for the idle sweep to cancel.
            unmapped if is_codex_provider_tool_item_type(unmapped) => {
                let tool_call_id = self
                    .tool_call_started_id(params, item_id.unwrap_or("tool-call"), unmapped)
                    .await;
                self.track_tool_requests(std::iter::once(tool_call_id.clone()))
                    .await;
                self.emit_tool_request(
                    &tool_call_id,
                    unmapped,
                    CodexToolRequest::other(json!(item)),
                )
                .await;
            }
            _ => {}
        }
    }

    async fn handle_item_completed(self: &Arc<Self>, params: &Value) {
        let Some(item) = params.get("item") else {
            return;
        };

        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let item_id = item.get("id").and_then(Value::as_str);
        // Clears the splitter's per-item bookkeeping; the card itself is
        // completed by the typed handler below, which owns it.
        let _ = self.finish_strict_typed_tool(params).await;
        if self.handle_strict_response_item_completed(params).await {
            return;
        }

        match item_type {
            "agentMessage" => {
                let Some(item_id) = item_id.filter(|item_id| !item_id.trim().is_empty()) else {
                    self.reject_agent_message_identity(
                        CodexProviderStreamConflict::MissingMessageId,
                        "item/completed",
                        None,
                    )
                    .await;
                    return;
                };
                let message_id = ChatMessageId(item_id.to_string());
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| extract_codex_item_text(item));
                let completion_reasoning = extract_codex_item_reasoning(item);
                if self
                    .handle_root_late_provider_event(
                        &message_id,
                        CodexLateProviderEvent::Completion {
                            kind: CodexProviderItemKind::AgentMessage,
                            text: text.clone(),
                            reasoning: completion_reasoning.clone(),
                        },
                        "item/completed",
                    )
                    .await
                {
                    return;
                }
                self.close_tool_container_if_open().await;
                if self
                    .state
                    .lock()
                    .await
                    .retired_unpublished_message_ids
                    .contains(&message_id)
                {
                    if contains_non_whitespace(&text)
                        || completion_reasoning
                            .as_deref()
                            .is_some_and(contains_non_whitespace)
                    {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            "item/completed",
                            Some(&message_id.0),
                        )
                        .await;
                    } else {
                        tracing::debug!(
                            provider_item_id = message_id.0.as_str(),
                            "Ignoring contentless completion for retired Codex reservation"
                        );
                    }
                    return;
                }
                let result = {
                    let mut state = self.state.lock().await;
                    if let Some(previous) = state.completed_agent_messages.get(&message_id) {
                        if previous.matches_replay(&text, &completion_reasoning) {
                            None
                        } else {
                            Some(Err(
                                CodexProviderStreamConflict::ConflictingDuplicateCompletion,
                            ))
                        }
                    } else if let Some(active) = state.active_stream.as_ref() {
                        if active.message_id != message_id || active.reasoning_only {
                            Some(Err(CodexProviderStreamConflict::ForeignActiveMessageId))
                        } else {
                            let stream = state
                                .active_stream
                                .take()
                                .expect("active Codex stream disappeared while completing item");
                            Some(Ok((stream, false)))
                        }
                    } else {
                        let images = std::mem::take(&mut state.tool_container_images);
                        Some(Ok((
                            ActiveStreamState {
                                turn_id: state
                                    .active_turn_id
                                    .clone()
                                    .unwrap_or_else(|| "turn".to_string()),
                                message_id: message_id.clone(),
                                generated_identity: None,
                                text: String::new(),
                                reasoning: String::new(),
                                reasoning_only: false,
                                stream_published: false,
                                images,
                            },
                            true,
                        )))
                    }
                };
                let Some(result) = result else {
                    tracing::debug!(
                        provider_item_id = message_id.0.as_str(),
                        "Ignoring idempotent duplicate Codex agentMessage completion"
                    );
                    return;
                };
                let (stream, _) = match result {
                    Ok(stream) => stream,
                    Err(violation) => {
                        self.reject_agent_message_identity(
                            violation,
                            "item/completed",
                            Some(&message_id.0),
                        )
                        .await;
                        return;
                    }
                };
                self.finalize_root_provider_stream(
                    stream,
                    CodexProviderStreamFinalization::Completed {
                        text,
                        reasoning: completion_reasoning,
                    },
                )
                .await;
            }
            "subAgentActivity" | "sub_agent_activity" => {
                self.register_codex_subagent_activity_if_needed(item).await;
            }
            "userMessage" => {
                // User messages are emitted synchronously when sent to keep ordering stable.
            }
            "reasoning" => {
                let completion_reasoning = extract_codex_item_reasoning(item)
                    .filter(|reasoning| contains_non_whitespace(reasoning));
                let provider_message_id = item_id
                    .filter(|item_id| !item_id.trim().is_empty())
                    .map(|item_id| ChatMessageId(item_id.to_string()));
                if let Some(message_id) = provider_message_id.as_ref()
                    && self
                        .handle_root_late_provider_event(
                            message_id,
                            CodexLateProviderEvent::Completion {
                                kind: CodexProviderItemKind::Reasoning,
                                text: String::new(),
                                reasoning: completion_reasoning.clone(),
                            },
                            "item/completed",
                        )
                        .await
                {
                    return;
                }
                self.close_tool_container_if_open().await;
                if let Some(message_id) = provider_message_id.as_ref()
                    && self
                        .state
                        .lock()
                        .await
                        .retired_unpublished_message_ids
                        .contains(message_id)
                {
                    if completion_reasoning.is_some() {
                        self.reject_agent_message_identity(
                            CodexProviderStreamConflict::ForeignActiveMessageId,
                            "item/completed",
                            Some(&message_id.0),
                        )
                        .await;
                    } else {
                        tracing::debug!(
                            provider_item_id = message_id.0.as_str(),
                            "Ignoring contentless completion for retired Codex reservation"
                        );
                    }
                    return;
                }
                let result = {
                    let mut state = self.state.lock().await;
                    let matches_active = |stream: &ActiveStreamState| {
                        if !stream.reasoning_only {
                            return false;
                        }
                        match provider_message_id.as_ref() {
                            Some(message_id) => stream.message_id == *message_id,
                            None => stream.generated_identity.as_ref().is_some_and(|identity| {
                                identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                            }),
                        }
                    };
                    if let Some(message_id) = provider_message_id.as_ref()
                        && let Some(previous) = state.completed_agent_messages.get(message_id)
                    {
                        if previous.matches_replay("", &completion_reasoning) {
                            None
                        } else {
                            Some(Err(
                                CodexProviderStreamConflict::ConflictingDuplicateCompletion,
                            ))
                        }
                    } else if let Some(active) = state.active_stream.as_ref() {
                        if matches_active(active) {
                            let stream = state
                                .active_stream
                                .take()
                                .expect("active Codex reasoning stream disappeared");
                            Some(Ok((stream, false)))
                        } else {
                            Some(Err(CodexProviderStreamConflict::ForeignActiveMessageId))
                        }
                    } else {
                        let generated_identity = provider_message_id.is_none().then(|| {
                            let identity = CodexProviderResponseIdentity {
                                origin: CodexProviderResponseOrigin::IdlessReasoning,
                                stream_epoch: state.generated_identity_epoch,
                                item_ordinal: state.next_generated_identity_ordinal,
                            };
                            state.next_generated_identity_ordinal =
                                state.next_generated_identity_ordinal.saturating_add(1);
                            identity
                        });
                        let message_id = provider_message_id.clone().unwrap_or_else(|| {
                            generated_identity
                                .as_ref()
                                .expect("generated reasoning identity")
                                .message_id()
                        });
                        let images = std::mem::take(&mut state.tool_container_images);
                        Some(Ok((
                            ActiveStreamState {
                                turn_id: state
                                    .active_turn_id
                                    .clone()
                                    .unwrap_or_else(|| "turn".to_string()),
                                message_id,
                                generated_identity,
                                text: String::new(),
                                reasoning: String::new(),
                                reasoning_only: true,
                                stream_published: false,
                                images,
                            },
                            true,
                        )))
                    }
                };
                let Some(result) = result else {
                    return;
                };
                let (stream, _) = match result {
                    Ok(result) => result,
                    Err(violation) => {
                        self.reject_agent_message_identity(
                            violation,
                            "item/completed",
                            provider_message_id
                                .as_ref()
                                .map(|message_id| message_id.0.as_str()),
                        )
                        .await;
                        return;
                    }
                };
                self.finalize_root_provider_stream(
                    stream,
                    CodexProviderStreamFinalization::Completed {
                        text: String::new(),
                        reasoning: completion_reasoning,
                    },
                )
                .await;
            }
            "imageGeneration" => {
                let item_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), "generate_image")
                    .await;
                self.complete_image_generation(&item_id, item).await;
            }
            "webSearch" | "imageView" | "sleep" => {
                let tool_name = codex_native_tool_completion(item_type)
                    .map(|(tool_name, _)| tool_name)
                    .unwrap_or(item_type);
                let item_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), tool_name)
                    .await;
                self.complete_native_tool(&item_id, item_type).await;
            }
            "commandExecution" => {
                let provider_item_id = item_id.unwrap_or("item");
                let item_id = self
                    .tool_call_completed_id(params, provider_item_id, "run_command")
                    .await;
                let background_command =
                    self.take_background_command(params, provider_item_id).await;
                let _ = self
                    .forget_command_execution(params, provider_item_id)
                    .await;
                self.warn_codex_raw_contract_drift_once_if_needed().await;
                let exit_code = item.get("exitCode").and_then(Value::as_i64).unwrap_or(-1) as i32;
                let output = item
                    .get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let success = exit_code == 0;
                self.emit_tool_execution_completed(
                    &item_id,
                    "run_command",
                    success,
                    json!({
                        "kind": "RunCommand",
                        "exit_code": exit_code,
                        "stdout": output.clone(),
                        "stderr": ""
                    }),
                    if success {
                        None
                    } else {
                        Some(format!("Command failed with exit code {exit_code}"))
                    },
                )
                .await;
                if let Some(command) = background_command {
                    self.enqueue_background_wake(command, exit_code, output)
                        .await;
                }
            }
            "fileChange" => {
                let item_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), "file_change")
                    .await;
                let success = item.get("status").and_then(Value::as_str) == Some("completed");
                let known_call_ids = {
                    let mut state = self.state.lock().await;
                    state
                        .file_change_call_ids
                        .remove(&item_id)
                        .unwrap_or_default()
                };
                let file_changes = parse_codex_file_changes(item);
                if let (Some(thread_id), Some(turn_id)) = (
                    extract_notification_thread_id(params),
                    extract_turn_id(params),
                ) {
                    let mut state = self.state.lock().await;
                    for change in &file_changes {
                        let key = state
                            .pending_raw_modify_calls
                            .iter()
                            .find(|((owner_thread_id, _), pending)| {
                                owner_thread_id == &thread_id
                                    && pending.turn_id == turn_id
                                    && pending.before == change.before
                                    && pending.after == change.after
                                    && codex_modify_paths_match(&pending.file_path, &change.path)
                            })
                            .map(|(key, _)| key.clone());
                        if let Some(key) = key {
                            state.pending_raw_modify_calls.remove(&key);
                        }
                    }
                }
                let completions =
                    codex_file_change_completion_plan(&item_id, &known_call_ids, &file_changes);

                if !completions.is_empty() {
                    for completion in completions {
                        if let Some(change) = completion.request.as_ref() {
                            self.emit_modify_file_request(
                                &completion.call_id,
                                &change.path,
                                &change.before,
                                &change.after,
                            )
                            .await;
                        }

                        let tool_result = if success {
                            json!({
                                "kind": "ModifyFile",
                                "lines_added": completion.lines_added,
                                "lines_removed": completion.lines_removed
                            })
                        } else {
                            json!({
                                "kind": "Error",
                                "short_message": "File changes were not applied",
                                "detailed_message": item.to_string()
                            })
                        };
                        self.emit_tool_execution_completed(
                            &completion.call_id,
                            "modify_file",
                            success,
                            tool_result,
                            if success {
                                None
                            } else {
                                Some("File changes were not applied".to_string())
                            },
                        )
                        .await;
                    }
                    return;
                }

                let request_was_emitted = {
                    let state = self.state.lock().await;
                    state.pending_tool_call_ids.contains(&item_id)
                };
                if !request_was_emitted {
                    // An empty fileChange never emitted a request at
                    // item/started; completing it here would fabricate a
                    // "completion without a pending request" card.
                    tracing::debug!(
                        tool_call_id = item_id.as_str(),
                        "Skipping Codex fileChange completion with no changes and no emitted request"
                    );
                    return;
                }
                self.emit_tool_execution_completed(
                    &item_id,
                    "file_change",
                    success,
                    json!({
                        "kind": "Other",
                        "result": item
                    }),
                    if success {
                        None
                    } else {
                        Some("File changes were not applied".to_string())
                    },
                )
                .await;
            }
            "mcpToolCall" | "dynamicToolCall" => {
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or(item_type);
                let item_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), tool_name)
                    .await;
                let provider_success = item.get("status").and_then(Value::as_str)
                    == Some("completed")
                    || item.get("success").and_then(Value::as_bool) == Some(true);
                let normalized_mcp =
                    (item_type == "mcpToolCall").then(|| normalize_mcp_call_tool_result(item));
                let result = if let Some(normalized) = normalized_mcp.as_ref() {
                    normalized.tool_result.clone()
                } else {
                    json!({
                        "kind": "Other",
                        "result": item
                    })
                };
                let success = normalized_mcp
                    .as_ref()
                    .map(|normalized| normalized.success && provider_success)
                    .unwrap_or(provider_success);
                let error = normalized_mcp
                    .and_then(|normalized| normalized.error)
                    .or_else(|| (!success).then(|| format!("{tool_name} failed")));
                if item_type == "dynamicToolCall" {
                    tracing::info!(
                        tool_call_id = item_id,
                        tool_name,
                        provider_success,
                        "Completing Codex dynamic tool from its provider item"
                    );
                }
                self.emit_agent_control_await_progress_if_needed(
                    &item_id,
                    tool_name,
                    item,
                    if success {
                        protocol::AgentControlProgressStatus::Completed
                    } else {
                        protocol::AgentControlProgressStatus::Failed
                    },
                )
                .await;
                self.emit_tool_execution_completed(&item_id, tool_name, success, result, error)
                    .await;
            }
            "collabToolCall" | "collabAgentToolCall" => {
                tracing::debug!(
                    item_id = item_id,
                    tool = item.get("tool").and_then(|value| value.as_str()),
                    status = item.get("status").and_then(|value| value.as_str()),
                    receiver_count = codex_native_wait_thread_ids(item).len(),
                    "Codex native collaboration completion"
                );
                tracing::info!(item = %item, "Codex native collaboration completion payload");
                let tool_name = item
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("collab_tool");
                let item_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), tool_name)
                    .await;
                let success = codex_item_success(item);
                let native_spawn = !parse_codex_subagent_collabs(item).is_empty();
                if !native_spawn {
                    self.emit_agent_control_await_progress_if_needed(
                        &item_id,
                        tool_name,
                        item,
                        if success {
                            protocol::AgentControlProgressStatus::Completed
                        } else {
                            protocol::AgentControlProgressStatus::Failed
                        },
                    )
                    .await;
                    self.emit_tool_execution_completed(
                        &item_id,
                        tool_name,
                        success,
                        json!({
                            "kind": "Other",
                            "result": item
                        }),
                        if success {
                            None
                        } else {
                            Some(format!("{tool_name} failed"))
                        },
                    )
                    .await;
                }
                self.record_codex_subagent_spawn_metadata_if_needed(
                    Some(&item_id),
                    Some(params),
                    item,
                )
                .await;
            }
            // Completes the card the unmapped arm above opened. `status` is the
            // one field every typed item carries, so it decides the outcome.
            unmapped if is_codex_provider_tool_item_type(unmapped) => {
                let tool_call_id = self
                    .tool_call_completed_id(params, item_id.unwrap_or("item"), unmapped)
                    .await;
                let failed = item.get("status").and_then(Value::as_str) == Some("failed");
                self.emit_tool_execution_completed(
                    &tool_call_id,
                    unmapped,
                    !failed,
                    if failed {
                        json!({
                            "kind": "Error",
                            "short_message": format!("{unmapped} failed"),
                            "detailed_message": item.to_string(),
                        })
                    } else {
                        json!({ "kind": "Other", "result": item })
                    },
                    failed.then(|| format!("{unmapped} failed")),
                )
                .await;
            }
            _ => {}
        }
    }

    async fn record_codex_subagent_spawn_metadata_if_needed(
        &self,
        canonical_item_id: Option<&str>,
        params: Option<&Value>,
        item: &Value,
    ) {
        let spawns = parse_codex_subagent_collabs(item);
        if spawns.is_empty() {
            return;
        }
        let register_from_completed_spawn = item
            .get("tool")
            .and_then(Value::as_str)
            .is_some_and(|tool| tool.eq_ignore_ascii_case("spawnAgent"))
            && item.get("status").and_then(Value::as_str) == Some("completed")
            && item.get("receiverThreadIds").is_some();
        for mut spawn in spawns {
            if let Some(canonical_item_id) = canonical_item_id {
                spawn.item_id = canonical_item_id.to_owned();
            } else if let Some(params) = params {
                spawn.item_id = codex_scoped_tool_call_id(params, &spawn.item_id);
            }
            let receiver_thread_id = spawn.receiver_thread_id.clone();
            let fallback_agent_path = spawn.name.clone();
            let native_tool_call_id = spawn.item_id.clone();
            let (conflict, already_registered) = {
                let mut state = self.state.lock().await;
                if spawn.sender_thread_id != state.thread_id {
                    let message = format!(
                        "Codex ownership invariant failed: spawn metadata for child thread '{}' names sender '{}' instead of parent thread '{}'",
                        receiver_thread_id, spawn.sender_thread_id, state.thread_id
                    );
                    state
                        .conflicting_subagent_threads
                        .insert(receiver_thread_id.clone(), message.clone());
                    (Some(message), false)
                } else if let Some(stream) = state.subagent_streams.get_mut(&receiver_thread_id) {
                    if crate::sub_agent::child_name_is_better(&stream.agent_name, &spawn.name) {
                        stream.agent_name = spawn.name.clone();
                        if let Some(tx) = &stream.name_update_tx {
                            let _ = tx.send(spawn.name.clone());
                        }
                    }
                    (None, true)
                } else if let Some(stream) = state
                    .completed_subagent_streams
                    .get_mut(&receiver_thread_id)
                {
                    if crate::sub_agent::child_name_is_better(&stream.agent_name, &spawn.name) {
                        stream.agent_name = spawn.name.clone();
                        if let Some(tx) = &stream.name_update_tx {
                            let _ = tx.send(spawn.name.clone());
                        }
                    }
                    (None, true)
                } else if let Some(existing) =
                    state.pending_subagent_spawns.get_mut(&receiver_thread_id)
                {
                    if existing.item_id == spawn.item_id
                        && existing.sender_thread_id == spawn.sender_thread_id
                    {
                        if crate::sub_agent::child_name_is_better(&existing.name, &spawn.name) {
                            existing.name = spawn.name;
                        }
                        if existing.prompt.is_none() {
                            existing.prompt = spawn.prompt;
                        }
                        tracing::debug!(
                            receiver_thread_id = receiver_thread_id.as_str(),
                            "Repeated authoritative Codex child spawn metadata"
                        );
                        (None, false)
                    } else {
                        (
                            Some(format!(
                                "Codex ownership invariant failed: child thread '{}' has contradictory pending spawn metadata ('{}' and '{}')",
                                receiver_thread_id, existing.item_id, spawn.item_id
                            )),
                            false,
                        )
                    }
                } else {
                    tracing::debug!(
                        item_id = spawn.item_id.as_str(),
                        sender_thread_id = spawn.sender_thread_id.as_str(),
                        receiver_thread_id = receiver_thread_id.as_str(),
                        "Recorded authoritative Codex child spawn metadata"
                    );
                    state
                        .pending_subagent_spawns
                        .insert(receiver_thread_id.clone(), spawn);
                    (None, false)
                }
            };
            if conflict.is_none() && !already_registered {
                self.state
                    .lock()
                    .await
                    .native_subagent_tool_call_ids
                    .insert(native_tool_call_id);
            }
            if let Some(message) = conflict {
                tracing::error!(
                    receiver_thread_id = receiver_thread_id.as_str(),
                    "{message}"
                );
                self.emitter.backend_error(&message);
                continue;
            }
            if register_from_completed_spawn && !already_registered {
                self.register_codex_subagent_activity_if_needed(&json!({
                    "type": "subAgentActivity",
                    "kind": "started",
                    "agentThreadId": receiver_thread_id,
                    "agentPath": fallback_agent_path
                }))
                .await;
            }
        }
    }

    async fn register_codex_subagent_activity_if_needed(&self, item: &Value) {
        tracing::debug!(
            item_id = item.get("id").and_then(|value| value.as_str()),
            agent_thread_id = item
                .get("agentThreadId")
                .or_else(|| item.get("agent_thread_id"))
                .and_then(|value| value.as_str()),
            agent_path = item
                .get("agentPath")
                .or_else(|| item.get("agent_path"))
                .and_then(|value| value.as_str()),
            kind = item.get("kind").and_then(|value| value.as_str()),
            "Codex native sub-agent registration input"
        );
        let Some(activity) = parse_codex_subagent_activity(item) else {
            return;
        };
        if activity.kind != "started" {
            tracing::debug!(
                kind = activity.kind.as_str(),
                agent_thread_id = activity.agent_thread_id.as_str(),
                agent_path = activity.agent_path.as_str(),
                "Observed non-start Codex sub-agent activity"
            );
            return;
        }

        let thread_id = activity.agent_thread_id.clone();
        let (spawn, subagent_sink, rejection, idempotent, needs_synthetic_request) = {
            let mut state = self.state.lock().await;
            if let Some(message) = state.conflicting_subagent_threads.get(&thread_id) {
                (None, None, Some(message.clone()), false, false)
            } else if let Some(stream) = state.subagent_streams.get(&thread_id) {
                if (stream.activity_item_id.is_none() || stream.agent_path == activity.agent_path)
                    && stream.sender_thread_id == state.thread_id
                    && !stream.spawn_item_id.is_empty()
                    && (stream.activity_item_id.is_none()
                        || activity.item_id.is_none()
                        || stream.activity_item_id == activity.item_id)
                {
                    (None, None, None, true, false)
                } else {
                    (
                        None,
                        None,
                        Some(format!(
                            "Codex ownership invariant failed: child thread '{}' was re-registered with contradictory activity metadata",
                            thread_id
                        )),
                        false,
                        false,
                    )
                }
            } else if let Some(stream) = state.completed_subagent_streams.get(&thread_id) {
                if stream.agent_path == activity.agent_path
                    && stream.sender_thread_id == state.thread_id
                    && !stream.spawn_item_id.is_empty()
                    && (stream.activity_item_id.is_none()
                        || activity.item_id.is_none()
                        || stream.activity_item_id == activity.item_id)
                {
                    (None, None, None, true, false)
                } else {
                    (
                        None,
                        None,
                        Some(format!(
                            "Codex ownership invariant failed: completed child thread '{}' was re-registered with contradictory activity metadata",
                            thread_id
                        )),
                        false,
                        false,
                    )
                }
            } else if !state.registering_subagent_threads.insert(thread_id.clone()) {
                (
                    None,
                    None,
                    Some(format!(
                        "Codex ownership invariant failed: child thread '{}' has concurrent duplicate registration activity",
                        thread_id
                    )),
                    false,
                    false,
                )
            } else {
                let pending = state.pending_subagent_spawns.remove(&thread_id);
                let needs_synthetic_request = pending.is_none();
                let spawn = pending.unwrap_or_else(|| CodexSubAgentSpawnInfo {
                    item_id: activity
                        .item_id
                        .clone()
                        .unwrap_or_else(|| thread_id.clone()),
                    tool_name: "spawnAgent".to_owned(),
                    name: activity.agent_path.clone(),
                    prompt: None,
                    agent_type: "sub-agent".to_string(),
                    receiver_thread_id: thread_id.clone(),
                    sender_thread_id: state.thread_id.clone(),
                });
                (
                    Some(spawn),
                    state.subagent_emitter.clone(),
                    None,
                    false,
                    needs_synthetic_request,
                )
            }
        };
        if idempotent {
            tracing::debug!(
                agent_thread_id = thread_id.as_str(),
                agent_path = activity.agent_path.as_str(),
                "Repeated Codex child activity is idempotent"
            );
            return;
        }
        if let Some(message) = rejection {
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
            return;
        }
        let (Some(spawn), Some(subagent_sink)) = (spawn, subagent_sink) else {
            let message = format!(
                "Codex ownership invariant failed: child thread '{}' started before its sub-agent emitter was installed",
                thread_id
            );
            let mut state = self.state.lock().await;
            state.registering_subagent_threads.remove(&thread_id);
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
            return;
        };

        let spawn_item_id = spawn.item_id.clone();
        let spawn_tool_name = spawn.tool_name.clone();
        let spawn_prompt = spawn.prompt.clone();
        let spawn_name = spawn.name.clone();
        let sender_thread_id = spawn.sender_thread_id.clone();
        let progress_tool_call_id = if needs_synthetic_request {
            let synthetic_tool_call_id = format!("codex-native-spawn:{spawn_item_id}");
            self.emit_tool_request(
                &synthetic_tool_call_id,
                &spawn_tool_name,
                CodexToolRequest::from_item(
                    item,
                    serde_json::to_value(protocol::ToolRequestType::AgentSpawn {
                        prompt: spawn_prompt.clone(),
                        name: Some(activity.agent_path.clone()),
                        execution_mode: protocol::AgentExecutionMode::Background,
                    })
                    .expect("serialize native Codex agent spawn"),
                ),
            )
            .await;
            synthetic_tool_call_id
        } else {
            spawn_item_id.clone()
        };
        let handle = match subagent_sink
            .on_subagent_spawned(
                progress_tool_call_id.clone(),
                spawn.name,
                spawn_prompt
                    .clone()
                    .unwrap_or_else(|| activity.agent_path.clone()),
                spawn.agent_type,
                Some(SessionId(thread_id.clone())),
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let message = format!(
                    "Codex child relay registration failed for thread '{}': {error}",
                    thread_id
                );
                {
                    let mut state = self.state.lock().await;
                    state.registering_subagent_threads.remove(&thread_id);
                    state
                        .native_subagent_tool_call_ids
                        .remove(&progress_tool_call_id);
                }
                tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
                self.complete_all_codex_subagents().await;
                if !self
                    .emitter
                    .fail_pending_tool(&progress_tool_call_id, &message)
                {
                    self.emitter.backend_error(&message);
                }
                return;
            }
        };
        let child_agent_id = handle.agent_id.clone();
        let spawned_agent = child_agent_id.clone();
        let (raw_event_tx, raw_event_rx) = mpsc::unbounded_channel();
        spawn_codex_subagent_event_bridge(raw_event_rx, handle.event_tx, handle.model_usage_tx);
        let emitter = Arc::new(TurnEmitter::new_for_agent(
            raw_event_tx,
            AgentName(CODEX_AGENT_NAME),
        ));
        let duplicate_after_spawn = {
            let mut state = self.state.lock().await;
            state.registering_subagent_threads.remove(&thread_id);
            if state.subagent_streams.contains_key(&thread_id)
                || state.completed_subagent_streams.contains_key(&thread_id)
            {
                true
            } else {
                tracing::info!(
                    agent_thread_id = thread_id.as_str(),
                    agent_path = activity.agent_path.as_str(),
                    spawn_item_id = spawn_item_id.as_str(),
                    sender_thread_id = sender_thread_id.as_str(),
                    "Registered authoritative Codex child thread"
                );
                let strict_response_splitting = state
                    .response_splitters
                    .get(&state.thread_id)
                    .is_some_and(|splitter| splitter.enabled);
                state.response_splitters.insert(
                    thread_id.clone(),
                    CodexResponseSplitter::new(&thread_id, strict_response_splitting),
                );
                state.subagent_streams.insert(
                    thread_id.clone(),
                    CodexSubAgentStream {
                        emitter,
                        agent_id: child_agent_id,
                        spawn_item_id: spawn_item_id.clone(),
                        activity_item_id: activity.item_id.clone(),
                        agent_path: activity.agent_path.clone(),
                        agent_name: spawn_name.clone(),
                        name_update_tx: handle.name_update_tx,
                        sender_thread_id,
                        active_turn_id: None,
                        current_message_id: None,
                        current_generated_identity: None,
                        current_reasoning_only: false,
                        current_stream_published: false,
                        current_response: None,
                        current_text: String::new(),
                        current_reasoning: String::new(),
                        current_tool_call_ids: Vec::new(),
                        tool_container: None,
                        pending_tool_call_ids: HashSet::new(),
                        tool_container_images: Vec::new(),
                        completed_agent_messages: HashMap::new(),
                        retired_unpublished_message_ids: HashSet::new(),
                        provider_supersessions_this_turn: 0,
                        supersession_warning_emitted: false,
                        provider_item_tombstones: VecDeque::new(),
                        terminated_turns: VecDeque::new(),
                        terminated_turn_awaiting_replacement: None,
                        pending_spawn_terminal_status: None,
                        background_work_failed: false,
                        generated_identity_epoch: codex_generated_identity_epoch(&thread_id),
                        next_generated_identity_ordinal: 1,
                        pending_message_metadata: None,
                        token_usage_by_turn: HashMap::new(),
                        model_token_usage_by_turn: HashMap::new(),
                        provider_usage_baseline: None,
                        current_images: Vec::new(),
                    },
                );
                self.emitter.tool_progress(&ToolProgressData {
                    tool_call_id: progress_tool_call_id.clone(),
                    execution_mode: ToolExecutionMode::Background,
                    cancellable: false,
                    update: ToolProgressUpdate::SubAgent(protocol::SubAgentProgress {
                        agent_id: spawned_agent,
                        agent_name: spawn_name,
                        last_tool_name: None,
                        tool_calls: 0,
                        completed: false,
                        status: protocol::SubAgentProgressStatus::Running,
                    }),
                });
                false
            }
        };
        if duplicate_after_spawn {
            let message = format!(
                "Codex ownership invariant failed: child relay was created twice for thread '{}'",
                thread_id
            );
            tracing::error!(agent_thread_id = thread_id.as_str(), "{message}");
            self.emitter.backend_error(&message);
        }
    }

    async fn complete_all_codex_subagents(&self) {
        let (stream_keys, native_ids, pending_spawns) = {
            let state = self.state.lock().await;
            (
                state.subagent_streams.keys().cloned().collect::<Vec<_>>(),
                state
                    .native_subagent_tool_call_ids
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                state
                    .pending_subagent_spawns
                    .iter()
                    .map(|(thread_id, spawn)| (thread_id.clone(), spawn.item_id.clone()))
                    .collect::<Vec<_>>(),
            )
        };
        eprintln!(
            "TYDE CODEX CHILD SHUTDOWN stream_keys={stream_keys:?} native_ids={native_ids:?} pending_spawns={pending_spawns:?}"
        );
        for stream_key in stream_keys {
            self.terminalize_codex_subagent_spawn(&stream_key, "cancelled")
                .await;
        }
        let unregistered_spawn_ids = {
            let mut state = self.state.lock().await;
            state.pending_subagent_spawns.clear();
            state.registering_subagent_threads.clear();
            state
                .native_subagent_tool_call_ids
                .drain()
                .collect::<Vec<_>>()
        };
        for recorded_tool_call_id in unregistered_spawn_ids {
            let tool_call_id = if self
                .emitter
                .has_pending_tool_request(&recorded_tool_call_id)
            {
                recorded_tool_call_id
            } else {
                format!("codex-native-spawn:{recorded_tool_call_id}")
            };
            if self.emitter.cancel_pending_tool(
                &tool_call_id,
                "Codex child registration ended with its parent",
            ) {
                self.mark_tool_completed(&tool_call_id).await;
            }
        }
        let mut state = self.state.lock().await;
        let streams = state.subagent_streams.drain().collect::<Vec<_>>();
        for (item_id, mut stream) in streams {
            let commands = take_codex_commands_for_thread(&mut state, &item_id);
            for command in commands {
                if stream
                    .emitter
                    .has_pending_tool_request(&command.tool_call_id)
                {
                    stream.emitter.tool_completed(
                        &command.tool_call_id,
                        ToolExecutionOutcome::Cancelled {
                            message: "Codex child owner exited".to_string(),
                        },
                    );
                }
            }
            for tool_call_id in stream.pending_tool_call_ids.drain() {
                stream
                    .emitter
                    .cancel_pending_tool(&tool_call_id, "Codex child owner exited");
            }
            stream.tool_container = None;
            stream.tool_container_images.clear();
            if stream.current_response.is_some() {
                stream
                    .emitter
                    .operation_cancelled("Codex child owner exited with an open response");
            } else if stream.active_turn_id.is_some() {
                stream
                    .emitter
                    .operation_cancelled("Parent agent turn ended before the sub-agent completed");
            } else {
                tracing::debug!(
                    thread_id = item_id.as_str(),
                    "Retaining completed Codex child without cancellation during parent teardown"
                );
            }
            state
                .completed_subagent_streams
                .insert(item_id, completed_codex_subagent_stream(stream, true));
        }
    }

    async fn retire_idle_codex_subagents(&self) {
        let mut state = self.state.lock().await;
        let idle = state
            .subagent_streams
            .iter()
            .filter(|(thread_id, stream)| {
                stream.active_turn_id.is_none()
                    && stream.current_message_id.is_none()
                    && !state
                        .background_commands
                        .keys()
                        .chain(state.outstanding_command_executions.keys())
                        .any(|(owner_thread_id, _)| owner_thread_id == *thread_id)
            })
            .map(|(thread_id, _)| thread_id.clone())
            .collect::<Vec<_>>();
        for thread_id in idle {
            if let Some(stream) = state.subagent_streams.remove(&thread_id) {
                state
                    .completed_subagent_streams
                    .insert(thread_id, completed_codex_subagent_stream(stream, false));
            }
        }
    }

    fn handle_plan_update(&self, params: &Value) {
        let title = params
            .get("explanation")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("Plan")
            .to_string();

        let tasks = params
            .get("plan")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(idx, step)| protocol::Task {
                id: idx as u64 + 1,
                description: step
                    .get("step")
                    .and_then(Value::as_str)
                    .unwrap_or("step")
                    .to_string(),
                status: map_plan_status(step.get("status").and_then(Value::as_str).unwrap_or("")),
            })
            .collect::<Vec<_>>();

        self.emitter
            .task_update(&protocol::TaskList { title, tasks });
    }

    async fn handle_turn_completed(self: &Arc<Self>, params: &Value) {
        let completed_turn_id = extract_turn_id(params);
        self.flush_raw_modify_failures(completed_turn_id.as_deref())
            .await;
        // Raw apply-patch failures have no typed fileChange item. Recovering
        // their cards above opens a response after the pre-routing terminal
        // sweep has already run, so close that response before this handler
        // reports the turn idle.
        self.finalize_strict_response(params, false).await;
        let consumed_terminated_turn = {
            let mut state = self.state.lock().await;
            match completed_turn_id.as_ref() {
                Some(completed_turn_id)
                    if state
                        .terminated_turns
                        .iter()
                        .any(|turn| turn.turn_id == *completed_turn_id) =>
                {
                    state.token_usage_by_turn.remove(completed_turn_id);
                    state.model_token_usage_by_turn.remove(completed_turn_id);
                    state
                        .completed_message_metadata_by_turn
                        .remove(completed_turn_id);
                    true
                }
                _ => false,
            }
        };
        if consumed_terminated_turn {
            tracing::debug!(
                ?completed_turn_id,
                "Consumed completion for terminated Codex root turn"
            );
            return;
        }
        self.close_tool_container_if_open().await;
        let turn_status = params
            .get("turn")
            .and_then(|v| v.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("completed")
            .to_string();
        let open_item_requires_termination = {
            let state = self.state.lock().await;
            state.active_stream.as_ref().is_some_and(|stream| {
                let matching_turn = completed_turn_id
                    .as_ref()
                    .is_none_or(|turn_id| stream.turn_id == *turn_id);
                let durable_idless_reasoning = stream.reasoning_only
                    && stream.stream_published
                    && stream.generated_identity.as_ref().is_some_and(|identity| {
                        identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                    })
                    && contains_non_whitespace(&stream.reasoning);
                !matching_turn || (turn_status != "interrupted" && !durable_idless_reasoning)
            })
        };
        if open_item_requires_termination {
            self.reject_agent_message_identity(
                CodexProviderStreamConflict::MismatchedEndMessageId,
                "turn/completed",
                None,
            )
            .await;
            return;
        }
        let model_hint = {
            let state = self.state.lock().await;
            state.effective_model.clone()
        };
        let turn_usage = extract_turn_token_usage(params, model_hint.as_deref());
        let model_usage = extract_model_request_token_usage(params, model_hint.as_deref());
        let open_foreground_tool_ids = self
            .emitter
            .open_foreground_tool_ids()
            .into_iter()
            .collect::<HashSet<_>>();

        let (
            open_item_without_completion,
            open_item_published,
            interrupted_published_stream,
            partial_idless_reasoning,
            metadata_update,
            model_usage,
            terminated_background_commands,
            defer_idle_until_foreground_tools_complete,
        ) = {
            let mut state = self.state.lock().await;
            if let Some((turn_id, token_usage)) = turn_usage {
                state.token_usage_by_turn.insert(turn_id, token_usage);
            }
            let model_usage =
                model_usage.and_then(|(turn_id, request, cumulative, context_window)| {
                    let usage = record_model_request_token_usage(
                        &mut state.model_token_usage_by_turn,
                        turn_id,
                        request,
                        cumulative,
                        context_window,
                    )?;
                    Some(usage)
                });

            let completed_turn_id =
                extract_turn_id(params).or_else(|| state.active_turn_id.clone());
            state.active_turn_id = None;
            state.foreground_response_completed = false;
            let mut open_item_without_completion = false;
            let mut open_item_published = false;
            let mut interrupted_published_stream = None;
            let mut partial_idless_reasoning = None;
            let mut metadata_update = None;
            let mut terminated_background_commands = Vec::new();
            if let Some(turn_id) = completed_turn_id {
                let has_open_stream = state
                    .active_stream
                    .as_ref()
                    .is_some_and(|stream| stream.turn_id == turn_id);
                let open_stream = has_open_stream
                    .then(|| state.active_stream.take())
                    .flatten();
                if let Some(stream) = open_stream {
                    state.pending_message_metadata = None;
                    open_item_published = stream.stream_published;
                    let reasoning = contains_non_whitespace(&stream.reasoning)
                        .then(|| stream.reasoning.clone());
                    if turn_status == "interrupted" && stream.stream_published {
                        let content = stream.text.clone();
                        state.completed_agent_messages.insert(
                            stream.message_id.clone(),
                            CompletedCodexAgentMessage {
                                reported_text: content.clone(),
                                reported_reasoning: reasoning.clone(),
                                completion_text: content.clone(),
                                completion_reasoning: reasoning.clone(),
                            },
                        );
                        interrupted_published_stream = Some(InterruptedPublishedStream {
                            response: self
                                .emitter
                                .open_response()
                                .expect("published Codex response"),
                            content,
                            reasoning,
                            images: stream.images,
                        });
                    } else {
                        let durable_idless_reasoning = stream.reasoning_only
                            && stream.generated_identity.as_ref().is_some_and(|identity| {
                                identity.origin == CodexProviderResponseOrigin::IdlessReasoning
                            })
                            && reasoning.is_some();
                        if durable_idless_reasoning && stream.stream_published {
                            let reasoning = reasoning.expect("durable reasoning");
                            state.completed_agent_messages.insert(
                                stream.message_id.clone(),
                                CompletedCodexAgentMessage {
                                    reported_text: String::new(),
                                    reported_reasoning: Some(reasoning.clone()),
                                    completion_text: String::new(),
                                    completion_reasoning: Some(reasoning.clone()),
                                },
                            );
                            partial_idless_reasoning = Some((
                                self.emitter
                                    .open_response()
                                    .expect("published Codex reasoning"),
                                reasoning,
                            ));
                        } else {
                            open_item_without_completion =
                                !(turn_status == "interrupted" && !stream.stream_published);
                        }
                    }
                }

                if turn_status != "interrupted"
                    && partial_idless_reasoning.is_none()
                    && !open_item_without_completion
                    && state
                        .pending_message_metadata
                        .as_ref()
                        .is_some_and(|pending| pending.turn_id == turn_id)
                    && let Some(pending) = state.pending_message_metadata.take()
                {
                    let token_usage = state.token_usage_by_turn.remove(&turn_id);
                    if token_usage.is_none() {
                        state
                            .completed_message_metadata_by_turn
                            .insert(turn_id.clone(), pending.clone());
                    }
                    let model_token_usage = state.model_token_usage_by_turn.get(&turn_id).cloned();
                    metadata_update = Some((pending, token_usage, model_token_usage, None));
                }
                if turn_status == "interrupted" || partial_idless_reasoning.is_some() {
                    state.pending_message_metadata = None;
                    state.completed_message_metadata_by_turn.remove(&turn_id);
                    push_codex_terminated_turn(&mut state.terminated_turns, turn_id.clone());
                    state.terminated_turn_awaiting_replacement = Some(turn_id.clone());
                    let root_thread_id = state.thread_id.clone();
                    terminated_background_commands =
                        take_codex_commands_for_turn(&mut state, &root_thread_id, &turn_id);
                }
                state.token_usage_by_turn.remove(&turn_id);
                state.model_token_usage_by_turn.remove(&turn_id);
            }
            state.pending_request = None;
            state.file_change_call_ids.clear();
            if turn_status == "interrupted" {
                let interrupted_tool_call_ids = state
                    .pending_tool_call_ids
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                state
                    .cancelled_tool_call_ids
                    .extend(interrupted_tool_call_ids);
                state.pending_tool_call_ids.clear();
                state.close_active_stream_when_tools_idle = false;
            } else {
                // The emitter's execution mode is the explicit protocol
                // ownership signal. Background requests may outlive an idle
                // turn; foreground requests keep the presentation active.
                state.pending_tool_call_ids = open_foreground_tool_ids;
                state.close_active_stream_when_tools_idle = !state.pending_tool_call_ids.is_empty();
            }
            state.tool_container_images.clear();
            let defer_idle_until_foreground_tools_complete =
                state.close_active_stream_when_tools_idle;
            (
                open_item_without_completion,
                open_item_published,
                interrupted_published_stream,
                partial_idless_reasoning,
                metadata_update,
                model_usage,
                terminated_background_commands,
                defer_idle_until_foreground_tools_complete,
            )
        };

        for command in terminated_background_commands {
            self.emitter.cancel_pending_tool(
                &command.tool_call_id,
                "Codex background command was cancelled with its interrupted turn",
            );
        }
        if let Some(usage) = model_usage {
            self.emitter.model_request_token_usage(&usage);
        }
        if let Some(stream) = interrupted_published_stream {
            self.trace_terminal_emission("stream_end", None).await;
            self.emitter.stream_end(
                stream.response,
                StreamEndPayload {
                    content: stream.content,
                    model_info: Some(ModelInfo {
                        model: model_hint.clone().unwrap_or_else(|| "codex".to_string()),
                    }),
                    reasoning: stream.reasoning.map(reasoning_data),
                    images: stream.images,
                    ..StreamEndPayload::default()
                },
            );
            self.emitter.operation_cancelled("Operation cancelled");
            return;
        }
        if let Some((response, reasoning)) = partial_idless_reasoning {
            if turn_status == "failed" {
                self.complete_all_codex_subagents().await;
            }
            self.trace_terminal_emission("stream_end", None).await;
            self.emitter.stream_end(
                response,
                StreamEndPayload {
                    model_info: Some(ModelInfo {
                        model: model_hint.unwrap_or_else(|| "codex".to_string()),
                    }),
                    reasoning: Some(reasoning_data(reasoning)),
                    ..StreamEndPayload::default()
                },
            );
            self.emitter
                .operation_cancelled("Codex turn ended before reasoning item completion");
            return;
        }
        if open_item_without_completion {
            if turn_status == "failed" {
                self.complete_all_codex_subagents().await;
            }
            let _ = open_item_published;
            self.emitter
                .backend_error("Codex ended a turn before completing its active response");
            self.emitter
                .operation_cancelled("Codex response ended without a terminal event");
            return;
        }
        if let Some((pending, token_usage, model_token_usage, context_breakdown)) = metadata_update
        {
            emit_codex_message_metadata_update(
                &self.emitter,
                pending,
                token_usage,
                model_token_usage.as_ref(),
                context_breakdown,
            );
        }

        if turn_status == "interrupted" {
            self.retire_idle_codex_subagents().await;
            // emitter.operation_cancelled runs the full cancel tail:
            // flush pending tools → OperationCancelled → TypingStatusChanged(false).
            self.emitter.operation_cancelled("Operation cancelled");
            return;
        }

        if defer_idle_until_foreground_tools_complete {
            tracing::debug!(
                pending_foreground_tools = self.emitter.open_foreground_tool_ids().len(),
                "Deferring Codex idle until foreground tools complete"
            );
            if turn_status == "failed" {
                let message = params
                    .get("turn")
                    .and_then(|v| v.get("error"))
                    .and_then(|v| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex turn failed")
                    .to_string();
                self.complete_all_codex_subagents().await;
                self.emitter.backend_error(&message);
            }
            return;
        }

        self.trace_terminal_emission("idle", None).await;
        self.emitter.typing_status_changed(false);
        self.spawn_pending_background_wake();

        if turn_status == "failed" {
            let message = params
                .get("turn")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed")
                .to_string();
            self.complete_all_codex_subagents().await;
            self.emitter.backend_error(&message);
        }
    }

    async fn emit_tool_execution_completed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        success: bool,
        tool_result: Value,
        error: Option<String>,
    ) {
        if self.suppress_cancelled_tool_completion(tool_call_id).await {
            return;
        }
        let (tool_result, normalization_failure) = normalize_codex_tool_result(
            &self.emitter,
            tool_call_id,
            tool_name,
            tool_result,
            success,
        );
        let outcome = codex_tool_execution_outcome(
            tool_result,
            success && normalization_failure.is_none(),
            error,
            normalization_failure,
        );
        eprintln!(
            "TYDE CODEX TOOL TERMINAL source=provider tool_call_id={tool_call_id} tool_name={tool_name} outcome={outcome:?}"
        );
        self.emitter.tool_completed(tool_call_id, outcome);
        self.mark_tool_completed(tool_call_id).await;
    }

    async fn complete_image_generation(&self, tool_call_id: &str, item: &Value) {
        if self.suppress_cancelled_tool_completion(tool_call_id).await {
            return;
        }
        let revised_prompt = codex_image_generation_prompt(item);
        let image = parse_codex_generated_image(item);
        let (success, tool_result, error, images) = match image {
            Ok(image) => (
                true,
                serde_json::to_value(protocol::ToolExecutionResult::GenerateImage {
                    revised_prompt,
                    image_count: 1,
                })
                .expect("serialize Codex image generation result"),
                None,
                vec![image],
            ),
            Err(error) => (
                false,
                json!({
                    "kind": "Error",
                    "short_message": "Image generation failed",
                    "detailed_message": error,
                }),
                Some(error),
                Vec::new(),
            ),
        };
        self.emitter.tool_completed(
            tool_call_id,
            codex_tool_execution_outcome(tool_result, success, error, None),
        );
        self.mark_tool_completed_with_images(tool_call_id, images)
            .await;
    }

    async fn complete_native_tool(&self, tool_call_id: &str, item_type: &str) {
        let Some((tool_name, tool_result)) = codex_native_tool_completion(item_type) else {
            return;
        };
        self.emit_tool_execution_completed(tool_call_id, tool_name, true, tool_result, None)
            .await;
    }

    async fn emit_agent_control_await_progress_if_needed(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &Value,
        status: protocol::AgentControlProgressStatus,
    ) {
        if !self.emitter.has_pending_tool_request(tool_call_id) {
            return;
        }
        // Tyde's own await is handled once for every backend in
        // `EventStream::project_tyde_agent_control`. What is left here is the
        // Codex-native wait, whose watched agents can only be resolved from
        // Codex's own thread state.
        if !is_codex_native_wait_tool(tool_name) {
            return;
        }

        let mut thread_ids = codex_native_wait_thread_ids(arguments);
        let agents = {
            let state = self.state.lock().await;
            if thread_ids.is_empty() {
                thread_ids.extend(
                    state
                        .subagent_streams
                        .iter()
                        .filter(|(_, stream)| {
                            self.emitter.has_pending_tool_request(&stream.spawn_item_id)
                                || self.emitter.has_pending_tool_request(&format!(
                                    "codex-native-spawn:{}",
                                    stream.spawn_item_id
                                ))
                        })
                        .map(|(thread_id, _)| thread_id.clone()),
                );
                thread_ids.sort();
            }
            thread_ids
                .iter()
                .filter_map(|thread_id| {
                    state
                        .subagent_streams
                        .get(thread_id)
                        .map(|stream| AgentControlAgentRef {
                            agent_id: stream.agent_id.clone(),
                            name: Some(stream.agent_path.clone()),
                        })
                        .or_else(|| {
                            state
                                .completed_subagent_streams
                                .get(thread_id)
                                .map(|stream| AgentControlAgentRef {
                                    agent_id: stream.agent_id.clone(),
                                    name: Some(stream.agent_path.clone()),
                                })
                        })
                })
                .collect::<Vec<_>>()
        };
        if agents.len() != thread_ids.len() {
            tracing::warn!(
                tool_call_id,
                resolved = agents.len(),
                requested = thread_ids.len(),
                "Codex native wait referenced an unregistered child thread"
            );
        }
        if agents.is_empty() {
            return;
        }
        self.emitter.tool_progress(&ToolProgressData {
            tool_call_id: tool_call_id.to_owned(),
            execution_mode: ToolExecutionMode::Foreground,
            cancellable: false,
            update: ToolProgressUpdate::AgentControl(AgentControlProgress {
                progress_kind: AgentControlProgressKind::Await,
                agents,
                status,
            }),
        });
    }

    async fn emit_tool_request(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        request: CodexToolRequest,
    ) {
        let thread_id = self.state.lock().await.thread_id.clone();
        self.emit_tool_request_for_thread(&thread_id, tool_call_id, tool_name, request)
            .await;
    }

    async fn emit_tool_request_for_thread(
        &self,
        thread_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        request: CodexToolRequest,
    ) {
        if self
            .buffer_strict_tool_request(
                thread_id,
                tool_call_id,
                tool_name,
                request.arguments.clone(),
                request.tool_type.clone(),
            )
            .await
        {
            return;
        }
        // Not `response_projection_target`: that one is `None` for a thread with
        // strict splitting off, which is every resumed and forked thread. Using
        // it here meant a resumed session dropped *every* tool card silently at
        // birth, while the completion path — which resolves its emitter without
        // that condition — still fired, so each tool produced a
        // `completion_without_request` and no card at all.
        let Some((emitter, model)) = self.tool_projection_target(thread_id).await else {
            tracing::error!(
                thread_id,
                tool_call_id,
                tool_name,
                "Codex tool request had no emitter to declare it on"
            );
            return;
        };
        if emitter.has_known_tool_request(tool_call_id) {
            return;
        }
        let response = emitter.stream_start(Some(&model));
        emitter.stream_end(
            response,
            StreamEndPayload {
                model_info: Some(ModelInfo { model }),
                tool_calls: vec![ToolUseData {
                    tool_call_id: tool_call_id.to_owned(),
                    name: tool_name.to_owned(),
                    arguments: request.arguments,
                    content_offset: Some(0),
                }],
                ..StreamEndPayload::default()
            },
        );
        emitter.tool_request(tool_call_id, codex_tool_request_type(request.tool_type));
    }

    async fn emit_modify_file_request(
        &self,
        tool_call_id: &str,
        file_path: &str,
        before: &str,
        after: &str,
    ) {
        self.emit_tool_request(
            tool_call_id,
            "modify_file",
            // Spelled out rather than taken off a provider item, because there
            // is no item to take it off: `fileChange` is an outcome Codex
            // reports, not a tool call with an argument object, and all three
            // callers reach here holding an already-parsed change. The flat
            // fields are what this card's arguments actually are.
            CodexToolRequest::typed(
                json!({
                    "file_path": file_path,
                    "before": before,
                    "after": after
                }),
                json!({
                    "kind": "ModifyFile",
                    "file_path": file_path,
                    "before": before,
                    "after": after
                }),
            ),
        )
        .await;
    }

    fn emit_user_message_added(&self, content: &str, images: Option<&[ImageAttachment]>) {
        let image_payload = images.map(|images| {
            images
                .iter()
                .map(|image| ImageData {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect::<Vec<_>>()
        });
        self.emitter.user_message(content, image_payload);
    }
}

/// What to report when `turn/completed` arrives with a response still open.
///
/// An interrupted turn ends without `rawResponse/completed` by construction:
/// the response was cut off mid-flight, not lost, so there is nothing to
/// report. Codex says which happened in `turn.status` — the same field
/// `handle_turn_completed` already reads to decide whether the open item needs
/// terminating and whether to attach a context breakdown. Reading it here too
/// is what keeps a cancel from putting an error card on every interrupted
/// turn.
///
/// Every other status still reports: a response left open by anything but a
/// cancel is a real violation, and staying silent about it would hide the bug
/// this check exists to catch.
fn incomplete_turn_response_error(params: &Value) -> Option<&'static str> {
    let status = params
        .get("turn")
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str);
    if status == Some("interrupted") {
        return None;
    }
    Some("Codex turn ended before rawResponse/completed")
}

fn extract_notification_thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.get("thread_id").and_then(Value::as_str))
        .or_else(|| {
            params
                .get("thread")
                .and_then(|thread| thread.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("threadId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("thread_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| params.get("senderThreadId").and_then(Value::as_str))
        .map(|id| id.to_string())
}

fn codex_scoped_tool_call_id(params: &Value, provider_item_id: &str) -> String {
    let Some(thread_id) = extract_notification_thread_id(params) else {
        return provider_item_id.to_owned();
    };
    let Some(turn_id) = extract_turn_id(params) else {
        return provider_item_id.to_owned();
    };
    let scoped = format!("codex:{thread_id}:{turn_id}:{provider_item_id}");
    tracing::debug!(
        provider_tool_call_id = provider_item_id,
        canonical_tool_call_id = scoped,
        thread_id,
        turn_id,
        "Scoped Codex tool identity"
    );
    scoped
}

fn codex_error_tool_call_id(params: &Value) -> Option<String> {
    [
        "/toolCallId",
        "/tool_call_id",
        "/callId",
        "/call_id",
        "/item/id",
    ]
    .into_iter()
    .find_map(|pointer| params.pointer(pointer).and_then(Value::as_str))
    .map(str::trim)
    .filter(|tool_call_id| !tool_call_id.is_empty())
    .map(|tool_call_id| codex_scoped_tool_call_id(params, tool_call_id))
}

fn is_thread_scoped_codex_notification(method: &str) -> bool {
    matches!(
        method,
        "turn/started"
            | "turn/completed"
            | "turn/plan/updated"
            | "thread/tokenUsage/updated"
            | "thread/settings/updated"
    ) || method.starts_with("item/")
        || is_reasoning_notification_method(method)
}

fn classify_codex_notification_owner(state: &CodexState, params: &Value) -> CodexNotificationOwner {
    let Some(thread_id) = extract_notification_thread_id(params) else {
        return CodexNotificationOwner::Unknown { thread_id: None };
    };
    if thread_id == state.thread_id
        || state.pending_resume_thread_id.as_deref() == Some(thread_id.as_str())
    {
        return CodexNotificationOwner::Parent { thread_id };
    }
    if state.subagent_streams.contains_key(&thread_id) {
        return CodexNotificationOwner::LiveChild { thread_id };
    }
    if state.completed_subagent_streams.contains_key(&thread_id) {
        return CodexNotificationOwner::CompletedChild { thread_id };
    }
    if let Some(ancestor_thread_id) = state.descendant_owner_threads.get(&thread_id) {
        return CodexNotificationOwner::Descendant {
            thread_id,
            ancestor_thread_id: ancestor_thread_id.clone(),
        };
    }
    CodexNotificationOwner::Unknown {
        thread_id: Some(thread_id),
    }
}

fn codex_plan_update_task_list_from_params(params: &Value) -> Option<protocol::TaskList> {
    let title = params
        .get("explanation")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Plan")
        .to_string();
    let plan = params.get("plan").and_then(Value::as_array)?;
    let tasks = plan
        .iter()
        .enumerate()
        .map(|(idx, step)| protocol::Task {
            id: idx as u64 + 1,
            description: step
                .get("step")
                .and_then(Value::as_str)
                .unwrap_or("step")
                .to_string(),
            status: map_plan_status(step.get("status").and_then(Value::as_str).unwrap_or("")),
        })
        .collect::<Vec<_>>();

    Some(protocol::TaskList { title, tasks })
}

fn codex_thread_to_session_metadata(thread: &Value) -> Option<Value> {
    let session_id = thread.get("id").and_then(Value::as_str)?;
    let preview = thread
        .get("preview")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = thread
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if preview.trim().is_empty() {
                "Codex Session".to_string()
            } else {
                preview.clone()
            }
        });

    let created_at = thread
        .get("createdAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(unix_now_ms);
    let last_modified = thread
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or(created_at);
    let workspace_root = thread
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let message_count: Option<u64> = thread.get("turns").and_then(Value::as_array).map(|turns| {
        turns
            .iter()
            .filter_map(|turn| turn.get("items").and_then(Value::as_array))
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
                    .count() as u64
            })
            .sum::<u64>()
    });

    Some(json!({
        "id": session_id,
        "session_id": session_id,
        "title": title,
        "created_at": created_at,
        "last_modified": last_modified,
        "last_message_preview": preview,
        "workspace_root": workspace_root,
        "message_count": message_count,
        "backend_kind": "codex"
    }))
}

fn codex_item_success(item: &Value) -> bool {
    if let Some(success) = item.get("success").and_then(Value::as_bool) {
        return success;
    }

    let normalized_status = item
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| item.get("agentStatus").and_then(Value::as_str))
        .map(|status| status.trim().to_ascii_lowercase());

    match normalized_status.as_deref() {
        Some("completed" | "complete" | "succeeded" | "success" | "ok" | "done") => true,
        Some("failed" | "error" | "cancelled" | "canceled" | "interrupted" | "denied") => false,
        _ => true,
    }
}

fn is_codex_native_wait_tool(tool_name: &str) -> bool {
    matches!(
        tool_name
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .map(|ch| ch.to_ascii_lowercase())
            .collect::<String>()
            .as_str(),
        "wait" | "waitagent"
    )
}

fn codex_native_wait_thread_ids(item: &Value) -> Vec<String> {
    let mut thread_ids = Vec::new();
    if let Some(thread_id) = item
        .get("receiverThreadId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|thread_id| !thread_id.is_empty())
    {
        thread_ids.push(thread_id.to_owned());
    }
    thread_ids.extend(
        item.get("receiverThreadIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|thread_id| !thread_id.is_empty())
            .map(str::to_owned),
    );
    if thread_ids.is_empty() {
        let mut state_ids = item
            .get("agentsStates")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|states| states.keys().cloned())
            .filter(|thread_id| !thread_id.trim().is_empty())
            .collect::<Vec<_>>();
        state_ids.sort();
        thread_ids.extend(state_ids);
    }
    let mut seen = HashSet::new();
    thread_ids.retain(|thread_id| seen.insert(thread_id.clone()));
    thread_ids
}

fn parse_codex_subagent_collabs(item: &Value) -> Vec<CodexSubAgentSpawnInfo> {
    if item.get("type").and_then(Value::as_str) != Some("collabAgentToolCall") {
        return Vec::new();
    }
    let explicit_spawn_tool = item
        .get("tool")
        .and_then(Value::as_str)
        .is_some_and(|tool| tool.eq_ignore_ascii_case("spawnAgent"));
    let has_legacy_spawn_shape = item.get("agentsStates").is_none()
        && item.get("prompt").and_then(Value::as_str).is_some()
        && (item
            .get("receiverAgentType")
            .and_then(Value::as_str)
            .is_some()
            || item
                .get("receiverAgentName")
                .and_then(Value::as_str)
                .is_some());
    if !explicit_spawn_tool && !has_legacy_spawn_shape {
        return Vec::new();
    }
    let Some(item_id) = item
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| item.get("callId").and_then(Value::as_str))
        .map(str::to_string)
    else {
        return Vec::new();
    };
    let mut receiver_thread_ids = item
        .get("receiverThreadId")
        .and_then(Value::as_str)
        .map(|thread_id| vec![thread_id.to_string()])
        .unwrap_or_default();
    if receiver_thread_ids.is_empty() {
        receiver_thread_ids.extend(
            item.get("receiverThreadIds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
    }
    let Some(sender_thread_id) = item
        .get("senderThreadId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Vec::new();
    };
    receiver_thread_ids.retain(|thread_id| !thread_id.trim().is_empty());
    receiver_thread_ids.sort();
    receiver_thread_ids.dedup();
    if receiver_thread_ids.is_empty() || sender_thread_id.trim().is_empty() {
        return Vec::new();
    }
    let agent_type = item
        .get("receiverAgentType")
        .and_then(Value::as_str)
        .unwrap_or("sub-agent")
        .to_string();
    let tool_name = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("spawnAgent")
        .to_string();
    let prompt = item
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_owned);
    let name = item
        .get("receiverAgentName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Sub-agent")
        .to_string();
    receiver_thread_ids
        .into_iter()
        .map(|receiver_thread_id| CodexSubAgentSpawnInfo {
            item_id: item_id.clone(),
            tool_name: tool_name.clone(),
            name: name.clone(),
            prompt: prompt.clone(),
            agent_type: agent_type.clone(),
            receiver_thread_id,
            sender_thread_id: sender_thread_id.clone(),
        })
        .collect()
}

fn parse_codex_subagent_activity(item: &Value) -> Option<CodexSubAgentActivity> {
    if !matches!(
        item.get("type").and_then(Value::as_str),
        Some("subAgentActivity" | "sub_agent_activity")
    ) {
        return None;
    }
    let agent_thread_id = item
        .get("agentThreadId")
        .or_else(|| item.get("agent_thread_id"))
        .and_then(Value::as_str)?
        .to_string();
    if agent_thread_id.trim().is_empty() {
        return None;
    }
    Some(CodexSubAgentActivity {
        item_id: item.get("id").and_then(Value::as_str).map(str::to_string),
        agent_thread_id,
        agent_path: item
            .get("agentPath")
            .or_else(|| item.get("agent_path"))
            .and_then(Value::as_str)
            .unwrap_or("Sub-agent")
            .to_string(),
        kind: item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase(),
    })
}

fn extract_codex_item_text(item: &Value) -> String {
    if let Some(text) = item.get("text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return text.to_string();
    }

    let mut chunks: Vec<String> = Vec::new();
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        for part in content {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("inputText").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    chunks.push(text.to_string());
                }
                continue;
            }
            if let Some(text) = part.get("value").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                chunks.push(text.to_string());
            }
        }
    }

    if chunks.is_empty() {
        String::new()
    } else {
        chunks.join("\n")
    }
}

fn extract_codex_reasoning_delta_text(params: &Value) -> Option<String> {
    for key in [
        "delta",
        "text",
        "summaryText",
        "summary_text",
        "reasoningSummary",
        "reasoning_summary",
        "reasoningSummaryText",
        "reasoning_summary_text",
        "summary",
        "reasoning",
        "thinking",
        "content",
    ] {
        if let Some(text) = extract_codex_reasoning_delta_fragment(params.get(key)) {
            return Some(text);
        }
    }

    for nested in ["msg", "event", "payload"] {
        if let Some(value) = params.get(nested)
            && let Some(text) = extract_codex_reasoning_delta_text(value)
        {
            return Some(text);
        }
    }

    params.get("item").and_then(extract_codex_item_reasoning)
}

fn extract_codex_reasoning_delta_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(values) => {
            let mut out = String::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_delta_fragment(Some(part)) {
                    out.push_str(&text);
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        Value::Object(map) => {
            for key in [
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "text",
                "value",
                "token",
                "output_text",
                "outputText",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_delta_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_reasoning_delta_from_legacy_codex_event(method: &str, params: &Value) -> Option<String> {
    let event_type = extract_codex_event_type(method, params)?;
    if event_type == "agent_reasoning_section_break" {
        return Some("\n\n".to_string());
    }
    if !is_codex_event_reasoning_type(&event_type) {
        return None;
    }
    extract_codex_reasoning_delta_text(params)
}

fn extract_legacy_codex_retry_attempt<'a>(
    method: &str,
    params: &'a Value,
) -> Option<RetryAttemptPayload<'a>> {
    if !method.ends_with("/stream_error") {
        return None;
    }
    let message = params
        .get("msg")
        .and_then(|msg| msg.get("message"))
        .and_then(Value::as_str)?;
    let (attempt, max_retries) = parse_codex_reconnecting_attempt(message)?;
    let error = params
        .get("msg")
        .and_then(|msg| msg.get("additional_details"))
        .and_then(Value::as_str)
        .unwrap_or(message);
    Some(RetryAttemptPayload {
        attempt,
        max_retries,
        error,
        backoff_ms: 250u64
            .saturating_mul(1u64 << attempt.saturating_sub(1))
            .min(4_000),
    })
}

fn parse_codex_reconnecting_attempt(message: &str) -> Option<(u64, u64)> {
    let reconnect = message.split("Reconnecting... ").nth(1)?;
    let reconnect = reconnect.split_whitespace().next()?;
    let (connection, total) = reconnect.split_once('/')?;
    let attempt = connection.parse::<u64>().ok()?.checked_sub(1)?;
    let max_retries = total.parse::<u64>().ok()?.checked_sub(1)?;
    (attempt > 0 && attempt <= max_retries).then_some((attempt, max_retries))
}

fn metadata_target_for_visible_message(
    turn_id: String,
    message_id: ChatMessageId,
    content: &str,
    reasoning: Option<&str>,
    model: String,
) -> Option<PendingCodexMessageMetadata> {
    if message_id.0.trim().is_empty() {
        return None;
    }
    if !contains_non_whitespace(content) && !reasoning.is_some_and(contains_non_whitespace) {
        return None;
    }
    Some(PendingCodexMessageMetadata {
        turn_id,
        message_id,
        model,
    })
}

fn emit_codex_message_metadata_update(
    emitter: &TurnEmitter,
    pending: PendingCodexMessageMetadata,
    token_usage: Option<Value>,
    model_token_usage: Option<&CodexTurnTokenUsage>,
    context_breakdown: Option<Value>,
) {
    let token_usage = codex_message_usage(token_usage, model_token_usage);
    emitter.message_metadata_updated(MessageMetadataUpdateData {
        message_id: pending.message_id,
        model_info: Some(ModelInfo {
            model: pending.model,
        }),
        token_usage,
        context_breakdown: context_breakdown.and_then(|value| serde_json::from_value(value).ok()),
    });
}

fn codex_message_usage(
    token_usage: Option<Value>,
    model_token_usage: Option<&CodexTurnTokenUsage>,
) -> Option<MessageTokenUsage> {
    let unavailable = || TokenUsageScope::Unavailable {
        reason: TokenUsageUnavailableReason::BackendDidNotReport,
    };
    let known = |usage: TokenUsage| TokenUsageScope::Known {
        usage: Box::new(usage),
    };
    if let Some(usage) = model_token_usage {
        return Some(MessageTokenUsage {
            request: usage
                .latest_request
                .clone()
                .map(&known)
                .unwrap_or_else(unavailable),
            turn: known(usage.turn.clone()),
            cumulative: usage
                .cumulative
                .clone()
                .map(known)
                .unwrap_or_else(unavailable),
        });
    }
    let usage = token_usage.as_ref().and_then(codex_token_usage)?;
    Some(MessageTokenUsage {
        request: known(usage.clone()),
        turn: known(usage),
        cumulative: unavailable(),
    })
}

fn reasoning_data(text: String) -> ReasoningData {
    ReasoningData {
        text,
        tokens: None,
        signature: None,
        blob: None,
    }
}

fn codex_tool_request_type(value: Value) -> ToolRequestType {
    serde_json::from_value(value.clone()).unwrap_or(ToolRequestType::Other { args: value })
}

/// One Codex tool call, as both things the stream has to carry: the arguments
/// the model passed, and Tyde's normalized reading of them.
///
/// A struct rather than two adjacent `Value` parameters because two adjacent
/// `Value`s are precisely how this went wrong. `buffer_strict_tool_request`
/// already took separate `arguments` and `tool_type`, and both of its call
/// sites passed the normalized type for both, so every Codex `ToolUseData`
/// carried Tyde's own `{"kind":...}` envelope instead of what the model passed
/// -- while `protocol::ToolRequest` documents `ToolUseData` as the one place a
/// provider's name and arguments survive. Pairing them at construction means a
/// caller cannot supply one without having decided the other.
#[derive(Clone, Debug)]
struct CodexToolRequest {
    /// What the provider reported the model passed, untouched.
    arguments: Value,
    /// A serialized [`ToolRequestType`].
    tool_type: Value,
}

impl CodexToolRequest {
    /// A tool Tyde has no typed card for. The normalized form wraps the very
    /// same arguments, so there is one source for both.
    fn other(arguments: Value) -> Self {
        Self {
            tool_type: json!({ "kind": "Other", "args": arguments.clone() }),
            arguments,
        }
    }

    /// A tool Tyde normalizes into a typed card. `arguments` stays the
    /// provider's own; `tool_type` is Tyde's reading of it, and the two are
    /// deliberately not derived from each other.
    fn typed(arguments: Value, tool_type: Value) -> Self {
        Self {
            arguments,
            tool_type,
        }
    }

    /// The common case: a provider item Tyde reads into a typed card. The
    /// arguments come off the item itself, so no call site restates them.
    fn from_item(item: &Value, tool_type: Value) -> Self {
        Self::typed(codex_generic_tool_arguments(item), tool_type)
    }
}

/// [`codex_public_tool_request_type`] paired with the arguments it read.
fn codex_public_tool_request(tool_name: &str, item: &Value) -> CodexToolRequest {
    CodexToolRequest::from_item(item, codex_public_tool_request_type(tool_name, item))
}

fn codex_token_usage(value: &Value) -> Option<TokenUsage> {
    serde_json::from_value(value.clone()).ok()
}

fn extract_codex_event_type(method: &str, params: &Value) -> Option<String> {
    params
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("msg")
                .and_then(|msg| msg.get("type"))
                .and_then(Value::as_str)
        })
        .or_else(|| method.strip_prefix("codex/event/"))
        .map(|raw| raw.trim().to_ascii_lowercase())
}

fn is_codex_event_reasoning_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "agent_reasoning"
            | "agent_reasoning_delta"
            | "agent_reasoning_raw_content"
            | "agent_reasoning_raw_content_delta"
    )
}

fn extract_codex_item_reasoning(item: &Value) -> Option<String> {
    extract_codex_reasoning_fragment(item.get("reasoning"))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summaryText")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("summary")))
        .or_else(|| extract_codex_reasoning_fragment(item.get("reasoningSummary")))
        .or_else(|| {
            let mut chunks = Vec::new();
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let part_type = part
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if !part_type.contains("reason")
                        && !part_type.contains("think")
                        && !part_type.contains("summary")
                    {
                        continue;
                    }
                    if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                        chunks.push(text);
                    }
                }
            }
            join_nonempty_chunks(chunks)
        })
}

fn extract_codex_reasoning_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(text) => {
            if !contains_non_whitespace(text) {
                None
            } else {
                Some(text.to_string())
            }
        }
        Value::Array(values) => {
            let mut chunks = Vec::new();
            for part in values {
                if let Some(text) = extract_codex_reasoning_fragment(Some(part)) {
                    chunks.push(text);
                }
            }
            join_nonempty_chunks(chunks)
        }
        Value::Object(map) => {
            for key in [
                "text",
                "summaryText",
                "summary_text",
                "summary",
                "reasoningSummary",
                "reasoning_summary",
                "reasoningSummaryText",
                "reasoning_summary_text",
                "reasoning",
                "thinking",
                "output_text",
                "outputText",
                "delta",
                "summary_delta",
                "summaryDelta",
                "reasoning_delta",
                "reasoningDelta",
                "token",
                "value",
                "content",
                "parts",
            ] {
                if let Some(text) = extract_codex_reasoning_fragment(map.get(key)) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn is_reasoning_notification_method(method: &str) -> bool {
    let normalized = method.to_ascii_lowercase();
    normalized.starts_with("item/reasoning/")
        || normalized.starts_with("item/reasoning")
        || normalized.starts_with("item/thinking/")
        || normalized.starts_with("item/thinking")
}

fn is_codex_response_side_notification(method: &str) -> bool {
    method.starts_with("item/")
        || method.starts_with("turn/")
        || method == "thread/tokenUsage/updated"
        || method == "model/rerouted"
        || method == "error"
}

fn is_codex_provider_tool_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "fileChange"
            | "imageGeneration"
            | "webSearch"
            | "imageView"
            | "sleep"
            | "collabToolCall"
            | "collabAgentToolCall"
            | "mcpToolCall"
            | "dynamicToolCall"
    )
}

fn is_codex_provider_output_item_type(item_type: &str) -> bool {
    matches!(item_type, "agentMessage" | "reasoning") || is_codex_provider_tool_item_type(item_type)
}

fn is_raw_codex_provider_output_item(item: &Value) -> bool {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if item_type.ends_with("_output") || item_type.ends_with("Output") {
        return false;
    }
    if item_type == "message" {
        return item.get("role").and_then(Value::as_str) != Some("user");
    }
    matches!(
        item_type,
        "reasoning"
            | "function_call"
            | "custom_tool_call"
            | "local_shell_call"
            | "shell_call"
            | "computer_call"
            | "web_search_call"
            | "image_generation_call"
    )
}

fn is_terminal_codex_error_notification(state: &CodexState, params: &Value) -> bool {
    if params.get("fatal").and_then(Value::as_bool) == Some(true)
        || params.get("terminal").and_then(Value::as_bool) == Some(true)
        || params.get("recoverable").and_then(Value::as_bool) == Some(false)
    {
        return true;
    }

    state.active_turn_id.is_none()
        && state.active_stream.is_none()
        && state.pending_request.is_none()
}

fn join_nonempty_chunks(chunks: Vec<String>) -> Option<String> {
    let normalized = chunks
        .into_iter()
        .filter(|chunk| contains_non_whitespace(chunk))
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.join("\n"))
    }
}

fn contains_non_whitespace(text: &str) -> bool {
    text.chars().any(|ch| !ch.is_whitespace())
}

fn codex_message_is_renderable(
    content: &str,
    reasoning: Option<&str>,
    declared_tool_count: usize,
    image_count: usize,
) -> bool {
    contains_non_whitespace(content)
        || reasoning.is_some_and(contains_non_whitespace)
        || declared_tool_count > 0
        || image_count > 0
}

fn codex_image_generation_prompt(item: &Value) -> Option<String> {
    item.get("revisedPrompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .map(str::to_owned)
}

fn codex_web_search_query_from_source(source: &str) -> Option<String> {
    let source = source.split_once("tools.web__run")?.1;
    let search = source.split_once("search_query")?.1;
    let encoded = search
        .split_once("\"q\":")
        .or_else(|| search.split_once("q:"))?
        .1
        .trim_start();
    serde_json::Deserializer::from_str(encoded)
        .into_iter::<String>()
        .next()?
        .ok()
        .filter(|query| !query.trim().is_empty())
}

fn codex_native_tool_completion(item_type: &str) -> Option<(&'static str, Value)> {
    match item_type {
        "webSearch" => Some((
            "web_search",
            serde_json::to_value(protocol::ToolExecutionResult::WebSearch)
                .expect("serialize Codex web search completion"),
        )),
        "imageView" => Some((
            "view_image",
            serde_json::to_value(protocol::ToolExecutionResult::ViewImage)
                .expect("serialize Codex image view completion"),
        )),
        "sleep" => Some((
            "sleep",
            serde_json::to_value(protocol::ToolExecutionResult::Sleep)
                .expect("serialize Codex sleep completion"),
        )),
        _ => None,
    }
}

fn parse_codex_generated_image(item: &Value) -> Result<ImageData, String> {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status != "completed" {
        return Err(format!(
            "Codex image generation ended with status '{}'",
            if status.is_empty() { "unknown" } else { status }
        ));
    }
    let encoded = item
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|result| !result.is_empty())
        .ok_or_else(|| "Codex image generation completed without image data".to_owned())?;
    let encoded_limit = CODEX_MAX_GENERATED_IMAGE_BYTES.saturating_mul(4) / 3 + 4;
    if encoded.len() > encoded_limit {
        return Err(format!(
            "Codex generated image exceeds the {} MiB limit",
            CODEX_MAX_GENERATED_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("Codex generated image is not valid base64: {error}"))?;
    if bytes.len() > CODEX_MAX_GENERATED_IMAGE_BYTES {
        return Err(format!(
            "Codex generated image exceeds the {} MiB limit",
            CODEX_MAX_GENERATED_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let media_type = detect_generated_image_media_type(&bytes)
        .ok_or_else(|| "Codex generated image has an unsupported file format".to_owned())?;
    Ok(ImageData {
        media_type: media_type.to_owned(),
        data: BASE64_STANDARD.encode(bytes),
    })
}

fn detect_generated_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn map_plan_status(status: &str) -> protocol::TaskStatus {
    match status {
        "completed" => protocol::TaskStatus::Completed,
        "inProgress" => protocol::TaskStatus::InProgress,
        _ => protocol::TaskStatus::Pending,
    }
}

#[derive(Debug, Clone)]
struct CodexFileChange {
    path: String,
    before: String,
    after: String,
    lines_added: u64,
    lines_removed: u64,
}

#[derive(Debug, Clone)]
struct CodexFileChangeCompletion {
    call_id: String,
    request: Option<CodexFileChange>,
    lines_added: u64,
    lines_removed: u64,
}

fn codex_file_change_call_id(item_id: &str, index: usize, total: usize) -> String {
    if total <= 1 {
        item_id.to_string()
    } else {
        format!("{item_id}#{}", index + 1)
    }
}

fn codex_file_change_completion_plan(
    item_id: &str,
    known_call_ids: &[String],
    file_changes: &[CodexFileChange],
) -> Vec<CodexFileChangeCompletion> {
    if file_changes.is_empty() {
        return known_call_ids
            .iter()
            .map(|call_id| CodexFileChangeCompletion {
                call_id: call_id.clone(),
                request: None,
                lines_added: 0,
                lines_removed: 0,
            })
            .collect();
    }

    let total = file_changes.len();
    let mut completions = Vec::with_capacity(known_call_ids.len().max(total));
    for (idx, change) in file_changes.iter().enumerate() {
        let known_call_id = known_call_ids.get(idx).cloned();
        completions.push(CodexFileChangeCompletion {
            call_id: known_call_id
                .unwrap_or_else(|| codex_file_change_call_id(item_id, idx, total)),
            request: (known_call_ids.get(idx).is_none()).then(|| change.clone()),
            lines_added: change.lines_added,
            lines_removed: change.lines_removed,
        });
    }

    completions.extend(known_call_ids.iter().skip(total).map(|call_id| {
        CodexFileChangeCompletion {
            call_id: call_id.clone(),
            request: None,
            lines_added: 0,
            lines_removed: 0,
        }
    }));

    completions
}

fn parse_codex_file_changes(item: &Value) -> Vec<CodexFileChange> {
    let Some(changes) = item.get("changes").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut parsed = Vec::new();
    for change in changes {
        let path = change
            .get("path")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                change
                    .get("kind")
                    .and_then(|k| k.get("move_path"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string();
        if path.trim().is_empty() {
            continue;
        }

        let diff = change
            .get("diff")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // `diff` only holds a diff when the change is an update. Measured
        // against codex-cli 0.146.0, whose `FileChange` kinds are exactly
        // add/delete/update: an add carries the new file's whole content
        // ("alpha\nbeta\nomega\n"), a delete carries the removed file's content,
        // and only an update carries a hunk ("@@ -1,3 +1,3 @@\n alpha\n-beta\n
        // +gamma\n omega\n"). Reading content as a diff put every line on both
        // sides, because unprefixed lines are context lines — so creating a file
        // rendered a card with no lines in it and a `+0 -0` footer, and deleting
        // one did the same.
        let kind = change
            .get("kind")
            .and_then(|kind| kind.get("type"))
            .and_then(Value::as_str);
        let (before, after, lines_added, lines_removed) = match kind {
            Some("add") => (
                String::new(),
                diff.to_owned(),
                diff.lines().count() as u64,
                0,
            ),
            Some("delete") => (
                diff.to_owned(),
                String::new(),
                0,
                diff.lines().count() as u64,
            ),
            Some("update") => parse_unified_diff_preview(diff),
            // Also the shape with no `kind` at all, which is what the history
            // and replay paths hand this function. Warned rather than dropped: a
            // fourth kind would be a Codex schema change we want to hear about,
            // and a missing card is the failure mode this whole area exists to
            // avoid.
            other => {
                if other.is_some() {
                    tracing::warn!(
                        kind = other,
                        "unknown Codex file change kind; reading its diff field as a unified diff"
                    );
                }
                parse_unified_diff_preview(diff)
            }
        };

        parsed.push(CodexFileChange {
            path,
            before,
            after,
            lines_added,
            lines_removed,
        });
    }

    parsed
}

fn parse_unified_diff_preview(diff: &str) -> (String, String, u64, u64) {
    let mut before_lines: Vec<String> = Vec::new();
    let mut after_lines: Vec<String> = Vec::new();
    let mut lines_added = 0u64;
    let mut lines_removed = 0u64;

    for line in diff.lines() {
        if line.starts_with("@@") || line.starts_with('\\') || line.is_empty() {
            continue;
        }

        if let Some(text) = line.strip_prefix('+') {
            // Skip patch file headers (`+++`) while counting actual additions.
            if !line.starts_with("+++ ") {
                after_lines.push(text.to_string());
                lines_added += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix('-') {
            // Skip patch file headers (`---`) while counting actual removals.
            if !line.starts_with("--- ") {
                before_lines.push(text.to_string());
                lines_removed += 1;
            }
            continue;
        }

        if let Some(text) = line.strip_prefix(' ') {
            before_lines.push(text.to_string());
            after_lines.push(text.to_string());
            continue;
        }

        before_lines.push(line.to_string());
        after_lines.push(line.to_string());
    }

    (
        before_lines.join("\n"),
        after_lines.join("\n"),
        lines_added,
        lines_removed,
    )
}

fn usage_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

const CODEX_TOKEN_USAGE_COUNTER_KEYS: &[&str] = &[
    "inputTokens",
    "input_tokens",
    "prompt_tokens",
    "outputTokens",
    "output_tokens",
    "completion_tokens",
    "totalTokens",
    "total_tokens",
    "cachedInputTokens",
    "cached_prompt_tokens",
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "reasoningOutputTokens",
    "reasoning_tokens",
];

fn has_numeric_token_usage_counter(value: &Value) -> bool {
    CODEX_TOKEN_USAGE_COUNTER_KEYS
        .iter()
        .any(|key| value.get(*key).and_then(Value::as_u64).is_some())
}

fn extract_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.get("turn_id").and_then(Value::as_str))
        .or_else(|| params.get("id").and_then(Value::as_str))
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turnId"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            params
                .get("turn")
                .and_then(|turn| turn.get("turn_id"))
                .and_then(Value::as_str)
        })
        .map(|id| id.to_string())
}

fn extract_turn_token_usage_value(params: &Value) -> Option<&Value> {
    params
        .get("tokenUsage")
        .or_else(|| params.get("token_usage"))
        .or_else(|| params.get("usage"))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("tokenUsage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("token_usage")))
        .or_else(|| params.get("turn").and_then(|turn| turn.get("usage")))
}

fn extract_turn_token_usage(params: &Value, model_hint: Option<&str>) -> Option<(String, Value)> {
    let turn_id = extract_turn_id(params)?;
    let usage = extract_turn_token_usage_value(params)?;
    let normalized = normalize_token_usage_with_envelope(usage, Some(params), model_hint)?;
    Some((turn_id, normalized))
}

fn extract_model_request_token_usage(
    params: &Value,
    model_hint: Option<&str>,
) -> Option<(String, TokenUsage, TokenUsage, Option<u64>)> {
    let turn_id = extract_turn_id(params)?;
    let raw = extract_turn_token_usage_value(params)?;
    let request_value = normalize_token_usage_with_envelope(raw, Some(params), model_hint)?;
    let cumulative_raw = raw
        .get("total")
        .filter(|value| value.is_object())
        .unwrap_or_else(|| {
            raw.get("last")
                .filter(|value| value.is_object())
                .unwrap_or(raw)
        });
    let cumulative_value =
        normalize_token_usage_with_envelope(cumulative_raw, Some(params), model_hint)?;
    let model_context_window = context_window_from_token_usage(raw, raw, Some(params));
    let request = serde_json::from_value(request_value).ok()?;
    let cumulative = serde_json::from_value(cumulative_value).ok()?;
    Some((turn_id, request, cumulative, model_context_window))
}

fn record_model_request_token_usage(
    usage_by_turn: &mut HashMap<String, CodexTurnTokenUsage>,
    turn_id: String,
    request: TokenUsage,
    cumulative: TokenUsage,
    model_context_window: Option<u64>,
) -> Option<ModelRequestTokenUsage> {
    let state = usage_by_turn.entry(turn_id.clone()).or_default();
    if state.cumulative.as_ref() == Some(&cumulative) {
        let context_window = model_context_window
            .filter(|context_window| Some(*context_window) != state.model_context_window)?;
        state.model_context_window = Some(context_window);
        return Some(ModelRequestTokenUsage {
            request_id: ModelRequestId {
                turn_id: ModelTurnId(turn_id),
                sequence: state.request_count,
            },
            current_context_usage: Some(current_context_usage(&request, context_window)),
            request,
            turn: state.turn.clone(),
            cumulative,
            model_context_window: state.model_context_window,
            estimated_context_breakdown: None,
        });
    }

    let sequence = state.request_count.saturating_add(1);
    state.request_count = state.request_count.saturating_add(1);
    add_token_usage(&mut state.turn, &request);
    state.latest_request = Some(request.clone());
    state.cumulative = Some(cumulative.clone());
    state.model_context_window = model_context_window.or(state.model_context_window);
    let current_context_usage = state
        .model_context_window
        .map(|context_window| current_context_usage(&request, context_window))
        .unwrap_or(CurrentContextUsage::Unknown);

    Some(ModelRequestTokenUsage {
        request_id: ModelRequestId {
            turn_id: ModelTurnId(turn_id),
            sequence,
        },
        request,
        turn: state.turn.clone(),
        cumulative,
        model_context_window: state.model_context_window,
        current_context_usage: Some(current_context_usage),
        estimated_context_breakdown: None,
    })
}

fn current_context_usage(request: &TokenUsage, context_window: u64) -> CurrentContextUsage {
    CurrentContextUsage::Known {
        input_tokens: request
            .input_tokens
            .saturating_add(request.cached_prompt_tokens.unwrap_or_default())
            .saturating_add(request.cache_creation_input_tokens.unwrap_or_default()),
        context_window,
    }
}

fn add_token_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cached_prompt_tokens =
        add_optional_token_usage(total.cached_prompt_tokens, usage.cached_prompt_tokens);
    total.cache_creation_input_tokens = add_optional_token_usage(
        total.cache_creation_input_tokens,
        usage.cache_creation_input_tokens,
    );
    total.reasoning_tokens =
        add_optional_token_usage(total.reasoning_tokens, usage.reasoning_tokens);
}

fn normalize_subagent_cumulative_usage(
    baseline: &mut Option<TokenUsage>,
    request: &TokenUsage,
    provider_cumulative: TokenUsage,
) -> TokenUsage {
    let Some(baseline) = baseline.as_ref() else {
        *baseline = Some(subtract_token_usage(&provider_cumulative, request));
        return request.clone();
    };
    subtract_token_usage(&provider_cumulative, baseline)
}

fn subtract_token_usage(total: &TokenUsage, baseline: &TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: total.input_tokens.saturating_sub(baseline.input_tokens),
        output_tokens: total.output_tokens.saturating_sub(baseline.output_tokens),
        total_tokens: total.total_tokens.saturating_sub(baseline.total_tokens),
        cached_prompt_tokens: subtract_optional_token_usage(
            total.cached_prompt_tokens,
            baseline.cached_prompt_tokens,
        ),
        cache_creation_input_tokens: subtract_optional_token_usage(
            total.cache_creation_input_tokens,
            baseline.cache_creation_input_tokens,
        ),
        reasoning_tokens: subtract_optional_token_usage(
            total.reasoning_tokens,
            baseline.reasoning_tokens,
        ),
    }
}

fn subtract_optional_token_usage(total: Option<u64>, baseline: Option<u64>) -> Option<u64> {
    total.map(|total| total.saturating_sub(baseline.unwrap_or(0)))
}

fn add_optional_token_usage(total: Option<u64>, usage: Option<u64>) -> Option<u64> {
    match (total, usage) {
        (None, None) => None,
        (total, usage) => Some(total.unwrap_or(0).saturating_add(usage.unwrap_or(0))),
    }
}

fn normalize_token_usage_with_envelope(
    raw: &Value,
    envelope: Option<&Value>,
    model_hint: Option<&str>,
) -> Option<Value> {
    let source = raw
        .get("last")
        .filter(|value| value.is_object())
        .unwrap_or(raw);
    if !has_numeric_token_usage_counter(source) {
        return None;
    }

    // OpenAI convention: `inputTokens` is the TOTAL including cached tokens,
    // and `cachedInputTokens` is a subset.  Our internal contract (matching
    // Anthropic) expects `input_tokens` to be the non-cached portion only,
    // with cache fields as separate additive values.
    let cached_prompt_tokens =
        usage_u64(source, &["cachedInputTokens", "cached_prompt_tokens"]).unwrap_or(0);
    let cache_creation_input_tokens = usage_u64(
        source,
        &["cacheCreationInputTokens", "cache_creation_input_tokens"],
    )
    .unwrap_or(0);
    let raw_input_tokens = usage_u64(source, &["inputTokens"]).unwrap_or(0);
    let input_tokens = if source.get("inputTokens").is_some() {
        raw_input_tokens
            .saturating_sub(cached_prompt_tokens)
            .saturating_sub(cache_creation_input_tokens)
    } else {
        usage_u64(source, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0)
    };
    let prompt_tokens_total = if raw_input_tokens > 0 {
        raw_input_tokens
    } else {
        input_tokens
            .saturating_add(cached_prompt_tokens)
            .saturating_add(cache_creation_input_tokens)
    };

    // OpenAI convention: `outputTokens` includes reasoning.  Our contract
    // treats `reasoning_tokens` as an informational subset of `output_tokens`,
    // so `output_tokens` is stored as-is (already includes reasoning).
    let output_tokens = usage_u64(
        source,
        &["outputTokens", "output_tokens", "completion_tokens"],
    )
    .unwrap_or(0);
    let reasoning_tokens =
        usage_u64(source, &["reasoningOutputTokens", "reasoning_tokens"]).unwrap_or(0);

    // total_tokens = input_tokens + output_tokens (no double-counting).
    let total_tokens =
        usage_u64(source, &["totalTokens", "total_tokens"]).unwrap_or(input_tokens + output_tokens);
    let context_window = context_window_from_token_usage(raw, source, envelope)
        .filter(|window| *window > 0)
        .unwrap_or_else(|| {
            let model_estimate = codex_estimated_context_window_for_model(model_hint);
            std::cmp::max(model_estimate, prompt_tokens_total.max(1))
        });

    Some(json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "cached_prompt_tokens": cached_prompt_tokens,
        "cache_creation_input_tokens": cache_creation_input_tokens,
        "reasoning_tokens": reasoning_tokens,
        "context_window": context_window
    }))
}

fn context_window_from_token_usage(
    raw: &Value,
    last: &Value,
    envelope: Option<&Value>,
) -> Option<u64> {
    const WINDOW_KEYS: &[&str] = &[
        "modelContextWindow",
        "model_context_window",
        "contextWindow",
        "context_window",
        "maxInputTokens",
        "max_input_tokens",
        "maxTokens",
        "max_tokens",
        "maxPromptTokens",
        "max_prompt_tokens",
    ];

    find_context_window_in_value(raw, WINDOW_KEYS, 2)
        .or_else(|| find_context_window_in_value(last, WINDOW_KEYS, 2))
        .or_else(|| envelope.and_then(|value| find_context_window_in_value(value, WINDOW_KEYS, 4)))
}

fn find_context_window_in_value(value: &Value, keys: &[&str], depth: usize) -> Option<u64> {
    if depth == 0 {
        return None;
    }

    if let Some(obj) = value.as_object() {
        for key in keys {
            if let Some(window) = obj.get(*key).and_then(Value::as_u64).filter(|w| *w > 0) {
                return Some(window);
            }
        }
        for nested in obj.values() {
            if let Some(window) = find_context_window_in_value(nested, keys, depth - 1) {
                return Some(window);
            }
        }
        return None;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(window) = find_context_window_in_value(item, keys, depth - 1) {
                return Some(window);
            }
        }
    }

    None
}

fn codex_estimated_context_window_for_model(model_hint: Option<&str>) -> u64 {
    let Some(model) = model_hint else {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    };
    let normalized = model.trim().to_ascii_lowercase();
    // `codex-mini-latest` is the one GPT-5-era model with a 200k window, so it
    // must be checked before the broader gpt-5 family match below.
    if normalized.contains("codex-mini") {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT;
    }
    // Match the whole gpt-5 family by substring so this stays correct across
    // version bumps, `-codex`/`-mini` suffixes, and provider prefixes (the CLI
    // now reports ids like `openai.gpt-5.5`).
    if normalized.contains("gpt-5") {
        return CODEX_ESTIMATED_CONTEXT_WINDOW_GPT5_FAMILY;
    }
    CODEX_ESTIMATED_CONTEXT_WINDOW_DEFAULT
}

fn codex_process_id(item: &Value) -> Option<String> {
    item.get("processId")
        .or_else(|| item.get("process_id"))
        .and_then(normalize_codex_process_id)
}

fn normalize_codex_process_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn codex_yielded_session_ids(params: &Value) -> Vec<String> {
    let Some(item) = params.get("item") else {
        return Vec::new();
    };
    if item.get("type").and_then(Value::as_str) != Some("custom_tool_call_output") {
        return Vec::new();
    }
    item.get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|entry| entry.get("text").and_then(Value::as_str))
        .filter_map(codex_yielded_session_id_from_text)
        .fold(Vec::new(), |mut session_ids, session_id| {
            if !session_ids.contains(&session_id) {
                session_ids.push(session_id);
            }
            session_ids
        })
}

fn codex_yielded_session_id_from_text(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|value| {
            value
                .get("session_id")
                .or_else(|| value.get("sessionId"))
                .and_then(normalize_codex_process_id)
        })
        .or_else(|| {
            text.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("SESSION_ID=")
                    .map(str::trim)
                    .filter(|session_id| !session_id.is_empty())
                    .map(str::to_owned)
            })
        })
}

/// The two shapes Codex reports a raw tool's result in.
///
/// A `custom_tool_call` is answered by `custom_tool_call_output`, a
/// `function_call` by `function_call_output`. Only the first was ever
/// recognised, so a `function_call`'s result was dropped outright.
///
/// Shell commands survived that because they finish a second way — their
/// `commandExecution` item completes the card independently. Tools with no
/// command behind them have no second route: `write_stdin`, which polls a
/// running process, was declared and then never completed, and the idle sweep
/// cancelled it at turn end.
fn is_raw_codex_tool_output_item_type(item_type: Option<&str>) -> bool {
    matches!(
        item_type,
        Some("custom_tool_call_output" | "function_call_output")
    )
}

fn raw_custom_tool_output_text(item: &Value) -> String {
    if let Some(output) = item.get("output").and_then(Value::as_str) {
        return output.to_owned();
    }
    item.get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn parse_raw_codex_apply_patch(input: &str) -> Option<(String, String, String)> {
    let decoded = input.replace("\\n", "\n");
    let patch = decoded
        .split_once("*** Begin Patch")?
        .1
        .split_once("*** End Patch")?
        .0;
    let file_path = patch
        .lines()
        .find_map(|line| line.strip_prefix("*** Update File: "))?
        .trim()
        .to_owned();
    let before = patch
        .lines()
        .filter_map(|line| line.strip_prefix('-').filter(|_| !line.starts_with("---")))
        .collect::<Vec<_>>()
        .join("\n");
    let after = patch
        .lines()
        .filter_map(|line| line.strip_prefix('+').filter(|_| !line.starts_with("+++")))
        .collect::<Vec<_>>()
        .join("\n");
    Some((file_path, before, after))
}

fn codex_modify_paths_match(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left == right || left.ends_with(right) || right.ends_with(left)
}

fn take_codex_raw_contract_drift_warning(
    state: &mut CodexState,
) -> Option<CodexRawContractDriftWarning> {
    if !state.experimental_raw_events_requested
        || state.raw_response_item_completed_seen
        || state.raw_contract_drift_warned
    {
        return None;
    }
    let thread_id = state.first_background_list_thread_id.clone()?;
    state.raw_contract_drift_warned = true;
    let mut observed_notification_methods = state
        .observed_notification_methods
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    observed_notification_methods.sort();
    Some(CodexRawContractDriftWarning {
        thread_id,
        observed_notification_methods,
        methods_truncated: state.raw_notification_methods_truncated,
    })
}

fn take_codex_commands_for_turn(
    state: &mut CodexState,
    thread_id: &str,
    turn_id: &str,
) -> Vec<CodexBackgroundCommand> {
    state
        .unowned_command_executions
        .retain(|(owner_thread_id, _), command| {
            owner_thread_id != thread_id || command.turn_id != turn_id
        });
    let keys = state
        .outstanding_command_executions
        .iter()
        .filter(|((owner_thread_id, _), command)| {
            owner_thread_id == thread_id && command.turn_id == turn_id
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut commands = Vec::new();
    for key in keys {
        if thread_id == state.thread_id && state.background_commands.contains_key(&key) {
            continue;
        }
        state.outstanding_command_executions.remove(&key);
        if let Some(command) = state.background_commands.remove(&key) {
            commands.push(command);
        }
    }
    commands
}

fn take_codex_commands_for_thread(
    state: &mut CodexState,
    thread_id: &str,
) -> Vec<CodexCommandTermination> {
    state
        .unowned_command_executions
        .retain(|(owner_thread_id, _), _| owner_thread_id != thread_id);
    let keys = state
        .outstanding_command_executions
        .keys()
        .filter(|(owner_thread_id, _)| owner_thread_id == thread_id)
        .cloned()
        .collect::<Vec<_>>();
    let mut commands = Vec::new();
    for key in keys {
        let Some(outstanding) = state.outstanding_command_executions.remove(&key) else {
            continue;
        };
        commands.push(CodexCommandTermination {
            thread_id: thread_id.to_owned(),
            tool_call_id: outstanding.tool_call_id,
        });
        state.background_commands.remove(&key);
    }
    let orphaned = state
        .background_commands
        .keys()
        .filter(|(owner_thread_id, _)| owner_thread_id == thread_id)
        .cloned()
        .collect::<Vec<_>>();
    for key in orphaned {
        if let Some(command) = state.background_commands.remove(&key) {
            commands.push(CodexCommandTermination {
                thread_id: thread_id.to_owned(),
                tool_call_id: command.tool_call_id,
            });
        }
    }
    commands
}

fn take_all_codex_commands(state: &mut CodexState) -> Vec<CodexCommandTermination> {
    let thread_ids = state
        .outstanding_command_executions
        .keys()
        .chain(state.background_commands.keys())
        .map(|(thread_id, _)| thread_id.clone())
        .collect::<HashSet<_>>();
    state.unowned_command_executions.clear();
    thread_ids
        .into_iter()
        .flat_map(|thread_id| take_codex_commands_for_thread(state, &thread_id))
        .collect()
}

/// Fold one `thread/backgroundTerminals/list` snapshot into the tracked set,
/// returning the progress updates that snapshot implies.
///
/// Root rows are liveness-only after a matched yielded-session result promotes
/// the command. Native-child rows retain list-based promotion until current
/// child raw-event semantics are captured. Absence from a list snapshot is not
/// terminal evidence; only the correlated provider completion closes a command.
fn reconcile_codex_background_terminals(
    state: &mut CodexState,
    thread_id: &str,
    listed: &[CodexBackgroundTerminalRow],
) -> (Vec<ToolProgressData>, Vec<String>) {
    if !state.background_command_owner_active {
        return (Vec::new(), Vec::new());
    }
    let mut progress = Vec::new();
    let cancelled = Vec::new();
    for row in listed {
        let key = (thread_id.to_owned(), row.item_id.clone());
        if state.background_commands.contains_key(&key) {
            continue;
        }
        if thread_id == state.thread_id {
            continue;
        }
        // The list is keyed by provider item id; without the matching
        // `item/started` there is no card to attach progress to.
        let Some(outstanding) = state.outstanding_command_executions.get(&key) else {
            tracing::debug!(
                "Codex reported a background terminal for unknown command execution '{}'",
                row.item_id
            );
            continue;
        };
        let command = CodexBackgroundCommand {
            tool_call_id: outstanding.tool_call_id.clone(),
            task_id: row.process_id.clone(),
            description: row.command.clone().or_else(|| outstanding.command.clone()),
        };
        progress.push(command.progress());
        state.background_commands.insert(key, command);
    }
    (progress, cancelled)
}

fn codex_command_text(item: &Value) -> Option<String> {
    item.get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(str::to_owned)
}

/// Rows of a `thread/backgroundTerminals/list` result.
///
/// The latest CLI lists both foreground and yielded unified-exec processes.
/// Root classification therefore comes from the matched yielded-session raw
/// result; this list only reconciles liveness after promotion.
fn parse_codex_background_terminals(result: &Value) -> Vec<CodexBackgroundTerminalRow> {
    let Some(rows) = result.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let item_id = row
                .get("itemId")
                .or_else(|| row.get("item_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|item_id| !item_id.is_empty())?;
            let process_id = row
                .get("processId")
                .or_else(|| row.get("process_id"))
                .and_then(normalize_codex_process_id)
                .filter(|value| !value.trim().is_empty())?;
            Some(CodexBackgroundTerminalRow {
                item_id: item_id.to_owned(),
                process_id,
                command: codex_command_text(row),
            })
        })
        .collect()
}

fn codex_background_wake_notification(wakes: &[CodexBackgroundWake]) -> String {
    let mut notification = String::from(
        "[Tyde internal background-task notification]\n\
         Background work from an earlier turn has finished. Continue the work that was waiting \
         for these results. Do not describe this notification as a new user request.\n",
    );
    for wake in wakes {
        let output = wake.output.chars().take(16_384).collect::<String>();
        notification.push_str(&format!(
            "\nTool call: {}\nTask: {}\nCommand: {}\nExit code: {}\nOutput:\n{}\n",
            wake.tool_call_id,
            wake.task_id,
            wake.description.as_deref().unwrap_or("unknown command"),
            wake.exit_code,
            output,
        ));
    }
    notification
}

fn codex_mcp_elicitation_result(params: &Value) -> Value {
    let server_name = params
        .get("serverName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let approval_kind = params
        .get("_meta")
        .and_then(|meta| meta.get("codex_approval_kind"))
        .and_then(Value::as_str);
    if approval_kind == Some("mcp_tool_call")
        && matches!(
            server_name,
            "tyde-debug"
                | "tyde-agent-control"
                | AGENT_CONTROL_AWAIT_MCP_SERVER_NAME
                | REVIEW_FEEDBACK_MCP_SERVER_NAME
        )
    {
        return json!({
            "action": "accept",
            "content": {}
        });
    }

    json!({
        "action": "cancel"
    })
}

fn parse_approval_decision(message: &str) -> &'static str {
    let normalized = message.trim().to_ascii_lowercase();
    if normalized.starts_with("cancel") {
        return "cancel";
    }
    if normalized.contains("decline")
        || normalized.contains("deny")
        || normalized == "no"
        || normalized == "n"
    {
        return "decline";
    }
    if normalized.contains("always") || normalized.contains("for session") {
        return "acceptForSession";
    }
    "accept"
}

fn is_codex_tool_server_request(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "execCommandApproval"
            | "applyPatchApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request"
            | "item/tool/call"
    )
}

fn parse_review_decision(message: &str) -> &'static str {
    match parse_approval_decision(message) {
        "accept" => "approved",
        "acceptForSession" => "approved_for_session",
        "decline" => "denied",
        "cancel" => "abort",
        _ => "approved",
    }
}

fn codex_has_http_mcp_servers(startup_mcp_servers: &[StartupMcpServer]) -> bool {
    startup_mcp_servers.iter().any(|server| {
        matches!(
            server.transport,
            StartupMcpTransport::Http {
                url: _,
                headers: _,
                bearer_token_env_var: _,
            }
        )
    })
}

fn codex_sandbox_mode(
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
) -> &'static str {
    if execution_mode == BackendExecutionMode::InferenceOnly {
        return CODEX_INFERENCE_SANDBOX;
    }
    match access_mode {
        BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => CODEX_UNRESTRICTED_SANDBOX,
    }
}

fn codex_approval_policy(execution_mode: BackendExecutionMode) -> &'static str {
    match execution_mode {
        BackendExecutionMode::Agent => CODEX_FORCED_APPROVAL_POLICY,
        BackendExecutionMode::InferenceOnly => CODEX_INFERENCE_APPROVAL_POLICY,
    }
}

fn codex_danger_full_access_sandbox_policy(_network_access: bool) -> Value {
    json!({ "type": "dangerFullAccess" })
}

fn codex_inference_sandbox_policy() -> Value {
    json!({
        "type": "readOnly",
        "networkAccess": false,
    })
}

fn codex_sandbox_policy(
    access_mode: BackendAccessMode,
    network_access: bool,
    execution_mode: BackendExecutionMode,
) -> Value {
    if execution_mode == BackendExecutionMode::InferenceOnly {
        return codex_inference_sandbox_policy();
    }
    match access_mode {
        BackendAccessMode::Unrestricted | BackendAccessMode::ReadOnly => {
            codex_danger_full_access_sandbox_policy(network_access)
        }
    }
}

fn codex_inference_config_overrides() -> Vec<String> {
    [
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.js_repl=false",
        "features.code_mode=false",
        "features.code_mode_host=false",
        "features.code_mode_only=false",
        "features.multi_agent=false",
        "features.multi_agent_v2=false",
        "features.multi_agent_mode=false",
        "features.web_search_request=false",
        "features.web_search_cached=false",
        "features.standalone_web_search=false",
        "features.search_tool=false",
        "features.image_generation=false",
        "features.apps=false",
        "features.enable_mcp_apps=false",
        "features.tool_search=false",
        "features.plugins=false",
        "features.tool_suggest=false",
        "features.request_permissions_tool=false",
        "features.default_mode_request_user_input=false",
        "features.in_app_browser=false",
        "features.browser_use=false",
        "features.browser_use_full_cdp_access=false",
        "features.browser_use_external=false",
        "features.computer_use=false",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

async fn codex_inference_thread_config(rpc: &CodexRpc, cwd: &str) -> Result<Value, String> {
    let effective_config = rpc
        .request(
            "config/read",
            json!({
                "includeLayers": false,
                "cwd": cwd,
            }),
        )
        .await?;
    let mcp_servers = effective_config
        .pointer("/config/mcp_servers")
        .and_then(Value::as_object)
        .ok_or("Codex config/read response missing config.mcp_servers")?;
    let disabled_mcp_servers = mcp_servers
        .keys()
        .map(|name| (name.clone(), json!({ "enabled": false })))
        .collect::<serde_json::Map<_, _>>();
    Ok(json!({
        "mcp_servers": disabled_mcp_servers,
        "notify": [],
    }))
}

fn codex_app_server_args(
    access_mode: BackendAccessMode,
    execution_mode: BackendExecutionMode,
    config_overrides: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--sandbox".to_string(),
        codex_sandbox_mode(access_mode, execution_mode).to_string(),
        "app-server".to_string(),
        "--listen".to_string(),
        "stdio://".to_string(),
    ];
    for override_key_value in config_overrides {
        args.push("-c".to_string());
        args.push(override_key_value.clone());
    }
    args
}

fn normalize_reasoning_effort(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let value = match normalized.as_str() {
        "off" => "none",
        "min" => "minimal",
        "med" => "medium",
        _ => normalized.as_str(),
    };
    Some(value.to_string())
}

fn codex_thread_settings_update_params(thread_id: &str, settings: &Value) -> Result<Value, String> {
    let settings = settings
        .as_object()
        .ok_or_else(|| "Codex runtime settings must be an object".to_owned())?;
    let mut params =
        serde_json::Map::from_iter([("threadId".to_owned(), Value::String(thread_id.to_owned()))]);
    if let Some(model) = settings.get("model") {
        params.insert("model".to_owned(), model.clone());
    }
    if let Some(effort) = settings
        .get("reasoning_effort")
        .or_else(|| settings.get("reasoningEffort"))
    {
        params.insert("effort".to_owned(), effort.clone());
    }
    if let Some(approval_policy) = settings
        .get("approval_policy")
        .or_else(|| settings.get("approvalPolicy"))
    {
        params.insert("approvalPolicy".to_owned(), approval_policy.clone());
    }
    Ok(Value::Object(params))
}

fn pick_workspace_root(workspace_roots: &[String]) -> Result<String, String> {
    if let Some(root) = workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.trim_start().starts_with("ssh://"))
        .cloned()
    {
        return Ok(root);
    }
    if workspace_roots
        .iter()
        .any(|root| !root.trim().is_empty() && root.trim_start().starts_with("ssh://"))
    {
        return Err("Codex backend requires at least one local workspace root".to_string());
    }
    crate::backend::tyde_owned_no_root_cwd("codex")
}

fn codex_runtime_workspace_roots(workspace_roots: &[String], cwd: &str) -> Vec<String> {
    let mut roots = workspace_roots
        .iter()
        .filter_map(|root| {
            let trimmed = root.trim();
            (!trimmed.is_empty() && !trimmed.starts_with("ssh://")).then(|| root.clone())
        })
        .collect::<Vec<_>>();

    if roots.is_empty() {
        if workspace_roots.iter().any(|root| !root.trim().is_empty()) {
            roots.push(cwd.to_string());
        }
    } else if !roots.iter().any(|root| root == cwd) {
        roots.insert(0, cwd.to_string());
    }

    roots
}

async fn persist_temp_image(image: &ImageAttachment) -> Result<String, String> {
    static IMAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

    let bytes = BASE64_STANDARD
        .decode(image.data.trim())
        .map_err(|e| format!("Failed to decode image attachment '{}': {e}", image.name))?;

    let ext = media_type_to_extension(&image.media_type);
    let id = IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts_ms = unix_now_ms();

    let dir = std::env::temp_dir().join("tyde-codex-images");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create temp image directory: {e}"))?;

    let file_name = format!("{}_{}_{}.{}", sanitize_name(&image.name), ts_ms, id, ext);
    let path = dir.join(file_name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| format!("Failed to write temp image file: {e}"))?;

    Ok(path.to_string_lossy().to_string())
}

fn sanitize_name(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if cleaned.is_empty() {
        "image".to_string()
    } else {
        cleaned
    }
}

fn media_type_to_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "png",
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[derive(Clone)]
enum CodexInbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Stderr(String),
    RolloutTrace(CodexRolloutTraceEvent),
    RolloutTraceError(String),
    Closed {
        exit_code: Option<i32>,
    },
}

#[derive(Clone)]
enum CodexRolloutTraceEvent {
    ToolStarted {
        owner: CodexCodeCellKey,
        tool_call_id: String,
    },
    ToolEnded {
        tool_call_id: String,
    },
    CodeCellEnded {
        owner: CodexCodeCellKey,
    },
}

#[derive(Default)]
struct CodexRolloutTraceCursor {
    offset: u64,
    buffered: Vec<u8>,
}

fn codex_rollout_trace_event(line: &[u8]) -> Result<Option<CodexRolloutTraceEvent>, String> {
    let record: Value =
        serde_json::from_slice(line).map_err(|error| format!("invalid JSON record: {error}"))?;
    if record.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(format!(
            "unsupported schema_version {:?}",
            record.get("schema_version")
        ));
    }
    let Some(payload) = record.get("payload") else {
        return Ok(None);
    };
    let Some(event_type) = payload.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    match event_type {
        "tool_call_started"
            if payload
                .get("requester")
                .and_then(|requester| requester.get("type"))
                .and_then(Value::as_str)
                == Some("code_cell") =>
        {
            let owner = codex_rollout_code_cell_key(&record, payload)?;
            let tool_call_id = payload
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or("tool_call_started record missing tool_call_id")?
                .to_owned();
            Ok(Some(CodexRolloutTraceEvent::ToolStarted {
                owner,
                tool_call_id,
            }))
        }
        "tool_call_ended" => {
            let tool_call_id = payload
                .get("tool_call_id")
                .and_then(Value::as_str)
                .ok_or("tool_call_ended record missing tool_call_id")?
                .to_owned();
            Ok(Some(CodexRolloutTraceEvent::ToolEnded { tool_call_id }))
        }
        "code_cell_ended" => Ok(Some(CodexRolloutTraceEvent::CodeCellEnded {
            owner: codex_rollout_code_cell_key(&record, payload)?,
        })),
        _ => Ok(None),
    }
}

fn codex_rollout_code_cell_key(
    record: &Value,
    payload: &Value,
) -> Result<CodexCodeCellKey, String> {
    let runtime_cell_id = payload
        .get("runtime_cell_id")
        .or_else(|| {
            payload
                .get("requester")
                .and_then(|requester| requester.get("runtime_cell_id"))
        })
        .and_then(Value::as_str)
        .ok_or("rollout trace record missing runtime_cell_id")?;
    Ok(CodexCodeCellKey {
        thread_id: record
            .get("thread_id")
            .and_then(Value::as_str)
            .ok_or("rollout trace record missing thread_id")?
            .to_owned(),
        turn_id: record
            .get("codex_turn_id")
            .and_then(Value::as_str)
            .ok_or("rollout trace record missing codex_turn_id")?
            .to_owned(),
        runtime_cell_id: runtime_cell_id.to_owned(),
    })
}

async fn forward_codex_rollout_trace(root: PathBuf, inbound: mpsc::UnboundedSender<CodexInbound>) {
    let mut cursors = HashMap::<PathBuf, CodexRolloutTraceCursor>::new();
    loop {
        if let Err(error) = read_codex_rollout_trace(&root, &inbound, &mut cursors).await {
            let _ = inbound.send(CodexInbound::RolloutTraceError(error));
            return;
        }
        tokio::time::sleep(CODEX_ROLLOUT_TRACE_POLL_INTERVAL).await;
    }
}

async fn read_codex_rollout_trace(
    root: &Path,
    inbound: &mpsc::UnboundedSender<CodexInbound>,
    cursors: &mut HashMap<PathBuf, CodexRolloutTraceCursor>,
) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(root)
        .await
        .map_err(|error| format!("failed to read {}: {error}", root.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| format!("failed to enumerate {}: {error}", root.display()))?
    {
        let trace_path = entry.path().join("trace.jsonl");
        let Ok(metadata) = tokio::fs::metadata(&trace_path).await else {
            continue;
        };
        let cursor = cursors.entry(trace_path.clone()).or_default();
        if metadata.len() < cursor.offset {
            return Err(format!(
                "rollout trace was truncated after Tyde read it: {}",
                trace_path.display()
            ));
        }
        if metadata.len() == cursor.offset {
            continue;
        }
        let mut file = tokio::fs::File::open(&trace_path)
            .await
            .map_err(|error| format!("failed to open {}: {error}", trace_path.display()))?;
        file.seek(SeekFrom::Start(cursor.offset))
            .await
            .map_err(|error| format!("failed to seek {}: {error}", trace_path.display()))?;
        let bytes_read = file
            .read_to_end(&mut cursor.buffered)
            .await
            .map_err(|error| format!("failed to read {}: {error}", trace_path.display()))?;
        cursor.offset = cursor.offset.saturating_add(bytes_read as u64);
        while let Some(newline) = cursor.buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = cursor.buffered.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.is_empty() {
                continue;
            }
            if let Some(event) = codex_rollout_trace_event(&line)?
                && inbound.send(CodexInbound::RolloutTrace(event)).is_err()
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn codex_nested_generic_tool_notification(
    inbound: &CodexInbound,
) -> Option<(&str, &Value, &str, &str)> {
    let CodexInbound::Notification { method, params } = inbound else {
        return None;
    };
    let item = params.get("item")?;
    let item_type = item.get("type")?.as_str()?;
    if !matches!(item_type, "mcpToolCall" | "dynamicToolCall") {
        return None;
    }
    let item_id = item.get("id")?.as_str()?;
    Some((method, params, item_type, item_id))
}

struct CodexNestedGenericBatch {
    starts_remaining: usize,
    completions_remaining: usize,
    started_ids: HashSet<String>,
    completed_ids: HashSet<String>,
    pending_completions: Vec<CodexInbound>,
}

impl CodexNestedGenericBatch {
    fn new(call_count: usize) -> Self {
        Self {
            starts_remaining: call_count,
            completions_remaining: call_count,
            started_ids: HashSet::new(),
            completed_ids: HashSet::new(),
            pending_completions: Vec::new(),
        }
    }

    fn observe_start(&mut self, item_id: &str) {
        assert!(
            self.started_ids.insert(item_id.to_owned()),
            "Codex duplicated nested batch request admission {item_id}"
        );
        self.starts_remaining = self
            .starts_remaining
            .checked_sub(1)
            .expect("Codex admitted more nested requests than the raw batch declared");
    }

    fn observe_completion(&mut self, item_id: &str) {
        assert!(
            self.completed_ids.insert(item_id.to_owned()),
            "Codex duplicated nested batch completion {item_id}"
        );
        self.completions_remaining = self
            .completions_remaining
            .checked_sub(1)
            .expect("Codex completed more nested requests than the raw batch declared");
    }
}

fn codex_raw_nested_batch_declaration(inbound: &CodexInbound) -> Option<(String, usize)> {
    let CodexInbound::Notification { method, params } = inbound else {
        return None;
    };
    if method != "rawResponseItem/completed" {
        return None;
    }
    let item = params.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
        return None;
    }
    let input = item.get("input").and_then(Value::as_str)?;
    if !input.contains("Promise.all") {
        return None;
    }
    let call_count = input.match_indices("tools.mcp__").count();
    if call_count < 2 {
        return None;
    }
    Some((extract_turn_id(params)?, call_count))
}

fn toml_quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

const CODEX_AGENT_AWAIT_TOOL_TIMEOUT_SECS: u64 = 315_576_000;

fn codex_mcp_config_overrides(
    startup_mcp_servers: &[StartupMcpServer],
    tyde_loopback_reachable: bool,
) -> Vec<String> {
    let mut overrides = Vec::new();

    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let base = format!("mcp_servers.{name}");
        if tyde_loopback_reachable
            && matches!(
                name,
                AGENT_CONTROL_MCP_SERVER_NAME | AGENT_CONTROL_AWAIT_MCP_SERVER_NAME
            )
        {
            // These are load-bearing Tyde conversation-control tools. Codex
            // gives optional uncached MCP servers only a short startup grace
            // before capturing the immutable tool binding for a turn.
            overrides.push(format!("{base}.required=true"));
        }
        if name == AGENT_CONTROL_AWAIT_MCP_SERVER_NAME {
            // Codex otherwise applies its 300-second default to a tool whose
            // contract is to wait until an agent changes state.
            overrides.push(format!(
                "{base}.tool_timeout_sec={CODEX_AGENT_AWAIT_TOOL_TIMEOUT_SECS}"
            ));
        }
        match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
                ..
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.url={}", toml_quoted(trimmed_url)));
                for (key, value) in headers {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.http_headers.{key}={}", toml_quoted(value)));
                }
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    overrides.push(format!(
                        "{base}.bearer_token_env_var={}",
                        toml_quoted(env_var)
                    ));
                }
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                overrides.push(format!("{base}.command={}", toml_quoted(trimmed_command)));
                if !args.is_empty() {
                    let args_literal =
                        serde_json::to_string(args).unwrap_or_else(|_| "[]".to_string());
                    overrides.push(format!("{base}.args={args_literal}"));
                }
                for (key, value) in env {
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    overrides.push(format!("{base}.env.{key}={}", toml_quoted(value)));
                }
            }
        }
    }

    overrides
}

type PendingRpcMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CodexRpcError>>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRpcError {
    code: Option<i64>,
    message: String,
}

impl CodexRpcError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    fn method_unavailable(&self, method: &str) -> bool {
        if self.code == Some(-32601) {
            return true;
        }
        if self.code != Some(-32600) {
            return false;
        }
        [
            format!("unknown variant `{method}`"),
            format!("unknown variant '{method}'"),
            format!("unknown variant \"{method}\""),
        ]
        .iter()
        .any(|quoted| self.message.contains(quoted))
    }

    fn rejected_before_dispatch(&self, method: &str) -> bool {
        self.method_unavailable(method)
            || (self.code == Some(-32600)
                && self.message.trim_start().starts_with("Invalid request:"))
    }

    fn no_active_turn_to_interrupt(&self) -> bool {
        self.code == Some(-32600)
            && self
                .message
                .trim()
                .eq_ignore_ascii_case("no active turn to interrupt")
    }
}

impl std::fmt::Display for CodexRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "Codex JSON-RPC error {code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

fn codex_rpc_error(err_obj: &Value) -> CodexRpcError {
    let message = err_obj
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| err_obj.to_string());
    CodexRpcError {
        code: err_obj.get("code").and_then(Value::as_i64),
        message,
    }
}

fn cache_codex_manual_trigger_absent(
    stored: &std::sync::Mutex<BackendCompactionCapability>,
    previous: &BackendCompactionCapability,
    method: &str,
    error: &CodexRpcError,
) -> bool {
    if !error.method_unavailable(method) {
        return false;
    }
    *stored
        .lock()
        .expect("Codex compaction capability mutex poisoned") =
        BackendCompactionCapability::context_unavailable_with_metadata(
            BackendCompactionUnavailableReason::ManualTriggerAbsent,
            previous.provider_version.clone(),
            previous.evidence.clone(),
        );
    true
}

struct CodexRpc {
    /// `None` once teardown has closed it. Closing stdin is how we ask the
    /// app-server to leave, so the handle has to be droppable, not just
    /// writable.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pending: PendingRpcMap,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<AsyncGroupChild>>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    rollout_trace_task: Option<JoinHandle<()>>,
    rollout_trace_root: Option<tempfile::TempDir>,
    compaction_capability: Arc<std::sync::Mutex<BackendCompactionCapability>>,
}

impl CodexRpc {
    fn abort_readers(&self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        if let Some(task) = &self.rollout_trace_task {
            task.abort();
        }
        if let Some(root) = &self.rollout_trace_root {
            tracing::debug!(
                path = %root.path().display(),
                "discarding private Codex rollout trace"
            );
        }
    }

    async fn spawn(
        ssh_host: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_tempfile: Option<&std::path::Path>,
        access_mode: BackendAccessMode,
        execution_mode: BackendExecutionMode,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CodexInbound>), String> {
        Self::spawn_with_local_program(
            ssh_host,
            startup_mcp_servers,
            steering_tempfile,
            access_mode,
            execution_mode,
            None,
        )
        .await
    }

    async fn spawn_with_local_program(
        ssh_host: Option<&str>,
        startup_mcp_servers: &[StartupMcpServer],
        steering_tempfile: Option<&std::path::Path>,
        access_mode: BackendAccessMode,
        execution_mode: BackendExecutionMode,
        local_program: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<CodexInbound>), String> {
        if execution_mode == BackendExecutionMode::InferenceOnly && ssh_host.is_some() {
            return Err("Codex transient inference requires a local Codex process".to_owned());
        }
        let mut config_overrides =
            codex_mcp_config_overrides(startup_mcp_servers, ssh_host.is_none());
        if execution_mode == BackendExecutionMode::Agent {
            config_overrides.push("features.multi_agent_v2=true".to_owned());
            config_overrides.push("tools.update_plan.enabled=true".to_owned());
        }
        if execution_mode == BackendExecutionMode::InferenceOnly {
            config_overrides.extend(codex_inference_config_overrides());
        }
        if let Some(path) = steering_tempfile {
            config_overrides.push(format!(
                "model_instructions_file={}",
                toml_quoted(&path.display().to_string())
            ));
        }
        let rollout_trace_root = if ssh_host.is_none()
            && execution_mode == BackendExecutionMode::Agent
        {
            let root = tempfile::Builder::new()
                .prefix(CODEX_ROLLOUT_TRACE_ROOT_PREFIX)
                .tempdir()
                .map_err(|error| format!("Failed to create Codex rollout trace root: {error}"))?;
            restrict_codex_directory(root.path())?;
            Some(root)
        } else {
            None
        };
        let mut child = if let Some(host) = ssh_host {
            let remote_args = codex_app_server_args(access_mode, execution_mode, &config_overrides);
            crate::remote::spawn_remote_process(host, "codex", &remote_args, None).await?
        } else {
            let mut cmd = match local_program {
                Some(program) => Command::new(program),
                None => codex_command(),
            };
            for arg in codex_app_server_args(access_mode, execution_mode, &config_overrides) {
                cmd.arg(arg);
            }
            if let Some(path) = process_env::resolved_child_process_path() {
                cmd.env("PATH", path);
            }
            if let Some(root) = rollout_trace_root.as_ref() {
                cmd.env("CODEX_ROLLOUT_TRACE_ROOT", root.path());
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .group_spawn()
                .map_err(|e| format!("Failed to spawn Codex app-server: {e}"))?
        };

        let stdin = child
            .inner()
            .stdin
            .take()
            .ok_or("Failed to capture Codex stdin")?;
        let stdout = child
            .inner()
            .stdout
            .take()
            .ok_or("Failed to capture Codex stdout")?;
        let stderr = child
            .inner()
            .stderr
            .take()
            .ok_or("Failed to capture Codex stderr")?;

        let child_ref = Arc::new(Mutex::new(Some(child)));
        let pending: PendingRpcMap = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        let stdout_pending = Arc::clone(&pending);
        let stdout_inbound = inbound_tx.clone();
        let stdout_child = Arc::clone(&child_ref);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let parsed = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("Failed to parse Codex stdout JSON: {err}; line: {line}");
                        continue;
                    }
                };

                if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
                    let has_method = parsed.get("method").is_some();
                    let has_result_or_error =
                        parsed.get("result").is_some() || parsed.get("error").is_some();
                    if has_result_or_error && !has_method {
                        let response = if let Some(result) = parsed.get("result") {
                            Ok(result.clone())
                        } else {
                            let err_obj = parsed.get("error").cloned().unwrap_or(Value::Null);
                            Err(codex_rpc_error(&err_obj))
                        };
                        if let Some(tx) = stdout_pending.lock().await.remove(&id) {
                            let _ = tx.send(response);
                        }
                        continue;
                    }
                }

                if let Some(method) = parsed.get("method").and_then(Value::as_str) {
                    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(id) = parsed.get("id").cloned() {
                        let _ = stdout_inbound.send(CodexInbound::ServerRequest {
                            id,
                            method: method.to_string(),
                            params,
                        });
                    } else {
                        let _ = stdout_inbound.send(CodexInbound::Notification {
                            method: method.to_string(),
                            params,
                        });
                    }
                }
            }

            let exit_code = match stdout_child.lock().await.as_mut() {
                Some(child) => child
                    .try_wait()
                    .ok()
                    .flatten()
                    .and_then(|status| status.code()),
                None => None,
            };

            fail_pending_codex_requests(&stdout_pending, "Codex app-server exited before response")
                .await;

            let _ = stdout_inbound.send(CodexInbound::Closed { exit_code });
        });

        let stderr_inbound = inbound_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = stderr_inbound.send(CodexInbound::Stderr(line));
            }
        });
        let rollout_trace_task = rollout_trace_root.as_ref().map(|root| {
            tokio::spawn(forward_codex_rollout_trace(
                root.path().to_path_buf(),
                inbound_tx,
            ))
        });

        Ok((
            Self {
                stdin: Arc::new(Mutex::new(Some(stdin))),
                pending,
                next_id: AtomicU64::new(1),
                child: child_ref,
                stdout_task,
                stderr_task,
                rollout_trace_task,
                rollout_trace_root,
                compaction_capability: Arc::new(std::sync::Mutex::new(
                    BackendCompactionCapability::unknown(
                        BackendCompactionUnknownReason::ProcessNotInitialized,
                        None,
                        BackendCompactionCapabilityEvidence::None,
                    ),
                )),
            },
            inbound_rx,
        ))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_typed(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    async fn request_typed(&self, method: &str, params: Value) -> Result<Value, CodexRpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self.send_json(&payload).await {
            let _ = self.pending.lock().await.remove(&id);
            return Err(CodexRpcError::transport(err));
        }
        observe_codex_request_sent(method);

        // No deadline. A request to the local app-server ends exactly two ways:
        // it is answered, or the process can no longer answer it — and both are
        // signalled here, by the stdout reader on EOF and by teardown, so a
        // clock could only ever pre-empt a healthy request. It used to: codex
        // does heavy sqlite work under CODEX_HOME on startup, and a home
        // directory slow enough to push `initialize` past the old bound turned
        // a working CLI into a failed one. Worse, the timeout also removed the
        // pending entry, so the answer that did arrive was dropped unmatched.
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(CodexRpcError::transport("Codex response channel closed")),
        }
    }

    fn spawn_request(&self, method: &'static str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stdin = Arc::clone(&self.stdin);
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            });
            let (tx, rx) = oneshot::channel();
            pending.lock().await.insert(id, tx);
            let send_result = write_codex_stdin_line(&stdin, &format!("{payload}\n")).await;
            if let Err(error) = send_result {
                pending.lock().await.remove(&id);
                tracing::warn!(
                    codex_method = method,
                    error,
                    "Detached Codex request failed"
                );
                return;
            }
            observe_codex_request_sent(method);
            let result = match rx.await {
                Ok(result) => result,
                Err(_) => Err(CodexRpcError::transport("Codex response channel closed")),
            };
            if let Err(error) = &result {
                tracing::warn!(
                    codex_method = method,
                    error = %error,
                    "Detached Codex request failed"
                );
            }
        });
    }

    async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
        .await
    }

    async fn send_json(&self, value: &Value) -> Result<(), String> {
        write_codex_stdin_line(&self.stdin, &format!("{value}\n")).await
    }

    /// Close our end of the app-server's stdin, which is how we ask it to exit.
    ///
    /// The protocol has no shutdown request — none of its client methods ends
    /// the process — so EOF is the only way to say "we're done" short of a
    /// signal. `codex app-server` answers it by exiting 0.
    async fn close_stdin(&self) {
        drop(self.stdin.lock().await.take());
    }

    /// Tear the app-server down.
    async fn shutdown(&self) {
        self.close_stdin().await;
        let mut child_guard = self.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            let _ = shut_down_codex_child(&mut child).await;
        }
        drop(child_guard);
        // child is taken (None) — Drop will be a no-op. Drop the readers so the
        // parent-side stdio pipe fds are released even if EOF hasn't propagated.
        self.abort_readers();
        self.fail_pending("Codex app-server was shut down before responding")
            .await;
    }

    async fn terminate(&self) -> Result<(), String> {
        self.close_stdin().await;
        let child = self.child.lock().await.take();
        let result = match child {
            Some(mut child) => shut_down_codex_child(&mut child).await,
            None => Ok(()),
        };
        self.abort_readers();
        self.fail_pending("Codex app-server was terminated before responding")
            .await;
        result
    }

    /// Resolve every in-flight request the app-server can no longer answer.
    ///
    /// The stdout reader does this on EOF, but teardown aborts that reader, so
    /// without this a request in flight when the session closes would wait on a
    /// sender nobody is left to fire. That wait is unbounded now, so this is
    /// what keeps "no deadline" from meaning "no way out".
    async fn fail_pending(&self, reason: &'static str) {
        fail_pending_codex_requests(&self.pending, reason).await;
    }

    /// Reap the app-server after it exited on its own (stdout EOF → `Closed`).
    ///
    /// Unlike claude (whose stdout reader calls `mark_process_exited`, which
    /// removes the runtime from its slot so `Drop` fires), nothing takes the
    /// `CodexRpc` out of `CodexInner` when the process exits mid-session — the
    /// forwarder task still holds `Arc<CodexInner>`, so `Drop` won't run until
    /// session teardown. Without this, an exited app-server lingers as a zombie
    /// for the rest of the session (the dominant observed leak). The reader
    /// tasks are already ending on EOF, so this only takes the child and
    /// `wait()`s it. Idempotent with `shutdown()`/`Drop`.
    async fn reap_after_exit(&self) {
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// Let the app-server exit on its own, then make sure nothing it spawned outlives it.
///
/// Callers close stdin first. `codex app-server` has no shutdown request — none
/// of its client methods ends the process — so that EOF is the only way to tell
/// it we are done short of a signal, and it answers by exiting 0. What it does
/// on the way out is the point: it closes sqlite databases under `CODEX_HOME`
/// that SIGKILL leaves open, so an unasked teardown strands their `-wal`/`-shm`
/// files for the next start to recover. Not a correctness fix — the rollout
/// transcript survives SIGKILL intact — but recovery is the work that needs the
/// POSIX locks a networked `CODEX_HOME` cannot serve.
///
/// The graceful wait is on the leader *alone*. `AsyncGroupChild::wait` reaps the
/// whole group, so using it here would let a descendant that outlives codex hold
/// the wait open long after the process we asked to leave had gone.
///
/// The group kill after it is unconditional and unchanged: codex exiting cleanly
/// is not a promise that everything it spawned did, and this is what guarantees
/// teardown terminates.
async fn shut_down_codex_child(child: &mut AsyncGroupChild) -> Result<(), String> {
    eprintln!(
        "TYDE CODEX PROCESS SHUTDOWN waiting_for_eof_exit pid={:?}",
        child.inner().id()
    );
    // No deadline. This ends when the process ends, which is the same liveness
    // signal the stdout reader watches. A clock could only pre-empt the flush
    // this wait exists to allow — and if some future codex stopped exiting on
    // EOF, hanging is the honest report of that, where a deadline would hide it.
    let _ = child.inner().wait().await;
    eprintln!(
        "TYDE CODEX PROCESS SHUTDOWN eof_exit_observed pid={:?}",
        child.inner().id()
    );

    let kill_error = child
        .start_kill()
        .err()
        .map(|err| format!("failed to kill Codex app-server process group: {err}"));
    // Unbounded on purpose: this reaps a process group we have just signalled,
    // which is a local operation with no peer to cooperate. A clock here could
    // only ever report a slow machine as a failed cleanup.
    let wait_error = child
        .wait()
        .await
        .err()
        .map(|err| format!("failed to reap Codex app-server process group: {err}"));

    codex_terminate_outcome(kill_error, wait_error)
}

/// Write one line to the app-server's stdin, or report that teardown closed it.
async fn write_codex_stdin_line(
    stdin: &Mutex<Option<ChildStdin>>,
    line: &str,
) -> Result<(), String> {
    match stdin.lock().await.as_mut() {
        Some(stdin) => stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| format!("Failed to write to Codex stdin: {error}")),
        None => Err("Codex app-server stdin is closed".to_string()),
    }
}

/// Fail every in-flight request because the app-server can no longer answer it.
async fn fail_pending_codex_requests(pending: &PendingRpcMap, reason: &'static str) {
    for (_, tx) in pending.lock().await.drain() {
        let _ = tx.send(Err(CodexRpcError::transport(reason)));
    }
}

fn codex_terminate_outcome(
    kill_error: Option<String>,
    wait_error: Option<String>,
) -> Result<(), String> {
    match (kill_error, wait_error) {
        // A failed kill is moot when the child was still reaped: killing an
        // already-exited process reports "No such process" (ESRCH) even
        // though cleanup fully succeeded. Only a child that then also fails
        // to exit turns the kill failure into a real error.
        (_, None) => Ok(()),
        (None, Some(error)) => Err(error),
        (Some(kill_error), Some(wait_error)) => Err(format!("{kill_error}; {wait_error}")),
    }
}

impl Drop for CodexRpc {
    /// Last-ditch net for panic/teardown. NOTE: because the forwarder task
    /// holds `Arc<CodexInner>` (which owns this `CodexRpc`), this Drop does NOT
    /// fire on mid-session process exit — the two real leak paths are covered
    /// explicitly instead:
    ///
    /// - Process self-exit: `handle_inbound(Closed)` calls `reap_after_exit()`.
    /// - Client disconnect / teardown: `shutdown()` reaps the running child.
    ///
    /// Drop then only runs at final `CodexInner` teardown and is normally a
    /// no-op (child already taken); it remains as a backstop for any path that
    /// drops a `CodexRpc` without calling either of the above.
    fn drop(&mut self) {
        self.stdout_task.abort();
        self.stderr_task.abort();
        if let Some(task) = &self.rollout_trace_task {
            task.abort();
        }
        crate::backend::subprocess::reap_group_child_slot(&self.child);
    }
}

// ---------------------------------------------------------------------------
// Backend trait implementation
// ---------------------------------------------------------------------------

use protocol::{
    AgentInput, ChatEvent, ChatMessage, CompactionMethod, CompactionMetrics, CompactionStage,
    CompactionTrigger, MessageSender, SessionId, SessionSettingField, SessionSettingFieldType,
    SessionSettingValue, SessionSettingsSchema, SpawnCostHint,
};

use super::{
    Backend, BackendCompactionCapability, BackendCompactionCapabilityEvidence,
    BackendCompactionDeferredReason, BackendCompactionDispatchState, BackendCompactionEvent,
    BackendCompactionFailure, BackendCompactionFailureKind, BackendCompactionMechanism,
    BackendCompactionMutationState, BackendCompactionObservationSource, BackendCompactionProgress,
    BackendCompactionRequest, BackendCompactionResult, BackendCompactionStart,
    BackendCompactionSuccess, BackendCompactionTerminalEvidence,
    BackendCompactionUnavailableReason, BackendCompactionUnknownReason, BackendCompactionUserFocus,
    BackendCompactionUserFocusProvenance, BackendEvent, BackendObservedCompaction, BackendSession,
    BackendSpawnConfig, BackendTranscriptEventMetadata, EventStream, PostCompactionTokenCount,
    protocol_images_to_attachments, resolve_settings as resolve_backend_settings,
    session_settings_to_json,
};

pub struct CodexBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    settings_tx: mpsc::UnboundedSender<CodexSettingsUpdate>,
    interrupt_tx: mpsc::UnboundedSender<CodexInterrupt>,
    cancel_task_tx: mpsc::UnboundedSender<CodexCancelBackgroundTask>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
    subagent_emitter_tx: watch::Sender<Option<Arc<dyn SubAgentEmitter>>>,
    compaction_handle: Arc<std::sync::Mutex<Option<CodexCommandHandle>>>,
}

struct CodexSettingsUpdate {
    payload: protocol::SetSessionSettingsPayload,
    reply: oneshot::Sender<Result<(), String>>,
}

struct CodexCancelBackgroundTask {
    tool_call_id: String,
    reply: oneshot::Sender<bool>,
}

struct CodexInterrupt {
    reply: oneshot::Sender<bool>,
}

impl CodexBackend {
    pub(crate) async fn set_subagent_emitter(
        &self,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(), String> {
        self.subagent_emitter_tx.send(Some(emitter)).map_err(|_| {
            "Codex sub-agent emitter update failed: backend event loop is not running".to_string()
        })
    }

    pub(crate) async fn spawn_with_subagent_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        emitter: Arc<dyn SubAgentEmitter>,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, Some(emitter))
            .await
    }

    async fn spawn_with_initial_emitter(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
        initial_emitter: Option<Arc<dyn SubAgentEmitter>>,
    ) -> Result<(Self, EventStream), String> {
        let inference_only = config.execution_mode == BackendExecutionMode::InferenceOnly;
        // No remote-skill guard here: a remote session drops its skills with a
        // notice when it starts (`codex_remote_skill_notice`), rather than
        // refusing to start at all.
        let initial_emitter = (!inference_only).then_some(initial_emitter).flatten();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (cancel_task_tx, mut cancel_task_rx) =
            mpsc::unbounded_channel::<CodexCancelBackgroundTask>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<CodexInterrupt>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(initial_emitter.clone());
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, String>>();
        let (startup_cancel_tx, startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = CodexStartupCancelGuard(Some(startup_cancel_tx));
        let compaction_handle = Arc::new(std::sync::Mutex::new(None::<CodexCommandHandle>));
        let task_compaction_handle = Arc::clone(&compaction_handle);

        tokio::spawn(async move {
            let combined_instructions = (!inference_only)
                .then(|| render_combined_spawn_instructions(&config.resolved_spawn_config))
                .flatten();
            let startup_mcp_servers = if inference_only {
                &[][..]
            } else {
                config.startup_mcp_servers.as_slice()
            };
            let selected_skills = if inference_only {
                &[][..]
            } else {
                config.resolved_spawn_config.skills.as_slice()
            };
            let session_result = CodexSession::spawn_with_mode(
                &workspace_roots,
                None,
                startup_mcp_servers,
                combined_instructions.as_deref(),
                CodexSessionSpawnOptions {
                    ephemeral: false,
                    access_mode: config.resolved_spawn_config.access_mode,
                    subagent_emitter: initial_emitter,
                    execution_mode: config.execution_mode,
                    installed_provider_version: config.provider_version.as_deref(),
                    selected_skills,
                    skill_selection: if inference_only {
                        SkillSelection::Explicit
                    } else {
                        config.resolved_spawn_config.skill_selection
                    },
                },
            )
            .await;
            let (session, mut raw_events) = match session_result {
                Ok(value) => value,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("Failed to start Codex session: {err}")));
                    return;
                }
            };

            // `thread/start` has already supplied the authoritative ID. Publish
            // it before doing any further startup work: Codex may announce a
            // native child before the initial turn RPC responds.
            let session_id = session.session_id();
            if ready_tx.send(Ok(session_id)).is_err() {
                session.shutdown().await;
                return;
            }
            observe_codex_spawn_ready();
            if startup_cancel_rx.await.is_ok() {
                session.shutdown().await;
                observe_codex_spawn_startup_cancelled();
                return;
            }

            let handle = session.command_handle();
            *task_compaction_handle
                .lock()
                .expect("Codex compaction handle mutex poisoned") = Some(handle.clone());
            let resolved_settings = if inference_only {
                protocol::SessionSettingsValues::default()
            } else {
                resolve_session_settings(&config)
            };
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            tracing::debug!(
                inference_only,
                has_model_override = model_override.is_some(),
                has_effort_override = effort_override.is_some(),
                "Codex startup settings resolved"
            );
            let mut normalization_failures = HashMap::new();
            let mut pending_initial_input_cancelled = false;
            if model_override.is_some() || effort_override.is_some() {
                tracing::debug!("Codex startup dispatching thread/settings/update");
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                let settings_handle = handle.clone();
                let settings_request = settings_handle.update_runtime_settings(settings);
                tokio::pin!(settings_request);

                enum StartupSettingsPhase {
                    Configured,
                    Terminated,
                    Failed(String),
                }

                let settings_phase = loop {
                    tokio::select! {
                        biased;
                        interrupt = interrupt_rx.recv() => {
                            let Some(interrupt) = interrupt else {
                                break StartupSettingsPhase::Terminated;
                            };
                            if !pending_initial_input_cancelled {
                                // No initial turn exists yet, so Codex cannot cancel it.
                                pending_initial_input_cancelled = true;
                                let _ = events_tx.send(BackendEvent::Chat(
                                    ChatEvent::OperationCancelled(
                                        protocol::OperationCancelledData {
                                            message: "Operation cancelled".to_string(),
                                        },
                                    ),
                                ));
                                let _ = events_tx
                                    .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                            }
                            let _ = interrupt.reply.send(true);
                        }
                        result = &mut settings_request => {
                            while let Ok(interrupt) = interrupt_rx.try_recv() {
                                if !pending_initial_input_cancelled {
                                    pending_initial_input_cancelled = true;
                                    let _ = events_tx.send(BackendEvent::Chat(
                                        ChatEvent::OperationCancelled(
                                            protocol::OperationCancelledData {
                                                message: "Operation cancelled".to_string(),
                                            },
                                        ),
                                    ));
                                    let _ = events_tx
                                        .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                                }
                                let _ = interrupt.reply.send(true);
                            }
                            break match result {
                                Ok(()) => StartupSettingsPhase::Configured,
                                Err(err) => StartupSettingsPhase::Failed(format!(
                                    "Failed to configure Codex session: {err}"
                                )),
                            };
                        }
                        incoming = raw_events.recv() => {
                            let Some(raw) = incoming else {
                                break StartupSettingsPhase::Failed(
                                    "Codex event stream ended while applying startup settings"
                                        .to_string(),
                                );
                            };
                            if !forward_codex_backend_stream_event(
                                raw,
                                &events_tx,
                                &mut normalization_failures,
                            ) {
                                break StartupSettingsPhase::Terminated;
                            }
                        }
                        changed = subagent_emitter_rx.changed() => {
                            if changed.is_err() {
                                break StartupSettingsPhase::Terminated;
                            }
                            let maybe_emitter = subagent_emitter_rx.borrow().clone();
                            if let Some(emitter) = maybe_emitter
                                && let Err(err) = session.set_subagent_emitter(emitter).await
                            {
                                break StartupSettingsPhase::Failed(format!(
                                    "Codex sub-agent emitter update failed while applying startup settings: {err}"
                                ));
                            }
                        }
                    }
                };

                match settings_phase {
                    StartupSettingsPhase::Configured => {}
                    StartupSettingsPhase::Terminated => {
                        drop(events_tx);
                        session.shutdown().await;
                        return;
                    }
                    StartupSettingsPhase::Failed(message) => {
                        tracing::error!(
                            %message,
                            "Codex startup settings failed after session publication"
                        );
                        let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
                        let _ = events_tx
                            .send(BackendEvent::Chat(ChatEvent::TypingStatusChanged(false)));
                        drop(events_tx);
                        session.shutdown().await;
                        return;
                    }
                }
            }

            let images = protocol_images_to_attachments(initial_input.images);
            let (initial_turn_tx, mut initial_turn_rx) = oneshot::channel();
            let mut initial_turn_pending = !pending_initial_input_cancelled;
            if initial_turn_pending {
                let initial_turn_handle = handle.clone();
                tokio::spawn(async move {
                    let result = initial_turn_handle
                        .execute(SessionCommand::SendMessage {
                            message: initial_input.message,
                            images,
                        })
                        .await;
                    let _ = initial_turn_tx.send(result);
                });
            } else {
                drop(initial_turn_tx);
            }

            loop {
                tokio::select! {
                    result = &mut initial_turn_rx, if initial_turn_pending => {
                        initial_turn_pending = false;
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => {
                                let message = format!("Failed to send initial Codex prompt: {err}");
                                tracing::error!(%err, "Codex initial prompt failed after session registration");
                                let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
                                break;
                            }
                            Err(_) => {
                                let _ = events_tx.send(BackendEvent::Chat(backend_error_message(
                                    "Codex initial prompt task ended before reporting its result".to_string(),
                                )));
                                break;
                            }
                        }
                    }
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else { break; };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else { break; };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP DEQUEUE mode=spawn message={:?}",
                                    payload.message.chars().take(96).collect::<String>()
                                );
                                let images = protocol_images_to_attachments(payload.images);
                                let result = handle.execute(SessionCommand::SendMessage {
                                    message: payload.message,
                                    images,
                                }).await;
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP RPC mode=spawn result={result:?}"
                                );
                                if let Err(err) = result {
                                    tracing::error!(%err, "Failed to send Codex follow-up");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!("queued-message inputs must be handled by the agent actor before reaching the backend");
                            }
                        }
                    }
                    cancel = cancel_task_rx.recv() => {
                        let Some(cancel) = cancel else { break; };
                        let cancelled = handle
                            .execute(SessionCommand::CancelBackgroundTask {
                                tool_call_id: cancel.tool_call_id,
                            })
                            .await
                            .is_ok();
                        let _ = cancel.reply.send(cancelled);
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break; };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await
                            .map_err(|err| format!("Codex session settings update failed: {err}"));
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(interrupt) = interrupt else { break; };
                        eprintln!("TYDE CODEX INTERRUPT DEQUEUE mode=spawn");
                        let result = handle.execute(SessionCommand::CancelConversation).await;
                        eprintln!("TYDE CODEX INTERRUPT RPC mode=spawn result={result:?}");
                        let accepted = result.is_ok();
                        let _ = interrupt.reply.send(accepted);
                        if let Err(err) = result {
                            tracing::error!(%err, "Failed to interrupt Codex turn");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Codex sub-agent emitter update failed");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        let session_id = match ready_rx.await {
            Ok(Ok(session_id)) => session_id,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("Codex spawn initialization task ended early".to_string()),
        };
        startup_cancel_guard.disarm();
        let backend_session_id = Arc::new(std::sync::Mutex::new(Some(session_id)));
        let transcript_session_id = Arc::clone(&backend_session_id);

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                cancel_task_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
                compaction_handle,
            },
            EventStream::new_backend_with_transcript_metadata(events_rx, move |event| {
                codex_transcript_event_metadata(&transcript_session_id, event)
            }),
        ))
    }
}

fn codex_session_settings_schema(models: Vec<CodexModelMetadata>) -> SessionSettingsSchema {
    let default_reasoning_options = models
        .iter()
        .find(|model| model.is_default)
        .map(|model| model.reasoning_options.clone())
        .unwrap_or_default();
    let model_options = models.iter().map(|model| model.option.clone()).collect();
    let reasoning_options_by_model = protocol::SelectOptionsBySetting {
        setting_key: "model".to_string(),
        values: models
            .into_iter()
            .map(|model| protocol::SelectOptionsForValue {
                setting_value: model.option.value,
                options: model.reasoning_options,
            })
            .collect(),
    };
    SessionSettingsSchema {
        backend_kind: protocol::BackendKind::Codex,
        fields: vec![
            SessionSettingField {
                key: "model".to_string(),
                label: "Model".to_string(),
                description: None,
                use_slider: false,
                select_options_by_setting: None,
                field_type: SessionSettingFieldType::Select {
                    options: model_options,
                    default: None,
                    nullable: true,
                },
            },
            SessionSettingField {
                key: "reasoning_effort".to_string(),
                label: "Reasoning Effort".to_string(),
                description: None,
                use_slider: true,
                select_options_by_setting: Some(reasoning_options_by_model),
                field_type: SessionSettingFieldType::Select {
                    options: default_reasoning_options,
                    default: None,
                    nullable: true,
                },
            },
        ],
    }
}

pub(crate) fn codex_cost_hint_defaults(
    cost_hint: SpawnCostHint,
) -> protocol::SessionSettingsValues {
    match cost_hint {
        SpawnCostHint::Low | SpawnCostHint::Medium | SpawnCostHint::High => {
            protocol::SessionSettingsValues::default()
        }
    }
}

pub(crate) fn codex_tier_config_from_schema(
    schema: &SessionSettingsSchema,
    selected_values: &protocol::SessionSettingsValues,
) -> Result<settings_model::BackendTierConfig, String> {
    if schema.backend_kind != protocol::BackendKind::Codex {
        return Err("Codex tier resolution received a non-Codex schema".to_owned());
    }
    let reasoning_field = schema
        .fields
        .iter()
        .find(|field| field.key == "reasoning_effort")
        .ok_or_else(|| "Codex model metadata omitted reasoning_effort".to_owned())?;
    let options = reasoning_field
        .select_options(selected_values)
        .filter(|options| !options.is_empty())
        .ok_or_else(|| {
            "selected Codex model metadata advertised no reasoning efforts".to_owned()
        })?;
    let low = options
        .first()
        .ok_or_else(|| "selected Codex model metadata has no lowest reasoning effort".to_owned())?
        .value
        .clone();
    let high = options
        .last()
        .ok_or_else(|| "selected Codex model metadata has no highest reasoning effort".to_owned())?
        .value
        .clone();
    let mut low_values = protocol::SessionSettingsValues::default();
    low_values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String(low),
    );
    let mut high_values = protocol::SessionSettingsValues::default();
    high_values.0.insert(
        "reasoning_effort".to_owned(),
        SessionSettingValue::String(high),
    );
    Ok(settings_model::BackendTierConfig {
        low: low_values,
        high: high_values,
    })
}

pub(crate) fn resolve_session_settings(
    config: &BackendSpawnConfig,
) -> protocol::SessionSettingsValues {
    resolve_backend_settings(
        config,
        &CodexBackend::session_settings_schema(),
        codex_cost_hint_defaults,
    )
}

fn backend_error_message(content: String) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn emit_codex_resume_startup_error(
    events_tx: &mpsc::UnboundedSender<BackendEvent>,
    replay_complete_tx: &mut Option<oneshot::Sender<()>>,
    message: String,
) {
    tracing::error!("{message}");
    let _ = events_tx.send(BackendEvent::Chat(backend_error_message(message)));
    if let Some(tx) = replay_complete_tx.take() {
        let _ = tx.send(());
    }
}

fn backend_warning_message(content: String) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Warning,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn is_codex_thread_fork_unsupported_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("thread/fork")
        && (normalized.contains("-32601")
            || normalized.contains("method not found")
            || normalized.contains("unknown method")
            || normalized.contains("unknown request")
            || normalized.contains("unsupported method")
            || normalized.contains("unknown variant"))
}

fn codex_thread_fork_unsupported_message() -> String {
    "Installed Codex CLI does not expose session fork (app-server method `thread/fork`). Update Codex CLI and try again."
        .to_string()
}

fn codex_ssh_fork_unsupported_error(workspace_roots: &[String]) -> Option<BackendStartupError> {
    let detail = codex_ssh_workspace_detail(workspace_roots)?;
    Some(BackendStartupError::unsupported(format!(
        "Codex backend does not support session fork {detail} yet"
    )))
}

fn raw_codex_tool_request(item_id: &str, item: &Value) -> Option<BufferedCodexToolRequest> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    if item_type.ends_with("_output") || item_type.ends_with("Output") {
        return None;
    }
    let provider_call_id = item
        .get("call_id")
        .or_else(|| item.get("callId"))
        .and_then(Value::as_str)
        .filter(|call_id| !call_id.trim().is_empty())
        .map(str::to_owned);
    let tool_call_id = provider_call_id
        .clone()
        .unwrap_or_else(|| item_id.to_owned());
    let tool_name = item
        .get("name")
        .or_else(|| item.get("tool"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            matches!(item_type, "local_shell_call" | "shell_call").then(|| "run_command".to_owned())
        })?;
    let arguments = item
        .get("arguments")
        .or_else(|| item.get("input"))
        .or_else(|| item.get("action"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments = arguments
        .as_str()
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or(arguments);
    let tool_type = raw_codex_tool_request_type(&tool_name, &arguments).unwrap_or_else(|| {
        json!({
            "kind": "Other",
            "args": arguments.clone(),
        })
    });
    Some(BufferedCodexToolRequest {
        turn_id: None,
        provider_item_id: Some(item_id.to_owned()),
        provider_call_id,
        tool_call_id,
        tool_name,
        arguments,
        tool_type,
        content_offset: None,
    })
}

/// Whether some other path already renders this raw call, so recording it here
/// would duplicate the card rather than rescue it.
///
/// Codex reports most tools twice — a raw `custom_tool_call` and a typed item —
/// and the typed item owns the card because it carries the execution's status,
/// exit code and output. The two ids share no key, so which raw call a typed
/// item belongs to is not knowable; the tool the model invoked is.
///
/// Measured against codex-cli 0.146.0: a turn that started one command and
/// watched it made one `tools.exec_command` and four `tools.write_stdin` calls
/// and produced exactly *one* typed item. `write_stdin` gets none, so under
/// "the typed item owns everything" it was dropped outright.
///
/// Listed the way round that fails safe. An unrecognised tool falls through to
/// a card carrying its JSON: if it turns out to have a typed item too the user
/// sees it twice, which is visible and fixable, rather than not at all.
fn codex_raw_call_is_rendered_elsewhere(tool_name: &str, arguments: &Value) -> bool {
    // Code-mode `wait` does not start new work. It only resumes a yielded
    // `exec` cell whose underlying typed item owns the user-visible card. For
    // image generation Codex 0.146.0 emits the completed imageGeneration item,
    // then this raw continuation with only a cell id; showing both makes one
    // image request look like two independent tools.
    if tool_name == "wait"
        && arguments
            .get("cell_id")
            .and_then(Value::as_str)
            .is_some_and(|cell_id| !cell_id.trim().is_empty())
    {
        return true;
    }
    // The same call reaches Tyde in two shapes, and which one the model picks
    // varies run to run: `exec_command` called directly, with JSON arguments,
    // and `exec` called with a source string that calls it. Reading only the
    // source string was measured against a day of runs that happened to
    // contain nothing but that form, so the direct form went unsuppressed and
    // left a second, never-completed card open on every command.
    if codex_function_is_rendered_elsewhere(tool_name) {
        return true;
    }
    let Some(source) = arguments.as_str() else {
        return false;
    };
    let Some(function) = source.split_once("tools.").map(|(_, rest)| {
        rest.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
            .next()
            .unwrap_or_default()
    }) else {
        return false;
    };
    codex_function_is_rendered_elsewhere(function)
}

/// Whether this Codex tool already reaches the user through a typed item:
/// `commandExecution`, `fileChange`, `webSearch`, `imageView`, `sleep`,
/// `mcpToolCall`, or native collaboration spawn.
///
/// `write_stdin` is deliberately absent — it gets no typed item, so the raw
/// declaration is the only record of it and suppressing it drops the call
/// outright.
///
/// `update_plan` is deliberately absent too. Its effect does show up, in the
/// task list, but it is still a tool the model called and it gets a card like
/// any other — without one, a response whose only act was a plan update
/// published a message with nothing in it, which is what `real_task_list` was
/// failing on.
fn codex_function_is_rendered_elsewhere(function: &str) -> bool {
    matches!(
        function,
        "exec_command" | "apply_patch" | "web__run" | "view_image" | "sleep"
    ) || function.starts_with("mcp__")
        || is_tyde_agent_control_spawn_tool_name(function)
        || is_tyde_agent_control_await_tool_name(function)
        || is_tyde_agent_control_send_message_tool_name(function)
}

fn raw_codex_tool_request_type(tool_name: &str, arguments: &Value) -> Option<Value> {
    if !tool_name.eq_ignore_ascii_case("exec") {
        return None;
    }
    let source = arguments.as_str()?;
    let call = source.split_once("tools.exec_command(")?.1;
    let command = javascript_object_string_field(call, "cmd")?;
    let working_directory = javascript_object_string_field(call, "workdir").unwrap_or_default();
    Some(json!({
        "kind": "RunCommand",
        "command": command,
        "working_directory": working_directory,
    }))
}

fn javascript_object_string_field(source: &str, field: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(relative_start) = source.get(search_from..)?.find(field) {
        let start = search_from + relative_start;
        let before = source[..start].chars().next_back();
        let after = source[start + field.len()..].chars().next();
        let identifier_boundary = before.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
            && after.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');
        if !identifier_boundary {
            search_from = start + field.len();
            continue;
        }
        let Some(value) = source[start + field.len()..].trim_start().strip_prefix(':') else {
            search_from = start + field.len();
            continue;
        };
        let value = value.trim_start();
        if !value.starts_with('"') {
            search_from = start + field.len();
            continue;
        }
        let mut escaped = false;
        for (offset, ch) in value[1..].char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                let literal = &value[..offset + 2];
                return serde_json::from_str(literal).ok();
            }
        }
        return None;
    }
    None
}

fn codex_public_tool_request_type(tool_name: &str, arguments: &Value) -> Value {
    // Tyde's own agent-control tools are typed once for every backend in
    // `EventStream::project_tyde_agent_control`; only Codex-native tools are
    // projected here.
    if tool_name.eq_ignore_ascii_case("spawnAgent") {
        return serde_json::to_value(protocol::ToolRequestType::AgentSpawn {
            prompt: arguments
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::to_owned),
            name: arguments
                .get("receiverAgentName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            execution_mode: protocol::AgentExecutionMode::Background,
        })
        .expect("serialize native Codex agent spawn");
    }
    parse_codex_subagent_collabs(arguments)
        .into_iter()
        .next()
        .map(|spawn| {
            serde_json::to_value(protocol::ToolRequestType::AgentSpawn {
                prompt: spawn.prompt,
                name: Some(spawn.name),
                execution_mode: protocol::AgentExecutionMode::Background,
            })
            .expect("serialize native Codex agent spawn")
        })
        .unwrap_or_else(|| {
            if codex_is_collaboration_item(arguments) {
                json!({
                    "kind": "Other",
                    "args": {
                        "action": tool_name,
                        "agent_count": codex_native_wait_thread_ids(arguments).len(),
                    }
                })
            } else {
                json!({
                    "kind": "Other",
                    "args": codex_generic_tool_arguments(arguments),
                })
            }
        })
}

fn codex_generic_tool_arguments(item: &Value) -> Value {
    item.get("arguments")
        .cloned()
        .unwrap_or_else(|| item.clone())
}

fn codex_public_generic_tool_result(tool_name: &str, item: &Value, success: bool) -> Value {
    if success {
        json!({
            "kind": "Other",
            "result": item.get("result").cloned().unwrap_or_else(|| item.clone()),
        })
    } else {
        json!({
            "kind": "Error",
            "short_message": format!("{tool_name} failed"),
            "detailed_message": serde_json::to_string_pretty(item)
                .unwrap_or_else(|_| item.to_string()),
        })
    }
}

fn normalize_codex_tool_result(
    _emitter: &TurnEmitter,
    _tool_call_id: &str,
    tool_name: &str,
    tool_result: Value,
    success: bool,
) -> (Value, Option<ToolExecutionNormalizationFailure>) {
    if success {
        match tyde_tool_result(tool_name, &tool_result) {
            Ok(Some(result)) => {
                return (
                    serde_json::to_value(result).expect("serialize normalized Tyde tool result"),
                    None,
                );
            }
            Ok(None) => {}
            Err(error) => {
                return (
                    json!({
                        "kind": "Error",
                        "short_message": format!("{tool_name} returned a malformed result"),
                        "detailed_message": error.to_string(),
                    }),
                    Some(error.normalization_failure),
                );
            }
        }
    }
    if let Some(item) = tool_result
        .get("result")
        .filter(|item| codex_is_collaboration_item(item))
    {
        return (
            codex_public_collaboration_result_with_name(tool_name, item, success),
            None,
        );
    }
    if !success
        && !matches!(
            tool_result.get("kind").and_then(Value::as_str),
            Some("Error" | "Cancelled")
        )
    {
        return (
            json!({
                "kind": "Error",
                "short_message": format!("{tool_name} failed"),
                "detailed_message": serde_json::to_string_pretty(&tool_result)
                    .unwrap_or_else(|_| tool_result.to_string()),
            }),
            None,
        );
    }
    (tool_result, None)
}

fn codex_tool_execution_outcome(
    result: Value,
    success: bool,
    error: Option<String>,
    normalization_failure: Option<ToolExecutionNormalizationFailure>,
) -> ToolExecutionOutcome {
    if success {
        let result =
            serde_json::from_value(result.clone()).unwrap_or(ToolExecutionResult::Other { result });
        return ToolExecutionOutcome::Succeeded { result };
    }
    if result.get("kind").and_then(Value::as_str) == Some("Cancelled") {
        return ToolExecutionOutcome::Cancelled {
            message: result
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(error)
                .unwrap_or_else(|| "Tool execution was cancelled".to_string()),
        };
    }
    ToolExecutionOutcome::Failed {
        message: error.unwrap_or_else(|| "Tool execution failed".to_string()),
        details: (!result.is_null()).then(|| result.to_string()),
        normalization_failure,
    }
}

fn codex_background_command_group_outcome(
    mut results: Vec<CodexBackgroundCommandResult>,
) -> ToolExecutionOutcome {
    results.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    if results.len() == 1 {
        let result = results.pop().expect("one background command result");
        return codex_tool_execution_outcome(
            json!({
                "kind": "RunCommand",
                "exit_code": result.exit_code,
                "stdout": result.output,
                "stderr": ""
            }),
            result.exit_code == 0,
            (result.exit_code != 0)
                .then(|| format!("Command failed with exit code {}", result.exit_code)),
            None,
        );
    }
    let success = results.iter().all(|result| result.exit_code == 0);
    let result = json!({
        "kind": "Other",
        "result": {
            "commands": results
                .iter()
                .map(|result| json!({
                    "task_id": result.task_id,
                    "description": result.description,
                    "exit_code": result.exit_code,
                    "stdout": result.output,
                    "stderr": "",
                }))
                .collect::<Vec<_>>()
        }
    });
    codex_tool_execution_outcome(
        result,
        success,
        (!success).then(|| "One or more background commands failed".to_owned()),
        None,
    )
}

fn codex_is_collaboration_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("collabToolCall" | "collabAgentToolCall")
    )
}

fn codex_public_collaboration_result(item: &Value, success: bool) -> Value {
    let tool_name = item
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("collab_tool");
    codex_public_collaboration_result_with_name(tool_name, item, success)
}

fn codex_public_collaboration_result_with_name(
    tool_name: &str,
    item: &Value,
    success: bool,
) -> Value {
    serde_json::to_value(protocol::ToolExecutionResult::Other {
        result: json!({
            "action": tool_name,
            "status": if success { "completed" } else { "failed" },
            "agent_count": codex_native_wait_thread_ids(item).len(),
        }),
    })
    .expect("serialize Codex collaboration result")
}

fn spawn_codex_subagent_event_bridge(
    mut raw_rx: mpsc::UnboundedReceiver<Value>,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
    model_usage_tx: mpsc::UnboundedSender<ModelRequestTokenUsage>,
) {
    tokio::spawn(async move {
        let mut normalization_failures = HashMap::new();
        while let Some(raw) = raw_rx.recv().await {
            if let Some(usage) = model_request_token_usage_from_raw(&raw) {
                if model_usage_tx.send(usage).is_err() {
                    break;
                }
                continue;
            }
            if !forward_codex_backend_event(raw, &event_tx, &mut normalization_failures) {
                break;
            }
        }
    });
}

fn forward_codex_backend_stream_event(
    raw: Value,
    events_tx: &mpsc::UnboundedSender<BackendEvent>,
    normalization_failures: &mut HashMap<String, PendingToolNormalizationFailure>,
) -> bool {
    if raw.get("kind").and_then(Value::as_str) == Some("BackendCompaction") {
        let Some(data) = raw.get("data") else {
            return true;
        };
        match serde_json::from_value::<BackendCompactionEvent>(data.clone()) {
            Ok(event) => return events_tx.send(BackendEvent::Compaction(event)).is_ok(),
            Err(err) => {
                tracing::warn!("Failed to decode internal Codex compaction event: {err}");
                return true;
            }
        }
    }
    if let Some(usage) = model_request_token_usage_from_raw(&raw) {
        return events_tx
            .send(BackendEvent::ModelRequestTokenUsage(usage))
            .is_ok();
    }
    let Some(event) = codex_backend_event_from_raw(&raw, normalization_failures) else {
        return true;
    };
    if let Some(error) = event.normalization_error
        && events_tx.send(BackendEvent::Chat(error)).is_err()
    {
        return false;
    }
    if events_tx
        .send(BackendEvent::Chat(event.chat_event))
        .is_err()
    {
        return false;
    }
    !event.terminal
}

fn forward_codex_backend_event(
    raw: Value,
    events_tx: &mpsc::UnboundedSender<ChatEvent>,
    normalization_failures: &mut HashMap<String, PendingToolNormalizationFailure>,
) -> bool {
    if model_request_token_usage_from_raw(&raw).is_some() {
        return true;
    }
    let Some(event) = codex_backend_event_from_raw(&raw, normalization_failures) else {
        return true;
    };
    if let Some(error) = event.normalization_error
        && events_tx.send(error).is_err()
    {
        return false;
    }
    if events_tx.send(event.chat_event).is_err() {
        return false;
    }
    !event.terminal
}

fn model_request_token_usage_from_raw(value: &Value) -> Option<ModelRequestTokenUsage> {
    if value.get("kind").and_then(Value::as_str) != Some("ModelRequestTokenUsage") {
        return None;
    }
    serde_json::from_value(value.get("data")?.clone()).ok()
}

fn codex_transcript_event_metadata(
    provider_session_id: &Arc<std::sync::Mutex<Option<SessionId>>>,
    event: &ChatEvent,
) -> BackendTranscriptEventMetadata {
    let Some(provider_event_id) = codex_transcript_provider_event_id(event) else {
        return BackendTranscriptEventMetadata {
            provider_session_id: None,
            provider_event_id: None,
        };
    };
    let Some(provider_session_id) = provider_session_id
        .lock()
        .expect("Codex session id mutex poisoned")
        .clone()
    else {
        return BackendTranscriptEventMetadata {
            provider_session_id: None,
            provider_event_id: None,
        };
    };
    BackendTranscriptEventMetadata::visible_provider_event(provider_session_id, provider_event_id)
}

fn codex_transcript_provider_event_id(event: &ChatEvent) -> Option<String> {
    fn provider_id(id: &str) -> Option<&str> {
        (!id.trim().is_empty() && !id.starts_with("server-generated:")).then_some(id)
    }

    let (role, id) = match event {
        ChatEvent::MessageAdded(message) => (
            "message",
            provider_id(message.message_id.as_ref()?.0.as_str())?,
        ),
        ChatEvent::MessageMetadataUpdated(update) => (
            "message-metadata",
            provider_id(update.message_id.0.as_str())?,
        ),
        ChatEvent::StreamStart(_)
        | ChatEvent::StreamDelta(_)
        | ChatEvent::StreamReasoningDelta(_) => return None,
        ChatEvent::StreamEnd(end) => (
            "stream-end",
            provider_id(end.message.message_id.as_ref()?.0.as_str())?,
        ),
        ChatEvent::ToolRequest(request) => {
            ("tool-request", provider_id(request.tool_call_id.as_str())?)
        }
        ChatEvent::ToolProgress(progress) => (
            "tool-progress",
            provider_id(progress.tool_call_id.as_str())?,
        ),
        ChatEvent::ToolExecutionCompleted(completion) => (
            "tool-completion",
            provider_id(completion.tool_call_id.as_str())?,
        ),
        ChatEvent::TypingStatusChanged(_)
        | ChatEvent::TaskUpdate(_)
        | ChatEvent::OperationCancelled(_)
        | ChatEvent::RetryAttempt(_)
        | ChatEvent::Orchestration(_)
        | ChatEvent::ContextCompaction(_) => return None,
    };
    Some(format!("{role}:{id}"))
}

struct CodexForwardedBackendEvent {
    chat_event: ChatEvent,
    terminal: bool,
    normalization_error: Option<ChatEvent>,
}

fn codex_backend_event_from_raw(
    value: &Value,
    _normalization_failures: &mut HashMap<String, PendingToolNormalizationFailure>,
) -> Option<CodexForwardedBackendEvent> {
    match serde_json::from_value::<ChatEvent>(value.clone()) {
        Ok(event) => {
            let (chat_event, normalization_error) =
                normalize_tyde_chat_event(event, _normalization_failures);
            let chat_event = normalize_codex_collaboration_chat_event(chat_event);
            Some(CodexForwardedBackendEvent {
                chat_event,
                terminal: false,
                normalization_error: normalization_error.map(backend_error_message),
            })
        }
        Err(err) => {
            let Some(kind) = value.get("kind").and_then(Value::as_str) else {
                tracing::warn!(raw = %value, error = %err, "Ignoring Codex raw event without kind");
                return None;
            };

            match kind {
                "ModelRequestTokenUsage" => None,
                "Error" => Some(CodexForwardedBackendEvent {
                    chat_event: backend_error_message(codex_raw_event_message(
                        value,
                        "Codex backend error",
                    )),
                    terminal: false,
                    normalization_error: None,
                }),
                "SubprocessStderr" => {
                    let message = codex_raw_event_message(value, "Codex subprocess stderr");
                    tracing::warn!(message = %message, "Codex subprocess stderr");
                    if let Some((attempt, max_retries)) = parse_codex_reconnecting_attempt(&message)
                    {
                        Some(CodexForwardedBackendEvent {
                            chat_event: ChatEvent::RetryAttempt(protocol::RetryAttemptData {
                                attempt,
                                max_retries,
                                error: message,
                                backoff_ms: 250u64
                                    .saturating_mul(1u64 << attempt.saturating_sub(1))
                                    .min(4_000),
                            }),
                            terminal: false,
                            normalization_error: None,
                        })
                    } else if codex_stderr_is_visible_warning(&message) {
                        Some(CodexForwardedBackendEvent {
                            chat_event: backend_warning_message(message),
                            terminal: false,
                            normalization_error: None,
                        })
                    } else {
                        None
                    }
                }
                "SubprocessExit" => {
                    let message = codex_subprocess_exit_message(value);
                    tracing::error!(message = %message, "Codex subprocess exited");
                    Some(CodexForwardedBackendEvent {
                        chat_event: backend_error_message(message),
                        terminal: true,
                        normalization_error: None,
                    })
                }
                other => {
                    tracing::warn!(
                        kind = %other,
                        raw = %value,
                        error = %err,
                        "Ignoring unsupported Codex raw event"
                    );
                    None
                }
            }
        }
    }
}

fn normalize_codex_collaboration_chat_event(event: ChatEvent) -> ChatEvent {
    event
}

fn codex_stderr_is_visible_warning(message: &str) -> bool {
    message.trim_start().starts_with("Codex warning:")
}

fn codex_subprocess_exit_message(value: &Value) -> String {
    match value
        .get("data")
        .and_then(|data| data.get("exit_code"))
        .and_then(Value::as_i64)
    {
        Some(exit_code) => format!("Codex subprocess exited with code {exit_code}"),
        None => "Codex subprocess exited".to_string(),
    }
}

struct CodexStartupCancelGuard(Option<oneshot::Sender<()>>);

fn write_codex_session_steering_tempfile(content: &str) -> Result<PathBuf, String> {
    crate::steering::write_codex_steering_tempfile(content)
}

fn observe_codex_request_sent(_method: &str) {}

fn observe_codex_fork_startup_cancelled() {}

fn observe_codex_spawn_ready() {}

fn observe_codex_spawn_startup_cancelled() {}

impl CodexStartupCancelGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CodexStartupCancelGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

fn codex_raw_event_message(value: &Value, default_message: &str) -> String {
    value
        .get("data")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| data.get("message"))
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        })
        .or_else(|| {
            value
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        })
        .map(str::to_string)
        .or_else(|| {
            value
                .get("data")
                .filter(|data| !data.is_null())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| default_message.to_string())
}

impl Backend for CodexBackend {
    fn capabilities() -> tyde_agent_adapter::BackendCapabilities {
        [
            tyde_agent_adapter::BackendCapability::ResumeSession,
            tyde_agent_adapter::BackendCapability::ForkSession,
            tyde_agent_adapter::BackendCapability::ImageInput,
            tyde_agent_adapter::BackendCapability::Interrupt,
            tyde_agent_adapter::BackendCapability::SessionSettings,
            tyde_agent_adapter::BackendCapability::StartupMcpServers,
            tyde_agent_adapter::BackendCapability::AgentControlTools,
            tyde_agent_adapter::BackendCapability::TurnUsageReported,
            tyde_agent_adapter::BackendCapability::CumulativeUsageReported,
            tyde_agent_adapter::BackendCapability::ModelRequestUsageReported,
            tyde_agent_adapter::BackendCapability::ContextUsageReported,
            tyde_agent_adapter::BackendCapability::CompactionReported,
            tyde_agent_adapter::BackendCapability::Subagents,
            tyde_agent_adapter::BackendCapability::NativeSubagentWaitProgress,
            tyde_agent_adapter::BackendCapability::BackgroundSubagents,
            tyde_agent_adapter::BackendCapability::BackgroundTasks,
            tyde_agent_adapter::BackendCapability::CancelsBackgroundTasks,
            tyde_agent_adapter::BackendCapability::YieldsRunningCommands,
            tyde_agent_adapter::BackendCapability::AgentInitiatedTurns,
            tyde_agent_adapter::BackendCapability::ReasoningDeltas,
            tyde_agent_adapter::BackendCapability::TaskUpdates,
            tyde_agent_adapter::BackendCapability::TaskListReplacement,
            tyde_agent_adapter::BackendCapability::TaskListClear,
            tyde_agent_adapter::BackendCapability::WorkspaceInstructions,
            tyde_agent_adapter::BackendCapability::Customization,
            tyde_agent_adapter::BackendCapability::GenericModifyFile,
            tyde_agent_adapter::BackendCapability::GenericGenerateImage,
            tyde_agent_adapter::BackendCapability::GenericWebSearch,
            tyde_agent_adapter::BackendCapability::GenericViewImage,
            tyde_agent_adapter::BackendCapability::GenericOtherTool,
            tyde_agent_adapter::BackendCapability::CapacityTelemetry,
            tyde_agent_adapter::BackendCapability::RetryTelemetry,
        ]
        .into()
    }

    fn session_settings_schema() -> SessionSettingsSchema {
        codex_session_settings_schema(Vec::new())
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        Self::spawn_with_initial_emitter(workspace_roots, config, initial_input, None).await
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: protocol::SessionId,
    ) -> Result<(Self, EventStream), String> {
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (cancel_task_tx, mut cancel_task_rx) =
            mpsc::unbounded_channel::<CodexCancelBackgroundTask>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<CodexInterrupt>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let compaction_handle = Arc::new(std::sync::Mutex::new(None::<CodexCommandHandle>));
        let task_compaction_handle = Arc::clone(&compaction_handle);

        let session_id = session_id.0;
        let backend_session_id =
            Arc::new(std::sync::Mutex::new(Some(SessionId(session_id.clone()))));
        let transcript_session_id = Arc::clone(&backend_session_id);

        tokio::spawn(async move {
            let mut resume_replay_complete_tx = Some(resume_replay_complete_tx);
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match CodexSession::spawn_with_mode(
                &workspace_roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
                CodexSessionSpawnOptions {
                    ephemeral: false,
                    access_mode: config.resolved_spawn_config.access_mode,
                    subagent_emitter: None,
                    execution_mode: BackendExecutionMode::Agent,
                    installed_provider_version: config.provider_version.as_deref(),
                    selected_skills: &config.resolved_spawn_config.skills,
                    skill_selection: config.resolved_spawn_config.skill_selection,
                },
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    emit_codex_resume_startup_error(
                        &events_tx,
                        &mut resume_replay_complete_tx,
                        format!("Failed to spawn Codex resume session: {err}"),
                    );
                    return;
                }
            };

            let handle = session.command_handle();
            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter
                && let Err(err) = session.set_subagent_emitter(emitter).await
            {
                emit_codex_resume_startup_error(
                    &events_tx,
                    &mut resume_replay_complete_tx,
                    format!("Failed to install Codex sub-agent emitter for resumed session: {err}"),
                );
                session.shutdown().await;
                return;
            }
            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if let Err(err) = handle
                .execute(SessionCommand::ResumeSession { session_id })
                .await
            {
                emit_codex_resume_startup_error(
                    &events_tx,
                    &mut resume_replay_complete_tx,
                    format!("Failed to resume Codex session: {err}"),
                );
                session.shutdown().await;
                return;
            }
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    emit_codex_resume_startup_error(
                        &events_tx,
                        &mut resume_replay_complete_tx,
                        format!("Failed to configure resumed Codex session: {err}"),
                    );
                    session.shutdown().await;
                    return;
                }
            }

            let mut normalization_failures = HashMap::new();
            while let Ok(raw) = raw_events.try_recv() {
                if !forward_codex_backend_stream_event(raw, &events_tx, &mut normalization_failures)
                {
                    if let Some(tx) = resume_replay_complete_tx.take() {
                        let _ = tx.send(());
                    }
                    session.shutdown().await;
                    return;
                }
            }
            *task_compaction_handle
                .lock()
                .expect("Codex compaction handle mutex poisoned") = Some(handle.clone());
            if let Some(tx) = resume_replay_complete_tx.take() {
                let _ = tx.send(());
            }

            loop {
                tokio::select! {
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else {
                            break;
                        };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP DEQUEUE mode=resume message={:?}",
                                    payload.message.chars().take(96).collect::<String>()
                                );
                                let images = protocol_images_to_attachments(payload.images);
                                let result = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await;
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP RPC mode=resume result={result:?}"
                                );
                                if let Err(err) = result {
                                    tracing::error!("Failed to send Codex resume follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    cancel = cancel_task_rx.recv() => {
                        let Some(cancel) = cancel else { break; };
                        let cancelled = handle
                            .execute(SessionCommand::CancelBackgroundTask {
                                tool_call_id: cancel.tool_call_id,
                            })
                            .await
                            .is_ok();
                        let _ = cancel.reply.send(cancelled);
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await;
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(interrupt) = interrupt else { break };
                        eprintln!("TYDE CODEX INTERRUPT DEQUEUE mode=resume");
                        let result = handle.execute(SessionCommand::CancelConversation).await;
                        eprintln!("TYDE CODEX INTERRUPT RPC mode=resume result={result:?}");
                        let accepted = result.is_ok();
                        let _ = interrupt.reply.send(accepted);
                        if let Err(err) = result {
                            tracing::error!("Failed to interrupt resumed Codex turn: {err}");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Failed to update Codex sub-agent emitter for resumed session");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                cancel_task_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
                compaction_handle,
            },
            EventStream::new_backend_with_resume_replay_barrier_and_transcript_metadata(
                events_rx,
                resume_replay_complete_rx,
                move |event| codex_transcript_event_metadata(&transcript_session_id, event),
            ),
        ))
    }

    async fn fork(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        from_session_id: protocol::SessionId,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        if let Some(error) = codex_ssh_fork_unsupported_error(&workspace_roots) {
            return Err(error);
        }

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (settings_tx, mut settings_rx) = mpsc::unbounded_channel::<CodexSettingsUpdate>();
        let (cancel_task_tx, mut cancel_task_rx) =
            mpsc::unbounded_channel::<CodexCancelBackgroundTask>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<CodexInterrupt>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<BackendEvent>();
        let (subagent_emitter_tx, mut subagent_emitter_rx) =
            watch::channel::<Option<Arc<dyn SubAgentEmitter>>>(None);
        let compaction_handle = Arc::new(std::sync::Mutex::new(None::<CodexCommandHandle>));
        let task_compaction_handle = Arc::clone(&compaction_handle);

        let (ready_tx, ready_rx) = oneshot::channel::<Result<SessionId, BackendStartupError>>();
        let (startup_cancel_tx, mut startup_cancel_rx) = oneshot::channel();
        let mut startup_cancel_guard = CodexStartupCancelGuard(Some(startup_cancel_tx));

        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let combined_instructions =
                render_combined_spawn_instructions(&config.resolved_spawn_config);
            let (session, mut raw_events) = match CodexSession::fork_with_selected_skills(
                &workspace_roots,
                None,
                &config.startup_mcp_servers,
                combined_instructions.as_deref(),
                config.resolved_spawn_config.access_mode,
                &from_session_id.0,
                CodexSelectedSkillContext {
                    skills: &config.resolved_spawn_config.skills,
                    selection: config.resolved_spawn_config.skill_selection,
                    installed_provider_version: config.provider_version.as_deref(),
                },
            )
            .await
            {
                Ok(value) => value,
                Err(err) => {
                    let startup_error = if is_codex_skills_extra_roots_unsupported_error(&err) {
                        BackendStartupError::unsupported(
                            codex_skills_extra_roots_unsupported_message(),
                        )
                    } else if is_codex_thread_fork_unsupported_error(&err) {
                        BackendStartupError::unsupported(codex_thread_fork_unsupported_message())
                    } else {
                        BackendStartupError::backend_failed(format!(
                            "Failed to fork Codex session: {err}"
                        ))
                    };
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(startup_error));
                    }
                    return;
                }
            };

            let child_session_id = session.session_id();
            let handle = session.command_handle();
            *task_compaction_handle
                .lock()
                .expect("Codex compaction handle mutex poisoned") = Some(handle.clone());
            let maybe_emitter = subagent_emitter_rx.borrow().clone();
            if let Some(emitter) = maybe_emitter
                && let Err(err) = session.set_subagent_emitter(emitter).await
            {
                session.shutdown().await;
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                        "Failed to install Codex sub-agent emitter for forked session: {err}"
                    ))));
                }
                return;
            }

            let resolved_settings = resolve_session_settings(&config);
            let model_override = match resolved_settings.0.get("model") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            let effort_override = match resolved_settings.0.get("reasoning_effort") {
                Some(SessionSettingValue::String(value)) => Some(value.clone()),
                _ => None,
            };
            if model_override.is_some() || effort_override.is_some() {
                let settings = json!({
                    "model": model_override,
                    "reasoning_effort": effort_override,
                    "approval_policy": CODEX_FORCED_APPROVAL_POLICY,
                });
                if let Err(err) = handle
                    .execute(SessionCommand::UpdateSettings {
                        settings,
                        persist: false,
                    })
                    .await
                {
                    session.shutdown().await;
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                            "Failed to configure forked Codex session: {err}"
                        ))));
                    }
                    return;
                }
            }

            if ready_tx.as_ref().is_some_and(oneshot::Sender::is_closed) {
                session.shutdown().await;
                observe_codex_fork_startup_cancelled();
                return;
            }

            let images = protocol_images_to_attachments(initial_input.images);
            let initial_prompt = handle.execute(SessionCommand::SendMessage {
                message: initial_input.message,
                images,
            });
            tokio::pin!(initial_prompt);
            tokio::select! {
                biased;
                _ = &mut startup_cancel_rx => {
                    session.shutdown().await;
                    observe_codex_fork_startup_cancelled();
                    return;
                }
                result = &mut initial_prompt => {
                    if let Err(err) = result {
                        session.shutdown().await;
                        if let Some(tx) = ready_tx.take() {
                            let _ = tx.send(Err(BackendStartupError::backend_failed(format!(
                                "Failed to send initial Codex fork prompt: {err}"
                            ))));
                        }
                        return;
                    }
                }
            }

            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(Ok(child_session_id));
            }
            let mut normalization_failures = HashMap::new();

            loop {
                tokio::select! {
                    incoming = raw_events.recv() => {
                        let Some(raw) = incoming else {
                            break;
                        };
                        if !forward_codex_backend_stream_event(
                            raw,
                            &events_tx,
                            &mut normalization_failures,
                        ) {
                            break;
                        }
                    }
                    input = input_rx.recv() => {
                        let Some(input) = input else {
                            break;
                        };
                        match input {
                            AgentInput::SendMessage(payload) => {
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP DEQUEUE mode=fork message={:?}",
                                    payload.message.chars().take(96).collect::<String>()
                                );
                                let images = protocol_images_to_attachments(payload.images);
                                let result = handle
                                    .execute(SessionCommand::SendMessage {
                                        message: payload.message,
                                        images,
                                    })
                                    .await;
                                eprintln!(
                                    "TYDE CODEX FOLLOWUP RPC mode=fork result={result:?}"
                                );
                                if let Err(err) = result {
                                    tracing::error!("Failed to send Codex fork follow-up: {err}");
                                    break;
                                }
                            }
                            AgentInput::UpdateSessionSettings(_) => {}
                            AgentInput::EditQueuedMessage(_)
                            | AgentInput::CancelQueuedMessage(_)
                            | AgentInput::SendQueuedMessageNow(_) => {
                                panic!(
                                    "queued-message inputs must be handled by the agent actor before reaching the backend"
                                );
                            }
                        }
                    }
                    cancel = cancel_task_rx.recv() => {
                        let Some(cancel) = cancel else { break; };
                        let cancelled = handle
                            .execute(SessionCommand::CancelBackgroundTask {
                                tool_call_id: cancel.tool_call_id,
                            })
                            .await
                            .is_ok();
                        let _ = cancel.reply.send(cancelled);
                    }
                    update = settings_rx.recv() => {
                        let Some(update) = update else { break };
                        let result = handle
                            .update_runtime_settings(session_settings_to_json(&update.payload.values))
                            .await;
                        let _ = update.reply.send(result);
                    }
                    interrupt = interrupt_rx.recv() => {
                        let Some(interrupt) = interrupt else { break };
                        eprintln!("TYDE CODEX INTERRUPT DEQUEUE mode=fork");
                        let result = handle.execute(SessionCommand::CancelConversation).await;
                        eprintln!("TYDE CODEX INTERRUPT RPC mode=fork result={result:?}");
                        let accepted = result.is_ok();
                        let _ = interrupt.reply.send(accepted);
                        if let Err(err) = result {
                            tracing::error!("Failed to interrupt forked Codex turn: {err}");
                            break;
                        }
                    }
                    changed = subagent_emitter_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let maybe_emitter = subagent_emitter_rx.borrow().clone();
                        if let Some(emitter) = maybe_emitter
                            && let Err(err) = session.set_subagent_emitter(emitter).await
                        {
                            tracing::error!(%err, "Failed to update Codex sub-agent emitter for forked session");
                            break;
                        }
                    }
                }
            }

            session.shutdown().await;
        });

        let child_session_id = match ready_rx.await {
            Ok(Ok(session_id)) => session_id,
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Err(BackendStartupError::backend_failed(
                    "Codex fork initialization task ended early",
                ));
            }
        };
        startup_cancel_guard.disarm();
        let backend_session_id = Arc::new(std::sync::Mutex::new(Some(child_session_id)));
        let transcript_session_id = Arc::clone(&backend_session_id);

        Ok((
            Self {
                input_tx,
                settings_tx,
                interrupt_tx,
                cancel_task_tx,
                session_id: backend_session_id,
                subagent_emitter_tx,
                compaction_handle,
            },
            EventStream::new_backend_with_transcript_metadata(events_rx, move |event| {
                codex_transcript_event_metadata(&transcript_session_id, event)
            }),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        Err("CodexBackend::list_sessions requires a live Codex RPC session".to_string())
    }

    fn compaction_capability(&self) -> BackendCompactionCapability {
        self.compaction_handle
            .lock()
            .expect("Codex compaction handle mutex poisoned")
            .as_ref()
            .map(CodexCommandHandle::compaction_capability)
            .unwrap_or_else(|| {
                BackendCompactionCapability::unknown(
                    BackendCompactionUnknownReason::ProcessNotInitialized,
                    None,
                    BackendCompactionCapabilityEvidence::None,
                )
            })
    }

    async fn begin_compaction(&self, request: BackendCompactionRequest) -> BackendCompactionStart {
        let handle = self
            .compaction_handle
            .lock()
            .expect("Codex compaction handle mutex poisoned")
            .clone();
        match handle {
            Some(handle) => handle.begin_compaction(request).await,
            None => BackendCompactionStart::Deferred {
                reason: BackendCompactionDeferredReason::SessionInitializing,
            },
        }
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("codex session_id mutex poisoned")
            .clone()
            .expect("codex session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        match input {
            AgentInput::UpdateSessionSettings(_) => false,
            other => self.input_tx.send(other).is_ok(),
        }
    }

    async fn send_with_outcome(&self, input: AgentInput) -> crate::backend::SendOutcome {
        use crate::backend::SendOutcome;

        let handle = self
            .compaction_handle
            .lock()
            .expect("Codex command handle mutex poisoned")
            .clone();
        let (payload, handle) = match (input, handle) {
            (AgentInput::SendMessage(payload), Some(handle)) if payload.tool_response.is_none() => {
                (payload, handle)
            }
            (input, _) => {
                return if self.send(input).await {
                    SendOutcome::Accepted
                } else {
                    SendOutcome::Closed
                };
            }
        };

        if !handle.try_reserve_user_turn().await {
            eprintln!("TYDE CODEX USER TURN ADMISSION busy");
            return SendOutcome::Busy(AgentInput::SendMessage(payload));
        }
        eprintln!("TYDE CODEX USER TURN ADMISSION reserved");
        match self.input_tx.send(AgentInput::SendMessage(payload)) {
            Ok(()) => SendOutcome::Accepted,
            Err(error) => {
                handle.release_user_turn_reservation().await;
                let _ = error;
                SendOutcome::Closed
            }
        }
    }

    async fn update_session_settings(
        &mut self,
        payload: protocol::SetSessionSettingsPayload,
    ) -> Result<(), String> {
        let (reply, result) = oneshot::channel();
        self.settings_tx
            .send(CodexSettingsUpdate { payload, reply })
            .map_err(|_| "Codex backend terminated before applying session settings".to_owned())?;
        result
            .await
            .map_err(|_| "Codex settings update response channel closed".to_owned())?
    }

    async fn interrupt(&self) -> bool {
        let (reply, done) = oneshot::channel();
        let accepted = self.interrupt_tx.send(CodexInterrupt { reply }).is_ok();
        eprintln!("TYDE CODEX INTERRUPT ENQUEUE accepted={accepted}");
        accepted && done.await.unwrap_or(false)
    }

    async fn cancel_background_task(&self, tool_call_id: &str) -> bool {
        let (reply, done) = oneshot::channel();
        if self
            .cancel_task_tx
            .send(CodexCancelBackgroundTask {
                tool_call_id: tool_call_id.to_owned(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        done.await.unwrap_or(false)
    }

    async fn shutdown(self) {
        drop(self);
    }
}
