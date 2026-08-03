use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::ops::Range;
use std::pin::Pin;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use mdns_sd::{HostnameResolutionEvent, ServiceDaemon};
use opus::{Application, Channels, Decoder, Encoder};
use str0m::change::SdpOffer;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, RtcConfig};
use tokio::sync::{mpsc, oneshot};

use crate::voice::{
    VoiceMediaEvent, VoiceMediaFactory, VoiceMediaFuture, VoiceMediaSession, VoicePcmFrame,
    VoiceRuntimeError,
};
use protocol::{MAX_VOICE_ICE_CANDIDATES, VoiceIceCandidate};

const MEDIA_QUEUE_CAPACITY: usize = 64;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BUFFERED_PCM_SAMPLES: usize = 48_000 * 10;
const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_MDNS_NAMES: usize = 4;
const MAX_MDNS_ANSWERS: usize = 4;
const MAX_MDNS_EXPANSIONS: usize = 8;
const MDNS_QUERY_TIMEOUT: Duration = Duration::from_secs(1);
const MDNS_DAEMON_TIMEOUT_MARGIN: Duration = Duration::from_millis(100);
const MDNS_SESSION_BUDGET: Duration = Duration::from_secs(2);
const NO_REMOTE_CANDIDATE_WATCHDOG: Duration = Duration::from_secs(3);
const CANDIDATE_QUEUE_CAPACITY: usize = MAX_VOICE_ICE_CANDIDATES + 1;
const CANDIDATE_PREFLIGHT_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

pub(crate) struct Str0mMediaFactory;

impl VoiceMediaFactory for Str0mMediaFactory {
    fn open(&self) -> VoiceMediaFuture<'_, Box<dyn VoiceMediaSession>> {
        Box::pin(async {
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || open_str0m(runtime))
                .await
                .map_err(|_| VoiceRuntimeError::Unavailable)?
        })
    }
}

fn open_str0m(
    runtime: tokio::runtime::Handle,
) -> Result<Box<dyn VoiceMediaSession>, VoiceRuntimeError> {
    install_crypto();
    let route = UdpSocket::bind("0.0.0.0:0").map_err(|_| VoiceRuntimeError::Unavailable)?;
    route
        .connect("192.0.2.1:9")
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let local_ip = route
        .local_addr()
        .map_err(|_| VoiceRuntimeError::Unavailable)?
        .ip();
    drop(route);
    if !matches!(local_ip, IpAddr::V4(_) | IpAddr::V6(_)) || local_ip.is_loopback() {
        return Err(VoiceRuntimeError::Unavailable);
    }
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let local_addr = socket
        .local_addr()
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let (command_tx, command_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (audio_tx, audio_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (candidate_tx, candidate_rx) = mpsc::channel(CANDIDATE_QUEUE_CAPACITY);
    let closed = Arc::new(AtomicBool::new(false));
    let thread_closed = Arc::clone(&closed);
    let candidate_event_tx = event_tx.downgrade();
    std::thread::Builder::new()
        .name("tyde-voice-webrtc".to_owned())
        .spawn(move || {
            run_webrtc(
                socket,
                local_addr,
                command_rx,
                event_tx,
                audio_tx,
                thread_closed,
            )
        })
        .map_err(|_| VoiceRuntimeError::Unavailable)?;
    let diagnostics = Arc::new(IceCandidateCounters::default());
    let candidate_task = runtime.spawn(run_candidate_worker(
        candidate_rx,
        command_tx.clone(),
        candidate_event_tx,
        RemoteCandidatePreparer::new(system_mdns_resolver(), Arc::clone(&diagnostics)),
    ));
    Ok(Box::new(Str0mMediaSession {
        command_tx,
        candidate_tx,
        candidate_task: Some(candidate_task),
        event_rx: Some(event_rx),
        audio_rx: Some(audio_rx),
        closed,
        diagnostics,
    }))
}

fn install_crypto() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
}

type MdnsResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<IpAddr>, MdnsResolveFailure>> + Send + 'a>>;

trait MdnsResolver: Send + Sync {
    fn resolve<'a>(&'a self, name: &'a MdnsName, timeout: Duration) -> MdnsResolveFuture<'a>;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MdnsResolveFailure {
    NoResponse,
    Unavailable,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct MdnsName(String);

impl MdnsName {
    fn as_str(&self) -> &str {
        &self.0
    }
}

struct SystemMdnsResolver {
    daemon: ServiceDaemon,
    query_lock: tokio::sync::Mutex<()>,
}

impl SystemMdnsResolver {
    fn new() -> Result<Self, ()> {
        ServiceDaemon::new()
            .map(|daemon| Self {
                daemon,
                query_lock: tokio::sync::Mutex::new(()),
            })
            .map_err(|_| ())
    }
}

struct ActiveMdnsQuery<'a> {
    daemon: &'a ServiceDaemon,
    name: &'a MdnsName,
}

impl Drop for ActiveMdnsQuery<'_> {
    fn drop(&mut self) {
        let _ = self.daemon.stop_resolve_hostname(self.name.as_str());
    }
}

impl MdnsResolver for SystemMdnsResolver {
    fn resolve<'a>(&'a self, name: &'a MdnsName, timeout: Duration) -> MdnsResolveFuture<'a> {
        Box::pin(async move {
            let _query_lock = self.query_lock.lock().await;
            let daemon_timeout = timeout
                .saturating_sub(MDNS_DAEMON_TIMEOUT_MARGIN)
                .max(Duration::from_millis(1));
            let timeout_millis = u64::try_from(daemon_timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1);
            let events = self
                .daemon
                .resolve_hostname(name.as_str(), Some(timeout_millis))
                .map_err(|_| MdnsResolveFailure::Unavailable)?;
            let _active_query = ActiveMdnsQuery {
                daemon: &self.daemon,
                name,
            };
            loop {
                match events.recv_async().await {
                    Ok(HostnameResolutionEvent::AddressesFound(_, addresses)) => {
                        return Ok(addresses
                            .into_iter()
                            .map(|address| address.to_ip_addr())
                            .collect());
                    }
                    Ok(HostnameResolutionEvent::SearchTimeout(_))
                    | Ok(HostnameResolutionEvent::SearchStopped(_)) => {
                        return Err(MdnsResolveFailure::NoResponse);
                    }
                    Ok(_) => {}
                    Err(_) => return Err(MdnsResolveFailure::Unavailable),
                }
            }
        })
    }
}

fn system_mdns_resolver() -> Option<Arc<dyn MdnsResolver>> {
    static RESOLVER: OnceLock<Option<Arc<SystemMdnsResolver>>> = OnceLock::new();
    RESOLVER
        .get_or_init(|| SystemMdnsResolver::new().ok().map(Arc::new))
        .as_ref()
        .map(|resolver| Arc::clone(resolver) as Arc<dyn MdnsResolver>)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MdnsProtocol {
    Udp,
    Tcp,
}

struct MdnsCandidate {
    name: MdnsName,
    address_range: Range<usize>,
    protocol: MdnsProtocol,
}

fn mdns_candidate(candidate: &str) -> Option<MdnsCandidate> {
    if !candidate.is_ascii()
        || candidate.bytes().any(|byte| byte.is_ascii_control())
        || candidate.starts_with(' ')
        || candidate.ends_with(' ')
    {
        return None;
    }
    let tokens: Vec<_> = candidate.split(' ').collect();
    if tokens.len() < 8 || tokens.iter().any(|token| token.is_empty()) {
        return None;
    }
    let foundation = tokens[0].strip_prefix("candidate:")?;
    if foundation.is_empty()
        || foundation.len() > 32
        || !foundation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return None;
    }
    let component = tokens[1].parse::<u16>().ok()?;
    if !(1..=256).contains(&component) {
        return None;
    }
    let protocol = match tokens[2] {
        "udp" => MdnsProtocol::Udp,
        "tcp" => MdnsProtocol::Tcp,
        _ => return None,
    };
    let priority = tokens[3].parse::<u32>().ok()?;
    if priority == 0 || priority > i32::MAX as u32 {
        return None;
    }
    if tokens[4].parse::<IpAddr>().is_ok() {
        return None;
    }
    let port = tokens[5].parse::<u16>().ok()?;
    if port == 0 || tokens[6] != "typ" || tokens[7] != "host" {
        return None;
    }
    if (tokens.len() - 8) % 2 != 0 {
        return None;
    }

    let address = tokens[4].to_ascii_lowercase();
    let label = address.strip_suffix(".local")?;
    if label.is_empty()
        || label.len() > 63
        || label.contains('.')
        || label.starts_with('-')
        || label.ends_with('-')
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let address_start = tokens[..4].iter().map(|token| token.len()).sum::<usize>() + 4;
    Some(MdnsCandidate {
        name: MdnsName(format!("{address}.")),
        address_range: address_start..address_start + tokens[4].len(),
        protocol,
    })
}

fn rewrite_candidate_address(
    candidate: &VoiceIceCandidate,
    address_range: &Range<usize>,
    address: Ipv4Addr,
) -> VoiceIceCandidate {
    let mut rewritten = String::with_capacity(candidate.candidate.len());
    rewritten.push_str(&candidate.candidate[..address_range.start]);
    rewritten.push_str(&address.to_string());
    rewritten.push_str(&candidate.candidate[address_range.end..]);
    VoiceIceCandidate {
        candidate: rewritten,
        sdp_mid: candidate.sdp_mid.clone(),
        sdp_m_line_index: candidate.sdp_m_line_index,
    }
}

fn usable_mdns_ipv4(address: Ipv4Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && address != Ipv4Addr::BROADCAST
}

#[derive(Clone, Copy)]
enum MdnsSkipReason {
    NoResponse,
    Unavailable,
    Timeout,
    NameBudget,
    TimeBudget,
    ExpansionBudget,
    UnsupportedTcp,
}

#[derive(Clone)]
enum CachedMdnsResolution {
    Addresses(Vec<Ipv4Addr>),
    Miss(MdnsSkipReason),
}

#[derive(Default)]
struct IceCandidateCounters {
    numeric_accepted: AtomicUsize,
    mdns_resolved: AtomicUsize,
    mdns_skipped_no_response: AtomicUsize,
    mdns_skipped_unavailable: AtomicUsize,
    mdns_skipped_timeout: AtomicUsize,
    mdns_skipped_name_budget: AtomicUsize,
    mdns_skipped_time_budget: AtomicUsize,
    mdns_skipped_expansion_budget: AtomicUsize,
    mdns_skipped_unsupported_tcp: AtomicUsize,
    malformed_rejected: AtomicUsize,
    summary_emitted: AtomicBool,
}

#[derive(Debug, PartialEq, Eq)]
struct IceCandidateDiagnosticEvent {
    numeric_accepted: usize,
    mdns_resolved: usize,
    mdns_skipped_no_response: usize,
    mdns_skipped_unavailable: usize,
    mdns_skipped_timeout: usize,
    mdns_skipped_name_budget: usize,
    mdns_skipped_time_budget: usize,
    mdns_skipped_expansion_budget: usize,
    mdns_skipped_unsupported_tcp: usize,
    malformed_rejected: usize,
}

impl std::fmt::Display for IceCandidateDiagnosticEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "numeric_accepted={} mdns_resolved={} mdns_skipped_no_response={} \
             mdns_skipped_unavailable={} mdns_skipped_timeout={} \
             mdns_skipped_name_budget={} mdns_skipped_time_budget={} \
             mdns_skipped_expansion_budget={} mdns_skipped_unsupported_tcp={} \
             malformed_rejected={}",
            self.numeric_accepted,
            self.mdns_resolved,
            self.mdns_skipped_no_response,
            self.mdns_skipped_unavailable,
            self.mdns_skipped_timeout,
            self.mdns_skipped_name_budget,
            self.mdns_skipped_time_budget,
            self.mdns_skipped_expansion_budget,
            self.mdns_skipped_unsupported_tcp,
            self.malformed_rejected
        )
    }
}

impl IceCandidateCounters {
    fn increment(counter: &AtomicUsize, amount: usize) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

    fn record_skip(&self, reason: MdnsSkipReason) {
        let counter = match reason {
            MdnsSkipReason::NoResponse => &self.mdns_skipped_no_response,
            MdnsSkipReason::Unavailable => &self.mdns_skipped_unavailable,
            MdnsSkipReason::Timeout => &self.mdns_skipped_timeout,
            MdnsSkipReason::NameBudget => &self.mdns_skipped_name_budget,
            MdnsSkipReason::TimeBudget => &self.mdns_skipped_time_budget,
            MdnsSkipReason::ExpansionBudget => &self.mdns_skipped_expansion_budget,
            MdnsSkipReason::UnsupportedTcp => &self.mdns_skipped_unsupported_tcp,
        };
        Self::increment(counter, 1);
    }

    fn snapshot(&self) -> IceCandidateDiagnosticEvent {
        IceCandidateDiagnosticEvent {
            numeric_accepted: self.numeric_accepted.load(Ordering::Relaxed),
            mdns_resolved: self.mdns_resolved.load(Ordering::Relaxed),
            mdns_skipped_no_response: self.mdns_skipped_no_response.load(Ordering::Relaxed),
            mdns_skipped_unavailable: self.mdns_skipped_unavailable.load(Ordering::Relaxed),
            mdns_skipped_timeout: self.mdns_skipped_timeout.load(Ordering::Relaxed),
            mdns_skipped_name_budget: self.mdns_skipped_name_budget.load(Ordering::Relaxed),
            mdns_skipped_time_budget: self.mdns_skipped_time_budget.load(Ordering::Relaxed),
            mdns_skipped_expansion_budget: self
                .mdns_skipped_expansion_budget
                .load(Ordering::Relaxed),
            mdns_skipped_unsupported_tcp: self.mdns_skipped_unsupported_tcp.load(Ordering::Relaxed),
            malformed_rejected: self.malformed_rejected.load(Ordering::Relaxed),
        }
    }

    fn emit_summary(&self) {
        if self
            .summary_emitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let diagnostics = self.snapshot();
        tracing::info!(diagnostics = %diagnostics, "voice ICE candidate summary");
    }
}

enum QueuedRemoteCandidate {
    Numeric(VoiceIceCandidate),
    Mdns {
        candidate: VoiceIceCandidate,
        mdns: MdnsCandidate,
    },
    UnsupportedTcp,
}

enum CandidateWorkerCommand {
    Candidate(QueuedRemoteCandidate),
    EndCandidates,
}

fn classify_remote_candidate(
    candidate: &VoiceIceCandidate,
    counters: &IceCandidateCounters,
) -> Result<QueuedRemoteCandidate, VoiceRuntimeError> {
    if Candidate::from_sdp_string(&candidate.candidate).is_ok() {
        return Ok(QueuedRemoteCandidate::Numeric(candidate.clone()));
    }
    let Some(mdns) = mdns_candidate(&candidate.candidate) else {
        IceCandidateCounters::increment(&counters.malformed_rejected, 1);
        return Err(VoiceRuntimeError::InvalidSignal);
    };
    let preflight =
        rewrite_candidate_address(candidate, &mdns.address_range, CANDIDATE_PREFLIGHT_ADDRESS);
    if Candidate::from_sdp_string(&preflight.candidate).is_err() {
        IceCandidateCounters::increment(&counters.malformed_rejected, 1);
        return Err(VoiceRuntimeError::InvalidSignal);
    }
    if mdns.protocol == MdnsProtocol::Tcp {
        return Ok(QueuedRemoteCandidate::UnsupportedTcp);
    }
    Ok(QueuedRemoteCandidate::Mdns {
        candidate: candidate.clone(),
        mdns,
    })
}

struct RemoteCandidatePreparer {
    resolver: Option<Arc<dyn MdnsResolver>>,
    cache: HashMap<MdnsName, CachedMdnsResolution>,
    resolution_budget: Duration,
    expansions: usize,
    counters: Arc<IceCandidateCounters>,
}

impl RemoteCandidatePreparer {
    fn new(resolver: Option<Arc<dyn MdnsResolver>>, counters: Arc<IceCandidateCounters>) -> Self {
        Self {
            resolver,
            cache: HashMap::new(),
            resolution_budget: MDNS_SESSION_BUDGET,
            expansions: 0,
            counters,
        }
    }

    async fn prepare_mdns(
        &mut self,
        candidate: &VoiceIceCandidate,
        mdns: &MdnsCandidate,
    ) -> Result<Vec<VoiceIceCandidate>, VoiceRuntimeError> {
        let resolution = if let Some(cached) = self.cache.get(&mdns.name) {
            cached.clone()
        } else {
            let resolution = self.resolve(&mdns.name).await;
            if self.cache.len() < MAX_MDNS_NAMES {
                self.cache.insert(mdns.name.clone(), resolution.clone());
            }
            resolution
        };
        let addresses = match resolution {
            CachedMdnsResolution::Addresses(addresses) => addresses,
            CachedMdnsResolution::Miss(reason) => {
                self.counters.record_skip(reason);
                return Ok(Vec::new());
            }
        };

        let remaining = MAX_MDNS_EXPANSIONS.saturating_sub(self.expansions);
        if remaining == 0 {
            self.counters.record_skip(MdnsSkipReason::ExpansionBudget);
            return Ok(Vec::new());
        }
        let address_count = addresses.len();
        let mut rewritten = Vec::with_capacity(address_count.min(remaining));
        for address in addresses.into_iter().take(remaining) {
            let candidate = rewrite_candidate_address(candidate, &mdns.address_range, address);
            if Candidate::from_sdp_string(&candidate.candidate).is_err() {
                IceCandidateCounters::increment(&self.counters.malformed_rejected, 1);
                return Err(VoiceRuntimeError::InvalidSignal);
            }
            rewritten.push(candidate);
        }
        if rewritten.len() < address_count {
            self.counters.record_skip(MdnsSkipReason::ExpansionBudget);
        }
        self.expansions += rewritten.len();
        Ok(rewritten)
    }

    #[cfg(test)]
    async fn prepare_for_test(
        &mut self,
        candidate: &VoiceIceCandidate,
    ) -> Result<Vec<VoiceIceCandidate>, VoiceRuntimeError> {
        match classify_remote_candidate(candidate, &self.counters)? {
            QueuedRemoteCandidate::Numeric(candidate) => Ok(vec![candidate]),
            QueuedRemoteCandidate::Mdns { candidate, mdns } => {
                self.prepare_mdns(&candidate, &mdns).await
            }
            QueuedRemoteCandidate::UnsupportedTcp => {
                self.counters.record_skip(MdnsSkipReason::UnsupportedTcp);
                Ok(Vec::new())
            }
        }
    }

    async fn resolve(&mut self, name: &MdnsName) -> CachedMdnsResolution {
        if self.cache.len() >= MAX_MDNS_NAMES {
            return CachedMdnsResolution::Miss(MdnsSkipReason::NameBudget);
        }
        if self.resolution_budget.is_zero() {
            return CachedMdnsResolution::Miss(MdnsSkipReason::TimeBudget);
        }
        let Some(resolver) = self.resolver.as_ref() else {
            return CachedMdnsResolution::Miss(MdnsSkipReason::Unavailable);
        };
        let timeout = MDNS_QUERY_TIMEOUT.min(self.resolution_budget);
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(timeout, resolver.resolve(name, timeout)).await;
        self.resolution_budget = self.resolution_budget.saturating_sub(started.elapsed());
        let addresses = match result {
            Err(_) => return CachedMdnsResolution::Miss(MdnsSkipReason::Timeout),
            Ok(Err(MdnsResolveFailure::NoResponse)) => {
                return CachedMdnsResolution::Miss(MdnsSkipReason::NoResponse);
            }
            Ok(Err(MdnsResolveFailure::Unavailable)) => {
                return CachedMdnsResolution::Miss(MdnsSkipReason::Unavailable);
            }
            Ok(Ok(addresses)) => addresses,
        };
        let mut addresses: Vec<_> = addresses
            .into_iter()
            .filter_map(|address| match address {
                IpAddr::V4(address) if usable_mdns_ipv4(address) => Some(address),
                _ => None,
            })
            .collect();
        addresses.sort_unstable();
        addresses.dedup();
        addresses.truncate(MAX_MDNS_ANSWERS);
        if addresses.is_empty() {
            CachedMdnsResolution::Miss(MdnsSkipReason::NoResponse)
        } else {
            CachedMdnsResolution::Addresses(addresses)
        }
    }
}

async fn run_candidate_worker(
    mut commands: mpsc::Receiver<CandidateWorkerCommand>,
    media_commands: mpsc::Sender<MediaCommand>,
    media_events: mpsc::WeakSender<VoiceMediaEvent>,
    mut preparer: RemoteCandidatePreparer,
) {
    while let Some(command) = commands.recv().await {
        let result = match command {
            CandidateWorkerCommand::Candidate(QueuedRemoteCandidate::Numeric(candidate)) => {
                let result = forward_candidates(&media_commands, vec![candidate]).await;
                if result.is_ok() {
                    IceCandidateCounters::increment(&preparer.counters.numeric_accepted, 1);
                }
                result
            }
            CandidateWorkerCommand::Candidate(QueuedRemoteCandidate::Mdns { candidate, mdns }) => {
                match preparer.prepare_mdns(&candidate, &mdns).await {
                    Ok(candidates) if candidates.is_empty() => Ok(()),
                    Ok(candidates) => {
                        let count = candidates.len();
                        let result = forward_candidates(&media_commands, candidates).await;
                        if result.is_ok() {
                            IceCandidateCounters::increment(
                                &preparer.counters.mdns_resolved,
                                count,
                            );
                        }
                        result
                    }
                    Err(error) => Err(error),
                }
            }
            CandidateWorkerCommand::Candidate(QueuedRemoteCandidate::UnsupportedTcp) => {
                preparer
                    .counters
                    .record_skip(MdnsSkipReason::UnsupportedTcp);
                Ok(())
            }
            CandidateWorkerCommand::EndCandidates => {
                preparer.counters.emit_summary();
                forward_end_candidates(&media_commands).await
            }
        };
        if result.is_err() {
            if let Some(media_events) = media_events.upgrade() {
                let _ = media_events.try_send(VoiceMediaEvent::Failed);
            }
            break;
        }
    }
    preparer.counters.emit_summary();
}

async fn forward_candidates(
    commands: &mpsc::Sender<MediaCommand>,
    candidates: Vec<VoiceIceCandidate>,
) -> Result<(), VoiceRuntimeError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .try_send(MediaCommand::AddCandidates(candidates, reply_tx))
        .map_err(|_| VoiceRuntimeError::Closed)?;
    tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
        .await
        .map_err(|_| VoiceRuntimeError::Closed)?
        .map_err(|_| VoiceRuntimeError::Closed)?
}

async fn forward_end_candidates(
    commands: &mpsc::Sender<MediaCommand>,
) -> Result<(), VoiceRuntimeError> {
    let (reply_tx, reply_rx) = oneshot::channel();
    commands
        .try_send(MediaCommand::EndCandidates(reply_tx))
        .map_err(|_| VoiceRuntimeError::Closed)?;
    tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
        .await
        .map_err(|_| VoiceRuntimeError::Closed)?
        .map_err(|_| VoiceRuntimeError::Closed)?
}

enum MediaCommand {
    AcceptOffer(String, oneshot::Sender<Result<String, VoiceRuntimeError>>),
    AddCandidates(
        Vec<VoiceIceCandidate>,
        oneshot::Sender<Result<(), VoiceRuntimeError>>,
    ),
    EndCandidates(oneshot::Sender<Result<(), VoiceRuntimeError>>),
    Play(VoicePcmFrame),
    Close,
}

struct Str0mMediaSession {
    command_tx: mpsc::Sender<MediaCommand>,
    candidate_tx: mpsc::Sender<CandidateWorkerCommand>,
    candidate_task: Option<tokio::task::JoinHandle<()>>,
    event_rx: Option<mpsc::Receiver<VoiceMediaEvent>>,
    audio_rx: Option<mpsc::Receiver<VoicePcmFrame>>,
    closed: Arc<AtomicBool>,
    diagnostics: Arc<IceCandidateCounters>,
}

impl VoiceMediaSession for Str0mMediaSession {
    fn accept_offer<'a>(&'a mut self, offer: &'a str) -> VoiceMediaFuture<'a, String> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.command_tx
                .try_send(MediaCommand::AcceptOffer(offer.to_owned(), reply_tx))
                .map_err(|_| VoiceRuntimeError::Closed)?;
            tokio::time::timeout(COMMAND_TIMEOUT, reply_rx)
                .await
                .map_err(|_| VoiceRuntimeError::Closed)?
                .map_err(|_| VoiceRuntimeError::Closed)?
        })
    }

    fn add_ice_candidate<'a>(
        &'a mut self,
        candidate: &'a VoiceIceCandidate,
    ) -> VoiceMediaFuture<'a, ()> {
        let candidate = classify_remote_candidate(candidate, &self.diagnostics);
        let result = candidate.and_then(|candidate| {
            self.candidate_tx
                .try_send(CandidateWorkerCommand::Candidate(candidate))
                .map_err(|_| VoiceRuntimeError::Closed)
        });
        Box::pin(async move { result })
    }

    fn end_ice_candidates(&mut self) -> VoiceMediaFuture<'_, ()> {
        let result = self
            .candidate_tx
            .try_send(CandidateWorkerCommand::EndCandidates)
            .map_err(|_| VoiceRuntimeError::Closed);
        Box::pin(async move { result })
    }

    fn take_input_audio(&mut self) -> Option<mpsc::Receiver<VoicePcmFrame>> {
        self.audio_rx.take()
    }

    fn take_events(&mut self) -> Option<mpsc::Receiver<VoiceMediaEvent>> {
        self.event_rx.take()
    }

    fn play_output_audio(&mut self, frame: VoicePcmFrame) -> Result<(), VoiceRuntimeError> {
        match self.command_tx.try_send(MediaCommand::Play(frame)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "dropping Nova audio frame because the WebRTC bridge is backpressured"
                );
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(VoiceRuntimeError::Closed),
        }
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::Release);
        if let Some(task) = self.candidate_task.take() {
            task.abort();
        }
        self.diagnostics.emit_summary();
        let _ = self.command_tx.try_send(MediaCommand::Close);
    }
}

impl Drop for Str0mMediaSession {
    fn drop(&mut self) {
        self.close();
    }
}

struct MediaWorkerExitGuard {
    events: mpsc::Sender<VoiceMediaEvent>,
    failure_reported: bool,
    intentional_shutdown: bool,
}

impl MediaWorkerExitGuard {
    fn new(events: mpsc::Sender<VoiceMediaEvent>) -> Self {
        Self {
            events,
            failure_reported: false,
            intentional_shutdown: false,
        }
    }

    fn send(&self, event: VoiceMediaEvent) -> bool {
        self.events.try_send(event).is_ok()
    }

    fn report_failure(&mut self) {
        if self.failure_reported {
            return;
        }
        self.failure_reported = true;
        let _ = self.events.try_send(VoiceMediaEvent::Failed);
    }

    fn mark_intentional_shutdown(&mut self) {
        self.intentional_shutdown = true;
    }
}

impl Drop for MediaWorkerExitGuard {
    fn drop(&mut self) {
        if !self.intentional_shutdown {
            self.report_failure();
        }
    }
}

#[derive(Default)]
struct PostGatherWatchdog {
    deadline: Option<Instant>,
}

impl PostGatherWatchdog {
    fn arm(&mut self, connected: bool, accepted_remote_candidates: usize, now: Instant) {
        if self.deadline.is_none() && !connected && accepted_remote_candidates == 0 {
            self.deadline = Some(now + NO_REMOTE_CANDIDATE_WATCHDOG);
        }
    }

    fn candidate_accepted(&mut self) {
        self.deadline = None;
    }

    fn connection_observed(&mut self) {
        self.deadline = None;
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }
}

fn run_webrtc(
    socket: UdpSocket,
    local_addr: SocketAddr,
    mut command_rx: mpsc::Receiver<MediaCommand>,
    event_tx: mpsc::Sender<VoiceMediaEvent>,
    audio_tx: mpsc::Sender<VoicePcmFrame>,
    closed: Arc<AtomicBool>,
) {
    let mut exit_guard = MediaWorkerExitGuard::new(event_tx);
    let mut rtc = RtcConfig::new().build(Instant::now());
    let Ok(candidate) = Candidate::host(local_addr, "udp") else {
        return;
    };
    if rtc.add_local_candidate(candidate).is_none() {
        return;
    }
    let Ok(mut encoder) = Encoder::new(48_000, Channels::Mono, Application::Voip) else {
        return;
    };
    let Ok(mut decoder) = Decoder::new(48_000, Channels::Mono) else {
        return;
    };
    let mut media_mid = None;
    let mut outgoing_pcm = VecDeque::new();
    let mut media_time = 0_u64;
    let mut next_send_at = None;
    let mut receive_buffer = vec![0_u8; 2_000];
    let mut drain_before_mutation = false;
    let mut remote_ice = RemoteIceState::Open;
    let mut accepted_remote_candidates = 0_usize;
    let mut connected = false;
    let mut post_gather_watchdog = PostGatherWatchdog::default();

    loop {
        if closed.load(Ordering::Acquire) {
            exit_guard.mark_intentional_shutdown();
            return;
        }
        if post_gather_watchdog.expired(Instant::now()) {
            tracing::warn!(
                reason = "no_usable_remote_candidates",
                "voice ICE connection watchdog expired"
            );
            exit_guard.report_failure();
            return;
        }
        let permit_mutation = !drain_before_mutation;
        drain_before_mutation = false;
        if permit_mutation && let Ok(command) = command_rx.try_recv() {
            match command {
                MediaCommand::AcceptOffer(sdp, reply) => {
                    let result = SdpOffer::from_sdp_string(&sdp)
                        .map_err(|_| VoiceRuntimeError::InvalidSignal)
                        .and_then(|offer| {
                            rtc.sdp_api()
                                .accept_offer(offer)
                                .map(|answer| mark_local_ice_complete(answer.to_sdp_string()))
                                .map_err(|_| VoiceRuntimeError::InvalidSignal)
                        });
                    let _ = reply.send(result);
                }
                MediaCommand::AddCandidates(candidates, reply) => {
                    if remote_ice.accept_candidate() {
                        tracing::debug!("accepting a remote ICE candidate that raced completion");
                    }
                    let candidate_count = candidates.len();
                    let result = candidates.into_iter().try_for_each(|candidate| {
                        Candidate::from_sdp_string(&candidate.candidate)
                            .map_err(|_| VoiceRuntimeError::InvalidSignal)
                            .map(|candidate| {
                                rtc.add_remote_candidate(candidate);
                            })
                    });
                    if result.is_ok() {
                        accepted_remote_candidates =
                            accepted_remote_candidates.saturating_add(candidate_count);
                        post_gather_watchdog.candidate_accepted();
                    }
                    let _ = reply.send(result);
                }
                MediaCommand::EndCandidates(reply) => {
                    remote_ice.complete();
                    post_gather_watchdog.arm(connected, accepted_remote_candidates, Instant::now());
                    let _ = reply.send(Ok(()));
                }
                MediaCommand::Play(frame) => {
                    append_48khz(&mut outgoing_pcm, &frame);
                    if outgoing_pcm.len() > MAX_BUFFERED_PCM_SAMPLES {
                        let overflow = outgoing_pcm.len() - MAX_BUFFERED_PCM_SAMPLES;
                        let drop_samples =
                            overflow.div_ceil(OPUS_FRAME_SAMPLES) * OPUS_FRAME_SAMPLES;
                        outgoing_pcm.drain(..drop_samples.min(outgoing_pcm.len()));
                        tracing::warn!(
                            drop_samples,
                            "dropping oldest buffered Nova audio under WebRTC backpressure"
                        );
                    }
                }
                MediaCommand::Close => {
                    exit_guard.mark_intentional_shutdown();
                    return;
                }
            }
        }

        let now = Instant::now();
        if permit_mutation
            && outgoing_pcm.len() >= OPUS_FRAME_SAMPLES
            && let Some(mid) = media_mid
            && next_send_at.is_none_or(|deadline| deadline <= now)
        {
            let pcm: Vec<_> = outgoing_pcm.drain(..OPUS_FRAME_SAMPLES).collect();
            let mut encoded = [0_u8; 1_500];
            let Ok(length) = encoder.encode(&pcm, &mut encoded) else {
                return;
            };
            let Some(writer) = rtc.writer(mid) else {
                return;
            };
            let Some(pt) = writer
                .payload_params()
                .find(|params| params.spec().codec == Codec::Opus)
                .map(|params| params.pt())
            else {
                return;
            };
            if writer
                .write(
                    pt,
                    now,
                    MediaTime::new(media_time, Frequency::FORTY_EIGHT_KHZ),
                    &encoded[..length],
                )
                .is_err()
            {
                return;
            }
            media_time = media_time.saturating_add(OPUS_FRAME_SAMPLES as u64);
            next_send_at = Some(next_rtp_deadline(now));
        }

        loop {
            match rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let _ = socket.send_to(&transmit.contents, transmit.destination);
                }
                Ok(Output::Event(Event::Connected)) => {
                    connected = true;
                    post_gather_watchdog.connection_observed();
                    if !exit_guard.send(VoiceMediaEvent::Connected) {
                        return;
                    }
                }
                Ok(Output::Event(Event::IceConnectionStateChange(
                    IceConnectionState::Disconnected,
                ))) => {
                    exit_guard.report_failure();
                    return;
                }
                Ok(Output::Event(Event::MediaAdded(media))) => {
                    if media.kind.is_audio() {
                        media_mid = Some(media.mid);
                    }
                }
                Ok(Output::Event(Event::MediaData(data)))
                    if data.params.spec().codec == Codec::Opus =>
                {
                    let mut decoded = [0_i16; 5_760];
                    let Ok(length) = decoder.decode(&data.data, &mut decoded, false) else {
                        continue;
                    };
                    let samples = resample_bandlimited(&decoded[..length], 48_000, 16_000);
                    if audio_tx
                        .try_send(VoicePcmFrame {
                            sample_rate_hertz: 16_000,
                            samples,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Output::Event(_)) => {}
                Ok(Output::Timeout(deadline)) => {
                    if deadline <= Instant::now() {
                        if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                            return;
                        }
                        drain_before_mutation = true;
                    }
                    break;
                }
                Err(_) => return,
            }
        }

        if !permit_mutation || drain_before_mutation {
            continue;
        }
        match socket.recv_from(&mut receive_buffer) {
            Ok((length, source)) => {
                let receive = Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local_addr,
                    contents: match receive_buffer[..length].try_into() {
                        Ok(contents) => contents,
                        Err(_) => continue,
                    },
                };
                if rtc
                    .handle_input(Input::Receive(Instant::now(), receive))
                    .is_err()
                {
                    return;
                }
                drain_before_mutation = true;
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteIceState {
    Open,
    Complete,
}

impl RemoteIceState {
    fn accept_candidate(&mut self) -> bool {
        let raced_completion = *self == Self::Complete;
        *self = Self::Open;
        raced_completion
    }

    fn complete(&mut self) {
        *self = Self::Complete;
    }
}

fn next_rtp_deadline(sent_at: Instant) -> Instant {
    sent_at + OPUS_FRAME_DURATION
}

fn mark_local_ice_complete(mut answer: String) -> String {
    if !answer.contains("a=end-of-candidates") {
        if !answer.ends_with("\r\n") {
            answer.push_str("\r\n");
        }
        answer.push_str("a=end-of-candidates\r\n");
    }
    answer
}

fn append_48khz(output: &mut VecDeque<i16>, frame: &VoicePcmFrame) {
    output.extend(resample_bandlimited(
        &frame.samples,
        frame.sample_rate_hertz,
        48_000,
    ));
}

fn resample_bandlimited(input: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    if input.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return input.to_vec();
    }
    const HALF_TAPS: isize = 24;
    let output_len = input
        .len()
        .saturating_mul(output_rate as usize)
        .div_ceil(input_rate as usize);
    let cutoff = (output_rate as f64 / input_rate as f64).min(1.0) * 0.94;
    let ratio = input_rate as f64 / output_rate as f64;
    let mut output = Vec::with_capacity(output_len);
    for output_index in 0..output_len {
        let source = output_index as f64 * ratio;
        let center = source.floor() as isize;
        let mut weighted = 0.0;
        let mut weight_sum = 0.0;
        for tap in (center - HALF_TAPS + 1)..=(center + HALF_TAPS) {
            if !(0..input.len() as isize).contains(&tap) {
                continue;
            }
            let distance = source - tap as f64;
            let normalized = distance / HALF_TAPS as f64;
            if normalized.abs() >= 1.0 {
                continue;
            }
            let sinc_position = std::f64::consts::PI * cutoff * distance;
            let sinc = if sinc_position.abs() < f64::EPSILON {
                1.0
            } else {
                sinc_position.sin() / sinc_position
            };
            let window = 0.42
                + 0.5 * (std::f64::consts::PI * normalized).cos()
                + 0.08 * (2.0 * std::f64::consts::PI * normalized).cos();
            let weight = cutoff * sinc * window;
            weighted += input[tap as usize] as f64 * weight;
            weight_sum += weight;
        }
        let sample = if weight_sum.abs() < f64::EPSILON {
            0.0
        } else {
            weighted / weight_sum
        };
        output.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const MDNS_CANDIDATE: &str = "candidate:1234 1 udp 2122262783 01234567-89ab-cdef-0123-456789abcdef.local 51234 typ host generation 0 network-id 1 network-cost 10 ufrag test";

    enum FakeResponse {
        Addresses(Vec<IpAddr>),
        Failure(MdnsResolveFailure),
        Pending,
        Cancellable(Arc<AtomicBool>),
        Panic,
    }

    struct CancellableResolution(Arc<AtomicBool>);

    impl Future for CancellableResolution {
        type Output = Result<Vec<IpAddr>, MdnsResolveFailure>;

        fn poll(
            self: Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for CancellableResolution {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    struct FakeResolver {
        calls: AtomicUsize,
        responses: Mutex<VecDeque<FakeResponse>>,
    }

    impl FakeResolver {
        fn new(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                responses: Mutex::new(responses.into_iter().collect()),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl MdnsResolver for FakeResolver {
        fn resolve<'a>(&'a self, _name: &'a MdnsName, _timeout: Duration) -> MdnsResolveFuture<'a> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            match response {
                FakeResponse::Addresses(addresses) => Box::pin(async move { Ok(addresses) }),
                FakeResponse::Failure(error) => Box::pin(async move { Err(error) }),
                FakeResponse::Pending => Box::pin(std::future::pending()),
                FakeResponse::Cancellable(cancelled) => Box::pin(CancellableResolution(cancelled)),
                FakeResponse::Panic => panic!("ineligible candidate reached the resolver"),
            }
        }
    }

    fn wire_candidate(value: impl Into<String>) -> VoiceIceCandidate {
        VoiceIceCandidate {
            candidate: value.into(),
            sdp_mid: Some("audio".to_owned()),
            sdp_m_line_index: Some(0),
        }
    }

    fn preparer_with(resolver: Arc<FakeResolver>) -> RemoteCandidatePreparer {
        RemoteCandidatePreparer::new(Some(resolver), Arc::new(IceCandidateCounters::default()))
    }

    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Relaxed)
    }

    fn mdns_with_label(label: &str, port: u16) -> VoiceIceCandidate {
        wire_candidate(format!(
            "candidate:1234 1 udp 2122262783 {label}.local {port} typ host generation 0"
        ))
    }

    #[test]
    fn locked_parser_rejects_mdns_and_accepts_exact_address_rewrite() {
        assert!(Candidate::from_sdp_string(MDNS_CANDIDATE).is_err());
        let candidate = wire_candidate(MDNS_CANDIDATE);
        let parsed = mdns_candidate(&candidate.candidate).unwrap();
        let rewritten = rewrite_candidate_address(
            &candidate,
            &parsed.address_range,
            Ipv4Addr::new(198, 51, 100, 7),
        );
        let parsed = Candidate::from_sdp_string(&rewritten.candidate).unwrap();
        assert_eq!(parsed.addr().ip(), Ipv4Addr::new(198, 51, 100, 7));
        assert_eq!(parsed.addr().port(), 51234);
        assert_eq!(parsed.prio(), 2_122_262_783);
    }

    #[test]
    fn mdns_gate_accepts_only_single_label_host_candidates() {
        let parsed = mdns_candidate(MDNS_CANDIDATE).unwrap();
        assert_eq!(parsed.protocol, MdnsProtocol::Udp);
        assert_eq!(
            parsed.name.as_str(),
            "01234567-89ab-cdef-0123-456789abcdef.local."
        );
        assert_eq!(
            mdns_candidate(&MDNS_CANDIDATE.replace(".local", ".LOCAL"))
                .unwrap()
                .name
                .as_str(),
            "01234567-89ab-cdef-0123-456789abcdef.local."
        );

        let tcp = MDNS_CANDIDATE.replacen(" udp ", " tcp ", 1);
        assert_eq!(mdns_candidate(&tcp).unwrap().protocol, MdnsProtocol::Tcp);

        let rejected = [
            MDNS_CANDIDATE.replace(".local", ".part.local"),
            MDNS_CANDIDATE.replace(".local", ".local."),
            MDNS_CANDIDATE.replace(".local", ".local.evil"),
            MDNS_CANDIDATE.replace("01234567-89ab-cdef-0123-456789abcdef.local", ".local"),
            MDNS_CANDIDATE.replace(
                "01234567-89ab-cdef-0123-456789abcdef.local",
                "bad_name.local",
            ),
            MDNS_CANDIDATE.replace(" 1 udp ", " bad udp "),
            MDNS_CANDIDATE.replace(" udp 2122262783 ", " udp bad "),
            MDNS_CANDIDATE.replace(" 51234 typ ", " bad typ "),
            MDNS_CANDIDATE.replace(" typ host ", " nope host "),
            MDNS_CANDIDATE.replace(" typ host ", " typ relay "),
            format!("{MDNS_CANDIDATE} dangling"),
            MDNS_CANDIDATE.replace(".local", "%2elocal"),
        ];
        for candidate in rejected {
            assert!(mdns_candidate(&candidate).is_none());
        }
        let long_label = "a".repeat(64);
        assert!(mdns_candidate(&mdns_with_label(&long_label, 51234).candidate).is_none());
        assert!(mdns_candidate("candidate:").is_none());
        assert!(mdns_candidate("candidate:1234 1 udp 1 192.0.2.1 9 typ host").is_none());
        assert!(
            mdns_candidate(
                "candidate:1234 1 udp 1 192.0.2.1 9 typ host raddr hidden.local rport 9"
            )
            .is_none()
        );
    }

    #[test]
    fn rewrite_preserves_every_byte_outside_the_address_and_metadata() {
        let candidate = VoiceIceCandidate {
            candidate: MDNS_CANDIDATE.to_owned(),
            sdp_mid: Some("non-default-mid".to_owned()),
            sdp_m_line_index: Some(7),
        };
        let mdns = mdns_candidate(&candidate.candidate).unwrap();
        let rewritten = rewrite_candidate_address(
            &candidate,
            &mdns.address_range,
            Ipv4Addr::new(203, 0, 113, 9),
        );
        assert_eq!(
            &rewritten.candidate[..mdns.address_range.start],
            &candidate.candidate[..mdns.address_range.start]
        );
        let rewritten_suffix = mdns.address_range.start + "203.0.113.9".len();
        assert_eq!(
            &rewritten.candidate[rewritten_suffix..],
            &candidate.candidate[mdns.address_range.end..]
        );
        assert_eq!(rewritten.sdp_mid, candidate.sdp_mid);
        assert_eq!(rewritten.sdp_m_line_index, candidate.sdp_m_line_index);
    }

    #[tokio::test]
    async fn numeric_candidates_never_call_the_resolver() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Panic]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        let candidate = wire_candidate(
            "candidate:1234 1 udp 2122262783 192.0.2.10 51234 typ host generation 0",
        );
        let prepared = preparer.prepare_for_test(&candidate).await.unwrap();
        assert_eq!(prepared, vec![candidate]);
        assert_eq!(resolver.calls(), 0);
    }

    #[tokio::test]
    async fn positive_and_negative_results_are_cached_per_session() {
        let resolver = Arc::new(FakeResolver::new([
            FakeResponse::Addresses(vec![IpAddr::V4(Ipv4Addr::new(198, 51, 100, 8))]),
            FakeResponse::Failure(MdnsResolveFailure::NoResponse),
        ]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        let first = mdns_with_label("first", 5000);
        assert_eq!(preparer.prepare_for_test(&first).await.unwrap().len(), 1);
        let first_again = mdns_with_label("first", 5001);
        assert_eq!(
            preparer.prepare_for_test(&first_again).await.unwrap().len(),
            1
        );
        let missing = mdns_with_label("missing", 5002);
        assert!(
            preparer
                .prepare_for_test(&missing)
                .await
                .unwrap()
                .is_empty()
        );
        let missing_again = mdns_with_label("missing", 5003);
        assert!(
            preparer
                .prepare_for_test(&missing_again)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(resolver.calls(), 2);
    }

    #[tokio::test]
    async fn unresolved_mdns_and_unsupported_tcp_are_narrowly_skipped() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Failure(
            MdnsResolveFailure::Unavailable,
        )]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        assert!(
            preparer
                .prepare_for_test(&wire_candidate(MDNS_CANDIDATE))
                .await
                .unwrap()
                .is_empty()
        );
        let tcp = wire_candidate(MDNS_CANDIDATE.replacen(" udp ", " tcp ", 1));
        assert!(preparer.prepare_for_test(&tcp).await.unwrap().is_empty());
        assert_eq!(resolver.calls(), 1);
        assert_eq!(count(&preparer.counters.mdns_skipped_unavailable), 1);
        assert_eq!(count(&preparer.counters.mdns_skipped_unsupported_tcp), 1);
    }

    #[tokio::test]
    async fn malformed_non_mdns_candidates_remain_fatal() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Panic]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        let result = preparer
            .prepare_for_test(&wire_candidate(
                "candidate:1234 1 udp 2122262783 bad.example 51234 typ host",
            ))
            .await;
        assert_eq!(result, Err(VoiceRuntimeError::InvalidSignal));
        assert_eq!(resolver.calls(), 0);
        assert_eq!(count(&preparer.counters.malformed_rejected), 1);
    }

    #[tokio::test]
    async fn malformed_extensions_fail_preflight_without_a_resolver_call() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Panic]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        let candidate = wire_candidate(format!("{MDNS_CANDIDATE} tcptype invalid"));
        assert_eq!(
            preparer.prepare_for_test(&candidate).await,
            Err(VoiceRuntimeError::InvalidSignal)
        );
        assert_eq!(resolver.calls(), 0);
        assert_eq!(count(&preparer.counters.malformed_rejected), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_resolution_stops_at_the_per_query_budget() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Cancellable(Arc::clone(
            &cancelled,
        ))]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        let started = tokio::time::Instant::now();
        assert!(
            preparer
                .prepare_for_test(&wire_candidate(MDNS_CANDIDATE))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(started.elapsed(), MDNS_QUERY_TIMEOUT);
        assert_eq!(count(&preparer.counters.mdns_skipped_timeout), 1);
        assert_eq!(resolver.calls(), 1);
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn answer_and_expansion_caps_are_deterministic() {
        let answers = (1..=6)
            .map(|last| IpAddr::V4(Ipv4Addr::new(198, 51, 100, last)))
            .collect();
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Addresses(answers)]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        assert_eq!(
            preparer
                .prepare_for_test(&mdns_with_label("bounded", 5000))
                .await
                .unwrap()
                .len(),
            MAX_MDNS_ANSWERS
        );
        assert_eq!(
            preparer
                .prepare_for_test(&mdns_with_label("bounded", 5001))
                .await
                .unwrap()
                .len(),
            MAX_MDNS_ANSWERS
        );
        assert!(
            preparer
                .prepare_for_test(&mdns_with_label("bounded", 5002))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(resolver.calls(), 1);
        assert_eq!(preparer.expansions, MAX_MDNS_EXPANSIONS);
        assert_eq!(count(&preparer.counters.mdns_skipped_expansion_budget), 1);
    }

    #[tokio::test]
    async fn distinct_name_budget_prevents_a_fifth_query() {
        let responses = (0..MAX_MDNS_NAMES).map(|last| {
            FakeResponse::Addresses(vec![IpAddr::V4(Ipv4Addr::new(
                198,
                51,
                100,
                last as u8 + 1,
            ))])
        });
        let resolver = Arc::new(FakeResolver::new(responses));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        for label in ["one", "two", "three", "four"] {
            assert_eq!(
                preparer
                    .prepare_for_test(&mdns_with_label(label, 5000))
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
        assert!(
            preparer
                .prepare_for_test(&mdns_with_label("five", 5000))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(resolver.calls(), MAX_MDNS_NAMES);
        assert_eq!(count(&preparer.counters.mdns_skipped_name_budget), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn total_resolution_budget_cannot_be_multiplied_by_distinct_names() {
        let resolver = Arc::new(FakeResolver::new([
            FakeResponse::Pending,
            FakeResponse::Pending,
        ]));
        let mut preparer = preparer_with(Arc::clone(&resolver));
        for label in ["one", "two"] {
            assert!(
                preparer
                    .prepare_for_test(&mdns_with_label(label, 5000))
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            preparer
                .prepare_for_test(&mdns_with_label("three", 5000))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(resolver.calls(), 2);
        assert_eq!(preparer.resolution_budget, Duration::ZERO);
        assert_eq!(count(&preparer.counters.mdns_skipped_timeout), 2);
        assert_eq!(count(&preparer.counters.mdns_skipped_time_budget), 1);
    }

    #[tokio::test]
    async fn only_usable_ipv4_answers_reach_str0m() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Addresses(vec![
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V6("2001:db8::1".parse().unwrap()),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)),
        ])]));
        let mut preparer = preparer_with(resolver);
        let prepared = preparer
            .prepare_for_test(&wire_candidate(MDNS_CANDIDATE))
            .await
            .unwrap();
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            Candidate::from_sdp_string(&prepared[0].candidate)
                .unwrap()
                .addr()
                .ip(),
            Ipv4Addr::new(192, 0, 2, 20)
        );
    }

    #[tokio::test]
    async fn resolver_worker_preserves_candidate_and_end_order() {
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Addresses(vec![
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, 40)),
        ])]));
        let diagnostics = Arc::new(IceCandidateCounters::default());
        let preparer = RemoteCandidatePreparer::new(Some(resolver), Arc::clone(&diagnostics));
        let (candidate_tx, candidate_rx) = mpsc::channel(3);
        let (media_tx, mut media_rx) = mpsc::channel(3);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let worker = tokio::spawn(run_candidate_worker(
            candidate_rx,
            media_tx,
            event_tx.downgrade(),
            preparer,
        ));

        let mdns = mdns_with_label("ordered", 5000);
        candidate_tx
            .send(CandidateWorkerCommand::Candidate(
                classify_remote_candidate(&mdns, &diagnostics).unwrap(),
            ))
            .await
            .unwrap();
        let numeric =
            wire_candidate("candidate:1234 1 udp 2122262783 192.0.2.10 5001 typ host generation 0");
        candidate_tx
            .send(CandidateWorkerCommand::Candidate(
                classify_remote_candidate(&numeric, &diagnostics).unwrap(),
            ))
            .await
            .unwrap();
        candidate_tx
            .send(CandidateWorkerCommand::EndCandidates)
            .await
            .unwrap();

        let MediaCommand::AddCandidates(candidates, reply) = media_rx.recv().await.unwrap() else {
            panic!("resolved mDNS candidate must be forwarded first");
        };
        assert_eq!(candidates.len(), 1);
        let resolved = Candidate::from_sdp_string(&candidates[0].candidate).unwrap();
        assert_eq!(resolved.addr().ip(), Ipv4Addr::new(198, 51, 100, 40));
        assert_eq!(resolved.addr().port(), 5000);
        reply.send(Ok(())).unwrap();

        let MediaCommand::AddCandidates(candidates, reply) = media_rx.recv().await.unwrap() else {
            panic!("numeric candidate must remain behind the earlier mDNS candidate");
        };
        assert_eq!(candidates, vec![numeric]);
        reply.send(Ok(())).unwrap();

        let MediaCommand::EndCandidates(reply) = media_rx.recv().await.unwrap() else {
            panic!("end-of-candidates must remain behind every queued candidate");
        };
        reply.send(Ok(())).unwrap();
        drop(candidate_tx);
        worker.await.unwrap();
        assert!(event_rx.try_recv().is_err());
        assert_eq!(count(&diagnostics.numeric_accepted), 1);
        assert_eq!(count(&diagnostics.mdns_resolved), 1);
    }

    #[tokio::test]
    async fn cancelling_the_resolver_worker_cancels_its_active_query() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let resolver = Arc::new(FakeResolver::new([FakeResponse::Cancellable(Arc::clone(
            &cancelled,
        ))]));
        let diagnostics = Arc::new(IceCandidateCounters::default());
        let resolver_for_preparer: Arc<dyn MdnsResolver> = resolver.clone();
        let preparer =
            RemoteCandidatePreparer::new(Some(resolver_for_preparer), Arc::clone(&diagnostics));
        let (candidate_tx, candidate_rx) = mpsc::channel(1);
        let (media_tx, _media_rx) = mpsc::channel(1);
        let (event_tx, _event_rx) = mpsc::channel(1);
        let worker = tokio::spawn(run_candidate_worker(
            candidate_rx,
            media_tx,
            event_tx.downgrade(),
            preparer,
        ));
        let candidate = wire_candidate(MDNS_CANDIDATE);
        candidate_tx
            .send(CandidateWorkerCommand::Candidate(
                classify_remote_candidate(&candidate, &diagnostics).unwrap(),
            ))
            .await
            .unwrap();
        while resolver.calls() == 0 {
            tokio::task::yield_now().await;
        }
        worker.abort();
        let _ = worker.await;
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn silent_media_worker_death_is_terminal_with_an_idle_resolver_worker() {
        let diagnostics = Arc::new(IceCandidateCounters::default());
        let preparer = RemoteCandidatePreparer::new(None, diagnostics);
        let (candidate_tx, candidate_rx) = mpsc::channel(1);
        let (media_tx, _media_rx) = mpsc::channel(1);
        let (event_tx, mut event_rx) = mpsc::channel(2);
        let worker = tokio::spawn(run_candidate_worker(
            candidate_rx,
            media_tx,
            event_tx.downgrade(),
            preparer,
        ));

        drop(MediaWorkerExitGuard::new(event_tx));
        assert!(matches!(event_rx.try_recv(), Ok(VoiceMediaEvent::Failed)));
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));

        drop(candidate_tx);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn intentional_media_worker_shutdown_closes_without_failure() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let mut guard = MediaWorkerExitGuard::new(event_tx);
        guard.mark_intentional_shutdown();
        drop(guard);
        assert!(event_rx.recv().await.is_none());
    }

    #[test]
    fn zero_candidate_watchdog_arms_only_after_gathering_and_before_connection() {
        let now = Instant::now();
        let mut watchdog = PostGatherWatchdog::default();
        watchdog.arm(false, 1, now);
        assert!(watchdog.deadline.is_none());
        watchdog.arm(true, 0, now);
        assert!(watchdog.deadline.is_none());
        watchdog.arm(false, 0, now);
        assert_eq!(watchdog.deadline, Some(now + NO_REMOTE_CANDIDATE_WATCHDOG));
    }

    #[test]
    fn watchdog_arm_then_connected_event_disarms_before_expiry() {
        let now = Instant::now();
        let mut watchdog = PostGatherWatchdog::default();
        watchdog.arm(false, 0, now);
        assert!(watchdog.deadline.is_some());
        watchdog.connection_observed();
        assert!(!watchdog.expired(now + NO_REMOTE_CANDIDATE_WATCHDOG));
    }

    #[test]
    fn diagnostic_event_exposes_only_bounded_counts() {
        let counters = IceCandidateCounters::default();
        IceCandidateCounters::increment(&counters.numeric_accepted, 1);
        IceCandidateCounters::increment(&counters.mdns_resolved, 2);
        counters.record_skip(MdnsSkipReason::NoResponse);
        counters.record_skip(MdnsSkipReason::UnsupportedTcp);
        IceCandidateCounters::increment(&counters.malformed_rejected, 1);
        let event = counters.snapshot();
        assert_eq!(
            event,
            IceCandidateDiagnosticEvent {
                numeric_accepted: 1,
                mdns_resolved: 2,
                mdns_skipped_no_response: 1,
                mdns_skipped_unavailable: 0,
                mdns_skipped_timeout: 0,
                mdns_skipped_name_budget: 0,
                mdns_skipped_time_budget: 0,
                mdns_skipped_expansion_budget: 0,
                mdns_skipped_unsupported_tcp: 1,
                malformed_rejected: 1,
            }
        );
        let rendered = event.to_string();
        assert_eq!(
            rendered,
            "numeric_accepted=1 mdns_resolved=2 mdns_skipped_no_response=1 \
             mdns_skipped_unavailable=0 mdns_skipped_timeout=0 \
             mdns_skipped_name_budget=0 mdns_skipped_time_budget=0 \
             mdns_skipped_expansion_budget=0 mdns_skipped_unsupported_tcp=1 \
             malformed_rejected=1"
        );
        assert!(!rendered.contains(".local"));
        assert!(!rendered.contains("192."));
    }

    #[test]
    fn hermetic_resampler_produces_opus_frame_rate() {
        let mut output = VecDeque::new();
        append_48khz(
            &mut output,
            &VoicePcmFrame {
                sample_rate_hertz: 24_000,
                samples: vec![1; 480],
            },
        );
        assert_eq!(output.len(), 960);
    }

    #[test]
    fn downsampler_attenuates_energy_above_the_destination_nyquist_limit() {
        fn tone(frequency: f64) -> Vec<i16> {
            (0..4_800)
                .map(|sample| {
                    ((2.0 * std::f64::consts::PI * frequency * sample as f64 / 48_000.0).sin()
                        * 12_000.0) as i16
                })
                .collect()
        }
        fn rms(samples: &[i16]) -> f64 {
            let interior = &samples[100..samples.len() - 100];
            (interior
                .iter()
                .map(|sample| (*sample as f64).powi(2))
                .sum::<f64>()
                / interior.len() as f64)
                .sqrt()
        }
        let passband = resample_bandlimited(&tone(1_000.0), 48_000, 16_000);
        let rejected = resample_bandlimited(&tone(12_000.0), 48_000, 16_000);
        assert!(rms(&rejected) < rms(&passband) * 0.1);
    }

    #[test]
    fn upsampler_suppresses_spectral_images() {
        fn amplitude(samples: &[i16], sample_rate: f64, frequency: f64) -> f64 {
            let interior = &samples[200..samples.len() - 200];
            let (sin, cos) =
                interior
                    .iter()
                    .enumerate()
                    .fold((0.0, 0.0), |(sin, cos), (index, sample)| {
                        let phase =
                            2.0 * std::f64::consts::PI * frequency * index as f64 / sample_rate;
                        (
                            sin + *sample as f64 * phase.sin(),
                            cos + *sample as f64 * phase.cos(),
                        )
                    });
            sin.hypot(cos) / interior.len() as f64
        }
        let input: Vec<i16> = (0..1_600)
            .map(|sample| {
                ((2.0 * std::f64::consts::PI * 1_000.0 * sample as f64 / 16_000.0).sin() * 12_000.0)
                    as i16
            })
            .collect();
        let output = resample_bandlimited(&input, 16_000, 48_000);
        let fundamental = amplitude(&output, 48_000.0, 1_000.0);
        let image = amplitude(&output, 48_000.0, 15_000.0);
        assert!(image < fundamental * 0.1);
    }

    #[test]
    fn rtp_deadlines_never_schedule_catch_up_bursts() {
        let sent_at = Instant::now();
        assert_eq!(
            next_rtp_deadline(sent_at).duration_since(sent_at),
            OPUS_FRAME_DURATION
        );
    }

    #[test]
    fn ice_completion_is_idempotent_and_accepts_a_racing_candidate() {
        let mut state = RemoteIceState::Open;
        state.complete();
        state.complete();
        assert!(state.accept_candidate());
        assert_eq!(state, RemoteIceState::Open);
        assert!(!state.accept_candidate());
    }

    #[test]
    fn answer_marks_the_pregathered_host_candidate_complete_once() {
        let answer =
            mark_local_ice_complete("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n".to_owned());
        assert_eq!(answer.matches("a=end-of-candidates").count(), 1);
        assert_eq!(mark_local_ice_complete(answer.clone()), answer);
    }

    #[test]
    fn hermetic_opus_path_encodes_and_decodes_one_audio_track_frame() {
        let mut encoder =
            Encoder::new(48_000, Channels::Mono, Application::Voip).expect("create Opus encoder");
        let mut decoder = Decoder::new(48_000, Channels::Mono).expect("create Opus decoder");
        let pcm: Vec<i16> = (0..960)
            .map(|sample| ((sample as f32 * 0.08).sin() * 8_000.0) as i16)
            .collect();
        let mut encoded = [0_u8; 1_500];
        let encoded_len = encoder.encode(&pcm, &mut encoded).expect("encode Opus");
        assert!(encoded_len > 0);
        let mut decoded = [0_i16; 960];
        let decoded_len = decoder
            .decode(&encoded[..encoded_len], &mut decoded, false)
            .expect("decode Opus");
        assert_eq!(decoded_len, 960);
        assert!(decoded.iter().any(|sample| *sample != 0));
    }
}
