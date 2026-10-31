//! Bounded direct-host apparatus. Wire payloads remain production protocol types.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use client::{ClientConfig, Connection};
use protocol::{
    Envelope, FrameKind, HelloPayload, HostBootstrapPayload, HostSettingsPayload,
    MobileAccessStatePayload, MobileDeviceRevokePayload, MobileDirectHostingStatus,
    MobilePairingOfferPayload, MobilePairingStartPayload, MobilePairingState,
    MobileWebBundleSource, SettingExpectation, SettingOp, SettingsWriteId, SettingsWritePayload,
    SettingsWriteResultPayload, StreamPath, TydeReleaseVersion, Version, WelcomePayload,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::BufReader;
use tokio::net::TcpStream;

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub protocol_version: u32,
    pub tyde_version: Version,
    pub release_version: TydeReleaseVersion,
    pub project_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunConfig {
    ready_path: PathBuf,
    expected: Expected,
    bundle_dir: PathBuf,
    inventory: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    run: PathBuf,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Configure,
    SetOrigin {
        previous: PathBuf,
        origin_path: PathBuf,
    },
    Offer {
        previous: PathBuf,
        offer_path: PathBuf,
    },
    Revoke {
        previous: PathBuf,
    },
    Disable {
        previous: PathBuf,
    },
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Configured,
    OriginSet,
    Offered,
    Revoked,
    Disabled,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    run_sha256: String,
    ready_sha256: String,
    phase: Phase,
    welcome: WelcomePayload,
    settings: HostSettingsPayload,
    mobile_access: MobileAccessStatePayload,
    offer: Option<MobilePairingOfferPayload>,
}

fn require(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.to_owned()) }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_parent(path: &Path) -> Result<()> {
    require(path.is_absolute(), "private path must be absolute")?;
    let parent = path.parent().ok_or("private path has no parent")?;
    let meta = fs::symlink_metadata(parent).map_err(|_| "cannot stat private directory")?;
    require(
        meta.is_dir()
            && meta.permissions().mode() & 0o777 == 0o700
            && meta.uid() == unsafe { libc::geteuid() },
        "private directory must be owned mode 0700",
    )?;
    require(
        fs::canonicalize(parent).map_err(|_| "cannot resolve private directory")? == parent,
        "private directory must be canonical",
    )
}

fn read_private(path: &Path) -> Result<Vec<u8>> {
    private_parent(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "cannot open private input")?;
    let meta = file.metadata().map_err(|_| "cannot stat private input")?;
    require(
        meta.is_file()
            && meta.permissions().mode() & 0o777 == 0o600
            && meta.uid() == unsafe { libc::geteuid() },
        "private input must be owned mode 0600",
    )?;
    std::io::read_to_string(file)
        .map(String::into_bytes)
        .map_err(|_| "cannot read private input".into())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|_| "invalid private JSON or typed payload".into())
}

fn reserve(path: &Path) -> Result<File> {
    private_parent(path)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "private output already exists or cannot be created".into())
}

fn save<T: Serialize>(file: &mut File, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *file, value).map_err(|_| "cannot serialize private output")?;
    file.write_all(b"\n")
        .map_err(|_| "cannot write private output")?;
    file.sync_all()
        .map_err(|_| "cannot sync private output".into())
}

/// Public Ready is devtools JSON, not a second Tyde wire schema. Only the
/// saved, decoded canonical wait result is accepted, never an endpoint override.
fn ready_endpoint(bytes: &[u8], expected: &Expected) -> Result<SocketAddr> {
    use devtools_protocol::{
        DevFrontendAttestation, DevFrontendProtocol, DevInstanceId, DevInstanceStatus,
        DevNativeStateAttestation, verify_dev_application_directories,
    };
    let ready: Value = decode(bytes)?;
    require(
        matches!(
            serde_json::from_value::<DevInstanceStatus>(ready["status"].clone()),
            Ok(DevInstanceStatus::Ready)
        ),
        "saved canonical instance is not Ready",
    )?;
    require(
        ready["projectDir"].as_str() == expected.project_dir.to_str()
            && ready["storesEphemeral"] == true,
        "Ready project or ephemeral custody mismatch",
    )?;
    let id: DevInstanceId = serde_json::from_value(ready["instanceId"].clone())
        .map_err(|_| "Ready lacks instance identity")?;
    let native: DevNativeStateAttestation = serde_json::from_value(ready["nativeState"].clone())
        .map_err(|_| "Ready lacks native attestation")?;
    native
        .verify_isolated(&id)
        .map_err(|_| "Ready native isolation mismatch")?;
    let dirs: Vec<PathBuf> = serde_json::from_value(ready["applicationDirectories"].clone())
        .map_err(|_| "Ready lacks application directories")?;
    verify_dev_application_directories(&id, &dirs)
        .map_err(|_| "Ready directory ownership mismatch")?;
    let build: DevFrontendAttestation = serde_json::from_value(ready["frontendBuild"].clone())
        .map_err(|_| "Ready lacks frontend build")?;
    require(
        build.protocol == DevFrontendProtocol::OwnedBuildServeV1
            && build.index_sha256.len() == 64
            && build.index_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "Ready frontend contract mismatch",
    )?;
    let store = Path::new(
        ready["storeDir"]
            .as_str()
            .ok_or("Ready lacks store directory")?,
    );
    require(
        store.is_absolute()
            && ready["sessionStorePath"].as_str() == store.join("sessions.json").to_str(),
        "Ready store custody mismatch",
    )?;
    let endpoint: SocketAddr = ready["hostAddr"]
        .as_str()
        .ok_or("Ready lacks host endpoint")?
        .parse()
        .map_err(|_| "invalid Ready host endpoint")?;
    let ui: SocketAddr = ready["uiDebugAddr"]
        .as_str()
        .ok_or("Ready lacks UI endpoint")?
        .parse()
        .map_err(|_| "invalid Ready UI endpoint")?;
    require(
        endpoint.ip().is_loopback()
            && endpoint.port() != 0
            && ui.ip().is_loopback()
            && ui.port() != 0
            && endpoint != ui,
        "Ready endpoints must be distinct bound loopbacks",
    )?;
    Ok(endpoint)
}

fn inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    require(
        root.is_absolute() && fs::canonicalize(root).map_err(|_| "cannot resolve bundle")? == root,
        "bundle root must be canonical and absolute",
    )?;
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(dir).map_err(|_| "cannot read bundle directory")? {
            let entry = entry.map_err(|_| "cannot read bundle entry")?;
            let kind = entry.file_type().map_err(|_| "cannot stat bundle entry")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("bundle path is not UTF-8")?;
            require(
                !name.starts_with("._") && name != ".stage",
                "bundle contains unserved build artifacts",
            )?;
            if kind.is_dir() {
                visit(root, &entry.path(), files)?;
            } else {
                require(
                    kind.is_file(),
                    "bundle must contain only regular files and directories",
                )?;
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| "bundle escaped root")?
                    .to_str()
                    .ok_or("bundle path is not UTF-8")?
                    .to_owned();
                let bytes = fs::read(entry.path()).map_err(|_| "cannot read bundle asset")?;
                files.insert(relative, digest(&bytes));
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    require(
        files.contains_key("index.html") && files.contains_key("manifest.json"),
        "bundle lacks shell or manifest",
    )?;
    Ok(files)
}

/// Owns one production client event stream; every wait folds state before
/// checking completion, so result/fanout ordering cannot discard observations.
pub struct Adapter {
    connection: Connection,
    stream: StreamPath,
    pub welcome: WelcomePayload,
    pub settings: HostSettingsPayload,
    pub mobile_access: MobileAccessStatePayload,
}

impl Adapter {
    pub async fn connect(endpoint: SocketAddr, expected: &Expected) -> Result<Self> {
        require(
            expected.protocol_version == protocol::PROTOCOL_VERSION
                && expected.tyde_version == protocol::TYDE_VERSION,
            "adapter source identity mismatch",
        )?;
        let socket = TcpStream::connect(endpoint)
            .await
            .map_err(|_| "Welcome probe connect failed")?;
        let (read, mut write) = socket.into_split();
        let stream = StreamPath(format!("/host/{}", uuid::Uuid::new_v4()));
        let config = ClientConfig::current();
        let hello = Envelope::from_payload(
            stream.clone(),
            FrameKind::Hello,
            0,
            &HelloPayload {
                protocol_version: config.protocol_version,
                tyde_version: config.tyde_version,
                client_name: config.client_name.clone(),
                platform: config.platform.clone(),
            },
        )
        .map_err(|_| "cannot encode Hello")?;
        protocol::write_envelope(&mut write, &hello)
            .await
            .map_err(|_| "cannot send Hello")?;
        let envelope = protocol::read_envelope(&mut BufReader::new(read))
            .await
            .map_err(|_| "cannot read Welcome")?
            .ok_or("EOF before Welcome")?;
        require(
            envelope.kind == FrameKind::Welcome && envelope.stream == stream && envelope.seq == 0,
            "invalid Welcome envelope",
        )?;
        let welcome: WelcomePayload = envelope
            .parse_payload()
            .map_err(|_| "invalid Welcome payload")?;
        require(
            welcome.protocol_version == expected.protocol_version
                && welcome.tyde_version == expected.tyde_version
                && welcome.release_version.as_ref() == Some(&expected.release_version),
            "exact Welcome version mismatch",
        )?;
        drop(write);
        let socket = TcpStream::connect(endpoint)
            .await
            .map_err(|_| "client connect failed")?;
        let mut connection = client::connect(&config, socket)
            .await
            .map_err(|_| "production client handshake failed")?;
        let (stream, bootstrap) = loop {
            let envelope = connection
                .next_event()
                .await
                .map_err(|_| "bootstrap read failed")?
                .ok_or("EOF before HostBootstrap")?;
            require(
                envelope.kind != FrameKind::CommandError,
                "server command error before bootstrap",
            )?;
            if envelope.kind == FrameKind::HostBootstrap {
                let bootstrap: HostBootstrapPayload = envelope
                    .parse_payload()
                    .map_err(|_| "invalid HostBootstrap")?;
                break (envelope.stream, bootstrap);
            }
        };
        require(
            bootstrap.sessions.is_empty() && bootstrap.agents.is_empty(),
            "host must have no sessions or agents",
        )?;
        require(
            bootstrap.projects.len() == 1
                && bootstrap.projects[0]
                    .root_paths()
                    .iter()
                    .any(|p| Path::new(&p.0) == expected.project_dir),
            "HostBootstrap project mismatch",
        )?;
        require(
            connection.outgoing_seq.contains_key(&stream),
            "bootstrap host stream mismatch",
        )?;
        Ok(Self {
            connection,
            stream,
            welcome,
            settings: HostSettingsPayload {
                settings: bootstrap.settings,
                etag: bootstrap.settings_etag,
                configured_secrets: bootstrap.configured_secrets,
            },
            mobile_access: bootstrap.mobile_access,
        })
    }

    async fn next(&mut self) -> Result<Envelope> {
        let envelope = loop {
            let envelope = self
                .connection
                .next_event()
                .await
                .map_err(|_| "production event read failed")?
                .ok_or("production event stream closed")?;
            if envelope.stream == self.stream {
                break envelope;
            }
            // Host registration also subscribes to project streams. The
            // production client validates those; they are not host operations.
            eprintln!("mobile-direct: ambient frame kind={}", envelope.kind);
            require(
                !matches!(
                    envelope.kind,
                    FrameKind::HostSettings
                        | FrameKind::MobileAccessState
                        | FrameKind::SettingsWriteResult
                        | FrameKind::MobilePairingOffer
                        | FrameKind::CommandError
                        | FrameKind::NewAgent
                        | FrameKind::SessionList
                ),
                "host operation event on foreign stream",
            )?;
        };
        match envelope.kind {
            FrameKind::HostSettings => {
                self.settings = envelope
                    .parse_payload()
                    .map_err(|_| "invalid HostSettings")?
            }
            FrameKind::MobileAccessState => {
                self.mobile_access = envelope
                    .parse_payload()
                    .map_err(|_| "invalid MobileAccessState")?
            }
            FrameKind::CommandError => {
                return Err("server rejected operation (private state retained)".into());
            }
            FrameKind::NewAgent | FrameKind::SessionList => {
                return Err("host is no longer empty".into());
            }
            _ => {}
        }
        if matches!(
            self.mobile_access.direct_hosting,
            MobileDirectHostingStatus::Error { .. }
        ) {
            return Err("authoritative direct hosting error".into());
        }
        Ok(envelope)
    }

    async fn send<T: Serialize>(&mut self, kind: FrameKind, payload: &T) -> Result<()> {
        let seq = *self
            .connection
            .outgoing_seq
            .get(&self.stream)
            .ok_or("missing host sequence")?;
        let envelope = Envelope::from_payload(self.stream.clone(), kind, seq, payload)
            .map_err(|_| "cannot encode typed command")?;
        protocol::write_envelope(&mut self.connection.writer, &envelope)
            .await
            .map_err(|_| "typed command write failed")?;
        self.connection
            .outgoing_seq
            .insert(self.stream.clone(), seq + 1);
        Ok(())
    }

    async fn replace(&mut self, values: Vec<(&str, Value)>, hosting: Option<u32>) -> Result<()> {
        let write_id = SettingsWriteId(uuid::Uuid::new_v4().to_string());
        let ops = values
            .iter()
            .map(|(path, value)| SettingOp::Replace {
                path: (*path).into(),
                value: value.clone(),
                expected: match self.settings.settings.pointer(path) {
                    Some(value) => SettingExpectation::Value {
                        value: value.clone(),
                    },
                    None => SettingExpectation::Absent,
                },
            })
            .collect();
        self.connection
            .settings_write(SettingsWritePayload {
                write_id: write_id.clone(),
                ops,
            })
            .await
            .map_err(|_| "CAS settings write failed")?;
        let mut result: Option<SettingsWriteResultPayload> = None;
        loop {
            let envelope = self.next().await?;
            if envelope.kind == FrameKind::SettingsWriteResult {
                let received: SettingsWriteResultPayload = envelope
                    .parse_payload()
                    .map_err(|_| "invalid SettingsWriteResult")?;
                require(
                    received.write_id == write_id
                        && received.applied
                        && received.field_errors.is_empty(),
                    "CAS settings rejected or reported field errors",
                )?;
                result = Some(received);
            }
            if result
                .as_ref()
                .is_some_and(|r| !r.current_etag.is_empty() && r.current_etag == self.settings.etag)
                && values
                    .iter()
                    .all(|(p, v)| self.settings.settings.pointer(p) == Some(v))
                && self.hosting_matches(hosting)?
            {
                return Ok(());
            }
        }
    }

    fn hosting_matches(&self, count: Option<u32>) -> Result<bool> {
        match (&self.mobile_access.direct_hosting, count) {
            (MobileDirectHostingStatus::Disabled, None) => Ok(true),
            (
                MobileDirectHostingStatus::Online {
                    bind_addr,
                    asset_count,
                    source,
                },
                Some(count),
            ) => {
                let addr: SocketAddr = bind_addr
                    .parse()
                    .map_err(|_| "invalid authoritative bind endpoint")?;
                require(
                    addr.ip().is_loopback()
                        && addr.port() != 0
                        && *asset_count == count
                        && *source == MobileWebBundleSource::Directory,
                    "authoritative directory hosting mismatch",
                )?;
                Ok(true)
            }
            (MobileDirectHostingStatus::Error { .. }, _) => {
                Err("authoritative direct hosting error".into())
            }
            _ => Ok(false),
        }
    }

    pub async fn configure(&mut self, bundle: &Path, count: u32) -> Result<()> {
        require(
            matches!(
                self.mobile_access.direct_hosting,
                MobileDirectHostingStatus::Disabled
            ) && self.mobile_access.paired_devices.is_empty()
                && !matches!(
                    self.mobile_access.pairing,
                    MobilePairingState::Active { .. }
                ),
            "configure requires disabled disposable host",
        )?;
        self.replace(
            vec![
                ("/enable_mobile_connections", json!(true)),
                ("/mobile_direct_bundle_dir", json!(bundle)),
                ("/mobile_direct_bind_addr", json!("127.0.0.1:0")),
                ("/mobile_direct_hosting_enabled", json!(true)),
            ],
            Some(count),
        )
        .await
    }

    pub async fn set_origin(&mut self, origin: &str, count: u32) -> Result<()> {
        require(
            self.hosting_matches(Some(count))?,
            "directory host is not Online",
        )?;
        self.replace(
            vec![("/mobile_direct_public_origin", json!(origin))],
            Some(count),
        )
        .await
    }

    pub async fn offer(&mut self) -> Result<MobilePairingOfferPayload> {
        require(
            self.mobile_access.paired_devices.is_empty()
                && !matches!(
                    self.mobile_access.pairing,
                    MobilePairingState::Active { .. }
                ),
            "offer requires no devices or active offer",
        )?;
        self.send(
            FrameKind::MobilePairingStart,
            &MobilePairingStartPayload { direct: true },
        )
        .await?;
        let mut offer: Option<MobilePairingOfferPayload> = None;
        loop {
            let envelope = self.next().await?;
            if envelope.kind == FrameKind::MobilePairingOffer {
                require(offer.is_none(), "multiple requester offers")?;
                offer = Some(
                    envelope
                        .parse_payload()
                        .map_err(|_| "invalid MobilePairingOffer")?,
                );
            }
            if envelope.kind == FrameKind::MobileAccessState {
                require(
                    !matches!(
                        self.mobile_access.pairing,
                        MobilePairingState::Failed { .. }
                            | MobilePairingState::Expired { .. }
                            | MobilePairingState::Cancelled { .. }
                            | MobilePairingState::RepairRequired { .. }
                    ),
                    "pairing failed before offer handoff",
                )?;
            }
            if let Some(offer) = &offer
                && let MobilePairingState::Active {
                    offer_id,
                    expires_at_ms,
                } = &self.mobile_access.pairing
            {
                require(
                    *offer_id == offer.offer_id && *expires_at_ms == offer.expires_at_ms,
                    "offer does not match authoritative Active state",
                )?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| "system clock before epoch")?
                    .as_millis();
                require(
                    u128::from(offer.expires_at_ms) > now,
                    "offer already expired",
                )?;
                return Ok(offer.clone());
            }
        }
    }

    pub async fn revoke(&mut self, offer: &MobilePairingOfferPayload) -> Result<()> {
        loop {
            match &self.mobile_access.pairing {
                MobilePairingState::Consumed { offer_id } => {
                    require(
                        *offer_id == offer.offer_id && self.mobile_access.paired_devices.len() == 1,
                        "revoke requires this run consumed offer and exactly one disposable device",
                    )?;
                    break;
                }
                MobilePairingState::Active { offer_id, .. } if *offer_id == offer.offer_id => {
                    self.next().await?;
                }
                _ => {
                    return Err("this run offer was not consumed; no device will be revoked".into());
                }
            }
        }
        let device_id = self.mobile_access.paired_devices[0].device_id.clone();
        self.send(
            FrameKind::MobileDeviceRevoke,
            &MobileDeviceRevokePayload {
                device_id: device_id.clone(),
            },
        )
        .await?;
        while self
            .mobile_access
            .paired_devices
            .iter()
            .any(|d| d.device_id == device_id)
        {
            self.next().await?;
        }
        require(
            self.mobile_access.paired_devices.is_empty(),
            "foreign paired devices remain",
        )
    }

    pub async fn disable(&mut self) -> Result<()> {
        require(
            self.mobile_access.paired_devices.is_empty()
                && !matches!(
                    self.mobile_access.pairing,
                    MobilePairingState::Active { .. }
                ),
            "disable cannot bypass an active offer or leave paired devices",
        )?;
        self.replace(vec![("/mobile_direct_hosting_enabled", json!(false))], None)
            .await
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let [request_path, output_path] = args else {
        return Err(
            "usage: tyde-dev-driver mobile-direct PRIVATE_REQUEST PRIVATE_NEW_RECEIPT".into(),
        );
    };
    let request: Request = decode(&read_private(Path::new(request_path))?)?;
    let run_bytes = read_private(&request.run)?;
    let config: RunConfig = decode(&run_bytes)?;
    require(
        inventory(&config.bundle_dir)? == config.inventory,
        "immutable bundle inventory mismatch",
    )?;
    let count = u32::try_from(config.inventory.len()).map_err(|_| "bundle asset count overflow")?;
    let ready_bytes = read_private(&config.ready_path)?;
    let endpoint = ready_endpoint(&ready_bytes, &config.expected)?;
    let mut adapter = Adapter::connect(endpoint, &config.expected).await?;
    let mut output = reserve(Path::new(output_path))?;
    let previous = match &request.operation {
        Operation::Configure => None,
        Operation::SetOrigin { previous, .. }
        | Operation::Offer { previous, .. }
        | Operation::Revoke { previous }
        | Operation::Disable { previous } => {
            let receipt: Receipt = decode(&read_private(previous)?)?;
            require(
                receipt.run_sha256 == digest(&run_bytes)
                    && receipt.ready_sha256 == digest(&ready_bytes)
                    && receipt.welcome == adapter.welcome,
                "previous receipt custody mismatch",
            )?;
            require(
                receipt.settings == adapter.settings,
                "host settings changed outside this run",
            )?;
            let mut claimed = previous.as_os_str().to_os_string();
            claimed.push(".used");
            reserve(Path::new(&claimed))?
                .sync_all()
                .map_err(|_| "cannot claim previous receipt")?;
            Some(receipt)
        }
    };
    let mut offer = previous.as_ref().and_then(|p| p.offer.clone());
    let phase = match request.operation {
        Operation::Configure => {
            adapter.configure(&config.bundle_dir, count).await?;
            Phase::Configured
        }
        Operation::SetOrigin { origin_path, .. } => {
            require(
                previous
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::Configured),
                "origin requires configured receipt",
            )?;
            let origin: String = decode(&read_private(&origin_path)?)?;
            adapter.set_origin(&origin, count).await?;
            Phase::OriginSet
        }
        Operation::Offer { offer_path, .. } => {
            require(
                previous
                    .as_ref()
                    .is_some_and(|p| p.phase == Phase::OriginSet),
                "offer requires origin receipt",
            )?;
            require(
                adapter.hosting_matches(Some(count))?,
                "directory host is not Online",
            )?;
            let mut file = reserve(&offer_path)?;
            let payload = adapter.offer().await?;
            save(&mut file, &payload)?;
            offer = Some(payload);
            Phase::Offered
        }
        Operation::Revoke { .. } => {
            require(
                previous.as_ref().is_some_and(|p| p.phase == Phase::Offered),
                "revoke requires offered receipt",
            )?;
            adapter
                .revoke(offer.as_ref().ok_or("receipt has no run offer")?)
                .await?;
            Phase::Revoked
        }
        Operation::Disable { .. } => {
            require(
                previous.as_ref().is_some_and(|p| {
                    matches!(
                        p.phase,
                        Phase::Configured | Phase::OriginSet | Phase::Revoked
                    )
                }),
                "disable requires unoffered or revoked receipt",
            )?;
            adapter.disable().await?;
            Phase::Disabled
        }
    };
    require(
        inventory(&config.bundle_dir)? == config.inventory,
        "bundle changed during operation",
    )?;
    save(
        &mut output,
        &Receipt {
            run_sha256: digest(&run_bytes),
            ready_sha256: digest(&ready_bytes),
            phase,
            welcome: adapter.welcome,
            settings: adapter.settings,
            mobile_access: adapter.mobile_access,
            offer,
        },
    )?;
    println!("mobile-direct: completed");
    Ok(())
}
