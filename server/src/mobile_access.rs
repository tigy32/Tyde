use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use fs2::FileExt;
use mqtt_transport::{
    BrokerAuth, BrokerEndpoint, EnvelopeStream, MobilePairingQrPayload, MqttConnectConfig,
    ParticipantRole, PreSharedKey, RoomId, default_mobile_broker_endpoint, validate_broker_url,
};
use protocol::{
    BrokerUrl, FrameKind, HostSettings, MobileAccessErrorCode, MobileAccessStatePayload,
    MobileBrokerStatus, MobileDeviceId, MobileDeviceRenamePayload, MobileDeviceRevokePayload,
    MobileDeviceState, MobilePairingCancelPayload, MobilePairingOfferId, MobilePairingOfferPayload,
    MobilePairingQrUri, MobilePairingState, PROTOCOL_VERSION, StreamPath,
};
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
    ActiveMobilePairingCredential, MobilePairingRecord, MobilePairings, MobilePairingsStore,
    key_fingerprint,
};
use crate::stream::{Stream, StreamClosed};

pub(crate) const DEFAULT_PAIRING_TTL: Duration = Duration::from_secs(120);
const PAIRING_TERMINAL_GRACE: Duration = Duration::from_millis(250);
const ACCEPT_RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const ACCEPT_RECONNECT_MAX: Duration = Duration::from_secs(30);

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
        let broker_status = if init.initial_settings.enable_mobile_connections {
            match effective_broker_endpoint(init.initial_settings.mobile_broker_url.as_ref()) {
                Ok(endpoint) => MobileBrokerStatus::Online {
                    broker_url: endpoint.url,
                },
                Err(message) => MobileBrokerStatus::Error {
                    broker_url: init.initial_settings.mobile_broker_url.clone(),
                    code: MobileAccessErrorCode::InvalidConfig,
                    message,
                },
            }
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
        let endpoint = match effective_broker_endpoint(self.settings.mobile_broker_url.as_ref()) {
            Ok(endpoint) => {
                self.broker_status = MobileBrokerStatus::Online {
                    broker_url: endpoint.url.clone(),
                };
                endpoint
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
                        broker_url: Some(endpoint.url),
                        code: MobileAccessErrorCode::BrokerUnavailable,
                        message,
                    };
                    return;
                }
            }
        }

        self.spawn_active_pairing_accept_if_needed();
        self.spawn_device_accepts_if_needed();
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

        let broker = match effective_broker_endpoint(self.settings.mobile_broker_url.as_ref()) {
            Ok(broker) => broker,
            Err(message) => {
                self.pairing = MobilePairingState::Failed {
                    offer_id,
                    code: MobileAccessErrorCode::InvalidConfig,
                    message,
                };
                self.fan_out_state().await;
                return;
            }
        };
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

    async fn cancel_pairing(&mut self, offer_id: &MobilePairingOfferId) {
        let Some(active) = self.pairings.active_pairing.as_ref() else {
            return;
        };
        if &active.offer_id != offer_id {
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
        };
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
        let task = spawn_device_accept_task(self.tx.clone(), record);
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
        let result = connect_mqtt_host_stream(
            credential.broker.clone(),
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
    record: MobilePairingRecord,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = ACCEPT_RECONNECT_INITIAL;
        loop {
            match connect_mqtt_host_stream(record.broker.clone(), record.room, record.psk.clone())
                .await
            {
                Ok(stream) => {
                    let _ = tx.send(MobileAccessCommand::DeviceTransportConnected {
                        device_id: record.device_id.clone(),
                        stream,
                    });
                    return;
                }
                Err(error) => {
                    let _ = tx.send(MobileAccessCommand::DeviceAcceptFailed {
                        device_id: record.device_id.clone(),
                        code: error.code,
                        message: error.message,
                    });
                    let delay = jittered_backoff(backoff);
                    sleep(delay).await;
                    backoff = backoff.saturating_mul(2).min(ACCEPT_RECONNECT_MAX);
                }
            }
        }
    })
}

async fn connect_mqtt_host_stream(
    broker: BrokerEndpoint,
    room: RoomId,
    psk: PreSharedKey,
) -> Result<EnvelopeStream, MobileTaskError> {
    let config = MqttConnectConfig {
        endpoint: broker,
        room,
        psk,
        role: ParticipantRole::Host,
    };
    mqtt_transport::connect_ephemeral(config)
        .await
        .map_err(|err| MobileTaskError::transport(format!("MQTT mobile transport failed: {err}")))
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

fn effective_broker_endpoint(configured: Option<&BrokerUrl>) -> Result<BrokerEndpoint, String> {
    let endpoint = match configured {
        Some(url) => BrokerEndpoint {
            url: url.clone(),
            auth: BrokerAuth::Anonymous,
        },
        None => default_mobile_broker_endpoint(),
    };
    validate_broker_url(&endpoint.url).map_err(|err| err.to_string())?;
    Ok(endpoint)
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

    fn endpoint() -> BrokerEndpoint {
        BrokerEndpoint {
            url: BrokerUrl::new("mqtts://127.0.0.1:9").expect("broker URL"),
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
    async fn enabling_uses_default_emqx_mqtt_broker() {
        let mut test = test_actor();
        test.actor.apply_settings(test_settings(true)).await;

        assert!(matches!(
            &test.actor.broker_status,
            MobileBrokerStatus::Online { broker_url }
                if broker_url.as_str() == mqtt_transport::DEFAULT_MOBILE_MQTT_BROKER_URL
        ));
        assert!(!test.actor.accept_tasks.is_empty() || test.actor.pairings.devices.is_empty());
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
}
