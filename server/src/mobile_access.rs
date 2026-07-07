use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs2::FileExt;
use hmac::{Hmac, Mac};
use mqtt_transport::{
    BrokerAuth, BrokerEndpoint, EnvelopeStream, ManagedMobilePairingQrPayload,
    ManagedMobilePairingQrPayloadParams, MobilePairingQrPayload, MqttConnectConfig,
    ParticipantRole, PreSharedKey, RoomId, validate_broker_url,
};
use protocol::{
    BrokerUrl, FrameKind, HostSettings, ManagedBrokerAuthorizerName, ManagedBrokerClientId,
    ManagedBrokerConnectAuth, ManagedBrokerCredentialScope, ManagedBrokerCredentials,
    ManagedBrokerEndpoint, ManagedBrokerGrantId, ManagedBrokerProvider, ManagedBrokerRegion,
    ManagedBrokerRole, ManagedBrokerTopicNamespace, MobileAccessErrorCode,
    MobileAccessStatePayload, MobileBrokerStatus, MobileDeviceId, MobileDeviceRenamePayload,
    MobileDeviceRevokePayload, MobileDeviceState, MobilePairingCancelPayload, MobilePairingOfferId,
    MobilePairingOfferPayload, MobilePairingQrUri, MobilePairingState, PROTOCOL_VERSION,
    StreamPath,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

use crate::ServerConfig;
use crate::accept;
use crate::connection::run_mobile_connection;
use crate::error::{AppError, AppResult};
use crate::host::HostHandle;
use crate::store::mobile_pairings::{
    ActiveManagedMobilePairingCredential, ActiveMobilePairingCredential,
    ManagedMobilePairingCredential, ManagedMobilePairingHandoff, MobilePairingRecord,
    MobilePairings, MobilePairingsStore, key_fingerprint,
};
use crate::stream::{Stream, StreamClosed};

pub(crate) const DEFAULT_PAIRING_TTL: Duration = Duration::from_secs(120);
const PAIRING_TERMINAL_GRACE: Duration = Duration::from_millis(250);
const ACCEPT_RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const ACCEPT_RECONNECT_MAX: Duration = Duration::from_secs(30);
const MANAGED_SERVICE_BASE_URL_ENV: &str = "TYDE_MOBILE_SERVICE_BASE_URL";
const DEFAULT_MANAGED_SERVICE_BASE_URL: &str = "https://tycode.dev/api/tyde/mobile/v1";
const PAIRING_HMAC_PREFIX: &str = "TYCODE-PAIRING-HMAC-V1";
const OFFER_POLL_INTERVAL: Duration = Duration::from_secs(1);

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub(crate) struct MobileAccessHandle {
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
}

impl MobileAccessHandle {
    pub(crate) fn new(tx: mpsc::UnboundedSender<MobileAccessCommand>) -> Self {
        Self { tx }
    }

    pub(crate) async fn register_bootstrap_subscriber(
        &self,
        stream: Stream,
    ) -> Result<MobileAccessStatePayload, StreamClosed> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MobileAccessCommand::RegisterBootstrapSubscriber {
                stream,
                reply: reply_tx,
            })
            .map_err(|_| StreamClosed)?;
        reply_rx.await.map_err(|_| StreamClosed)?
    }

    pub(crate) fn activate_bootstrap_subscriber(&self, path: StreamPath) {
        let _ = self
            .tx
            .send(MobileAccessCommand::ActivateBootstrapSubscriber { path });
    }

    pub(crate) fn unregister_subscriber(&self, path: StreamPath) {
        let _ = self
            .tx
            .send(MobileAccessCommand::UnregisterSubscriber { path });
    }

    pub(crate) fn settings_changed(&self, settings: HostSettings) {
        let _ = self
            .tx
            .send(MobileAccessCommand::SettingsChanged { settings });
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.tx.send(MobileAccessCommand::Shutdown);
    }

    pub(crate) fn start_pairing(&self, requester: StreamPath) -> AppResult<()> {
        self.tx
            .send(MobileAccessCommand::StartPairing { requester })
            .map_err(|_| {
                AppError::internal(
                    "mobile_pairing_start",
                    anyhow!("mobile access actor stopped"),
                )
            })
    }

    pub(crate) fn cancel_pairing(&self, payload: MobilePairingCancelPayload) -> AppResult<()> {
        self.tx
            .send(MobileAccessCommand::CancelPairing {
                offer_id: payload.offer_id,
            })
            .map_err(|_| {
                AppError::internal(
                    "mobile_pairing_cancel",
                    anyhow!("mobile access actor stopped"),
                )
            })
    }

    pub(crate) async fn revoke_device(&self, payload: MobileDeviceRevokePayload) -> AppResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MobileAccessCommand::RevokeDevice {
                device_id: payload.device_id,
                reply: reply_tx,
            })
            .map_err(|_| {
                AppError::internal(
                    "mobile_device_revoke",
                    anyhow!("mobile access actor stopped"),
                )
            })?;
        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into_app_error("mobile_device_revoke")),
            Err(_) => Err(AppError::internal(
                "mobile_device_revoke",
                anyhow!("mobile access actor dropped revoke reply"),
            )),
        }
    }

    pub(crate) async fn rename_device(&self, payload: MobileDeviceRenamePayload) -> AppResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MobileAccessCommand::RenameDevice {
                device_id: payload.device_id,
                label: payload.label,
                reply: reply_tx,
            })
            .map_err(|_| {
                AppError::internal(
                    "mobile_device_rename",
                    anyhow!("mobile access actor stopped"),
                )
            })?;
        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into_app_error("mobile_device_rename")),
            Err(_) => Err(AppError::internal(
                "mobile_device_rename",
                anyhow!("mobile access actor dropped rename reply"),
            )),
        }
    }
}

pub(crate) struct MobileAccessInit {
    pub(crate) pairings_store: MobilePairingsStore,
    pub(crate) initial_settings: HostSettings,
    pub(crate) pairing_ttl: Duration,
    pub(crate) managed_service_base_url: Option<String>,
}

pub(crate) fn spawn_mobile_access_actor(
    host: HostHandle,
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    rx: mpsc::UnboundedReceiver<MobileAccessCommand>,
    init: MobileAccessInit,
) -> Result<(), String> {
    let actor = MobileAccessActor::new(host, tx, rx, init)?;
    spawn_worker("tyde-mobile-access", actor.run());
    Ok(())
}

pub(crate) enum MobileAccessCommand {
    Shutdown,
    RegisterBootstrapSubscriber {
        stream: Stream,
        reply: oneshot::Sender<Result<MobileAccessStatePayload, StreamClosed>>,
    },
    ActivateBootstrapSubscriber {
        path: StreamPath,
    },
    UnregisterSubscriber {
        path: StreamPath,
    },
    SettingsChanged {
        settings: HostSettings,
    },
    StartPairing {
        requester: StreamPath,
    },
    CancelPairing {
        offer_id: MobilePairingOfferId,
    },
    RevokeDevice {
        device_id: MobileDeviceId,
        reply: oneshot::Sender<Result<(), MobileAccessCommandFailure>>,
    },
    RenameDevice {
        device_id: MobileDeviceId,
        label: String,
        reply: oneshot::Sender<Result<(), MobileAccessCommandFailure>>,
    },
    PairingTransportConnected {
        offer_id: MobilePairingOfferId,
        stream: EnvelopeStream,
    },
    DeviceTransportConnected {
        device_id: MobileDeviceId,
        stream: EnvelopeStream,
    },
    PairingOfferRedeemed {
        offer_id: MobilePairingOfferId,
        handoff: Box<ManagedMobilePairingHandoff>,
    },
    PairingOfferTerminal {
        offer_id: MobilePairingOfferId,
        state: ManagedOfferTerminalState,
    },
    PairingFailed {
        offer_id: MobilePairingOfferId,
        code: MobileAccessErrorCode,
        message: String,
    },
    DeviceAcceptFailed {
        device_id: MobileDeviceId,
        code: MobileAccessErrorCode,
        message: String,
    },
    PairingExpired {
        offer_id: MobilePairingOfferId,
    },
    PairingGraceElapsed {
        offer_id: MobilePairingOfferId,
    },
    DeviceDisconnected {
        device_id: MobileDeviceId,
        connection_instance_id: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagedOfferTerminalState {
    Expired,
    Cancelled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MobileAccessCommandFailure {
    code: MobileAccessErrorCode,
    message: String,
}

impl MobileAccessCommandFailure {
    fn new(code: MobileAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn into_app_error(self, operation: &'static str) -> AppError {
        AppError::internal(
            operation,
            anyhow!("{}: {}", self.code_label(), self.message),
        )
    }

    fn code_label(&self) -> &'static str {
        match self.code {
            MobileAccessErrorCode::InvalidConfig => "invalid_config",
            MobileAccessErrorCode::PassRequired => "pass_required",
            MobileAccessErrorCode::RepairRequired => "repair_required",
            MobileAccessErrorCode::ServiceAuthRequired => "service_auth_required",
            MobileAccessErrorCode::ServiceAuthFailed => "service_auth_failed",
            MobileAccessErrorCode::ServiceUnavailable => "service_unavailable",
            MobileAccessErrorCode::BrokerUnavailable => "broker_unavailable",
            MobileAccessErrorCode::BrokerConnectionFailed => "broker_connection_failed",
            MobileAccessErrorCode::BrokerProtocol => "broker_protocol",
            MobileAccessErrorCode::BrokerRejected => "broker_rejected",
            MobileAccessErrorCode::PairingExpired => "pairing_expired",
            MobileAccessErrorCode::PairingRejected => "pairing_rejected",
            MobileAccessErrorCode::CryptoFailed => "crypto_failed",
            MobileAccessErrorCode::DuplicateDevice => "duplicate_device",
            MobileAccessErrorCode::InvalidPairingQr => "invalid_pairing_qr",
            MobileAccessErrorCode::KeystoreFailed => "keystore_failed",
            MobileAccessErrorCode::StoreLoadFailed => "store_load_failed",
            MobileAccessErrorCode::TransportFailed => "transport_failed",
            MobileAccessErrorCode::UnknownDevice => "unknown_device",
            MobileAccessErrorCode::RevokedDevice => "revoked_device",
            MobileAccessErrorCode::VersionMismatch => "version_mismatch",
            MobileAccessErrorCode::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum AcceptTaskKey {
    Pairing(MobilePairingOfferId),
    Device(MobileDeviceId),
}

pub(crate) struct MobileAccessActor {
    host: HostHandle,
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    rx: mpsc::UnboundedReceiver<MobileAccessCommand>,
    pairings_store: MobilePairingsStore,
    managed_service: ManagedMobileServiceClient,
    settings: HostSettings,
    pairing_ttl: Duration,
    pairings: MobilePairings,
    broker_status: MobileBrokerStatus,
    pairing: MobilePairingState,
    subscribers: HashMap<StreamPath, Stream>,
    bootstrap_subscribers: HashMap<StreamPath, PendingBootstrapSubscriber>,
    active_requester: Option<StreamPath>,
    accept_tasks: HashMap<AcceptTaskKey, JoinHandle<()>>,
    connected_tasks: HashMap<MobileDeviceId, ConnectedMobileTask>,
    pairing_ttl_task: Option<JoinHandle<()>>,
    offer_poll_task: Option<JoinHandle<()>>,
    next_connection_instance_id: u64,
    mobile_pairings_lease: Option<MobilePairingsLease>,
}

struct ConnectedMobileTask {
    instance_id: u64,
    task: JoinHandle<()>,
}

struct PendingBootstrapSubscriber {
    stream: Stream,
    snapshot: MobileAccessStatePayload,
}

#[derive(Debug)]
struct MobilePairingsLease {
    file: File,
}

impl MobilePairingsLease {
    fn try_acquire(pairings_path: &Path) -> Result<Self, String> {
        let parent = pairings_path.parent().ok_or_else(|| {
            format!(
                "mobile pairings store path has no parent: {}",
                pairings_path.display()
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create mobile pairings store directory {}: {err}",
                parent.display()
            )
        })?;
        let lock_path = pairings_path.with_extension("lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| {
                format!(
                    "failed to open mobile pairings lock {}: {err}",
                    lock_path.display()
                )
            })?;
        try_lock_mobile_pairings_file(&file, &lock_path)?;
        file.set_len(0).map_err(|err| {
            format!(
                "failed to truncate mobile pairings lock {}: {err}",
                lock_path.display()
            )
        })?;
        writeln!(
            file,
            "pid={}\nstore={}",
            std::process::id(),
            pairings_path.display()
        )
        .map_err(|err| {
            format!(
                "failed to write mobile pairings lock {}: {err}",
                lock_path.display()
            )
        })?;
        if let Err(err) = file.sync_all() {
            tracing::warn!(path = %lock_path.display(), error = %err, "failed to sync mobile pairings lock");
        }
        Ok(Self { file })
    }
}

fn try_lock_mobile_pairings_file(file: &File, lock_path: &Path) -> Result<(), String> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(()),
        Err(err) if is_lock_contended(&err) => Err(format!(
            "mobile pairings are already in use by another Tyde host process ({})",
            lock_path.display()
        )),
        Err(err) => Err(format!(
            "failed to lock mobile pairings {}: {err}",
            lock_path.display()
        )),
    }
}

fn is_lock_contended(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::WouldBlock)
        || err.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

impl Drop for MobilePairingsLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Clone)]
struct ManagedMobileServiceClient {
    base: ManagedServiceBaseUrl,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
struct ManagedServiceBaseUrl {
    url: String,
    path_prefix: String,
}

impl ManagedMobileServiceClient {
    fn new(configured_base_url: Option<String>) -> Result<Self, String> {
        // `reqwest` uses no-provider rustls; ensure a default crypto provider is
        // installed before building the client or `Client::new` panics with
        // "No provider set". Idempotent, so binaries that already installed one
        // at startup are unaffected.
        crate::install_default_crypto_provider();
        Ok(Self {
            base: ManagedServiceBaseUrl::new(configured_base_url)?,
            http: reqwest::Client::new(),
        })
    }

    async fn create_host_offer(
        &self,
        request: CreateHostOfferRequest,
    ) -> Result<CreateHostOfferResponse, ManagedServiceError> {
        let url = self.base.url_for("/host/offers");
        let response = self
            .http
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(ManagedServiceError::transport)?;
        parse_managed_response(response).await
    }

    async fn poll_host_offer(
        &self,
        offer_id: &MobilePairingOfferId,
        host_offer_token: &str,
    ) -> Result<PollHostOfferResponse, ManagedServiceError> {
        let response = self
            .http
            .get(self.base.url_for(&format!("/host/offers/{offer_id}")))
            .bearer_auth(host_offer_token)
            .send()
            .await
            .map_err(ManagedServiceError::transport)?;
        parse_managed_response(response).await
    }

    async fn cancel_host_offer(
        &self,
        offer_id: &MobilePairingOfferId,
        host_offer_token: &str,
    ) -> Result<(), ManagedServiceError> {
        let response = self
            .http
            .delete(self.base.url_for(&format!("/host/offers/{offer_id}")))
            .bearer_auth(host_offer_token)
            .send()
            .await
            .map_err(ManagedServiceError::transport)?;
        let response: CancelHostOfferResponse = parse_managed_response(response).await?;
        if response.offer_id != offer_id.as_str() || response.status != HostOfferStatus::Cancelled {
            return Err(ManagedServiceError::new(
                MobileAccessErrorCode::ServiceUnavailable,
                "managed mobile service returned an invalid cancel response",
            ));
        }
        Ok(())
    }

    async fn mint_host_broker_credentials(
        &self,
        record: &MobilePairingRecord,
    ) -> Result<MintBrokerCredentialsResponse, ManagedServiceError> {
        let managed = record.managed.as_ref().ok_or_else(|| {
            ManagedServiceError::new(
                MobileAccessErrorCode::RepairRequired,
                "mobile pairing has no managed tycode.dev identity",
            )
        })?;
        let request = MintBrokerCredentialsRequest {
            role: BrokerRole::Host,
            client_instance_id: Uuid::new_v4().to_string(),
            protocol_version: PROTOCOL_VERSION,
            transport_protocol_version: mqtt_transport::MQTT_TRANSPORT_PROTOCOL_VERSION,
            requested_rooms: vec![RequestedRoom {
                room_id: record.room.to_string(),
                purpose: RequestedRoomPurpose::Rendezvous,
            }],
        };
        let body = serde_json::to_vec(&request).map_err(|err| {
            ManagedServiceError::new(
                MobileAccessErrorCode::Internal,
                format!("failed to serialize broker credential request: {err}"),
            )
        })?;
        let path = self.base.path_for(&format!(
            "/pairings/{}/broker-credentials",
            managed.pairing_id
        ));
        let auth = pairing_auth_header(
            &managed.host_pairing_secret,
            "POST",
            &path,
            &body,
            BrokerRole::Host,
            &managed.pairing_id,
        )?;
        let response = self
            .http
            .post(self.base.url_for(&format!(
                "/pairings/{}/broker-credentials",
                managed.pairing_id
            )))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("x-tycode-pairing-auth", auth)
            .body(body)
            .send()
            .await
            .map_err(ManagedServiceError::transport)?;
        parse_managed_response(response).await
    }
}

impl ManagedServiceBaseUrl {
    fn new(configured_base_url: Option<String>) -> Result<Self, String> {
        let value = match configured_base_url {
            Some(value) => value,
            None => std::env::var(MANAGED_SERVICE_BASE_URL_ENV)
                .unwrap_or_else(|_| DEFAULT_MANAGED_SERVICE_BASE_URL.to_owned()),
        };
        let trimmed = value.trim().trim_end_matches('/').to_owned();
        if trimmed.is_empty() {
            return Err(format!("{MANAGED_SERVICE_BASE_URL_ENV} must not be empty"));
        }
        let parsed = url::Url::parse(&trimmed)
            .map_err(|err| format!("managed mobile service URL {trimmed:?} is invalid: {err}"))?;
        match parsed.scheme() {
            "https" => {}
            "http" if is_loopback_url(&parsed) => {}
            scheme => {
                return Err(format!(
                    "managed mobile service URL scheme {scheme:?} is unsupported; expected https://"
                ));
            }
        }
        let path_prefix = parsed.path().trim_end_matches('/').to_owned();
        let path_prefix = if path_prefix.is_empty() {
            String::new()
        } else {
            path_prefix
        };
        Ok(Self {
            url: trimmed,
            path_prefix,
        })
    }

    fn url_for(&self, endpoint_path: &str) -> String {
        format!("{}{}", self.url, endpoint_path)
    }

    fn path_for(&self, endpoint_path: &str) -> String {
        format!("{}{}", self.path_prefix, endpoint_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedServiceError {
    code: MobileAccessErrorCode,
    message: String,
}

impl ManagedServiceError {
    fn new(code: MobileAccessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn transport(error: reqwest::Error) -> Self {
        Self::new(
            MobileAccessErrorCode::ServiceUnavailable,
            format!("managed mobile service request failed: {error}"),
        )
    }
}

#[derive(Debug, Serialize)]
struct CreateHostOfferRequest {
    host_label: String,
    host_release_version: String,
    protocol_version: u32,
    transport_protocol_version: u32,
    host_nonce: String,
}

#[derive(Deserialize)]
struct CreateHostOfferResponse {
    offer_id: String,
    offer_secret: String,
    host_offer_token: String,
    expires_at_ms: u64,
    broker: ContractBrokerEndpoint,
    host_broker_credentials: ContractBrokerCredentials,
    status: HostOfferStatus,
}

impl std::fmt::Debug for CreateHostOfferResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreateHostOfferResponse")
            .field("offer_id", &self.offer_id)
            .field("offer_secret", &"<redacted>")
            .field("host_offer_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("broker", &self.broker)
            .field("host_broker_credentials", &"<redacted>")
            .field("status", &self.status)
            .finish()
    }
}

#[derive(Deserialize)]
struct PollHostOfferResponse {
    offer_id: String,
    status: HostOfferStatus,
    expires_at_ms: Option<u64>,
    pairing_id: Option<String>,
    host_pairing_secret: Option<String>,
    device: Option<ContractDeviceSummary>,
    broker: Option<ContractBrokerEndpoint>,
    host_broker_credentials: Option<ContractBrokerCredentials>,
}

impl std::fmt::Debug for PollHostOfferResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PollHostOfferResponse")
            .field("offer_id", &self.offer_id)
            .field("status", &self.status)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("pairing_id", &self.pairing_id)
            .field(
                "host_pairing_secret",
                &self.host_pairing_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("device", &self.device)
            .field("broker", &self.broker)
            .field(
                "host_broker_credentials",
                &self.host_broker_credentials.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct CancelHostOfferResponse {
    offer_id: String,
    status: HostOfferStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HostOfferStatus {
    Pending,
    Redeemed,
    Expired,
    Cancelled,
    Failed,
}

#[derive(Debug, Serialize)]
struct MintBrokerCredentialsRequest {
    role: BrokerRole,
    client_instance_id: String,
    protocol_version: u32,
    transport_protocol_version: u32,
    requested_rooms: Vec<RequestedRoom>,
}

#[derive(Debug, Serialize)]
struct RequestedRoom {
    room_id: String,
    purpose: RequestedRoomPurpose,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RequestedRoomPurpose {
    Rendezvous,
}

#[derive(Deserialize)]
struct MintBrokerCredentialsResponse {
    pairing_id: String,
    status: PairingStatus,
    broker: ContractBrokerEndpoint,
    broker_credentials: ContractBrokerCredentials,
}

impl std::fmt::Debug for MintBrokerCredentialsResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MintBrokerCredentialsResponse")
            .field("pairing_id", &self.pairing_id)
            .field("status", &self.status)
            .field("broker", &self.broker)
            .field("broker_credentials", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BrokerRole {
    Host,
    Mobile,
}

impl std::fmt::Display for BrokerRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host => formatter.write_str("host"),
            Self::Mobile => formatter.write_str("mobile"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PairingStatus {
    Active,
    Revoked,
    RepairRequired,
    Suspended,
}

#[derive(Debug, Clone, Deserialize)]
struct ContractDeviceSummary {
    device_id: String,
    label: String,
    created_at_ms: u64,
    last_seen_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ContractBrokerEndpoint {
    endpoint: String,
    provider: ContractBrokerProvider,
    region: String,
    authorizer_name: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContractBrokerProvider {
    AwsIotCore,
}

#[derive(Clone, Deserialize)]
struct ContractBrokerCredentials {
    grant_id: String,
    client_id: String,
    connect: ContractBrokerConnect,
    scope: ContractBrokerCredentialScope,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

impl std::fmt::Debug for ContractBrokerCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContractBrokerCredentials")
            .field("grant_id", &self.grant_id)
            .field("client_id", &self.client_id)
            .field("connect", &"<redacted>")
            .field("scope", &self.scope)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
struct ContractBrokerConnect {
    username: String,
    password: String,
    #[serde(default)]
    websocket_url: Option<protocol::BrokerUrl>,
    headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ContractBrokerConnect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContractBrokerConnect")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field(
                "websocket_url",
                &self.websocket_url.as_ref().map(|_| "<redacted>"),
            )
            .field("headers", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ContractBrokerCredentialScope {
    namespace: String,
    role: BrokerRole,
    publish: Vec<String>,
    subscribe: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ManagedErrorEnvelope {
    error: ManagedErrorBody,
}

#[derive(Debug, Deserialize)]
struct ManagedErrorBody {
    code: ManagedErrorCode,
    message: String,
    retryable: bool,
    state: Option<String>,
    paywall_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManagedErrorCode {
    InvalidRequest,
    InvalidTyggsAuth,
    MobileSessionRequired,
    PassRequired,
    Forbidden,
    NotFound,
    OfferAlreadyRedeemed,
    DuplicateDevice,
    OfferExpired,
    RepairRequired,
    PairingRevoked,
    VersionMismatch,
    BrokerUnavailable,
    ServiceUnavailable,
    RateLimited,
    Internal,
}

async fn parse_managed_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ManagedServiceError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(ManagedServiceError::transport)?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|err| {
            ManagedServiceError::new(
                MobileAccessErrorCode::ServiceUnavailable,
                format!("managed mobile service response was malformed: {err}"),
            )
        });
    }
    match serde_json::from_slice::<ManagedErrorEnvelope>(&bytes) {
        Ok(envelope) => Err(envelope.error.into_error()),
        Err(err) => Err(ManagedServiceError::new(
            MobileAccessErrorCode::ServiceUnavailable,
            format!(
                "managed mobile service returned HTTP {status} with malformed error body: {err}"
            ),
        )),
    }
}

impl ManagedErrorBody {
    fn into_error(self) -> ManagedServiceError {
        let code = match self.code {
            ManagedErrorCode::InvalidRequest => MobileAccessErrorCode::InvalidConfig,
            ManagedErrorCode::InvalidTyggsAuth => MobileAccessErrorCode::ServiceAuthFailed,
            ManagedErrorCode::MobileSessionRequired => MobileAccessErrorCode::ServiceAuthRequired,
            ManagedErrorCode::PassRequired => MobileAccessErrorCode::PassRequired,
            ManagedErrorCode::Forbidden | ManagedErrorCode::NotFound => {
                MobileAccessErrorCode::BrokerRejected
            }
            ManagedErrorCode::OfferAlreadyRedeemed => MobileAccessErrorCode::PairingRejected,
            ManagedErrorCode::DuplicateDevice => MobileAccessErrorCode::DuplicateDevice,
            ManagedErrorCode::OfferExpired => MobileAccessErrorCode::PairingExpired,
            ManagedErrorCode::RepairRequired => MobileAccessErrorCode::RepairRequired,
            ManagedErrorCode::PairingRevoked => MobileAccessErrorCode::RevokedDevice,
            ManagedErrorCode::VersionMismatch => MobileAccessErrorCode::VersionMismatch,
            ManagedErrorCode::BrokerUnavailable => MobileAccessErrorCode::BrokerUnavailable,
            ManagedErrorCode::ServiceUnavailable
            | ManagedErrorCode::RateLimited
            | ManagedErrorCode::Internal => MobileAccessErrorCode::ServiceUnavailable,
        };
        let mut message = self.message;
        if let Some(state) = self.state
            && !state.is_empty()
        {
            message = format!("{message} ({state})");
        }
        if self.retryable {
            message = format!("{message} Retryable.");
        }
        if self.paywall_url.is_some() && code == MobileAccessErrorCode::PassRequired {
            message = "A Tyggs Pass is required for Tyde mobile access.".to_owned();
        }
        ManagedServiceError::new(code, message)
    }
}

impl ContractBrokerEndpoint {
    fn into_protocol(self) -> Result<ManagedBrokerEndpoint, ManagedServiceError> {
        let provider = match self.provider {
            ContractBrokerProvider::AwsIotCore => ManagedBrokerProvider::AwsIotCore,
        };
        Ok(ManagedBrokerEndpoint {
            endpoint: BrokerUrl::new(self.endpoint).map_err(|err| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed service returned invalid broker endpoint: {err}"),
                )
            })?,
            provider,
            region: ManagedBrokerRegion::new(self.region).map_err(|err| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed service returned invalid broker region: {err}"),
                )
            })?,
            authorizer_name: ManagedBrokerAuthorizerName::new(self.authorizer_name).map_err(
                |err| {
                    ManagedServiceError::new(
                        MobileAccessErrorCode::ServiceUnavailable,
                        format!("managed service returned invalid broker authorizer: {err}"),
                    )
                },
            )?,
        })
    }
}

impl ContractBrokerCredentials {
    fn into_protocol(self) -> Result<ManagedBrokerCredentials, ManagedServiceError> {
        Ok(ManagedBrokerCredentials {
            grant_id: ManagedBrokerGrantId::new(self.grant_id).map_err(|err| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed service returned invalid broker grant id: {err}"),
                )
            })?,
            client_id: ManagedBrokerClientId::new(self.client_id).map_err(|err| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed service returned invalid broker client id: {err}"),
                )
            })?,
            connect: ManagedBrokerConnectAuth {
                username: Some(self.connect.username),
                password: Some(self.connect.password),
                websocket_url: self.connect.websocket_url,
                headers: self.connect.headers,
            },
            scope: ManagedBrokerCredentialScope {
                namespace: ManagedBrokerTopicNamespace::new(self.scope.namespace).map_err(
                    |err| {
                        ManagedServiceError::new(
                            MobileAccessErrorCode::ServiceUnavailable,
                            format!(
                                "managed service returned invalid broker topic namespace: {err}"
                            ),
                        )
                    },
                )?,
                role: match self.scope.role {
                    BrokerRole::Host => ManagedBrokerRole::Host,
                    BrokerRole::Mobile => ManagedBrokerRole::Mobile,
                },
                publish: self.scope.publish,
                subscribe: self.scope.subscribe,
            },
            issued_at_ms: self.issued_at_ms,
            expires_at_ms: self.expires_at_ms,
        })
    }
}

fn pairing_auth_header(
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
    role: BrokerRole,
    pairing_id: &str,
) -> Result<String, ManagedServiceError> {
    let nonce = Uuid::new_v4().to_string();
    let timestamp_ms = now_ms().map_err(|message| {
        ManagedServiceError::new(
            MobileAccessErrorCode::Internal,
            format!("failed to timestamp managed service request: {message}"),
        )
    })?;
    let body_sha256 = body_sha256_base64url(body);
    let signature = sign_pairing_request(PairingSignatureInput {
        secret,
        method,
        path,
        body_sha256: &body_sha256,
        nonce: &nonce,
        timestamp_ms,
        pairing_id,
        role,
    })?;
    Ok(format!(
        "v1;role={role};nonce={nonce};timestamp_ms={timestamp_ms};signature={signature}"
    ))
}

struct PairingSignatureInput<'a> {
    secret: &'a str,
    method: &'a str,
    path: &'a str,
    body_sha256: &'a str,
    nonce: &'a str,
    timestamp_ms: u64,
    pairing_id: &'a str,
    role: BrokerRole,
}

fn sign_pairing_request(input: PairingSignatureInput<'_>) -> Result<String, ManagedServiceError> {
    if input.secret.trim().is_empty() {
        return Err(ManagedServiceError::new(
            MobileAccessErrorCode::RepairRequired,
            "managed mobile host pairing secret is missing",
        ));
    }
    let mut mac = HmacSha256::new_from_slice(input.secret.as_bytes()).map_err(|err| {
        ManagedServiceError::new(
            MobileAccessErrorCode::Internal,
            format!("failed to initialize managed service request signer: {err}"),
        )
    })?;
    mac.update(PAIRING_HMAC_PREFIX.as_bytes());
    mac.update(b"\n");
    mac.update(input.method.as_bytes());
    mac.update(b"\n");
    mac.update(input.path.as_bytes());
    mac.update(b"\n");
    mac.update(input.body_sha256.as_bytes());
    mac.update(b"\n");
    mac.update(input.nonce.as_bytes());
    mac.update(b"\n");
    mac.update(input.timestamp_ms.to_string().as_bytes());
    mac.update(b"\n");
    mac.update(input.pairing_id.as_bytes());
    mac.update(b"\n");
    mac.update(input.role.to_string().as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn body_sha256_base64url(body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

impl MobileAccessActor {
    fn new(
        host: HostHandle,
        tx: mpsc::UnboundedSender<MobileAccessCommand>,
        rx: mpsc::UnboundedReceiver<MobileAccessCommand>,
        init: MobileAccessInit,
    ) -> Result<Self, String> {
        let mut pairings = init.pairings_store.get()?;
        if pairings.normalize_startup_runtime_state() {
            init.pairings_store.save(&pairings)?;
        }
        let managed_service = ManagedMobileServiceClient::new(init.managed_service_base_url)?;
        let legacy_repair_changed =
            mark_legacy_pairings_repair_required(&mut pairings, &init.initial_settings);
        if legacy_repair_changed {
            init.pairings_store.save(&pairings)?;
        }
        let broker_status = if init.initial_settings.enable_mobile_connections {
            initial_enabled_broker_status(&pairings, &init.initial_settings)
        } else {
            MobileBrokerStatus::Disabled
        };
        let pairing = match &pairings.active_pairing {
            Some(active) => MobilePairingState::Active {
                offer_id: active.offer_id.clone(),
                expires_at_ms: active
                    .created_at_ms
                    .saturating_add(init.pairing_ttl.as_millis() as u64),
            },
            None => MobilePairingState::Idle,
        };

        Ok(Self {
            host,
            tx,
            rx,
            pairings_store: init.pairings_store,
            managed_service,
            settings: init.initial_settings,
            pairing_ttl: init.pairing_ttl,
            pairings,
            broker_status,
            pairing,
            subscribers: HashMap::new(),
            bootstrap_subscribers: HashMap::new(),
            active_requester: None,
            accept_tasks: HashMap::new(),
            connected_tasks: HashMap::new(),
            pairing_ttl_task: None,
            offer_poll_task: None,
            next_connection_instance_id: 0,
            mobile_pairings_lease: None,
        })
    }

    async fn run(mut self) {
        if self.settings.enable_mobile_connections {
            self.enable_mobile_access().await;
        }

        while let Some(command) = self.rx.recv().await {
            match command {
                MobileAccessCommand::Shutdown => {
                    self.shutdown_runtime_state().await;
                    break;
                }
                MobileAccessCommand::RegisterBootstrapSubscriber { stream, reply } => {
                    let result = self.register_bootstrap_subscriber(stream).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::ActivateBootstrapSubscriber { path } => {
                    self.activate_bootstrap_subscriber(path).await;
                }
                MobileAccessCommand::UnregisterSubscriber { path } => {
                    self.unregister_subscriber(&path).await;
                }
                MobileAccessCommand::SettingsChanged { settings } => {
                    self.apply_settings(settings).await;
                }
                MobileAccessCommand::StartPairing { requester } => {
                    self.start_pairing(requester).await;
                }
                MobileAccessCommand::CancelPairing { offer_id } => {
                    self.cancel_pairing(&offer_id).await;
                }
                MobileAccessCommand::RevokeDevice { device_id, reply } => {
                    let result = self.revoke_device(&device_id).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::RenameDevice {
                    device_id,
                    label,
                    reply,
                } => {
                    let result = self.rename_device(&device_id, label).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::PairingTransportConnected { offer_id, stream } => {
                    self.pairing_transport_connected(&offer_id, stream).await;
                }
                MobileAccessCommand::DeviceTransportConnected { device_id, stream } => {
                    self.device_transport_connected(&device_id, stream).await;
                }
                MobileAccessCommand::PairingOfferRedeemed { offer_id, handoff } => {
                    self.pairing_offer_redeemed(&offer_id, *handoff).await;
                }
                MobileAccessCommand::PairingOfferTerminal { offer_id, state } => {
                    self.pairing_offer_terminal(&offer_id, state).await;
                }
                MobileAccessCommand::PairingFailed {
                    offer_id,
                    code,
                    message,
                } => {
                    self.pairing_failed(&offer_id, code, message).await;
                }
                MobileAccessCommand::DeviceAcceptFailed {
                    device_id,
                    code,
                    message,
                } => {
                    self.device_accept_failed(&device_id, code, message).await;
                }
                MobileAccessCommand::PairingExpired { offer_id } => {
                    self.pairing_expired(&offer_id).await;
                }
                MobileAccessCommand::PairingGraceElapsed { offer_id } => {
                    self.pairing_grace_elapsed(&offer_id).await;
                }
                MobileAccessCommand::DeviceDisconnected {
                    device_id,
                    connection_instance_id,
                } => {
                    self.device_disconnected(&device_id, connection_instance_id)
                        .await;
                }
            }
        }
    }

    async fn register_bootstrap_subscriber(
        &mut self,
        stream: Stream,
    ) -> Result<MobileAccessStatePayload, StreamClosed> {
        let path = stream.path().clone();
        let snapshot = self.state_payload();
        self.subscribers.remove(&path);
        self.bootstrap_subscribers.insert(
            path,
            PendingBootstrapSubscriber {
                stream,
                snapshot: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }

    async fn activate_bootstrap_subscriber(&mut self, path: StreamPath) {
        let Some(pending) = self.bootstrap_subscribers.remove(&path) else {
            return;
        };
        let current = self.state_payload();
        if current != pending.snapshot
            && send_mobile_access_state(&pending.stream, &current)
                .await
                .is_err()
        {
            return;
        }
        self.subscribers.insert(path, pending.stream);
    }

    async fn unregister_subscriber(&mut self, path: &StreamPath) {
        self.subscribers.remove(path);
        self.bootstrap_subscribers.remove(path);
        if self.active_requester.as_ref() == Some(path) {
            if let Some(active) = self.pairings.active_pairing.clone() {
                self.pairing_failed(
                    &active.offer_id,
                    MobileAccessErrorCode::PairingRejected,
                    "pairing requester disconnected".to_owned(),
                )
                .await;
            }
            self.active_requester = None;
        }
    }

    async fn shutdown_runtime_state(&mut self) {
        self.abort_all_tasks();
        self.mobile_pairings_lease = None;
        self.subscribers.clear();
        self.bootstrap_subscribers.clear();
    }

    async fn apply_settings(&mut self, settings: HostSettings) {
        let was_enabled = self.settings.enable_mobile_connections;
        let old_url = self.settings.mobile_broker_url.clone();
        self.settings = settings;
        let url_changed = old_url != self.settings.mobile_broker_url;
        if mark_legacy_pairings_repair_required(&mut self.pairings, &self.settings)
            && let Err(message) = self.pairings_store.save(&self.pairings)
        {
            self.broker_status = MobileBrokerStatus::Error {
                broker_url: self.settings.mobile_broker_url.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.fan_out_state().await;
            return;
        }

        if !self.settings.enable_mobile_connections {
            if !was_enabled && !url_changed {
                return;
            }
            self.disable_mobile_access().await;
            self.fan_out_state().await;
            return;
        }

        if !was_enabled || url_changed {
            self.enable_mobile_access().await;
            self.fan_out_state().await;
        }
    }

    async fn enable_mobile_access(&mut self) {
        let endpoint = match dev_broker_endpoint(self.settings.mobile_broker_url.as_ref()) {
            Ok(Some(endpoint)) => {
                self.broker_status = MobileBrokerStatus::Online {
                    broker_url: endpoint.url.clone(),
                };
                Some(endpoint)
            }
            Ok(None) => {
                self.broker_status = managed_broker_status_for_pairings(&self.pairings);
                None
            }
            Err(message) => {
                self.abort_all_tasks();
                self.mobile_pairings_lease = None;
                self.broker_status = MobileBrokerStatus::Error {
                    broker_url: self.settings.mobile_broker_url.clone(),
                    code: MobileAccessErrorCode::InvalidConfig,
                    message,
                };
                return;
            }
        };

        if self.mobile_pairings_lease.is_none() {
            match MobilePairingsLease::try_acquire(self.pairings_store.path()) {
                Ok(lease) => {
                    self.mobile_pairings_lease = Some(lease);
                }
                Err(message) => {
                    self.abort_all_tasks();
                    self.broker_status = MobileBrokerStatus::Error {
                        broker_url: endpoint
                            .as_ref()
                            .map(|endpoint| endpoint.url.clone())
                            .or_else(|| first_managed_broker_url(&self.pairings)),
                        code: MobileAccessErrorCode::BrokerUnavailable,
                        message,
                    };
                    return;
                }
            }
        }

        if endpoint.is_some() {
            self.spawn_active_pairing_accept_if_needed();
            self.spawn_device_accepts_if_needed();
        } else {
            self.spawn_managed_device_accepts_if_needed();
        }
    }

    async fn disable_mobile_access(&mut self) {
        self.abort_all_tasks();
        self.mobile_pairings_lease = None;
        self.broker_status = MobileBrokerStatus::Disabled;
        self.pairing = MobilePairingState::Idle;
        self.active_requester = None;
        if self.pairings.active_pairing.take().is_some() {
            let _ = self.pairings_store.save(&self.pairings);
        }
        for record in &mut self.pairings.devices {
            if record.state == MobileDeviceState::Connected {
                record.state = MobileDeviceState::Paired;
            }
        }
        let _ = self.pairings_store.save(&self.pairings);
    }

    async fn start_pairing(&mut self, requester: StreamPath) {
        let offer_id = match new_offer_id() {
            Ok(offer_id) => offer_id,
            Err(message) => {
                tracing::warn!(error = %message, "failed to create mobile pairing offer id");
                return;
            }
        };

        if !self.settings.enable_mobile_connections {
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::InvalidConfig,
                message: "mobile connections are disabled".to_owned(),
            };
            self.fan_out_state().await;
            return;
        }

        match dev_broker_endpoint(self.settings.mobile_broker_url.as_ref()) {
            Ok(Some(broker)) => {
                self.start_dev_pairing(requester, offer_id, broker).await;
            }
            Ok(None) => {
                self.start_managed_pairing(requester).await;
            }
            Err(message) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::InvalidConfig,
                    message,
                };
                self.fan_out_state().await;
            }
        }
    }

    async fn start_dev_pairing(
        &mut self,
        requester: StreamPath,
        offer_id: MobilePairingOfferId,
        broker: BrokerEndpoint,
    ) {
        let created_at_ms = match now_ms() {
            Ok(now) => now,
            Err(message) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::Internal,
                    message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        let expires_at_ms = created_at_ms.saturating_add(self.pairing_ttl.as_millis() as u64);
        let room = RoomId::random();
        let psk = PreSharedKey::random();
        let key_fingerprint = key_fingerprint(&psk);
        let credential = ActiveMobilePairingCredential {
            offer_id: offer_id.clone(),
            broker: broker.clone(),
            room,
            psk,
            created_at_ms,
            key_fingerprint,
            managed: None,
        };
        let mut qr_payload = MobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            broker,
            credential.room,
            credential.psk.clone(),
            "Tyde Host".to_owned(),
        );
        // Advertise the host's real build version so the web/PWA loader can pick
        // the matching versioned bundle.
        qr_payload.release_version = crate::host_release_version();
        let qr_uri = match qr_payload.to_pairing_url() {
            Ok(uri) => MobilePairingQrUri(uri),
            Err(err) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::Internal,
                    message: format!("failed to encode pairing QR payload: {err}"),
                };
                self.fan_out_state().await;
                return;
            }
        };

        self.cancel_active_pairing_without_state();
        self.pairings.active_pairing = Some(credential.clone());
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.fan_out_state().await;
            return;
        }
        self.active_requester = Some(requester.clone());
        self.pairing = MobilePairingState::Active {
            offer_id: offer_id.clone(),
            expires_at_ms,
        };
        self.spawn_pairing_accept(credential);
        self.schedule_pairing_ttl(offer_id.clone(), expires_at_ms);
        self.fan_out_state().await;

        let Some(stream) = self.subscribers.get(&requester).cloned() else {
            return;
        };
        let offer = MobilePairingOfferPayload {
            offer_id,
            qr_uri,
            expires_at_ms,
        };
        if send_mobile_pairing_offer(&stream, &offer).await.is_err() {
            self.subscribers.remove(&requester);
        }
    }

    async fn start_managed_pairing(&mut self, requester: StreamPath) {
        let created_at_ms = match now_ms() {
            Ok(now) => now,
            Err(message) => {
                let offer_id = new_offer_id()
                    .unwrap_or_else(|_| MobilePairingOfferId("failed-managed-offer".to_owned()));
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::Internal,
                    message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        let host_label = "Tyde Host".to_owned();
        let host_release_version = match host_release_version_for_qr() {
            Ok(version) => version,
            Err(message) => {
                let offer_id = new_offer_id()
                    .unwrap_or_else(|_| MobilePairingOfferId("failed-managed-offer".to_owned()));
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::Internal,
                    message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        let request = CreateHostOfferRequest {
            host_label: host_label.clone(),
            host_release_version: host_release_version.to_string(),
            protocol_version: PROTOCOL_VERSION,
            transport_protocol_version: mqtt_transport::MQTT_TRANSPORT_PROTOCOL_VERSION,
            host_nonce: Uuid::new_v4().to_string(),
        };
        let response = match self.managed_service.create_host_offer(request).await {
            Ok(response) => response,
            Err(error) => {
                let offer_id = new_offer_id()
                    .unwrap_or_else(|_| MobilePairingOfferId("failed-managed-offer".to_owned()));
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: error.code,
                    message: error.message,
                };
                self.broker_status = MobileBrokerStatus::Error {
                    broker_url: None,
                    code: error.code,
                    message: "managed mobile service could not create a pairing offer".to_owned(),
                };
                self.fan_out_state().await;
                return;
            }
        };
        if response.status != HostOfferStatus::Pending {
            let offer_id = MobilePairingOfferId::new(response.offer_id)
                .unwrap_or_else(|_| MobilePairingOfferId("invalid-managed-offer".to_owned()));
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::ServiceUnavailable,
                message: format!(
                    "managed mobile service returned non-pending offer status {:?}",
                    response.status
                ),
            };
            self.fan_out_state().await;
            return;
        }
        let offer_id = match MobilePairingOfferId::new(response.offer_id) {
            Ok(offer_id) => offer_id,
            Err(err) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id: MobilePairingOfferId("invalid-managed-offer".to_owned()),
                    code: MobileAccessErrorCode::ServiceUnavailable,
                    message: format!("managed mobile service returned invalid offer id: {err}"),
                };
                self.fan_out_state().await;
                return;
            }
        };
        let broker = match response.broker.into_protocol() {
            Ok(broker) => broker,
            Err(error) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: error.code,
                    message: error.message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        let host_broker_credentials = match response.host_broker_credentials.into_protocol() {
            Ok(credentials) => credentials,
            Err(error) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: error.code,
                    message: error.message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        let room = RoomId::random();
        let psk = PreSharedKey::random();
        let key_fingerprint = key_fingerprint(&psk);
        let qr_payload = ManagedMobilePairingQrPayload::new_with_rendezvous(
            ManagedMobilePairingQrPayloadParams {
                protocol_version: PROTOCOL_VERSION,
                release_version: host_release_version,
                offer_id: offer_id.clone(),
                offer_secret: response.offer_secret,
                broker: broker.clone(),
                room,
                psk: psk.clone(),
                host_label,
                expires_at_ms: response.expires_at_ms,
            },
        );
        let pairing_url = match qr_payload.to_pairing_url() {
            Ok(url) => url,
            Err(err) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::InvalidPairingQr,
                    message: format!("failed to encode managed pairing QR payload: {err}"),
                };
                self.fan_out_state().await;
                return;
            }
        };
        let broker_endpoint = BrokerEndpoint {
            url: broker.endpoint.clone(),
            auth: BrokerAuth::Anonymous,
        };
        let credential = ActiveMobilePairingCredential {
            offer_id: offer_id.clone(),
            broker: broker_endpoint,
            room,
            psk,
            created_at_ms,
            key_fingerprint,
            managed: Some(ActiveManagedMobilePairingCredential {
                host_offer_token: response.host_offer_token,
                pairing_url: pairing_url.clone(),
                broker: broker.clone(),
                host_broker_credentials,
                expires_at_ms: response.expires_at_ms,
                handoff: None,
            }),
        };
        self.cancel_active_pairing_without_state();
        self.pairings.active_pairing = Some(credential.clone());
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.fan_out_state().await;
            return;
        }
        self.active_requester = Some(requester.clone());
        self.pairing = MobilePairingState::Active {
            offer_id: offer_id.clone(),
            expires_at_ms: response.expires_at_ms,
        };
        self.broker_status = MobileBrokerStatus::Connecting {
            broker_url: broker.endpoint.clone(),
        };
        self.schedule_pairing_ttl(offer_id.clone(), response.expires_at_ms);
        self.spawn_offer_poll(credential);
        self.fan_out_state().await;

        let Some(stream) = self.subscribers.get(&requester).cloned() else {
            return;
        };
        let offer = MobilePairingOfferPayload {
            offer_id,
            qr_uri: MobilePairingQrUri(pairing_url),
            expires_at_ms: response.expires_at_ms,
        };
        if send_mobile_pairing_offer(&stream, &offer).await.is_err() {
            self.subscribers.remove(&requester);
        }
    }

    async fn cancel_pairing(&mut self, offer_id: &MobilePairingOfferId) {
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        if let Some(managed) = active.managed.as_ref()
            && let Err(error) = self
                .managed_service
                .cancel_host_offer(offer_id, &managed.host_offer_token)
                .await
        {
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: error.code,
                message: error.message,
            };
            self.fan_out_state().await;
            return;
        }
        self.cancel_active_pairing_without_state();
        self.pairing = MobilePairingState::Cancelled {
            offer_id: offer_id.clone(),
        };
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn revoke_device(
        &mut self,
        device_id: &MobileDeviceId,
    ) -> Result<(), MobileAccessCommandFailure> {
        let Some(index) = self
            .pairings
            .devices
            .iter()
            .position(|record| &record.device_id == device_id)
        else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        };
        self.pairings.devices.remove(index);
        if let Some(task) = self
            .accept_tasks
            .remove(&AcceptTaskKey::Device(device_id.clone()))
        {
            task.abort();
        }
        if let Some(task) = self.connected_tasks.remove(device_id) {
            task.task.abort();
        }
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;
        self.fan_out_state().await;
        Ok(())
    }

    async fn rename_device(
        &mut self,
        device_id: &MobileDeviceId,
        label: String,
    ) -> Result<(), MobileAccessCommandFailure> {
        if label.trim().is_empty() {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::InvalidConfig,
                "mobile device label must not be empty",
            ));
        }
        let Some(record) = self
            .pairings
            .devices
            .iter_mut()
            .find(|record| &record.device_id == device_id)
        else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        };
        record.label = label;
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;
        self.fan_out_state().await;
        Ok(())
    }

    async fn pairing_transport_connected(
        &mut self,
        offer_id: &MobilePairingOfferId,
        stream: EnvelopeStream,
    ) {
        let Some(active) = self.pairings.active_pairing.take() else {
            return;
        };
        if &active.offer_id != offer_id {
            self.pairings.active_pairing = Some(active);
            return;
        }
        self.accept_tasks
            .remove(&AcceptTaskKey::Pairing(offer_id.clone()));
        if let Some(task) = self.pairing_ttl_task.take() {
            task.abort();
        }
        let device_id = match new_device_id() {
            Ok(device_id) => device_id,
            Err(message) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id: offer_id.clone(),
                    code: MobileAccessErrorCode::Internal,
                    message,
                };
                self.pairings.active_pairing = Some(active);
                self.fan_out_state().await;
                return;
            }
        };
        let now = now_ms().unwrap_or(active.created_at_ms);
        let managed_record = match active
            .managed
            .as_ref()
            .and_then(|managed| managed.handoff.as_ref())
        {
            Some(handoff) => {
                let device_id = handoff.device_id.clone();
                let record = MobilePairingRecord {
                    device_id,
                    broker: BrokerEndpoint {
                        url: handoff.broker.endpoint.clone(),
                        auth: BrokerAuth::Anonymous,
                    },
                    room: active.room,
                    psk: active.psk.clone(),
                    label: handoff.device_label.clone(),
                    created_at_ms: handoff.device_created_at_ms,
                    last_seen_at_ms: handoff.device_last_seen_at_ms.or(Some(now)),
                    state: MobileDeviceState::Connected,
                    key_fingerprint: active.key_fingerprint.clone(),
                    managed: Some(ManagedMobilePairingCredential {
                        pairing_id: handoff.pairing_id.clone(),
                        host_pairing_secret: handoff.host_pairing_secret.clone(),
                        broker: handoff.broker.clone(),
                    }),
                };
                Some((record.device_id.clone(), record))
            }
            None if active.managed.is_some() => {
                self.pairing = MobilePairingState::Failed {
                    offer_id: offer_id.clone(),
                    code: MobileAccessErrorCode::RepairRequired,
                    message: "managed pairing completed without tycode.dev handoff".to_owned(),
                };
                self.pairings.active_pairing = Some(active);
                self.fan_out_state().await;
                return;
            }
            None => None,
        };
        let record = MobilePairingRecord {
            device_id: device_id.clone(),
            broker: active.broker,
            room: active.room,
            psk: active.psk,
            label: "Mobile device".to_owned(),
            created_at_ms: active.created_at_ms,
            last_seen_at_ms: Some(now),
            state: MobileDeviceState::Connected,
            key_fingerprint: active.key_fingerprint,
            managed: None,
        };
        let (device_id, record) = managed_record.unwrap_or((device_id, record));
        self.pairings.devices.push(record);
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.fan_out_state().await;
            return;
        }
        self.active_requester = None;
        self.pairing = MobilePairingState::Consumed {
            offer_id: offer_id.clone(),
        };
        self.spawn_connected_bridge(device_id.clone(), stream);
        self.spawn_device_accept(device_id);
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn device_transport_connected(
        &mut self,
        device_id: &MobileDeviceId,
        stream: EnvelopeStream,
    ) {
        self.accept_tasks
            .remove(&AcceptTaskKey::Device(device_id.clone()));
        let now = now_ms().ok();
        if !self.mark_device_connected(device_id, now) {
            return;
        }
        self.spawn_connected_bridge(device_id.clone(), stream);
        self.spawn_device_accept(device_id.clone());
        self.fan_out_state().await;
    }

    async fn pairing_offer_redeemed(
        &mut self,
        offer_id: &MobilePairingOfferId,
        handoff: ManagedMobilePairingHandoff,
    ) {
        let Some(active) = self.pairings.active_pairing.as_mut() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        let Some(managed) = active.managed.as_mut() else {
            return;
        };
        managed.handoff = Some(handoff);
        managed.host_broker_credentials = managed
            .handoff
            .as_ref()
            .map(|handoff| handoff.host_broker_credentials.clone())
            .unwrap_or_else(|| managed.host_broker_credentials.clone());
        managed.broker = managed
            .handoff
            .as_ref()
            .map(|handoff| handoff.broker.clone())
            .unwrap_or_else(|| managed.broker.clone());
        active.broker = BrokerEndpoint {
            url: managed.broker.endpoint.clone(),
            auth: BrokerAuth::Anonymous,
        };
        let broker_url = managed.broker.endpoint.clone();
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.fan_out_state().await;
            return;
        }
        if let Some(task) = self.offer_poll_task.take() {
            task.abort();
        }
        self.broker_status = MobileBrokerStatus::Connecting { broker_url };
        self.spawn_active_pairing_accept_if_needed();
        self.fan_out_state().await;
    }

    async fn pairing_offer_terminal(
        &mut self,
        offer_id: &MobilePairingOfferId,
        state: ManagedOfferTerminalState,
    ) {
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        match state {
            ManagedOfferTerminalState::Expired => {
                self.pairing_expired(offer_id).await;
            }
            ManagedOfferTerminalState::Cancelled => {
                self.cancel_active_pairing_without_state();
                self.pairing = MobilePairingState::Cancelled {
                    offer_id: offer_id.clone(),
                };
                self.fan_out_state().await;
                self.schedule_pairing_grace(offer_id.clone());
            }
            ManagedOfferTerminalState::Failed(message) => {
                self.pairing_failed(offer_id, MobileAccessErrorCode::ServiceUnavailable, message)
                    .await;
            }
        }
    }

    fn mark_device_connected(&mut self, device_id: &MobileDeviceId, now: Option<u64>) -> bool {
        let Some(record) = self
            .pairings
            .devices
            .iter_mut()
            .find(|record| &record.device_id == device_id)
        else {
            return false;
        };
        record.state = MobileDeviceState::Connected;
        if let Some(now) = now {
            record.last_seen_at_ms = Some(now);
        }
        let broker_url = record.broker.url.clone();
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            tracing::warn!(error = %message, "failed to persist mobile device connection state");
        }
        self.broker_status = MobileBrokerStatus::Online { broker_url };
        true
    }

    async fn pairing_failed(
        &mut self,
        offer_id: &MobilePairingOfferId,
        code: MobileAccessErrorCode,
        message: String,
    ) {
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        self.cancel_active_pairing_without_state();
        self.pairing = MobilePairingState::Failed {
            offer_id: offer_id.clone(),
            code,
            message,
        };
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn device_accept_failed(
        &mut self,
        device_id: &MobileDeviceId,
        code: MobileAccessErrorCode,
        message: String,
    ) {
        let terminal = terminal_device_accept_error(code);
        if terminal {
            self.accept_tasks
                .remove(&AcceptTaskKey::Device(device_id.clone()));
        }
        if !self.settings.enable_mobile_connections {
            return;
        }
        if self.connected_tasks.contains_key(device_id) {
            tracing::warn!(
                device_id = %device_id,
                code = ?code,
                message = %message,
                "mobile reconnect listener failed while device data connection is active"
            );
            return;
        }
        if self
            .pairings
            .devices
            .iter()
            .any(|record| &record.device_id == device_id)
        {
            if matches!(
                code,
                MobileAccessErrorCode::RepairRequired | MobileAccessErrorCode::RevokedDevice
            ) {
                if let Some(record) = self
                    .pairings
                    .devices
                    .iter_mut()
                    .find(|record| &record.device_id == device_id)
                {
                    record.state = if code == MobileAccessErrorCode::RevokedDevice {
                        MobileDeviceState::Revoked
                    } else {
                        MobileDeviceState::RepairRequired
                    };
                }
                if let Err(message) = self.pairings_store.save(&self.pairings) {
                    tracing::warn!(error = %message, "failed to persist mobile device repair state");
                }
            }
            self.broker_status = MobileBrokerStatus::Error {
                broker_url: self
                    .pairings
                    .devices
                    .iter()
                    .find(|record| &record.device_id == device_id)
                    .map(|record| record.broker.url.clone()),
                code,
                message,
            };
            self.fan_out_state().await;
        }
    }

    async fn pairing_expired(&mut self, offer_id: &MobilePairingOfferId) {
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        self.cancel_active_pairing_without_state();
        self.pairing = MobilePairingState::Expired {
            offer_id: offer_id.clone(),
        };
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn pairing_grace_elapsed(&mut self, offer_id: &MobilePairingOfferId) {
        match &self.pairing {
            MobilePairingState::Consumed { offer_id: current }
            | MobilePairingState::Expired { offer_id: current }
            | MobilePairingState::Cancelled { offer_id: current }
            | MobilePairingState::Failed {
                offer_id: current, ..
            } if current == offer_id => {
                self.pairing = MobilePairingState::Idle;
                self.fan_out_state().await;
            }
            _ => {}
        }
    }

    async fn device_disconnected(&mut self, device_id: &MobileDeviceId, instance_id: u64) {
        let Some(current) = self.connected_tasks.get(device_id) else {
            return;
        };
        if current.instance_id != instance_id {
            tracing::info!(
                device_id = %device_id,
                instance_id,
                current_instance_id = current.instance_id,
                "ignoring stale mobile device disconnect"
            );
            return;
        }
        self.connected_tasks.remove(device_id);
        if let Some(record) = self
            .pairings
            .devices
            .iter_mut()
            .find(|record| &record.device_id == device_id)
        {
            if record.state == MobileDeviceState::Connected {
                record.state = MobileDeviceState::Paired;
            }
            if let Err(message) = self.pairings_store.save(&self.pairings) {
                tracing::warn!(error = %message, "failed to persist mobile device disconnect state");
            }
        }
        if self.settings.enable_mobile_connections {
            self.spawn_device_accept(device_id.clone());
        }
        self.fan_out_state().await;
    }

    fn spawn_active_pairing_accept_if_needed(&mut self) {
        let Some(active) = self.pairings.active_pairing.clone() else {
            return;
        };
        self.spawn_pairing_accept(active);
    }

    fn spawn_device_accepts_if_needed(&mut self) {
        let device_ids: Vec<MobileDeviceId> = self
            .pairings
            .devices
            .iter()
            .filter(|record| record.state != MobileDeviceState::RepairRequired)
            .filter(|record| record.state != MobileDeviceState::Revoked)
            .filter(|record| record.managed.is_none())
            .map(|record| record.device_id.clone())
            .collect();
        for device_id in device_ids {
            self.spawn_device_accept(device_id);
        }
    }

    fn spawn_managed_device_accepts_if_needed(&mut self) {
        let device_ids: Vec<MobileDeviceId> = self
            .pairings
            .devices
            .iter()
            .filter(|record| record.state != MobileDeviceState::RepairRequired)
            .filter(|record| record.state != MobileDeviceState::Revoked)
            .filter(|record| record.managed.is_some())
            .map(|record| record.device_id.clone())
            .collect();
        for device_id in device_ids {
            self.spawn_device_accept(device_id);
        }
    }

    fn spawn_pairing_accept(&mut self, credential: ActiveMobilePairingCredential) {
        let key = AcceptTaskKey::Pairing(credential.offer_id.clone());
        if self.accept_tasks.contains_key(&key) {
            return;
        }
        if credential
            .managed
            .as_ref()
            .is_some_and(|managed| managed.handoff.is_none())
        {
            return;
        }
        let task = spawn_pairing_accept_task(self.tx.clone(), credential);
        self.accept_tasks.insert(key, task);
    }

    fn spawn_device_accept(&mut self, device_id: MobileDeviceId) {
        let key = AcceptTaskKey::Device(device_id.clone());
        if self.accept_tasks.contains_key(&key) {
            return;
        }
        let Some(record) = self
            .pairings
            .devices
            .iter()
            .find(|record| record.device_id == device_id)
            .cloned()
        else {
            return;
        };
        if record.state == MobileDeviceState::RepairRequired
            || record.state == MobileDeviceState::Revoked
        {
            return;
        }
        let task = spawn_device_accept_task(self.tx.clone(), self.managed_service.clone(), record);
        self.accept_tasks.insert(key, task);
    }

    fn spawn_connected_bridge(&mut self, device_id: MobileDeviceId, stream: EnvelopeStream) {
        if let Some(previous) = self.connected_tasks.remove(&device_id) {
            previous.task.abort();
        }
        let instance_id = self.allocate_connection_instance_id();
        let task = tokio::spawn(bridge_authenticated_mobile(
            self.host.clone(),
            self.tx.clone(),
            device_id.clone(),
            instance_id,
            stream,
        ));
        self.connected_tasks
            .insert(device_id, ConnectedMobileTask { instance_id, task });
    }

    fn spawn_offer_poll(&mut self, credential: ActiveMobilePairingCredential) {
        if let Some(task) = self.offer_poll_task.take() {
            task.abort();
        }
        let Some(managed) = credential.managed.clone() else {
            return;
        };
        self.offer_poll_task = Some(spawn_offer_poll_task(
            self.tx.clone(),
            self.managed_service.clone(),
            credential.offer_id,
            managed.host_offer_token,
        ));
    }

    fn allocate_connection_instance_id(&mut self) -> u64 {
        let instance_id = self.next_connection_instance_id;
        self.next_connection_instance_id = self
            .next_connection_instance_id
            .checked_add(1)
            .unwrap_or_else(|| {
                tracing::warn!("mobile connection instance id overflow; wrapping to zero");
                0
            });
        instance_id
    }

    fn schedule_pairing_ttl(&mut self, offer_id: MobilePairingOfferId, expires_at_ms: u64) {
        if let Some(task) = self.pairing_ttl_task.take() {
            task.abort();
        }
        let tx = self.tx.clone();
        self.pairing_ttl_task = Some(tokio::spawn(async move {
            let sleep_for = match now_ms() {
                Ok(now) if expires_at_ms > now => Duration::from_millis(expires_at_ms - now),
                _ => Duration::ZERO,
            };
            sleep(sleep_for).await;
            let _ = tx.send(MobileAccessCommand::PairingExpired { offer_id });
        }));
    }

    fn schedule_pairing_grace(&self, offer_id: MobilePairingOfferId) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            sleep(PAIRING_TERMINAL_GRACE).await;
            let _ = tx.send(MobileAccessCommand::PairingGraceElapsed { offer_id });
        });
    }

    fn cancel_active_pairing_without_state(&mut self) {
        if let Some(active) = self.pairings.active_pairing.take()
            && let Some(task) = self
                .accept_tasks
                .remove(&AcceptTaskKey::Pairing(active.offer_id.clone()))
        {
            task.abort();
        }
        if let Some(task) = self.pairing_ttl_task.take() {
            task.abort();
        }
        if let Some(task) = self.offer_poll_task.take() {
            task.abort();
        }
        self.active_requester = None;
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            tracing::warn!(error = %message, "failed to persist active mobile pairing cancellation");
        }
    }

    fn abort_all_tasks(&mut self) {
        for (_, task) in self.accept_tasks.drain() {
            task.abort();
        }
        for (_, task) in self.connected_tasks.drain() {
            task.task.abort();
        }
        if let Some(task) = self.pairing_ttl_task.take() {
            task.abort();
        }
        if let Some(task) = self.offer_poll_task.take() {
            task.abort();
        }
    }

    fn state_payload(&self) -> MobileAccessStatePayload {
        let connected: HashSet<MobileDeviceId> = self.connected_tasks.keys().cloned().collect();
        let mut paired_devices = self.pairings.summaries();
        for summary in &mut paired_devices {
            if connected.contains(&summary.device_id) && summary.state != MobileDeviceState::Revoked
            {
                summary.state = MobileDeviceState::Connected;
            }
        }
        MobileAccessStatePayload {
            broker_status: self.broker_status.clone(),
            pairing: self.pairing.clone(),
            paired_devices,
        }
    }

    async fn fan_out_state(&mut self) {
        let payload = self.state_payload();
        let paths: Vec<StreamPath> = self.subscribers.keys().cloned().collect();
        let mut dead_paths = Vec::new();
        for path in paths {
            let Some(stream) = self.subscribers.get(&path).cloned() else {
                continue;
            };
            if send_mobile_access_state(&stream, &payload).await.is_err() {
                dead_paths.push(path);
            }
        }
        for path in dead_paths {
            self.subscribers.remove(&path);
        }
    }
}

fn spawn_pairing_accept_task(
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    credential: ActiveMobilePairingCredential,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let offer_id = credential.offer_id.clone();
        let result = connect_mobile_record_stream(
            credential.broker.clone(),
            credential.managed.as_ref().map(|managed| {
                (
                    managed.broker.clone(),
                    managed.host_broker_credentials.clone(),
                )
            }),
            credential.room,
            credential.psk.clone(),
        )
        .await;
        match result {
            Ok(stream) => {
                let _ =
                    tx.send(MobileAccessCommand::PairingTransportConnected { offer_id, stream });
            }
            Err(error) => {
                let _ = tx.send(MobileAccessCommand::PairingFailed {
                    offer_id,
                    code: error.code,
                    message: error.message,
                });
            }
        }
    })
}

fn spawn_device_accept_task(
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    managed_service: ManagedMobileServiceClient,
    record: MobilePairingRecord,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = ACCEPT_RECONNECT_INITIAL;
        loop {
            match connect_mobile_device_stream(&managed_service, &record).await {
                Ok(stream) => {
                    let _ = tx.send(MobileAccessCommand::DeviceTransportConnected {
                        device_id: record.device_id.clone(),
                        stream,
                    });
                    return;
                }
                Err(error) => {
                    let terminal = terminal_device_accept_error(error.code);
                    let _ = tx.send(MobileAccessCommand::DeviceAcceptFailed {
                        device_id: record.device_id.clone(),
                        code: error.code,
                        message: error.message,
                    });
                    if terminal {
                        return;
                    }
                    let delay = jittered_backoff(backoff);
                    sleep(delay).await;
                    backoff = backoff.saturating_mul(2).min(ACCEPT_RECONNECT_MAX);
                }
            }
        }
    })
}

fn spawn_offer_poll_task(
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    managed_service: ManagedMobileServiceClient,
    offer_id: MobilePairingOfferId,
    host_offer_token: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = managed_service
                .poll_host_offer(&offer_id, &host_offer_token)
                .await;
            match result {
                Ok(response) => match managed_handoff_from_poll_response(response) {
                    Ok(ManagedPollOutcome::Pending) => {
                        sleep(OFFER_POLL_INTERVAL).await;
                    }
                    Ok(ManagedPollOutcome::Redeemed(handoff)) => {
                        let _ = tx
                            .send(MobileAccessCommand::PairingOfferRedeemed { offer_id, handoff });
                        return;
                    }
                    Ok(ManagedPollOutcome::Terminal(state)) => {
                        let _ =
                            tx.send(MobileAccessCommand::PairingOfferTerminal { offer_id, state });
                        return;
                    }
                    Err(error) => {
                        let _ = tx.send(MobileAccessCommand::PairingFailed {
                            offer_id,
                            code: error.code,
                            message: error.message,
                        });
                        return;
                    }
                },
                Err(error) => {
                    let _ = tx.send(MobileAccessCommand::PairingFailed {
                        offer_id,
                        code: error.code,
                        message: error.message,
                    });
                    return;
                }
            }
        }
    })
}

enum ManagedPollOutcome {
    Pending,
    Redeemed(Box<ManagedMobilePairingHandoff>),
    Terminal(ManagedOfferTerminalState),
}

fn managed_handoff_from_poll_response(
    response: PollHostOfferResponse,
) -> Result<ManagedPollOutcome, ManagedServiceError> {
    let offer_id = response.offer_id;
    match response.status {
        HostOfferStatus::Pending => {
            if response.expires_at_ms.is_none() {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed mobile offer {offer_id} pending response omitted expiry"),
                ));
            }
            Ok(ManagedPollOutcome::Pending)
        }
        HostOfferStatus::Redeemed => {
            let pairing_id = response.pairing_id.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} was redeemed without pairing id"),
                )
            })?;
            let host_pairing_secret = response.host_pairing_secret.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} handoff was already consumed"),
                )
            })?;
            let device = response.device.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} was redeemed without device summary"),
                )
            })?;
            let broker = response
                .broker
                .ok_or_else(|| {
                    ManagedServiceError::new(
                        MobileAccessErrorCode::RepairRequired,
                        format!("managed mobile offer {offer_id} was redeemed without broker"),
                    )
                })?
                .into_protocol()?;
            let host_broker_credentials = response
                .host_broker_credentials
                .ok_or_else(|| {
                    ManagedServiceError::new(
                        MobileAccessErrorCode::RepairRequired,
                        format!(
                            "managed mobile offer {offer_id} was redeemed without host broker credentials"
                        ),
                    )
                })?
                .into_protocol()?;
            Ok(ManagedPollOutcome::Redeemed(Box::new(
                ManagedMobilePairingHandoff {
                    pairing_id,
                    host_pairing_secret,
                    device_id: MobileDeviceId(device.device_id),
                    device_label: device.label,
                    device_created_at_ms: device.created_at_ms,
                    device_last_seen_at_ms: device.last_seen_at_ms,
                    broker,
                    host_broker_credentials,
                },
            )))
        }
        HostOfferStatus::Expired => Ok(ManagedPollOutcome::Terminal(
            ManagedOfferTerminalState::Expired,
        )),
        HostOfferStatus::Cancelled => Ok(ManagedPollOutcome::Terminal(
            ManagedOfferTerminalState::Cancelled,
        )),
        HostOfferStatus::Failed => Ok(ManagedPollOutcome::Terminal(
            ManagedOfferTerminalState::Failed(format!("managed mobile offer {offer_id} failed")),
        )),
    }
}

async fn connect_mobile_device_stream(
    managed_service: &ManagedMobileServiceClient,
    record: &MobilePairingRecord,
) -> Result<EnvelopeStream, MobileTaskError> {
    let managed = match &record.managed {
        Some(managed) => {
            let response = managed_service
                .mint_host_broker_credentials(record)
                .await
                .map_err(MobileTaskError::managed_service)?;
            if response.pairing_id != managed.pairing_id || response.status != PairingStatus::Active
            {
                return Err(MobileTaskError {
                    code: MobileAccessErrorCode::RepairRequired,
                    message: "managed mobile service returned credentials for the wrong pairing"
                        .to_owned(),
                });
            }
            let broker = response
                .broker
                .into_protocol()
                .map_err(MobileTaskError::managed_service)?;
            let credentials = response
                .broker_credentials
                .into_protocol()
                .map_err(MobileTaskError::managed_service)?;
            Some((broker, credentials))
        }
        None => None,
    };
    connect_mobile_record_stream(
        record.broker.clone(),
        managed,
        record.room,
        record.psk.clone(),
    )
    .await
}

async fn connect_mobile_record_stream(
    broker: BrokerEndpoint,
    managed: Option<(ManagedBrokerEndpoint, ManagedBrokerCredentials)>,
    room: RoomId,
    psk: PreSharedKey,
) -> Result<EnvelopeStream, MobileTaskError> {
    match managed {
        Some((broker, credentials)) => {
            let config = mqtt_transport::ManagedMqttConnectConfig {
                broker,
                credentials,
                room,
                psk,
                role: ParticipantRole::Host,
            };
            mqtt_transport::connect_managed_ephemeral(config)
                .await
                .map_err(|err| {
                    MobileTaskError::transport(format!(
                        "managed MQTT mobile transport failed: {err}"
                    ))
                })
        }
        None => {
            let config = MqttConnectConfig {
                endpoint: broker,
                room,
                psk,
                role: ParticipantRole::Host,
            };
            mqtt_transport::connect_ephemeral(config)
                .await
                .map_err(|err| {
                    MobileTaskError::transport(format!("MQTT mobile transport failed: {err}"))
                })
        }
    }
}

async fn bridge_authenticated_mobile(
    host: HostHandle,
    tx: mpsc::UnboundedSender<MobileAccessCommand>,
    device_id: MobileDeviceId,
    connection_instance_id: u64,
    stream: EnvelopeStream,
) {
    match accept(&ServerConfig::current(), stream).await {
        Ok(connection) => {
            if let Err(err) = run_mobile_connection(connection, host).await {
                tracing::warn!(device_id = %device_id, error = ?err, "mobile Tyde connection ended with frame error");
            }
        }
        Err(err) => {
            tracing::warn!(device_id = %device_id, error = ?err, "mobile Tyde handshake failed");
        }
    }
    let _ = tx.send(MobileAccessCommand::DeviceDisconnected {
        device_id,
        connection_instance_id,
    });
}

fn dev_broker_endpoint(configured: Option<&BrokerUrl>) -> Result<Option<BrokerEndpoint>, String> {
    let Some(url) = configured else {
        return Ok(None);
    };
    validate_broker_url(url).map_err(|err| err.to_string())?;
    if url.as_str() == protocol::DEFAULT_MOBILE_MQTT_BROKER_URL {
        return Err(
            "the public default mobile broker is no longer supported; pair through tycode.dev"
                .to_owned(),
        );
    }
    if !is_loopback_broker_url(url) {
        return Err(
            "custom mobile broker URLs are dev/test-only; production mobile access uses tycode.dev"
                .to_owned(),
        );
    }
    Ok(Some(BrokerEndpoint {
        url: url.clone(),
        auth: BrokerAuth::Anonymous,
    }))
}

fn initial_enabled_broker_status(
    pairings: &MobilePairings,
    settings: &HostSettings,
) -> MobileBrokerStatus {
    match dev_broker_endpoint(settings.mobile_broker_url.as_ref()) {
        Ok(Some(endpoint)) => MobileBrokerStatus::Online {
            broker_url: endpoint.url,
        },
        Ok(None) => managed_broker_status_for_pairings(pairings),
        Err(message) => MobileBrokerStatus::Error {
            broker_url: settings.mobile_broker_url.clone(),
            code: MobileAccessErrorCode::InvalidConfig,
            message,
        },
    }
}

fn managed_broker_status_for_pairings(pairings: &MobilePairings) -> MobileBrokerStatus {
    if let Some(broker_url) = first_managed_broker_url(pairings) {
        return MobileBrokerStatus::Connecting { broker_url };
    }
    if pairings
        .devices
        .iter()
        .any(|record| record.state == MobileDeviceState::RepairRequired)
    {
        return MobileBrokerStatus::RepairRequired {
            code: MobileAccessErrorCode::RepairRequired,
            message: "Stored mobile pairings must be repaired by pairing again through tycode.dev"
                .to_owned(),
        };
    }
    MobileBrokerStatus::RepairRequired {
        code: MobileAccessErrorCode::RepairRequired,
        message: "Mobile access requires a tycode.dev managed pairing before connecting".to_owned(),
    }
}

fn first_managed_broker_url(pairings: &MobilePairings) -> Option<BrokerUrl> {
    pairings.devices.iter().find_map(|record| {
        if matches!(
            record.state,
            MobileDeviceState::RepairRequired | MobileDeviceState::Revoked
        ) {
            return None;
        }
        record
            .managed
            .as_ref()
            .map(|managed| managed.broker.endpoint.clone())
    })
}

fn terminal_device_accept_error(code: MobileAccessErrorCode) -> bool {
    matches!(
        code,
        MobileAccessErrorCode::RepairRequired | MobileAccessErrorCode::RevokedDevice
    )
}

fn mark_legacy_pairings_repair_required(
    pairings: &mut MobilePairings,
    settings: &HostSettings,
) -> bool {
    let mut changed = false;
    for record in &mut pairings.devices {
        if record.managed.is_none()
            && !legacy_dev_pairing_allowed(record, settings)
            && record.state != MobileDeviceState::RepairRequired
        {
            record.state = MobileDeviceState::RepairRequired;
            changed = true;
        }
    }
    changed
}

fn legacy_dev_pairing_allowed(record: &MobilePairingRecord, settings: &HostSettings) -> bool {
    let Some(configured) = settings.mobile_broker_url.as_ref() else {
        return false;
    };
    configured == &record.broker.url && is_loopback_broker_url(configured)
}

fn is_loopback_broker_url(url: &BrokerUrl) -> bool {
    url::Url::parse(url.as_str())
        .ok()
        .is_some_and(|parsed| is_loopback_url(&parsed))
}

fn is_loopback_url(parsed: &url::Url) -> bool {
    match parsed.host() {
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<IpAddr>()
                    .map(|addr| addr.is_loopback())
                    .unwrap_or(false)
        }
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        None => false,
    }
}

fn host_release_version_string() -> String {
    crate::host_release_version()
        .map(|version| version.to_string())
        .unwrap_or_else(|| protocol::TYDE_VERSION.to_string())
}

fn host_release_version_for_qr() -> Result<protocol::TydeReleaseVersion, String> {
    let value = host_release_version_string();
    protocol::TydeReleaseVersion::parse(&value)
        .map_err(|err| format!("host release version {value:?} is invalid: {err}"))
}

#[derive(Debug)]
struct MobileTaskError {
    code: MobileAccessErrorCode,
    message: String,
}

impl MobileTaskError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            code: MobileAccessErrorCode::TransportFailed,
            message: message.into(),
        }
    }

    fn managed_service(error: ManagedServiceError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

fn jittered_backoff(base: Duration) -> Duration {
    let nanos = match now_ms() {
        Ok(now) => now % 1_000,
        Err(_) => 0,
    };
    let jitter = base / 4;
    if jitter.is_zero() {
        return base;
    }
    let jitter_ms = (jitter.as_millis() as u64).saturating_mul(nanos) / 1_000;
    (base + Duration::from_millis(jitter_ms)).min(ACCEPT_RECONNECT_MAX)
}

async fn send_mobile_access_state(
    stream: &Stream,
    payload: &MobileAccessStatePayload,
) -> Result<(), StreamClosed> {
    match serde_json::to_value(payload) {
        Ok(value) => stream.send_value(FrameKind::MobileAccessState, value),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialize MobileAccessState payload");
            Err(StreamClosed)
        }
    }
}

async fn send_mobile_pairing_offer(
    stream: &Stream,
    payload: &MobilePairingOfferPayload,
) -> Result<(), StreamClosed> {
    match serde_json::to_value(payload) {
        Ok(value) => stream.send_value(FrameKind::MobilePairingOffer, value),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialize MobilePairingOffer payload");
            Err(StreamClosed)
        }
    }
}

fn new_offer_id() -> Result<MobilePairingOfferId, String> {
    MobilePairingOfferId::new(Uuid::new_v4().to_string())
        .map_err(|err| format!("failed to create mobile pairing offer id: {err}"))
}

fn new_device_id() -> Result<MobileDeviceId, String> {
    Ok(MobileDeviceId(Uuid::new_v4().to_string()))
}

fn now_ms() -> Result<u64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock is before UNIX epoch: {err}"))?;
    let millis = duration.as_millis();
    u64::try_from(millis).map_err(|_| "current time does not fit in u64 milliseconds".to_owned())
}

fn spawn_worker<F>(name: &'static str, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Err(err) = std::thread::Builder::new().name(name.to_owned()).spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build mobile access runtime");
        runtime.block_on(future);
    }) {
        tracing::error!(error = %err, "failed to spawn mobile access worker thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Envelope, HostSettings};

    struct TestActor {
        actor: MobileAccessActor,
        _dir: tempfile::TempDir,
    }

    fn test_actor() -> TestActor {
        test_actor_with(None, test_settings(false))
    }

    fn test_actor_with(
        initial_pairings: Option<MobilePairings>,
        initial_settings: HostSettings,
    ) -> TestActor {
        test_actor_with_service(initial_pairings, initial_settings, None)
    }

    fn test_actor_with_service(
        initial_pairings: Option<MobilePairings>,
        initial_settings: HostSettings,
        managed_service_base_url: Option<String>,
    ) -> TestActor {
        let dir = tempfile::tempdir().expect("tempdir");
        let pairings_store =
            MobilePairingsStore::load(dir.path().join("mobile_pairings.json")).expect("pairings");
        if let Some(pairings) = initial_pairings {
            pairings_store.save(&pairings).expect("save pairings");
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let host = crate::spawn_host_with_mock_backend(
            dir.path().join("sessions.json"),
            dir.path().join("projects.json"),
            dir.path().join("settings.json"),
        )
        .expect("host");
        let actor = MobileAccessActor::new(
            host,
            tx.clone(),
            rx,
            MobileAccessInit {
                pairings_store,
                initial_settings,
                pairing_ttl: DEFAULT_PAIRING_TTL,
                managed_service_base_url,
            },
        )
        .expect("actor");
        TestActor { actor, _dir: dir }
    }

    fn test_settings(enabled: bool) -> HostSettings {
        HostSettings {
            enabled_backends: Vec::new(),
            default_backend: None,
            enable_mobile_connections: enabled,
            mobile_broker_url: None,
            tyde_debug_mcp_enabled: false,
            tyde_agent_control_mcp_enabled: true,
            complexity_tiers_enabled: false,
            backend_tier_configs: std::collections::HashMap::new(),
            background_agent_features: Default::default(),
            code_intel: Default::default(),
            backend_config: std::collections::HashMap::new(),
            launch_profiles: Vec::new(),
        }
    }

    #[test]
    fn mobile_pairings_lease_rejects_second_holder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mobile_pairings.json");
        let first = MobilePairingsLease::try_acquire(&path).expect("first lease");
        let second_error =
            MobilePairingsLease::try_acquire(&path).expect_err("second lease must fail");
        assert!(
            second_error.contains("already in use"),
            "unexpected lock error: {second_error}"
        );
        drop(first);
        MobilePairingsLease::try_acquire(&path).expect("lease after drop");
    }

    fn plaintext_test_settings(enabled: bool) -> HostSettings {
        let mut settings = test_settings(enabled);
        settings.mobile_broker_url =
            Some(BrokerUrl::new("mqtt://broker.example.test:1883").unwrap());
        settings
    }

    fn loopback_tls_test_settings(enabled: bool) -> HostSettings {
        let mut settings = test_settings(enabled);
        settings.mobile_broker_url = Some(BrokerUrl::new("mqtts://127.0.0.1:9").unwrap());
        settings
    }

    fn ipv6_loopback_tls_test_settings(enabled: bool) -> HostSettings {
        let mut settings = test_settings(enabled);
        settings.mobile_broker_url = Some(BrokerUrl::new("mqtts://[::1]:8883").unwrap());
        settings
    }

    fn public_ipv6_tls_test_settings(enabled: bool) -> HostSettings {
        let mut settings = test_settings(enabled);
        settings.mobile_broker_url = Some(BrokerUrl::new("mqtts://[2001:db8::1]:8883").unwrap());
        settings
    }

    fn endpoint() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://127.0.0.1:9").expect("broker URL"),
            auth: BrokerAuth::Anonymous,
        }
    }

    fn public_endpoint() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new(mqtt_transport::DEFAULT_MOBILE_MQTT_BROKER_URL)
                .expect("default broker URL"),
            auth: BrokerAuth::Anonymous,
        }
    }

    fn test_psk() -> PreSharedKey {
        PreSharedKey::from_slice(&[9_u8; 32]).expect("psk")
    }

    fn active_pairing() -> ActiveMobilePairingCredential {
        let psk = test_psk();
        ActiveMobilePairingCredential {
            offer_id: MobilePairingOfferId::new("offer-1").expect("offer id"),
            broker: endpoint(),
            room: RoomId([7_u8; 16]),
            psk: psk.clone(),
            created_at_ms: 1,
            key_fingerprint: key_fingerprint(&psk),
            managed: None,
        }
    }

    fn paired_device(device_id: MobileDeviceId) -> MobilePairingRecord {
        let psk = test_psk();
        MobilePairingRecord {
            device_id,
            broker: endpoint(),
            room: RoomId([8_u8; 16]),
            psk: psk.clone(),
            label: "Mobile device".to_owned(),
            created_at_ms: 1,
            last_seen_at_ms: None,
            state: MobileDeviceState::Paired,
            key_fingerprint: key_fingerprint(&psk),
            managed: None,
        }
    }

    fn public_paired_device(device_id: MobileDeviceId) -> MobilePairingRecord {
        let psk = test_psk();
        MobilePairingRecord {
            device_id,
            broker: public_endpoint(),
            room: RoomId([8_u8; 16]),
            psk: psk.clone(),
            label: "Legacy mobile device".to_owned(),
            created_at_ms: 1,
            last_seen_at_ms: None,
            state: MobileDeviceState::Paired,
            key_fingerprint: key_fingerprint(&psk),
            managed: None,
        }
    }

    fn pairings_with_active() -> MobilePairings {
        MobilePairings {
            version: crate::store::mobile_pairings::MOBILE_PAIRINGS_VERSION,
            active_pairing: Some(active_pairing()),
            devices: Vec::new(),
        }
    }

    fn pairings_with_device(device_id: MobileDeviceId) -> MobilePairings {
        MobilePairings {
            version: crate::store::mobile_pairings::MOBILE_PAIRINGS_VERSION,
            active_pairing: None,
            devices: vec![paired_device(device_id)],
        }
    }

    fn pairings_with_public_device(device_id: MobileDeviceId) -> MobilePairings {
        MobilePairings {
            version: crate::store::mobile_pairings::MOBILE_PAIRINGS_VERSION,
            active_pairing: None,
            devices: vec![public_paired_device(device_id)],
        }
    }

    fn stream(path: &str) -> (Stream, mpsc::UnboundedReceiver<Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Stream::new(StreamPath(path.to_owned()), tx), rx)
    }

    async fn recv_kind(rx: &mut mpsc::UnboundedReceiver<Envelope>) -> FrameKind {
        rx.recv().await.expect("event").kind
    }

    #[tokio::test]
    async fn disabled_snapshot_is_idle() {
        let actor = test_actor().actor;
        let payload = actor.state_payload();
        assert_eq!(payload.broker_status, MobileBrokerStatus::Disabled);
        assert_eq!(payload.pairing, MobilePairingState::Idle);
        assert!(payload.paired_devices.is_empty());
    }

    #[tokio::test]
    async fn startup_discards_persisted_active_pairing() {
        let test = test_actor_with(Some(pairings_with_active()), test_settings(true));

        assert_eq!(test.actor.pairing, MobilePairingState::Idle);
        assert!(test.actor.pairings.active_pairing.is_none());
        let persisted = test.actor.pairings_store.get().expect("persist pairings");
        assert!(persisted.active_pairing.is_none());
    }

    #[tokio::test]
    async fn startup_discards_persisted_active_pairing_when_disabled() {
        let test = test_actor_with(Some(pairings_with_active()), test_settings(false));

        assert_eq!(test.actor.broker_status, MobileBrokerStatus::Disabled);
        assert_eq!(test.actor.pairing, MobilePairingState::Idle);
        assert!(test.actor.pairings.active_pairing.is_none());
        let persisted = test.actor.pairings_store.get().expect("persist pairings");
        assert!(persisted.active_pairing.is_none());
    }

    #[tokio::test]
    async fn successful_device_reconnect_restores_online_broker_status() {
        let device_id = MobileDeviceId("device-1".to_owned());
        let mut test = test_actor_with(
            Some(pairings_with_device(device_id.clone())),
            loopback_tls_test_settings(true),
        );
        test.actor.broker_status = MobileBrokerStatus::Error {
            broker_url: Some(endpoint().url),
            code: MobileAccessErrorCode::TransportFailed,
            message: "previous reconnect failed".to_owned(),
        };

        assert!(test.actor.mark_device_connected(&device_id, Some(42)));

        assert!(matches!(
            &test.actor.broker_status,
            MobileBrokerStatus::Online { broker_url } if broker_url.as_str() == "mqtts://127.0.0.1:9"
        ));
        let record = test
            .actor
            .pairings
            .devices
            .iter()
            .find(|record| record.device_id == device_id)
            .expect("device record");
        assert_eq!(record.state, MobileDeviceState::Connected);
        assert_eq!(record.last_seen_at_ms, Some(42));
    }

    #[tokio::test]
    async fn startup_marks_public_broker_pairing_repair_required() {
        let device_id = MobileDeviceId("device-1".to_owned());
        let test = test_actor_with(
            Some(pairings_with_public_device(device_id.clone())),
            test_settings(true),
        );

        assert!(matches!(
            test.actor.broker_status,
            MobileBrokerStatus::RepairRequired {
                code: MobileAccessErrorCode::RepairRequired,
                ..
            }
        ));
        let record = test
            .actor
            .pairings
            .devices
            .iter()
            .find(|record| record.device_id == device_id)
            .expect("device record");
        assert_eq!(record.state, MobileDeviceState::RepairRequired);
        assert!(test.actor.accept_tasks.is_empty());
    }

    #[tokio::test]
    async fn enabling_without_pairing_requires_managed_repair() {
        let mut test = test_actor();
        test.actor.apply_settings(test_settings(true)).await;

        assert!(matches!(
            &test.actor.broker_status,
            MobileBrokerStatus::RepairRequired {
                code: MobileAccessErrorCode::RepairRequired,
                ..
            }
        ));
        assert!(test.actor.accept_tasks.is_empty());
    }

    #[tokio::test]
    async fn repair_required_device_accept_failure_stops_retry_task() {
        let device_id = MobileDeviceId("device-1".to_owned());
        let mut test = test_actor_with(
            Some(pairings_with_device(device_id.clone())),
            test_settings(true),
        );
        let key = AcceptTaskKey::Device(device_id.clone());
        test.actor
            .accept_tasks
            .insert(key.clone(), tokio::spawn(async {}));

        test.actor
            .device_accept_failed(
                &device_id,
                MobileAccessErrorCode::RepairRequired,
                "managed pairing requires repair".to_owned(),
            )
            .await;

        assert!(!test.actor.accept_tasks.contains_key(&key));
        let record = test
            .actor
            .pairings
            .devices
            .iter()
            .find(|record| record.device_id == device_id)
            .expect("device record");
        assert_eq!(record.state, MobileDeviceState::RepairRequired);
    }

    #[test]
    fn service_unavailable_error_code_is_parsed_as_typed_error() {
        let envelope: ManagedErrorEnvelope = serde_json::from_value(serde_json::json!({
            "error": {
                "code": "service_unavailable",
                "message": "tycode.dev maintenance",
                "retryable": true,
                "state": "maintenance",
                "paywall_url": null
            }
        }))
        .expect("parse service_unavailable error");

        let error = envelope.error.into_error();

        assert_eq!(error.code, MobileAccessErrorCode::ServiceUnavailable);
        assert!(error.message.contains("tycode.dev maintenance"));
        assert!(error.message.contains("maintenance"));
        assert!(error.message.contains("Retryable."));
    }

    #[test]
    fn managed_service_dto_debug_redacts_broker_secrets() {
        let credentials: ContractBrokerCredentials = serde_json::from_value(serde_json::json!({
            "grant_id": "grant_01JSECRET",
            "client_id": "tyde/prod/pair_01J/host/grant_01JSECRET",
            "connect": {
                "username": "x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant",
                "password": "signed-grant-secret",
                "websocket_url": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt?x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant&tycode-grant=signed-grant-secret",
                "headers": {
                    "x-tycode-grant": "signed-grant-secret"
                }
            },
            "scope": {
                "namespace": "tyde/prod/pair_01J",
                "role": "host",
                "publish": ["tyde/prod/pair_01J/rooms/+/host-to-client"],
                "subscribe": ["tyde/prod/pair_01J/rooms/+/client-to-host"]
            },
            "issued_at_ms": 1760000000000_u64,
            "expires_at_ms": 1760000300000_u64
        }))
        .expect("credentials");
        let debug = format!("{credentials:?}");

        assert!(!debug.contains("signed-grant-secret"));
        assert!(!debug.contains("a1234567890-ats.iot.us-west-2.amazonaws.com"));
        assert!(!debug.contains("x-tycode-grant"));
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn plaintext_public_broker_url_reports_invalid_config() {
        let mut test = test_actor();
        test.actor
            .apply_settings(plaintext_test_settings(true))
            .await;

        let MobileBrokerStatus::Error { code, message, .. } = &test.actor.broker_status else {
            panic!(
                "expected invalid broker URL to report Error, got {:?}",
                test.actor.broker_status
            );
        };
        assert_eq!(*code, MobileAccessErrorCode::InvalidConfig);
        assert!(message.contains("insecure"));
    }

    #[tokio::test]
    async fn ipv6_loopback_broker_url_reports_online() {
        let mut test = test_actor();
        test.actor
            .apply_settings(ipv6_loopback_tls_test_settings(true))
            .await;

        assert!(matches!(
            &test.actor.broker_status,
            MobileBrokerStatus::Online { broker_url }
                if broker_url.as_str() == "mqtts://[::1]:8883"
        ));
    }

    #[tokio::test]
    async fn public_ipv6_broker_url_reports_invalid_config() {
        let mut test = test_actor();
        test.actor
            .apply_settings(public_ipv6_tls_test_settings(true))
            .await;

        let MobileBrokerStatus::Error { code, message, .. } = &test.actor.broker_status else {
            panic!(
                "expected invalid broker URL to report Error, got {:?}",
                test.actor.broker_status
            );
        };
        assert_eq!(*code, MobileAccessErrorCode::InvalidConfig);
        assert!(message.contains("dev/test-only"));
    }

    #[tokio::test]
    async fn pairing_offer_contains_configured_mqtt_qr() {
        let mut test = test_actor();
        let (requester_stream, mut requester_rx) = stream("/host/requester");
        let requester_path = requester_stream.path().clone();
        test.actor
            .register_bootstrap_subscriber(requester_stream)
            .await
            .expect("register requester");
        test.actor
            .activate_bootstrap_subscriber(requester_path)
            .await;
        test.actor
            .apply_settings(loopback_tls_test_settings(true))
            .await;
        let _ = recv_kind(&mut requester_rx).await;

        test.actor
            .start_pairing(StreamPath("/host/requester".to_owned()))
            .await;

        assert_eq!(
            recv_kind(&mut requester_rx).await,
            FrameKind::MobileAccessState
        );
        let offer = requester_rx.recv().await.expect("offer");
        assert_eq!(offer.kind, FrameKind::MobilePairingOffer);
        let payload: MobilePairingOfferPayload = offer.parse_payload().expect("offer payload");
        let qr = MobilePairingQrPayload::from_any(&payload.qr_uri.0).expect("QR payload");
        assert_eq!(qr.broker.url.as_str(), "mqtts://127.0.0.1:9");
        assert_eq!(qr.policy, mqtt_transport::MqttTransportPolicy::default());
        assert_eq!(
            mqtt_transport::host_to_client_topic(&qr.room),
            format!("tyde/v1/{}/host-to-client", qr.room)
        );
    }

    #[tokio::test]
    async fn managed_pairing_start_calls_service_and_surfaces_host_built_qr() {
        let mock = MockManagedService::start().await;
        let mut test = test_actor_with_service(None, test_settings(true), Some(mock.base_url()));
        let (requester_stream, mut requester_rx) = stream("/host/requester");
        let requester_path = requester_stream.path().clone();
        test.actor
            .register_bootstrap_subscriber(requester_stream)
            .await
            .expect("register requester");
        test.actor
            .activate_bootstrap_subscriber(requester_path)
            .await;

        test.actor
            .start_pairing(StreamPath("/host/requester".to_owned()))
            .await;

        assert_eq!(
            recv_kind(&mut requester_rx).await,
            FrameKind::MobileAccessState
        );
        let offer = requester_rx.recv().await.expect("offer");
        assert_eq!(offer.kind, FrameKind::MobilePairingOffer);
        let payload: MobilePairingOfferPayload = offer.parse_payload().expect("offer payload");
        assert_eq!(payload.offer_id.as_str(), "offer_01JMANAGED");
        assert!(
            !payload.qr_uri.0.contains("payload=managed-test"),
            "server must not surface the service-owned QR stub"
        );
        assert!(matches!(
            &test.actor.broker_status,
            MobileBrokerStatus::Connecting { broker_url }
                if broker_url.as_str() == "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt"
        ));
        assert!(test.actor.accept_tasks.is_empty());
        let active = test
            .actor
            .pairings
            .active_pairing
            .as_ref()
            .expect("active managed pairing");
        let managed = active.managed.as_ref().expect("managed active pairing");
        let qr =
            ManagedMobilePairingQrPayload::from_any(&payload.qr_uri.0).expect("decode managed QR");
        assert_eq!(qr.offer_id.as_str(), "offer_01JMANAGED");
        assert_eq!(qr.offer_secret, "offer_secret_01JMANAGED");
        assert_eq!(qr.broker, managed.broker);
        assert_eq!(qr.room, active.room);
        assert_eq!(qr.psk, active.psk);
        assert_eq!(qr.host_label, "Tyde Host");
        assert_eq!(qr.expires_at_ms, 1760000300000_u64);
        assert_eq!(managed.pairing_url, payload.qr_uri.0);
        assert_eq!(managed.host_offer_token, "host_offer_01JMANAGED");
        assert_eq!(
            managed.host_broker_credentials.scope.role,
            ManagedBrokerRole::Host
        );

        let request = mock.last_create_offer().await;
        assert_eq!(request["host_label"], "Tyde Host");
        assert_eq!(
            request["transport_protocol_version"],
            mqtt_transport::MQTT_TRANSPORT_PROTOCOL_VERSION
        );
        assert!(request.get("tyggs_oauth_access_token").is_none());
        assert!(request.get("tyggs_pass_proof").is_none());
    }

    #[tokio::test]
    async fn pairing_offer_is_sent_only_to_requesting_stream() {
        let mut test = test_actor();
        let (requester_stream, mut requester_rx) = stream("/host/requester");
        let (other_stream, mut other_rx) = stream("/host/other");
        let requester_path = requester_stream.path().clone();
        let other_path = other_stream.path().clone();
        test.actor
            .register_bootstrap_subscriber(requester_stream)
            .await
            .expect("register requester");
        test.actor
            .register_bootstrap_subscriber(other_stream)
            .await
            .expect("register other");
        test.actor
            .activate_bootstrap_subscriber(requester_path)
            .await;
        test.actor.activate_bootstrap_subscriber(other_path).await;
        test.actor
            .apply_settings(loopback_tls_test_settings(true))
            .await;
        let _ = recv_kind(&mut requester_rx).await;
        let _ = recv_kind(&mut other_rx).await;

        test.actor
            .start_pairing(StreamPath("/host/requester".to_owned()))
            .await;

        assert_eq!(
            recv_kind(&mut requester_rx).await,
            FrameKind::MobileAccessState
        );
        assert_eq!(
            recv_kind(&mut requester_rx).await,
            FrameKind::MobilePairingOffer
        );
        assert_eq!(recv_kind(&mut other_rx).await, FrameKind::MobileAccessState);
        assert!(other_rx.try_recv().is_err());
    }

    #[derive(Clone)]
    struct MockManagedService {
        addr: std::net::SocketAddr,
        last_create_offer: std::sync::Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    }

    impl MockManagedService {
        async fn start() -> Self {
            use axum::extract::State;
            use axum::routing::post;
            use axum::{Json, Router};

            #[derive(Clone)]
            struct StateValue {
                last_create_offer: std::sync::Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
            }

            async fn create_offer(
                State(state): State<StateValue>,
                Json(request): Json<serde_json::Value>,
            ) -> Json<serde_json::Value> {
                *state.last_create_offer.lock().await = Some(request);
                Json(serde_json::json!({
                    "offer_id": "offer_01JMANAGED",
                    "offer_secret": "offer_secret_01JMANAGED",
                    "host_offer_token": "host_offer_01JMANAGED",
                    "expires_at_ms": 1760000300000_u64,
                    "broker": managed_broker_json(),
                    "host_broker_credentials": managed_credentials_json("offer_01JMANAGED"),
                    "pairing_url": "https://tycode.dev/tyde/#tyde-pair://v2?payload=managed-test",
                    "status": "pending"
                }))
            }

            fn managed_broker_json() -> serde_json::Value {
                serde_json::json!({
                    "endpoint": "wss://a1234567890-ats.iot.us-west-2.amazonaws.com/mqtt",
                    "provider": "aws_iot_core",
                    "region": "us-west-2",
                    "authorizer_name": "tycode-mobile-v1"
                })
            }

            fn managed_credentials_json(owner: &str) -> serde_json::Value {
                let namespace = format!("tyde/prod/{owner}");
                serde_json::json!({
                    "grant_id": "grant_01JMANAGED",
                    "client_id": format!("{namespace}/host/grant_01JMANAGED"),
                    "connect": {
                        "username": "x-amz-customauthorizer-name=tycode-mobile-v1&token-key-name=tycode-grant",
                        "password": "signed-grant",
                        "headers": {
                            "x-tycode-grant": "signed-grant"
                        }
                    },
                    "scope": {
                        "namespace": namespace,
                        "role": "host",
                        "publish": [format!("{namespace}/rooms/+/host-to-client")],
                        "subscribe": [format!("{namespace}/rooms/+/client-to-host")]
                    },
                    "issued_at_ms": 1760000000000_u64,
                    "expires_at_ms": 1760000300000_u64
                })
            }

            let last_create_offer = std::sync::Arc::new(tokio::sync::Mutex::new(None));
            let state = StateValue {
                last_create_offer: last_create_offer.clone(),
            };
            let app = Router::new()
                .route("/api/tyde/mobile/v1/host/offers", post(create_offer))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock managed service");
            let addr = listener.local_addr().expect("mock managed service addr");
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("mock managed service");
            });
            Self {
                addr,
                last_create_offer,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}/api/tyde/mobile/v1", self.addr)
        }

        async fn last_create_offer(&self) -> serde_json::Value {
            self.last_create_offer
                .lock()
                .await
                .clone()
                .expect("create offer request")
        }
    }
}
