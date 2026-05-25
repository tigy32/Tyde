use std::collections::HashSet;
#[cfg(any(not(target_os = "ios"), test))]
use std::io;
#[cfg(any(not(target_os = "ios"), test))]
use std::path::Path;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mqtt_transport::{BrokerAuth, BrokerEndpoint, PreSharedKey, RoomId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(not(target_os = "ios"))]
use tauri::Manager;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::types::{
    BrokerAuthSummary, BrokerEndpointSummary, KeychainSecretId, LocalHostId, PairedHostSummary,
    RoomIdSummary,
};

#[cfg(any(not(target_os = "ios"), test))]
pub const PAIRED_HOSTS_FILE_NAME: &str = "paired_hosts.json";
const STORE_VERSION: u32 = 2;
#[cfg(any(not(target_os = "ios"), test))]
const LEGACY_STORE_VERSION: u64 = 1;
const STORE_CHANNEL_CAPACITY: usize = 64;
#[cfg(target_os = "ios")]
const PAIRED_HOSTS_KEYCHAIN_SERVICE: &str = "dev.tyde.mobile.paired-hosts";
#[cfg(target_os = "ios")]
const PAIRED_HOSTS_KEYCHAIN_ACCOUNT: &str = "paired-hosts";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHostRecord {
    pub local_host_id: LocalHostId,
    pub host_label: String,
    pub broker: BrokerEndpoint,
    pub room: RoomId,
    pub psk_keychain_key_id: KeychainSecretId,
    pub credential_fingerprint: String,
    pub auto_connect: bool,
    pub last_connected_at_ms: Option<u64>,
}

impl PairedHostRecord {
    pub fn summary(&self) -> PairedHostSummary {
        PairedHostSummary {
            local_host_id: self.local_host_id.clone(),
            host_label: self.host_label.clone(),
            broker: BrokerEndpointSummary {
                url: self.broker.url.clone(),
                auth: match &self.broker.auth {
                    BrokerAuth::Anonymous => BrokerAuthSummary::Anonymous,
                    BrokerAuth::UsernamePassword { username, password } => {
                        BrokerAuthSummary::UsernamePassword {
                            username: username.clone(),
                            has_password: !password.is_empty(),
                        }
                    }
                },
            },
            room: RoomIdSummary(self.room.to_string()),
            credential_fingerprint: self.credential_fingerprint.clone(),
            auto_connect: self.auto_connect,
            last_connected_at_ms: self.last_connected_at_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHosts {
    pub version: u32,
    pub hosts: Vec<PairedHostRecord>,
}

impl PairedHosts {
    pub fn empty() -> Self {
        Self {
            version: STORE_VERSION,
            hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLoadFailure {
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("paired-host store is unavailable: {message}")]
    Unavailable { message: String },
    #[cfg(not(target_os = "ios"))]
    #[error("failed to resolve app data directory: {0}")]
    AppDataDir(String),
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to load paired hosts from {path}: {source}")]
    Load { path: PathBuf, source: io::Error },
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to parse paired hosts from {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("unsupported paired-host store version at {path}: expected {expected}, got {actual}")]
    UnsupportedVersion {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to create paired-host store directory {path}: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("failed to serialize paired-host store: {0}")]
    Serialize(serde_json::Error),
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to write paired-host store temp file {path}: {source}")]
    WriteTemp { path: PathBuf, source: io::Error },
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to set paired-host store temp file permissions {path}: {source}")]
    SetPermissions { path: PathBuf, source: io::Error },
    #[cfg(any(not(target_os = "ios"), test))]
    #[error("failed to rename paired-host store temp file {temp_path} to {path}: {source}")]
    Rename {
        temp_path: PathBuf,
        path: PathBuf,
        source: io::Error,
    },
    #[cfg(target_os = "ios")]
    #[error("paired-host keychain operation {operation} failed: {message}")]
    Keychain {
        operation: &'static str,
        message: String,
    },
    #[cfg(target_os = "ios")]
    #[error("failed to parse paired hosts from iOS Keychain: {0}")]
    KeychainParse(serde_json::Error),
    #[cfg(target_os = "ios")]
    #[error(
        "unsupported paired-host store version in iOS Keychain: expected {expected}, got {actual}"
    )]
    KeychainUnsupportedVersion { expected: u32, actual: u32 },
    #[error("paired host {0} was not found")]
    HostNotFound(LocalHostId),
    #[error("paired host {0} already exists")]
    HostAlreadyExists(LocalHostId),
    #[error("paired-host store validation failed: {0}")]
    Validation(String),
    #[error("paired-host store actor stopped")]
    ActorStopped,
    #[error("paired-host store response channel closed")]
    ResponseClosed,
}

pub struct Store {
    tx: mpsc::Sender<StoreCommand>,
}

impl Store {
    pub async fn open(app: &tauri::AppHandle) -> (Self, Option<StoreLoadFailure>) {
        let (state, load_failure) = open_default_state(app).await;

        let (tx, rx) = mpsc::channel(STORE_CHANNEL_CAPACITY);
        tauri::async_runtime::spawn(StoreActor { state, rx }.run());
        (Self { tx }, load_failure)
    }

    #[cfg(test)]
    pub async fn open_at_path(path: PathBuf) -> (Self, Option<StoreLoadFailure>) {
        let (state, load_failure) = match load_from_path(&path).await {
            Ok(hosts) => (
                StoreState::Loaded {
                    backend: StoreBackend::File { path },
                    hosts,
                },
                None,
            ),
            Err(error) => {
                let failure = StoreLoadFailure {
                    path: Some(error.path().to_path_buf()),
                    message: error.to_string(),
                };
                (
                    StoreState::Unavailable {
                        message: error.to_string(),
                    },
                    Some(failure),
                )
            }
        };
        let (tx, rx) = mpsc::channel(STORE_CHANNEL_CAPACITY);
        tauri::async_runtime::spawn(StoreActor { state, rx }.run());
        (Self { tx }, load_failure)
    }

    pub async fn list_records(&self) -> Result<Vec<PairedHostRecord>, StoreError> {
        self.request(|reply| StoreCommand::List { reply }).await
    }

    pub async fn list_summaries(&self) -> Result<Vec<PairedHostSummary>, StoreError> {
        let records = self.list_records().await?;
        Ok(records.iter().map(PairedHostRecord::summary).collect())
    }

    pub async fn get(&self, local_host_id: LocalHostId) -> Result<PairedHostRecord, StoreError> {
        self.request(|reply| StoreCommand::Get {
            local_host_id,
            reply,
        })
        .await
    }

    pub async fn insert(&self, record: PairedHostRecord) -> Result<(), StoreError> {
        self.request(|reply| StoreCommand::Insert { record, reply })
            .await
    }

    pub async fn remove(&self, local_host_id: LocalHostId) -> Result<PairedHostRecord, StoreError> {
        self.request(|reply| StoreCommand::Remove {
            local_host_id,
            reply,
        })
        .await
    }

    pub async fn set_auto_connect(
        &self,
        local_host_id: LocalHostId,
        auto_connect: bool,
    ) -> Result<PairedHostRecord, StoreError> {
        self.request(|reply| StoreCommand::SetAutoConnect {
            local_host_id,
            auto_connect,
            reply,
        })
        .await
    }

    pub async fn set_last_connected_at_ms(
        &self,
        local_host_id: LocalHostId,
        last_connected_at_ms: Option<u64>,
    ) -> Result<PairedHostRecord, StoreError> {
        self.request(|reply| StoreCommand::SetLastConnectedAtMs {
            local_host_id,
            last_connected_at_ms,
            reply,
        })
        .await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, StoreError>>) -> StoreCommand,
    ) -> Result<T, StoreError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| StoreError::ActorStopped)?;
        response.await.map_err(|_| StoreError::ResponseClosed)?
    }
}

#[cfg(target_os = "ios")]
async fn open_default_state(_app: &tauri::AppHandle) -> (StoreState, Option<StoreLoadFailure>) {
    match load_from_keychain() {
        Ok(hosts) => (
            StoreState::Loaded {
                backend: StoreBackend::Keychain,
                hosts,
            },
            None,
        ),
        Err(error) => {
            let message = error.to_string();
            (
                StoreState::Unavailable {
                    message: message.clone(),
                },
                Some(StoreLoadFailure {
                    path: None,
                    message,
                }),
            )
        }
    }
}

#[cfg(not(target_os = "ios"))]
async fn open_default_state(app: &tauri::AppHandle) -> (StoreState, Option<StoreLoadFailure>) {
    let path_result = app
        .path()
        .app_data_dir()
        .map(|dir| dir.join(PAIRED_HOSTS_FILE_NAME));
    match path_result {
        Ok(path) => match load_from_path(&path).await {
            Ok(hosts) => (
                StoreState::Loaded {
                    backend: StoreBackend::File { path },
                    hosts,
                },
                None,
            ),
            Err(error) => {
                let failure = StoreLoadFailure {
                    path: Some(error.path().to_path_buf()),
                    message: error.to_string(),
                };
                (
                    StoreState::Unavailable {
                        message: error.to_string(),
                    },
                    Some(failure),
                )
            }
        },
        Err(error) => {
            let message = StoreError::AppDataDir(error.to_string()).to_string();
            (
                StoreState::Unavailable {
                    message: message.clone(),
                },
                Some(StoreLoadFailure {
                    path: None,
                    message,
                }),
            )
        }
    }
}

enum StoreCommand {
    List {
        reply: oneshot::Sender<Result<Vec<PairedHostRecord>, StoreError>>,
    },
    Get {
        local_host_id: LocalHostId,
        reply: oneshot::Sender<Result<PairedHostRecord, StoreError>>,
    },
    Insert {
        record: PairedHostRecord,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Remove {
        local_host_id: LocalHostId,
        reply: oneshot::Sender<Result<PairedHostRecord, StoreError>>,
    },
    SetAutoConnect {
        local_host_id: LocalHostId,
        auto_connect: bool,
        reply: oneshot::Sender<Result<PairedHostRecord, StoreError>>,
    },
    SetLastConnectedAtMs {
        local_host_id: LocalHostId,
        last_connected_at_ms: Option<u64>,
        reply: oneshot::Sender<Result<PairedHostRecord, StoreError>>,
    },
}

enum StoreState {
    Loaded {
        backend: StoreBackend,
        hosts: PairedHosts,
    },
    Unavailable {
        message: String,
    },
}

enum StoreBackend {
    #[cfg(any(not(target_os = "ios"), test))]
    File { path: PathBuf },
    #[cfg(target_os = "ios")]
    Keychain,
}

struct StoreActor {
    state: StoreState,
    rx: mpsc::Receiver<StoreCommand>,
}

impl StoreActor {
    async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                StoreCommand::List { reply } => {
                    let result = self.with_loaded(|hosts| Ok(hosts.hosts.clone())).await;
                    let _send_result = reply.send(result);
                }
                StoreCommand::Get {
                    local_host_id,
                    reply,
                } => {
                    let result = self
                        .with_loaded(|hosts| Ok(find_record(&hosts.hosts, &local_host_id).cloned()))
                        .await
                        .and_then(|record| record.ok_or(StoreError::HostNotFound(local_host_id)));
                    let _send_result = reply.send(result);
                }
                StoreCommand::Insert { record, reply } => {
                    let result = self.insert_record(record).await;
                    let _send_result = reply.send(result);
                }
                StoreCommand::Remove {
                    local_host_id,
                    reply,
                } => {
                    let result = self.remove_record(local_host_id).await;
                    let _send_result = reply.send(result);
                }
                StoreCommand::SetAutoConnect {
                    local_host_id,
                    auto_connect,
                    reply,
                } => {
                    let result = self.set_auto_connect(local_host_id, auto_connect).await;
                    let _send_result = reply.send(result);
                }
                StoreCommand::SetLastConnectedAtMs {
                    local_host_id,
                    last_connected_at_ms,
                    reply,
                } => {
                    let result = self
                        .set_last_connected_at_ms(local_host_id, last_connected_at_ms)
                        .await;
                    let _send_result = reply.send(result);
                }
            }
        }
    }

    async fn with_loaded<T>(
        &self,
        f: impl FnOnce(&PairedHosts) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match &self.state {
            StoreState::Loaded { hosts, .. } => f(hosts),
            StoreState::Unavailable { message, .. } => Err(StoreError::Unavailable {
                message: message.clone(),
            }),
        }
    }

    async fn insert_record(&mut self, record: PairedHostRecord) -> Result<(), StoreError> {
        match &mut self.state {
            StoreState::Loaded { backend, hosts } => {
                validate_record(&record)?;
                if find_record(&hosts.hosts, &record.local_host_id).is_some() {
                    return Err(StoreError::HostAlreadyExists(record.local_host_id));
                }
                hosts.hosts.push(record);
                validate_records(&hosts.hosts)?;
                save_to_backend(backend, hosts).await
            }
            StoreState::Unavailable { message, .. } => Err(StoreError::Unavailable {
                message: message.clone(),
            }),
        }
    }

    async fn remove_record(
        &mut self,
        local_host_id: LocalHostId,
    ) -> Result<PairedHostRecord, StoreError> {
        match &mut self.state {
            StoreState::Loaded { backend, hosts } => {
                let index = hosts
                    .hosts
                    .iter()
                    .position(|record| record.local_host_id == local_host_id)
                    .ok_or_else(|| StoreError::HostNotFound(local_host_id.clone()))?;
                let record = hosts.hosts.remove(index);
                save_to_backend(backend, hosts).await?;
                Ok(record)
            }
            StoreState::Unavailable { message, .. } => Err(StoreError::Unavailable {
                message: message.clone(),
            }),
        }
    }

    async fn set_auto_connect(
        &mut self,
        local_host_id: LocalHostId,
        auto_connect: bool,
    ) -> Result<PairedHostRecord, StoreError> {
        match &mut self.state {
            StoreState::Loaded { backend, hosts } => {
                let record = find_record_mut(&mut hosts.hosts, &local_host_id)
                    .ok_or_else(|| StoreError::HostNotFound(local_host_id.clone()))?;
                record.auto_connect = auto_connect;
                let updated = record.clone();
                save_to_backend(backend, hosts).await?;
                Ok(updated)
            }
            StoreState::Unavailable { message, .. } => Err(StoreError::Unavailable {
                message: message.clone(),
            }),
        }
    }

    async fn set_last_connected_at_ms(
        &mut self,
        local_host_id: LocalHostId,
        last_connected_at_ms: Option<u64>,
    ) -> Result<PairedHostRecord, StoreError> {
        match &mut self.state {
            StoreState::Loaded { backend, hosts } => {
                let record = find_record_mut(&mut hosts.hosts, &local_host_id)
                    .ok_or_else(|| StoreError::HostNotFound(local_host_id.clone()))?;
                record.last_connected_at_ms = last_connected_at_ms;
                let updated = record.clone();
                save_to_backend(backend, hosts).await?;
                Ok(updated)
            }
            StoreState::Unavailable { message, .. } => Err(StoreError::Unavailable {
                message: message.clone(),
            }),
        }
    }
}

fn find_record<'a>(
    hosts: &'a [PairedHostRecord],
    local_host_id: &LocalHostId,
) -> Option<&'a PairedHostRecord> {
    hosts
        .iter()
        .find(|record| record.local_host_id == *local_host_id)
}

fn find_record_mut<'a>(
    hosts: &'a mut [PairedHostRecord],
    local_host_id: &LocalHostId,
) -> Option<&'a mut PairedHostRecord> {
    hosts
        .iter_mut()
        .find(|record| record.local_host_id == *local_host_id)
}

pub fn credential_fingerprint(
    broker: &BrokerEndpoint,
    room: &RoomId,
    psk: &PreSharedKey,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(broker.url.as_str().as_bytes());
    hasher.update(room.as_base64url_no_pad().as_bytes());
    hasher.update(psk.as_bytes());
    let encoded = URL_SAFE_NO_PAD.encode(hasher.finalize());
    encoded.chars().take(16).collect()
}

#[cfg(any(not(target_os = "ios"), test))]
#[derive(Debug, Error)]
enum LoadFromPathError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[cfg(any(not(target_os = "ios"), test))]
impl LoadFromPathError {
    fn path(&self) -> &Path {
        match self {
            Self::Store(StoreError::Load { path, .. })
            | Self::Store(StoreError::Parse { path, .. })
            | Self::Store(StoreError::UnsupportedVersion { path, .. }) => path,
            Self::Store(_) => Path::new(PAIRED_HOSTS_FILE_NAME),
        }
    }
}

#[cfg(target_os = "ios")]
fn load_from_keychain() -> Result<PairedHosts, StoreError> {
    use security_framework::passwords::get_generic_password;

    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    let bytes =
        match get_generic_password(PAIRED_HOSTS_KEYCHAIN_SERVICE, PAIRED_HOSTS_KEYCHAIN_ACCOUNT) {
            Ok(bytes) => bytes,
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
                return Ok(PairedHosts::empty());
            }
            Err(error) => {
                return Err(StoreError::Keychain {
                    operation: "load",
                    message: error.to_string(),
                });
            }
        };
    let hosts: PairedHosts = serde_json::from_slice(&bytes).map_err(StoreError::KeychainParse)?;
    if hosts.version != STORE_VERSION {
        return Err(StoreError::KeychainUnsupportedVersion {
            expected: STORE_VERSION,
            actual: hosts.version,
        });
    }
    validate_records(&hosts.hosts)?;
    Ok(hosts)
}

#[cfg(any(not(target_os = "ios"), test))]
async fn load_from_path(path: &Path) -> Result<PairedHosts, LoadFromPathError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PairedHosts::empty()),
        Err(source) => {
            return Err(StoreError::Load {
                path: path.to_path_buf(),
                source,
            }
            .into());
        }
    };

    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|source| StoreError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| version == LEGACY_STORE_VERSION)
    {
        tokio::fs::remove_file(path)
            .await
            .map_err(|source| StoreError::Load {
                path: path.to_path_buf(),
                source,
            })?;
        return Ok(PairedHosts::empty());
    }

    let hosts: PairedHosts = serde_json::from_value(value).map_err(|source| StoreError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if hosts.version != STORE_VERSION {
        return Err(StoreError::UnsupportedVersion {
            path: path.to_path_buf(),
            expected: STORE_VERSION,
            actual: hosts.version,
        }
        .into());
    }
    validate_records(&hosts.hosts)?;
    Ok(hosts)
}

async fn save_to_backend(backend: &StoreBackend, hosts: &PairedHosts) -> Result<(), StoreError> {
    match backend {
        #[cfg(any(not(target_os = "ios"), test))]
        StoreBackend::File { path } => save_to_path(path, hosts).await,
        #[cfg(target_os = "ios")]
        StoreBackend::Keychain => save_to_keychain(hosts),
    }
}

#[cfg(target_os = "ios")]
fn save_to_keychain(hosts: &PairedHosts) -> Result<(), StoreError> {
    use security_framework::passwords::set_generic_password;

    validate_records(&hosts.hosts)?;
    let bytes = serde_json::to_vec(hosts).map_err(StoreError::Serialize)?;
    set_generic_password(
        PAIRED_HOSTS_KEYCHAIN_SERVICE,
        PAIRED_HOSTS_KEYCHAIN_ACCOUNT,
        &bytes,
    )
    .map_err(|error| StoreError::Keychain {
        operation: "store",
        message: error.to_string(),
    })
}

#[cfg(any(not(target_os = "ios"), test))]
async fn save_to_path(path: &Path, hosts: &PairedHosts) -> Result<(), StoreError> {
    validate_records(&hosts.hosts)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    let bytes = serde_json::to_vec_pretty(hosts).map_err(StoreError::Serialize)?;
    let temp_path = temp_path_for(path);
    tokio::fs::write(&temp_path, bytes)
        .await
        .map_err(|source| StoreError::WriteTemp {
            path: temp_path.clone(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|source| StoreError::SetPermissions {
                path: temp_path.clone(),
                source,
            })?;
    }
    tokio::fs::rename(&temp_path, path)
        .await
        .map_err(|source| StoreError::Rename {
            temp_path,
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

fn validate_records(records: &[PairedHostRecord]) -> Result<(), StoreError> {
    let mut ids = HashSet::new();
    for record in records {
        validate_record(record)?;
        if !ids.insert(record.local_host_id.clone()) {
            return Err(StoreError::Validation(format!(
                "duplicate local_host_id {}",
                record.local_host_id
            )));
        }
    }
    Ok(())
}

fn validate_record(record: &PairedHostRecord) -> Result<(), StoreError> {
    if record.local_host_id.0.trim().is_empty() {
        return Err(StoreError::Validation(
            "paired host local_host_id must not be empty".to_owned(),
        ));
    }
    if record.host_label.trim().is_empty() {
        return Err(StoreError::Validation(format!(
            "paired host {} host_label must not be empty",
            record.local_host_id
        )));
    }
    if record.psk_keychain_key_id.0.trim().is_empty() {
        return Err(StoreError::Validation(format!(
            "paired host {} psk_keychain_key_id must not be empty",
            record.local_host_id
        )));
    }
    if record.credential_fingerprint.trim().is_empty() {
        return Err(StoreError::Validation(format!(
            "paired host {} credential_fingerprint must not be empty",
            record.local_host_id
        )));
    }
    Ok(())
}

#[cfg(any(not(target_os = "ios"), test))]
fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!(".{name}.{}.tmp", uuid::Uuid::new_v4()),
        None => format!(".paired_hosts.{}.tmp", uuid::Uuid::new_v4()),
    };
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::BrokerUrl;

    fn broker() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://broker.emqx.io:8883").expect("broker url"),
            auth: BrokerAuth::Anonymous,
        }
    }

    fn broker_with_password(password: &str) -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://broker.emqx.io:8883").expect("broker url"),
            auth: BrokerAuth::UsernamePassword {
                username: "mobile".to_owned(),
                password: password.to_owned(),
            },
        }
    }

    fn record(local: &str) -> PairedHostRecord {
        let broker = broker();
        let room = RoomId([7_u8; 16]);
        let psk = PreSharedKey::from_slice(&[9_u8; 32]).expect("psk");
        PairedHostRecord {
            local_host_id: LocalHostId(local.to_owned()),
            host_label: format!("Host {local}"),
            broker: broker.clone(),
            room,
            psk_keychain_key_id: KeychainSecretId(format!("key-{local}")),
            credential_fingerprint: credential_fingerprint(&broker, &room, &psk),
            auto_connect: true,
            last_connected_at_ms: Some(42),
        }
    }

    #[tokio::test]
    async fn paired_hosts_round_trip_without_psk_material() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PAIRED_HOSTS_FILE_NAME);
        let (store, load_failure) = Store::open_at_path(path.clone()).await;
        assert!(load_failure.is_none());

        store.insert(record("one")).await?;
        let listed = store.list_records().await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].local_host_id, LocalHostId("one".to_owned()));

        let json = tokio::fs::read_to_string(&path).await?;
        assert!(
            !json.contains(
                &PreSharedKey::from_slice(&[9_u8; 32])
                    .expect("psk")
                    .as_base64url_no_pad()
            )
        );
        assert!(json.contains("pskKeychainKeyId"));

        let (store2, load_failure2) = Store::open_at_path(path).await;
        assert!(load_failure2.is_none());
        let listed2 = store2.list_records().await?;
        assert_eq!(listed2, listed);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_version_one_store_is_wiped() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(PAIRED_HOSTS_FILE_NAME);
        tokio::fs::write(&path, r#"{"version":1,"hosts":[]}"#).await?;
        let (store, load_failure) = Store::open_at_path(path.clone()).await;
        assert!(load_failure.is_none());
        assert_eq!(store.list_records().await?, Vec::new());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn credential_fingerprint_is_base64url_16_chars() {
        let broker = broker();
        let fingerprint = credential_fingerprint(
            &broker,
            &RoomId([1_u8; 16]),
            &PreSharedKey::from_slice(&[2_u8; 32]).expect("psk"),
        );
        assert_eq!(fingerprint.len(), 16);
        assert!(
            fingerprint
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn paired_host_summary_redacts_broker_password() {
        let password = "super-secret-broker-password";
        let broker = broker_with_password(password);
        let mut record = record("password");
        record.broker = broker;

        let summary = record.summary();
        assert_eq!(
            summary.broker.auth,
            BrokerAuthSummary::UsernamePassword {
                username: "mobile".to_owned(),
                has_password: true,
            }
        );
        let encoded = serde_json::to_string(&summary).expect("summary json");
        assert!(!encoded.contains(password));
    }
}
