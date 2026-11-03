//! Bounded direct-host apparatus. Wire payloads remain production protocol types.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io::Read;
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
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;

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

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandaloneTarget {
    pub socket_path: PathBuf,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub pid: u32,
    pub start_ticks: u64,
    pub store_environment: BTreeMap<String, PathBuf>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StandaloneAttestation {
    LinuxUds {
        pid: u32,
        uid: u32,
        start_ticks: u64,
        socket_device: u64,
        socket_inode: u64,
        executable_sha256: String,
        store_environment_sha256: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StandaloneRunConfig {
    target: StandaloneTarget,
    expected: Expected,
    bundle_dir: PathBuf,
    inventory: BTreeMap<String, String>,
}

trait AdapterIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> AdapterIo for T {}
enum Endpoint {
    Tcp(SocketAddr),
    Standalone(StandaloneTarget),
}
impl Endpoint {
    async fn open(&self) -> Result<(Box<dyn AdapterIo>, Option<StandaloneAttestation>)> {
        match self {
            Self::Tcp(endpoint) => Ok((
                Box::new(
                    TcpStream::connect(endpoint)
                        .await
                        .map_err(|_| "production TCP connect failed")?,
                ),
                None,
            )),
            Self::Standalone(target) => {
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = target;
                    Err("standalone UDS source attestation requires Linux".into())
                }
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::FileTypeExt;
                    private_parent(&target.socket_path)?;
                    let meta = fs::symlink_metadata(&target.socket_path)
                        .map_err(|_| "cannot stat owned host socket")?;
                    require(
                        meta.file_type().is_socket()
                            && meta.uid() == unsafe { libc::geteuid() }
                            && meta.mode() & 0o077 == 0,
                        "host socket must be owned and private",
                    )?;
                    let process = PathBuf::from(format!("/proc/{}", target.pid));
                    require(
                        fs::canonicalize(process.join("exe"))
                            .map_err(|_| "cannot resolve live host executable")?
                            == target.executable,
                        "live host executable path mismatch",
                    )?;
                    // Hash before connecting: the real host bounds its initial Hello
                    // wait, and debug images can take arbitrarily long to read.
                    eprintln!("mobile-direct: attesting host image before connecting");
                    let mut image = File::open(process.join("exe"))
                        .map_err(|_| "cannot open live host executable")?;
                    let image_meta = image.metadata().map_err(|_| "cannot stat live image")?;
                    let mut hasher = Sha256::new();
                    let mut buffer = [0; 64 * 1024];
                    loop {
                        let count = image
                            .read(&mut buffer)
                            .map_err(|_| "cannot hash live image")?;
                        if count == 0 {
                            break;
                        }
                        hasher.update(&buffer[..count]);
                    }
                    let executable_sha256 = format!("{:x}", hasher.finalize());
                    require(
                        executable_sha256 == target.executable_sha256,
                        "live host executable hash mismatch",
                    )?;
                    let stat = fs::read_to_string(process.join("stat"))
                        .map_err(|_| "cannot attest host lifetime")?;
                    let start_ticks = stat
                        .rsplit_once(") ")
                        .ok_or("invalid host lifetime record")?
                        .1
                        .split_whitespace()
                        .nth(19)
                        .ok_or("missing host start clock")?
                        .parse::<u64>()
                        .map_err(|_| "invalid host start clock")?;
                    require(
                        start_ticks == target.start_ticks,
                        "owned host process lifetime changed",
                    )?;
                    let environment = fs::read(process.join("environ"))
                        .map_err(|_| "cannot attest host store environment")?;
                    let mut store_environment = BTreeMap::new();
                    for entry in environment.split(|b| *b == 0) {
                        let Some(separator) = entry.iter().position(|b| *b == b'=') else {
                            continue;
                        };
                        let name = std::str::from_utf8(&entry[..separator])
                            .map_err(|_| "invalid host environment name")?;
                        if name == "HOME"
                            || name == "TYDE_SKILLS_DIR_PATH"
                            || (name.starts_with("TYDE_")
                                && (name.ends_with("_STORE_PATH") || name.ends_with("_STORE_DIR")))
                        {
                            let value = std::str::from_utf8(&entry[separator + 1..])
                                .map_err(|_| "invalid host store path")?;
                            store_environment.insert(name.to_owned(), PathBuf::from(value));
                        }
                    }
                    require(
                        store_environment.contains_key("HOME")
                            && store_environment.values().all(|p| p.is_absolute())
                            && store_environment == target.store_environment,
                        "owned host store environment mismatch",
                    )?;
                    let store_environment_sha256 = digest(
                        &serde_json::to_vec(&store_environment)
                            .map_err(|_| "cannot encode store custody")?,
                    );
                    eprintln!("mobile-direct: image and store custody verified; connecting UDS");
                    let socket = UnixStream::connect(&target.socket_path)
                        .await
                        .map_err(|_| "owned UDS host is unavailable")?;
                    let peer = socket.peer_cred().map_err(|_| "cannot attest UDS peer")?;
                    let current_image = fs::metadata(process.join("exe"))
                        .map_err(|_| "cannot recheck live image")?;
                    let current_socket = fs::symlink_metadata(&target.socket_path)
                        .map_err(|_| "cannot recheck owned host socket")?;
                    let current_stat = fs::read_to_string(process.join("stat"))
                        .map_err(|_| "cannot recheck host lifetime")?;
                    require(
                        peer.uid() == unsafe { libc::geteuid() }
                            && peer.pid().and_then(|p| u32::try_from(p).ok()) == Some(target.pid)
                            && current_stat
                                .rsplit_once(") ")
                                .and_then(|(_, s)| s.split_whitespace().nth(19))
                                .and_then(|s| s.parse::<u64>().ok())
                                == Some(start_ticks)
                            && (current_image.dev(), current_image.ino())
                                == (image_meta.dev(), image_meta.ino())
                            && (current_socket.dev(), current_socket.ino())
                                == (meta.dev(), meta.ino()),
                        "UDS peer, image, socket or lifetime changed during attestation",
                    )?;
                    Ok((
                        Box::new(socket),
                        Some(StandaloneAttestation::LinuxUds {
                            pid: target.pid,
                            uid: peer.uid(),
                            start_ticks,
                            socket_device: meta.dev(),
                            socket_inode: meta.ino(),
                            executable_sha256,
                            store_environment_sha256,
                        }),
                    ))
                }
            }
        }
    }
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
    TransitionOrigin {
        previous: PathBuf,
        previous_run: PathBuf,
        origin_path: PathBuf,
    },
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
    Reoffered,
    Revoked,
    Disabled,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    run_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ready_sha256: Option<String>,
    phase: Phase,
    welcome: WelcomePayload,
    settings: HostSettingsPayload,
    mobile_access: MobileAccessStatePayload,
    offer: Option<MobilePairingOfferPayload>,
    #[serde(default)]
    superseded_receipt_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    standalone_attestation: Option<StandaloneAttestation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_transition_seed_sha256: Option<String>,
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
    standalone_attestation: Option<StandaloneAttestation>,
    projects: Vec<protocol::Project>,
}

impl Adapter {
    pub async fn connect(endpoint: SocketAddr, expected: &Expected) -> Result<Self> {
        Self::connect_endpoint(&Endpoint::Tcp(endpoint), expected, false).await
    }
    async fn connect_endpoint(
        endpoint: &Endpoint,
        expected: &Expected,
        allow_empty_project: bool,
    ) -> Result<Self> {
        require(
            expected.protocol_version == protocol::PROTOCOL_VERSION
                && expected.tyde_version == protocol::TYDE_VERSION,
            "adapter source identity mismatch",
        )?;
        let (socket, first_attestation) = endpoint.open().await?;
        let (read, mut write) = tokio::io::split(socket);
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
        let (socket, standalone_attestation) = endpoint.open().await?;
        require(
            first_attestation == standalone_attestation,
            "host authority changed between Welcome and bootstrap",
        )?;
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
            (bootstrap.projects.len() == 1
                && bootstrap.projects[0]
                    .root_paths()
                    .iter()
                    .any(|p| Path::new(&p.0) == expected.project_dir))
                || (allow_empty_project
                    && standalone_attestation.is_some()
                    && bootstrap.projects.is_empty()),
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
            standalone_attestation,
            projects: bootstrap.projects,
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
            FrameKind::ProjectNotify => match envelope
                .parse_payload::<protocol::ProjectNotifyPayload>()
                .map_err(|_| "invalid project publication")?
            {
                protocol::ProjectNotifyPayload::Upsert { project } => {
                    self.projects.retain(|p| p.id != project.id);
                    self.projects.push(project);
                }
                protocol::ProjectNotifyPayload::Delete { project } => {
                    self.projects.retain(|p| p.id != project.id)
                }
            },
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

    async fn prepare_project(&mut self, expected: &Expected) -> Result<()> {
        if self.projects.is_empty() {
            require(
                self.standalone_attestation.is_some(),
                "project preparation requires attested standalone UDS",
            )?;
            require(
                expected.project_dir.is_absolute()
                    && expected.project_dir.is_dir()
                    && fs::canonicalize(&expected.project_dir)
                        .map_err(|_| "owned QA project is unavailable")?
                        == expected.project_dir,
                "QA project directory must be canonical",
            )?;
            self.connection
                .project_create(protocol::ProjectCreatePayload {
                    name: "Native QA".into(),
                    roots: vec![protocol::ProjectRootPath(
                        expected.project_dir.to_string_lossy().into_owned(),
                    )],
                })
                .await
                .map_err(|_| "cannot create owned QA project")?;
            loop {
                let event = self.next().await?;
                if event.kind == FrameKind::ProjectOperation {
                    let operation: protocol::ProjectOperationPayload = event
                        .parse_payload()
                        .map_err(|_| "invalid project operation")?;
                    require(
                        !matches!(
                            operation.state,
                            protocol::ProjectOperationState::Failed { .. }
                        ),
                        "server rejected owned project creation",
                    )?;
                }
                if event.kind == FrameKind::ProjectNotify {
                    break;
                }
            }
        }
        require(
            self.projects.len() == 1
                && self.projects[0].root_paths().len() == 1
                && self.projects[0]
                    .root_paths()
                    .iter()
                    .any(|p| Path::new(&p.0) == expected.project_dir),
            "server must attest exactly the owned project root",
        )
    }

    fn require_configurable(&self) -> Result<()> {
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
        )
    }

    pub async fn configure(&mut self, bundle: &Path, count: u32) -> Result<()> {
        self.require_configurable()?;
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

    pub async fn offer_after_expired_unused(
        &mut self,
        previous: &MobilePairingOfferPayload,
    ) -> Result<MobilePairingOfferPayload> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock before epoch")?
            .as_millis();
        require(
            u128::from(previous.expires_at_ms) <= now
                && self.mobile_access.paired_devices.is_empty()
                && match &self.mobile_access.pairing {
                    MobilePairingState::Expired { offer_id } => *offer_id == previous.offer_id,
                    // The server clears its terminal state after the grace period.
                    // The packet owner separately proves the old offer was never staged.
                    MobilePairingState::Idle => true,
                    _ => false,
                },
            "renewal requires expired prior offer, no devices and authoritative retirement",
        )?;
        self.offer().await
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
    run_mode(args, false).await
}
pub async fn run_uds(args: &[String]) -> Result<()> {
    run_mode(args, true).await
}
async fn run_mode(args: &[String], standalone: bool) -> Result<()> {
    let [request_path, output_path] = args else {
        return Err(format!(
            "usage: tyde-dev-driver {} PRIVATE_REQUEST PRIVATE_NEW_RECEIPT",
            if standalone {
                "mobile-direct-uds"
            } else {
                "mobile-direct"
            }
        ));
    };
    let request: Request = decode(&read_private(Path::new(request_path))?)?;
    let run_bytes = read_private(&request.run)?;
    let (expected, bundle_dir, bundle_inventory, endpoint, dev_ready) = if standalone {
        let config: StandaloneRunConfig = decode(&run_bytes)?;
        (
            config.expected,
            config.bundle_dir,
            config.inventory,
            Endpoint::Standalone(config.target),
            None,
        )
    } else {
        let config: RunConfig = decode(&run_bytes)?;
        let bytes = read_private(&config.ready_path)?;
        let endpoint = ready_endpoint(&bytes, &config.expected)?;
        (
            config.expected,
            config.bundle_dir,
            config.inventory,
            Endpoint::Tcp(endpoint),
            Some(bytes),
        )
    };
    require(
        inventory(&bundle_dir)? == bundle_inventory,
        "immutable bundle inventory mismatch",
    )?;
    let count = u32::try_from(bundle_inventory.len()).map_err(|_| "bundle asset count overflow")?;
    let mut adapter = Adapter::connect_endpoint(
        &endpoint,
        &expected,
        standalone && matches!(request.operation, Operation::Configure),
    )
    .await?;
    let ready_sha256 = dev_ready.as_ref().map(|bytes| digest(bytes));
    let mut output = reserve(Path::new(output_path))?;
    let mut previous_sha256 = None;
    let previous = match &request.operation {
        Operation::Configure => None,
        Operation::TransitionOrigin {
            previous,
            previous_run,
            ..
        } => {
            let Endpoint::Standalone(target) = &endpoint else {
                return Err("origin transition requires attested standalone UDS".into());
            };
            let old_bytes = read_private(previous_run)?;
            let old: StandaloneRunConfig = decode(&old_bytes)?;
            let bytes = read_private(previous)?;
            previous_sha256 = Some(digest(&bytes));
            let receipt: Receipt = decode(&bytes)?;
            require(
                receipt.run_sha256 == digest(&old_bytes)
                    && receipt.ready_sha256.is_none()
                    && matches!(receipt.phase, Phase::Configured | Phase::OriginSet)
                    && receipt.offer.is_none()
                    && receipt.superseded_receipt_sha256.is_none(),
                "transition requires an actual unoffered standalone receipt",
            )?;
            require(
                old.expected.protocol_version == expected.protocol_version
                    && old.expected.tyde_version == expected.tyde_version
                    && old.expected.release_version == expected.release_version
                    && old.expected.project_dir == expected.project_dir
                    && old.bundle_dir == bundle_dir
                    && old.inventory == bundle_inventory
                    && old.target.socket_path == target.socket_path
                    && old.target.store_environment == target.store_environment,
                "transition must retain the exact stores, project and immutable bundle",
            )?;
            let store_hash = digest(
                &serde_json::to_vec(&old.target.store_environment)
                    .map_err(|_| "cannot encode transition store custody")?,
            );
            require(
                matches!(&receipt.standalone_attestation,
                Some(StandaloneAttestation::LinuxUds { pid, uid, start_ticks, executable_sha256, store_environment_sha256, .. })
                    if *pid == old.target.pid && *uid == unsafe { libc::geteuid() }
                        && *start_ticks == old.target.start_ticks
                        && executable_sha256 == &old.target.executable_sha256
                        && store_environment_sha256 == &store_hash),
                "old standalone receipt is not bound to its actual process/stores",
            )?;
            require(
                receipt.welcome == adapter.welcome && receipt.settings == adapter.settings,
                "preserved host identity/settings differ from transition seed",
            )?;
            require(
                receipt.mobile_access.paired_devices.is_empty()
                    && !matches!(
                        receipt.mobile_access.pairing,
                        MobilePairingState::Active { .. }
                    )
                    && adapter.mobile_access.paired_devices.is_empty()
                    && matches!(adapter.mobile_access.pairing, MobilePairingState::Idle)
                    && adapter.hosting_matches(Some(count))?,
                "origin transition requires unchanged Online hosting with no devices or offers",
            )?;
            let mut claimed = previous.as_os_str().to_os_string();
            claimed.push(".used");
            reserve(Path::new(&claimed))?
                .sync_all()
                .map_err(|_| "cannot claim transition seed")?;
            Some(receipt)
        }
        Operation::SetOrigin { previous, .. }
        | Operation::Offer { previous, .. }
        | Operation::Revoke { previous }
        | Operation::Disable { previous } => {
            let bytes = read_private(previous)?;
            previous_sha256 = Some(digest(&bytes));
            let receipt: Receipt = decode(&bytes)?;
            require(
                receipt.run_sha256 == digest(&run_bytes)
                    && receipt.ready_sha256 == ready_sha256
                    && receipt.welcome == adapter.welcome
                    && receipt.standalone_attestation == adapter.standalone_attestation,
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
    let mut superseded_receipt_sha256 = previous
        .as_ref()
        .and_then(|p| p.superseded_receipt_sha256.clone());
    let mut origin_transition_seed_sha256 = previous
        .as_ref()
        .and_then(|p| p.origin_transition_seed_sha256.clone());
    let phase = match request.operation {
        Operation::TransitionOrigin { origin_path, .. } => {
            let origin: String = decode(&read_private(&origin_path)?)?;
            adapter.set_origin(&origin, count).await?;
            origin_transition_seed_sha256 = previous_sha256.clone();
            Phase::OriginSet
        }
        Operation::Configure => {
            adapter.require_configurable()?;
            if standalone {
                adapter.prepare_project(&expected).await?;
            }
            adapter.configure(&bundle_dir, count).await?;
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
            let prior = previous.as_ref().ok_or("offer requires previous receipt")?;
            require(
                match prior.phase {
                    Phase::OriginSet | Phase::Offered => prior.superseded_receipt_sha256.is_none(),
                    Phase::Reoffered => prior.superseded_receipt_sha256.is_some(),
                    _ => false,
                },
                "offer requires origin receipt or an intact offered receipt chain",
            )?;
            require(
                adapter.hosting_matches(Some(count))?
                    && prior.mobile_access.direct_hosting == adapter.mobile_access.direct_hosting,
                "directory host is not the same Online listener",
            )?;
            let mut file = reserve(&offer_path)?;
            let (payload, phase) = if matches!(prior.phase, Phase::Offered | Phase::Reoffered) {
                let old = prior.offer.as_ref().ok_or("receipt has no prior offer")?;
                require(
                    matches!(&prior.mobile_access.pairing,
                        MobilePairingState::Active { offer_id, expires_at_ms }
                        if *offer_id == old.offer_id && *expires_at_ms == old.expires_at_ms)
                        && prior.mobile_access.paired_devices.is_empty(),
                    "previous receipt did not attest this unused active offer",
                )?;
                let payload = adapter.offer_after_expired_unused(old).await?;
                require(
                    payload.offer_id != old.offer_id,
                    "renewal reused prior offer",
                )?;
                superseded_receipt_sha256 = previous_sha256.clone();
                (payload, Phase::Reoffered)
            } else {
                (adapter.offer().await?, Phase::Offered)
            };
            save(&mut file, &payload)?;
            offer = Some(payload);
            phase
        }
        Operation::Revoke { .. } => {
            require(
                previous
                    .as_ref()
                    .is_some_and(|p| matches!(p.phase, Phase::Offered | Phase::Reoffered)),
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
        inventory(&bundle_dir)? == bundle_inventory,
        "bundle changed during operation",
    )?;
    save(
        &mut output,
        &Receipt {
            run_sha256: digest(&run_bytes),
            ready_sha256,
            phase,
            welcome: adapter.welcome,
            settings: adapter.settings,
            mobile_access: adapter.mobile_access,
            offer,
            superseded_receipt_sha256,
            standalone_attestation: adapter.standalone_attestation,
            origin_transition_seed_sha256,
        },
    )?;
    println!("mobile-direct: completed");
    Ok(())
}
