use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use command_group::AsyncCommandGroup;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use protocol::{
    AgentInput, BackendAccessMode, BackendConfigSnapshotStatus, BackendConfigValues, BackendKind,
    BackendNativeSettingsAdvisory, BackendNativeSettingsGroup, BackendNativeSettingsProvenance,
    BackendNativeSettingsSnapshot, ChatEvent, ChatMessage, ChatMessageId, HostAbsPath,
    MessageMetadataUpdateData, MessageSender, ModelInfo, OrchestrationEvent, ReasoningData,
    SelectOption, SessionId, SessionSettingField, SessionSettingFieldType, SessionSettingValue,
    SessionSettingsSchema, SessionSettingsValues, StreamEndData, StreamTextDeltaData,
    TycodeManagedProjectionRecoveryState, TycodeProjectionId, TycodeProjectionSource,
    TycodeProjectionSourceDigest, TycodeProjectionStateHash, Version,
};

use super::{
    Backend, BackendSession, BackendSpawnConfig, BackendStartupError, EventStream,
    StartupMcpServer, StartupMcpTransport, apply_session_settings_update,
    backend_fork_unsupported_message, render_combined_spawn_instructions,
    setup::{TYCODE_VERSION, ensure_tycode_command_compatible, resolve_tycode_binary_path},
};
use crate::process_env;

async fn subprocess_bin() -> Result<String, String> {
    #[cfg(test)]
    if let Some(path) = TEST_TYCODE_SUBPROCESS_BIN
        .lock()
        .expect("test Tycode subprocess bin mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    let path =
        resolve_tycode_binary_path().ok_or_else(|| "tycode-subprocess not found".to_string())?;
    ensure_tycode_command_compatible(&path).await
}

#[cfg(test)]
static TEST_TYCODE_SUBPROCESS_BIN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_SESSIONS_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_STARTUP_TIMEOUT: std::sync::Mutex<Option<Duration>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED: std::sync::Mutex<Option<bool>> =
    std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_HOME_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
#[cfg(test)]
static TEST_TYCODE_STARTUP_PROCESS_OBSERVER: std::sync::Mutex<
    Option<TestTycodeStartupProcessObserver>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
struct TestTycodeStartupProcessObserver {
    spawned: Option<tokio::sync::oneshot::Sender<u32>>,
    reaped: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
fn install_tycode_startup_process_observer() -> (
    tokio::sync::oneshot::Receiver<u32>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (spawned_tx, spawned_rx) = tokio::sync::oneshot::channel();
    let (reaped_tx, reaped_rx) = tokio::sync::oneshot::channel();
    *TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned") =
        Some(TestTycodeStartupProcessObserver {
            spawned: Some(spawned_tx),
            reaped: Some(reaped_tx),
        });
    (spawned_rx, reaped_rx)
}

#[cfg(test)]
fn observe_tycode_startup_process_spawned(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    let mut observer = TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned");
    if let Some(spawned) = observer
        .as_mut()
        .and_then(|observer| observer.spawned.take())
    {
        let _ = spawned.send(pid);
    }
}

#[cfg(test)]
fn observe_tycode_startup_process_reaped() {
    let observer = TEST_TYCODE_STARTUP_PROCESS_OBSERVER
        .lock()
        .expect("test Tycode startup process observer mutex poisoned")
        .take();
    if let Some(reaped) = observer.and_then(|observer| observer.reaped) {
        let _ = reaped.send(());
    }
}

static TYCODE_PROJECTION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeCommandPurpose {
    NewSession,
    ResumeSession,
    NativeSettingsProbe,
    LegacyConfigProbe,
    NativeSettingsPersist,
    LegacyConfigPersist,
    PostSaveVerification,
    ProjectionNormalization,
    ProjectionVerification,
}

impl TycodeCommandPurpose {
    fn description(self) -> &'static str {
        match self {
            Self::NewSession => "new session",
            Self::ResumeSession => "resume",
            Self::NativeSettingsProbe => "native settings probe",
            Self::LegacyConfigProbe => "legacy configuration probe",
            Self::NativeSettingsPersist => "native settings save",
            Self::LegacyConfigPersist => "legacy configuration save",
            Self::PostSaveVerification => "post-save verification",
            Self::ProjectionNormalization => "managed projection normalization",
            Self::ProjectionVerification => "managed projection verification",
        }
    }
}

#[derive(Clone, Debug)]
struct TycodeProjectionPaths {
    directory: PathBuf,
    shared: PathBuf,
    managed: PathBuf,
    provenance: PathBuf,
    transaction: PathBuf,
    recovery: PathBuf,
    lock: PathBuf,
}

#[derive(Clone, Debug)]
struct TycodeManagedProjection {
    paths: TycodeProjectionPaths,
    provenance: BackendNativeSettingsProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TycodeProjectionRecord {
    provenance: BackendNativeSettingsProvenance,
    managed_digest: TycodeProjectionSourceDigest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TycodeTransactionOperation {
    Create,
    Save,
    Acknowledge,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TycodeTransactionPhase {
    Prepared,
    ProvenancePublished,
    SettingsPublished,
    Committed,
    Cleaning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TycodePairIdentity {
    projection_id: TycodeProjectionId,
    managed_digest: TycodeProjectionSourceDigest,
    provenance_digest: TycodeProjectionSourceDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TycodeTransactionArtifact {
    path: HostAbsPath,
    digest: TycodeProjectionSourceDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TycodeTransactionRecord {
    transaction_id: String,
    operation: TycodeTransactionOperation,
    phase: TycodeTransactionPhase,
    before: Option<TycodePairIdentity>,
    after: Option<TycodePairIdentity>,
    managed_stage: Option<TycodeTransactionArtifact>,
    provenance_stage: Option<TycodeTransactionArtifact>,
    managed_backup: Option<TycodeTransactionArtifact>,
    provenance_backup: Option<TycodeTransactionArtifact>,
    #[serde(default)]
    reset_artifacts: Vec<TycodeTransactionArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reset_state_hash: Option<TycodeProjectionStateHash>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TycodeRecoveryRecord {
    reason: String,
    projection_id: TycodeProjectionId,
    state_hash: TycodeProjectionStateHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TycodeLockOwner {
    owner_token: String,
    pid: u32,
    process_start_identity: String,
    created_at_ms: u64,
}

struct TycodeTempFiles(Vec<PathBuf>);

impl Drop for TycodeTempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            if fs::remove_file(path).is_ok()
                && let Some(directory) = path.parent()
            {
                let _ = sync_directory(directory);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TycodeProjectionNoticeAcknowledgementError {
    Conflict(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TycodeManagedProjectionResetError {
    Conflict(String),
    Failed(String),
}

impl std::fmt::Display for TycodeManagedProjectionResetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::fmt::Display for TycodeProjectionNoticeAcknowledgementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

fn tycode_startup_timeout() -> Duration {
    #[cfg(test)]
    if let Some(timeout) = *TEST_TYCODE_STARTUP_TIMEOUT
        .lock()
        .expect("test Tycode startup timeout mutex poisoned")
    {
        return timeout;
    }

    Duration::from_secs(30)
}

fn tycode_projection_paths() -> Result<TycodeProjectionPaths, String> {
    #[cfg(test)]
    let home = TEST_TYCODE_HOME_DIR
        .lock()
        .expect("test Tycode home dir mutex poisoned")
        .clone();
    #[cfg(not(test))]
    let home: Option<PathBuf> = None;

    let home = match home {
        Some(home) => home,
        None => crate::paths::home_dir()?,
    };
    let directory = home.join(".tycode");
    Ok(TycodeProjectionPaths {
        shared: directory.join("settings.toml"),
        managed: directory.join("tyde-settings.toml"),
        provenance: directory.join("tyde-settings.provenance.json"),
        transaction: directory.join("tyde-settings.transaction.json"),
        recovery: directory.join("tyde-settings.recovery.json"),
        lock: directory.join("tyde-settings.lock"),
        directory,
    })
}

fn path_exists_without_following(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("Failed to inspect {}: {err}", path.display())),
    }
}

#[cfg(test)]
fn wait_for_tycode_directory_creation_race() -> Result<(), String> {
    let Some(ready) = std::env::var_os("TYDE_TEST_TYCODE_DIRECTORY_RACE_READY") else {
        return Ok(());
    };
    let release = std::env::var_os("TYDE_TEST_TYCODE_DIRECTORY_RACE_RELEASE")
        .ok_or_else(|| "Tycode directory race test has no release path".to_string())?;
    fs::write(&ready, b"ready")
        .map_err(|err| format!("Failed to signal Tycode directory race readiness: {err}"))?;
    let release = PathBuf::from(release);
    for _ in 0..1_000 {
        if path_exists_without_following(&release)? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("Timed out waiting for Tycode directory race release".to_string())
}

fn ensure_private_tycode_directory(path: &Path) -> Result<(), String> {
    if !path_exists_without_following(path)? {
        #[cfg(test)]
        wait_for_tycode_directory_creation_race()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => {
                    if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o700)) {
                        return Err(format!(
                            "Failed to secure new Tycode directory {}: {err}",
                            path.display()
                        ));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(format!(
                        "Failed to create private Tycode directory {}: {err}",
                        path.display()
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(format!(
                    "Failed to create private Tycode directory {}: {err}",
                    path.display()
                ));
            }
        }
    }

    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("Failed to inspect {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Tycode settings directory {} is not a real directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "Tycode settings directory {} is group- or world-writable",
                path.display()
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("Tycode settings directory {} has no parent", path.display()))?;
        let parent_metadata = fs::metadata(parent)
            .map_err(|err| format!("Failed to inspect {}: {err}", parent.display()))?;
        if metadata.uid() != parent_metadata.uid() {
            return Err(format!(
                "Tycode settings directory {} is not owned by the home-directory owner",
                path.display()
            ));
        }
    }
    Ok(())
}

fn ensure_private_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("Failed to inspect {label} {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(format!(
                "{label} {} has unsafe permissions; expected mode 0600",
                path.display()
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("{label} {} has no parent", path.display()))?;
        let parent_metadata = fs::metadata(parent)
            .map_err(|err| format!("Failed to inspect {}: {err}", parent.display()))?;
        if metadata.uid() != parent_metadata.uid() {
            return Err(format!("{label} {} has an unsafe owner", path.display()));
        }
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("Failed to inspect {label} {}: {err}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} {} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = path
            .parent()
            .ok_or_else(|| format!("{label} {} has no parent", path.display()))?;
        let parent_metadata = fs::metadata(parent)
            .map_err(|err| format!("Failed to inspect {}: {err}", parent.display()))?;
        if metadata.uid() != parent_metadata.uid() {
            return Err(format!("{label} {} has an unsafe owner", path.display()));
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|err| format!("Failed to create private file {}: {err}", path.display()))
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = create_private_file(path)?;
    file.write_all(bytes)
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))?;
    file.sync_all()
        .map_err(|err| format!("Failed to sync {}: {err}", path.display()))
}

fn sync_file(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|err| format!("Failed to sync {}: {err}", path.display()))
}

fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            format!(
                "Failed to restrict permissions on {}: {err}",
                path.display()
            )
        })?;
    }
    ensure_private_regular_file(path, "Tycode managed settings stage")?;
    sync_file(path)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|err| format!("Failed to sync directory {}: {err}", path.display()))
}

fn tycode_process_start_identity() -> &'static str {
    static IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IDENTITY
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .as_str()
}

struct TycodeFilesystemLock {
    file: File,
    directory: PathBuf,
    owner_token: String,
}

impl Drop for TycodeFilesystemLock {
    fn drop(&mut self) {
        let mut encoded = Vec::new();
        let owns_record = self
            .file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.read_to_end(&mut encoded))
            .ok()
            .and_then(|_| serde_json::from_slice::<TycodeLockOwner>(&encoded).ok())
            .is_some_and(|owner| owner.owner_token == self.owner_token);
        if owns_record {
            let _ = self.file.set_len(0);
            let _ = self.file.seek(SeekFrom::Start(0));
            let _ = self.file.sync_all();
            let _ = sync_directory(&self.directory);
        }
        let _ = FileExt::unlock(&self.file);
    }
}

async fn acquire_tycode_filesystem_lock(
    paths: &TycodeProjectionPaths,
) -> Result<TycodeFilesystemLock, String> {
    if path_exists_without_following(&paths.lock)? {
        ensure_private_regular_file(&paths.lock, "Tycode projection lock")?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&paths.lock)
        .map_err(|err| format!("Failed to open Tycode projection lock: {err}"))?;
    let file = tokio::task::spawn_blocking(move || {
        FileExt::lock_exclusive(&file)
            .map(|()| file)
            .map_err(|err| format!("Failed to acquire Tycode projection lock: {err}"))
    })
    .await
    .map_err(|err| format!("Tycode projection lock task failed: {err}"))??;
    ensure_private_regular_file(&paths.lock, "Tycode projection lock")?;
    let owner_token = uuid::Uuid::new_v4().to_string();
    let owner = TycodeLockOwner {
        owner_token: owner_token.clone(),
        pid: std::process::id(),
        process_start_identity: tycode_process_start_identity().to_string(),
        created_at_ms: unix_now_ms(),
    };
    let encoded = serde_json::to_vec(&owner)
        .map_err(|err| format!("Failed to encode Tycode projection lock owner: {err}"))?;
    let mut file = file;
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| file.write_all(&encoded))
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("Failed to persist Tycode projection lock owner: {err}"))?;
    sync_directory(&paths.directory)?;
    Ok(TycodeFilesystemLock {
        file,
        directory: paths.directory.clone(),
        owner_token,
    })
}

fn tycode_digest(bytes: &[u8]) -> TycodeProjectionSourceDigest {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    TycodeProjectionSourceDigest(encoded)
}

fn valid_tycode_digest(digest: &TycodeProjectionSourceDigest) -> bool {
    digest
        .0
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn atomic_write_private(path: &Path, bytes: &[u8], directory: &Path) -> Result<(), String> {
    if path.file_name().and_then(|name| name.to_str()).is_none() {
        return Err(format!("Invalid private artifact path {}", path.display()));
    }
    let temp = directory.join(format!(
        ".tyde-settings.atomic-{}.txn",
        uuid::Uuid::new_v4()
    ));
    let _temp_files = TycodeTempFiles(vec![temp.clone()]);
    write_private_file(&temp, bytes)?;
    fs::rename(&temp, path)
        .map_err(|err| format!("Failed to publish {} atomically: {err}", path.display()))?;
    sync_directory(directory)
}

fn remove_file_durable(path: &Path, directory: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(directory),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("Failed to remove {}: {err}", path.display())),
    }
}

fn transaction_artifact(path: &Path) -> Result<TycodeTransactionArtifact, String> {
    ensure_private_regular_file(path, "Tycode transaction artifact")?;
    let bytes = fs::read(path).map_err(|err| {
        format!(
            "Failed to read transaction artifact {}: {err}",
            path.display()
        )
    })?;
    Ok(TycodeTransactionArtifact {
        path: projection_path_value(path),
        digest: tycode_digest(&bytes),
    })
}

fn reset_inventory_artifact(path: &Path) -> Result<TycodeTransactionArtifact, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("Failed to inspect reset artifact {}: {err}", path.display()))?;
    let digest =
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            tycode_digest(&fs::read(path).map_err(|err| {
                format!("Failed to read reset artifact {}: {err}", path.display())
            })?)
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)
                .map_err(|err| format!("Failed to read reset symlink {}: {err}", path.display()))?;
            tycode_digest(target.to_string_lossy().as_bytes())
        } else {
            tycode_digest(b"other")
        };
    Ok(TycodeTransactionArtifact {
        path: projection_path_value(path),
        digest,
    })
}

fn artifact_path(artifact: &TycodeTransactionArtifact) -> PathBuf {
    PathBuf::from(&artifact.path.0)
}

fn is_reserved_transaction_artifact_name(name: &str) -> bool {
    if let Some(identity) = name
        .strip_prefix(".tyde-settings.")
        .and_then(|name| name.strip_suffix(".tmp"))
    {
        let legacy_identity = [
            "default.",
            "source.",
            "provenance.",
            "acknowledgement.",
            "toml.",
            "provenance.json.",
            "transaction.json.",
            "recovery.json.",
            "",
        ]
        .into_iter()
        .any(|prefix| {
            identity
                .strip_prefix(prefix)
                .is_some_and(|identity| uuid::Uuid::parse_str(identity).is_ok())
        });
        if legacy_identity {
            return true;
        }
    }
    let Some(stem) = name
        .strip_prefix(".tyde-settings.")
        .and_then(|name| name.strip_suffix(".txn"))
    else {
        return false;
    };
    if stem
        .strip_prefix("atomic-")
        .is_some_and(|identity| !identity.is_empty())
    {
        return true;
    }
    if let Some(prejournal) = stem.strip_prefix("prejournal-") {
        return [
            "default",
            "source",
            "provenance",
            "acknowledgement-managed",
            "acknowledgement-provenance",
            "save-managed",
            "save-provenance",
        ]
        .into_iter()
        .any(|role| {
            prejournal
                .strip_prefix(role)
                .and_then(|identity| identity.strip_prefix('-'))
                .is_some_and(|identity| !identity.is_empty())
        });
    }
    [
        ".managed-stage",
        ".provenance-stage",
        ".managed-backup",
        ".provenance-backup",
    ]
    .into_iter()
    .any(|suffix| {
        stem.strip_suffix(suffix)
            .is_some_and(|identity| !identity.is_empty())
    })
}

fn local_reserved_artifact_path(
    paths: &TycodeProjectionPaths,
    path: PathBuf,
) -> Result<PathBuf, String> {
    if path.parent() != Some(paths.directory.as_path()) {
        return Err("Tycode transaction artifact escaped the managed directory".to_string());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Tycode transaction artifact has an invalid name".to_string())?;
    if !is_reserved_transaction_artifact_name(name) {
        return Err("Tycode transaction artifact is outside the reserved namespace".to_string());
    }
    Ok(path)
}

fn validate_transaction_artifact(
    paths: &TycodeProjectionPaths,
    transaction_id: &str,
    artifact: &TycodeTransactionArtifact,
) -> Result<PathBuf, String> {
    let path = local_reserved_artifact_path(paths, artifact_path(artifact))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Tycode transaction artifact has an invalid name".to_string())?;
    if !name.contains(transaction_id) {
        return Err("Tycode transaction artifact name does not match its transaction".to_string());
    }
    ensure_private_regular_file(&path, "Tycode transaction artifact")?;
    let bytes = fs::read(&path)
        .map_err(|err| format!("Failed to read Tycode transaction artifact: {err}"))?;
    if tycode_digest(&bytes) != artifact.digest {
        return Err("Tycode transaction artifact integrity check failed".to_string());
    }
    Ok(path)
}

fn reset_artifact_path(
    paths: &TycodeProjectionPaths,
    artifact: &TycodeTransactionArtifact,
) -> Result<PathBuf, String> {
    local_reserved_artifact_path(paths, artifact_path(artifact))
}

fn valid_projection_id(projection_id: &TycodeProjectionId) -> bool {
    let value = projection_id.0.as_str();
    !value.is_empty()
        && value == value.trim()
        && value.len() <= 256
        && !value.chars().any(char::is_control)
}

fn projection_id_from_record(record: &TycodeProjectionRecord) -> Option<TycodeProjectionId> {
    let BackendNativeSettingsProvenance::TycodeManagedProjection { projection_id, .. } =
        &record.provenance;
    valid_projection_id(projection_id).then(|| projection_id.clone())
}

fn pair_identity_from_bytes(
    managed_bytes: &[u8],
    provenance_bytes: &[u8],
) -> Result<TycodePairIdentity, String> {
    let record: TycodeProjectionRecord = serde_json::from_slice(provenance_bytes)
        .map_err(|err| format!("Failed to parse Tycode projection provenance: {err}"))?;
    if record.managed_digest != tycode_digest(managed_bytes) {
        return Err("Tycode projection pair has mismatched managed provenance".to_string());
    }
    Ok(TycodePairIdentity {
        projection_id: projection_id_from_record(&record)
            .ok_or_else(|| "Tycode projection pair has an invalid projection ID".to_string())?,
        managed_digest: tycode_digest(managed_bytes),
        provenance_digest: tycode_digest(provenance_bytes),
    })
}

fn current_pair_identity(
    paths: &TycodeProjectionPaths,
) -> Result<Option<TycodePairIdentity>, String> {
    let managed_exists = path_exists_without_following(&paths.managed)?;
    let provenance_exists = path_exists_without_following(&paths.provenance)?;
    if !managed_exists && !provenance_exists {
        return Ok(None);
    }
    if !managed_exists || !provenance_exists {
        return Err("Tycode managed projection pair is incomplete".to_string());
    }
    ensure_private_regular_file(&paths.managed, "Tyde-managed Tycode settings")?;
    ensure_private_regular_file(&paths.provenance, "Tycode projection provenance")?;
    let managed = fs::read(&paths.managed)
        .map_err(|err| format!("Failed to read Tyde-managed Tycode settings: {err}"))?;
    let provenance = fs::read(&paths.provenance)
        .map_err(|err| format!("Failed to read Tycode projection provenance: {err}"))?;
    pair_identity_from_bytes(&managed, &provenance).map(Some)
}

fn pair_matches(paths: &TycodeProjectionPaths, expected: &TycodePairIdentity) -> bool {
    current_pair_identity(paths)
        .ok()
        .flatten()
        .is_some_and(|actual| actual == *expected)
}

fn write_transaction(
    paths: &TycodeProjectionPaths,
    transaction: &TycodeTransactionRecord,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|err| format!("Failed to encode Tycode transaction journal: {err}"))?;
    atomic_write_private(&paths.transaction, &bytes, &paths.directory)
}

fn transition_transaction(
    paths: &TycodeProjectionPaths,
    transaction: &mut TycodeTransactionRecord,
    phase: TycodeTransactionPhase,
) -> Result<(), String> {
    transaction.phase = phase;
    write_transaction(paths, transaction)
}

fn publish_artifact(
    paths: &TycodeProjectionPaths,
    artifact: &TycodeTransactionArtifact,
    destination: &Path,
    transaction_id: &str,
) -> Result<(), String> {
    let source = validate_transaction_artifact(paths, transaction_id, artifact)?;
    let bytes = fs::read(&source)
        .map_err(|err| format!("Failed to read Tycode transaction artifact: {err}"))?;
    atomic_write_private(destination, &bytes, &paths.directory)
}

fn transaction_artifacts(transaction: &TycodeTransactionRecord) -> Vec<&TycodeTransactionArtifact> {
    [
        transaction.managed_stage.as_ref(),
        transaction.provenance_stage.as_ref(),
        transaction.managed_backup.as_ref(),
        transaction.provenance_backup.as_ref(),
    ]
    .into_iter()
    .flatten()
    .chain(transaction.reset_artifacts.iter())
    .collect()
}

fn load_transaction(
    paths: &TycodeProjectionPaths,
) -> Result<Option<TycodeTransactionRecord>, String> {
    if !path_exists_without_following(&paths.transaction)? {
        return Ok(None);
    }
    ensure_private_regular_file(&paths.transaction, "Tycode transaction journal")?;
    let bytes = fs::read(&paths.transaction)
        .map_err(|err| format!("Failed to read Tycode transaction journal: {err}"))?;
    let transaction: TycodeTransactionRecord = serde_json::from_slice(&bytes)
        .map_err(|err| format!("Failed to parse Tycode transaction journal: {err}"))?;
    if transaction.transaction_id.trim().is_empty() {
        return Err("Tycode transaction journal has no transaction identity".to_string());
    }
    Ok(Some(transaction))
}

fn artifact_matches_destination(
    paths: &TycodeProjectionPaths,
    transaction_id: &str,
    artifact: &TycodeTransactionArtifact,
    destination: &Path,
) -> bool {
    validate_transaction_artifact(paths, transaction_id, artifact).is_ok()
        && destination_is_transaction_owned(destination, Some(artifact), None).unwrap_or(false)
}

fn destination_is_transaction_owned(
    destination: &Path,
    stage: Option<&TycodeTransactionArtifact>,
    backup: Option<&TycodeTransactionArtifact>,
) -> Result<bool, String> {
    if !path_exists_without_following(destination)? {
        return Ok(true);
    }
    let metadata = fs::symlink_metadata(destination)
        .map_err(|err| format!("Failed to inspect Tycode transaction destination: {err}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(false);
    }
    let digest = tycode_digest(
        &fs::read(destination)
            .map_err(|err| format!("Failed to read Tycode transaction destination: {err}"))?,
    );
    Ok(stage.is_some_and(|artifact| artifact.digest == digest)
        || backup.is_some_and(|artifact| artifact.digest == digest))
}

fn cleanup_transaction_artifacts(
    paths: &TycodeProjectionPaths,
    transaction: &TycodeTransactionRecord,
) -> Result<(), String> {
    for artifact in transaction_artifacts(transaction) {
        let path = local_reserved_artifact_path(paths, artifact_path(artifact))?;
        if path_exists_without_following(&path)? {
            validate_transaction_artifact(paths, &transaction.transaction_id, artifact)?;
            remove_file_durable(&path, &paths.directory)?;
        }
    }
    remove_file_durable(&paths.transaction, &paths.directory)
}

fn restore_transaction_before(
    paths: &TycodeProjectionPaths,
    transaction: &TycodeTransactionRecord,
) -> Result<(), String> {
    match &transaction.before {
        Some(before) => {
            let managed = transaction
                .managed_backup
                .as_ref()
                .ok_or_else(|| "Tycode transaction is missing its managed backup".to_string())?;
            let provenance = transaction
                .provenance_backup
                .as_ref()
                .ok_or_else(|| "Tycode transaction is missing its provenance backup".to_string())?;
            if !destination_is_transaction_owned(
                &paths.managed,
                transaction.managed_stage.as_ref(),
                Some(managed),
            )? || !destination_is_transaction_owned(
                &paths.provenance,
                transaction.provenance_stage.as_ref(),
                Some(provenance),
            )? {
                return Err(
                    "Tycode transaction rollback encountered state owned by another writer"
                        .to_string(),
                );
            }
            publish_artifact(
                paths,
                provenance,
                &paths.provenance,
                &transaction.transaction_id,
            )?;
            publish_artifact(paths, managed, &paths.managed, &transaction.transaction_id)?;
            if !pair_matches(paths, before) {
                return Err(
                    "Tycode transaction rollback did not restore the proven prior pair".to_string(),
                );
            }
        }
        None => {
            for (destination, stage) in [
                (&paths.managed, transaction.managed_stage.as_ref()),
                (&paths.provenance, transaction.provenance_stage.as_ref()),
            ] {
                if path_exists_without_following(destination)? {
                    let Some(stage) = stage else {
                        return Err(
                            "Tycode create rollback encountered an unowned artifact".to_string()
                        );
                    };
                    if !artifact_matches_destination(
                        paths,
                        &transaction.transaction_id,
                        stage,
                        destination,
                    ) {
                        return Err(
                            "Tycode create rollback encountered changed managed state".to_string()
                        );
                    }
                    remove_file_durable(destination, &paths.directory)?;
                }
            }
        }
    }
    Ok(())
}

fn complete_transaction_after(
    paths: &TycodeProjectionPaths,
    transaction: &TycodeTransactionRecord,
) -> Result<(), String> {
    let after = transaction
        .after
        .as_ref()
        .ok_or_else(|| "Tycode transaction has no target pair".to_string())?;
    let managed = transaction
        .managed_stage
        .as_ref()
        .ok_or_else(|| "Tycode transaction is missing its managed stage".to_string())?;
    let provenance = transaction
        .provenance_stage
        .as_ref()
        .ok_or_else(|| "Tycode transaction is missing its provenance stage".to_string())?;
    if !destination_is_transaction_owned(
        &paths.managed,
        Some(managed),
        transaction.managed_backup.as_ref(),
    )? || !destination_is_transaction_owned(
        &paths.provenance,
        Some(provenance),
        transaction.provenance_backup.as_ref(),
    )? {
        return Err(
            "Tycode transaction completion encountered state owned by another writer".to_string(),
        );
    }
    publish_artifact(
        paths,
        provenance,
        &paths.provenance,
        &transaction.transaction_id,
    )?;
    publish_artifact(paths, managed, &paths.managed, &transaction.transaction_id)?;
    if !pair_matches(paths, after) {
        return Err(
            "Tycode transaction completion did not publish the proven target pair".to_string(),
        );
    }
    Ok(())
}

fn reserved_transaction_artifact_paths(
    paths: &TycodeProjectionPaths,
) -> Result<Vec<PathBuf>, String> {
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(&paths.directory)
        .map_err(|err| format!("Failed to inspect Tycode managed directory: {err}"))?
    {
        let entry =
            entry.map_err(|err| format!("Failed to inspect Tycode managed artifact: {err}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_reserved_transaction_artifact_name(name) {
            artifacts.push(entry.path());
        }
    }
    artifacts.sort();
    Ok(artifacts)
}

fn inventory_state_hash(
    paths: &TycodeProjectionPaths,
) -> Result<TycodeProjectionStateHash, String> {
    let mut inventory = vec![
        paths.managed.clone(),
        paths.provenance.clone(),
        paths.transaction.clone(),
    ];
    inventory.extend(reserved_transaction_artifact_paths(paths)?);
    inventory.sort();
    inventory.dedup();
    let mut bytes = Vec::new();
    for path in inventory {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Tycode recovery inventory contains an invalid path".to_string())?;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                bytes.extend_from_slice(b"file\0");
                let contents = fs::read(&path).map_err(|err| {
                    format!(
                        "Failed to read Tycode recovery inventory {}: {err}",
                        path.display()
                    )
                })?;
                bytes.extend_from_slice(tycode_digest(&contents).0.as_bytes());
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bytes.extend_from_slice(b"symlink\0");
                let target = fs::read_link(&path).map_err(|err| {
                    format!(
                        "Failed to read Tycode recovery symlink {}: {err}",
                        path.display()
                    )
                })?;
                let digest = tycode_digest(target.to_string_lossy().as_bytes());
                bytes.extend_from_slice(digest.0.as_bytes());
            }
            Ok(_) => bytes.extend_from_slice(b"other"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bytes.extend_from_slice(b"missing")
            }
            Err(err) => {
                return Err(format!(
                    "Failed to inspect Tycode recovery inventory {}: {err}",
                    path.display()
                ));
            }
        }
        bytes.push(b'\n');
    }
    Ok(TycodeProjectionStateHash(tycode_digest(&bytes).0))
}

fn recovery_projection_id(
    paths: &TycodeProjectionPaths,
    transaction: Option<&TycodeTransactionRecord>,
) -> TycodeProjectionId {
    load_recovery_record(paths)
        .ok()
        .flatten()
        .map(|recovery| recovery.projection_id)
        .or_else(|| {
            ensure_private_regular_file(&paths.provenance, "Tycode projection provenance")
                .and_then(|()| load_projection_record(&paths.provenance))
                .ok()
                .and_then(|record| projection_id_from_record(&record))
        })
        .or_else(|| {
            transaction
                .and_then(|transaction| transaction.after.as_ref())
                .filter(|pair| valid_projection_id(&pair.projection_id))
                .map(|pair| pair.projection_id.clone())
        })
        .or_else(|| {
            transaction
                .and_then(|transaction| transaction.before.as_ref())
                .filter(|pair| valid_projection_id(&pair.projection_id))
                .map(|pair| pair.projection_id.clone())
        })
        .unwrap_or_else(|| TycodeProjectionId(format!("recovery-{}", uuid::Uuid::new_v4())))
}

fn persist_recovery_required(
    paths: &TycodeProjectionPaths,
    reason: String,
    transaction: Option<&TycodeTransactionRecord>,
) -> Result<TycodeRecoveryRecord, String> {
    let record = TycodeRecoveryRecord {
        reason,
        projection_id: recovery_projection_id(paths, transaction),
        state_hash: inventory_state_hash(paths)?,
    };
    let bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| format!("Failed to encode Tycode recovery state: {err}"))?;
    atomic_write_private(&paths.recovery, &bytes, &paths.directory)?;
    Ok(record)
}

fn load_recovery_record(
    paths: &TycodeProjectionPaths,
) -> Result<Option<TycodeRecoveryRecord>, String> {
    if !path_exists_without_following(&paths.recovery)? {
        return Ok(None);
    }
    ensure_private_regular_file(&paths.recovery, "Tycode projection recovery state")?;
    let bytes = fs::read(&paths.recovery)
        .map_err(|err| format!("Failed to read Tycode projection recovery state: {err}"))?;
    let recovery: TycodeRecoveryRecord = serde_json::from_slice(&bytes)
        .map_err(|err| format!("Failed to parse Tycode projection recovery state: {err}"))?;
    if !valid_projection_id(&recovery.projection_id)
        || !valid_tycode_digest(&TycodeProjectionSourceDigest(recovery.state_hash.0.clone()))
    {
        return Err("Tycode projection recovery state has invalid identity or hash".to_string());
    }
    Ok(Some(recovery))
}

fn recover_reset_transaction(
    paths: &TycodeProjectionPaths,
    transaction: &TycodeTransactionRecord,
) -> Result<(), String> {
    let mut transaction = transaction.clone();
    if transaction.phase == TycodeTransactionPhase::Prepared {
        let recovery = load_recovery_record(paths)?.ok_or_else(|| {
            "Prepared Tycode reset transaction has no recovery authorization".to_string()
        })?;
        if transaction.reset_state_hash.as_ref() != Some(&recovery.state_hash) {
            return Err(
                "Prepared Tycode reset transaction has stale recovery authorization".to_string(),
            );
        }
    } else if transaction.phase != TycodeTransactionPhase::Cleaning {
        return Err("Tycode reset transaction has an invalid phase".to_string());
    }
    for artifact in &transaction.reset_artifacts {
        let path = reset_artifact_path(paths, artifact)?;
        if path_exists_without_following(&path)? {
            if reset_inventory_artifact(&path)?.digest != artifact.digest {
                return Err("Tycode reset artifact integrity check failed".to_string());
            }
            remove_file_durable(&path, &paths.directory)?;
        }
    }
    remove_file_durable(&paths.managed, &paths.directory)?;
    remove_file_durable(&paths.provenance, &paths.directory)?;
    if transaction.phase == TycodeTransactionPhase::Prepared {
        transition_transaction(paths, &mut transaction, TycodeTransactionPhase::Cleaning)?;
    }
    remove_file_durable(&paths.recovery, &paths.directory)?;
    remove_file_durable(&paths.transaction, &paths.directory)
}

fn cleanup_unjournaled_transaction_artifacts(paths: &TycodeProjectionPaths) -> Result<(), String> {
    for path in reserved_transaction_artifact_paths(paths)? {
        ensure_regular_file(&path, "Unjournaled Tycode transaction artifact")?;
        remove_file_durable(&path, &paths.directory)?;
    }
    Ok(())
}

fn recover_tycode_transaction(paths: &TycodeProjectionPaths) -> Result<(), String> {
    let loaded_transaction = load_transaction(paths);
    let loaded_recovery = match load_recovery_record(paths) {
        Ok(recovery) => recovery,
        Err(err) => {
            let transaction = loaded_transaction.as_ref().ok().and_then(Option::as_ref);
            persist_recovery_required(paths, err.clone(), transaction)?;
            return Err(err);
        }
    };
    if let Ok(Some(transaction)) = &loaded_transaction
        && transaction.operation == TycodeTransactionOperation::Reset
    {
        return match recover_reset_transaction(paths, transaction) {
            Ok(()) => Ok(()),
            Err(err) => {
                persist_recovery_required(paths, err.clone(), Some(transaction))?;
                Err(err)
            }
        };
    }
    if let Some(mut recovery) = loaded_recovery {
        let state_hash = inventory_state_hash(paths)?;
        if recovery.state_hash != state_hash {
            recovery.state_hash = state_hash;
            let bytes = serde_json::to_vec_pretty(&recovery)
                .map_err(|err| format!("Failed to encode Tycode recovery state: {err}"))?;
            atomic_write_private(&paths.recovery, &bytes, &paths.directory)?;
        }
        return Err("Tycode managed projection requires an explicit reset".to_string());
    }
    let transaction = match loaded_transaction {
        Ok(Some(transaction)) => transaction,
        Ok(None) => {
            return match cleanup_unjournaled_transaction_artifacts(paths) {
                Ok(()) => Ok(()),
                Err(err) => {
                    persist_recovery_required(paths, err.clone(), None)?;
                    Err(err)
                }
            };
        }
        Err(err) => {
            persist_recovery_required(paths, err.clone(), None)?;
            return Err(err);
        }
    };
    let result = if transaction
        .after
        .as_ref()
        .is_some_and(|after| pair_matches(paths, after))
        || transaction
            .before
            .as_ref()
            .is_some_and(|before| pair_matches(paths, before))
        || (transaction.before.is_none()
            && !path_exists_without_following(&paths.managed)?
            && !path_exists_without_following(&paths.provenance)?)
    {
        Ok(())
    } else if transaction.phase >= TycodeTransactionPhase::SettingsPublished {
        complete_transaction_after(paths, &transaction)
            .or_else(|_| restore_transaction_before(paths, &transaction))
    } else {
        restore_transaction_before(paths, &transaction)
            .or_else(|_| complete_transaction_after(paths, &transaction))
    };
    match result {
        Ok(()) => {
            let cleanup = cleanup_transaction_artifacts(paths, &transaction)
                .and_then(|()| cleanup_unjournaled_transaction_artifacts(paths));
            match cleanup {
                Ok(()) => Ok(()),
                Err(err) => {
                    let reason = format!(
                        "Tycode could not safely clean interrupted transaction {}: {err}",
                        transaction.transaction_id
                    );
                    persist_recovery_required(paths, reason.clone(), Some(&transaction))?;
                    Err(reason)
                }
            }
        }
        Err(err) => {
            let reason = format!(
                "Tycode could not prove either side of interrupted transaction {}: {err}",
                transaction.transaction_id
            );
            persist_recovery_required(paths, reason.clone(), Some(&transaction))?;
            Err(reason)
        }
    }
}

fn publish_pair_transaction(
    paths: &TycodeProjectionPaths,
    operation: TycodeTransactionOperation,
    managed_stage_path: &Path,
    provenance_stage_path: &Path,
    before: Option<TycodePairIdentity>,
) -> Result<(), String> {
    let transaction_id = uuid::Uuid::new_v4().to_string();
    let managed_stage = paths
        .directory
        .join(format!(".tyde-settings.{transaction_id}.managed-stage.txn"));
    let provenance_stage = paths.directory.join(format!(
        ".tyde-settings.{transaction_id}.provenance-stage.txn"
    ));
    fs::rename(managed_stage_path, &managed_stage)
        .map_err(|err| format!("Failed to stage Tycode managed transaction: {err}"))?;
    fs::rename(provenance_stage_path, &provenance_stage)
        .map_err(|err| format!("Failed to stage Tycode provenance transaction: {err}"))?;
    sync_directory(&paths.directory)?;
    let managed_stage = transaction_artifact(&managed_stage)?;
    let provenance_stage = transaction_artifact(&provenance_stage)?;
    let managed_bytes = fs::read(artifact_path(&managed_stage))
        .map_err(|err| format!("Failed to read staged Tycode settings: {err}"))?;
    let provenance_bytes = fs::read(artifact_path(&provenance_stage))
        .map_err(|err| format!("Failed to read staged Tycode provenance: {err}"))?;
    let after = pair_identity_from_bytes(&managed_bytes, &provenance_bytes)?;
    let (managed_backup, provenance_backup) = if before.is_some() {
        let managed_path = paths.directory.join(format!(
            ".tyde-settings.{transaction_id}.managed-backup.txn"
        ));
        let provenance_path = paths.directory.join(format!(
            ".tyde-settings.{transaction_id}.provenance-backup.txn"
        ));
        write_private_file(
            &managed_path,
            &fs::read(&paths.managed)
                .map_err(|err| format!("Failed to back up Tycode managed settings: {err}"))?,
        )?;
        write_private_file(
            &provenance_path,
            &fs::read(&paths.provenance)
                .map_err(|err| format!("Failed to back up Tycode provenance: {err}"))?,
        )?;
        sync_directory(&paths.directory)?;
        (
            Some(transaction_artifact(&managed_path)?),
            Some(transaction_artifact(&provenance_path)?),
        )
    } else {
        (None, None)
    };
    if before
        .as_ref()
        .is_some_and(|before| !pair_matches(paths, before))
    {
        return Err(
            "Tycode managed projection changed while its transaction was being staged".to_string(),
        );
    }
    if before.is_none()
        && (path_exists_without_following(&paths.managed)?
            || path_exists_without_following(&paths.provenance)?)
    {
        return Err(
            "Tycode managed projection appeared while its creation transaction was being staged"
                .to_string(),
        );
    }
    let mut transaction = TycodeTransactionRecord {
        transaction_id,
        operation,
        phase: TycodeTransactionPhase::Prepared,
        before,
        after: Some(after.clone()),
        managed_stage: Some(managed_stage),
        provenance_stage: Some(provenance_stage),
        managed_backup,
        provenance_backup,
        reset_artifacts: Vec::new(),
        reset_state_hash: None,
    };
    write_transaction(paths, &transaction)?;
    publish_artifact(
        paths,
        transaction
            .provenance_stage
            .as_ref()
            .expect("provenance stage"),
        &paths.provenance,
        &transaction.transaction_id,
    )?;
    transition_transaction(
        paths,
        &mut transaction,
        TycodeTransactionPhase::ProvenancePublished,
    )?;
    publish_artifact(
        paths,
        transaction.managed_stage.as_ref().expect("managed stage"),
        &paths.managed,
        &transaction.transaction_id,
    )?;
    transition_transaction(
        paths,
        &mut transaction,
        TycodeTransactionPhase::SettingsPublished,
    )?;
    if !pair_matches(paths, &after) {
        return Err(
            "Published Tycode transaction did not match its proven target pair".to_string(),
        );
    }
    transition_transaction(paths, &mut transaction, TycodeTransactionPhase::Committed)?;
    transition_transaction(paths, &mut transaction, TycodeTransactionPhase::Cleaning)?;
    cleanup_transaction_artifacts(paths, &transaction)
}

fn exact_tycode_version() -> Result<Version, String> {
    TYCODE_VERSION
        .parse()
        .map_err(|err| format!("Pinned Tycode version {TYCODE_VERSION} is invalid: {err}"))
}

fn projection_path_value(path: &Path) -> HostAbsPath {
    HostAbsPath(path.to_string_lossy().to_string())
}

fn raw_tycode_command(subprocess: &str, settings_path: &Path, roots_json: &str) -> Command {
    let mut command = Command::new(subprocess);
    command
        .arg("--settings-path")
        .arg(settings_path)
        .arg("--workspace-roots")
        .arg(roots_json);
    if let Some(path) = process_env::resolved_child_process_path() {
        command.env("PATH", path);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn tycode_command(
    purpose: TycodeCommandPurpose,
    roots_json: &str,
) -> Result<(Command, TycodeManagedProjection), String> {
    let projection = ensure_tyde_settings_projection().await.map_err(|err| {
        format!(
            "Cannot start Tycode {} without a verified managed settings projection: {err}",
            purpose.description()
        )
    })?;
    let subprocess = subprocess_bin().await?;
    let command = raw_tycode_command(&subprocess, &projection.paths.managed, roots_json);
    Ok((command, projection))
}

async fn tycode_migration_command(
    purpose: TycodeCommandPurpose,
    settings_path: &Path,
) -> Result<Command, String> {
    debug_assert!(matches!(
        purpose,
        TycodeCommandPurpose::ProjectionNormalization
            | TycodeCommandPurpose::ProjectionVerification
    ));
    tycode_staged_command(purpose, settings_path).await
}

async fn tycode_staged_command(
    purpose: TycodeCommandPurpose,
    settings_path: &Path,
) -> Result<Command, String> {
    let subprocess = subprocess_bin()
        .await
        .map_err(|err| format!("Cannot start Tycode {}: {err}", purpose.description()))?;
    Ok(raw_tycode_command(&subprocess, settings_path, "[]"))
}

fn validate_projection_record(
    paths: &TycodeProjectionPaths,
    record: TycodeProjectionRecord,
) -> Result<TycodeManagedProjection, String> {
    ensure_private_regular_file(&paths.managed, "Tyde-managed Tycode settings")?;
    ensure_private_regular_file(&paths.provenance, "Tycode projection provenance")?;

    let BackendNativeSettingsProvenance::TycodeManagedProjection {
        managed_settings_path,
        source_settings_path,
        tycode_version,
        projection_id,
        source_digest,
        original_unchanged,
        ..
    } = &record.provenance;
    if managed_settings_path != &projection_path_value(&paths.managed)
        || source_settings_path != &projection_path_value(&paths.shared)
    {
        return Err("Tycode managed projection provenance names unexpected paths".to_string());
    }
    if tycode_version != &exact_tycode_version()? {
        return Err(format!(
            "Tycode managed projection was created for version {tycode_version}, not pinned version {TYCODE_VERSION}"
        ));
    }
    if !valid_projection_id(projection_id)
        || !valid_tycode_digest(source_digest)
        || !valid_tycode_digest(&record.managed_digest)
        || !original_unchanged
    {
        return Err("Tycode managed projection provenance is incomplete".to_string());
    }
    let managed_bytes = fs::read(&paths.managed).map_err(|err| {
        format!(
            "Failed to read Tyde-managed Tycode settings {}: {err}",
            paths.managed.display()
        )
    })?;
    if record.managed_digest != tycode_digest(&managed_bytes) {
        return Err("Tycode managed projection integrity check failed".to_string());
    }
    Ok(TycodeManagedProjection {
        paths: paths.clone(),
        provenance: record.provenance,
    })
}

fn load_projection_record(path: &Path) -> Result<TycodeProjectionRecord, String> {
    let bytes = fs::read(path)
        .map_err(|err| format!("Failed to read Tycode projection provenance: {err}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| format!("Failed to parse Tycode projection provenance: {err}"))
}

fn existing_tycode_projection(
    paths: &TycodeProjectionPaths,
) -> Result<Option<TycodeManagedProjection>, String> {
    let managed_exists = path_exists_without_following(&paths.managed)?;
    let provenance_exists = path_exists_without_following(&paths.provenance)?;
    match (managed_exists, provenance_exists) {
        (false, false) => Ok(None),
        (false, true) => Err(
            "Tycode managed projection provenance exists without its settings pair; recovery is required"
                .to_string(),
        ),
        (true, false) => Err(format!(
            "Tyde-managed Tycode settings {} exist without provenance; refusing to adopt or overwrite them",
            paths.managed.display()
        )),
        (true, true) => {
            let record = load_projection_record(&paths.provenance)?;
            validate_projection_record(paths, record).map(Some)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeSettingsOperationPhase {
    AwaitSessionStarted,
    AwaitSettingsSchema,
    AwaitSettingsSaved,
    VerifySettingsSchema,
}

impl TycodeSettingsOperationPhase {
    fn description(self) -> &'static str {
        match self {
            Self::AwaitSessionStarted => "waiting for SessionStarted",
            Self::AwaitSettingsSchema => "waiting for SettingsSchema",
            Self::AwaitSettingsSaved => "waiting for SettingsSchema after SaveSettings",
            Self::VerifySettingsSchema => "verifying SettingsSchema in a fresh process",
        }
    }
}

enum TycodeSettingsRequiredResult<'a> {
    SessionStarted,
    SettingsSchema(&'a Value),
}

enum TycodeSettingsEventClassification<'a> {
    Continue,
    CollectAdvisory(BackendNativeSettingsAdvisory),
    RequiredResult(TycodeSettingsRequiredResult<'a>),
    Fatal(String),
}

fn tycode_message_added_error(value: &Value) -> Option<&str> {
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return None;
    }
    let data = value.get("data")?;
    let error_sender = data.get("sender").and_then(Value::as_str) == Some("Error")
        || data
            .get("sender")
            .and_then(Value::as_object)
            .is_some_and(|sender| sender.contains_key("Error"));
    error_sender
        .then(|| data.get("content").and_then(Value::as_str))
        .flatten()
}

fn tycode_structured_error(value: &Value) -> Option<&str> {
    (value.get("kind").and_then(Value::as_str) == Some("Error"))
        .then(|| value.get("data").and_then(Value::as_str))
        .flatten()
}

fn tycode_settings_advisory(message: &str) -> BackendNativeSettingsAdvisory {
    let message = tycode_text_diagnostic(message);
    let lower = message.to_ascii_lowercase();
    if lower.contains("no ai provider is configured") || lower.contains("no provider is configured")
    {
        BackendNativeSettingsAdvisory::NoProviderConfigured { message }
    } else {
        BackendNativeSettingsAdvisory::BackendReported { message }
    }
}

fn classify_tycode_settings_event(
    phase: TycodeSettingsOperationPhase,
    value: &Value,
) -> TycodeSettingsEventClassification<'_> {
    if let Some(error) = tycode_structured_error(value) {
        return TycodeSettingsEventClassification::Fatal(tycode_text_diagnostic(error));
    }
    if let Some(error) = tycode_message_added_error(value) {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::CollectAdvisory(tycode_settings_advisory(error))
        } else {
            TycodeSettingsEventClassification::Fatal(tycode_text_diagnostic(error))
        };
    }
    if tycode_session_started(value).is_some() {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SessionStarted,
            )
        } else {
            TycodeSettingsEventClassification::Fatal(
                "Tycode emitted an unexpected second SessionStarted event".to_string(),
            )
        };
    }
    if let Some(schema) = tycode_settings_schema_data(value) {
        return if phase == TycodeSettingsOperationPhase::AwaitSessionStarted {
            TycodeSettingsEventClassification::Fatal(
                "Tycode emitted SettingsSchema before SessionStarted".to_string(),
            )
        } else {
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SettingsSchema(schema),
            )
        };
    }
    TycodeSettingsEventClassification::Continue
}

fn advisory_context(advisories: &[BackendNativeSettingsAdvisory]) -> String {
    if advisories.is_empty() {
        return String::new();
    }
    let summaries = advisories
        .iter()
        .map(|advisory| match advisory {
            BackendNativeSettingsAdvisory::NoProviderConfigured { message }
            | BackendNativeSettingsAdvisory::BackendReported { message } => message.as_str(),
            BackendNativeSettingsAdvisory::UnsupportedActiveProvider { message, .. } => {
                message.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("; earlier advisory: {summaries}")
}

enum TycodeSettingsOperation {
    Probe,
    Save(Value),
    Normalize,
}

struct TycodeSettingsOperationResult {
    snapshot: BackendNativeSettingsSnapshot,
    advisories: Vec<BackendNativeSettingsAdvisory>,
}

async fn run_tycode_settings_operation(
    mut command: Command,
    purpose: TycodeCommandPurpose,
    operation: TycodeSettingsOperation,
) -> Result<TycodeSettingsOperationResult, String> {
    let mut child = command.group_spawn().map_err(|err| {
        format!(
            "Failed to spawn tycode-subprocess for {}: {err}",
            purpose.description()
        )
    })?;
    let mut stdin = child.inner().stdin.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stdin for {}",
            purpose.description()
        )
    })?;
    let stdout = child.inner().stdout.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stdout for {}",
            purpose.description()
        )
    })?;
    let stderr = child.inner().stderr.take().ok_or_else(|| {
        format!(
            "Failed to capture Tycode stderr for {}",
            purpose.description()
        )
    })?;
    let last_stderr_line = spawn_tycode_stderr_logger(stderr);
    let mut lines = BufReader::new(stdout).lines();
    let mut phase = TycodeSettingsOperationPhase::AwaitSessionStarted;
    let mut advisories = Vec::new();
    let mut normalization_requested = false;
    let deadline = tokio::time::Instant::now() + tycode_startup_timeout();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let _ = child.kill().await;
            return Err(format!(
                "Timed out after {} during Tycode {}: {}{}",
                format_tycode_timeout(tycode_startup_timeout()),
                purpose.description(),
                phase.description(),
                advisory_context(&advisories)
            ));
        }
        let line = match tokio::time::timeout(deadline - now, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                let _ = child.kill().await;
                return Err(tycode_process_exit_error(
                    &last_stderr_line,
                    &format!(
                        "Tycode process exited during {}: {}{}",
                        purpose.description(),
                        phase.description(),
                        advisory_context(&advisories)
                    ),
                ));
            }
            Ok(Err(err)) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Failed to read Tycode output during {}: {err}",
                    purpose.description()
                ));
            }
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Malformed Tycode event during {}: {err}; event {}",
                    purpose.description(),
                    tycode_line_diagnostic(trimmed)
                ));
            }
        };
        match classify_tycode_settings_event(phase, &value) {
            TycodeSettingsEventClassification::Continue => {}
            TycodeSettingsEventClassification::CollectAdvisory(advisory) => {
                advisories.push(advisory);
            }
            TycodeSettingsEventClassification::Fatal(error) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Tycode {} failed while {}: {error}{}",
                    purpose.description(),
                    phase.description(),
                    advisory_context(&advisories)
                ));
            }
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SessionStarted,
            ) => match &operation {
                TycodeSettingsOperation::Probe | TycodeSettingsOperation::Normalize => {
                    phase = if purpose == TycodeCommandPurpose::ProjectionVerification
                        || purpose == TycodeCommandPurpose::PostSaveVerification
                    {
                        TycodeSettingsOperationPhase::VerifySettingsSchema
                    } else {
                        TycodeSettingsOperationPhase::AwaitSettingsSchema
                    };
                    if !write_command(&mut stdin, &Value::String("GetSettingsSchema".to_string()))
                        .await
                    {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Failed to request Tycode SettingsSchema for {}",
                            purpose.description()
                        ));
                    }
                }
                TycodeSettingsOperation::Save(settings) => {
                    phase = TycodeSettingsOperationPhase::AwaitSettingsSaved;
                    if !write_command(
                        &mut stdin,
                        &serde_json::json!({
                            "SaveSettings": {
                                "settings": settings,
                                "persist": true,
                            }
                        }),
                    )
                    .await
                        || !write_command(
                            &mut stdin,
                            &Value::String("GetSettingsSchema".to_string()),
                        )
                        .await
                    {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Failed to send Tycode SaveSettings for {}",
                            purpose.description()
                        ));
                    }
                }
            },
            TycodeSettingsEventClassification::RequiredResult(
                TycodeSettingsRequiredResult::SettingsSchema(schema),
            ) => {
                let snapshot = match tycode_native_settings_snapshot_from_schema(schema) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        let _ = child.kill().await;
                        return Err(format!(
                            "Tycode {} returned an invalid SettingsSchema while {}: {err}{}",
                            purpose.description(),
                            phase.description(),
                            advisory_context(&advisories)
                        ));
                    }
                };
                if matches!(&operation, TycodeSettingsOperation::Normalize)
                    && !normalization_requested
                {
                    let settings = snapshot.settings.clone().ok_or_else(|| {
                        "Tycode normalization schema omitted current settings".to_string()
                    })?;
                    normalization_requested = true;
                    phase = TycodeSettingsOperationPhase::AwaitSettingsSaved;
                    if !write_command(
                        &mut stdin,
                        &serde_json::json!({
                            "SaveSettings": {
                                "settings": settings,
                                "persist": true,
                            }
                        }),
                    )
                    .await
                        || !write_command(
                            &mut stdin,
                            &Value::String("GetSettingsSchema".to_string()),
                        )
                        .await
                    {
                        let _ = child.kill().await;
                        return Err(
                            "Failed to normalize the staged Tycode settings projection".to_string()
                        );
                    }
                    continue;
                }
                let _ = child.kill().await;
                return Ok(TycodeSettingsOperationResult {
                    snapshot,
                    advisories,
                });
            }
        }
    }
}

fn add_snapshot_advisories(
    snapshot: &mut BackendNativeSettingsSnapshot,
    advisories: &mut Vec<BackendNativeSettingsAdvisory>,
) {
    let Some(settings) = snapshot.settings.as_ref().and_then(Value::as_object) else {
        return;
    };
    let active_provider = settings
        .get("active_provider")
        .and_then(Value::as_str)
        .filter(|provider| !provider.trim().is_empty());
    let providers = settings.get("providers").and_then(Value::as_object);
    if let Some(provider) = active_provider {
        if !providers.is_some_and(|providers| providers.contains_key(provider)) {
            advisories.push(BackendNativeSettingsAdvisory::UnsupportedActiveProvider {
                provider: provider.to_string(),
                message: format!(
                    "Tycode v{TYCODE_VERSION} cannot model active provider '{provider}' in Tyde's managed copy. Choose a supported provider. Tyde does not write to or remove the shared Tycode CLI/VS Code settings file."
                ),
            });
        }
    } else if !advisories.iter().any(|advisory| {
        matches!(
            advisory,
            BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
        )
    }) {
        advisories.push(BackendNativeSettingsAdvisory::NoProviderConfigured {
            message: "No Tycode provider is configured. Choose a provider to use Tycode."
                .to_string(),
        });
    }
}

fn tycode_settings_are_semantically_default(settings: &Value, defaults: &Value) -> bool {
    let mut comparable = settings.clone();
    let Some(comparable_settings) = comparable.as_object_mut() else {
        return false;
    };
    if comparable_settings
        .get("default_agent")
        .and_then(Value::as_str)
        .is_some_and(|agent| agent.trim().is_empty())
    {
        let Some(default_agent) = defaults.get("default_agent") else {
            return false;
        };
        comparable_settings.insert("default_agent".to_string(), default_agent.clone());
    }
    &comparable == defaults
}

async fn create_tyde_settings_projection(
    paths: &TycodeProjectionPaths,
) -> Result<TycodeManagedProjection, String> {
    let source_exists = path_exists_without_following(&paths.shared)?;
    let source_bytes = if source_exists {
        ensure_regular_file(&paths.shared, "Shared Tycode settings")?;
        fs::read(&paths.shared)
            .map_err(|err| format!("Failed to snapshot shared Tycode settings: {err}"))?
    } else {
        Vec::new()
    };
    let source = if source_exists {
        TycodeProjectionSource::SharedSettings
    } else {
        TycodeProjectionSource::Defaults
    };
    let nonce = uuid::Uuid::new_v4();
    let default_temp = paths
        .directory
        .join(format!(".tyde-settings.prejournal-default-{nonce}.txn"));
    let source_temp = paths
        .directory
        .join(format!(".tyde-settings.prejournal-source-{nonce}.txn"));
    let provenance_temp = paths
        .directory
        .join(format!(".tyde-settings.prejournal-provenance-{nonce}.txn"));
    let _temp_files = TycodeTempFiles(vec![
        default_temp.clone(),
        source_temp.clone(),
        provenance_temp.clone(),
    ]);

    async {
        let default_command = tycode_migration_command(
            TycodeCommandPurpose::ProjectionVerification,
            &default_temp,
        )
        .await?;
        let initialized_defaults = run_tycode_settings_operation(
            default_command,
            TycodeCommandPurpose::ProjectionVerification,
            TycodeSettingsOperation::Probe,
        )
        .await?;
        set_private_file_permissions(&default_temp)?;

        let default_verification_command = tycode_migration_command(
            TycodeCommandPurpose::ProjectionVerification,
            &default_temp,
        )
        .await?;
        let verified_defaults = run_tycode_settings_operation(
            default_verification_command,
            TycodeCommandPurpose::ProjectionVerification,
            TycodeSettingsOperation::Probe,
        )
        .await?;
        if initialized_defaults.snapshot.settings != verified_defaults.snapshot.settings
            || initialized_defaults.snapshot.groups != verified_defaults.snapshot.groups
        {
            return Err(
                "Fresh-process verification of Tycode's nonexistent-path defaults did not match initialization"
                    .to_string(),
            );
        }

        let chosen_settings = if source_exists {
            write_private_file(&source_temp, &source_bytes)?;
            let source_probe_command = tycode_migration_command(
                TycodeCommandPurpose::ProjectionVerification,
                &source_temp,
            )
            .await?;
            let source_probe = run_tycode_settings_operation(
                source_probe_command,
                TycodeCommandPurpose::ProjectionVerification,
                TycodeSettingsOperation::Probe,
            )
            .await?;
            let source_settings = source_probe.snapshot.settings.as_ref().ok_or_else(|| {
                "Tycode source projection probe omitted current settings".to_string()
            })?;
            let default_settings = verified_defaults.snapshot.settings.as_ref().ok_or_else(|| {
                "Tycode default projection probe omitted current settings".to_string()
            })?;
            if tycode_settings_are_semantically_default(source_settings, default_settings) {
                &default_temp
            } else {
                let normalization_command = tycode_migration_command(
                    TycodeCommandPurpose::ProjectionNormalization,
                    &source_temp,
                )
                .await?;
                let normalized = run_tycode_settings_operation(
                    normalization_command,
                    TycodeCommandPurpose::ProjectionNormalization,
                    TycodeSettingsOperation::Normalize,
                )
                .await?;
                set_private_file_permissions(&source_temp)?;
                let verification_command = tycode_migration_command(
                    TycodeCommandPurpose::ProjectionVerification,
                    &source_temp,
                )
                .await?;
                let verified = run_tycode_settings_operation(
                    verification_command,
                    TycodeCommandPurpose::ProjectionVerification,
                    TycodeSettingsOperation::Probe,
                )
                .await?;
                if normalized.snapshot.settings != verified.snapshot.settings
                    || normalized.snapshot.groups != verified.snapshot.groups
                {
                    return Err(
                        "Fresh-process verification of the Tycode settings projection did not match normalization"
                            .to_string(),
                    );
                }
                &source_temp
            }
        } else {
            &default_temp
        };

        let source_unchanged = if source_exists {
            fs::read(&paths.shared)
                .is_ok_and(|bytes| bytes == source_bytes)
        } else {
            !path_exists_without_following(&paths.shared)?
        };
        if !source_unchanged {
            return Err(
                "Shared Tycode settings changed while Tyde was creating its managed projection; no projection was published"
                    .to_string(),
            );
        }

        sync_file(chosen_settings)?;
        let managed_bytes = fs::read(chosen_settings)
            .map_err(|err| format!("Failed to read staged Tycode settings: {err}"))?;
        let provenance = BackendNativeSettingsProvenance::TycodeManagedProjection {
            managed_settings_path: projection_path_value(&paths.managed),
            source_settings_path: projection_path_value(&paths.shared),
            source,
            tycode_version: exact_tycode_version()?,
            projection_id: TycodeProjectionId(uuid::Uuid::new_v4().to_string()),
            created_at_ms: unix_now_ms(),
            source_digest: tycode_digest(&source_bytes),
            original_unchanged: true,
            notice_pending: true,
        };
        let record = TycodeProjectionRecord {
            provenance,
            managed_digest: tycode_digest(&managed_bytes),
        };
        let provenance_bytes = serde_json::to_vec_pretty(&record)
            .map_err(|err| format!("Failed to encode Tycode projection provenance: {err}"))?;
        write_private_file(&provenance_temp, &provenance_bytes)?;
        publish_pair_transaction(
            paths,
            TycodeTransactionOperation::Create,
            chosen_settings,
            &provenance_temp,
            None,
        )?;
        validate_projection_record(paths, record)
    }
    .await
}

async fn ensure_tyde_settings_projection() -> Result<TycodeManagedProjection, String> {
    let _guard = TYCODE_PROJECTION_LOCK.lock().await;
    let paths = tycode_projection_paths()?;
    ensure_private_tycode_directory(&paths.directory)?;
    let _filesystem_lock = acquire_tycode_filesystem_lock(&paths).await?;
    recover_tycode_transaction(&paths)?;
    match existing_tycode_projection(&paths) {
        Ok(Some(projection)) => return Ok(projection),
        Ok(None) => {}
        Err(err) => {
            persist_recovery_required(&paths, err.clone(), None)?;
            return Err(err);
        }
    }
    create_tyde_settings_projection(&paths).await
}

pub(crate) async fn acknowledge_tycode_projection_notice(
    projection_id: &TycodeProjectionId,
) -> Result<(), TycodeProjectionNoticeAcknowledgementError> {
    let _guard = TYCODE_PROJECTION_LOCK.lock().await;
    let paths =
        tycode_projection_paths().map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    ensure_private_tycode_directory(&paths.directory)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    let _filesystem_lock = acquire_tycode_filesystem_lock(&paths)
        .await
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    recover_tycode_transaction(&paths)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    existing_tycode_projection(&paths)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?
        .ok_or_else(|| {
            TycodeProjectionNoticeAcknowledgementError::Failed(
                "Tycode managed projection does not exist".to_string(),
            )
        })?;
    let before = current_pair_identity(&paths)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    let mut record = load_projection_record(&paths.provenance)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    let BackendNativeSettingsProvenance::TycodeManagedProjection {
        projection_id: current_id,
        notice_pending,
        ..
    } = &mut record.provenance;
    if current_id != projection_id {
        return Err(TycodeProjectionNoticeAcknowledgementError::Conflict(
            "Tycode projection notice belongs to a newer managed projection".to_string(),
        ));
    }
    if !*notice_pending {
        return Ok(());
    }
    *notice_pending = false;
    let nonce = uuid::Uuid::new_v4();
    let provenance_temp = paths.directory.join(format!(
        ".tyde-settings.prejournal-acknowledgement-provenance-{nonce}.txn"
    ));
    let managed_temp = paths.directory.join(format!(
        ".tyde-settings.prejournal-acknowledgement-managed-{nonce}.txn"
    ));
    let _temp_files = TycodeTempFiles(vec![managed_temp.clone(), provenance_temp.clone()]);
    let bytes = serde_json::to_vec_pretty(&record).map_err(|err| {
        TycodeProjectionNoticeAcknowledgementError::Failed(format!(
            "Failed to encode Tycode projection acknowledgement: {err}"
        ))
    })?;
    write_private_file(
        &managed_temp,
        &fs::read(&paths.managed).map_err(|err| {
            TycodeProjectionNoticeAcknowledgementError::Failed(format!(
                "Failed to stage Tycode managed settings acknowledgement: {err}"
            ))
        })?,
    )
    .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    write_private_file(&provenance_temp, &bytes)
        .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)?;
    publish_pair_transaction(
        &paths,
        TycodeTransactionOperation::Acknowledge,
        &managed_temp,
        &provenance_temp,
        before,
    )
    .map_err(TycodeProjectionNoticeAcknowledgementError::Failed)
}

pub(crate) async fn reset_tycode_managed_projection(
    expected_projection_id: &TycodeProjectionId,
    expected_state_hash: &TycodeProjectionStateHash,
) -> Result<(), TycodeManagedProjectionResetError> {
    let _guard = TYCODE_PROJECTION_LOCK.lock().await;
    let paths = tycode_projection_paths().map_err(TycodeManagedProjectionResetError::Failed)?;
    ensure_private_tycode_directory(&paths.directory)
        .map_err(TycodeManagedProjectionResetError::Failed)?;
    let _filesystem_lock = acquire_tycode_filesystem_lock(&paths)
        .await
        .map_err(TycodeManagedProjectionResetError::Failed)?;
    match recover_tycode_transaction(&paths) {
        Ok(()) => {
            return Err(TycodeManagedProjectionResetError::Conflict(
                "Tycode managed projection recovery completed automatically; reset is no longer required"
                    .to_string(),
            ));
        }
        Err(error) if !path_exists_without_following(&paths.recovery).unwrap_or(false) => {
            return Err(TycodeManagedProjectionResetError::Failed(error));
        }
        Err(_) => {}
    }
    let recovery = load_recovery_record(&paths)
        .map_err(TycodeManagedProjectionResetError::Failed)?
        .ok_or_else(|| {
            TycodeManagedProjectionResetError::Conflict(
                "Tycode managed projection no longer requires reset".to_string(),
            )
        })?;
    let current_hash =
        inventory_state_hash(&paths).map_err(TycodeManagedProjectionResetError::Failed)?;
    if &recovery.projection_id != expected_projection_id
        || &recovery.state_hash != expected_state_hash
        || current_hash != recovery.state_hash
    {
        return Err(TycodeManagedProjectionResetError::Conflict(
            "Tycode managed projection changed after reset was offered; refresh before retrying"
                .to_string(),
        ));
    }

    let transaction_id = uuid::Uuid::new_v4().to_string();
    let mut reset_artifacts = Vec::new();
    for path in reserved_transaction_artifact_paths(&paths)
        .map_err(TycodeManagedProjectionResetError::Failed)?
    {
        reset_artifacts.push(
            reset_inventory_artifact(&path).map_err(TycodeManagedProjectionResetError::Failed)?,
        );
    }
    let reset = TycodeTransactionRecord {
        transaction_id,
        operation: TycodeTransactionOperation::Reset,
        phase: TycodeTransactionPhase::Prepared,
        before: None,
        after: None,
        managed_stage: None,
        provenance_stage: None,
        managed_backup: None,
        provenance_backup: None,
        reset_artifacts,
        reset_state_hash: Some(current_hash),
    };
    write_transaction(&paths, &reset).map_err(TycodeManagedProjectionResetError::Failed)?;
    recover_reset_transaction(&paths, &reset).map_err(TycodeManagedProjectionResetError::Failed)
}

pub struct TycodeBackend {
    input_tx: mpsc::UnboundedSender<AgentInput>,
    interrupt_tx: mpsc::UnboundedSender<()>,
    shutdown_tx: mpsc::UnboundedSender<()>,
    session_id: Arc<std::sync::Mutex<Option<SessionId>>>,
}

enum TycodeStdinCommand {
    Json(Value),
    Cancel,
}

struct TempWorkspaceRoot {
    path: PathBuf,
}

impl TempWorkspaceRoot {
    fn new(prefix: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).map_err(|err| {
            format!(
                "Failed to create temporary workspace {}: {err}",
                path.display()
            )
        })?;
        Ok(Self { path })
    }
}

impl Drop for TempWorkspaceRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_text_file(path: &PathBuf, body: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("Path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("Failed to create directory {}: {err}", parent.display()))?;
    fs::write(path, body).map_err(|err| format!("Failed to write {}: {err}", path.display()))
}

fn materialize_tycode_customization(
    config: &BackendSpawnConfig,
) -> Result<Option<TempWorkspaceRoot>, String> {
    let steering = render_combined_spawn_instructions(&config.resolved_spawn_config);
    if steering.is_none() && config.resolved_spawn_config.skills.is_empty() {
        return Ok(None);
    }
    let root = TempWorkspaceRoot::new("tyde-tycode-customization")?;
    if let Some(steering) = steering {
        write_text_file(
            &root.path.join(".tycode").join("tyde_steering.md"),
            &steering,
        )?;
    }
    for skill in &config.resolved_spawn_config.skills {
        write_text_file(
            &root
                .path
                .join(".tycode")
                .join("skills")
                .join(&skill.name)
                .join("SKILL.md"),
            &skill.body,
        )?;
    }
    Ok(Some(root))
}

fn tycode_read_only_agent_json(config: &BackendSpawnConfig) -> Option<String> {
    if config.resolved_spawn_config.access_mode != BackendAccessMode::ReadOnly {
        return None;
    }
    let system_prompt = render_combined_spawn_instructions(&config.resolved_spawn_config)
        .unwrap_or_else(|| {
            "Backend access mode is read-only: inspect files and call configured MCP tools only."
                .to_string()
        });
    Some(
        serde_json::json!({
            "name": "tyde-read-only",
            "description": "Tyde read-only agent",
            "systemPrompt": system_prompt,
            "tools": [
                "set_tracked_files",
                "search_types",
                "get_type_docs",
                "run_build_test"
            ]
        })
        .to_string(),
    )
}

fn tycode_session_settings_schema() -> SessionSettingsSchema {
    if !tycode_set_root_agent_supported() {
        return SessionSettingsSchema {
            backend_kind: BackendKind::Tycode,
            fields: Vec::new(),
        };
    }

    SessionSettingsSchema {
        backend_kind: BackendKind::Tycode,
        fields: vec![SessionSettingField {
            key: "default_agent".to_string(),
            label: "Orchestration".to_string(),
            description: Some(
                "Controls Tycode's session root agent: None runs one agent, Auto lets Tycode \
                 delegate as needed, Pipeline runs the builder workflow, and Swarm runs the \
                 fan-out integration workflow."
                    .to_string(),
            ),
            field_type: SessionSettingFieldType::Select {
                options: vec![
                    select_option("one_shot", "None"),
                    select_option("tycode", "Auto"),
                    select_option("builder", "Pipeline"),
                    select_option("swarm", "Swarm"),
                ],
                default: Some("tycode".to_string()),
                nullable: false,
            },
            use_slider: true,
            select_options_by_setting: None,
        }],
    }
}

pub(crate) fn resolve_session_settings(config: &BackendSpawnConfig) -> SessionSettingsValues {
    let mut resolved = SessionSettingsValues::default();
    if let Some(session_settings) = config.session_settings.as_ref() {
        apply_session_settings_update(&mut resolved, session_settings);
    }
    resolved
}

fn select_option(value: &str, label: &str) -> SelectOption {
    SelectOption {
        value: value.to_string(),
        label: label.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TycodeSettingsOverlay {
    settings: Value,
    active_provider_change: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TycodeSettingsOverlayMode {
    SessionRuntime,
    PersistentSettingsPanel,
}

#[cfg(test)]
fn apply_tycode_backend_config_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
    mode: TycodeSettingsOverlayMode,
) -> Result<TycodeSettingsOverlay, String> {
    apply_tycode_settings_overlay(
        current_settings,
        config,
        &SessionSettingsValues::default(),
        mode,
    )
}

fn apply_tycode_settings_overlay(
    current_settings: &Value,
    config: &BackendConfigValues,
    _session_settings: &SessionSettingsValues,
    mode: TycodeSettingsOverlayMode,
) -> Result<TycodeSettingsOverlay, String> {
    let mut settings = current_settings.clone();
    let object = settings
        .as_object_mut()
        .ok_or_else(|| "Tycode Settings event data must be a JSON object".to_string())?;

    if mode == TycodeSettingsOverlayMode::SessionRuntime
        && object.contains_key("orchestration_progress_messages")
    {
        object.insert(
            "orchestration_progress_messages".to_string(),
            Value::Bool(false),
        );
    }

    let mut active_provider_change = None;
    for (key, value) in &config.0 {
        match (key.as_str(), value) {
            ("active_provider", SessionSettingValue::String(provider)) => {
                let provider = provider.trim();
                if provider.is_empty() {
                    return Err("Tycode active_provider must not be empty".to_string());
                }
                let providers = object
                    .get("providers")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        "Tycode settings missing providers object while validating active_provider"
                            .to_string()
                    })?;
                if !providers.contains_key(provider) {
                    let available = providers.keys().cloned().collect::<Vec<_>>().join(", ");
                    return Err(format!(
                        "Configured Tycode active_provider '{provider}' is absent from returned providers{}",
                        if available.is_empty() {
                            String::new()
                        } else {
                            format!(" (available: {available})")
                        }
                    ));
                }
                active_provider_change = Some(provider.to_string());
                object.insert(
                    "active_provider".to_string(),
                    Value::String(provider.to_string()),
                );
            }
            ("active_provider", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("active_provider".to_string(), Value::Null);
                }
            }
            ("model_quality", SessionSettingValue::String(model_quality)) => {
                object.insert(
                    "model_quality".to_string(),
                    Value::String(model_quality.clone()),
                );
            }
            ("model_quality", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("model_quality".to_string(), Value::Null);
                }
                continue;
            }
            ("reasoning_effort", SessionSettingValue::String(reasoning_effort)) => {
                object.insert(
                    "reasoning_effort".to_string(),
                    Value::String(reasoning_effort.clone()),
                );
            }
            ("reasoning_effort", SessionSettingValue::Null) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert("reasoning_effort".to_string(), Value::Null);
                }
                continue;
            }
            (
                "autonomy_level" | "review_level" | "spawn_context_mode",
                SessionSettingValue::String(setting),
            ) => {
                object.insert(key.clone(), Value::String(setting.clone()));
            }
            (
                "autonomy_level" | "review_level" | "spawn_context_mode",
                SessionSettingValue::Null,
            ) => {
                if mode == TycodeSettingsOverlayMode::PersistentSettingsPanel {
                    object.insert(key.clone(), tycode_managed_setting_default(key));
                }
                continue;
            }
            ("active_provider", _) => {
                return Err(
                    "Tycode active_provider backend config must be a string or null".to_string(),
                );
            }
            ("model_quality" | "reasoning_effort", _) => {
                return Err(format!(
                    "Tycode {key} backend config must be a string or null"
                ));
            }
            ("autonomy_level" | "review_level" | "spawn_context_mode", _) => {
                return Err(format!(
                    "Tycode {key} backend config must be a string or null"
                ));
            }
            _ => {}
        }
    }

    Ok(TycodeSettingsOverlay {
        settings,
        active_provider_change,
    })
}

const TYCODE_MANAGED_SETTINGS: &[&str] = &[
    "active_provider",
    "model_quality",
    "reasoning_effort",
    "autonomy_level",
    "review_level",
    "spawn_context_mode",
];

fn tycode_managed_setting_default(key: &str) -> Value {
    match key {
        "active_provider" | "model_quality" | "reasoning_effort" => Value::Null,
        "autonomy_level" => Value::String("plan_approval_required".to_string()),
        "review_level" => Value::String("None".to_string()),
        "spawn_context_mode" => Value::String("Fork".to_string()),
        _ => unreachable!("unmanaged Tycode setting default requested: {key}"),
    }
}

pub(crate) fn tycode_backend_config_persistence_values(
    incoming: &BackendConfigValues,
    previous: &BackendConfigValues,
) -> BackendConfigValues {
    let mut values = incoming.clone();
    if incoming.0.is_empty() {
        for key in TYCODE_MANAGED_SETTINGS {
            if previous.0.contains_key(*key) {
                values
                    .0
                    .insert((*key).to_string(), SessionSettingValue::Null);
            }
        }
    }
    values
}

pub(crate) fn validate_runtime_session_settings_update(
    update: &SessionSettingsValues,
) -> Result<(), String> {
    if update.0.contains_key("default_agent") {
        return Err(
            "Tycode default_agent cannot be changed on a running session; start a new Tycode \
             session with the desired orchestration setting"
                .to_string(),
        );
    }
    Ok(())
}

enum TycodeStartupFollowUp {
    InitialUserInput(String),
    ResumeSession { session_id: String },
}

enum TycodeStartupPhase {
    AwaitSessionStarted,
    AwaitInitialSettings,
    AwaitVerification {
        expected_settings: Value,
        active_provider_change: Option<String>,
    },
    AwaitProviderChange {
        provider: String,
    },
    AwaitRootAgentChanged {
        agent: String,
    },
    Complete,
}

enum TycodeStartupObservation {
    Allow,
    Suppress,
    Completed,
}

#[derive(Clone, Copy)]
enum TycodeRootAgentOverridePolicy {
    Supported,
    UnsupportedPinnedVersion,
    DisabledForReadOnly,
}

fn tycode_set_root_agent_supported() -> bool {
    #[cfg(test)]
    if let Some(supported) = *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
        .lock()
        .expect("test Tycode SetRootAgent support mutex poisoned")
    {
        return supported;
    }

    true
}

fn tycode_root_agent_override_policy(config: &BackendSpawnConfig) -> TycodeRootAgentOverridePolicy {
    if config.resolved_spawn_config.access_mode == BackendAccessMode::ReadOnly {
        return TycodeRootAgentOverridePolicy::DisabledForReadOnly;
    }
    if tycode_set_root_agent_supported() {
        TycodeRootAgentOverridePolicy::Supported
    } else {
        TycodeRootAgentOverridePolicy::UnsupportedPinnedVersion
    }
}

struct TycodeStartupController {
    backend_config: BackendConfigValues,
    session_settings: SessionSettingsValues,
    root_agent_override_policy: TycodeRootAgentOverridePolicy,
    phase: TycodeStartupPhase,
    follow_up: TycodeStartupFollowUp,
    persist_settings: bool,
    runtime_settings: Option<Value>,
}

impl TycodeStartupController {
    fn new(
        backend_config: BackendConfigValues,
        session_settings: SessionSettingsValues,
        root_agent_override_policy: TycodeRootAgentOverridePolicy,
        follow_up: TycodeStartupFollowUp,
        persist_settings: bool,
    ) -> Self {
        Self {
            backend_config,
            session_settings,
            root_agent_override_policy,
            phase: TycodeStartupPhase::AwaitSessionStarted,
            follow_up,
            persist_settings,
            runtime_settings: None,
        }
    }

    fn observe(
        &mut self,
        value: &Value,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<TycodeStartupObservation, String> {
        match &mut self.phase {
            TycodeStartupPhase::AwaitSessionStarted => {
                if tycode_session_started(value).is_some() {
                    send_tycode_json(stdin_tx, Value::String("GetSettings".to_string()))?;
                    self.phase = TycodeStartupPhase::AwaitInitialSettings;
                }
                Ok(TycodeStartupObservation::Allow)
            }
            TycodeStartupPhase::AwaitInitialSettings => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode settings initialization failed before Settings: {error}"
                    ));
                }
                if let Some(settings) = tycode_settings_data(value) {
                    let overlay = apply_tycode_settings_overlay(
                        settings,
                        &self.backend_config,
                        &self.session_settings,
                        if self.persist_settings {
                            TycodeSettingsOverlayMode::PersistentSettingsPanel
                        } else {
                            TycodeSettingsOverlayMode::SessionRuntime
                        },
                    )
                    .map_err(|err| format!("Failed to apply Tycode settings overlay: {err}"))?;
                    send_tycode_json(
                        stdin_tx,
                        serde_json::json!({
                            "SaveSettings": {
                                "settings": overlay.settings.clone(),
                                "persist": self.persist_settings,
                            }
                        }),
                    )?;
                    send_tycode_json(stdin_tx, Value::String("GetSettings".to_string()))?;
                    self.phase = TycodeStartupPhase::AwaitVerification {
                        expected_settings: overlay.settings,
                        active_provider_change: overlay.active_provider_change,
                    };
                    return Ok(TycodeStartupObservation::Suppress);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitVerification {
                expected_settings,
                active_provider_change,
            } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode settings SaveSettings/verification failed: {error}"
                    ));
                }
                if let Some(settings) = tycode_settings_data(value) {
                    verify_tycode_settings_overlay(expected_settings, settings)?;
                    self.runtime_settings = Some(settings.clone());
                    if let Some(provider) = active_provider_change.take() {
                        send_tycode_json(
                            stdin_tx,
                            serde_json::json!({ "ChangeProvider": provider }),
                        )?;
                        self.phase = TycodeStartupPhase::AwaitProviderChange { provider };
                    } else {
                        return self.send_root_agent_or_follow_up(stdin_tx);
                    }
                    return Ok(TycodeStartupObservation::Suppress);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitProviderChange { provider } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!(
                        "Tycode ChangeProvider '{provider}' failed: {error}"
                    ));
                }
                if tycode_provider_changed_message(value, provider) {
                    return self.send_root_agent_or_follow_up(stdin_tx);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::AwaitRootAgentChanged { agent } => {
                if let Some(error) = tycode_error_message(value) {
                    return Err(format!("Tycode SetRootAgent '{agent}' failed: {error}"));
                }
                if let Some(changed_agent) = tycode_root_agent_changed(value) {
                    if changed_agent != agent {
                        return Err(format!(
                            "Tycode SetRootAgent '{agent}' acknowledged unexpected root agent '{changed_agent}'"
                        ));
                    }
                    self.send_follow_up(stdin_tx)?;
                    self.phase = TycodeStartupPhase::Complete;
                    return Ok(TycodeStartupObservation::Completed);
                }
                Ok(tycode_startup_internal_observation(value))
            }
            TycodeStartupPhase::Complete => Ok(TycodeStartupObservation::Allow),
        }
    }

    fn runtime_settings(&self) -> Option<&Value> {
        self.runtime_settings.as_ref()
    }

    fn send_root_agent_or_follow_up(
        &mut self,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<TycodeStartupObservation, String> {
        if let Some(agent) = self.requested_root_agent()? {
            send_tycode_json(
                stdin_tx,
                serde_json::json!({ "SetRootAgent": { "agent": agent } }),
            )?;
            self.phase = TycodeStartupPhase::AwaitRootAgentChanged { agent };
            return Ok(TycodeStartupObservation::Suppress);
        }
        self.send_follow_up(stdin_tx)?;
        self.phase = TycodeStartupPhase::Complete;
        Ok(TycodeStartupObservation::Completed)
    }

    fn requested_root_agent(&self) -> Result<Option<String>, String> {
        if !matches!(self.follow_up, TycodeStartupFollowUp::InitialUserInput(_)) {
            return Ok(None);
        }
        match self.session_settings.0.get("default_agent") {
            Some(SessionSettingValue::String(agent))
                if matches!(agent.as_str(), "one_shot" | "tycode" | "builder" | "swarm") =>
            {
                match self.root_agent_override_policy {
                    TycodeRootAgentOverridePolicy::Supported => Ok(Some(agent.clone())),
                    TycodeRootAgentOverridePolicy::UnsupportedPinnedVersion => Err(format!(
                        "Tycode default_agent session setting requires SetRootAgent support, but \
                         the selected tycode-subprocess does not support that protocol; Tyde \
                         requires Tycode {TYCODE_VERSION}"
                    )),
                    TycodeRootAgentOverridePolicy::DisabledForReadOnly => Err(
                        "Tycode default_agent session setting cannot be used with read-only \
                         Tycode sessions because it would replace Tyde's read-only root agent"
                            .to_string(),
                    ),
                }
            }
            Some(SessionSettingValue::String(agent)) => Err(format!(
                "Tycode default_agent session setting has unsupported value '{agent}'"
            )),
            Some(_) => Err("Tycode default_agent session setting must be a string".to_string()),
            None => Ok(None),
        }
    }

    fn send_follow_up(
        &self,
        stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    ) -> Result<(), String> {
        match &self.follow_up {
            TycodeStartupFollowUp::InitialUserInput(message) => {
                send_tycode_json(stdin_tx, serde_json::json!({ "UserInput": message }))
            }
            TycodeStartupFollowUp::ResumeSession { session_id } => {
                send_tycode_json(
                    stdin_tx,
                    serde_json::json!({
                        "ResumeSession": { "session_id": session_id }
                    }),
                )?;
                send_tycode_json(stdin_tx, Value::String("ListSessions".to_string()))
            }
        }
    }

    fn phase_description(&self) -> &'static str {
        match self.phase {
            TycodeStartupPhase::AwaitSessionStarted => "waiting for SessionStarted",
            TycodeStartupPhase::AwaitInitialSettings => "waiting for Settings after GetSettings",
            TycodeStartupPhase::AwaitVerification { .. } => {
                "waiting for Settings verification after SaveSettings"
            }
            TycodeStartupPhase::AwaitProviderChange { .. } => {
                "waiting for ChangeProvider acknowledgement"
            }
            TycodeStartupPhase::AwaitRootAgentChanged { .. } => {
                "waiting for RootAgentChanged acknowledgement"
            }
            TycodeStartupPhase::Complete => "complete",
        }
    }
}

type TycodeStartupStatus = Arc<std::sync::Mutex<&'static str>>;

fn new_tycode_startup_status() -> TycodeStartupStatus {
    Arc::new(std::sync::Mutex::new("waiting for task start"))
}

fn set_tycode_startup_status(status: &TycodeStartupStatus, phase: &'static str) {
    *status.lock().expect("tycode startup status mutex poisoned") = phase;
}

async fn await_tycode_startup(
    ready_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
    shutdown_tx: &mpsc::UnboundedSender<()>,
    operation: &str,
    status: &TycodeStartupStatus,
) -> Result<(), String> {
    let timeout = tycode_startup_timeout();
    match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(_)) => Err(format!(
            "Tycode {operation} initialization task ended early"
        )),
        Err(_) => {
            let _ = shutdown_tx.send(());
            let phase = *status.lock().expect("tycode startup status mutex poisoned");
            Err(format!(
                "Timed out after {} waiting for Tycode {operation} startup/settings handshake: {phase}",
                format_tycode_timeout(timeout)
            ))
        }
    }
}

fn unavailable_native_settings_snapshot(message: String) -> BackendNativeSettingsSnapshot {
    BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Unavailable,
        settings: None,
        groups: Vec::new(),
        message: Some(message),
        provenance: None,
        advisories: Vec::new(),
        managed_projection_recovery: None,
    }
}

async fn unavailable_native_settings_snapshot_with_recovery(
    message: String,
) -> BackendNativeSettingsSnapshot {
    let Ok(paths) = tycode_projection_paths() else {
        return unavailable_native_settings_snapshot(message);
    };
    if !path_exists_without_following(&paths.directory).unwrap_or(false) {
        return unavailable_native_settings_snapshot(message);
    }
    let _guard = TYCODE_PROJECTION_LOCK.lock().await;
    if ensure_private_tycode_directory(&paths.directory).is_err() {
        return unavailable_native_settings_snapshot(message);
    }
    let Ok(_filesystem_lock) = acquire_tycode_filesystem_lock(&paths).await else {
        return unavailable_native_settings_snapshot(message);
    };
    let Ok(Some(recovery)) = load_recovery_record(&paths) else {
        return unavailable_native_settings_snapshot(message);
    };
    BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Unavailable,
        settings: None,
        groups: Vec::new(),
        message: Some(recovery.reason.clone()),
        provenance: None,
        advisories: Vec::new(),
        managed_projection_recovery: Some(
            TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
                reason: recovery.reason,
                expected_projection_id: recovery.projection_id,
                expected_state_hash: recovery.state_hash,
            },
        ),
    }
}

pub(crate) async fn native_settings_snapshot() -> BackendNativeSettingsSnapshot {
    match probe_native_settings_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => unavailable_native_settings_snapshot_with_recovery(error).await,
    }
}

async fn probe_native_settings_snapshot() -> Result<BackendNativeSettingsSnapshot, String> {
    probe_native_settings_snapshot_for(TycodeCommandPurpose::NativeSettingsProbe).await
}

async fn probe_native_settings_snapshot_for(
    purpose: TycodeCommandPurpose,
) -> Result<BackendNativeSettingsSnapshot, String> {
    let (command, projection) = tycode_command(purpose, "[]").await?;
    let mut result =
        run_tycode_settings_operation(command, purpose, TycodeSettingsOperation::Probe).await?;
    add_snapshot_advisories(&mut result.snapshot, &mut result.advisories);
    result.snapshot.provenance = Some(projection.provenance);
    result.snapshot.advisories = result.advisories;
    Ok(result.snapshot)
}

pub(crate) async fn persist_native_settings(settings: Value) -> Result<(), String> {
    persist_settings_with_purpose(settings, TycodeCommandPurpose::NativeSettingsPersist).await
}

async fn persist_settings_with_purpose(
    settings: Value,
    purpose: TycodeCommandPurpose,
) -> Result<(), String> {
    if !settings.is_object() {
        return Err("Tycode native settings must be a JSON object".to_string());
    }
    let _guard = TYCODE_PROJECTION_LOCK.lock().await;
    let paths = tycode_projection_paths()?;
    ensure_private_tycode_directory(&paths.directory)?;
    let _filesystem_lock = acquire_tycode_filesystem_lock(&paths).await?;
    recover_tycode_transaction(&paths)?;
    existing_tycode_projection(&paths)?.ok_or_else(|| {
        "Tycode managed projection is missing during staged settings save".to_string()
    })?;
    let before = current_pair_identity(&paths)?;
    let mut record = load_projection_record(&paths.provenance)?;
    let managed_bytes = fs::read(&paths.managed)
        .map_err(|err| format!("Failed to read Tyde-managed Tycode settings: {err}"))?;
    let nonce = uuid::Uuid::new_v4();
    let settings_temp = paths.directory.join(format!(
        ".tyde-settings.prejournal-save-managed-{nonce}.txn"
    ));
    let provenance_temp = paths.directory.join(format!(
        ".tyde-settings.prejournal-save-provenance-{nonce}.txn"
    ));
    let _temp_files = TycodeTempFiles(vec![settings_temp.clone(), provenance_temp.clone()]);
    write_private_file(&settings_temp, &managed_bytes)?;

    let command = tycode_staged_command(purpose, &settings_temp).await?;
    let saved =
        run_tycode_settings_operation(command, purpose, TycodeSettingsOperation::Save(settings))
            .await?;
    set_private_file_permissions(&settings_temp)?;

    let verification_command =
        tycode_staged_command(TycodeCommandPurpose::PostSaveVerification, &settings_temp).await?;
    let verified = run_tycode_settings_operation(
        verification_command,
        TycodeCommandPurpose::PostSaveVerification,
        TycodeSettingsOperation::Probe,
    )
    .await?;
    if saved.snapshot.settings != verified.snapshot.settings
        || saved.snapshot.groups != verified.snapshot.groups
    {
        return Err(
            "Fresh-process verification after Tycode native settings save did not match the saved canonical settings"
                .to_string(),
        );
    }
    ensure_private_regular_file(&settings_temp, "Staged Tycode settings save")?;
    sync_file(&settings_temp)?;
    let saved_bytes = fs::read(&settings_temp)
        .map_err(|err| format!("Failed to read staged Tycode settings save: {err}"))?;
    record.managed_digest = tycode_digest(&saved_bytes);
    let provenance_bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| format!("Failed to encode Tycode projection provenance: {err}"))?;
    write_private_file(&provenance_temp, &provenance_bytes)?;
    publish_pair_transaction(
        &paths,
        TycodeTransactionOperation::Save,
        &settings_temp,
        &provenance_temp,
        before,
    )?;
    validate_projection_record(&paths, record).map(|_| ())
}

pub(crate) async fn persist_backend_config(values: BackendConfigValues) -> Result<(), String> {
    if values.0.is_empty() {
        return Ok(());
    }
    let snapshot =
        probe_native_settings_snapshot_for(TycodeCommandPurpose::LegacyConfigProbe).await?;
    let settings = snapshot
        .settings
        .as_ref()
        .ok_or_else(|| "Tycode settings schema omitted current settings".to_string())?;
    let overlay = apply_tycode_settings_overlay(
        settings,
        &values,
        &SessionSettingsValues::default(),
        TycodeSettingsOverlayMode::PersistentSettingsPanel,
    )
    .map_err(|err| format!("Failed to apply Tycode settings overlay: {err}"))?;
    persist_settings_with_purpose(overlay.settings, TycodeCommandPurpose::LegacyConfigPersist).await
}

#[cfg(test)]
pub(crate) async fn backend_config_snapshot() -> Result<BackendConfigValues, String> {
    let snapshot =
        probe_native_settings_snapshot_for(TycodeCommandPurpose::LegacyConfigProbe).await?;
    let settings = snapshot
        .settings
        .as_ref()
        .ok_or_else(|| "Tycode settings schema omitted current settings".to_string())?;
    Ok(tycode_backend_config_snapshot_values(settings))
}

#[cfg(test)]
fn tycode_backend_config_snapshot_values(settings: &Value) -> BackendConfigValues {
    let mut values = BackendConfigValues::default();
    for key in TYCODE_MANAGED_SETTINGS {
        let Some(value) = settings.get(*key) else {
            continue;
        };
        let setting = match value {
            Value::String(value) if !value.trim().is_empty() => {
                SessionSettingValue::String(value.clone())
            }
            Value::Null => SessionSettingValue::Null,
            _ => continue,
        };
        values.0.insert((*key).to_string(), setting);
    }
    values
}

fn format_tycode_timeout(timeout: Duration) -> String {
    if timeout.as_secs() > 0 {
        format!("{}s", timeout.as_secs())
    } else {
        format!("{}ms", timeout.as_millis())
    }
}

fn send_tycode_json(
    stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
    value: Value,
) -> Result<(), String> {
    stdin_tx
        .send(TycodeStdinCommand::Json(value))
        .map_err(|_| "Tycode stdin writer closed".to_string())
}

fn send_tycode_runtime_session_settings_update(
    runtime_settings: &mut Option<Value>,
    update: &SessionSettingsValues,
    stdin_tx: &mpsc::UnboundedSender<TycodeStdinCommand>,
) -> Result<(), String> {
    validate_runtime_session_settings_update(update)?;
    let current_settings = runtime_settings.as_ref().ok_or_else(|| {
        "Tycode runtime settings unavailable while applying session settings update".to_string()
    })?;
    let overlay = apply_tycode_settings_overlay(
        current_settings,
        &BackendConfigValues::default(),
        update,
        TycodeSettingsOverlayMode::SessionRuntime,
    )
    .map_err(|err| format!("Failed to apply Tycode session settings update: {err}"))?;
    send_tycode_json(
        stdin_tx,
        serde_json::json!({
            "SaveSettings": {
                "settings": overlay.settings.clone(),
                "persist": false,
            }
        }),
    )?;
    *runtime_settings = Some(overlay.settings);
    Ok(())
}

fn tycode_settings_data(value: &Value) -> Option<&Value> {
    (value.get("kind").and_then(Value::as_str) == Some("Settings"))
        .then(|| value.get("data"))
        .flatten()
}

fn tycode_settings_schema_data(value: &Value) -> Option<&Value> {
    (value.get("kind").and_then(Value::as_str) == Some("SettingsSchema"))
        .then(|| value.get("data"))
        .flatten()
        .and_then(|data| data.get("schema"))
}

fn tycode_native_settings_snapshot_from_schema(
    schema: &Value,
) -> Result<BackendNativeSettingsSnapshot, String> {
    let settings = schema
        .get("settings")
        .cloned()
        .ok_or_else(|| "Tycode SettingsSchema event missing current settings".to_string())?;
    if !settings.is_object() {
        return Err("Tycode SettingsSchema current settings must be an object".to_string());
    }
    let groups_value = schema
        .get("groups")
        .cloned()
        .ok_or_else(|| "Tycode SettingsSchema event missing groups".to_string())?;
    let groups = serde_json::from_value::<Vec<BackendNativeSettingsGroup>>(groups_value)
        .map_err(|err| format!("Failed to parse Tycode SettingsSchema groups: {err}"))?;

    Ok(BackendNativeSettingsSnapshot {
        backend_kind: BackendKind::Tycode,
        status: BackendConfigSnapshotStatus::Ready,
        settings: Some(settings),
        groups,
        message: None,
        provenance: None,
        advisories: Vec::new(),
        managed_projection_recovery: None,
    })
}

fn tycode_error_message(value: &Value) -> Option<String> {
    if value.get("kind").and_then(Value::as_str) == Some("Error") {
        return value
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return None;
    }
    let data = value.get("data")?;
    (data.get("sender").and_then(Value::as_str) == Some("Error")
        || data
            .get("sender")
            .and_then(Value::as_object)
            .is_some_and(|sender| sender.contains_key("Error")))
    .then(|| data.get("content").and_then(Value::as_str))
    .flatten()
    .map(str::to_string)
}

fn tycode_provider_changed_message(value: &Value, provider: &str) -> bool {
    if value.get("kind").and_then(Value::as_str) != Some("MessageAdded") {
        return false;
    }
    let Some(data) = value.get("data") else {
        return false;
    };
    let is_system = data.get("sender").and_then(Value::as_str) == Some("System");
    let expected = format!("Switched to provider: {provider}");
    is_system && data.get("content").and_then(Value::as_str) == Some(expected.as_str())
}

fn tycode_root_agent_changed(value: &Value) -> Option<&str> {
    (value.get("kind").and_then(Value::as_str) == Some("RootAgentChanged"))
        .then(|| {
            value
                .get("data")
                .and_then(|data| data.get("agent"))
                .and_then(Value::as_str)
        })
        .flatten()
}

fn tycode_startup_internal_observation(value: &Value) -> TycodeStartupObservation {
    match value.get("kind").and_then(Value::as_str) {
        Some("Settings" | "TimingUpdate" | "TypingStatusChanged" | "RootAgentChanged") => {
            TycodeStartupObservation::Suppress
        }
        _ => TycodeStartupObservation::Allow,
    }
}

fn tycode_settings_verification_error(expected: &Value, actual: &Value) -> String {
    let managed_keys = [
        "active_provider",
        "model_quality",
        "reasoning_effort",
        "autonomy_level",
        "review_level",
        "spawn_context_mode",
        "orchestration_progress_messages",
    ];
    let mismatched = managed_keys
        .into_iter()
        .filter(|key| expected.get(*key) != actual.get(*key))
        .collect::<Vec<_>>();
    let providers_changed = expected.get("providers") != actual.get("providers");
    let mut details = Vec::new();
    if !mismatched.is_empty() {
        details.push(format!(
            "mismatched managed keys: {}",
            mismatched.join(", ")
        ));
    }
    if providers_changed {
        details.push("providers changed".to_string());
    }
    if details.is_empty() {
        details.push("returned settings differed outside Tyde-managed fields".to_string());
    }
    format!(
        "Tycode settings verification failed after SaveSettings ({})",
        details.join("; ")
    )
}

fn verify_tycode_settings_overlay(expected: &Value, actual: &Value) -> Result<(), String> {
    let managed_keys = [
        "active_provider",
        "model_quality",
        "reasoning_effort",
        "autonomy_level",
        "review_level",
        "spawn_context_mode",
        "orchestration_progress_messages",
    ];
    let managed_keys_match = managed_keys
        .into_iter()
        .all(|key| expected.get(key) == actual.get(key));
    let providers_match = expected.get("providers") == actual.get("providers");
    if managed_keys_match && providers_match {
        Ok(())
    } else {
        Err(tycode_settings_verification_error(expected, actual))
    }
}

impl Backend for TycodeBackend {
    fn session_settings_schema() -> protocol::SessionSettingsSchema {
        tycode_session_settings_schema()
    }

    async fn spawn(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), String> {
        let initial_message = initial_input.message;
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let session_id = Arc::new(std::sync::Mutex::new(None));
        let session_id_task = Arc::clone(&session_id);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);
        let startup_status = new_tycode_startup_status();
        let startup_status_task = Arc::clone(&startup_status);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode customization: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let (mut command, _) =
                match tycode_command(TycodeCommandPurpose::NewSession, &roots_json).await {
                    Ok(command) => command,
                    Err(err) => {
                        tracing::error!("{err}");
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }

            let mut child = match command.group_spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn tycode-subprocess: {err}")));
                    return;
                }
            };

            let stdin = match child.inner().stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin");
                    let _ =
                        ready_tx.send(Err("Failed to capture tycode-subprocess stdin".to_string()));
                    return;
                }
            };
            let stdout = match child.inner().stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stdout".to_string()));
                    return;
                }
            };
            let stderr = match child.inner().stderr.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stderr");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stderr".to_string()));
                    return;
                }
            };
            let last_stderr_line = spawn_tycode_stderr_logger(stderr);

            // Spawn a task to forward follow-up messages to stdin
            let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            let (settings_update_tx, mut settings_update_rx) =
                mpsc::unbounded_channel::<SessionSettingsValues>();

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                resolve_session_settings(&config),
                tycode_root_agent_override_policy(&config),
                TycodeStartupFollowUp::InitialUserInput(initial_message),
                false,
            );
            set_tycode_startup_status(&startup_status_task, startup.phase_description());

            // Forward AgentInput to the stdin writer
            let stdin_tx2 = stdin_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(payload) => {
                            if settings_update_tx.send(payload.values).is_err() {
                                break;
                            }
                        }
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt.send(TycodeStdinCommand::Cancel).is_err() {
                        break;
                    }
                }
            });

            // Read stdout line by line — the subprocess emits ChatEvent JSON directly
            let mut lines = BufReader::new(stdout).lines();
            let mut stream_state = TycodeStreamState::default();
            let mut runtime_settings = None;
            let mut settings_updates_open = true;
            let mut ready_tx = Some(ready_tx);
            #[cfg(test)]
            observe_tycode_startup_process_spawned(child.inner().id());
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    settings_update = settings_update_rx.recv(), if settings_updates_open => {
                        let Some(settings_update) = settings_update else {
                            settings_updates_open = false;
                            continue;
                        };
                        if let Err(err) = send_tycode_runtime_session_settings_update(
                            &mut runtime_settings,
                            &settings_update,
                            &stdin_tx,
                        ) {
                            tracing::error!("{err}");
                            let _ = events_tx.send(tycode_error_chat_event(err));
                        }
                        continue;
                    }
                    _ = shutdown_rx.recv() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        #[cfg(test)]
                        observe_tycode_startup_process_reaped();
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            event = %tycode_line_diagnostic(trimmed),
                            "Failed to parse tycode-subprocess event: {err}"
                        );
                        continue;
                    }
                };

                if session_id_task
                    .lock()
                    .expect("tycode session_id mutex poisoned")
                    .is_none()
                    && let Some(session) = tycode_session_started(&value)
                {
                    *session_id_task
                        .lock()
                        .expect("tycode session_id mutex poisoned") = Some(session);
                }

                let observation = match startup.observe(&value, &stdin_tx) {
                    Ok(observation) => observation,
                    Err(err) => {
                        tracing::error!("{err}");
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Err(err));
                        }
                        let _ = child.kill().await;
                        return;
                    }
                };
                set_tycode_startup_status(&startup_status_task, startup.phase_description());
                match observation {
                    TycodeStartupObservation::Allow => {}
                    TycodeStartupObservation::Suppress => continue,
                    TycodeStartupObservation::Completed => {
                        runtime_settings = startup.runtime_settings().cloned();
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                if let Some(settings) = tycode_settings_data(&value) {
                    runtime_settings = Some(settings.clone());
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in tycode_events_with_synthesized_completion(events, &mut stream_state) {
                    if events_tx.send(event).is_err() {
                        break;
                    }
                    if events_tx.is_closed() {
                        break;
                    }
                }
            }

            // Some tycode builds terminate without emitting StreamEnd. Synthesize
            // one so downstream callers don't hang waiting for end-of-turn.
            if stream_state.open {
                let _ = events_tx.send(stream_state.synthetic_stream_end());
            }

            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(tycode_startup_exit_error(&last_stderr_line)));
            }
        });

        await_tycode_startup(ready_rx, &shutdown_tx, "spawn", &startup_status).await?;

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
                session_id,
            },
            EventStream::new(events_rx),
        ))
    }

    async fn resume(
        workspace_roots: Vec<String>,
        config: BackendSpawnConfig,
        session_id: SessionId,
    ) -> Result<(Self, EventStream), String> {
        let replay_event_count = tycode_resume_replay_event_count(&session_id)?;
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<AgentInput>();
        let (interrupt_tx, mut interrupt_rx) = mpsc::unbounded_channel::<()>();
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ChatEvent>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let (resume_replay_complete_tx, resume_replay_complete_rx) =
            tokio::sync::oneshot::channel();
        let known_session_id = Arc::new(std::sync::Mutex::new(Some(session_id.clone())));
        let mcp_servers_json = build_tycode_mcp_servers_json(&config.startup_mcp_servers);
        let startup_status = new_tycode_startup_status();
        let startup_status_task = Arc::clone(&startup_status);

        tokio::spawn(async move {
            let materialized_customization = match materialize_tycode_customization(&config) {
                Ok(root) => root,
                Err(err) => {
                    tracing::error!("Failed to materialize Tycode resume customization: {err}");
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };
            let mut workspace_roots = workspace_roots;
            if let Some(root) = materialized_customization.as_ref() {
                workspace_roots.push(root.path.to_string_lossy().to_string());
            }
            let roots_json = serde_json::json!(workspace_roots).to_string();
            let (mut command, _) =
                match tycode_command(TycodeCommandPurpose::ResumeSession, &roots_json).await {
                    Ok(command) => command,
                    Err(err) => {
                        tracing::error!("{err}");
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
            if let Some(agent_json) = tycode_read_only_agent_json(&config) {
                command.arg("--agent").arg(agent_json);
            }
            if let Some(mcp_servers_json) = mcp_servers_json.as_deref() {
                command.arg("--mcp-servers").arg(mcp_servers_json);
            }

            let mut child = match command.group_spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("Failed to spawn tycode-subprocess for resume: {err}");
                    let _ = ready_tx.send(Err(format!("Failed to spawn tycode-subprocess: {err}")));
                    return;
                }
            };

            let stdin = match child.inner().stdin.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdin for resume");
                    let _ =
                        ready_tx.send(Err("Failed to capture tycode-subprocess stdin".to_string()));
                    return;
                }
            };
            let stdout = match child.inner().stdout.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stdout for resume");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stdout".to_string()));
                    return;
                }
            };
            let stderr = match child.inner().stderr.take() {
                Some(s) => s,
                None => {
                    tracing::error!("Failed to capture tycode-subprocess stderr for resume");
                    let _ = ready_tx
                        .send(Err("Failed to capture tycode-subprocess stderr".to_string()));
                    return;
                }
            };
            let last_stderr_line = spawn_tycode_stderr_logger(stderr);

            let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(command) = stdin_rx.recv().await {
                    let ok = match command {
                        TycodeStdinCommand::Json(command) => {
                            write_command(&mut stdin, &command).await
                        }
                        TycodeStdinCommand::Cancel => write_cancel(&mut stdin).await,
                    };
                    if !ok {
                        break;
                    }
                }
            });

            let (settings_update_tx, mut settings_update_rx) =
                mpsc::unbounded_channel::<SessionSettingsValues>();

            let mut startup = TycodeStartupController::new(
                config.backend_config.clone(),
                resolve_session_settings(&config),
                tycode_root_agent_override_policy(&config),
                TycodeStartupFollowUp::ResumeSession {
                    session_id: session_id.0.clone(),
                },
                false,
            );
            set_tycode_startup_status(&startup_status_task, startup.phase_description());

            let stdin_tx2 = stdin_tx.clone();
            tokio::spawn(async move {
                while let Some(input) = input_rx.recv().await {
                    match input {
                        AgentInput::SendMessage(payload) => {
                            let message = payload.message;
                            if stdin_tx2
                                .send(TycodeStdinCommand::Json(
                                    serde_json::json!({ "UserInput": message }),
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        AgentInput::UpdateSessionSettings(payload) => {
                            if settings_update_tx.send(payload.values).is_err() {
                                break;
                            }
                        }
                        AgentInput::EditQueuedMessage(_)
                        | AgentInput::CancelQueuedMessage(_)
                        | AgentInput::SendQueuedMessageNow(_) => {
                            panic!(
                                "queued-message inputs must be handled by the agent actor before reaching the backend"
                            );
                        }
                    }
                }
            });

            let stdin_tx_interrupt = stdin_tx.clone();
            tokio::spawn(async move {
                while interrupt_rx.recv().await.is_some() {
                    if stdin_tx_interrupt.send(TycodeStdinCommand::Cancel).is_err() {
                        break;
                    }
                }
            });

            let mut lines = BufReader::new(stdout).lines();
            let mut stream_state = TycodeStreamState::default();
            let mut runtime_settings = None;
            let mut settings_updates_open = true;
            let mut replay_barrier =
                TycodeResumeReplayBarrier::new(session_id.0.clone(), replay_event_count);
            let mut resume_replay_complete_tx = Some(resume_replay_complete_tx);
            let mut ready_tx = Some(ready_tx);
            #[cfg(test)]
            observe_tycode_startup_process_spawned(child.inner().id());
            loop {
                let line = tokio::select! {
                    line = lines.next_line() => line,
                    settings_update = settings_update_rx.recv(), if settings_updates_open => {
                        let Some(settings_update) = settings_update else {
                            settings_updates_open = false;
                            continue;
                        };
                        if let Err(err) = send_tycode_runtime_session_settings_update(
                            &mut runtime_settings,
                            &settings_update,
                            &stdin_tx,
                        ) {
                            tracing::error!("{err}");
                            let _ = events_tx.send(tycode_error_chat_event(err));
                        }
                        continue;
                    }
                    _ = shutdown_rx.recv() => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        #[cfg(test)]
                        observe_tycode_startup_process_reaped();
                        break;
                    }
                };
                let Ok(Some(line)) = line else {
                    break;
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let value: Value = match serde_json::from_str(trimmed) {
                    Ok(value) => value,
                    Err(err) => {
                        tracing::warn!(
                            event = %tycode_line_diagnostic(trimmed),
                            "Failed to parse tycode-subprocess resume event: {err}"
                        );
                        continue;
                    }
                };

                let observation = match startup.observe(&value, &stdin_tx) {
                    Ok(observation) => observation,
                    Err(err) => {
                        tracing::error!("{err}");
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Err(err));
                        }
                        let _ = child.kill().await;
                        return;
                    }
                };
                set_tycode_startup_status(&startup_status_task, startup.phase_description());
                match observation {
                    TycodeStartupObservation::Allow => {}
                    TycodeStartupObservation::Suppress => continue,
                    TycodeStartupObservation::Completed => {
                        runtime_settings = startup.runtime_settings().cloned();
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(Ok(()));
                        }
                        continue;
                    }
                }

                if let Some(settings) = tycode_settings_data(&value) {
                    runtime_settings = Some(settings.clone());
                }

                if resume_replay_complete_tx.is_some() && replay_barrier.observe(&value) {
                    if let Some(tx) = resume_replay_complete_tx.take() {
                        let _ = tx.send(());
                    }
                    continue;
                }

                let events = map_tycode_value_to_chat_events(&value);
                if events.is_empty() {
                    continue;
                }

                for event in tycode_events_with_synthesized_completion(events, &mut stream_state) {
                    if events_tx.send(event).is_err() {
                        break;
                    }
                }
            }

            if stream_state.open {
                let _ = events_tx.send(stream_state.synthetic_stream_end());
            }

            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(tycode_startup_exit_error(&last_stderr_line)));
            }
        });

        await_tycode_startup(ready_rx, &shutdown_tx, "resume", &startup_status).await?;

        Ok((
            Self {
                input_tx,
                interrupt_tx,
                shutdown_tx,
                session_id: known_session_id,
            },
            EventStream::new_with_resume_replay_barrier(events_rx, resume_replay_complete_rx),
        ))
    }

    async fn fork(
        _workspace_roots: Vec<String>,
        _config: BackendSpawnConfig,
        _from_session_id: SessionId,
        _initial_input: protocol::SendMessagePayload,
    ) -> Result<(Self, EventStream), BackendStartupError> {
        Err(BackendStartupError::unsupported(
            backend_fork_unsupported_message(BackendKind::Tycode),
        ))
    }

    async fn list_sessions() -> Result<Vec<BackendSession>, String> {
        list_tycode_sessions()
    }

    fn session_id(&self) -> SessionId {
        self.session_id
            .lock()
            .expect("tycode session_id mutex poisoned")
            .clone()
            .expect("tycode session_id not initialized")
    }

    async fn send(&self, input: AgentInput) -> bool {
        self.input_tx.send(input).is_ok()
    }

    async fn interrupt(&self) -> bool {
        self.interrupt_tx.send(()).is_ok()
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

async fn write_command(stdin: &mut tokio::process::ChildStdin, command: &Value) -> bool {
    let line = match serde_json::to_string(command) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("Failed to serialize tycode command: {err}");
            return false;
        }
    };

    if let Err(err) = stdin.write_all(line.as_bytes()).await {
        tracing::error!("Failed to write to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.write_all(b"\n").await {
        tracing::error!("Failed to write newline to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess stdin: {err}");
        return false;
    }
    true
}

async fn write_cancel(stdin: &mut tokio::process::ChildStdin) -> bool {
    if let Err(err) = stdin.write_all(b"CANCEL\n").await {
        tracing::error!("Failed to write cancel to tycode-subprocess stdin: {err}");
        return false;
    }
    if let Err(err) = stdin.flush().await {
        tracing::error!("Failed to flush tycode-subprocess cancel: {err}");
        return false;
    }
    true
}

fn tycode_sessions_dir() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_TYCODE_SESSIONS_DIR
        .lock()
        .expect("test Tycode sessions dir mutex poisoned")
        .clone()
    {
        return Ok(path);
    }

    Ok(crate::paths::home_dir()?.join(".tycode").join("sessions"))
}

fn build_tycode_mcp_servers_json(startup_mcp_servers: &[StartupMcpServer]) -> Option<String> {
    if startup_mcp_servers.is_empty() {
        return None;
    }

    let mut servers = serde_json::Map::new();
    for server in startup_mcp_servers {
        let name = server.name.trim();
        if name.is_empty() {
            continue;
        }
        let config = match &server.transport {
            StartupMcpTransport::Http {
                url,
                headers,
                bearer_token_env_var,
            } => {
                let trimmed_url = url.trim();
                if trimmed_url.is_empty() {
                    continue;
                }
                let mut config = serde_json::Map::new();
                config.insert("url".to_string(), Value::String(trimmed_url.to_string()));
                if !headers.is_empty() {
                    config.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .expect("HashMap<String, String> is always serializable"),
                    );
                }
                if let Some(env_var) = bearer_token_env_var
                    .as_ref()
                    .map(|raw| raw.trim())
                    .filter(|raw| !raw.is_empty())
                {
                    config.insert(
                        "bearer_token_env_var".to_string(),
                        Value::String(env_var.to_string()),
                    );
                }
                Value::Object(config)
            }
            StartupMcpTransport::Stdio { command, args, env } => {
                let trimmed_command = command.trim();
                if trimmed_command.is_empty() {
                    continue;
                }
                serde_json::json!({
                    "command": trimmed_command,
                    "args": args,
                    "env": env,
                })
            }
        };
        servers.insert(name.to_string(), config);
    }

    if servers.is_empty() {
        return None;
    }

    Some(Value::Object(servers).to_string())
}

fn spawn_tycode_stderr_logger(
    stderr: tokio::process::ChildStderr,
) -> Arc<std::sync::Mutex<Option<String>>> {
    let last_stderr_line = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&last_stderr_line);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let diagnostic = tycode_text_diagnostic(trimmed);
            tracing::warn!(stderr = %diagnostic, "tycode-subprocess stderr");
            *sink.lock().expect("tycode stderr mutex poisoned") = Some(diagnostic);
        }
    });
    last_stderr_line
}

const TYCODE_DIAGNOSTIC_PREVIEW_CHARS: usize = 240;

fn tycode_line_diagnostic(line: &str) -> String {
    if let Some(kind) = extract_json_string_field(line, "kind")
        && matches!(
            kind.as_str(),
            "Settings"
                | "SettingsSchema"
                | "MessageAdded"
                | "StreamDelta"
                | "StreamReasoningDelta"
                | "StreamEnd"
        )
    {
        return format!("{{\"kind\":\"{kind}\",\"data\":\"<redacted>\"}}");
    }

    tycode_text_diagnostic(line)
}

fn tycode_event_diagnostic(value: &Value) -> String {
    tycode_diagnostic_preview(
        &serde_json::to_string(&sanitize_tycode_value_for_diagnostics(value))
            .unwrap_or_else(|_| "<unserializable Tycode event>".to_string()),
    )
}

fn sanitize_tycode_value_for_diagnostics(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sanitized = serde_json::Map::new();
            for (key, value) in map {
                if tycode_diagnostic_key_is_sensitive(key) {
                    sanitized.insert(key.clone(), Value::String("<redacted>".to_string()));
                } else {
                    sanitized.insert(key.clone(), sanitize_tycode_value_for_diagnostics(value));
                }
            }
            Value::Object(sanitized)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(sanitize_tycode_value_for_diagnostics)
                .collect(),
        ),
        Value::String(value) => Value::String(tycode_text_diagnostic(value)),
        _ => value.clone(),
    }
}

fn tycode_diagnostic_key_is_sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "authorization"
            | "bearer"
            | "content"
            | "credential"
            | "credentials"
            | "images"
            | "input"
            | "arguments"
            | "message"
            | "password"
            | "prompt"
            | "providers"
            | "reasoning"
            | "secret"
            | "settings"
            | "text"
            | "token"
            | "tool_calls"
            | "userinput"
            | "savesettings"
    ) || key.ends_with("_key")
        || key.ends_with("_token")
}

fn tycode_text_diagnostic(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    for marker in [
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "password",
        "secret",
        "token",
        "credential",
        "userinput",
        "save_settings",
        "savesettings",
    ] {
        if let Some(index) = lower.find(marker) {
            return tycode_diagnostic_preview(&format!(
                "{} <redacted>",
                trimmed[..index + marker.len()].trim_end()
            ));
        }
    }

    tycode_diagnostic_preview(trimmed)
}

fn tycode_diagnostic_preview(text: &str) -> String {
    let mut preview = String::new();
    let mut chars = text.chars();
    for _ in 0..TYCODE_DIAGNOSTIC_PREVIEW_CHARS {
        let Some(ch) = chars.next() else {
            return preview;
        };
        preview.push(ch);
    }
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn extract_json_string_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let field_start = line.find(&needle)?;
    let after_field = &line[field_start + needle.len()..];
    let colon_index = after_field.find(':')?;
    let after_colon = after_field[colon_index + 1..].trim_start();
    let mut chars = after_colon.chars();
    if chars.next()? != '"' {
        return None;
    }
    let mut value = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            value.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(value),
            _ => value.push(ch),
        }
    }
    None
}

fn tycode_startup_exit_error(last_stderr_line: &Arc<std::sync::Mutex<Option<String>>>) -> String {
    tycode_process_exit_error(
        last_stderr_line,
        "Tycode process exited before reporting a session_id",
    )
}

fn tycode_process_exit_error(
    last_stderr_line: &Arc<std::sync::Mutex<Option<String>>>,
    message: &str,
) -> String {
    match last_stderr_line
        .lock()
        .expect("tycode stderr mutex poisoned")
        .clone()
    {
        Some(stderr) => format!("{message}: {stderr}"),
        None => message.to_string(),
    }
}

fn tycode_session_started(value: &Value) -> Option<SessionId> {
    if value.get("kind").and_then(Value::as_str) != Some("SessionStarted") {
        return None;
    }

    value
        .get("data")
        .and_then(|data| data.get("session_id"))
        .and_then(Value::as_str)
        .map(|session_id| SessionId(session_id.to_string()))
}

fn list_tycode_sessions() -> Result<Vec<BackendSession>, String> {
    let sessions_dir = tycode_sessions_dir()?;
    let entries = match fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "Failed to read Tycode sessions directory {}: {err}",
                sessions_dir.display()
            ));
        }
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }

        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!("Skipping unreadable Tycode session {:?}: {err}", path);
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&json) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("Skipping unparseable Tycode session {:?}: {err}", path);
                continue;
            }
        };

        let Some(id) = value.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let created_at_ms = value.get("created_at").and_then(Value::as_u64);
        let updated_at_ms = value.get("last_modified").and_then(Value::as_u64);
        let title = extract_tycode_title(&value);

        sessions.push(BackendSession {
            id: SessionId(id),
            backend_kind: BackendKind::Tycode,
            workspace_roots: Vec::new(),
            title,
            token_count: None,
            created_at_ms,
            updated_at_ms,
            resumable: true,
        });
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at_ms));
    Ok(sessions)
}

fn extract_tycode_title(value: &Value) -> Option<String> {
    let messages = value.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("User") {
            continue;
        }
        if let Some(text) = message
            .get("content")
            .and_then(|content| content.get("blocks"))
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.chars().take(80).collect());
            }
        }
    }
    None
}

fn is_tycode_sessions_list(value: &Value) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("SessionsList")
}

struct TycodeResumeReplayBarrier {
    session_id: String,
    replay_started: bool,
    replay_events_remaining: usize,
}

impl TycodeResumeReplayBarrier {
    fn new(session_id: String, replay_events_remaining: usize) -> Self {
        Self {
            session_id,
            replay_started: false,
            replay_events_remaining,
        }
    }

    fn observe(&mut self, value: &Value) -> bool {
        if !self.replay_started {
            if is_tycode_session_started(value, &self.session_id) {
                self.replay_started = true;
                self.replay_events_remaining = self.replay_events_remaining.saturating_sub(1);
            }
            return false;
        }
        if self.replay_events_remaining > 0 {
            self.replay_events_remaining -= 1;
            return false;
        }
        is_tycode_sessions_list(value)
    }
}

fn is_tycode_session_started(value: &Value, session_id: &str) -> bool {
    value.get("kind").and_then(Value::as_str) == Some("SessionStarted")
        && value
            .get("data")
            .and_then(|data| data.get("session_id"))
            .and_then(Value::as_str)
            == Some(session_id)
}

fn tycode_resume_replay_event_count(session_id: &SessionId) -> Result<usize, String> {
    let path = tycode_sessions_dir()?.join(format!("{}.json", session_id.0));
    let json = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read Tycode session {}: {err}", path.display()))?;
    tycode_resume_replay_event_count_from_json(&json)
}

fn tycode_resume_replay_event_count_from_json(json: &str) -> Result<usize, String> {
    let value: Value = serde_json::from_str(json)
        .map_err(|err| format!("failed to parse Tycode session JSON: {err}"))?;
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| "Tycode session JSON is missing an events array".to_owned())?;
    Ok(2 + events
        .iter()
        .filter(|event| !is_tycode_replay_filtered_delta(event))
        .count())
}

fn is_tycode_replay_filtered_delta(value: &Value) -> bool {
    matches!(
        value.get("kind").and_then(Value::as_str),
        Some("StreamDelta" | "StreamReasoningDelta")
    )
}

fn map_tycode_value_to_chat_events(value: &Value) -> Vec<ChatEvent> {
    if value.get("kind").and_then(Value::as_str) == Some("Orchestration") {
        return map_tycode_orchestration_event(value);
    }

    let normalized = normalize_tycode_event_value(value);
    if let Ok(event) = serde_json::from_value::<ChatEvent>(normalized) {
        return vec![event];
    }

    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        tracing::warn!(
            event = %tycode_event_diagnostic(value),
            "Ignoring Tycode event without kind"
        );
        return Vec::new();
    };

    if is_known_tycode_typed_chat_event_kind(kind) {
        let err = serde_json::from_value::<ChatEvent>(normalize_tycode_event_value(value))
            .expect_err("known Tycode event failed to deserialize above");
        tracing::error!(
            kind,
            error = %err,
            event = %tycode_event_diagnostic(value),
            "Malformed Tycode chat event"
        );
        let error_event = tycode_error_chat_event(format!("Malformed Tycode {kind} event: {err}"));
        if kind == "StreamEnd" {
            return vec![error_event, tycode_malformed_stream_end_event()];
        }
        return vec![error_event];
    }

    match kind {
        "Settings"
        | "SettingsSchema"
        | "ConversationCleared"
        | "SessionsList"
        | "ProfilesList"
        | "TimingUpdate"
        | "ModuleSchemas"
        | "SessionStarted"
        | "RootAgentChanged" => Vec::new(),
        "Error" => {
            let Some(message) = value.get("data").and_then(Value::as_str) else {
                tracing::error!(
                    event = %tycode_event_diagnostic(value),
                    "Malformed Tycode Error event"
                );
                return vec![tycode_error_chat_event(
                    "Malformed Tycode Error event: data must be a string",
                )];
            };
            vec![tycode_error_chat_event(message)]
        }
        other => {
            tracing::warn!(
                kind = %other,
                event = %tycode_event_diagnostic(value),
                "Ignoring unsupported Tycode event"
            );
            Vec::new()
        }
    }
}

fn normalize_tycode_event_value(value: &Value) -> Value {
    let mut normalized = value.clone();
    match normalized.get("kind").and_then(Value::as_str) {
        Some("MessageAdded") => {
            if let Some(message) = normalized.get_mut("data") {
                normalize_tycode_chat_message(message);
            }
        }
        Some("StreamEnd") => {
            if let Some(message) = normalized
                .get_mut("data")
                .and_then(|data| data.get_mut("message"))
            {
                normalize_tycode_chat_message(message);
            }
        }
        _ => {}
    }
    normalized
}

fn normalize_tycode_chat_message(message: &mut Value) {
    let Some(token_usage) = message.get_mut("token_usage") else {
        return;
    };
    let Value::Object(usage) = token_usage else {
        return;
    };
    if usage.contains_key("request")
        || usage.contains_key("turn")
        || usage.contains_key("cumulative")
        || !(usage.contains_key("input_tokens")
            && usage.contains_key("output_tokens")
            && usage.contains_key("total_tokens"))
    {
        return;
    }

    let flat_usage = Value::Object(usage.clone());
    *token_usage = serde_json::json!({
        "request": {
            "kind": "known",
            "usage": flat_usage.clone(),
        },
        "turn": {
            "kind": "known",
            "usage": flat_usage,
        },
        "cumulative": {
            "kind": "unavailable",
            "reason": "backend_did_not_report",
        },
    });
}

fn is_known_tycode_typed_chat_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "MessageAdded"
            | "TypingStatusChanged"
            | "StreamStart"
            | "StreamDelta"
            | "StreamReasoningDelta"
            | "StreamEnd"
            | "ToolRequest"
            | "ToolExecutionCompleted"
            | "OperationCancelled"
            | "RetryAttempt"
            | "TaskUpdate"
    )
}

fn map_tycode_orchestration_event(value: &Value) -> Vec<ChatEvent> {
    let Some(payload_kind) = value
        .get("data")
        .and_then(|data| data.get("payload"))
        .and_then(|payload| payload.get("kind"))
        .and_then(Value::as_str)
    else {
        tracing::error!(
            event = %tycode_event_diagnostic(value),
            "Malformed Tycode Orchestration event missing payload kind"
        );
        return vec![tycode_error_chat_event(
            "Malformed Tycode Orchestration event: missing data.payload.kind",
        )];
    };

    if !is_known_tycode_orchestration_payload_kind(payload_kind) {
        tracing::warn!(
            payload_kind,
            event = %tycode_event_diagnostic(value),
            "Ignoring unknown Tycode Orchestration payload kind"
        );
        return Vec::new();
    }

    match value
        .get("data")
        .cloned()
        .ok_or_else(|| "missing data".to_string())
        .and_then(|data| {
            serde_json::from_value::<OrchestrationEvent>(data)
                .map_err(|err| format!("failed to parse {payload_kind}: {err}"))
        }) {
        Ok(event) => vec![ChatEvent::Orchestration(event)],
        Err(err) => {
            tracing::error!(
                payload_kind,
                error = %err,
                event = %tycode_event_diagnostic(value),
                "Malformed Tycode Orchestration event"
            );
            vec![tycode_error_chat_event(format!(
                "Malformed Tycode Orchestration event ({payload_kind}): {err}"
            ))]
        }
    }
}

fn is_known_tycode_orchestration_payload_kind(kind: &str) -> bool {
    matches!(
        kind,
        "AgentStarted"
            | "AgentCompleted"
            | "PhaseChanged"
            | "FanOutStarted"
            | "WorkerStarted"
            | "WorkerCompleted"
            | "FanOutCompleted"
            | "ConsensusRoundResolved"
            | "PlanSelected"
            | "ReviewRoundResolved"
    )
}

fn tycode_error_chat_event(message: impl Into<String>) -> ChatEvent {
    ChatEvent::MessageAdded(ChatMessage {
        message_id: None,
        timestamp: unix_now_ms(),
        sender: MessageSender::Error,
        content: message.into(),
        reasoning: None,
        tool_calls: Vec::new(),
        model_info: None,
        token_usage: None,
        context_breakdown: None,
        images: None,
    })
}

fn tycode_malformed_stream_end_event() -> ChatEvent {
    tycode_stream_end_event(String::new())
}

fn tycode_stream_end_event(content: String) -> ChatEvent {
    ChatEvent::StreamEnd(StreamEndData {
        message: ChatMessage {
            message_id: None,
            timestamp: unix_now_ms(),
            sender: MessageSender::Assistant {
                agent: "tycode".to_string(),
            },
            content,
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        },
    })
}

#[derive(Debug, Default)]
struct TycodeStreamState {
    open: bool,
    message_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    accumulated_text: String,
    accumulated_reasoning: String,
    synthetic_completion: Option<SyntheticTycodeCompletion>,
}

#[derive(Debug)]
struct SyntheticTycodeCompletion {
    message_id: Option<ChatMessageId>,
    content: String,
    reasoning_text: Option<String>,
}

impl TycodeStreamState {
    fn events_with_synthesized_completion(&mut self, events: Vec<ChatEvent>) -> Vec<ChatEvent> {
        let mut output = Vec::new();
        for event in events {
            if let Some(events) = self.late_authoritative_stream_end_events(&event) {
                output.extend(events);
                continue;
            }
            if let Some(stream_end) = self.synthesize_stream_end_before(&event) {
                output.push(stream_end);
            }
            self.update(&event);
            output.push(event);
        }

        output
    }

    fn late_authoritative_stream_end_events(
        &mut self,
        event: &ChatEvent,
    ) -> Option<Vec<ChatEvent>> {
        let ChatEvent::StreamEnd(end) = event else {
            return None;
        };
        if self.open {
            return None;
        }
        let synthetic = self.synthetic_completion.take()?;
        self.warn_if_late_stream_end_has_unmerged_fields(&synthetic, &end.message);

        let message_id = synthetic
            .message_id
            .clone()
            .or_else(|| end.message.message_id.clone());
        let Some(message_id) = message_id else {
            tracing::warn!(
                "Forwarding delayed Tycode StreamEnd after synthesized completion because no \
                 message_id is available for metadata merge"
            );
            return Some(vec![event.clone()]);
        };

        if end.message.model_info.is_none()
            && end.message.token_usage.is_none()
            && end.message.context_breakdown.is_none()
        {
            return Some(Vec::new());
        }

        Some(vec![ChatEvent::MessageMetadataUpdated(
            MessageMetadataUpdateData {
                message_id,
                model_info: end.message.model_info.clone(),
                token_usage: end.message.token_usage.clone(),
                context_breakdown: end.message.context_breakdown.clone(),
            },
        )])
    }

    fn synthesize_stream_end_before(&mut self, event: &ChatEvent) -> Option<ChatEvent> {
        if matches!(event, ChatEvent::TypingStatusChanged(false)) && self.open {
            let stream_end = self.synthetic_stream_end();
            if let ChatEvent::StreamEnd(end) = &stream_end {
                self.synthetic_completion = Some(SyntheticTycodeCompletion {
                    message_id: end.message.message_id.clone(),
                    content: end.message.content.clone(),
                    reasoning_text: end
                        .message
                        .reasoning
                        .as_ref()
                        .map(|reasoning| reasoning.text.clone()),
                });
            }
            self.open = false;
            return Some(stream_end);
        }

        None
    }

    fn synthetic_stream_end(&self) -> ChatEvent {
        ChatEvent::StreamEnd(StreamEndData {
            message: ChatMessage {
                message_id: self.message_id.clone().map(ChatMessageId),
                timestamp: unix_now_ms(),
                sender: MessageSender::Assistant {
                    agent: self.agent.clone().unwrap_or_else(|| "tycode".to_string()),
                },
                content: self.accumulated_text.clone(),
                reasoning: (!self.accumulated_reasoning.is_empty()).then(|| ReasoningData {
                    text: self.accumulated_reasoning.clone(),
                    tokens: None,
                    signature: None,
                    blob: None,
                }),
                tool_calls: Vec::new(),
                model_info: self.model.clone().map(|model| ModelInfo { model }),
                token_usage: None,
                context_breakdown: None,
                images: None,
            },
        })
    }

    fn update(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::TypingStatusChanged(true) | ChatEvent::StreamStart(_) => {
                if let ChatEvent::StreamStart(start) = event {
                    self.open = true;
                    self.message_id.clone_from(&start.message_id);
                    self.agent = Some(start.agent.clone());
                    self.model.clone_from(&start.model);
                    self.accumulated_text.clear();
                    self.accumulated_reasoning.clear();
                }
                self.synthetic_completion = None;
            }
            ChatEvent::StreamDelta(StreamTextDeltaData { message_id, text }) if self.open => {
                if let Some(message_id) = message_id {
                    self.message_id = Some(message_id.clone());
                }
                self.accumulated_text.push_str(text);
            }
            ChatEvent::StreamReasoningDelta(StreamTextDeltaData {
                message_id, text, ..
            }) if self.open => {
                if let Some(message_id) = message_id {
                    self.message_id = Some(message_id.clone());
                }
                self.accumulated_reasoning.push_str(text);
            }
            ChatEvent::StreamEnd(_) => {
                self.open = false;
                self.synthetic_completion = None;
            }
            _ => {}
        }
    }

    fn warn_if_late_stream_end_has_unmerged_fields(
        &self,
        synthetic: &SyntheticTycodeCompletion,
        message: &ChatMessage,
    ) {
        let authoritative_reasoning = message
            .reasoning
            .as_ref()
            .map(|reasoning| reasoning.text.as_str());
        if message.content != synthetic.content
            || authoritative_reasoning != synthetic.reasoning_text.as_deref()
            || !message.tool_calls.is_empty()
            || message
                .images
                .as_ref()
                .is_some_and(|images| !images.is_empty())
        {
            tracing::warn!(
                message_id = ?message.message_id,
                "Delayed Tycode StreamEnd after synthesized completion contains content fields \
                 that cannot be merged into the already visible assistant message without a \
                 duplicate StreamEnd"
            );
        }
    }
}

fn tycode_events_with_synthesized_completion(
    events: Vec<ChatEvent>,
    stream_state: &mut TycodeStreamState,
) -> Vec<ChatEvent> {
    stream_state.events_with_synthesized_completion(events)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::*;
    use protocol::{
        OrchestrationAgentOrigin, OrchestrationPayload, SendMessagePayload, TokenUsageScope,
        TokenUsageUnavailableReason,
    };
    use tempfile::TempDir;

    const TEST_TYCODE_STARTUP_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

    fn managed_recovery_tokens(
        snapshot: &BackendNativeSettingsSnapshot,
    ) -> (TycodeProjectionId, TycodeProjectionStateHash) {
        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Unavailable);
        let Some(TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
            expected_projection_id,
            expected_state_hash,
            ..
        }) = snapshot.managed_projection_recovery.as_ref()
        else {
            panic!("expected typed managed projection recovery");
        };
        (expected_projection_id.clone(), expected_state_hash.clone())
    }

    async fn reset_exact_recovery_snapshot(
        snapshot: &BackendNativeSettingsSnapshot,
        paths: &TycodeProjectionPaths,
        shared: &[u8],
    ) {
        let (projection_id, state_hash) = managed_recovery_tokens(snapshot);
        reset_tycode_managed_projection(&projection_id, &state_hash)
            .await
            .expect("exact recovery tokens reset managed state");
        assert!(!paths.managed.exists());
        assert!(!paths.provenance.exists());
        assert!(!paths.transaction.exists());
        assert!(!paths.recovery.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
    }

    #[test]
    fn tycode_session_settings_schema_omits_orchestration_until_supported() {
        let _guard = TestTycodeRootAgentSupportGuard::set(false);

        let schema = tycode_session_settings_schema();
        assert_eq!(schema.backend_kind, BackendKind::Tycode);
        assert!(schema.fields.is_empty());
    }

    #[test]
    fn tycode_session_settings_schema_exposes_orchestration_slider_when_supported() {
        let _guard = TestTycodeRootAgentSupportGuard::set(true);

        let schema = tycode_session_settings_schema();
        assert_eq!(schema.backend_kind, BackendKind::Tycode);
        assert_eq!(schema.fields.len(), 1);
        let field = &schema.fields[0];
        assert_eq!(field.key, "default_agent");
        assert_eq!(field.label, "Orchestration");
        assert!(field.use_slider);
        match &field.field_type {
            SessionSettingFieldType::Select {
                options,
                default,
                nullable,
            } => {
                assert_eq!(default.as_deref(), Some("tycode"));
                assert!(!nullable);
                assert_eq!(
                    options
                        .iter()
                        .map(|option| (option.value.as_str(), option.label.as_str()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("one_shot", "None"),
                        ("tycode", "Auto"),
                        ("builder", "Pipeline"),
                        ("swarm", "Swarm"),
                    ]
                );
            }
            other => panic!("default_agent should be Select, got {other:?}"),
        }
    }

    #[test]
    fn tycode_resolve_session_settings_keeps_only_explicit_root_agent() {
        let default_config = BackendSpawnConfig::default();
        assert!(resolve_session_settings(&default_config).0.is_empty());

        let mut config = BackendSpawnConfig {
            session_settings: Some(SessionSettingsValues::default()),
            ..Default::default()
        };
        config
            .session_settings
            .as_mut()
            .expect("session settings")
            .0
            .insert(
                "default_agent".to_string(),
                SessionSettingValue::String("tycode".to_string()),
            );
        assert_eq!(
            resolve_session_settings(&config).0.get("default_agent"),
            Some(&SessionSettingValue::String("tycode".to_string()))
        );
    }

    #[test]
    fn tycode_omits_legacy_backend_config_schema() {
        assert!(TycodeBackend::backend_config_schema().is_none());
    }

    #[test]
    fn tycode_settings_overlay_preserves_providers_and_unmanaged_keys() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "secret",
                    "unmanaged_provider_key": { "keep": true }
                },
                "other": {
                    "type": "codex",
                    "command": "codex"
                }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "orchestration_progress_messages": true,
            "disable_custom_steering": false,
            "disable_streaming": false,
            "unmanaged_top_level": { "still": "here" }
        });
        let original_providers = settings["providers"].clone();
        let mut config = BackendConfigValues::default();
        config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        config.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );
        config.0.insert(
            "reasoning_effort".to_string(),
            SessionSettingValue::String("Max".to_string()),
        );
        config.0.insert(
            "review_level".to_string(),
            SessionSettingValue::String("Task".to_string()),
        );
        config.0.insert(
            "spawn_context_mode".to_string(),
            SessionSettingValue::String("Fresh".to_string()),
        );
        config.0.insert(
            "unknown".to_string(),
            SessionSettingValue::String("ignored".to_string()),
        );

        let overlay = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");
        assert_eq!(overlay.active_provider_change.as_deref(), Some("other"));
        assert_eq!(overlay.settings["providers"], original_providers);
        assert_eq!(
            overlay.settings["unmanaged_top_level"],
            serde_json::json!({ "still": "here" })
        );
        assert_eq!(overlay.settings["active_provider"], "other");
        assert_eq!(overlay.settings["model_quality"], "low");
        assert_eq!(overlay.settings["reasoning_effort"], "Max");
        assert_eq!(overlay.settings["review_level"], "Task");
        assert_eq!(overlay.settings["spawn_context_mode"], "Fresh");
        assert_eq!(overlay.settings["orchestration_progress_messages"], false);
        assert_eq!(overlay.settings["disable_custom_steering"], false);
        assert_eq!(overlay.settings["disable_streaming"], false);
        assert!(overlay.settings.get("unknown").is_none());
    }

    #[test]
    fn tycode_settings_overlay_treats_nullable_auto_as_noop() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let mut config = BackendConfigValues::default();
        for key in [
            "model_quality",
            "reasoning_effort",
            "autonomy_level",
            "review_level",
            "spawn_context_mode",
        ] {
            config.0.insert(key.to_string(), SessionSettingValue::Null);
        }

        let overlay = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");
        assert_eq!(overlay.settings, settings);
        assert_eq!(overlay.active_provider_change, None);
    }

    #[test]
    fn tycode_settings_overlay_keeps_default_agent_out_of_save_settings() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );

        let overlay = apply_tycode_settings_overlay(
            &settings,
            &BackendConfigValues::default(),
            &session_settings,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect("overlay settings");

        assert_eq!(overlay.settings["default_agent"], "tycode");
        assert_eq!(overlay.settings["orchestration_progress_messages"], false);
    }

    #[test]
    fn tycode_runtime_session_settings_reject_default_agent_update() {
        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<TycodeStdinCommand>();
        let mut runtime_settings = Some(serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        }));
        let mut update = SessionSettingsValues::default();
        update.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );

        let err =
            send_tycode_runtime_session_settings_update(&mut runtime_settings, &update, &stdin_tx)
                .expect_err("live default_agent update should be rejected");

        assert!(err.contains("cannot be changed on a running session"));
        assert!(stdin_rx.try_recv().is_err());
        assert_eq!(
            runtime_settings.expect("runtime settings")["default_agent"],
            "tycode"
        );
    }

    #[test]
    fn tycode_message_added_flat_token_usage_maps_to_typed_usage_scopes() {
        let events = map_tycode_value_to_chat_events(&tycode_message_added_with_flat_usage());

        assert_eq!(events.len(), 1);
        let ChatEvent::MessageAdded(message) = &events[0] else {
            panic!("expected MessageAdded, got {:?}", events[0]);
        };
        assert_flat_usage_mapped(message);
    }

    #[test]
    fn tycode_stream_end_flat_token_usage_maps_to_typed_usage_scopes() {
        let events = map_tycode_value_to_chat_events(&serde_json::json!({
            "kind": "StreamEnd",
            "data": {
                "message": tycode_assistant_message_with_flat_usage()
            }
        }));

        assert_eq!(events.len(), 1);
        let ChatEvent::StreamEnd(end) = &events[0] else {
            panic!("expected StreamEnd, got {:?}", events[0]);
        };
        assert_flat_usage_mapped(&end.message);
    }

    #[test]
    fn tycode_malformed_known_event_surfaces_error_message() {
        let events = map_tycode_value_to_chat_events(&serde_json::json!({
            "kind": "StreamEnd",
            "data": {
                "message": {
                    "timestamp": "not a number"
                }
            }
        }));

        assert_eq!(events.len(), 2);
        let ChatEvent::MessageAdded(message) = &events[0] else {
            panic!("expected visible error message, got {:?}", events[0]);
        };
        assert!(matches!(&message.sender, MessageSender::Error));
        assert!(
            message.content.contains("Malformed Tycode StreamEnd event"),
            "unexpected error content: {}",
            message.content
        );
        assert!(
            matches!(events[1], ChatEvent::StreamEnd(_)),
            "malformed StreamEnd should still close the stream, got {:?}",
            events[1]
        );
        let mut stream_state = TycodeStreamState {
            open: true,
            accumulated_text: "partial".to_string(),
            ..Default::default()
        };
        for event in &events {
            stream_state.update(event);
        }
        assert!(
            !stream_state.open,
            "malformed StreamEnd must not leave cursor active"
        );
    }

    #[test]
    fn tycode_typing_false_synthesizes_stream_end_before_idle() {
        let raw_events = [
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": true
            }),
            serde_json::json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": "m1",
                    "agent": "tycode",
                    "model": "ClaudeSonnet46"
                }
            }),
            serde_json::json!({
                "kind": "StreamDelta",
                "data": {
                    "message_id": "m1",
                    "text": "Acknowledged."
                }
            }),
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": false
            }),
        ];
        let mut stream_state = TycodeStreamState::default();
        let mut emitted = Vec::new();
        for raw_event in raw_events {
            emitted.extend(tycode_events_with_synthesized_completion(
                map_tycode_value_to_chat_events(&raw_event),
                &mut stream_state,
            ));
        }

        assert_eq!(
            event_kinds(&emitted),
            vec![
                "TypingStatusChanged(true)",
                "StreamStart",
                "StreamDelta",
                "StreamEnd",
                "TypingStatusChanged(false)",
            ]
        );
        let ChatEvent::StreamEnd(end) = &emitted[3] else {
            panic!("expected synthesized StreamEnd, got {:?}", emitted[3]);
        };
        assert_eq!(end.message.content, "Acknowledged.");
        assert!(matches!(
            &end.message.sender,
            MessageSender::Assistant { agent } if agent == "tycode"
        ));
        assert!(!stream_state.open);
    }

    #[test]
    fn tycode_late_real_stream_end_updates_synthetic_completion_metadata() {
        let raw_events = [
            serde_json::json!({
                "kind": "StreamStart",
                "data": {
                    "message_id": "m1",
                    "agent": "tycode",
                    "model": "stream-model"
                }
            }),
            serde_json::json!({
                "kind": "StreamDelta",
                "data": {
                    "message_id": "m1",
                    "text": "Authoritative content."
                }
            }),
            serde_json::json!({
                "kind": "TypingStatusChanged",
                "data": false
            }),
            serde_json::json!({
                "kind": "StreamEnd",
                "data": {
                    "message": {
                        "message_id": "m1",
                        "timestamp": 1776827246365_u64,
                        "sender": {
                            "Assistant": {
                                "agent": "tycode"
                            }
                        },
                        "content": "Authoritative content.",
                        "reasoning": null,
                        "tool_calls": [],
                        "model_info": {
                            "model": "authoritative-model"
                        },
                        "token_usage": {
                            "input_tokens": 11,
                            "output_tokens": 7,
                            "total_tokens": 18
                        },
                        "context_breakdown": {
                            "system_prompt_bytes": 101,
                            "tool_io_bytes": 102,
                            "conversation_history_bytes": 103,
                            "reasoning_bytes": 104,
                            "context_injection_bytes": 105,
                            "input_tokens": 11,
                            "context_window": 200000
                        },
                        "images": []
                    }
                }
            }),
        ];
        let mut stream_state = TycodeStreamState::default();
        let mut emitted = Vec::new();
        for raw_event in raw_events {
            emitted.extend(tycode_events_with_synthesized_completion(
                map_tycode_value_to_chat_events(&raw_event),
                &mut stream_state,
            ));
        }

        assert_eq!(
            event_kinds(&emitted),
            vec![
                "StreamStart",
                "StreamDelta",
                "StreamEnd",
                "TypingStatusChanged(false)",
                "MessageMetadataUpdated",
            ]
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .count(),
            1,
            "delayed real StreamEnd must not create a duplicate visible message"
        );
        let ChatEvent::StreamEnd(end) = &emitted[2] else {
            panic!("expected synthesized StreamEnd, got {:?}", emitted[2]);
        };
        assert_eq!(
            end.message.message_id,
            Some(ChatMessageId("m1".to_string()))
        );
        assert_eq!(end.message.content, "Authoritative content.");

        let ChatEvent::MessageMetadataUpdated(update) = &emitted[4] else {
            panic!("expected late metadata update, got {:?}", emitted[4]);
        };
        assert_eq!(update.message_id, ChatMessageId("m1".to_string()));
        assert_eq!(
            update.model_info.as_ref().map(|model| model.model.as_str()),
            Some("authoritative-model")
        );
        let usage = update
            .token_usage
            .as_ref()
            .expect("late real StreamEnd token usage should be preserved");
        for scope in [&usage.request, &usage.turn] {
            let TokenUsageScope::Known { usage } = scope else {
                panic!("request and turn usage should be known, got {scope:?}");
            };
            assert_eq!(usage.input_tokens, 11);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.total_tokens, 18);
        }
        let context = update
            .context_breakdown
            .as_ref()
            .expect("late real StreamEnd context should be preserved");
        assert_eq!(context.system_prompt_bytes, 101);
        assert_eq!(context.tool_io_bytes, 102);
        assert_eq!(context.conversation_history_bytes, 103);
        assert_eq!(context.reasoning_bytes, 104);
        assert_eq!(context.context_injection_bytes, 105);
        assert_eq!(context.input_tokens, 11);
        assert_eq!(context.context_window, 200000);
    }

    #[test]
    fn tycode_real_stream_end_prevents_typing_false_synthesis() {
        let mut stream_state = TycodeStreamState::default();
        let emitted = tycode_events_with_synthesized_completion(
            vec![
                ChatEvent::TypingStatusChanged(true),
                ChatEvent::StreamStart(protocol::StreamStartData {
                    message_id: Some("m1".to_string()),
                    agent: "tycode".to_string(),
                    model: Some("ClaudeSonnet46".to_string()),
                }),
                ChatEvent::StreamDelta(StreamTextDeltaData {
                    message_id: Some("m1".to_string()),
                    text: "Acknowledged.".to_string(),
                }),
                ChatEvent::StreamEnd(StreamEndData {
                    message: tycode_assistant_chat_message("Real end."),
                }),
                ChatEvent::TypingStatusChanged(false),
            ],
            &mut stream_state,
        );

        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, ChatEvent::StreamEnd(_)))
                .count(),
            1
        );
        let ChatEvent::StreamEnd(end) = &emitted[3] else {
            panic!("expected real StreamEnd, got {:?}", emitted[3]);
        };
        assert_eq!(end.message.content, "Real end.");
        assert_eq!(event_kinds(&emitted)[4], "TypingStatusChanged(false)");
    }

    #[test]
    fn tycode_diagnostics_redact_settings_payloads_but_keep_event_kind() {
        let diagnostic = tycode_line_diagnostic(
            r#"{"kind":"Settings","data":{"providers":{"openrouter":{"api_key":"secret"}}}}"#,
        );

        assert_eq!(diagnostic, r#"{"kind":"Settings","data":"<redacted>"}"#);
    }

    #[test]
    fn tycode_stderr_diagnostics_preserve_sanitized_context() {
        let diagnostic =
            tycode_text_diagnostic("Fatal provider setup failed: api_key sk-secret-value invalid");

        assert!(diagnostic.contains("Fatal provider setup failed"));
        assert!(diagnostic.contains("api_key"));
        assert!(!diagnostic.contains("sk-secret-value"));
    }

    fn tycode_message_added_with_flat_usage() -> Value {
        serde_json::json!({
            "kind": "MessageAdded",
            "data": tycode_assistant_message_with_flat_usage()
        })
    }

    fn tycode_assistant_message_with_flat_usage() -> Value {
        serde_json::json!({
            "timestamp": 123,
            "sender": {
                "Assistant": {
                    "agent": "tycode"
                }
            },
            "content": "done",
            "reasoning": null,
            "tool_calls": [],
            "model_info": {
                "model": "claude-fable",
                "version": "claude-fable-5"
            },
            "token_usage": {
                "input_tokens": 11,
                "output_tokens": 7,
                "total_tokens": 18,
                "cached_prompt_tokens": 3,
                "cache_creation_input_tokens": 5,
                "reasoning_tokens": 2
            },
            "context_breakdown": null,
            "images": []
        })
    }

    fn assert_flat_usage_mapped(message: &ChatMessage) {
        let usage = message
            .token_usage
            .as_ref()
            .expect("flat usage should map to MessageTokenUsage");
        for scope in [&usage.request, &usage.turn] {
            let TokenUsageScope::Known { usage } = scope else {
                panic!("request and turn usage should be known, got {scope:?}");
            };
            assert_eq!(usage.input_tokens, 11);
            assert_eq!(usage.output_tokens, 7);
            assert_eq!(usage.total_tokens, 18);
            assert_eq!(usage.cached_prompt_tokens, Some(3));
            assert_eq!(usage.cache_creation_input_tokens, Some(5));
            assert_eq!(usage.reasoning_tokens, Some(2));
        }
        assert_eq!(
            usage.cumulative,
            TokenUsageScope::Unavailable {
                reason: TokenUsageUnavailableReason::BackendDidNotReport
            }
        );
    }

    fn event_kinds(events: &[ChatEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                ChatEvent::TypingStatusChanged(true) => "TypingStatusChanged(true)",
                ChatEvent::TypingStatusChanged(false) => "TypingStatusChanged(false)",
                ChatEvent::StreamStart(_) => "StreamStart",
                ChatEvent::StreamDelta(_) => "StreamDelta",
                ChatEvent::StreamEnd(_) => "StreamEnd",
                ChatEvent::MessageAdded(_) => "MessageAdded",
                ChatEvent::MessageMetadataUpdated(_) => "MessageMetadataUpdated",
                ChatEvent::StreamReasoningDelta(_) => "StreamReasoningDelta",
                ChatEvent::ToolRequest(_) => "ToolRequest",
                ChatEvent::ToolProgress(_) => "ToolProgress",
                ChatEvent::ToolExecutionCompleted(_) => "ToolExecutionCompleted",
                ChatEvent::TaskUpdate(_) => "TaskUpdate",
                ChatEvent::OperationCancelled(_) => "OperationCancelled",
                ChatEvent::RetryAttempt(_) => "RetryAttempt",
                ChatEvent::Orchestration(_) => "Orchestration",
            })
            .collect()
    }

    fn tycode_assistant_chat_message(content: &str) -> ChatMessage {
        ChatMessage {
            message_id: None,
            timestamp: 123,
            sender: MessageSender::Assistant {
                agent: "tycode".to_string(),
            },
            content: content.to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
            model_info: None,
            token_usage: None,
            context_breakdown: None,
            images: None,
        }
    }

    #[test]
    fn tycode_settings_overlay_rejects_absent_active_provider() {
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            }
        });
        let mut config = BackendConfigValues::default();
        config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("missing".to_string()),
        );

        let err = apply_tycode_backend_config_overlay(
            &settings,
            &config,
            TycodeSettingsOverlayMode::SessionRuntime,
        )
        .expect_err("missing provider");
        assert!(err.contains("Configured Tycode active_provider 'missing' is absent"));
        assert!(err.contains("available: default"));
    }

    #[test]
    fn tycode_settings_classifier_keeps_only_pre_session_sender_error_advisory() {
        let event = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "sender": "Error",
                "content": "No AI provider is configured. Configure one now."
            }
        });

        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSessionStarted,
                &event
            ),
            TycodeSettingsEventClassification::CollectAdvisory(
                BackendNativeSettingsAdvisory::NoProviderConfigured { .. }
            )
        ));
        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSettingsSchema,
                &event
            ),
            TycodeSettingsEventClassification::Fatal(message)
                if message == "No AI provider is configured. Configure one now."
        ));
    }

    #[test]
    fn tycode_settings_classifier_keeps_structured_error_fatal_before_session() {
        let event = serde_json::json!({
            "kind": "Error",
            "data": "settings loader failed"
        });

        assert!(matches!(
            classify_tycode_settings_event(
                TycodeSettingsOperationPhase::AwaitSessionStarted,
                &event
            ),
            TycodeSettingsEventClassification::Fatal(message)
                if message == "settings loader failed"
        ));
    }

    #[test]
    fn tycode_dangling_active_provider_is_ready_and_typed() {
        let mut snapshot = tycode_native_settings_snapshot_from_schema(&serde_json::json!({
            "settings": {
                "active_provider": "legacy-provider",
                "providers": {
                    "supported": { "type": "mock" }
                }
            },
            "groups": []
        }))
        .expect("valid settings schema");
        let mut advisories = Vec::new();

        add_snapshot_advisories(&mut snapshot, &mut advisories);

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        assert!(snapshot.settings.is_some());
        assert!(matches!(
            advisories.as_slice(),
            [BackendNativeSettingsAdvisory::UnsupportedActiveProvider {
                provider,
                message,
            }]
                if provider == "legacy-provider"
                    && message.contains(
                        "Tyde does not write to or remove the shared Tycode CLI/VS Code settings file"
                    )
                    && !message.contains("unchanged")
                    && !message.contains("still contains")
                    && !message.contains("still there")
        ));
    }

    #[test]
    fn tycode_raw_command_has_exactly_one_settings_path() {
        let command = raw_tycode_command(
            "/tmp/tycode-subprocess",
            Path::new("/tmp/tyde-settings.toml"),
            "[]",
        );
        let arguments = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.as_str() == "--settings-path")
                .count(),
            1
        );
        assert_eq!(
            arguments.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "--settings-path",
                "/tmp/tyde-settings.toml",
                "--workspace-roots",
                "[]"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn tycode_directory_accepts_user_owned_readable_modes_and_rejects_writable_modes() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempDir::new().expect("tempdir");
        let existing = home.path().join(".tycode");
        fs::create_dir(&existing).expect("create existing Tycode directory");
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755))
            .expect("set conventional Tycode directory mode");

        ensure_private_tycode_directory(&existing)
            .expect("user-owned 0755 Tycode directory must be accepted");
        assert_eq!(
            fs::metadata(&existing)
                .expect("stat existing Tycode directory")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "Tyde must never chmod an existing user directory"
        );

        for unsafe_mode in [0o775, 0o707] {
            fs::set_permissions(&existing, fs::Permissions::from_mode(unsafe_mode))
                .expect("set unsafe Tycode directory mode");
            let error = ensure_private_tycode_directory(&existing)
                .expect_err("group/world-writable Tycode directory must be rejected");
            assert!(error.contains("group- or world-writable"));
            assert_eq!(
                fs::metadata(&existing)
                    .expect("stat rejected Tycode directory")
                    .permissions()
                    .mode()
                    & 0o777,
                unsafe_mode,
                "Tyde must not chmod an unsafe existing directory"
            );
        }

        let created = home.path().join("tyde-created");
        ensure_private_tycode_directory(&created).expect("create private Tycode directory");
        assert_eq!(
            fs::metadata(&created)
                .expect("stat Tyde-created directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let wrong_type = home.path().join("wrong-type");
        fs::write(&wrong_type, b"not a directory").expect("write wrong-type fixture");
        assert!(ensure_private_tycode_directory(&wrong_type).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn tycode_fresh_home_directory_process_helper() {
        let Some(directory) = std::env::var_os("TYDE_TEST_TYCODE_DIRECTORY_RACE_TARGET") else {
            return;
        };
        ensure_private_tycode_directory(&PathBuf::from(directory))
            .expect("create or validate raced Tycode directory");
    }

    #[cfg(unix)]
    #[test]
    fn tycode_fresh_home_directory_creation_converges_across_processes() {
        use std::os::unix::fs::PermissionsExt;

        let home = TempDir::new().expect("tempdir");
        let directory = home.path().join("fresh-home").join(".tycode");
        fs::create_dir(directory.parent().expect("fresh home path"))
            .expect("create fresh home without Tycode directory");
        let ready_one = home.path().join("creator-one-ready");
        let ready_two = home.path().join("creator-two-ready");
        let release = home.path().join("release-creators");
        let spawn_creator = |ready: &Path| {
            std::process::Command::new(std::env::current_exe().expect("current test binary"))
                .arg("tycode_fresh_home_directory_process_helper")
                .arg("--nocapture")
                .env("TYDE_TEST_TYCODE_DIRECTORY_RACE_TARGET", &directory)
                .env("TYDE_TEST_TYCODE_DIRECTORY_RACE_READY", ready)
                .env("TYDE_TEST_TYCODE_DIRECTORY_RACE_RELEASE", &release)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn Tycode directory creator")
        };
        let creator_one = spawn_creator(&ready_one);
        let creator_two = spawn_creator(&ready_two);
        for _ in 0..1_000 {
            if ready_one.exists() && ready_two.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let both_ready = ready_one.exists() && ready_two.exists();
        fs::write(&release, b"release").expect("release directory creators");
        for (label, creator) in [("one", creator_one), ("two", creator_two)] {
            let output = creator
                .wait_with_output()
                .expect("wait for Tycode directory creator");
            assert!(
                output.status.success(),
                "creator {label} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            both_ready,
            "both processes must observe the absent Tycode directory before release"
        );
        let metadata = fs::symlink_metadata(&directory).expect("stat converged Tycode directory");
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        ensure_private_tycode_directory(&directory)
            .expect("converged Tycode directory remains valid");
    }

    #[test]
    fn tycode_settings_verification_error_redacts_provider_values() {
        let expected = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "secret"
                }
            },
            "model_quality": "high"
        });
        let actual = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": {
                    "type": "openrouter",
                    "api_key": "different-secret"
                }
            },
            "model_quality": "low"
        });

        let err = tycode_settings_verification_error(&expected, &actual);
        assert!(err.contains("mismatched managed keys: model_quality"));
        assert!(err.contains("providers changed"));
        assert!(!err.contains("secret"));
        assert!(!err.contains("different-secret"));
    }

    #[test]
    fn tycode_semantic_default_comparison_normalizes_only_blank_default_agent() {
        let defaults = serde_json::json!({
            "active_provider": null,
            "default_agent": "tycode",
            "providers": {}
        });
        for agent in ["", " ", "\t\n"] {
            let settings = serde_json::json!({
                "active_provider": null,
                "default_agent": agent,
                "providers": {}
            });
            assert!(tycode_settings_are_semantically_default(
                &settings, &defaults
            ));
            assert_eq!(settings["default_agent"].as_str(), Some(agent));
        }
        assert!(!tycode_settings_are_semantically_default(
            &serde_json::json!({
                "active_provider": null,
                "default_agent": "builder",
                "providers": {}
            }),
            &defaults
        ));
        assert!(!tycode_settings_are_semantically_default(
            &serde_json::json!({
                "active_provider": "   ",
                "default_agent": "tycode",
                "providers": {}
            }),
            &defaults
        ));
        assert!(!tycode_settings_are_semantically_default(
            &serde_json::json!({
                "active_provider": null,
                "default_agent": null,
                "providers": {}
            }),
            &defaults
        ));
    }

    #[test]
    fn tycode_settings_verification_allows_unmanaged_changes() {
        let expected = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "unmanaged": "before"
        });
        let actual = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "unmanaged": "after"
        });

        verify_tycode_settings_overlay(&expected, &actual)
            .expect("unmanaged settings changes should not fail verification");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_applies_settings_before_user_input() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork",
            "disable_custom_steering": false,
            "disable_streaming": false
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        config.backend_config.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("high".to_string()),
        );
        config.backend_config.0.insert(
            "spawn_context_mode".to_string(),
            SessionSettingValue::String("Fresh".to_string()),
        );

        let (backend, mut events) =
            TycodeBackend::spawn(Vec::new(), config, payload("hello Tycode"))
                .await
                .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 5, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "other" })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "high");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert!(save["settings"].get("default_agent").is_none());
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
        );
        assert_eq!(save["settings"]["disable_streaming"], false);
        assert_eq!(save["settings"]["providers"], settings["providers"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_disables_supported_progress_messages_without_user_config() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let (backend, mut events) = TycodeBackend::spawn(
            Vec::new(),
            BackendSpawnConfig::default(),
            payload("hello Tycode"),
        )
        .await
        .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["default_agent"], "tycode");
        assert_eq!(save["settings"]["orchestration_progress_messages"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_tolerates_old_binary_that_drops_unknown_settings() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let (backend, mut events) = TycodeBackend::spawn(
            Vec::new(),
            BackendSpawnConfig::default(),
            payload("hello old Tycode"),
        )
        .await
        .expect("spawn old fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "UserInput": "hello old Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
        );
        assert!(save["settings"].get("default_agent").is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_sets_requested_root_agent_before_user_input() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

        let (backend, mut events) =
            TycodeBackend::spawn(Vec::new(), config, payload("hello Tycode"))
                .await
                .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 5, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "SetRootAgent": { "agent": "swarm" } })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], false);
        assert_eq!(save["settings"]["default_agent"], "tycode");
        assert_eq!(save["settings"]["orchestration_progress_messages"], false);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_surfaces_root_agent_rejection_before_prompt() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess_rejecting_root_agent(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("root agent rejection should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("Tycode SetRootAgent 'swarm' failed"));
        assert!(err.contains("Unknown agent type 'swarm'"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "SetRootAgent": { "agent": "swarm" } })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_rejects_root_agent_when_command_is_unsupported() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_without_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("unsupported SetRootAgent should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("requires SetRootAgent support"));
        assert!(err.contains(TYCODE_VERSION));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 3, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert!(
            commands
                .iter()
                .all(|command| command.get("SetRootAgent").is_none()),
            "unsupported Tycode must not receive SetRootAgent: {commands:#?}"
        );
        assert!(
            commands
                .iter()
                .all(|command| command.get("UserInput").is_none()),
            "prompt must not be sent after root-agent startup rejection: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_rejects_root_agent_for_read_only_session() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "default_agent": "tycode",
            "orchestration_progress_messages": true
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_root_agent_support(fake);

        let mut session_settings = SessionSettingsValues::default();
        session_settings.0.insert(
            "default_agent".to_string(),
            SessionSettingValue::String("swarm".to_string()),
        );
        let config = BackendSpawnConfig {
            session_settings: Some(session_settings),
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        };

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("read-only root override should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("cannot be used with read-only Tycode sessions"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 3, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert!(
            commands
                .iter()
                .all(|command| command.get("SetRootAgent").is_none()),
            "read-only Tycode must not receive SetRootAgent: {commands:#?}"
        );
        assert!(
            commands
                .iter()
                .all(|command| command.get("UserInput").is_none()),
            "prompt must not be sent after read-only root-agent rejection: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_uses_persist_true() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut values = BackendConfigValues::default();
        values.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );
        values.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "low");
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "Task");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_snapshot_carries_current_values_and_groups() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "modules": {
                "memory": { "enabled": true }
            },
            "unrelated": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        let mut expected_settings = settings.clone();
        expected_settings["profile"] = Value::String("default".to_string());
        assert_eq!(snapshot.settings.as_ref(), Some(&expected_settings));
        assert!(
            snapshot
                .groups
                .iter()
                .any(|group| group.id == "providers" && group.settings_path.is_empty()),
            "providers group should be carried through: {:?}",
            snapshot.groups
        );
        assert!(
            snapshot.groups.iter().any(|group| {
                group.id == "module:memory"
                    && group.settings_path == vec!["modules".to_string(), "memory".to_string()]
            }),
            "module group should be carried through: {:?}",
            snapshot.groups
        );
        assert_eq!(
            read_fake_commands(&log),
            vec![Value::String("GetSettingsSchema".to_string())]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_pre_session_error_is_ready_advisory() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess_with_pre_session_advisory(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        assert!(snapshot.settings.is_some());
        assert!(snapshot.provenance.is_some());
        assert!(snapshot.advisories.iter().any(|advisory| matches!(
            advisory,
            BackendNativeSettingsAdvisory::NoProviderConfigured { message }
                if message == "No AI provider is configured. Configure one now."
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_post_command_error_is_unavailable() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess_with_post_command_error(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Unavailable);
        assert!(snapshot.settings.is_none());
        let message = snapshot.message.expect("fatal probe message");
        assert!(message.contains("native settings probe failed"));
        assert!(message.contains("waiting for SettingsSchema"));
        assert!(message.contains("schema command failed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_projection_creation_preserves_shared_bytes_for_pruned_default() {
        let dir = TempDir::new().expect("tempdir");
        let modeled = serde_json::json!({
            "active_provider": null,
            "providers": {},
            "model_quality": null
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &modeled);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        fs::remove_file(&paths.managed).expect("remove bootstrapped managed settings");
        fs::remove_file(&paths.provenance).expect("remove bootstrapped provenance");
        let shared = br#"legacy_secret = "do-not-touch"
"#;
        fs::write(&paths.shared, shared).expect("write shared settings fixture");

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
        let managed = fs::read_to_string(&paths.managed).expect("read managed settings");
        assert!(!managed.contains("legacy_secret"));
        let Some(BackendNativeSettingsProvenance::TycodeManagedProjection {
            source,
            original_unchanged,
            notice_pending,
            ..
        }) = snapshot.provenance
        else {
            panic!("expected managed projection provenance");
        };
        assert_eq!(source, TycodeProjectionSource::SharedSettings);
        assert!(original_unchanged);
        assert!(notice_pending);
        let commands = read_fake_commands(&dir.path().join("commands.jsonl"));
        assert!(
            commands
                .iter()
                .all(|command| command.get("SaveSettings").is_none()),
            "settings that prune to defaults must use nonexistent-path initialization, not SaveSettings: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_nondefault_projection_normalizes_once_then_verifies_fresh() {
        let dir = TempDir::new().expect("tempdir");
        let defaults = serde_json::json!({
            "active_provider": null,
            "providers": {},
            "model_quality": null
        });
        let fake = write_fake_tycode_subprocess_dropping_unknown_settings(dir.path(), &defaults);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        fs::remove_file(&paths.managed).expect("remove bootstrapped managed settings");
        fs::remove_file(&paths.provenance).expect("remove bootstrapped provenance");
        let shared = b"model_quality = \"high\"\n";
        fs::write(&paths.shared, shared).expect("write nondefault shared TOML");

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        assert_eq!(
            snapshot
                .settings
                .as_ref()
                .and_then(|settings| settings.get("model_quality")),
            Some(&Value::String("high".to_string()))
        );
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
        let commands = read_fake_commands(&log);
        let persistent_saves = commands
            .iter()
            .filter(|command| {
                command
                    .get("SaveSettings")
                    .and_then(|save| save.get("persist"))
                    == Some(&Value::Bool(true))
            })
            .count();
        assert_eq!(persistent_saves, 1, "commands: {commands:#?}");
        assert_eq!(
            commands
                .iter()
                .filter(|command| **command == Value::String("GetSettingsSchema".to_string()))
                .count(),
            7,
            "defaults, source, normalized fresh verification, and caller probe are required"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_toml_fallback_preserves_fixture_settings_across_processes() {
        let dir = TempDir::new().expect("tempdir");
        let defaults = serde_json::json!({
            "active_provider": null,
            "providers": {},
            "model_quality": null
        });
        let fake = write_fake_tycode_subprocess_without_tomllib(dir.path(), &defaults);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        fs::remove_file(&paths.managed).expect("remove bootstrapped managed settings");
        fs::remove_file(&paths.provenance).expect("remove bootstrapped provenance");
        let shared = br#"active_provider = "fallback"
labels = ["comma,value", "hash#value", "equals=value"]

[providers.fallback]
type = "mock"

[modules.memory]
enabled = true
"#;
        fs::write(&paths.shared, shared).expect("write portable-parser shared TOML");

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
        let settings = snapshot.settings.expect("portable-parser settings");
        assert_eq!(settings["active_provider"], "fallback");
        assert_eq!(settings["providers"]["fallback"]["type"], "mock");
        assert_eq!(
            settings["modules"]["memory"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            settings["labels"],
            serde_json::json!(["comma,value", "hash#value", "equals=value"])
        );
        assert_eq!(
            fs::read(&paths.shared).expect("read portable-parser shared TOML"),
            shared
        );
        let commands = read_fake_commands(&log);
        assert_eq!(
            commands
                .iter()
                .filter(|command| {
                    command
                        .get("SaveSettings")
                        .and_then(|save| save.get("persist"))
                        == Some(&Value::Bool(true))
                })
                .count(),
            1,
            "portable fallback must retain staged persistence fidelity: {commands:#?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_default_equivalents_use_nonexistent_path_without_save() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "default_agent": "tycode",
            "providers": {},
            "model_quality": null
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        for source in [
            None,
            Some(&b""[..]),
            Some(&b"# comments only\n"[..]),
            Some(&b"[providers]\n"[..]),
            Some(&b"default_agent = \"   \"\n"[..]),
        ] {
            if paths.managed.exists() {
                fs::remove_file(&paths.managed).expect("remove managed settings");
            }
            if paths.provenance.exists() {
                fs::remove_file(&paths.provenance).expect("remove provenance");
            }
            if paths.shared.exists() {
                fs::remove_file(&paths.shared).expect("remove shared settings");
            }
            if let Some(source) = source {
                fs::write(&paths.shared, source).expect("write default-equivalent shared TOML");
            }

            let snapshot = native_settings_snapshot().await;

            assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Ready);
            assert_eq!(
                snapshot
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.get("default_agent")),
                Some(&Value::String("tycode".to_string()))
            );
            assert!(paths.managed.exists());
            assert!(
                fs::read_to_string(&paths.managed)
                    .expect("read default managed TOML")
                    .contains("providers")
            );
        }
        let commands = read_fake_commands(&log);
        assert_eq!(
            commands
                .iter()
                .filter(|command| **command == Value::String("GetSettingsSchema".to_string()))
                .count(),
            19,
            "each default uses initialization and fresh verification; existing sources also receive a typed probe"
        );
        assert!(
            commands
                .iter()
                .all(|command| command.get("SaveSettings").is_none())
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_refuses_persistent_save_of_semantic_defaults() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "default_agent": "tycode",
            "providers": {},
            "model_quality": null
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let semantic_defaults = dir.path().join("semantic-defaults.toml");
        fs::write(&semantic_defaults, b"default_agent = \"   \"\n")
            .expect("write whitespace-default Tycode settings");
        let command = tycode_staged_command(
            TycodeCommandPurpose::ProjectionNormalization,
            &semantic_defaults,
        )
        .await
        .expect("build fake normalization command");

        let error = match run_tycode_settings_operation(
            command,
            TycodeCommandPurpose::ProjectionNormalization,
            TycodeSettingsOperation::Normalize,
        )
        .await
        {
            Ok(_) => panic!("Tycode must refuse forbidden default SaveSettings"),
            Err(error) => error,
        };

        assert!(error.contains("Refusing to persist empty settings"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_projection_notice_acknowledgement_is_atomic_and_id_scoped() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let mut record = load_projection_record(&paths.provenance).expect("projection record");
        let BackendNativeSettingsProvenance::TycodeManagedProjection {
            projection_id,
            notice_pending,
            ..
        } = &mut record.provenance;
        *notice_pending = true;
        let projection_id = projection_id.clone();
        fs::remove_file(&paths.provenance).expect("replace provenance");
        write_private_file(
            &paths.provenance,
            &serde_json::to_vec_pretty(&record).expect("encode projection record"),
        )
        .expect("write pending provenance");

        acknowledge_tycode_projection_notice(&projection_id)
            .await
            .expect("acknowledge current projection");

        let acknowledged =
            load_projection_record(&paths.provenance).expect("acknowledged projection record");
        let BackendNativeSettingsProvenance::TycodeManagedProjection { notice_pending, .. } =
            acknowledged.provenance;
        assert!(!notice_pending);
        let stale = TycodeProjectionId("stale-projection".to_string());
        assert!(matches!(
            acknowledge_tycode_projection_notice(&stale).await,
            Err(TycodeProjectionNoticeAcknowledgementError::Conflict(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn tycode_locked_startup_cleans_every_reserved_prejournal_stage() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let shared = b"api_key = \"shared-secret-must-survive\"\n";
        fs::write(&paths.shared, shared).expect("write shared settings");
        let names = [
            ".tyde-settings.prejournal-default-crash.txn",
            ".tyde-settings.prejournal-source-crash.txn",
            ".tyde-settings.prejournal-provenance-crash.txn",
            ".tyde-settings.prejournal-acknowledgement-managed-crash.txn",
            ".tyde-settings.prejournal-acknowledgement-provenance-crash.txn",
            ".tyde-settings.prejournal-save-managed-crash.txn",
            ".tyde-settings.prejournal-save-provenance-crash.txn",
            ".tyde-settings.atomic-crash.txn",
            ".tyde-settings.default.00000000-0000-4000-8000-000000000001.tmp",
            ".tyde-settings.source.00000000-0000-4000-8000-000000000002.tmp",
            ".tyde-settings.provenance.00000000-0000-4000-8000-000000000003.tmp",
            ".tyde-settings.acknowledgement.00000000-0000-4000-8000-000000000004.tmp",
            ".tyde-settings.00000000-0000-4000-8000-000000000005.tmp",
            ".tyde-settings.toml.00000000-0000-4000-8000-000000000006.tmp",
            ".tyde-settings.provenance.json.00000000-0000-4000-8000-000000000007.tmp",
            ".tyde-settings.transaction.json.00000000-0000-4000-8000-000000000008.tmp",
            ".tyde-settings.recovery.json.00000000-0000-4000-8000-000000000009.tmp",
        ];
        for name in [
            ".tyde-settings.prejournal-unknown-crash.txn",
            ".tyde-settings.default.not-a-uuid.tmp",
            ".tyde-settings.source-crash.tmp",
            ".tyde-settings.user-owned.tmp",
            "tyde-settings.prejournal-source-crash.txn",
        ] {
            assert!(!is_reserved_transaction_artifact_name(name));
        }
        for name in names {
            assert!(is_reserved_transaction_artifact_name(name));
            write_private_file(&paths.directory.join(name), b"secret-bearing-stage")
                .expect("write reserved pre-journal stage");
        }
        let permissive_default = paths
            .directory
            .join(".tyde-settings.prejournal-default-crash.txn");
        fs::set_permissions(&permissive_default, fs::Permissions::from_mode(0o644))
            .expect("model Tycode-created pre-chmod default stage");

        let filesystem_lock = acquire_tycode_filesystem_lock(&paths)
            .await
            .expect("acquire projection filesystem lock");
        recover_tycode_transaction(&paths).expect("clean all unjournaled reserved stages");
        drop(filesystem_lock);

        for name in names {
            assert!(
                !paths.directory.join(name).exists(),
                "orphaned pre-journal stage survived cleanup: {name}"
            );
        }
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
        assert!(
            reserved_transaction_artifact_paths(&paths)
                .expect("enumerate reserved artifacts")
                .is_empty()
        );

        let outside = dir.path().join("outside-stage-target");
        fs::write(&outside, b"outside data must survive").expect("write outside target");
        let reserved_link = paths
            .directory
            .join(".tyde-settings.prejournal-source-symlink-crash.txn");
        std::os::unix::fs::symlink(&outside, &reserved_link)
            .expect("write reserved pre-journal symlink");
        let recovery = native_settings_snapshot().await;
        reset_exact_recovery_snapshot(&recovery, &paths, shared).await;
        assert!(!reserved_link.exists());
        assert_eq!(
            fs::read(&outside).expect("read outside target"),
            b"outside data must survive"
        );
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_corrupt_local_metadata_always_reaches_exact_typed_reset() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let shared = b"# shared source must remain byte-identical\n";
        fs::write(&paths.shared, shared).expect("write shared settings");

        let mut blank_id_record =
            load_projection_record(&paths.provenance).expect("load projection record");
        let BackendNativeSettingsProvenance::TycodeManagedProjection { projection_id, .. } =
            &mut blank_id_record.provenance;
        *projection_id = TycodeProjectionId("   ".to_string());
        atomic_write_private(
            &paths.provenance,
            &serde_json::to_vec_pretty(&blank_id_record).expect("encode blank-ID provenance"),
            &paths.directory,
        )
        .expect("write blank-ID provenance");
        let blank_id_snapshot = native_settings_snapshot().await;
        let (recovery_id, _) = managed_recovery_tokens(&blank_id_snapshot);
        assert!(recovery_id.0.starts_with("recovery-"));
        assert!(valid_projection_id(&recovery_id));
        reset_exact_recovery_snapshot(&blank_id_snapshot, &paths, shared).await;
        assert_eq!(
            native_settings_snapshot().await.status,
            BackendConfigSnapshotStatus::Ready
        );

        atomic_write_private(
            &paths.provenance,
            b"{malformed provenance",
            &paths.directory,
        )
        .expect("write malformed provenance");
        let malformed_provenance = native_settings_snapshot().await;
        let (recovery_id, _) = managed_recovery_tokens(&malformed_provenance);
        assert!(recovery_id.0.starts_with("recovery-"));
        reset_exact_recovery_snapshot(&malformed_provenance, &paths, shared).await;
        assert_eq!(
            native_settings_snapshot().await.status,
            BackendConfigSnapshotStatus::Ready
        );

        atomic_write_private(&paths.transaction, b"{malformed journal", &paths.directory)
            .expect("write malformed journal");
        let malformed_journal = native_settings_snapshot().await;
        reset_exact_recovery_snapshot(&malformed_journal, &paths, shared).await;
        assert_eq!(
            native_settings_snapshot().await.status,
            BackendConfigSnapshotStatus::Ready
        );

        let outside = dir.path().join("outside-reserved-transaction.txn");
        fs::write(&outside, b"must not be read or deleted").expect("write outside sentinel");
        let before = current_pair_identity(&paths)
            .expect("read current pair")
            .expect("current pair");
        let escaped_artifact = TycodeTransactionArtifact {
            path: projection_path_value(&outside),
            digest: tycode_digest(b"must not be read or deleted"),
        };
        let escaped_journal = TycodeTransactionRecord {
            transaction_id: "escaped-journal".to_string(),
            operation: TycodeTransactionOperation::Save,
            phase: TycodeTransactionPhase::Prepared,
            before: Some(before),
            after: None,
            managed_stage: Some(escaped_artifact),
            provenance_stage: None,
            managed_backup: None,
            provenance_backup: None,
            reset_artifacts: Vec::new(),
            reset_state_hash: None,
        };
        write_transaction(&paths, &escaped_journal).expect("write escaped journal");
        let escaped_snapshot = native_settings_snapshot().await;
        assert_eq!(
            fs::read(&outside).expect("read outside sentinel"),
            b"must not be read or deleted"
        );
        reset_exact_recovery_snapshot(&escaped_snapshot, &paths, shared).await;
        assert_eq!(
            fs::read(&outside).expect("read outside sentinel after reset"),
            b"must not be read or deleted"
        );
        assert_eq!(
            native_settings_snapshot().await.status,
            BackendConfigSnapshotStatus::Ready
        );
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_transaction_recovery_resolves_every_durable_pair_phase() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let before_managed = fs::read(&paths.managed).expect("read before managed settings");
        let before_provenance = fs::read(&paths.provenance).expect("read before provenance");
        let before = pair_identity_from_bytes(&before_managed, &before_provenance)
            .expect("before pair identity");
        let after_managed = b"model_quality = \"low\"\n".to_vec();
        let mut after_record =
            load_projection_record(&paths.provenance).expect("projection record");
        after_record.managed_digest = tycode_digest(&after_managed);
        let after_provenance =
            serde_json::to_vec_pretty(&after_record).expect("encode after provenance");
        let after = pair_identity_from_bytes(&after_managed, &after_provenance)
            .expect("after pair identity");

        for (label, phase, published_components, partial_cleanup) in [
            ("prepared", TycodeTransactionPhase::Prepared, 0, false),
            (
                "provenance-before-phase",
                TycodeTransactionPhase::Prepared,
                1,
                false,
            ),
            (
                "provenance-published",
                TycodeTransactionPhase::ProvenancePublished,
                1,
                false,
            ),
            (
                "settings-before-phase",
                TycodeTransactionPhase::ProvenancePublished,
                2,
                false,
            ),
            (
                "settings-published",
                TycodeTransactionPhase::SettingsPublished,
                2,
                false,
            ),
            ("committed", TycodeTransactionPhase::Committed, 2, false),
            ("cleaning", TycodeTransactionPhase::Cleaning, 2, true),
        ] {
            atomic_write_private(&paths.managed, &before_managed, &paths.directory)
                .expect("restore before managed settings");
            atomic_write_private(&paths.provenance, &before_provenance, &paths.directory)
                .expect("restore before provenance");
            let transaction_id = format!("phase-{label}");
            let managed_stage_path = paths
                .directory
                .join(format!(".tyde-settings.{transaction_id}.managed-stage.txn"));
            let provenance_stage_path = paths.directory.join(format!(
                ".tyde-settings.{transaction_id}.provenance-stage.txn"
            ));
            let managed_backup_path = paths.directory.join(format!(
                ".tyde-settings.{transaction_id}.managed-backup.txn"
            ));
            let provenance_backup_path = paths.directory.join(format!(
                ".tyde-settings.{transaction_id}.provenance-backup.txn"
            ));
            write_private_file(&managed_stage_path, &after_managed).expect("write managed stage");
            write_private_file(&provenance_stage_path, &after_provenance)
                .expect("write provenance stage");
            write_private_file(&managed_backup_path, &before_managed)
                .expect("write managed backup");
            write_private_file(&provenance_backup_path, &before_provenance)
                .expect("write provenance backup");
            let transaction = TycodeTransactionRecord {
                transaction_id,
                operation: TycodeTransactionOperation::Save,
                phase,
                before: Some(before.clone()),
                after: Some(after.clone()),
                managed_stage: Some(
                    transaction_artifact(&managed_stage_path).expect("managed stage artifact"),
                ),
                provenance_stage: Some(
                    transaction_artifact(&provenance_stage_path)
                        .expect("provenance stage artifact"),
                ),
                managed_backup: Some(
                    transaction_artifact(&managed_backup_path).expect("managed backup artifact"),
                ),
                provenance_backup: Some(
                    transaction_artifact(&provenance_backup_path)
                        .expect("provenance backup artifact"),
                ),
                reset_artifacts: Vec::new(),
                reset_state_hash: None,
            };
            write_transaction(&paths, &transaction).expect("write interrupted transaction");
            if published_components >= 1 {
                atomic_write_private(&paths.provenance, &after_provenance, &paths.directory)
                    .expect("publish interrupted provenance");
            }
            if published_components >= 2 {
                atomic_write_private(&paths.managed, &after_managed, &paths.directory)
                    .expect("publish interrupted managed settings");
            }
            if partial_cleanup {
                remove_file_durable(&managed_stage_path, &paths.directory)
                    .expect("simulate partial transaction cleanup");
                remove_file_durable(&managed_backup_path, &paths.directory)
                    .expect("simulate later partial cleanup boundary");
            }

            recover_tycode_transaction(&paths).expect("recover interrupted transaction");

            let expected = if published_components >= 2 {
                &after
            } else {
                &before
            };
            assert_eq!(
                current_pair_identity(&paths).expect("current pair"),
                Some(expected.clone())
            );
            assert!(!paths.transaction.exists());
            for artifact in transaction_artifacts(&transaction) {
                assert!(!artifact_path(artifact).exists());
            }
        }

        let orphan = paths
            .directory
            .join(".tyde-settings.crash-before-journal.managed-stage.txn");
        write_private_file(&orphan, b"staged").expect("write pre-journal stage");
        recover_tycode_transaction(&paths).expect("clean pre-journal stage");
        assert!(!orphan.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_reset_journal_resumes_precleaning_and_cleaning_crashes() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let shared = b"# reset crash must not touch shared settings\n";
        fs::write(&paths.shared, shared).expect("write shared settings");
        fs::write(&paths.managed, b"corrupt managed settings").expect("corrupt managed settings");
        let unavailable = native_settings_snapshot().await;
        assert_eq!(unavailable.status, BackendConfigSnapshotStatus::Unavailable);
        let recovery = load_recovery_record(&paths)
            .expect("load recovery checkpoint")
            .expect("recovery checkpoint");
        let mut reset = TycodeTransactionRecord {
            transaction_id: "reset-precleaning-crash".to_string(),
            operation: TycodeTransactionOperation::Reset,
            phase: TycodeTransactionPhase::Prepared,
            before: None,
            after: None,
            managed_stage: None,
            provenance_stage: None,
            managed_backup: None,
            provenance_backup: None,
            reset_artifacts: Vec::new(),
            reset_state_hash: Some(recovery.state_hash.clone()),
        };
        write_transaction(&paths, &reset).expect("write prepared reset journal");
        remove_file_durable(&paths.managed, &paths.directory)
            .expect("simulate crash after managed deletion");

        recover_tycode_transaction(&paths).expect("resume prepared reset");

        assert!(!paths.managed.exists());
        assert!(!paths.provenance.exists());
        assert!(!paths.recovery.exists());
        assert!(!paths.transaction.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );

        reset.transaction_id = "reset-cleaning-crash".to_string();
        reset.phase = TycodeTransactionPhase::Cleaning;
        write_transaction(&paths, &reset).expect("write cleaning reset journal");
        recover_tycode_transaction(&paths).expect("resume cleaning reset");
        assert!(!paths.transaction.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_failed_prepared_reset_refreshes_live_recovery_tokens() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let shared = b"# failed reset recovery must not touch shared settings\n";
        fs::write(&paths.shared, shared).expect("write shared settings");
        fs::write(&paths.managed, b"corrupt managed settings").expect("corrupt managed settings");
        let initial_snapshot = native_settings_snapshot().await;
        let (initial_projection_id, initial_state_hash) =
            managed_recovery_tokens(&initial_snapshot);
        let reset_artifact_path = paths
            .directory
            .join(".tyde-settings.prejournal-save-managed-reset-refresh.txn");
        write_private_file(&reset_artifact_path, b"journaled reset artifact")
            .expect("write reset artifact");
        let reset = TycodeTransactionRecord {
            transaction_id: "reset-refresh-after-publication".to_string(),
            operation: TycodeTransactionOperation::Reset,
            phase: TycodeTransactionPhase::Prepared,
            before: None,
            after: None,
            managed_stage: None,
            provenance_stage: None,
            managed_backup: None,
            provenance_backup: None,
            reset_artifacts: vec![
                reset_inventory_artifact(&reset_artifact_path).expect("inventory reset artifact"),
            ],
            reset_state_hash: Some(initial_state_hash.clone()),
        };
        write_transaction(&paths, &reset).expect("publish prepared reset journal");
        atomic_write_private(
            &reset_artifact_path,
            b"changed after reset journal publication",
            &paths.directory,
        )
        .expect("change reset artifact after journal publication");

        let refreshed_snapshot = native_settings_snapshot().await;
        let (refreshed_projection_id, refreshed_state_hash) =
            managed_recovery_tokens(&refreshed_snapshot);
        assert_eq!(refreshed_projection_id, initial_projection_id);
        assert_ne!(refreshed_state_hash, initial_state_hash);
        let refreshed_recovery = load_recovery_record(&paths)
            .expect("load refreshed recovery checkpoint")
            .expect("refreshed recovery checkpoint");
        assert!(
            refreshed_recovery
                .reason
                .contains("Tycode reset artifact integrity check failed")
        );
        assert_eq!(refreshed_recovery.state_hash, refreshed_state_hash);
        assert_eq!(
            inventory_state_hash(&paths).expect("hash exact failed-reset inventory"),
            refreshed_state_hash
        );
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );

        assert!(matches!(
            reset_tycode_managed_projection(&initial_projection_id, &initial_state_hash).await,
            Err(TycodeManagedProjectionResetError::Conflict(_))
        ));
        assert!(reset_artifact_path.exists());
        assert!(paths.managed.exists());
        assert!(paths.provenance.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );

        reset_tycode_managed_projection(&refreshed_projection_id, &refreshed_state_hash)
            .await
            .expect("reset with refreshed exact tokens");
        assert!(!reset_artifact_path.exists());
        assert!(!paths.managed.exists());
        assert!(!paths.provenance.exists());
        assert!(!paths.transaction.exists());
        assert!(!paths.recovery.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
        assert_eq!(
            native_settings_snapshot().await.status,
            BackendConfigSnapshotStatus::Ready
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_filesystem_lock_serializes_handles_and_replaces_stale_owner() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let stale = TycodeLockOwner {
            owner_token: "stale-owner".to_string(),
            pid: std::process::id(),
            process_start_identity: "reused-pid-from-old-process".to_string(),
            created_at_ms: 1,
        };
        write_private_file(
            &paths.lock,
            &serde_json::to_vec(&stale).expect("encode stale lock owner"),
        )
        .expect("write stale lock owner");

        let lock = acquire_tycode_filesystem_lock(&paths)
            .await
            .expect("OS lock recovers stale owner record");
        let owner: TycodeLockOwner =
            serde_json::from_slice(&fs::read(&paths.lock).expect("read current lock owner"))
                .expect("parse current lock owner");
        assert_ne!(owner.owner_token, stale.owner_token);
        assert_eq!(
            owner.process_start_identity.as_str(),
            tycode_process_start_identity()
        );

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&paths.lock)
            .expect("open contender lock handle");
        assert!(FileExt::try_lock_exclusive(&contender).is_err());
        drop(lock);
        FileExt::try_lock_exclusive(&contender).expect("released OS lock is acquirable");
        FileExt::unlock(&contender).expect("unlock contender");

        fs::write(&paths.lock, b"initializing-owner-record")
            .expect("write malformed initializing owner record");
        let recovered = acquire_tycode_filesystem_lock(&paths)
            .await
            .expect("OS evidence permits malformed stale-owner recovery");
        let owner: TycodeLockOwner =
            serde_json::from_slice(&fs::read(&paths.lock).expect("read recovered lock owner"))
                .expect("parse recovered lock owner");
        assert_eq!(owner.owner_token, recovered.owner_token);
        drop(recovered);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn tycode_filesystem_lock_waits_for_another_process() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let ready = dir.path().join("lock-ready");
        let release = dir.path().join("lock-release");
        let locker = dir.path().join("hold_lock.py");
        fs::write(
            &locker,
            r#"import fcntl
import json
import os
import sys
import time

descriptor = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o600)
fcntl.flock(descriptor, fcntl.LOCK_EX)
owner = json.dumps({
    "owner_token": "other-process",
    "pid": os.getpid(),
    "process_start_identity": "other-process-start",
    "created_at_ms": 1,
}).encode()
os.ftruncate(descriptor, 0)
os.write(descriptor, owner)
os.fsync(descriptor)
open(sys.argv[2], "w").close()
while not os.path.exists(sys.argv[3]):
    time.sleep(0.01)
fcntl.flock(descriptor, fcntl.LOCK_UN)
"#,
        )
        .expect("write lock-holder process");
        let mut child = Command::new("python3")
            .arg(&locker)
            .arg(&paths.lock)
            .arg(&ready)
            .arg(&release)
            .spawn()
            .expect("spawn lock-holder process");
        for _ in 0..300 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            ready.exists(),
            "other process did not acquire the filesystem lock"
        );

        let acquisition_paths = paths.clone();
        let acquisition =
            tokio::spawn(async move { acquire_tycode_filesystem_lock(&acquisition_paths).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !acquisition.is_finished(),
            "live OS lock must not be stolen"
        );
        fs::write(&release, b"release").expect("release lock-holder process");
        let lock = tokio::time::timeout(Duration::from_secs(5), acquisition)
            .await
            .expect("filesystem lock acquisition timed out")
            .expect("filesystem lock acquisition task panicked")
            .expect("acquire filesystem lock after process exit");
        child.wait().await.expect("wait for lock-holder process");
        drop(lock);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_projection_hash_mismatch_requires_exact_reset_before_rederivation() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": null,
            "providers": {}
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let paths = tycode_projection_paths().expect("test projection paths");
        let shared = b"# shared settings remain byte-identical\n";
        fs::write(&paths.shared, shared).expect("write shared settings");
        fs::write(&paths.managed, b"tampered").expect("tamper managed settings");

        let snapshot = native_settings_snapshot().await;

        assert_eq!(snapshot.status, BackendConfigSnapshotStatus::Unavailable);
        assert!(
            snapshot
                .message
                .as_deref()
                .is_some_and(|message| message.contains("integrity check failed"))
        );
        assert!(!log.exists(), "integrity failure must occur before spawn");
        let Some(TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
            expected_projection_id,
            expected_state_hash,
            ..
        }) = snapshot.managed_projection_recovery
        else {
            panic!("tamper must surface typed managed projection recovery");
        };
        let tampered = fs::read(&paths.managed).expect("read tampered projection");
        assert!(matches!(
            reset_tycode_managed_projection(
                &TycodeProjectionId("stale-projection".to_string()),
                &expected_state_hash
            )
            .await,
            Err(TycodeManagedProjectionResetError::Conflict(_))
        ));
        assert_eq!(
            fs::read(&paths.managed).expect("stale reset preserves state"),
            tampered
        );
        assert!(matches!(
            reset_tycode_managed_projection(
                &expected_projection_id,
                &TycodeProjectionStateHash("sha256:stale-state".to_string())
            )
            .await,
            Err(TycodeManagedProjectionResetError::Conflict(_))
        ));
        assert_eq!(
            fs::read(&paths.managed).expect("stale hash preserves state"),
            tampered
        );

        let changed = b"tampered after reset offer";
        fs::write(&paths.managed, changed).expect("change state after reset offer");
        assert!(matches!(
            reset_tycode_managed_projection(&expected_projection_id, &expected_state_hash).await,
            Err(TycodeManagedProjectionResetError::Conflict(_))
        ));
        assert_eq!(
            fs::read(&paths.managed).expect("changed state survives stale reset"),
            changed
        );
        let refreshed_recovery = native_settings_snapshot().await;
        let Some(TycodeManagedProjectionRecoveryState::ManagedProjectionResetRequired {
            expected_projection_id,
            expected_state_hash,
            ..
        }) = refreshed_recovery.managed_projection_recovery
        else {
            panic!("refresh must issue tokens for the exact current inventory");
        };
        reset_tycode_managed_projection(&expected_projection_id, &expected_state_hash)
            .await
            .expect("exact recovery tokens reset only managed state");
        assert!(!paths.managed.exists());
        assert!(!paths.provenance.exists());
        assert!(!paths.transaction.exists());
        assert!(!paths.recovery.exists());
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );

        let refreshed = native_settings_snapshot().await;
        assert_eq!(refreshed.status, BackendConfigSnapshotStatus::Ready);
        assert_eq!(
            fs::read(&paths.shared).expect("read shared settings"),
            shared
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_native_settings_save_accepts_canonicalized_post_save_snapshot() {
        let dir = TempDir::new().expect("tempdir");
        let initial_settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "modules": {
                "memory": { "enabled": true }
            },
            "unrelated": { "keep": true }
        });
        let saved_settings = serde_json::json!({
            "active_provider": "other",
            "providers": initial_settings["providers"].clone(),
            "model_quality": "low",
            "reasoning_effort": "Max",
            "modules": {
                "memory": { "enabled": true }
            },
            "unrelated": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess_canonicalizing_native_settings(
            dir.path(),
            &initial_settings,
        );
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        persist_native_settings(saved_settings.clone())
            .await
            .expect("persist native Tycode settings");
        let refreshed = native_settings_snapshot().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        let save = commands[0]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"], saved_settings);
        assert_eq!(commands[1], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(
            refreshed
                .settings
                .as_ref()
                .and_then(|settings| settings.get("profile")),
            Some(&Value::String("default".to_string()))
        );
        let paths = tycode_projection_paths().expect("test projection paths");
        assert!(
            !fs::read_to_string(paths.managed)
                .expect("read canonical managed TOML")
                .contains("\"profile\"")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_empty_without_previous_is_noop() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let incoming = BackendConfigValues::default();
        let previous = BackendConfigValues::default();
        let values = tycode_backend_config_persistence_values(&incoming, &previous);
        assert!(values.0.is_empty());

        persist_backend_config(values)
            .await
            .expect("persist empty Tycode backend config");
        assert!(
            !log.exists(),
            "empty config with no previous Tyde-managed keys should not spawn Tycode"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_snapshot_reads_current_settings_without_save() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let values = backend_config_snapshot()
            .await
            .expect("read Tycode backend config snapshot");

        assert_eq!(
            values.0.get("active_provider"),
            Some(&SessionSettingValue::String("other".to_string()))
        );
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::String("high".to_string()))
        );
        assert_eq!(
            values.0.get("reasoning_effort"),
            Some(&SessionSettingValue::String("Max".to_string()))
        );
        assert_eq!(
            read_fake_commands(&log),
            vec![Value::String("GetSettingsSchema".to_string())]
        );
    }

    #[test]
    fn tycode_backend_config_persistent_save_omitted_previous_key_is_preserved() {
        let mut incoming = BackendConfigValues::default();
        incoming.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("low".to_string()),
        );
        let mut previous = BackendConfigValues::default();
        previous.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let values = tycode_backend_config_persistence_values(&incoming, &previous);

        assert_eq!(values.0.len(), 1);
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::String("low".to_string()))
        );
        assert!(
            !values.0.contains_key("active_provider"),
            "omitted Tycode keys are preserved by generic settings merge, not reset during persistence"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_null_resets_only_that_key() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh",
            "unmanaged_top_level": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut values = BackendConfigValues::default();
        values
            .0
            .insert("review_level".to_string(), SessionSettingValue::Null);

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config with explicit null");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], "high");
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "None");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
        assert_eq!(
            save["settings"]["unmanaged_top_level"],
            serde_json::json!({ "keep": true })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tycode_backend_config_persistent_save_empty_update_resets_previous_keys() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "other",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "openrouter", "api_key": "secret" }
            },
            "model_quality": "high",
            "reasoning_effort": "Max",
            "autonomy_level": "fully_autonomous",
            "review_level": "Task",
            "spawn_context_mode": "Fresh",
            "unmanaged_top_level": { "keep": true }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let incoming = BackendConfigValues::default();
        let mut previous = BackendConfigValues::default();
        previous.0.insert(
            "model_quality".to_string(),
            SessionSettingValue::String("high".to_string()),
        );
        let values = tycode_backend_config_persistence_values(&incoming, &previous);
        assert_eq!(values.0.len(), 1);
        assert_eq!(
            values.0.get("model_quality"),
            Some(&SessionSettingValue::Null)
        );

        persist_backend_config(values)
            .await
            .expect("persist Tycode backend config with removed key");

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[2], Value::String("GetSettingsSchema".to_string()));
        assert_eq!(commands[3], Value::String("GetSettingsSchema".to_string()));

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert_eq!(save["persist"], true);
        assert_eq!(save["settings"]["active_provider"], "other");
        assert_eq!(save["settings"]["model_quality"], Value::Null);
        assert_eq!(save["settings"]["reasoning_effort"], "Max");
        assert_eq!(save["settings"]["autonomy_level"], "fully_autonomous");
        assert_eq!(save["settings"]["review_level"], "Task");
        assert_eq!(save["settings"]["spawn_context_mode"], "Fresh");
        assert_eq!(save["settings"]["providers"], settings["providers"]);
        assert_eq!(
            save["settings"]["unmanaged_top_level"],
            serde_json::json!({ "keep": true })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_sends_change_provider_for_explicit_same_provider() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let (backend, mut events) =
            TycodeBackend::spawn(Vec::new(), config, payload("hello Tycode"))
                .await
                .expect("spawn fake Tycode");
        wait_for_fake_done(&mut events).await;
        backend.shutdown().await;

        let commands = read_fake_commands(&log);
        assert_eq!(commands.len(), 5, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "default" })
        );
        assert_eq!(
            commands[4],
            serde_json::json!({ "UserInput": "hello Tycode" })
        );

        let save = commands[1]
            .get("SaveSettings")
            .expect("SaveSettings command");
        assert!(save["settings"].get("default_agent").is_none());
        assert!(
            save["settings"]
                .get("orchestration_progress_messages")
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_fails_before_prompt_for_invalid_provider() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" }
            }
        });
        let fake = write_fake_tycode_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set(fake);

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("missing".to_string()),
        );

        let err = match TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await {
            Ok(_) => panic!("invalid provider should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("active_provider 'missing' is absent"));

        let commands = read_fake_commands(&log);
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_times_out_waiting_for_settings() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_settings_stall_subprocess(dir.path());
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            None,
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let spawn_handle = tokio::spawn(async move {
            TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await
        });

        let commands = read_fake_commands_eventually(&log, 1).await;
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);

        let err = match spawn_handle.await.expect("fake Tycode spawn task panicked") {
            Ok(_) => panic!("settings stall should fail startup"),
            Err(err) => err,
        };
        assert!(
            err.contains("Timed out after 2s"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("Tycode spawn startup/settings handshake"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("waiting for Settings after GetSettings"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_resume_times_out_waiting_for_settings() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_settings_stall_subprocess(dir.path());
        let log = dir.path().join("commands.jsonl");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create fake sessions dir");
        std::fs::write(
            sessions_dir.join("resume-session.json"),
            serde_json::json!({
                "id": "resume-session",
                "events": []
            })
            .to_string(),
        )
        .expect("write fake Tycode resume session");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            Some(sessions_dir),
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("default".to_string()),
        );

        let resume_handle = tokio::spawn(async move {
            TycodeBackend::resume(Vec::new(), config, SessionId("resume-session".to_string())).await
        });

        let commands = read_fake_commands_eventually(&log, 1).await;
        assert_eq!(commands, vec![Value::String("GetSettings".to_string())]);

        let err = match resume_handle
            .await
            .expect("fake Tycode resume task panicked")
        {
            Ok(_) => panic!("settings stall should fail resume startup"),
            Err(err) => err,
        };
        assert!(err.contains("Timed out after 2s"));
        assert!(err.contains("Tycode resume startup/settings handshake"));
        assert!(err.contains("waiting for Settings after GetSettings"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_tycode_spawn_times_out_waiting_for_change_provider_ack() {
        let dir = TempDir::new().expect("tempdir");
        let settings = serde_json::json!({
            "active_provider": "default",
            "providers": {
                "default": { "type": "mock" },
                "other": { "type": "mock" }
            },
            "model_quality": null,
            "reasoning_effort": null,
            "autonomy_level": "plan_approval_required",
            "review_level": "None",
            "spawn_context_mode": "Fork"
        });
        let fake = write_fake_tycode_provider_ack_stall_subprocess(dir.path(), &settings);
        let log = dir.path().join("commands.jsonl");
        let _guard = TestTycodeSubprocessGuard::set_with_options(
            fake,
            None,
            Some(TEST_TYCODE_STARTUP_TIMEOUT_DURATION),
        );

        let mut config = BackendSpawnConfig::default();
        config.backend_config.0.insert(
            "active_provider".to_string(),
            SessionSettingValue::String("other".to_string()),
        );

        let spawn_handle = tokio::spawn(async move {
            TycodeBackend::spawn(Vec::new(), config, payload("must not send")).await
        });

        let commands = read_fake_commands_eventually(&log, 4).await;
        assert_eq!(commands.len(), 4, "commands: {commands:#?}");
        assert_eq!(commands[0], Value::String("GetSettings".to_string()));
        assert_eq!(commands[2], Value::String("GetSettings".to_string()));
        assert_eq!(
            commands[3],
            serde_json::json!({ "ChangeProvider": "other" })
        );

        let err = match spawn_handle.await.expect("fake Tycode spawn task panicked") {
            Ok(_) => panic!("provider ack stall should fail startup"),
            Err(err) => err,
        };
        assert!(err.contains("Timed out after 2s"));
        assert!(err.contains("waiting for ChangeProvider acknowledgement"));
    }

    #[test]
    fn build_tycode_mcp_servers_json_supports_http_servers() {
        let json = build_tycode_mcp_servers_json(&[StartupMcpServer {
            name: "tyde-debug".to_string(),
            transport: StartupMcpTransport::Http {
                url: "http://127.0.0.1:4123/mcp".to_string(),
                headers: HashMap::from([(
                    "x-tyde-debug-repo-root".to_string(),
                    "/tmp/project".to_string(),
                )]),
                bearer_token_env_var: None,
            },
        }])
        .expect("HTTP MCP config should serialize");
        let value: Value = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(
            value["tyde-debug"]["url"],
            Value::String("http://127.0.0.1:4123/mcp".to_string())
        );
        assert_eq!(
            value["tyde-debug"]["headers"]["x-tyde-debug-repo-root"],
            Value::String("/tmp/project".to_string())
        );
    }

    struct TestTycodeSubprocessGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_bin: Option<String>,
        previous_home_dir: Option<PathBuf>,
        previous_sessions_dir: Option<PathBuf>,
        previous_timeout: Option<Duration>,
        previous_set_root_agent_supported: Option<bool>,
    }

    static TEST_TYCODE_SUBPROCESS_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_test_managed_projection(home: &Path) {
        fs::create_dir_all(home).expect("create test home");
        let directory = home.join(".tycode");
        ensure_private_tycode_directory(&directory).expect("create test Tycode directory");
        let managed = directory.join("tyde-settings.toml");
        let shared = directory.join("settings.toml");
        let provenance_path = directory.join("tyde-settings.provenance.json");
        if !managed.exists() {
            write_private_file(&managed, b"").expect("write test managed settings");
        }
        let managed_bytes = fs::read(&managed).expect("read test managed settings");
        let provenance = BackendNativeSettingsProvenance::TycodeManagedProjection {
            managed_settings_path: projection_path_value(&managed),
            source_settings_path: projection_path_value(&shared),
            source: TycodeProjectionSource::Defaults,
            tycode_version: exact_tycode_version().expect("test pinned Tycode version"),
            projection_id: TycodeProjectionId("test-projection".to_string()),
            created_at_ms: 1,
            source_digest: tycode_digest(b""),
            original_unchanged: true,
            notice_pending: false,
        };
        let record = TycodeProjectionRecord {
            provenance,
            managed_digest: tycode_digest(&managed_bytes),
        };
        let bytes = serde_json::to_vec_pretty(&record).expect("encode test provenance");
        if provenance_path.exists() {
            fs::remove_file(&provenance_path).expect("replace test provenance");
        }
        write_private_file(&provenance_path, &bytes).expect("write test provenance");
    }

    struct TestTycodeRootAgentSupportGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_set_root_agent_supported: Option<bool>,
    }

    impl TestTycodeRootAgentSupportGuard {
        fn set(supported: bool) -> Self {
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let mut configured_set_root_agent_supported = TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned");
            let previous_set_root_agent_supported =
                configured_set_root_agent_supported.replace(supported);
            drop(configured_set_root_agent_supported);
            Self {
                _lock: lock,
                previous_set_root_agent_supported,
            }
        }
    }

    impl Drop for TestTycodeRootAgentSupportGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned") =
                self.previous_set_root_agent_supported.take();
        }
    }

    impl TestTycodeSubprocessGuard {
        fn set(path: String) -> Self {
            Self::set_with_options(path, None, None)
        }

        fn set_with_root_agent_support(path: String) -> Self {
            Self::set_with_options_inner(path, None, None, Some(true))
        }

        fn set_without_root_agent_support(path: String) -> Self {
            Self::set_with_options_inner(path, None, None, Some(false))
        }

        fn set_with_options(
            path: String,
            sessions_dir: Option<PathBuf>,
            startup_timeout: Option<Duration>,
        ) -> Self {
            Self::set_with_options_inner(path, sessions_dir, startup_timeout, None)
        }

        fn set_with_options_inner(
            path: String,
            sessions_dir: Option<PathBuf>,
            startup_timeout: Option<Duration>,
            set_root_agent_supported: Option<bool>,
        ) -> Self {
            let _ = crate::process_env::resolved_child_process_path();
            let lock = TEST_TYCODE_SUBPROCESS_MUTEX
                .lock()
                .expect("test Tycode subprocess mutex poisoned");
            let test_home = PathBuf::from(&path)
                .parent()
                .expect("fake Tycode subprocess parent")
                .join("tycode-home");
            let mut configured = TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned");
            let previous_bin = configured.replace(path);
            drop(configured);
            let mut configured_home = TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned");
            let previous_home_dir = configured_home.replace(test_home.clone());
            drop(configured_home);
            write_test_managed_projection(&test_home);
            let mut configured_sessions_dir = TEST_TYCODE_SESSIONS_DIR
                .lock()
                .expect("test Tycode sessions dir mutex poisoned");
            let previous_sessions_dir =
                std::mem::replace(&mut *configured_sessions_dir, sessions_dir);
            drop(configured_sessions_dir);
            let mut configured_timeout = TEST_TYCODE_STARTUP_TIMEOUT
                .lock()
                .expect("test Tycode startup timeout mutex poisoned");
            let previous_timeout = std::mem::replace(&mut *configured_timeout, startup_timeout);
            drop(configured_timeout);
            let mut configured_set_root_agent_supported = TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned");
            let previous_set_root_agent_supported = std::mem::replace(
                &mut *configured_set_root_agent_supported,
                set_root_agent_supported,
            );
            drop(configured_set_root_agent_supported);
            Self {
                _lock: lock,
                previous_bin,
                previous_home_dir,
                previous_sessions_dir,
                previous_timeout,
                previous_set_root_agent_supported,
            }
        }
    }

    impl Drop for TestTycodeSubprocessGuard {
        fn drop(&mut self) {
            *TEST_TYCODE_SUBPROCESS_BIN
                .lock()
                .expect("test Tycode subprocess bin mutex poisoned") = self.previous_bin.take();
            *TEST_TYCODE_HOME_DIR
                .lock()
                .expect("test Tycode home dir mutex poisoned") = self.previous_home_dir.take();
            *TEST_TYCODE_SESSIONS_DIR
                .lock()
                .expect("test Tycode sessions dir mutex poisoned") =
                self.previous_sessions_dir.take();
            *TEST_TYCODE_STARTUP_TIMEOUT
                .lock()
                .expect("test Tycode startup timeout mutex poisoned") =
                self.previous_timeout.take();
            *TEST_TYCODE_SET_ROOT_AGENT_SUPPORTED
                .lock()
                .expect("test Tycode SetRootAgent support mutex poisoned") =
                self.previous_set_root_agent_supported.take();
        }
    }

    fn write_fake_tycode_subprocess(dir: &Path, settings: &Value) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, false, false)
    }

    fn write_fake_tycode_subprocess_dropping_unknown_settings(
        dir: &Path,
        settings: &Value,
    ) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, true, false, false)
    }

    fn write_fake_tycode_subprocess_rejecting_root_agent(dir: &Path, settings: &Value) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, true, false)
    }

    fn write_fake_tycode_subprocess_canonicalizing_native_settings(
        dir: &Path,
        settings: &Value,
    ) -> String {
        write_fake_tycode_subprocess_with_options(dir, settings, false, false, true)
    }

    fn write_fake_tycode_subprocess_without_tomllib(dir: &Path, settings: &Value) -> String {
        let script = write_fake_tycode_subprocess(dir, settings);
        let body = fs::read_to_string(&script).expect("read fake Tycode script");
        let insertion_point = "\ndefault_settings = json.loads(";
        assert!(body.contains(insertion_point));
        let body = body.replacen(
            insertion_point,
            "\ntomllib = None\n\ndefault_settings = json.loads(",
            1,
        );
        fs::write(&script, body).expect("force portable fake Tycode TOML parser");
        script
    }

    fn write_fake_tycode_subprocess_with_pre_session_advisory(
        dir: &Path,
        settings: &Value,
    ) -> String {
        let script = write_fake_tycode_subprocess(dir, settings);
        let body = fs::read_to_string(&script).expect("read fake Tycode script");
        let body = body.replacen(
            "emit({\"kind\": \"SessionStarted\", \"data\": {\"session_id\": \"fake-session\"}})",
            "emit(message(\"Error\", \"No AI provider is configured. Configure one now.\"))\nemit({\"kind\": \"SessionStarted\", \"data\": {\"session_id\": \"fake-session\"}})",
            1,
        );
        fs::write(&script, body).expect("rewrite fake Tycode advisory script");
        script
    }

    fn write_fake_tycode_subprocess_with_post_command_error(
        dir: &Path,
        settings: &Value,
    ) -> String {
        let script = write_fake_tycode_subprocess(dir, settings);
        let body = fs::read_to_string(&script).expect("read fake Tycode script");
        let body = body.replacen(
            "emit({\"kind\": \"SettingsSchema\", \"data\": {\"schema\": {\"settings\": settings, \"groups\": settings_groups}}})",
            "emit(message(\"Error\", \"schema command failed\"))",
            1,
        );
        fs::write(&script, body).expect("rewrite fake Tycode error script");
        script
    }

    fn write_fake_tycode_subprocess_with_options(
        dir: &Path,
        settings: &Value,
        drop_unknown_settings: bool,
        reject_root_agent: bool,
        canonicalize_native_settings: bool,
    ) -> String {
        let script = dir.join("fake_tycode_subprocess.py");
        let log = dir.join("commands.jsonl");
        let settings_literal =
            serde_json::to_string(&settings.to_string()).expect("settings literal");
        let mut default_settings = settings.clone();
        let default_settings_object = default_settings
            .as_object_mut()
            .expect("fake Tycode settings object");
        for key in TYCODE_MANAGED_SETTINGS {
            if default_settings_object.contains_key(*key) {
                default_settings_object
                    .insert((*key).to_string(), tycode_managed_setting_default(key));
            }
        }
        let default_settings_literal =
            serde_json::to_string(&default_settings.to_string()).expect("default settings literal");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let drop_unknown_literal = if drop_unknown_settings {
            "True"
        } else {
            "False"
        };
        let reject_root_agent_literal = if reject_root_agent { "True" } else { "False" };
        let canonicalize_native_settings_literal = if canonicalize_native_settings {
            "True"
        } else {
            "False"
        };
        let body = r####"#!/usr/bin/env python3
import copy
import json
import sys

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

def split_toml_top_level(value, delimiter):
    parts = []
    start = 0
    quote = None
    escaped = False
    square_depth = 0
    curly_depth = 0
    for index, character in enumerate(value):
        if quote is not None:
            if quote == '"' and escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in {'"', "'"}:
            quote = character
        elif character == "[":
            square_depth += 1
        elif character == "]":
            square_depth -= 1
        elif character == "{":
            curly_depth += 1
        elif character == "}":
            curly_depth -= 1
        elif character == delimiter and square_depth == 0 and curly_depth == 0:
            parts.append(value[start:index].strip())
            start = index + 1
    parts.append(value[start:].strip())
    return parts

def strip_toml_comment(line):
    quote = None
    escaped = False
    for index, character in enumerate(line):
        if quote is not None:
            if quote == '"' and escaped:
                escaped = False
            elif quote == '"' and character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in {'"', "'"}:
            quote = character
        elif character == "#":
            return line[:index]
    return line

def parse_toml_key(value):
    value = value.strip()
    if value.startswith('"'):
        return json.loads(value)
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    return value

def parse_toml_key_path(value):
    return [parse_toml_key(part) for part in split_toml_top_level(value, ".")]

def split_toml_assignment(line):
    parts = split_toml_top_level(line, "=")
    if len(parts) != 2:
        raise ValueError(f"unsupported TOML assignment: {line}")
    return parts

def assign_toml_path(target, path, value):
    current = target
    for part in path[:-1]:
        existing = current.setdefault(part, {})
        if not isinstance(existing, dict):
            raise ValueError(f"TOML key path collides at {part}")
        current = existing
    current[path[-1]] = value

def parse_toml_value(value):
    value = value.strip()
    if value.startswith('"'):
        return json.loads(value)
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1]
    if value == "true":
        return True
    if value == "false":
        return False
    if value.startswith("[") and value.endswith("]"):
        contents = value[1:-1].strip()
        if not contents:
            return []
        return [
            parse_toml_value(item)
            for item in split_toml_top_level(contents, ",")
            if item
        ]
    if value.startswith("{") and value.endswith("}"):
        contents = value[1:-1].strip()
        result = {}
        if not contents:
            return result
        for entry in split_toml_top_level(contents, ","):
            key, item = split_toml_assignment(entry)
            assign_toml_path(result, parse_toml_key_path(key), parse_toml_value(item))
        return result
    number = value.replace("_", "")
    try:
        return json.loads(number)
    except json.JSONDecodeError:
        try:
            return int(number, 0)
        except ValueError as error:
            raise ValueError(f"unsupported TOML value: {value}") from error

def parse_toml(contents):
    result = {}
    current = result
    for raw_line in contents.splitlines():
        line = strip_toml_comment(raw_line).strip()
        if not line:
            continue
        if line.startswith("[") and line.endswith("]"):
            if line.startswith("[["):
                raise ValueError("TOML array tables are not used by the Tycode test fixture")
            current = result
            for part in parse_toml_key_path(line[1:-1]):
                nested = current.setdefault(part, {})
                if not isinstance(nested, dict):
                    raise ValueError(f"TOML table path collides at {part}")
                current = nested
            continue
        key, value = split_toml_assignment(line)
        assign_toml_path(current, parse_toml_key_path(key), parse_toml_value(value))
    return result

def load_toml(settings_file):
    if tomllib is not None:
        return tomllib.load(settings_file)
    return parse_toml(settings_file.read().decode("utf-8"))

initial_settings = json.loads(__SETTINGS__)
default_settings = json.loads(__DEFAULT_SETTINGS__)
known_settings_keys = set(default_settings.keys())
settings_path = None
for index, argument in enumerate(sys.argv):
    if argument == "--settings-path" and index + 1 < len(sys.argv):
        settings_path = sys.argv[index + 1]
        break
drop_unknown_settings = __DROP_UNKNOWN_SETTINGS__
reject_root_agent = __REJECT_ROOT_AGENT__
canonicalize_native_settings = __CANONICALIZE_NATIVE_SETTINGS__
log_path = __LOG__

def merge_defaults(base, loaded):
    merged = copy.deepcopy(base)
    for key, value in loaded.items():
        merged[key] = copy.deepcopy(value)
    return merged

def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        entries = [
            f"{json.dumps(key)} = {toml_value(item)}"
            for key, item in value.items()
            if item is not None
        ]
        return "{ " + ", ".join(entries) + " }"
    raise TypeError(f"unsupported TOML value: {type(value)}")

def write_toml_table(lines, prefix, table):
    if prefix:
        lines.append("[" + ".".join(json.dumps(part) for part in prefix) + "]")
    for key, value in table.items():
        if value is not None and not isinstance(value, dict):
            lines.append(f"{json.dumps(key)} = {toml_value(value)}")
    for key, value in table.items():
        if isinstance(value, dict):
            if lines and lines[-1] != "":
                lines.append("")
            write_toml_table(lines, prefix + [key], value)

def persist_toml(value):
    lines = []
    write_toml_table(lines, [], value)
    with open(settings_path, "w", encoding="utf-8") as settings_file:
        settings_file.write("\n".join(lines).rstrip() + "\n")

def is_empty_for_persistence(value):
    comparable = copy.deepcopy(value)
    default_agent = comparable.get("default_agent")
    if isinstance(default_agent, str) and not default_agent.strip():
        comparable["default_agent"] = default_settings.get("default_agent", "tycode")
    return comparable == default_settings

settings = copy.deepcopy(initial_settings)
if settings_path is not None:
    try:
        with open(settings_path, "rb") as settings_file:
            loaded_settings = load_toml(settings_file)
            if loaded_settings:
                settings = merge_defaults(default_settings, loaded_settings)
    except FileNotFoundError:
        settings = copy.deepcopy(default_settings)
        persist_toml(default_settings)
if drop_unknown_settings:
    settings = {key: value for key, value in settings.items() if key in known_settings_keys}
settings = dict(settings)
settings["profile"] = "default"

settings_groups = [
    {
        "id": "general",
        "title": "General",
        "kind": "core",
        "settings_path": [],
        "description": "General settings",
        "schema": {
            "type": "object",
            "properties": {
                "model_quality": {"type": ["string", "null"]},
                "reasoning_effort": {"type": ["string", "null"]},
            },
        },
    },
    {
        "id": "providers",
        "title": "Providers",
        "kind": "core",
        "settings_path": [],
        "description": "Provider settings",
        "schema": {
            "type": "object",
            "properties": {
                "active_provider": {"type": ["string", "null"]},
                "providers": {"type": "object"},
            },
        },
    },
    {
        "id": "module:memory",
        "title": "Memory",
        "kind": "module",
        "settings_path": ["modules", "memory"],
        "description": "Memory module settings",
        "schema": {
            "type": "object",
            "properties": {"enabled": {"type": "boolean"}},
        },
    },
]

def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

def message(sender, content):
    return {
        "kind": "MessageAdded",
        "data": {
            "timestamp": 1,
            "sender": sender,
            "content": content,
            "reasoning": None,
            "tool_calls": [],
            "model_info": None,
            "token_usage": None,
            "context_breakdown": None,
            "images": [],
        },
    }

emit({"kind": "SessionStarted", "data": {"session_id": "fake-session"}})

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(line + "\n")
    command = json.loads(line)
    if command == "GetSettings":
        emit({"kind": "Settings", "data": settings})
    elif command == "GetSettingsSchema":
        emit({"kind": "SettingsSchema", "data": {"schema": {"settings": settings, "groups": settings_groups}}})
    elif isinstance(command, dict) and "SaveSettings" in command:
        incoming_settings = command["SaveSettings"]["settings"]
        if drop_unknown_settings:
            modeled_settings = {
                key: value
                for key, value in incoming_settings.items()
                if key in known_settings_keys
            }
        else:
            modeled_settings = incoming_settings
        settings = merge_defaults(default_settings, modeled_settings)
        settings = dict(settings)
        settings["profile"] = "default"
        if command["SaveSettings"].get("persist") and settings_path is not None:
            persisted = dict(settings)
            persisted.pop("profile", None)
            if is_empty_for_persistence(persisted):
                emit({"kind": "Error", "data": "Refusing to persist empty settings"})
                continue
            persist_toml(persisted)
    elif isinstance(command, dict) and "ChangeProvider" in command:
        emit(message("System", f"Switched to provider: {command['ChangeProvider']}"))
    elif isinstance(command, dict) and "SetRootAgent" in command:
        agent = command["SetRootAgent"]["agent"]
        valid_agents = {"one_shot", "tycode", "builder", "swarm"}
        if reject_root_agent or agent not in valid_agents:
            emit({
                "kind": "Error",
                "data": (
                    f"Unknown agent type '{agent}'. Available agents: "
                    "one_shot, tycode, builder, swarm"
                ),
            })
        else:
            emit({"kind": "RootAgentChanged", "data": {"agent": agent}})
    elif isinstance(command, dict) and "UserInput" in command:
        emit(message({"Assistant": {"agent": "tycode"}}, "fake done"))
"####
        .replace("__SETTINGS__", &settings_literal)
        .replace("__DEFAULT_SETTINGS__", &default_settings_literal)
        .replace("__DROP_UNKNOWN_SETTINGS__", drop_unknown_literal)
        .replace("__REJECT_ROOT_AGENT__", reject_root_agent_literal)
        .replace(
            "__CANONICALIZE_NATIVE_SETTINGS__",
            canonicalize_native_settings_literal,
        )
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script)
                .expect("stat fake Tycode script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).expect("chmod fake Tycode script");
        }
        script.to_string_lossy().to_string()
    }

    fn write_fake_tycode_settings_stall_subprocess(dir: &Path) -> String {
        let script = dir.join("fake_tycode_settings_stall.sh");
        let log = dir.join("commands.jsonl");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let body = r#"#!/bin/sh
LOG_PATH=__LOG__
printf '%s\n' '{"kind":"SessionStarted","data":{"session_id":"fake-session"}}'
while IFS= read -r line; do
  [ -n "$line" ] || continue
  printf '%s\n' "$line" >> "$LOG_PATH"
done
"#
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    fn write_fake_tycode_startup_stall_subprocess(dir: &Path) -> String {
        let script = dir.join("fake_tycode_startup_stall.sh");
        let body = r#"#!/bin/sh
while IFS= read -r line; do
  :
done
"#;
        std::fs::write(&script, body).expect("write fake Tycode startup stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    fn process_is_running(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_tycode_startup_terminates_and_reaps_stalled_child() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_startup_stall_subprocess(dir.path());
        let _guard = TestTycodeSubprocessGuard::set(fake);
        let (mut spawned_rx, reaped_rx) = install_tycode_startup_process_observer();
        let mut startup = Box::pin(TycodeBackend::spawn(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            protocol::SendMessagePayload {
                message: "must not survive startup cancellation".to_owned(),
                images: None,
                origin: None,
                tool_response: None,
            },
        ));
        let pid = tokio::select! {
            biased;
            pid = &mut spawned_rx => {
                pid.expect("Tycode startup process observer must retain PID sender")
            }
            _ = startup.as_mut() => {
                panic!("Tycode startup completed before stall cancellation")
            }
        }
        .to_string();
        assert!(process_is_running(&pid), "fixture child must be running");

        drop(startup);

        reaped_rx
            .await
            .expect("Tycode startup worker must report reaping its cancelled child");
        assert!(
            !process_is_running(&pid),
            "closing the startup shutdown channel must terminate and reap the Tycode child"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_tycode_resume_startup_terminates_and_reaps_stalled_child() {
        let dir = TempDir::new().expect("tempdir");
        let fake = write_fake_tycode_startup_stall_subprocess(dir.path());
        let sessions_dir = dir.path().join("sessions");
        fs::create_dir_all(&sessions_dir).expect("create fake sessions dir");
        fs::write(
            sessions_dir.join("resume-cancelled-session.json"),
            serde_json::json!({
                "id": "resume-cancelled-session",
                "events": []
            })
            .to_string(),
        )
        .expect("write fake Tycode resume session");
        let _guard = TestTycodeSubprocessGuard::set_with_options(fake, Some(sessions_dir), None);
        let (mut spawned_rx, reaped_rx) = install_tycode_startup_process_observer();
        let mut startup = Box::pin(TycodeBackend::resume(
            vec![dir.path().to_string_lossy().to_string()],
            BackendSpawnConfig::default(),
            SessionId("resume-cancelled-session".to_owned()),
        ));
        let pid = tokio::select! {
            biased;
            pid = &mut spawned_rx => {
                pid.expect("Tycode resume process observer must retain PID sender")
            }
            _ = startup.as_mut() => {
                panic!("Tycode resume completed before stall cancellation")
            }
        }
        .to_string();
        assert!(
            process_is_running(&pid),
            "fixture resume child must be running"
        );

        drop(startup);

        reaped_rx
            .await
            .expect("Tycode resume worker must report reaping its cancelled child");
        assert!(
            !process_is_running(&pid),
            "closing the resume startup shutdown channel must terminate and reap the Tycode child"
        );
    }

    fn write_fake_tycode_provider_ack_stall_subprocess(dir: &Path, settings: &Value) -> String {
        let script = dir.join("fake_tycode_provider_ack_stall.py");
        let log = dir.join("commands.jsonl");
        let settings_literal =
            serde_json::to_string(&settings.to_string()).expect("settings literal");
        let log_literal = serde_json::to_string(&log.to_string_lossy()).expect("log literal");
        let body = r#"#!/usr/bin/env python3
import json
import sys

settings = json.loads(__SETTINGS__)
log_path = __LOG__

def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)

emit({"kind": "SessionStarted", "data": {"session_id": "fake-session"}})

for raw_line in sys.stdin:
    line = raw_line.strip()
    if not line:
        continue
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(line + "\n")
    command = json.loads(line)
    if command == "GetSettings":
        emit({"kind": "Settings", "data": settings})
    elif isinstance(command, dict) and "SaveSettings" in command:
        settings = command["SaveSettings"]["settings"]
"#
        .replace("__SETTINGS__", &settings_literal)
        .replace("__LOG__", &log_literal);
        std::fs::write(&script, body).expect("write fake Tycode provider ack stall script");
        make_executable(&script);
        script.to_string_lossy().to_string()
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path)
                .expect("stat fake Tycode script")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod fake Tycode script");
        }
    }

    fn read_fake_commands(log: &Path) -> Vec<Value> {
        let body = std::fs::read_to_string(log).expect("read fake Tycode command log");
        body.lines()
            .map(|line| serde_json::from_str(line).expect("parse fake Tycode command"))
            .collect()
    }

    async fn read_fake_commands_eventually(log: &Path, minimum_len: usize) -> Vec<Value> {
        for _ in 0..300 {
            if let Ok(body) = std::fs::read_to_string(log) {
                let commands = body
                    .lines()
                    .map(|line| serde_json::from_str(line).expect("parse fake Tycode command"))
                    .collect::<Vec<_>>();
                if commands.len() >= minimum_len {
                    return commands;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        read_fake_commands(log)
    }

    async fn wait_for_fake_done(events: &mut EventStream) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(ChatEvent::MessageAdded(message)) if message.content == "fake done" => {
                        return;
                    }
                    Some(_) => {}
                    None => panic!("fake Tycode event stream ended before fake done"),
                }
            }
        })
        .await;
        assert!(event.is_ok(), "timed out waiting for fake Tycode response");
    }

    fn payload(message: &str) -> SendMessagePayload {
        SendMessagePayload {
            message: message.to_string(),
            images: None,
            origin: None,
            tool_response: None,
        }
    }

    #[test]
    fn build_tycode_mcp_servers_json_supports_stdio_servers() {
        let json = build_tycode_mcp_servers_json(&[StartupMcpServer {
            name: "context7".to_string(),
            transport: StartupMcpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["@upstash/context7-mcp@latest".to_string()],
                env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            },
        }])
        .expect("stdio MCP config should serialize");
        let value: Value = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(
            value["context7"]["command"],
            Value::String("npx".to_string())
        );
        assert_eq!(
            value["context7"]["args"],
            Value::Array(vec![Value::String(
                "@upstash/context7-mcp@latest".to_string()
            )])
        );
        assert_eq!(
            value["context7"]["env"]["FOO"],
            Value::String("bar".to_string())
        );
    }

    #[test]
    fn tycode_read_only_access_mode_uses_read_only_agent_tools() {
        let agent_json = tycode_read_only_agent_json(&BackendSpawnConfig {
            resolved_spawn_config: crate::agent::customization::ResolvedSpawnConfig {
                access_mode: BackendAccessMode::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("read-only agent json");
        let value: Value = serde_json::from_str(&agent_json).expect("valid agent json");
        let tools = value
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert!(tools.contains(&"set_tracked_files"));
        assert!(
            tools.contains(&"run_build_test"),
            "read-only is advisory, so build/test commands must still be available"
        );
        assert!(!tools.contains(&"write_file"));
        assert!(!tools.contains(&"modify_file"));
    }

    #[test]
    fn map_tycode_value_to_chat_events_passes_through_assistant_message_added() {
        let value = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1776827246365_u64,
                "sender": {
                    "Assistant": {
                        "agent": "tycode"
                    }
                },
                "content": "hello from tycode",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(
            events.len(),
            1,
            "assistant message should stay a single event"
        );

        match &events[0] {
            ChatEvent::MessageAdded(message) => {
                assert_eq!(message.timestamp, 1776827246365_u64);
                assert_eq!(message.content, "hello from tycode");
                match &message.sender {
                    MessageSender::Assistant { agent } => assert_eq!(agent, "tycode"),
                    other => panic!("expected assistant sender, got {other:?}"),
                }
            }
            other => panic!("expected MessageAdded pass-through, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_passes_through_stream_start() {
        let value = serde_json::json!({
            "kind": "StreamStart",
            "data": {
                "message_id": "msg-1776827246365",
                "agent": "tycode",
                "model": "ClaudeSonnet46"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);

        match &events[0] {
            ChatEvent::StreamStart(data) => {
                assert_eq!(data.message_id.as_deref(), Some("msg-1776827246365"));
                assert_eq!(data.agent, "tycode");
                assert_eq!(data.model.as_deref(), Some("ClaudeSonnet46"));
            }
            other => panic!("expected StreamStart pass-through, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_translates_orchestration() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "AgentStarted",
                    "parent_agent_id": null,
                    "task_preview": "plan the work",
                    "origin": { "kind": "Root" },
                    "depth": 1,
                    "interactive": true,
                    "model": "claude-fable"
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::Orchestration(event) => {
                assert_eq!(event.agent_id.0, "root-1");
                assert_eq!(event.agent_type.0, "swarm");
                match &event.payload {
                    OrchestrationPayload::AgentStarted {
                        parent_agent_id,
                        task_preview,
                        origin,
                        depth,
                        interactive,
                        model,
                    } => {
                        assert_eq!(parent_agent_id, &None);
                        assert_eq!(task_preview, "plan the work");
                        assert!(matches!(origin, OrchestrationAgentOrigin::Root));
                        assert_eq!(*depth, 1);
                        assert!(*interactive);
                        assert_eq!(model, &Some(protocol::TycodeModel::ClaudeFable));
                    }
                    other => panic!("expected AgentStarted, got {other:?}"),
                }
            }
            other => panic!("expected Orchestration event, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_value_to_chat_events_ignores_unknown_orchestration_payload_kind() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "FuturePayload",
                    "new_field": true
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert!(events.is_empty());
    }

    #[test]
    fn map_tycode_value_to_chat_events_surfaces_malformed_known_orchestration_payload() {
        let value = serde_json::json!({
            "kind": "Orchestration",
            "data": {
                "agent_id": "root-1",
                "agent_type": "swarm",
                "payload": {
                    "kind": "AgentCompleted",
                    "status": "Succeeded"
                }
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::MessageAdded(message) => {
                assert!(matches!(message.sender, MessageSender::Error));
                assert!(
                    message
                        .content
                        .contains("Malformed Tycode Orchestration event")
                );
                assert!(message.content.contains("AgentCompleted"));
            }
            other => panic!("expected visible error message, got {other:?}"),
        }
    }

    #[test]
    fn map_tycode_operation_cancelled_passes_through_without_terminal_worker_inference() {
        let value = serde_json::json!({
            "kind": "OperationCancelled",
            "data": {
                "message": "cancelled"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ChatEvent::OperationCancelled(data) if data.message == "cancelled"
        ));
    }

    #[test]
    fn map_tycode_value_to_chat_events_ignores_session_started() {
        let value = serde_json::json!({
            "kind": "SessionStarted",
            "data": {
                "session_id": "session-123"
            }
        });

        let events = map_tycode_value_to_chat_events(&value);
        assert!(
            events.is_empty(),
            "SessionStarted should stay out of chat streams"
        );
    }

    #[test]
    fn resume_replay_barrier_ignores_historical_sessions_list_until_replay_count_exhausted() {
        let mut barrier = TycodeResumeReplayBarrier::new("session-1".to_owned(), 5);
        let pre_resume_warning = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1_u64,
                "sender": {
                    "Error": {}
                },
                "content": "startup warning before resume",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });
        let session_started = serde_json::json!({
            "kind": "SessionStarted",
            "data": { "session_id": "session-1" }
        });
        let conversation_cleared = serde_json::json!({ "kind": "ConversationCleared" });
        let historical_sessions_list = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });
        let historical_message = serde_json::json!({
            "kind": "MessageAdded",
            "data": {
                "timestamp": 1_u64,
                "sender": {
                    "Assistant": {
                        "agent": "tycode"
                    }
                },
                "content": "still replayed after historical SessionsList",
                "reasoning": null,
                "tool_calls": [],
                "model_info": null,
                "token_usage": null,
                "context_breakdown": null,
                "images": []
            }
        });
        let historical_final_sessions_list = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });
        let genuine_sentinel = serde_json::json!({
            "kind": "SessionsList",
            "data": { "sessions": [] }
        });

        assert!(
            !barrier.observe(&pre_resume_warning),
            "pre-resume startup output must not consume replay count or complete the barrier"
        );
        for event in [
            &session_started,
            &conversation_cleared,
            &historical_sessions_list,
            &historical_message,
            &historical_final_sessions_list,
        ] {
            assert!(
                !barrier.observe(event),
                "historical replay event must not complete the barrier: {event}"
            );
        }
        assert!(
            barrier.observe(&genuine_sentinel),
            "the post-resume ListSessions response should complete the barrier"
        );
    }

    #[test]
    fn resume_replay_event_count_includes_historical_sessions_list_and_skips_deltas() {
        let session = serde_json::json!({
            "id": "session-1",
            "events": [
                { "kind": "SessionsList", "data": { "sessions": [] } },
                { "kind": "StreamDelta", "data": { "message_id": "m1", "text": "skip" } },
                { "kind": "StreamReasoningDelta", "data": { "message_id": "m1", "text": "skip" } },
                { "kind": "MessageAdded", "data": { "content": "keep" } }
            ]
        });
        let count = tycode_resume_replay_event_count_from_json(&session.to_string())
            .expect("session replay count should parse");
        assert_eq!(
            count, 4,
            "SessionStarted and ConversationCleared plus non-delta persisted events"
        );
    }
}
