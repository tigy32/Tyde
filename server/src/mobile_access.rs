use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent::registry::{AgentStatusTransition, PendingUserResponseKind};
use anyhow::anyhow;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs2::FileExt;
use hmac::{Hmac, Mac};
use mqtt_transport::{
    BrokerAuth, BrokerEndpoint, DirectMobilePairingQrPayload, EnvelopeStream,
    ManagedMobilePairingQrPayload, ManagedMobilePairingQrPayloadParams, MobilePairingQrPayload,
    MqttConnectConfig, ParticipantRole, PreSharedKey, RoomId, validate_broker_url,
};
use protocol::{
    AgentControlStatus, AgentOrigin, BrokerUrl, FrameKind, ManagedBrokerAuthorizerName,
    ManagedBrokerClientId, ManagedBrokerConnectAuth, ManagedBrokerCredentialScope,
    ManagedBrokerCredentials, ManagedBrokerEndpoint, ManagedBrokerGrantId, ManagedBrokerProvider,
    ManagedBrokerRegion, ManagedBrokerRole, ManagedBrokerTopicNamespace, MobileAccessErrorCode,
    MobileAccessStatePayload, MobileBrokerStatus, MobileDeviceId, MobileDeviceRenamePayload,
    MobileDeviceRevokePayload, MobileDeviceState, MobileDirectHostingStatus,
    MobilePairingCancelPayload, MobilePairingOfferId, MobilePairingOfferPayload,
    MobilePairingQrUri, MobilePairingState, MobilePushNotification, MobilePushReason,
    MobilePushSubscription, MobileWebBundleSource, PROTOCOL_VERSION, StreamPath,
};
use serde::{Deserialize, Serialize};
use settings_model::HostSettings;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

use crate::ServerConfig;
use crate::accept;
use crate::connection::run_mobile_connection;
use crate::error::{AppError, AppResult};
use crate::host::HostHandle;
use crate::mobile_http::{MobileHttpServer, MobileWebAssets, resolve_bind_addr};
use crate::mobile_push::{PushSendError, send_push};
use crate::store::mobile_pairings::{
    ActiveDirectMobilePairing, ActiveManagedMobilePairingCredential, ActiveMobilePairingCredential,
    DevicePushRegistration, DirectMobilePairingRecord, ManagedMobilePairingCredential,
    ManagedMobilePairingHandoff, ManagedMobilePairingRecordInsert, MobilePairingRecord,
    MobilePairings, MobilePairingsStore, PendingManagedMobileHandoffAck, constant_time_eq,
    direct_key_fingerprint, key_fingerprint, token_hash,
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
const HANDOFF_ACK_RETRY_INITIAL: Duration = Duration::from_secs(1);
const HANDOFF_ACK_RETRY_MAX: Duration = Duration::from_secs(30);
/// Shown on the pairing QR and in push notifications, so both name the same
/// host to the user.
pub(crate) const HOST_LABEL: &str = "Tyde Host";

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
        let _ = self.tx.send(MobileAccessCommand::SettingsChanged {
            settings: Box::new(settings),
        });
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.tx.send(MobileAccessCommand::Shutdown);
    }

    pub(crate) fn start_pairing(&self, requester: StreamPath, direct: bool) -> AppResult<()> {
        self.tx
            .send(MobileAccessCommand::StartPairing { requester, direct })
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

    /// `device_id` is the identity the connection authenticated as, never a
    /// value the client asserted, so one paired device cannot register a
    /// subscription against another.
    pub(crate) async fn register_push(
        &self,
        device_id: MobileDeviceId,
        subscription: MobilePushSubscription,
    ) -> AppResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MobileAccessCommand::RegisterPush {
                device_id,
                subscription: Box::new(subscription),
                reply: reply_tx,
            })
            .map_err(|_| {
                AppError::internal(
                    "mobile_push_subscribe",
                    anyhow!("mobile access actor stopped"),
                )
            })?;
        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into_app_error("mobile_push_subscribe")),
            Err(_) => Err(AppError::internal(
                "mobile_push_subscribe",
                anyhow!("mobile access actor dropped push subscribe reply"),
            )),
        }
    }

    pub(crate) async fn unregister_push(&self, device_id: MobileDeviceId) -> AppResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(MobileAccessCommand::UnregisterPush {
                device_id,
                reply: reply_tx,
            })
            .map_err(|_| {
                AppError::internal(
                    "mobile_push_unsubscribe",
                    anyhow!("mobile access actor stopped"),
                )
            })?;
        match reply_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error.into_app_error("mobile_push_unsubscribe")),
            Err(_) => Err(AppError::internal(
                "mobile_push_unsubscribe",
                anyhow!("mobile access actor dropped push unsubscribe reply"),
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
    /// Handed in rather than subscribed to from the actor: at actor-spawn time
    /// the host is still being assembled and asserts its state lock is free.
    pub(crate) agent_status_transitions: broadcast::Receiver<AgentStatusTransition>,
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

/// Anything the mobile bridge can run the Tyde protocol over. `accept` has
/// always been generic over the byte stream; this names that requirement so the
/// actor can carry either transport without knowing which it has.
pub(crate) trait MobileTransport:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Send
{
}

impl<T> MobileTransport for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send {}

pub(crate) type BoxedMobileTransport = Box<dyn MobileTransport + Unpin>;

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
        settings: Box<HostSettings>,
    },
    StartPairing {
        requester: StreamPath,
        /// Pair against the host's own HTTP origin rather than the broker.
        direct: bool,
    },
    /// A phone is redeeming a direct-hosting pairing offer over HTTP.
    RedeemDirectPairing {
        request: Box<protocol::MobileDirectPairRequest>,
        reply:
            oneshot::Sender<Result<protocol::MobileDirectPairResponse, MobileAccessCommandFailure>>,
    },
    /// A direct-hosting WebSocket is presenting a device token.
    AuthenticateDirectDevice {
        token: String,
        reply: oneshot::Sender<Option<MobileDeviceId>>,
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
    RegisterPush {
        device_id: MobileDeviceId,
        subscription: Box<MobilePushSubscription>,
        reply: oneshot::Sender<Result<(), MobileAccessCommandFailure>>,
    },
    UnregisterPush {
        device_id: MobileDeviceId,
        reply: oneshot::Sender<Result<(), MobileAccessCommandFailure>>,
    },
    /// An agent finished a turn and has nothing queued behind it.
    NotifyAgentIdle {
        notification: Box<MobilePushNotification>,
    },
    /// A push service reported a stored subscription gone. Recorded so the
    /// device list can say so rather than silently delivering nothing.
    PushSubscriptionGone {
        device_id: MobileDeviceId,
    },
    PairingTransportConnected {
        offer_id: MobilePairingOfferId,
        stream: EnvelopeStream,
    },
    DeviceTransportConnected {
        device_id: MobileDeviceId,
        stream: BoxedMobileTransport,
    },
    PairingOfferRedeemed {
        offer_id: MobilePairingOfferId,
        handoff: Box<ManagedMobilePairingHandoff>,
    },
    PairingHandoffAckRetry {
        offer_id: MobilePairingOfferId,
        pairing_id: String,
        attempt: u32,
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

    pub(crate) fn into_parts(self) -> (MobileAccessErrorCode, String) {
        (self.code, self.message)
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
    handoff_ack_retry_task: Option<JoinHandle<()>>,
    next_connection_instance_id: u64,
    mobile_pairings_lease: Option<MobilePairingsLease>,
    push_client: reqwest::Client,
    agent_status_transitions: Option<broadcast::Receiver<AgentStatusTransition>>,
    idle_notifier_task: Option<JoinHandle<()>>,
    direct_hosting: DirectHostingState,
}

/// The direct mobile web server is deliberately independent of
/// `enable_mobile_connections`: that switch governs the managed broker path,
/// and a network locked down enough to want direct hosting is exactly the one
/// that does not want an outbound broker connection alongside it.
enum DirectHostingState {
    Disabled,
    Running(RunningDirectHost),
    Failed(String),
}

struct RunningDirectHost {
    server: MobileHttpServer,
    bind_addr: SocketAddr,
    bundle: DirectBundleChoice,
    asset_count: u32,
}

/// Which bundle direct hosting should serve. An explicit directory always wins
/// so a host can serve a bundle it just built; otherwise it serves the one this
/// binary was built with, if it was built with one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectBundleChoice {
    Directory(PathBuf),
    BuiltIn,
}

impl DirectBundleChoice {
    fn source(&self) -> MobileWebBundleSource {
        match self {
            Self::Directory(_) => MobileWebBundleSource::Directory,
            Self::BuiltIn => MobileWebBundleSource::BuiltIn,
        }
    }
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

    async fn acknowledge_host_handoff(
        &self,
        offer_id: &MobilePairingOfferId,
        host_offer_token: &str,
        pairing_id: &str,
    ) -> Result<(), ManagedServiceError> {
        let response = self
            .http
            .post(
                self.base
                    .url_for(&format!("/host/offers/{offer_id}/handoff/ack")),
            )
            .bearer_auth(host_offer_token)
            .json(&AcknowledgeHostHandoffRequest {
                pairing_id: pairing_id.to_owned(),
            })
            .send()
            .await
            .map_err(ManagedServiceError::transport)?;
        let response: AcknowledgeHostHandoffResponse = parse_managed_response(response).await?;
        if response.offer_id != offer_id.as_str()
            || response.pairing_id != pairing_id
            || response.status != HostHandoffStatus::Acknowledged
        {
            return Err(ManagedServiceError::new(
                MobileAccessErrorCode::ServiceUnavailable,
                "managed mobile service returned an invalid handoff acknowledgement",
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
    host_handoff: Option<ContractHostHandoffState>,
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
            .field("host_handoff", &self.host_handoff)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct CancelHostOfferResponse {
    offer_id: String,
    status: HostOfferStatus,
}

#[derive(Debug, Serialize)]
struct AcknowledgeHostHandoffRequest {
    pairing_id: String,
}

#[derive(Debug, Deserialize)]
struct AcknowledgeHostHandoffResponse {
    offer_id: String,
    pairing_id: String,
    status: HostHandoffStatus,
}

#[derive(Debug, Deserialize)]
struct ContractHostHandoffState {
    status: HostHandoffStatus,
    expires_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HostHandoffStatus {
    Available,
    Acknowledged,
    Expired,
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
    connect_valid_until_ms: u64,
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
            .field("connect_valid_until_ms", &self.connect_valid_until_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Clone, Deserialize)]
struct ContractBrokerConnect {
    username: Option<String>,
    password: Option<String>,
    #[serde(default)]
    websocket_url: Option<protocol::BrokerUrl>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for ContractBrokerConnect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContractBrokerConnect")
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
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
    HostHandoffExpired,
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
            ManagedErrorCode::HostHandoffExpired => MobileAccessErrorCode::PairingExpired,
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
                username: self.connect.username,
                password: self.connect.password,
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
        let pairing = match (&pairings.pending_handoff_ack, &pairings.active_pairing) {
            (Some(pending), None) => MobilePairingState::Active {
                offer_id: pending.offer_id.clone(),
                expires_at_ms: pending.expires_at_ms,
            },
            (None, Some(active)) => MobilePairingState::Active {
                offer_id: active.offer_id.clone(),
                expires_at_ms: active.managed.as_ref().map_or_else(
                    || {
                        active
                            .created_at_ms
                            .saturating_add(init.pairing_ttl.as_millis() as u64)
                    },
                    |managed| managed.expires_at_ms,
                ),
            },
            (None, None) => MobilePairingState::Idle,
            (Some(_), Some(_)) => {
                return Err(
                    "mobile pairings contain conflicting active offer and pending acknowledgement"
                        .to_owned(),
                );
            }
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
            handoff_ack_retry_task: None,
            next_connection_instance_id: 0,
            mobile_pairings_lease: None,
            push_client: reqwest::Client::new(),
            agent_status_transitions: Some(init.agent_status_transitions),
            idle_notifier_task: None,
            direct_hosting: DirectHostingState::Disabled,
        })
    }

    async fn run(mut self) {
        self.spawn_idle_notifier();
        self.apply_direct_hosting();
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
                    self.apply_settings(*settings).await;
                }
                MobileAccessCommand::StartPairing { requester, direct } => {
                    self.start_pairing(requester, direct).await;
                }
                MobileAccessCommand::RedeemDirectPairing { request, reply } => {
                    let result = self.redeem_direct_pairing(*request).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::AuthenticateDirectDevice { token, reply } => {
                    let _ = reply.send(self.authenticate_direct_device(&token));
                }
                MobileAccessCommand::CancelPairing { offer_id } => {
                    self.cancel_pairing(&offer_id).await;
                }
                MobileAccessCommand::RevokeDevice { device_id, reply } => {
                    let result = self.revoke_device(&device_id).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::RegisterPush {
                    device_id,
                    subscription,
                    reply,
                } => {
                    let result = self.register_push(&device_id, *subscription).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::UnregisterPush { device_id, reply } => {
                    let result = self.unregister_push(&device_id).await;
                    let _ = reply.send(result);
                }
                MobileAccessCommand::NotifyAgentIdle { notification } => {
                    self.notify_agent_idle(*notification).await;
                }
                MobileAccessCommand::PushSubscriptionGone { device_id } => {
                    self.mark_push_expired(&device_id).await;
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
                MobileAccessCommand::PairingHandoffAckRetry {
                    offer_id,
                    pairing_id,
                    attempt,
                } => {
                    self.handoff_ack_retry_task.take();
                    self.acknowledge_persisted_handoff(&offer_id, &pairing_id, attempt)
                        .await;
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
        self.direct_hosting = DirectHostingState::Disabled;
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
        let direct_hosting_before = self.direct_hosting_status();
        self.apply_direct_hosting();
        let direct_hosting_changed = self.direct_hosting_status() != direct_hosting_before;
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
                if direct_hosting_changed {
                    self.fan_out_state().await;
                }
                return;
            }
            self.disable_mobile_access().await;
            self.fan_out_state().await;
            return;
        }

        if !was_enabled || url_changed {
            self.enable_mobile_access().await;
            self.fan_out_state().await;
        } else if direct_hosting_changed {
            self.fan_out_state().await;
        }
    }

    /// Starts, stops, or restarts the direct mobile web server to match
    /// settings. A configuration or bind failure is recorded as
    /// [`DirectHostingState::Failed`] so it reaches the Mobile settings tab
    /// through the state payload rather than living only in the host log.
    fn apply_direct_hosting(&mut self) {
        if !self.settings.mobile_direct_hosting_enabled {
            if !matches!(self.direct_hosting, DirectHostingState::Disabled) {
                tracing::info!("mobile direct hosting disabled");
            }
            self.direct_hosting = DirectHostingState::Disabled;
            return;
        }
        // `enable_mobile_connections` is the master switch for every mobile
        // transport, not just the broker: turning it off has to stop the direct
        // origin too, or a paired phone would keep connecting over HTTP after
        // the user believes they cut mobile access off. Report it rather than
        // going quiet, or turning direct hosting on with the master switch off
        // looks like nothing happened.
        if !self.settings.enable_mobile_connections {
            if !matches!(self.direct_hosting, DirectHostingState::Disabled) {
                tracing::info!("mobile direct hosting stopped: mobile connections are off");
            }
            self.direct_hosting = DirectHostingState::Failed(
                "mobile connections are off; turn them on to serve the mobile app from this host"
                    .to_owned(),
            );
            return;
        }

        let desired = match self.desired_direct_hosting() {
            Ok(desired) => desired,
            Err(message) => {
                tracing::error!(error = %message, "mobile direct hosting misconfigured");
                self.direct_hosting = DirectHostingState::Failed(message);
                return;
            }
        };

        if let DirectHostingState::Running(running) = &self.direct_hosting
            && running.bind_addr == desired.0
            && running.bundle == desired.1
        {
            return;
        }

        // Release the running listener before binding, so restarting on the
        // same address does not fail against the socket we are replacing.
        self.direct_hosting = DirectHostingState::Disabled;
        self.direct_hosting = match start_direct_hosting(desired.0, desired.1, self.tx.clone()) {
            Ok(running) => {
                tracing::info!(
                    bind_addr = %running.server.local_addr(),
                    assets = running.asset_count,
                    "mobile direct hosting started"
                );
                DirectHostingState::Running(running)
            }
            Err(message) => {
                tracing::error!(error = %message, "mobile direct hosting failed to start");
                DirectHostingState::Failed(message)
            }
        };
    }

    fn desired_direct_hosting(&self) -> Result<(SocketAddr, DirectBundleChoice), String> {
        let bind_addr = resolve_bind_addr(self.settings.mobile_direct_bind_addr.as_deref())?;
        let configured = self
            .settings
            .mobile_direct_bundle_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let bundle = match configured {
            Some(dir) => DirectBundleChoice::Directory(PathBuf::from(dir)),
            None if MobileWebAssets::has_embedded() => DirectBundleChoice::BuiltIn,
            None => {
                return Err(
                    "mobile direct hosting is on but this build has no mobile web bundle in it; set the bundle directory to one built with tools/build-mobile-web-bundle.sh"
                        .to_owned(),
                );
            }
        };
        Ok((bind_addr, bundle))
    }

    fn direct_hosting_status(&self) -> MobileDirectHostingStatus {
        match &self.direct_hosting {
            DirectHostingState::Disabled => MobileDirectHostingStatus::Disabled,
            DirectHostingState::Running(running) => MobileDirectHostingStatus::Online {
                bind_addr: running.server.local_addr().to_string(),
                asset_count: running.asset_count,
                source: running.bundle.source(),
            },
            DirectHostingState::Failed(message) => MobileDirectHostingStatus::Error {
                message: message.clone(),
            },
        }
    }

    async fn enable_mobile_access(&mut self) {
        let endpoint = match dev_broker_endpoint(&self.settings) {
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
            self.resume_managed_pairing_handoff_if_needed();
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

    async fn start_pairing(&mut self, requester: StreamPath, direct: bool) {
        let offer_id = match new_offer_id() {
            Ok(offer_id) => offer_id,
            Err(message) => {
                tracing::warn!(error = %message, "failed to create mobile pairing offer id");
                return;
            }
        };

        // Direct pairing does not touch the broker, so it deliberately skips
        // every managed precondition below — including the mobile-connections
        // switch, which governs the broker path only.
        if direct {
            self.start_direct_pairing(requester, offer_id).await;
            return;
        }

        if !self.settings.enable_mobile_connections {
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::InvalidConfig,
                message: "mobile connections are disabled".to_owned(),
            };
            self.fan_out_state().await;
            return;
        }
        if let Some(pending) = self.pairings.pending_handoff_ack.clone()
            && now_ms().is_ok_and(|now| now >= pending.expires_at_ms)
            && let Some(broker_url) = self
                .pairings
                .devices
                .iter()
                .find(|record| record.device_id == pending.device_id)
                .map(|record| record.broker.url.clone())
        {
            self.expire_pending_handoff(
                &pending.offer_id,
                &pending.pairing_id,
                broker_url,
                "Managed mobile handoff acknowledgement expired".to_owned(),
                0,
            )
            .await;
            if self.pairings.pending_handoff_ack.is_some() {
                return;
            }
        }
        if self.pairings.pending_handoff_ack.is_some() {
            self.pairing = MobilePairingState::Failed {
                offer_id,
                code: MobileAccessErrorCode::PairingRejected,
                message: "A managed mobile handoff is still being acknowledged".to_owned(),
            };
            self.fan_out_state().await;
            return;
        }

        match dev_broker_endpoint(&self.settings) {
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

    /// Publishes a single-use, short-lived offer for a phone that will reach
    /// this host over its own HTTP origin.
    ///
    /// The QR carries only the offer secret. Redeeming it at the host exchanges
    /// that secret for a durable device token, so a QR photographed off a
    /// screen stops being a credential the moment it is used or expires —
    /// unlike a QR that simply contained the long-lived key.
    async fn start_direct_pairing(
        &mut self,
        requester: StreamPath,
        offer_id: MobilePairingOfferId,
    ) {
        if !self.settings.enable_mobile_connections {
            self.fail_pairing(
                offer_id,
                MobileAccessErrorCode::InvalidConfig,
                "mobile connections are disabled".to_owned(),
            )
            .await;
            return;
        }
        if !self.settings.mobile_direct_hosting_enabled {
            self.fail_pairing(
                offer_id,
                MobileAccessErrorCode::InvalidConfig,
                "direct hosting is turned off".to_owned(),
            )
            .await;
            return;
        }
        let origin = match self
            .settings
            .mobile_direct_public_origin
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(origin) => origin.to_owned(),
            None => {
                self.fail_pairing(
                    offer_id,
                    MobileAccessErrorCode::InvalidConfig,
                    "set the direct hosting public URL before pairing; the host cannot see the address your proxy publishes it under".to_owned(),
                )
                .await;
                return;
            }
        };
        let created_at_ms = match now_ms() {
            Ok(now) => now,
            Err(message) => {
                self.fail_pairing(offer_id, MobileAccessErrorCode::Internal, message)
                    .await;
                return;
            }
        };
        let release_version = match host_release_version_for_qr() {
            Ok(version) => version,
            Err(message) => {
                self.fail_pairing(offer_id, MobileAccessErrorCode::Internal, message)
                    .await;
                return;
            }
        };

        let expires_at_ms = created_at_ms.saturating_add(self.pairing_ttl.as_millis() as u64);
        let secret = new_shared_secret();
        let payload = DirectMobilePairingQrPayload::new(
            PROTOCOL_VERSION,
            release_version,
            offer_id.clone(),
            secret.clone(),
            HOST_LABEL.to_owned(),
            expires_at_ms,
        );
        let qr_uri = match payload.to_pairing_url(&origin) {
            Ok(uri) => MobilePairingQrUri(uri),
            Err(err) => {
                self.fail_pairing(
                    offer_id,
                    MobileAccessErrorCode::InvalidConfig,
                    format!("failed to encode direct pairing QR: {err}"),
                )
                .await;
                return;
            }
        };

        // One outstanding offer at a time, whichever transport asked for it.
        self.cancel_active_pairing_without_state();
        self.pairings.active_direct_pairing = Some(ActiveDirectMobilePairing {
            offer_id: offer_id.clone(),
            secret_hash: token_hash(&secret),
            created_at_ms,
            expires_at_ms,
        });
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            self.fail_pairing(offer_id, MobileAccessErrorCode::StoreLoadFailed, message)
                .await;
            return;
        }

        self.active_requester = Some(requester.clone());
        self.pairing = MobilePairingState::Active {
            offer_id: offer_id.clone(),
            expires_at_ms,
        };
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

    async fn fail_pairing(
        &mut self,
        offer_id: MobilePairingOfferId,
        code: MobileAccessErrorCode,
        message: String,
    ) {
        self.pairing = MobilePairingState::Failed {
            offer_id,
            code,
            message,
        };
        self.fan_out_state().await;
    }

    /// Exchanges a pairing offer secret for a durable device token.
    ///
    /// The offer is consumed whether or not this succeeds past the secret
    /// check, so a leaked QR cannot be retried against.
    async fn redeem_direct_pairing(
        &mut self,
        request: protocol::MobileDirectPairRequest,
    ) -> Result<protocol::MobileDirectPairResponse, MobileAccessCommandFailure> {
        if !self.settings.mobile_direct_hosting_enabled {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::InvalidConfig,
                "direct hosting is turned off",
            ));
        }
        let now = now_ms().map_err(|message| {
            MobileAccessCommandFailure::new(MobileAccessErrorCode::Internal, message)
        })?;
        let Some(active) = self.pairings.active_direct_pairing.clone() else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::PairingRejected,
                "no pairing code is waiting to be used",
            ));
        };
        if active.offer_id != request.offer_id {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::PairingRejected,
                "this pairing code is no longer the current one",
            ));
        }
        if now >= active.expires_at_ms {
            self.pairings.active_direct_pairing = None;
            let _ = self.pairings_store.save(&self.pairings);
            self.pairing = MobilePairingState::Expired {
                offer_id: active.offer_id,
            };
            self.fan_out_state().await;
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::PairingRejected,
                "this pairing code has expired; show a new one",
            ));
        }
        if !constant_time_eq(
            active.secret_hash.as_bytes(),
            token_hash(&request.offer_secret).as_bytes(),
        ) {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::PairingRejected,
                "this pairing code is not valid",
            ));
        }

        let label = request.device_label.trim();
        let label = if label.is_empty() {
            "Phone".to_owned()
        } else {
            label.chars().take(64).collect()
        };
        let device_id = new_device_id().map_err(|message| {
            MobileAccessCommandFailure::new(MobileAccessErrorCode::Internal, message)
        })?;
        let token = new_shared_secret();

        self.pairings
            .direct_devices
            .push(DirectMobilePairingRecord {
                device_id: device_id.clone(),
                token_hash: token_hash(&token),
                label,
                created_at_ms: now,
                last_seen_at_ms: None,
                state: MobileDeviceState::Paired,
                key_fingerprint: direct_key_fingerprint(&token),
                push: None,
            });
        // Single use: the offer is spent even though the device has not
        // connected yet.
        self.pairings.active_direct_pairing = None;
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;

        self.pairing = MobilePairingState::Consumed {
            offer_id: active.offer_id,
        };
        self.fan_out_state().await;

        Ok(protocol::MobileDirectPairResponse {
            device_id,
            device_token: protocol::MobileDeviceToken(token),
            host_label: "Tyde Host".to_owned(),
            protocol_version: PROTOCOL_VERSION,
        })
    }

    fn authenticate_direct_device(&self, token: &str) -> Option<MobileDeviceId> {
        if !self.settings.mobile_direct_hosting_enabled {
            return None;
        }
        self.pairings
            .direct_device_for_token(token)
            .map(|record| record.device_id.clone())
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
        let host_label = HOST_LABEL.to_owned();
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
        if !self.pairings.remove_device(device_id) {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        }
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
        let Some(device) = self.pairings.device_mut(device_id) else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        };
        *device.label = label;
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;
        self.fan_out_state().await;
        Ok(())
    }

    /// Watches agent status edges and turns "finished a turn with nothing
    /// queued" into a notification command. Lives in its own task so the host
    /// lookups it needs cannot block the actor loop.
    fn spawn_idle_notifier(&mut self) {
        let Some(mut transitions) = self.agent_status_transitions.take() else {
            return;
        };
        let host = self.host.clone();
        let tx = self.tx.clone();
        self.idle_notifier_task = Some(tokio::spawn(async move {
            loop {
                let transition = match transitions.recv().await {
                    Ok(transition) => transition,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::error!(
                            missed,
                            "mobile push notifier fell behind agent status transitions; \
                             notifications for those turns were not delivered"
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };

                if transition.from != AgentControlStatus::Thinking
                    || transition.to != AgentControlStatus::Idle
                {
                    continue;
                }
                // Another message is already queued behind this turn, so the
                // agent resumes immediately and is not really idle.
                if transition.has_queued_messages {
                    continue;
                }
                // Reopening a saved session lands on Idle without the agent
                // having done anything.
                if transition.restored_without_live_turn {
                    continue;
                }

                let reason = match transition.pending_user_response {
                    Some(PendingUserResponseKind::UserQuestion) => {
                        MobilePushReason::QuestionPending
                    }
                    Some(PendingUserResponseKind::PlanApproval) => MobilePushReason::PlanApproval,
                    None => MobilePushReason::TurnComplete,
                };
                let Some(start) = host.agent_start_snapshot(&transition.agent_id).await else {
                    // The agent was removed between the edge and this lookup.
                    continue;
                };
                // Only agents the user is personally waiting on. Orchestrated,
                // team, workflow, and backend-native sub-agents have a parent
                // agent waiting on them instead, and a workflow fanning out to
                // a dozen of them would otherwise buzz the phone a dozen times.
                match start.origin {
                    AgentOrigin::User => {}
                    AgentOrigin::AgentControl
                    | AgentOrigin::BackendNative
                    | AgentOrigin::TeamMember
                    | AgentOrigin::Workflow => continue,
                }

                let _ = tx.send(MobileAccessCommand::NotifyAgentIdle {
                    notification: Box::new(MobilePushNotification {
                        agent_id: transition.agent_id,
                        agent_name: start.name,
                        host_label: HOST_LABEL.to_owned(),
                        reason,
                    }),
                });
            }
        }));
    }

    async fn register_push(
        &mut self,
        device_id: &MobileDeviceId,
        subscription: MobilePushSubscription,
    ) -> Result<(), MobileAccessCommandFailure> {
        let now = now_ms().map_err(|message| {
            MobileAccessCommandFailure::new(MobileAccessErrorCode::Internal, message)
        })?;
        let Some(device) = self.pairings.device_mut(device_id) else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        };
        // Re-registering clears `expired`: the device just proved it holds a
        // live subscription, which is exactly what the flag denies.
        *device.push = Some(DevicePushRegistration {
            subscription,
            registered_at_ms: now,
            expired: false,
        });
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;
        self.fan_out_state().await;
        Ok(())
    }

    async fn unregister_push(
        &mut self,
        device_id: &MobileDeviceId,
    ) -> Result<(), MobileAccessCommandFailure> {
        let Some(device) = self.pairings.device_mut(device_id) else {
            return Err(MobileAccessCommandFailure::new(
                MobileAccessErrorCode::UnknownDevice,
                format!("unknown mobile device {device_id}"),
            ));
        };
        *device.push = None;
        self.pairings_store
            .save(&self.pairings)
            .map_err(|message| {
                MobileAccessCommandFailure::new(MobileAccessErrorCode::StoreLoadFailed, message)
            })?;
        self.fan_out_state().await;
        Ok(())
    }

    async fn mark_push_expired(&mut self, device_id: &MobileDeviceId) {
        let Some(device) = self.pairings.device_mut(device_id) else {
            return;
        };
        let Some(push) = device.push.as_mut() else {
            return;
        };
        if push.expired {
            return;
        }
        push.expired = true;
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            tracing::error!(
                %device_id,
                %message,
                "failed to persist expired push subscription"
            );
        }
        self.fan_out_state().await;
    }

    /// Fans a notification out to every paired device that is not currently
    /// connected. A connected device has the app open and already shows the
    /// agent, so buzzing it would be noise.
    async fn notify_agent_idle(&mut self, notification: MobilePushNotification) {
        // Mobile access off means no device can connect, so a notification
        // would open an app that cannot reach this host.
        if !self.settings.enable_mobile_connections {
            return;
        }
        let now_secs = match now_ms() {
            Ok(millis) => millis / 1000,
            Err(message) => {
                tracing::error!(%message, "cannot sign push notification without a usable clock");
                return;
            }
        };
        let targets: Vec<(MobileDeviceId, MobilePushSubscription)> = self
            .pairings
            .push_targets()
            .into_iter()
            .filter(|(device_id, _, state)| {
                *state != MobileDeviceState::Revoked
                    && !self.connected_tasks.contains_key(device_id)
            })
            .map(|(device_id, subscription, _)| (device_id, subscription))
            .collect();
        if targets.is_empty() {
            tracing::info!(
                agent_id = %notification.agent_id,
                reason = ?notification.reason,
                "no disconnected device holds a live push subscription; agent idle not delivered"
            );
        }

        for (device_id, subscription) in targets {
            let client = self.push_client.clone();
            let tx = self.tx.clone();
            let notification = notification.clone();
            // Delivery is a network round trip to a third-party push service;
            // holding the actor loop for it would stall every other command.
            tokio::spawn(async move {
                match send_push(&client, &subscription, &notification, now_secs).await {
                    Ok(()) => {
                        tracing::info!(
                            %device_id,
                            agent_id = %notification.agent_id,
                            reason = ?notification.reason,
                            "delivered mobile push notification"
                        );
                    }
                    Err(PushSendError::SubscriptionGone) => {
                        tracing::warn!(
                            %device_id,
                            agent_id = %notification.agent_id,
                            "push service reports the device subscription is gone; marking it expired"
                        );
                        let _ = tx.send(MobileAccessCommand::PushSubscriptionGone { device_id });
                    }
                    Err(error) => {
                        tracing::error!(
                            %device_id,
                            %error,
                            "failed to deliver mobile push notification"
                        );
                    }
                }
            });
        }
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
                    push: None,
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
            push: None,
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
        self.spawn_connected_bridge(device_id.clone(), Box::new(stream));
        self.spawn_device_accept(device_id);
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn device_transport_connected(
        &mut self,
        device_id: &MobileDeviceId,
        stream: BoxedMobileTransport,
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
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
            return;
        }
        let Some(managed) = active.managed.as_ref() else {
            return;
        };
        if handoff.host_broker_credentials.scope.role != ManagedBrokerRole::Host
            || handoff.host_broker_credentials.issued_at_ms
                >= handoff.host_broker_credentials.expires_at_ms
        {
            let message =
                "managed mobile handoff contained invalid host broker credentials".to_owned();
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::ServiceUnavailable,
                message: message.clone(),
            };
            self.broker_status = MobileBrokerStatus::Error {
                broker_url: Some(handoff.broker.endpoint),
                code: MobileAccessErrorCode::ServiceUnavailable,
                message,
            };
            self.fan_out_state().await;
            return;
        }
        if let Some(task) = self.pairing_ttl_task.take() {
            task.abort();
        }
        let active = active.clone();
        let pairing_id = handoff.pairing_id.clone();
        let handoff_expires_at_ms = handoff.handoff_expires_at_ms;
        let device_id = handoff.device_id.clone();
        let broker_url = handoff.broker.endpoint.clone();
        let record = MobilePairingRecord {
            device_id: device_id.clone(),
            broker: BrokerEndpoint {
                url: broker_url.clone(),
                auth: BrokerAuth::Anonymous,
            },
            room: active.room,
            psk: active.psk.clone(),
            label: handoff.device_label,
            created_at_ms: handoff.device_created_at_ms,
            last_seen_at_ms: handoff.device_last_seen_at_ms,
            state: MobileDeviceState::Paired,
            key_fingerprint: active.key_fingerprint.clone(),
            push: None,
            managed: Some(ManagedMobilePairingCredential {
                pairing_id: pairing_id.clone(),
                host_pairing_secret: handoff.host_pairing_secret,
                broker: handoff.broker,
            }),
        };
        let pending_ack = PendingManagedMobileHandoffAck {
            offer_id: offer_id.clone(),
            host_offer_token: managed.host_offer_token.clone(),
            pairing_id: pairing_id.clone(),
            device_id: device_id.clone(),
            expires_at_ms: handoff_expires_at_ms,
        };
        let mut persisted_pairings = self.pairings.clone();
        let insert = match persisted_pairings.insert_managed_record(record) {
            Ok(insert) => insert,
            Err(message) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id: offer_id.clone(),
                    code: MobileAccessErrorCode::DuplicateDevice,
                    message: message.clone(),
                };
                self.broker_status = MobileBrokerStatus::Error {
                    broker_url: Some(broker_url),
                    code: MobileAccessErrorCode::DuplicateDevice,
                    message,
                };
                self.fan_out_state().await;
                return;
            }
        };
        persisted_pairings.active_pairing = None;
        persisted_pairings.pending_handoff_ack = Some(pending_ack);
        if let Err(message) = self.pairings_store.save(&persisted_pairings) {
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message: message.clone(),
            };
            self.broker_status = MobileBrokerStatus::Error {
                broker_url: Some(broker_url),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            self.spawn_offer_poll_after(active, OFFER_POLL_INTERVAL);
            self.fan_out_state().await;
            return;
        }
        self.pairings = persisted_pairings;
        if let Some(task) = self.offer_poll_task.take() {
            task.abort();
        }
        self.active_requester = None;
        self.pairing = MobilePairingState::Active {
            offer_id: offer_id.clone(),
            expires_at_ms: handoff_expires_at_ms,
        };
        self.broker_status = MobileBrokerStatus::Connecting {
            broker_url: broker_url.clone(),
        };
        self.spawn_device_accept(device_id);
        if insert == ManagedMobilePairingRecordInsert::Inserted {
            self.fan_out_state().await;
        }

        self.acknowledge_persisted_handoff(offer_id, &pairing_id, 0)
            .await;
    }

    async fn acknowledge_persisted_handoff(
        &mut self,
        offer_id: &MobilePairingOfferId,
        pairing_id: &str,
        attempt: u32,
    ) {
        let Some(pending) = self.pairings.pending_handoff_ack.clone() else {
            return;
        };
        if &pending.offer_id != offer_id || pending.pairing_id != pairing_id {
            return;
        }
        let Some(record) = self
            .pairings
            .devices
            .iter()
            .find(|record| record.device_id == pending.device_id)
            .cloned()
        else {
            let message = format!(
                "managed mobile handoff {pairing_id} was not durably persisted before acknowledgement"
            );
            self.pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::RepairRequired,
                message: message.clone(),
            };
            self.broker_status = MobileBrokerStatus::Error {
                broker_url: None,
                code: MobileAccessErrorCode::RepairRequired,
                message,
            };
            self.fan_out_state().await;
            return;
        };
        let device_id = record.device_id;
        let broker_url = record.broker.url;
        if now_ms().is_ok_and(|now| now >= pending.expires_at_ms) {
            self.expire_pending_handoff(
                offer_id,
                pairing_id,
                broker_url,
                "Managed mobile handoff acknowledgement expired".to_owned(),
                attempt,
            )
            .await;
            return;
        }
        if let Err(error) = self
            .managed_service
            .acknowledge_host_handoff(offer_id, &pending.host_offer_token, pairing_id)
            .await
        {
            if error.code == MobileAccessErrorCode::PairingExpired {
                self.expire_pending_handoff(
                    offer_id,
                    pairing_id,
                    broker_url,
                    error.message,
                    attempt,
                )
                .await;
                return;
            }
            let pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: error.code,
                message: error.message.clone(),
            };
            let broker_status = MobileBrokerStatus::Error {
                broker_url: Some(broker_url),
                code: error.code,
                message: error.message,
            };
            let changed = self.pairing != pairing || self.broker_status != broker_status;
            self.pairing = pairing;
            self.broker_status = broker_status;
            if error.code == MobileAccessErrorCode::ServiceUnavailable {
                self.schedule_handoff_ack_retry(
                    offer_id.clone(),
                    pairing_id.to_owned(),
                    attempt.saturating_add(1),
                    handoff_ack_retry_delay(attempt),
                );
            }
            if changed {
                self.fan_out_state().await;
            }
            return;
        }

        let mut persisted_pairings = self.pairings.clone();
        persisted_pairings.pending_handoff_ack = None;
        if let Err(message) = self.pairings_store.save(&persisted_pairings) {
            let pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message: message.clone(),
            };
            let broker_status = MobileBrokerStatus::Error {
                broker_url: Some(broker_url),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            let changed = self.pairing != pairing || self.broker_status != broker_status;
            self.pairing = pairing;
            self.broker_status = broker_status;
            self.schedule_handoff_ack_retry(
                offer_id.clone(),
                pairing_id.to_owned(),
                attempt.saturating_add(1),
                handoff_ack_retry_delay(attempt),
            );
            if changed {
                self.fan_out_state().await;
            }
            return;
        }
        self.pairings = persisted_pairings;
        self.pairing = MobilePairingState::Consumed {
            offer_id: offer_id.clone(),
        };
        if !self.connected_tasks.contains_key(&device_id) {
            self.broker_status = MobileBrokerStatus::Connecting { broker_url };
        }
        self.fan_out_state().await;
        self.schedule_pairing_grace(offer_id.clone());
    }

    async fn expire_pending_handoff(
        &mut self,
        offer_id: &MobilePairingOfferId,
        pairing_id: &str,
        broker_url: BrokerUrl,
        expiry_message: String,
        attempt: u32,
    ) {
        let mut persisted_pairings = self.pairings.clone();
        persisted_pairings.pending_handoff_ack = None;
        if let Err(message) = self.pairings_store.save(&persisted_pairings) {
            let pairing = MobilePairingState::Failed {
                offer_id: offer_id.clone(),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message: message.clone(),
            };
            let broker_status = MobileBrokerStatus::Error {
                broker_url: Some(broker_url),
                code: MobileAccessErrorCode::StoreLoadFailed,
                message,
            };
            let changed = self.pairing != pairing || self.broker_status != broker_status;
            self.pairing = pairing;
            self.broker_status = broker_status;
            self.schedule_handoff_ack_retry(
                offer_id.clone(),
                pairing_id.to_owned(),
                attempt.saturating_add(1),
                handoff_ack_retry_delay(attempt),
            );
            if changed {
                self.fan_out_state().await;
            }
            return;
        }
        self.pairings = persisted_pairings;
        let pairing = MobilePairingState::Failed {
            offer_id: offer_id.clone(),
            code: MobileAccessErrorCode::PairingExpired,
            message: expiry_message,
        };
        let changed = self.pairing != pairing;
        self.pairing = pairing;
        if changed {
            self.fan_out_state().await;
        }
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
        let broker_url = self
            .pairings
            .devices
            .iter()
            .find(|record| &record.device_id == device_id)
            .map(|record| record.broker.url.clone());
        let Some(device) = self.pairings.device_mut(device_id) else {
            return false;
        };
        *device.state = MobileDeviceState::Connected;
        if let Some(now) = now {
            *device.last_seen_at_ms = Some(now);
        }
        if let Err(message) = self.pairings_store.save(&self.pairings) {
            tracing::warn!(error = %message, "failed to persist mobile device connection state");
        }
        // Only an MQTT pairing has a broker whose health this reports; a direct
        // device reached the host without one.
        if let Some(broker_url) = broker_url {
            self.broker_status = MobileBrokerStatus::Online { broker_url };
        }
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
        let is_mqtt_device = self
            .pairings
            .devices
            .iter()
            .any(|record| &record.device_id == device_id);
        if let Some(device) = self.pairings.device_mut(device_id) {
            if *device.state == MobileDeviceState::Connected {
                *device.state = MobileDeviceState::Paired;
            }
            if let Err(message) = self.pairings_store.save(&self.pairings) {
                tracing::warn!(error = %message, "failed to persist mobile device disconnect state");
            }
        }
        // The host re-arms an MQTT accept itself. A direct device owns its own
        // reconnect, so waiting on one here would never resolve.
        if is_mqtt_device && self.settings.enable_mobile_connections {
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

    fn resume_managed_pairing_handoff_if_needed(&mut self) {
        if let Some(pending) = self.pairings.pending_handoff_ack.clone() {
            self.schedule_handoff_ack_retry(
                pending.offer_id,
                pending.pairing_id,
                0,
                Duration::ZERO,
            );
            return;
        }
        if let Some(active) = self.pairings.active_pairing.clone()
            && active.managed.is_some()
        {
            self.spawn_offer_poll(active);
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

    fn spawn_connected_bridge(&mut self, device_id: MobileDeviceId, stream: BoxedMobileTransport) {
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
        self.spawn_offer_poll_after(credential, Duration::ZERO);
    }

    fn spawn_offer_poll_after(
        &mut self,
        credential: ActiveMobilePairingCredential,
        initial_delay: Duration,
    ) {
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
            initial_delay,
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

    fn schedule_handoff_ack_retry(
        &mut self,
        offer_id: MobilePairingOfferId,
        pairing_id: String,
        attempt: u32,
        delay: Duration,
    ) {
        if let Some(task) = self.handoff_ack_retry_task.take() {
            task.abort();
        }
        let tx = self.tx.clone();
        self.handoff_ack_retry_task = Some(tokio::spawn(async move {
            sleep(delay).await;
            let _ = tx.send(MobileAccessCommand::PairingHandoffAckRetry {
                offer_id,
                pairing_id,
                attempt,
            });
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
        if let Some(task) = self.handoff_ack_retry_task.take() {
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
        if let Some(task) = self.handoff_ack_retry_task.take() {
            task.abort();
        }
        if let Some(task) = self.idle_notifier_task.take() {
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
            direct_hosting: self.direct_hosting_status(),
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
                        stream: Box::new(stream),
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
    initial_delay: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        sleep(initial_delay).await;
        loop {
            let result = managed_service
                .poll_host_offer(&offer_id, &host_offer_token)
                .await;
            match result {
                Ok(response) => match managed_handoff_from_poll_response(&offer_id, response) {
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

#[derive(Debug)]
enum ManagedPollOutcome {
    Pending,
    Redeemed(Box<ManagedMobilePairingHandoff>),
    Terminal(ManagedOfferTerminalState),
}

fn managed_handoff_from_poll_response(
    expected_offer_id: &MobilePairingOfferId,
    response: PollHostOfferResponse,
) -> Result<ManagedPollOutcome, ManagedServiceError> {
    let offer_id = MobilePairingOfferId::new(response.offer_id).map_err(|err| {
        ManagedServiceError::new(
            MobileAccessErrorCode::ServiceUnavailable,
            format!("managed mobile service returned invalid polled offer id: {err}"),
        )
    })?;
    if &offer_id != expected_offer_id {
        return Err(ManagedServiceError::new(
            MobileAccessErrorCode::ServiceUnavailable,
            format!(
                "managed mobile service returned offer {offer_id} while polling {expected_offer_id}"
            ),
        ));
    }
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
            let host_handoff = response.host_handoff.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed mobile offer {offer_id} omitted host_handoff state"),
                )
            })?;
            if host_handoff.expires_at_ms == 0 {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!("managed mobile offer {offer_id} returned invalid handoff expiry"),
                ));
            }
            match host_handoff.status {
                HostHandoffStatus::Available => {}
                HostHandoffStatus::Expired => {
                    return Ok(ManagedPollOutcome::Terminal(
                        ManagedOfferTerminalState::Expired,
                    ));
                }
                HostHandoffStatus::Acknowledged => {
                    return Err(ManagedServiceError::new(
                        MobileAccessErrorCode::RepairRequired,
                        format!(
                            "managed mobile offer {offer_id} handoff was acknowledged before durable receipt"
                        ),
                    ));
                }
            }
            let pairing_id = response.pairing_id.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} was redeemed without pairing id"),
                )
            })?;
            if pairing_id.trim().is_empty() {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} returned an empty pairing id"),
                ));
            }
            let host_pairing_secret = response.host_pairing_secret.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} handoff was already consumed"),
                )
            })?;
            if host_pairing_secret.trim().is_empty() {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!(
                        "managed mobile offer {offer_id} returned an empty host pairing secret"
                    ),
                ));
            }
            let device = response.device.ok_or_else(|| {
                ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} was redeemed without device summary"),
                )
            })?;
            if device.device_id.trim().is_empty()
                || device.label.trim().is_empty()
                || device.created_at_ms == 0
            {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::RepairRequired,
                    format!("managed mobile offer {offer_id} returned an invalid device summary"),
                ));
            }
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
            if host_broker_credentials.scope.role != ManagedBrokerRole::Host
                || host_broker_credentials.issued_at_ms >= host_broker_credentials.expires_at_ms
            {
                return Err(ManagedServiceError::new(
                    MobileAccessErrorCode::ServiceUnavailable,
                    format!(
                        "managed mobile offer {offer_id} returned invalid host broker credentials"
                    ),
                ));
            }
            Ok(ManagedPollOutcome::Redeemed(Box::new(
                ManagedMobilePairingHandoff {
                    pairing_id,
                    host_pairing_secret,
                    handoff_expires_at_ms: host_handoff.expires_at_ms,
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
    stream: BoxedMobileTransport,
) {
    match accept(&ServerConfig::current(), stream).await {
        Ok(connection) => {
            if let Err(err) = run_mobile_connection(connection, host, device_id.clone()).await {
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

fn dev_broker_endpoint(settings: &HostSettings) -> Result<Option<BrokerEndpoint>, String> {
    let Some(url) = settings.mobile_broker_url.as_ref() else {
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
    let auth = match settings.mobile_broker_auth.password.as_ref() {
        Some(password) => BrokerAuth::UsernamePassword {
            username: settings.mobile_broker_auth.username.clone(),
            password: password.expose().to_owned(),
        },
        None => BrokerAuth::Anonymous,
    };
    Ok(Some(BrokerEndpoint {
        url: url.clone(),
        auth,
    }))
}

fn initial_enabled_broker_status(
    pairings: &MobilePairings,
    settings: &HostSettings,
) -> MobileBrokerStatus {
    match dev_broker_endpoint(settings) {
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
    if let Some(broker_url) = pairings
        .active_pairing
        .as_ref()
        .and_then(|active| active.managed.as_ref())
        .map(|managed| managed.broker.endpoint.clone())
    {
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

fn handoff_ack_retry_delay(attempt: u32) -> Duration {
    HANDOFF_ACK_RETRY_INITIAL
        .saturating_mul(2_u32.saturating_pow(attempt.min(31)))
        .min(HANDOFF_ACK_RETRY_MAX)
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

/// A 244-bit secret as lowercase hex. Two v4 UUIDs rather than a hand-rolled
/// RNG so the entropy comes from the same source the rest of the host trusts
/// for identifiers.
fn new_shared_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
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

fn start_direct_hosting(
    bind_addr: SocketAddr,
    bundle: DirectBundleChoice,
    mobile_access: mpsc::UnboundedSender<MobileAccessCommand>,
) -> Result<RunningDirectHost, String> {
    let assets = match &bundle {
        DirectBundleChoice::Directory(dir) => MobileWebAssets::from_dir(dir)?,
        DirectBundleChoice::BuiltIn => MobileWebAssets::embedded()
            .ok_or_else(|| "this build has no mobile web bundle in it".to_owned())?,
    };
    let asset_count = assets.asset_count() as u32;
    let server = MobileHttpServer::start(bind_addr, Arc::new(assets), mobile_access)?;
    Ok(RunningDirectHost {
        server,
        bind_addr,
        bundle,
        asset_count,
    })
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
