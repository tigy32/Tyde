#[cfg(feature = "launcher")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "launcher")]
use std::collections::HashSet;
#[cfg(feature = "launcher")]
use std::ffi::OsStr;
#[cfg(feature = "launcher")]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(feature = "launcher")]
use url::{Host, Url};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevInstanceMutablePath {
    pub env: &'static str,
    pub relative_path: &'static str,
}

pub const WORKFLOW_RUN_STORE_PATH_ENV: &str = "TYDE_WORKFLOW_RUN_STORE_PATH";
pub const CONFIGURED_HOST_STORE_PATH_ENV: &str = "TYDE_CONFIGURED_HOST_STORE_PATH";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_HOME_ENV: &str = "HOME";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_HERMES_HOME_ENV: &str = "HERMES_HOME";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_HERMES_HOME_RELATIVE_PATH: &str = "hermes-home/.hermes";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_DENY_PROXY_URL: &str = "http://127.0.0.1:9";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_HERMES_EXECUTABLE_ENV: &str = "HERMES_EXECUTABLE";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_HERMES_PYTHON_ENV: &str = "HERMES_PYTHON";
#[cfg(feature = "launcher")]
pub const DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS: &[&str] = &[
    "API_KEY",
    "API_MODE",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_CONFIG_FILE",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_DEFAULT_PROFILE",
    "AWS_PROFILE",
    "AWS_ROLE_ARN",
    "AWS_ROLE_SESSION_NAME",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_SESSION_TOKEN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
    "AZURE_OPENAI_AD_TOKEN",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_BASE_URL",
    "AZURE_OPENAI_DEPLOYMENT",
    "AZURE_OPENAI_ENDPOINT",
    "AZURE_TENANT_ID",
    "CEREBRAS_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "COHERE_API_KEY",
    "COPILOT_GITHUB_TOKEN",
    "CUSTOM_API_KEY",
    "CUSTOM_BASE_URL",
    "DEEPSEEK_API_KEY",
    "ELEVENLABS_API_KEY",
    "FIREWORKS_API_KEY",
    "GEMINI_API_KEY",
    "GH_TOKEN",
    "GITHUB_COPILOT_TOKEN",
    "GITHUB_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_API_KEY",
    "GOOGLE_API_BASE",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_QUOTA_PROJECT",
    "GROQ_API_KEY",
    "HERMES_API_KEY",
    "HERMES_BASE_URL",
    "HERMES_INFERENCE_API_KEY",
    "HERMES_INFERENCE_BASE_URL",
    "HERMES_TUI_API_KEY",
    "HERMES_TUI_BASE_URL",
    "HF_TOKEN",
    "HUGGINGFACE_API_KEY",
    "MISTRAL_API_KEY",
    "NOUS_API_KEY",
    "NOUS_PORTAL_REFRESH_TOKEN",
    "NOUS_PORTAL_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT",
    "OPENROUTER_API_KEY",
    "OPENROUTER_BASE_URL",
    "PERPLEXITY_API_KEY",
    "TOGETHER_API_KEY",
    "TOKEN",
    "VERTEXAI_LOCATION",
    "VERTEXAI_PROJECT",
    "VERTEXAI_REGION",
    "VERTEX_LOCATION",
    "VERTEX_PROJECT",
    "XAI_API_KEY",
    "XAI_BASE_URL",
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

#[cfg(feature = "launcher")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisposableHermesEnvironment {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub profiles: Vec<String>,
    pub loopback_stub: HermesLoopbackStub,
}

#[cfg(feature = "launcher")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HermesLoopbackStub {
    pub base_url: String,
    pub model: String,
}

#[cfg(feature = "launcher")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DevInstanceHermesNetworkPolicy {
    LoopbackStubOnly,
}

#[cfg(feature = "launcher")]
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
    pub loopback_stub_url: String,
    pub network_policy: DevInstanceHermesNetworkPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hermes_executable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_hermes_executable: Option<String>,
    pub hermes_launcher_chain: Vec<String>,
    pub skipped_launchers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hermes_python: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_hermes_python: Option<String>,
    pub hermes_python_launcher_chain: Vec<String>,
    pub skipped_python_launchers: Vec<String>,
    pub launcher_environment_preserved: bool,
}

#[cfg(feature = "launcher")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedHermesRuntime {
    pub executable: Option<PathBuf>,
    pub executable_launcher_chain: Vec<PathBuf>,
    pub skipped_executable_launchers: Vec<PathBuf>,
    pub python: Option<PathBuf>,
    pub python_launcher_chain: Vec<PathBuf>,
    pub skipped_python_launchers: Vec<PathBuf>,
}

#[cfg(feature = "launcher")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDisposableHermesEnvironment {
    pub home: PathBuf,
    pub hermes_home: PathBuf,
    pub runtime: ResolvedHermesRuntime,
    pub attestation: DevInstanceHermesEnvironmentAttestation,
}

#[cfg(feature = "launcher")]
pub fn prepare_disposable_hermes_environment(
    store_dir: &Path,
    input: &DisposableHermesEnvironment,
    runtime: &ResolvedHermesRuntime,
) -> Result<PreparedDisposableHermesEnvironment, String> {
    validate_profile_names(&input.profiles)?;
    let stub = validate_loopback_stub(&input.loopback_stub)?;
    let executable = validate_runtime_evidence(
        runtime.executable.as_deref(),
        &runtime.executable_launcher_chain,
        &runtime.skipped_executable_launchers,
        "Hermes",
    )?;
    let python = validate_runtime_evidence(
        runtime.python.as_deref(),
        &runtime.python_launcher_chain,
        &runtime.skipped_python_launchers,
        "HERMES_PYTHON",
    )?;
    let runtime = ResolvedHermesRuntime {
        executable: executable.program,
        executable_launcher_chain: executable.launcher_chain,
        skipped_executable_launchers: executable.skipped_launchers,
        python: python.program,
        python_launcher_chain: python.launcher_chain,
        skipped_python_launchers: python.skipped_launchers,
    };
    if runtime.executable.is_none() && runtime.python.is_none() {
        return Err(
            "disposable Hermes environment requires a resolved Hermes executable or HERMES_PYTHON"
                .to_string(),
        );
    }

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
        write_disposable_hermes_config(config_home, &stub)?;
    }

    let home_ephemeral = resolved_home.starts_with(&resolved_store);
    let hermes_home_ephemeral = resolved_hermes_home.starts_with(&resolved_home);
    Ok(PreparedDisposableHermesEnvironment {
        home: resolved_home.clone(),
        hermes_home: resolved_hermes_home.clone(),
        runtime: runtime.clone(),
        attestation: DevInstanceHermesEnvironmentAttestation {
            home: resolved_home.display().to_string(),
            resolved_home: resolved_home.display().to_string(),
            hermes_home: resolved_hermes_home.display().to_string(),
            resolved_hermes_home: resolved_hermes_home.display().to_string(),
            home_ephemeral,
            hermes_home_ephemeral,
            profiles: input.profiles.clone(),
            loopback_stub_url: stub.0.to_string(),
            network_policy: DevInstanceHermesNetworkPolicy::LoopbackStubOnly,
            hermes_executable: runtime
                .executable
                .as_ref()
                .map(|path| path.display().to_string()),
            resolved_hermes_executable: runtime
                .executable
                .as_deref()
                .map(|path| canonical_executable(path, "Hermes"))
                .transpose()?
                .map(|path| path.display().to_string()),
            hermes_launcher_chain: display_paths(&runtime.executable_launcher_chain),
            skipped_launchers: display_paths(&runtime.skipped_executable_launchers),
            hermes_python: runtime
                .python
                .as_ref()
                .map(|path| path.display().to_string()),
            resolved_hermes_python: runtime
                .python
                .as_deref()
                .map(|path| canonical_executable(path, "HERMES_PYTHON"))
                .transpose()?
                .map(|path| path.display().to_string()),
            hermes_python_launcher_chain: display_paths(&runtime.python_launcher_chain),
            skipped_python_launchers: display_paths(&runtime.skipped_python_launchers),
            launcher_environment_preserved: runtime.skipped_executable_launchers.is_empty()
                && runtime.skipped_python_launchers.is_empty(),
        },
    })
}

#[cfg(feature = "launcher")]
#[derive(Debug, Default)]
struct RuntimeProgramEvidence {
    program: Option<PathBuf>,
    launcher_chain: Vec<PathBuf>,
    skipped_launchers: Vec<PathBuf>,
}

#[cfg(feature = "launcher")]
fn validate_runtime_evidence(
    program: Option<&Path>,
    launcher_chain: &[PathBuf],
    skipped_launchers: &[PathBuf],
    source: &str,
) -> Result<RuntimeProgramEvidence, String> {
    let Some(program) = program else {
        if launcher_chain.is_empty() && skipped_launchers.is_empty() {
            return Ok(RuntimeProgramEvidence::default());
        }
        return Err(format!(
            "{source} launcher evidence requires a resolved executable"
        ));
    };
    let program = checked_executable_path(program, source)?;
    let launcher_chain = if launcher_chain.is_empty() {
        vec![program.clone()]
    } else {
        launcher_chain
            .iter()
            .map(|path| checked_executable_path(path, source))
            .collect::<Result<Vec<_>, _>>()?
    };
    if launcher_chain.last() != Some(&program) {
        return Err(format!(
            "{source} launcher chain must end at the exported executable {}",
            program.display()
        ));
    }
    let skipped_launchers = skipped_launchers
        .iter()
        .map(|path| checked_executable_path(path, source))
        .collect::<Result<Vec<_>, _>>()?;
    if skipped_launchers
        .iter()
        .any(|path| !launcher_chain.contains(path) || path == &program)
    {
        return Err(format!(
            "{source} skipped launchers must be non-final entries in its launcher chain"
        ));
    }
    Ok(RuntimeProgramEvidence {
        program: Some(program),
        launcher_chain,
        skipped_launchers,
    })
}

#[cfg(feature = "launcher")]
fn display_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect()
}

#[cfg(feature = "launcher")]
fn validate_profile_names(profiles: &[String]) -> Result<(), String> {
    if profiles.len() > 64 {
        return Err("at most 64 disposable Hermes profiles may be requested".to_string());
    }
    let mut unique = HashSet::new();
    for profile in profiles {
        if !is_valid_disposable_hermes_profile_name(profile) {
            return Err(format!(
                "invalid disposable Hermes profile name '{profile}'"
            ));
        }
        if !unique.insert(profile) {
            return Err(format!(
                "duplicate disposable Hermes profile name '{profile}'"
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "launcher")]
pub fn is_valid_disposable_hermes_profile_name(profile: &str) -> bool {
    profile != "default"
        && (1..=64).contains(&profile.len())
        && profile.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
}

#[cfg(feature = "launcher")]
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
    let is_supported_loopback = matches!(
        url.host(),
        Some(Host::Ipv4(address)) if address == std::net::Ipv4Addr::LOCALHOST
    );
    if !is_supported_loopback {
        return Err("Hermes loopback stub base URL must use 127.0.0.1".to_string());
    }
    let model = stub.model.trim();
    if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err("Hermes loopback stub model must be 1-256 printable characters".to_string());
    }
    Ok((url, model.to_owned()))
}

#[cfg(feature = "launcher")]
fn write_disposable_hermes_config(home: &Path, stub: &(Url, String)) -> Result<(), String> {
    let model = serde_json::to_string(&stub.1)
        .map_err(|error| format!("failed to quote disposable Hermes model: {error}"))?;
    let base_url = serde_json::to_string(stub.0.as_str())
        .map_err(|error| format!("failed to quote disposable Hermes base URL: {error}"))?;
    let config = format!(
        "model:\n  provider: openai\n  default: {model}\n  base_url: {base_url}\nbedrock:\n  discovery:\n    enabled: false\n"
    );
    fs::write(home.join("config.yaml"), config).map_err(|error| {
        format!(
            "failed to write disposable Hermes config in {}: {error}",
            home.display()
        )
    })?;
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
    Ok(())
}

#[cfg(feature = "launcher")]
pub fn resolve_parent_hermes_runtime(
    parent_home: Option<&OsStr>,
    search_path: Option<&OsStr>,
    explicit_executable: Option<&OsStr>,
    explicit_python: Option<&OsStr>,
) -> Result<ResolvedHermesRuntime, String> {
    let python = explicit_python
        .map(|program| resolve_program(program, search_path, "HERMES_PYTHON"))
        .transpose()?
        .as_deref()
        .map(|path| resolve_home_dependent_launcher(path, parent_home, "HERMES_PYTHON"))
        .transpose()?;

    let executable_candidate = match explicit_executable {
        Some(program) => Some(resolve_program(program, search_path, "HERMES_EXECUTABLE")?),
        None => parent_home
            .map(PathBuf::from)
            .map(|home| home.join(".local").join("bin").join("hermes"))
            .filter(|path| path.is_file())
            .or_else(|| find_program_in_path(OsStr::new("hermes"), search_path)),
    };
    let executable = executable_candidate
        .as_deref()
        .map(|path| resolve_home_dependent_launcher(path, parent_home, "Hermes"))
        .transpose()?;

    if executable.is_none() && python.is_none() {
        return Err(
            "disposable Hermes environment requires a parent-resolved Hermes executable or HERMES_PYTHON"
                .to_string(),
        );
    }

    let executable = resolved_launcher_evidence(executable);
    let python = resolved_launcher_evidence(python);
    Ok(ResolvedHermesRuntime {
        executable: executable.program,
        executable_launcher_chain: executable.launcher_chain,
        skipped_executable_launchers: executable.skipped_launchers,
        python: python.program,
        python_launcher_chain: python.launcher_chain,
        skipped_python_launchers: python.skipped_launchers,
    })
}

#[cfg(feature = "launcher")]
#[derive(Debug)]
struct ResolvedLauncher {
    program: PathBuf,
    chain: Vec<PathBuf>,
}

#[cfg(feature = "launcher")]
fn resolved_launcher_evidence(resolved: Option<ResolvedLauncher>) -> RuntimeProgramEvidence {
    let Some(resolved) = resolved else {
        return RuntimeProgramEvidence::default();
    };
    let skipped = resolved
        .chain
        .iter()
        .take(resolved.chain.len().saturating_sub(1))
        .cloned()
        .collect();
    RuntimeProgramEvidence {
        program: Some(resolved.program),
        launcher_chain: resolved.chain,
        skipped_launchers: skipped,
    }
}

#[cfg(feature = "launcher")]
fn resolve_program(
    program: &OsStr,
    search_path: Option<&OsStr>,
    source: &str,
) -> Result<PathBuf, String> {
    let path = Path::new(program);
    let candidate = if path.components().count() > 1 {
        path.to_path_buf()
    } else {
        find_program_in_path(program, search_path)
            .ok_or_else(|| format!("{source} executable '{}' was not found", path.display()))?
    };
    checked_executable_path(&candidate, source)
}

#[cfg(feature = "launcher")]
fn find_program_in_path(program: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(search_path?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(feature = "launcher")]
fn canonical_executable(path: &Path, source: &str) -> Result<PathBuf, String> {
    checked_executable_path(path, source)?
        .canonicalize()
        .map_err(|error| format!("failed to resolve {source} {}: {error}", path.display()))
}

#[cfg(feature = "launcher")]
fn checked_executable_path(path: &Path, source: &str) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("failed to resolve current directory: {error}"))?
            .join(path)
    };
    if !path.is_file() {
        return Err(format!(
            "{source} executable {} is not a file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = path
            .metadata()
            .map_err(|error| format!("failed to inspect {source} {}: {error}", path.display()))?
            .permissions()
            .mode()
            & 0o111
            != 0;
        if !executable {
            return Err(format!(
                "{source} executable {} is not executable",
                path.display()
            ));
        }
    }
    Ok(path)
}

#[cfg(feature = "launcher")]
fn resolved_executable_invocation(path: &Path, source: &str) -> Result<PathBuf, String> {
    let executable = checked_executable_path(path, source)?;
    let metadata = fs::symlink_metadata(&executable).map_err(|error| {
        format!(
            "failed to inspect {source} {}: {error}",
            executable.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Ok(executable);
    }
    canonical_executable(&executable, source)
}

#[cfg(feature = "launcher")]
fn resolve_home_dependent_launcher(
    path: &Path,
    parent_home: Option<&OsStr>,
    source: &str,
) -> Result<ResolvedLauncher, String> {
    resolve_home_dependent_launcher_at_depth(path, parent_home, source, 0)
}

#[cfg(feature = "launcher")]
fn resolve_home_dependent_launcher_at_depth(
    path: &Path,
    parent_home: Option<&OsStr>,
    source: &str,
    depth: usize,
) -> Result<ResolvedLauncher, String> {
    if depth > 6 {
        return Err(format!("{source} launcher indirection exceeded six levels"));
    }
    let executable = resolved_executable_invocation(path, source)?;
    let Ok(contents) = fs::read_to_string(&executable) else {
        return Ok(ResolvedLauncher {
            program: executable.clone(),
            chain: vec![executable],
        });
    };
    if !contents.contains("$HOME") && !contents.contains("${HOME}") {
        return Ok(ResolvedLauncher {
            program: executable.clone(),
            chain: vec![executable],
        });
    }
    let home = parent_home.ok_or_else(|| {
        format!(
            "{source} launcher {} depends on HOME, but the parent HOME is unavailable",
            executable.display()
        )
    })?;
    let targets = contents
        .lines()
        .find_map(|line| home_dependent_exec_targets(line, home))
        .ok_or_else(|| {
            format!(
                "{source} launcher {} depends on HOME and its executable target could not be resolved safely",
                executable.display()
            )
        })?;
    let mut chain = vec![executable];
    let mut target = targets[0].clone();
    if targets.len() > 1
        && let Some(passthrough) = argument_passthrough_launcher(&target, source)?
    {
        chain.push(passthrough);
        target = targets[1].clone();
    }
    let mut resolved =
        resolve_home_dependent_launcher_at_depth(&target, parent_home, source, depth + 1)?;
    chain.append(&mut resolved.chain);
    resolved.chain = chain;
    Ok(resolved)
}

#[cfg(feature = "launcher")]
fn home_dependent_exec_targets(line: &str, home: &OsStr) -> Option<Vec<PathBuf>> {
    let rest = line.trim_start().strip_prefix("exec")?;
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let home = home.to_string_lossy();
    let targets = shell_tokens(rest.trim())
        .into_iter()
        .take_while(|token| token != "$@")
        .map(|token| {
            token
                .replace("${HOME}", home.as_ref())
                .replace("$HOME", home.as_ref())
        })
        .take_while(|token| !token.contains('$'))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    (!targets.is_empty()).then_some(targets)
}

#[cfg(feature = "launcher")]
fn argument_passthrough_launcher(path: &Path, source: &str) -> Result<Option<PathBuf>, String> {
    let executable = resolved_executable_invocation(path, source)?;
    let Ok(contents) = fs::read_to_string(&executable) else {
        return Ok(None);
    };
    Ok(contents
        .lines()
        .any(|line| {
            let line = line.trim();
            line == "exec \"$@\"" || line == "exec '$@'" || line == "exec $@"
        })
        .then_some(executable))
}

#[cfg(feature = "launcher")]
fn shell_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut chars = input.chars().peekable();
    let mut quote = None;
    while let Some(character) = chars.next() {
        match quote {
            Some(expected) if character == expected => quote = None,
            Some('"') if character == '\\' => {
                let Some(character) = chars.next() else {
                    return Vec::new();
                };
                token.push(character);
            }
            Some(_) => token.push(character),
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            None if character == '\\' => {
                let Some(character) = chars.next() else {
                    return Vec::new();
                };
                token.push(character);
            }
            None => token.push(character),
        }
    }
    if quote.is_some() {
        return Vec::new();
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

#[cfg(feature = "launcher")]
pub fn is_provider_environment_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    let upper = key.to_ascii_uppercase();
    DEV_INSTANCE_PROVIDER_ENV_EXACT_KEYS.contains(&upper.as_str())
        || upper.ends_with("_API_KEY")
        || upper.ends_with("_AUTH_TOKEN")
        || upper.ends_with("_ACCESS_TOKEN")
        || upper.ends_with("_BEARER_TOKEN")
        || upper.ends_with("_TOKEN")
        || upper.ends_with("_SECRET_ACCESS_KEY")
        || upper.ends_with("_CLIENT_SECRET")
        || upper.ends_with("_PRIVATE_KEY")
        || upper.ends_with("_CREDENTIALS")
        || upper.ends_with("_CREDENTIALS_FILE")
        || upper.ends_with("_KEY_FILE")
        || upper.ends_with("_TOKEN_FILE")
        || upper.ends_with("_BASE_URL")
        || upper.ends_with("_API_BASE")
        || upper.ends_with("_API_BASE_URL")
        || upper.ends_with("_ENDPOINT")
        || upper.ends_with("_API_HOST")
        || upper.starts_with("VERTEX_")
        || upper.starts_with("VERTEXAI_")
}

#[cfg(feature = "launcher")]
pub fn disposable_hermes_environment_json_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(DisposableHermesEnvironment))
        .expect("disposable Hermes environment schema must serialize")
}

#[cfg(feature = "launcher")]
#[derive(Debug)]
pub struct DevInstanceStartupCleanup {
    store_dir: PathBuf,
    config_path: Option<PathBuf>,
    armed: bool,
}

#[cfg(feature = "launcher")]
impl DevInstanceStartupCleanup {
    pub fn new(store_dir: PathBuf) -> Self {
        Self {
            store_dir,
            config_path: None,
            armed: true,
        }
    }

    pub fn track_config(&mut self, config_path: PathBuf) {
        self.config_path = Some(config_path);
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(feature = "launcher")]
impl Drop for DevInstanceStartupCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(config_path) = &self.config_path {
            let _ = fs::remove_file(config_path);
        }
        let _ = fs::remove_dir_all(&self.store_dir);
    }
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
