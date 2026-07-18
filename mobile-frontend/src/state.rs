use std::collections::{HashMap, HashSet};

use leptos::prelude::*;

use crate::bridge::{LocalSubmissionId, SubmissionTransportOutcome};
pub use mobile_shell_types::{
    LocalHostId, MobilePairingPreview, MobileShellError, PairedHostConnectionStatus,
    PairedHostSummary,
};
pub use protocol::MobileServiceAuthState;
use protocol::types::AgentCompactNotifyPayload;
use protocol::{
    AgentId, AgentOrigin, BackendKind, BackendSetupInfo, ChatMessage, ChatMessageId, CustomAgent,
    CustomAgentId, DiffContextMode, HostAbsPath, HostBrowseEntriesPayload, HostBrowseErrorPayload,
    HostBrowseOpenedPayload, HostSettings, McpServerConfig, McpServerId, MessageMetadataUpdateData,
    MobileAccessErrorCode, Project, ProjectDiffScope, ProjectFileContentsPayload,
    ProjectGitDiffFile, ProjectId, ProjectPath, ProjectRootGitStatus, ProjectRootListing,
    ProjectRootPath, QueuedMessageEntry, Review, ReviewErrorPayload, ReviewId, ReviewSummary,
    SessionId, SessionListCursor, SessionListPageInfo, SessionListPageStatus, SessionSchemaEntry,
    SessionSettingsValues, SessionSummary, Skill, SkillId, Steering, SteeringId, StreamPath,
    TaskList, Team, TeamCompactNotifyPayload, TeamDraft, TeamDraftId, TeamMember,
    TeamMemberBindingPayload, TeamMemberId, TeamMemberShuffleSuggestion, TeamPresetCatalog,
    ToolExecutionCompletedData, ToolRequest, TydeReleaseVersion,
};

// ── Tool output viewing mode ───────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolOutputMode {
    Summary,
    Compact,
    Full,
}

// ── Connection status ──────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Bootstrapping,
    Connected,
    Error(String),
    /// The MQTT transport connected, but the Tyde application handshake was
    /// rejected because the host speaks a protocol version this build cannot.
    /// This is a higher-order truth than transport connectivity: it stays true
    /// across transport reconnects, so it is **sticky** — automatic reconnect
    /// statuses (`Connecting`/`Connected`/`Disconnected`/`Failed`) must not
    /// overwrite it (see `app::apply_connection_status`). Only a successful
    /// `Welcome` (a genuinely compatible reconnect) or forgetting the host
    /// clears it. Carries the typed protocol numbers so the UI can render an
    /// actionable "update required" message instead of an indefinite spinner.
    /// `release_version` is the host's exact published build from the reject
    /// (`RejectPayload::release_version`) when the host sent one, so the message
    /// can name the host build rather than only bare protocol integers.
    UpdateRequired {
        host_protocol: u32,
        app_protocol: u32,
        release_version: Option<TydeReleaseVersion>,
    },
    /// A managed pairing stopped with a terminal, user-actionable failure: it
    /// needs the user to sign in with Tyggs again (`ServiceAuthRequired` /
    /// `PassRequired`) or re-pair (`RepairRequired`). The connection actor has
    /// stopped retrying, so the picker renders an explicit action (sign-in /
    /// re-pair / forget) instead of a spinner or a dismissible generic error
    /// (findings #3/#7). Carries the typed code so the UI picks the right action.
    NeedsAction {
        code: MobileAccessErrorCode,
        message: String,
    },
}

/// Whether `code` is a terminal, user-actionable managed-access failure that the
/// UI should surface with a sign-in / re-pair affordance rather than a generic
/// error or an endless spinner.
pub fn is_actionable_managed_failure(code: MobileAccessErrorCode) -> bool {
    matches!(
        code,
        MobileAccessErrorCode::ServiceAuthRequired
            | MobileAccessErrorCode::PassRequired
            | MobileAccessErrorCode::RepairRequired
    )
}

/// Whether the actionable failure is specifically a "sign in with Tyggs again"
/// case (vs. a "re-pair" case). Drives which button the UI shows.
pub fn needs_tyggs_sign_in(code: MobileAccessErrorCode) -> bool {
    matches!(
        code,
        MobileAccessErrorCode::ServiceAuthRequired | MobileAccessErrorCode::PassRequired
    )
}

/// Actionable, user-facing message for an incompatible-protocol reject. Shared
/// by every surface that renders [`ConnectionStatus::UpdateRequired`] so the
/// wording stays consistent. Names the host build when the reject carried a
/// `release_version`; otherwise falls back to the protocol integers alone.
pub fn update_required_message(
    host_protocol: u32,
    app_protocol: u32,
    release_version: Option<&TydeReleaseVersion>,
) -> String {
    match release_version {
        Some(version) => format!(
            "Update required — host build {version} (protocol {host_protocol}, app protocol {app_protocol})"
        ),
        None => {
            format!("Update required — host protocol {host_protocol}, app protocol {app_protocol}")
        }
    }
}

impl From<PairedHostConnectionStatus> for ConnectionStatus {
    fn from(status: PairedHostConnectionStatus) -> Self {
        match status {
            PairedHostConnectionStatus::Connecting => ConnectionStatus::Connecting,
            PairedHostConnectionStatus::Connected => ConnectionStatus::Bootstrapping,
            PairedHostConnectionStatus::Disconnected { .. } => ConnectionStatus::Disconnected,
            PairedHostConnectionStatus::Failed { code, message } => {
                connection_failed_status(code, message)
            }
        }
    }
}

/// Maps a terminal `Failed { code, message }` onto the connection status the UI
/// renders: an actionable [`ConnectionStatus::NeedsAction`] for sign-in / re-pair
/// cases, otherwise a plain error.
pub fn connection_failed_status(code: MobileAccessErrorCode, message: String) -> ConnectionStatus {
    if is_actionable_managed_failure(code) {
        ConnectionStatus::NeedsAction { code, message }
    } else {
        ConnectionStatus::Error(message)
    }
}

// ── Outbound submission recovery ───────────────────────────────────────
//
// The typed transport vocabulary — `LocalSubmissionId`, `Accepted`,
// `SendRejected`, and the four transport facts in `SubmissionTransportOutcome`
// — is owned and minted by the connection layer (`crate::bridge`). It is
// consumed here, never re-declared: a parallel copy of those types in `state`
// would be two sources of truth for one wire fact.
//
// What `state` owns is the *recovery model* built on top of them: what the
// client is holding for the user, for which target, and what it is allowed to
// claim about it.
//
// The client makes **no** claim about server receipt or application, and never
// attributes a server event to a submission. There is no event correlation and
// no inference available under the current protocol: a record is retired only
// on a client-local *transport* fact, never on a server event.

/// Stable, client-local identity for one **logical** submission — the thing the
/// user made, as distinct from one transport attempt at sending it.
///
/// A [`LocalSubmissionId`] names a single attempt: a deliberate resend mints a
/// brand-new one, and the old record is retired. The UI that *originated* the
/// submission — a tool card sitting in the transcript — has to keep describing it
/// across those attempts, so it holds this instead and the replacement record
/// inherits it.
///
/// This is **not** correlation with any server event. It never goes on the wire.
/// It is a UI identity, and it says nothing about what the host did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionOriginId(pub u64);

/// Why a submission's record left the pending store *without* the transport
/// finishing with it.
///
/// A record disappearing is ambiguous on its own: it can mean the broker
/// acknowledged the publish, or it can mean the user took the message back.
/// "Queued locally" is the last true statement about the first and an outright
/// lie about the second, so the client records the fact it actually knows rather
/// than guessing from the absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionWithdrawal {
    /// The user discarded it. It will never be sent.
    Discarded,
    /// The user pulled the text back into the composer. It is theirs again.
    ReturnedToComposer,
}

/// What became of a logical submission, from the point of view of the UI that
/// created it.
///
/// Every variant is a fact the client actually holds. None of them claims the
/// host received or applied anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionLifecycle {
    /// Admitted and still on its way out (or already retired by a broker ack —
    /// which is a transport fact only, and leaves "queued locally" as the last
    /// true thing that can be said).
    QueuedLocally,
    /// Definitely never transmitted.
    NotSent,
    /// May or may not have reached the host.
    DeliveryUnknown,
    /// The user took it back.
    Withdrawn(SubmissionWithdrawal),
}

/// A terminal fact about a logical submission the user took back.
///
/// Carries the host so the fact can be dropped at the one point where every UI
/// that could consult it is provably gone — see [`AppState::clear_host_runtime`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WithdrawnSubmission {
    pub local_host_id: LocalHostId,
    pub withdrawal: SubmissionWithdrawal,
}

/// A withdrawal count that is implausible for a human tapping Discard, and so
/// almost certainly means something is minting withdrawals in a loop.
///
/// **This does not evict.** Dropping a withdrawal is dropping the truth about a
/// message the user destroyed, and there is no count at which that becomes
/// acceptable. It logs, loudly, so a real leak is visible instead of silently
/// eating the facts that make the UI honest.
const IMPLAUSIBLE_WITHDRAWAL_COUNT: usize = 4096;

/// Where a submission was headed.
///
/// A new-chat submission has **no agent**: the agent does not exist yet, and
/// guessing which `NewAgent` is "ours" is exactly the inference this model
/// bans. New-chat recovery is therefore host-scoped, never agent-scoped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SubmissionTarget {
    /// Spawns a new agent. No agent ownership is known or guessed.
    NewChat,
    /// Sent to an agent that already exists — ownership is known, because that
    /// is the agent we sent to.
    Agent(AgentRef),
}

/// Resting state of a recovery record.
///
/// `BrokerAcknowledged` is deliberately not representable here: it *retires*
/// the record. A record can therefore never rest in a state that claims
/// delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingSubmissionState {
    /// Admitted to this connection's bounded outbound queue. Held **silently** —
    /// no banner, no artifact, no announcement. The happy path is
    /// indistinguishable from having no recovery model at all.
    QueuedLocally,
    /// Definitely not transmitted. Safe to send again with zero duplicate risk.
    NotSent,
    /// May or may not have reached the host. Persists until the user explicitly
    /// resolves it.
    DeliveryUnknown,
}

impl PendingSubmissionState {
    /// Whether this record is shown to the user at all. Pending work is
    /// surfaced **only** on failure, so a healthy send is never an artifact the
    /// user has to dismiss.
    pub fn is_surfaced(&self) -> bool {
        !matches!(self, PendingSubmissionState::QueuedLocally)
    }
}

/// One outbound user submission the client is holding on the user's behalf.
///
/// Text and images move here **atomically on admission** — the composer is
/// cleared in the same synchronous step that creates the record, so there is no
/// window in which the user's input has no holder. That window was the defect:
/// the composer used to be cleared *before* the send was awaited, so a send
/// that never settled destroyed the text with no error and no holder.
///
/// In-memory and non-persistent by design: this is client-originated input, not
/// a mirror of server state. A page reload drops it, exactly as an unsent
/// composer draft has always been dropped.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingSubmission {
    pub local_submission_id: LocalSubmissionId,
    /// The logical submission this attempt belongs to. A deliberate resend mints
    /// a new `local_submission_id` and **inherits** this, so the UI that created
    /// the submission keeps tracking it across attempts.
    pub origin: SubmissionOriginId,
    pub local_host_id: LocalHostId,
    /// The connection instance that admitted the frame. A transport outcome
    /// reported by a *different* instance can never mutate this record, and a
    /// resend is only ever offered once the admitting connection is gone.
    pub connection_instance_id: u64,
    pub target: SubmissionTarget,
    pub text: String,
    pub images: Vec<protocol::ImageData>,
    /// A typed tool response riding the same `SendMessage` frame — a plan
    /// approval, or a rejection with feedback.
    ///
    /// It has to be held with the record, not reconstructed: for a plan decision
    /// the payload **is** the decision and `text` is empty, so a resend that
    /// dropped this would put an empty chat message on the wire and leave the
    /// agent still waiting for an answer it never gets.
    pub tool_response: Option<protocol::SendMessageToolResponse>,
    pub state: PendingSubmissionState,
}

impl PendingSubmission {
    /// The held images, shaped for the wire **exactly as a first-time send would
    /// shape them**.
    ///
    /// `SendMessagePayload::images` is `Option<Vec<_>>` and is
    /// `skip_serializing_if = "Option::is_none"`, so `None` and `Some(vec![])` are
    /// *different bytes on the wire*. A resend that got that mapping wrong would
    /// not be a resend of the same message — and the agent-targeted path was
    /// hardcoding `images: None`, silently dropping every attachment the record
    /// was holding. One helper, used by every send, so the two cannot drift.
    pub fn wire_images(&self) -> Option<Vec<protocol::ImageData>> {
        wire_images(&self.images)
    }

    /// Whether the composer could actually hold this submission if the user asked
    /// for it back.
    ///
    /// A tool response cannot be expressed as chat text — it is a typed decision —
    /// so there is nothing meaningful to hand back, and handing back its (empty)
    /// text would just look broken. Recovery for those runs through **Send again**
    /// or **Discard**.
    pub fn is_editable_in_composer(&self) -> bool {
        self.tool_response.is_none()
    }

    /// What to show the user for this record.
    ///
    /// A plan decision carries no message text — the payload *is* the decision —
    /// so rendering `text` verbatim would show an empty box and tell the user
    /// nothing about what they are being asked to recover.
    pub fn display_text(&self) -> String {
        if !self.text.trim().is_empty() {
            return self.text.clone();
        }
        match &self.tool_response {
            Some(protocol::SendMessageToolResponse::ExitPlanMode {
                decision, feedback, ..
            }) => {
                let decision = match decision {
                    protocol::ExitPlanModeDecision::Approve => "Plan approved",
                    protocol::ExitPlanModeDecision::Reject => "Plan rejected",
                };
                match feedback {
                    Some(feedback) if !feedback.trim().is_empty() => {
                        format!("{decision} — {feedback}")
                    }
                    _ => decision.to_owned(),
                }
            }
            None => String::new(),
        }
    }
    /// A resend is only ever offered on a genuinely new connection: the one that
    /// admitted this frame is gone, and the client never replays intent across
    /// connections on its own.
    pub fn can_resend_on(&self, current_connection_instance_id: Option<u64>) -> bool {
        match current_connection_instance_id {
            Some(current) => current != self.connection_instance_id,
            None => false,
        }
    }

    /// One-line label for the record's transport state. Deliberately never says
    /// "delivered" or "sent": the client cannot know either.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            PendingSubmissionState::QueuedLocally => "Waiting to send",
            PendingSubmissionState::NotSent => "Not sent",
            PendingSubmissionState::DeliveryUnknown => "May not have been received",
        }
    }

    /// What the client actually knows, in the user's terms.
    pub fn state_detail(&self) -> &'static str {
        match self.state {
            PendingSubmissionState::QueuedLocally => "This message is still queued to go out.",
            PendingSubmissionState::NotSent => {
                "The connection dropped before this message went out, so it definitely never \
                 reached the host. Sending it again is safe."
            }
            PendingSubmissionState::DeliveryUnknown => {
                "The connection dropped while this message was going out. It may or may not have \
                 reached the host — Tyde cannot tell which."
            }
        }
    }
}

/// Shape held images for `SendMessagePayload::images`.
///
/// The single definition of that mapping. `None` and `Some(vec![])` serialize
/// differently, so a resend and a first send must both go through this or they
/// are not sending the same message.
pub fn wire_images(images: &[protocol::ImageData]) -> Option<Vec<protocol::ImageData>> {
    (!images.is_empty()).then(|| images.to_vec())
}

/// Cap on recovery records held for one host.
///
/// It is a **hard gate, checked before the frame reaches the transport**
/// ([`AppState::can_hold_submission_untracked`]). At the cap the *new* send is
/// refused while its text is still safely in the composer — because once a frame
/// is admitted it cannot be un-sent, so a cap enforced afterwards could only make
/// room by destroying a record.
///
/// **No record is ever evicted**, including a `QueuedLocally` one: an in-flight
/// submission is *unresolved*, not safe — it can still come back `NotSent` or
/// `DeliveryUnknown`, and its record is the only holder of the text the user would
/// then need back. A record leaves the store by exactly three routes:
/// `BrokerAcknowledged` retires it, the user withdraws it, or the host is
/// forgotten. Going transiently one over the cap (a resend holds its replacement
/// before retiring the attempt it supersedes) is strictly better than losing a
/// message, so [`AppState::hold_submission`] inserts unconditionally.
///
/// Reaching the cap therefore means this many submissions are unresolved at once,
/// which is already a broken-connection scenario.
pub const MAX_PENDING_SUBMISSIONS_PER_HOST: usize = 64;

// ── App mode + pairing screens ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum PairingScreen {
    Scanner,
    ManualPaste,
    /// Direct-pairing confirmation (legacy v1 native-shell path). The QR already
    /// carries the room + PSK, so tapping Pair stores the credential and starts
    /// the encrypted MQTT connection immediately (no `tycode.dev` round-trip).
    Confirm {
        qr_uri: String,
        preview: MobilePairingPreview,
    },
    /// Runs the direct-pairing MQTT connect for [`PairingScreen::Confirm`].
    InProgress {
        qr_uri: String,
        preview: MobilePairingPreview,
    },
    /// A managed (`tyde-pair://v2`) offer: authenticate with `tycode.dev`
    /// (Tyggs OAuth → pass proof → mobile session) and, once authenticated,
    /// redeem the offer and connect to the managed broker. `auth` is the typed
    /// server-owned auth state driving which card renders (spinner / paywall /
    /// retry / redeeming).
    ServiceAuth {
        qr_uri: String,
        host_label: String,
        auth: MobileServiceAuthState,
    },
    /// A boot OAuth callback completed after Safari lost the pending QR. The
    /// typed auth result remains renderable (paywall/retry/sign-in) without
    /// inventing pairing data or attempting redemption.
    ServiceAuthStatus {
        auth: MobileServiceAuthState,
    },
    /// A legacy public-broker QR or stored record that fails closed: the user
    /// must re-pair from the host's current QR. Never a spinner, never a silent
    /// public-broker connect.
    RepairRequired {
        message: String,
    },
    Failed {
        message: String,
    },
}

/// Classification of a scanned/pasted pairing URI, produced by
/// [`crate::bridge::classify_pairing_offer`] via
/// `mqtt_transport::parse_mobile_pairing_qr_offer`. The pairing flow renders a
/// different screen per variant, so the UI branches on typed offer data rather
/// than on which bridge backend is active.
#[derive(Clone, Debug, PartialEq)]
pub enum PairingOffer {
    /// A `tycode.dev` managed-service offer (`tyde-pair://v2`). Requires the
    /// pre-transport Tyggs auth + redeem sequence before connecting.
    ManagedService { host_label: String },
    /// A legacy public-broker QR (`tyde-pair://v1`). Cannot connect; the user
    /// must re-pair from the host's updated QR.
    RepairRequired { message: String },
    /// Native-shell direct pairing: the existing preview → confirm →
    /// `start_pairing` path. The native shell owns its own managed handshake, so
    /// the web bundle never produces this variant.
    DirectPairing { preview: MobilePairingPreview },
}

#[derive(Clone, Debug, PartialEq)]
pub enum AppMode {
    /// User has zero paired hosts.
    Onboarding,
    /// User has at least one paired host but is not currently in the pairing
    /// flow. The picker / per-host workspace renders here.
    Workspace,
    /// Pairing flow is on-screen.
    Pairing(PairingScreen),
}

// ── Refs ───────────────────────────────────────────────────────────────

/// Composite key for per-agent state (chat, tasks, streaming, etc).
/// Combines `LocalHostId` with `AgentId` so two paired hosts that happen to
/// generate colliding agent identifiers stay isolated.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentRef {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ActiveProjectRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectFileRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
    pub path: ProjectPath,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectFileState {
    pub path: ProjectPath,
    pub contents: Option<String>,
    pub is_binary: bool,
}

impl From<ProjectFileContentsPayload> for ProjectFileState {
    fn from(payload: ProjectFileContentsPayload) -> Self {
        Self {
            path: payload.path,
            contents: payload.contents,
            is_binary: payload.is_binary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectDiffRef {
    pub local_host_id: LocalHostId,
    pub project_id: ProjectId,
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectDiffState {
    pub root: ProjectRootPath,
    pub scope: ProjectDiffScope,
    pub path: Option<String>,
    pub context_mode: DiffContextMode,
    pub pending: bool,
    pub files: Vec<ProjectGitDiffFile>,
}

impl ProjectDiffState {
    pub fn for_request(
        previous: Option<&ProjectDiffState>,
        root: ProjectRootPath,
        scope: ProjectDiffScope,
        path: Option<String>,
        context_mode: DiffContextMode,
    ) -> Self {
        let files = previous
            .filter(|existing| existing.context_mode == context_mode)
            .map(|existing| existing.files.clone())
            .unwrap_or_default();
        Self {
            root,
            scope,
            path,
            context_mode,
            pending: true,
            files,
        }
    }
}

pub fn reduce_project_diff_response(
    current: Option<&ProjectDiffState>,
    payload: protocol::ProjectGitDiffPayload,
) -> Option<ProjectDiffState> {
    if current.is_some_and(|state| state.pending && state.context_mode != payload.context_mode) {
        return None;
    }
    Some(ProjectDiffState {
        root: payload.root,
        scope: payload.scope,
        path: payload.path,
        context_mode: payload.context_mode,
        pending: false,
        files: payload.files,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReviewRef {
    pub local_host_id: LocalHostId,
    pub review_id: ReviewId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAgentRef {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
}

impl ActiveAgentRef {
    pub fn as_agent_ref(&self) -> AgentRef {
        AgentRef {
            local_host_id: self.local_host_id.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

// ── Agent info ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub local_host_id: LocalHostId,
    pub agent_id: AgentId,
    pub name: String,
    pub origin: AgentOrigin,
    pub backend_kind: BackendKind,
    pub workspace_roots: Vec<String>,
    pub project_id: Option<ProjectId>,
    pub parent_agent_id: Option<AgentId>,
    pub session_id: Option<SessionId>,
    pub custom_agent_id: Option<CustomAgentId>,
    pub created_at_ms: u64,
    pub instance_stream: StreamPath,
    pub started: bool,
    pub fatal_error: Option<String>,
}

impl AgentInfo {
    pub fn agent_ref(&self) -> AgentRef {
        AgentRef {
            local_host_id: self.local_host_id.clone(),
            agent_id: self.agent_id.clone(),
        }
    }
}

// ── Chat types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ChatMessageEntry {
    pub message: ChatMessage,
    pub tool_requests: Vec<ToolRequestEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHistoryState {
    pub message_count: u32,
    pub oldest_seq: Option<u64>,
    pub has_more_before: bool,
    pub loading: bool,
}

#[derive(Clone, Debug)]
pub struct ToolRequestEntry {
    pub request: ToolRequest,
    pub result: Option<ToolExecutionCompletedData>,
}

#[derive(Clone, Debug)]
pub struct StreamingState {
    pub agent_name: String,
    pub model: Option<String>,
    pub text: ArcRwSignal<String>,
    pub reasoning: ArcRwSignal<String>,
    pub tool_requests: ArcRwSignal<Vec<ToolRequestEntry>>,
}

#[derive(Clone, Debug)]
pub enum TransientEvent {
    OperationCancelled {
        message: String,
    },
    RetryAttempt {
        attempt: u64,
        max_retries: u64,
        error: String,
        backoff_ms: u64,
    },
}

// ── Project/session info ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectInfo {
    pub local_host_id: LocalHostId,
    pub project: Project,
}

/// Orders the project list the way the server's `ordered_projects` emits
/// it: hosts first, then each host's top-level projects by `sort_order`,
/// with every parent's git-workbench children listed directly beneath it
/// (children ordered by their own per-parent `sort_order`). Workbench
/// children carry an independent `sort_order` sequence starting at 0, so
/// a flat sort by raw `sort_order` would interleave them among top-level
/// projects. Updates arrive as single-project upserts
/// (`ProjectNotify::Upsert`, project bootstraps) as well as full
/// snapshots, so the grouped order is re-derived locally instead of
/// trusting arrival order. A workbench whose parent hasn't arrived yet
/// sorts after every known top-level project (grouped with any orphan
/// siblings of the same parent) until the parent's upsert lands and the
/// next re-sort slots it into place.
pub fn sort_project_infos(projects: &mut [ProjectInfo]) {
    // (top-level sort_order, top-level name, top-level id) for parent
    // lookup, keyed per host so colliding ids across hosts stay isolated.
    let top_level: HashMap<(LocalHostId, ProjectId), (u64, String)> = projects
        .iter()
        .filter(|info| !info.project.is_workbench())
        .map(|info| {
            (
                (info.local_host_id.clone(), info.project.id.clone()),
                (info.project.sort_order, info.project.name.clone()),
            )
        })
        .collect();

    projects.sort_by_cached_key(|info| {
        let host = info.local_host_id.0.clone();
        let own = (
            info.project.sort_order,
            info.project.name.clone(),
            info.project.id.0.clone(),
        );
        match info.project.parent_project_id() {
            None => {
                let (order, name, id) = own;
                // Top-level rows sort by their own key and come before
                // their children (`is_child = 0`).
                (
                    host,
                    order,
                    name,
                    id,
                    0u8,
                    0u64,
                    String::new(),
                    String::new(),
                )
            }
            Some(parent_id) => {
                let key = (info.local_host_id.clone(), parent_id.clone());
                let (parent_order, parent_name) = top_level
                    .get(&key)
                    .cloned()
                    // Orphan workbench: parent not (yet) in the list.
                    // Push it after all known top-level groups.
                    .unwrap_or((u64::MAX, String::new()));
                let (order, name, id) = own;
                (
                    host,
                    parent_order,
                    parent_name,
                    parent_id.0.clone(),
                    1u8,
                    order,
                    name,
                    id,
                )
            }
        }
    });
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub local_host_id: LocalHostId,
    pub summary: SessionSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListLoadState {
    pub page: SessionListPageInfo,
    pub loaded_count: u32,
    pub loading_more: bool,
}

impl SessionListLoadState {
    /// Build a load state from a freshly-applied server page. `loading_more`
    /// tracks an **explicit** in-flight "load more" request, so it starts
    /// `false` — the mobile client no longer auto-drains remaining pages, it
    /// waits for the user to ask (see [`crate::dispatch::load_next_session_page`]).
    pub fn from_page(page: SessionListPageInfo, loaded_count: usize) -> Self {
        let loaded_count = u32::try_from(loaded_count).unwrap_or(u32::MAX);
        Self {
            page,
            loaded_count,
            loading_more: false,
        }
    }

    pub fn next_cursor(&self) -> Option<SessionListCursor> {
        self.page.next_cursor()
    }

    /// Whether the server reports additional pages beyond what's loaded.
    pub fn has_more(&self) -> bool {
        matches!(self.page.status, SessionListPageStatus::More { .. })
    }
}

#[derive(Clone, Debug)]
pub struct HostBrowseSession {
    pub local_host_id: LocalHostId,
    pub stream: StreamPath,
    pub opened: Option<HostBrowseOpenedPayload>,
    pub entries_by_path: HashMap<HostAbsPath, HostBrowseEntriesPayload>,
    pub latest_error: Option<HostBrowseErrorPayload>,
}

// ── Navigation ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Copy)]
pub enum MobileTab {
    Home,
    Agents,
    Sessions,
    Projects,
    Settings,
}

// ── App state ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    // Top-level routing
    pub app_mode: RwSignal<AppMode>,
    pub active_local_host_id: RwSignal<Option<LocalHostId>>,

    // Multi-host
    pub paired_hosts: RwSignal<Vec<PairedHostSummary>>,
    pub connection_statuses: RwSignal<HashMap<LocalHostId, ConnectionStatus>>,
    /// Tracks the `connection_instance_id` of the MQTT connection for which
    /// the frontend last sent Hello.  Used to detect same-connection status
    /// replays (no re-Hello needed) vs. genuinely new connections.
    pub active_connection_instance_ids: RwSignal<HashMap<LocalHostId, u64>>,
    pub host_streams: RwSignal<HashMap<LocalHostId, StreamPath>>,
    /// Host stream whose `HostBootstrap` has actually been applied. This is
    /// distinct from cached host settings so a fresh stream cannot be marked
    /// ready by stale state from an older connection.
    pub bootstrapped_host_streams: RwSignal<HashMap<LocalHostId, StreamPath>>,
    pub heartbeat_pending_since_by_host: RwSignal<HashMap<LocalHostId, u64>>,
    pub heartbeat_round_trip_ms_by_host: RwSignal<HashMap<LocalHostId, u64>>,
    pub host_settings_by_host: RwSignal<HashMap<LocalHostId, HostSettings>>,
    pub command_errors_by_host: RwSignal<HashMap<LocalHostId, String>>,
    pub backend_setup_by_host: RwSignal<HashMap<LocalHostId, Vec<BackendSetupInfo>>>,
    /// Server-owned subscription-capacity snapshots, keyed by host then backend.
    /// The same `BackendCapacity` event desktop consumes: replayed on subscribe,
    /// re-emitted on change. Mobile renders it verbatim — no mobile-only
    /// capacity model, no mobile-only freshness maths (staleness is the server's
    /// `CapacityFreshness` verdict), and no inference from token usage.
    pub backend_capacity_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<BackendKind, protocol::BackendCapacitySnapshot>>>,
    pub session_schemas_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<BackendKind, SessionSchemaEntry>>>,
    pub custom_agents_by_host: RwSignal<HashMap<LocalHostId, HashMap<CustomAgentId, CustomAgent>>>,
    pub mcp_servers_by_host: RwSignal<HashMap<LocalHostId, HashMap<McpServerId, McpServerConfig>>>,
    pub steering_by_host: RwSignal<HashMap<LocalHostId, HashMap<SteeringId, Steering>>>,
    pub skills_by_host: RwSignal<HashMap<LocalHostId, HashMap<SkillId, Skill>>>,

    // Mobile shell error notification
    pub mobile_shell_error: RwSignal<Option<MobileShellError>>,

    // Tab navigation within the per-host workspace
    pub active_tab: RwSignal<MobileTab>,
    pub viewing_chat: RwSignal<bool>,

    // Projects
    pub projects: RwSignal<Vec<ProjectInfo>>,
    pub active_project: RwSignal<Option<ActiveProjectRef>>,
    pub file_tree: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ProjectRootListing>>>,
    pub git_status: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ProjectRootGitStatus>>>,
    pub project_file_contents: RwSignal<HashMap<ProjectFileRef, ProjectFileState>>,
    pub project_diffs: RwSignal<HashMap<ProjectDiffRef, ProjectDiffState>>,
    pub review_summaries: RwSignal<HashMap<(LocalHostId, ProjectId), Vec<ReviewSummary>>>,
    pub reviews: RwSignal<HashMap<ReviewRef, Review>>,
    pub review_errors: RwSignal<HashMap<ReviewRef, ReviewErrorPayload>>,
    pub review_streams: RwSignal<HashMap<ReviewRef, StreamPath>>,

    // Agents & Chat
    pub agents: RwSignal<Vec<AgentInfo>>,
    pub active_agent: RwSignal<Option<ActiveAgentRef>>,
    /// Agents whose `AgentBootstrap` has been requested or received on this
    /// frontend connection. Mobile asks for bootstraps lazily when a chat is
    /// opened instead of replaying every transcript on startup.
    pub agent_load_requests: RwSignal<HashSet<AgentRef>>,
    /// Agents whose `AgentBootstrap` snapshot has actually arrived. Distinct
    /// from `agent_load_requests`, which latches as soon as a load is sent —
    /// this only flips once the transcript snapshot lands, so a chat opened on
    /// a slow link can show a loading spinner instead of a premature "empty"
    /// state in the window between the request and its bootstrap reply.
    pub agent_loaded: RwSignal<HashSet<AgentRef>>,
    /// Agents whose load failed, with the typed reason. This is the spinner's
    /// **terminal** state: `agent_loaded` is only ever written by a successful
    /// `AgentBootstrap`, so a spinner gated on it alone spins forever on any
    /// failure. Written on a local admission/enqueue failure and on a server
    /// `CommandError(LoadAgent)`; cleared by a successful bootstrap and by a
    /// disconnect (the next connection re-attempts the load on a fresh stream).
    ///
    /// The load latch in `agent_load_requests` stays set alongside it: a retried
    /// `LoadAgent` on an already-attached stream is a server-side conflict, so
    /// recovery is a deliberate reconnect, never an in-place retry.
    pub agent_load_errors: RwSignal<HashMap<AgentRef, String>>,
    pub chat_messages: RwSignal<HashMap<AgentRef, Vec<ChatMessageEntry>>>,
    /// Per-agent index from server-issued `ChatMessageId` to the position
    /// in `chat_messages[agent]` that carries it. Populated when a row is
    /// pushed (live `MessageAdded`/`StreamEnd`, replayed bootstrap events)
    /// if the message's `message_id` is `Some`, and read when a
    /// `MessageMetadataUpdated` event lands so the existing row can be
    /// patched in place. Cleared anywhere `chat_messages` is cleared
    /// (host runtime reset, agent close, agent bootstrap snapshot).
    pub chat_message_index: RwSignal<HashMap<AgentRef, HashMap<ChatMessageId, usize>>>,
    /// Server-owned prior-history availability for each agent. The server
    /// sends only this indicator in `AgentBootstrap`; actual prior transcript
    /// rows are fetched explicitly with `FetchSessionHistory` and prepended
    /// when `SessionHistory` arrives.
    pub session_history: RwSignal<HashMap<AgentRef, SessionHistoryState>>,
    pub streaming_text: RwSignal<HashMap<AgentRef, StreamingState>>,
    pub chat_input: RwSignal<String>,
    /// Outbound submissions the client is holding on the user's behalf, keyed by
    /// the connection layer's [`LocalSubmissionId`] — one entry per *transport
    /// attempt*. Bounded, in-memory, and non-persistent. Records are held silently
    /// while `QueuedLocally` and are surfaced only on a transport failure;
    /// `BrokerAcknowledged` removes them.
    pub pending_submissions: RwSignal<HashMap<LocalSubmissionId, PendingSubmission>>,
    /// Logical submissions the user took back, so the UI that created one can
    /// still say what became of it after its record is gone.
    ///
    /// Without this, "no record" is ambiguous — acknowledged, or discarded? — and
    /// a tool card whose reply the user *threw away* would quietly revert to
    /// claiming it was "queued locally".
    ///
    /// **These are tombstones, and they are never evicted by count.** A tool card
    /// lives in the transcript for as long as the conversation is open: scrolling
    /// past it does not unmount it, and it re-reads its lifecycle reactively
    /// forever. An LRU cap therefore does not drop "old, unreachable" facts — it
    /// drops facts belonging to UI that is still on screen, and the card
    /// immediately resumes telling the user that a message they discarded is on its
    /// way to the agent. That is the exact lie the tombstone exists to prevent, so
    /// there is no count at which discarding one is acceptable.
    ///
    /// They are dropped at the one point where every UI that could consult them is
    /// provably gone: [`AppState::clear_host_runtime`], which unmounts that host's
    /// entire workspace. Otherwise they live for the page lifetime.
    ///
    /// The growth is fine, and it is worth being explicit about why: an entry is
    /// added **only** by an explicit destructive tap (Discard, or Edit pulling text
    /// back), it is a host id plus an integer plus a two-variant enum, and it holds
    /// **no user text**. This is a log of the user's own deliberate actions, not a
    /// cache of server state. A leak would need a bug, and
    /// [`IMPLAUSIBLE_WITHDRAWAL_COUNT`] makes one loud rather than silently eating
    /// the truth.
    pub withdrawn_submissions: RwSignal<HashMap<SubmissionOriginId, WithdrawnSubmission>>,
    /// Monotonic source of [`SubmissionOriginId`]s. Client-local; never on the wire.
    next_submission_origin: RwSignal<u64>,
    pub task_lists: RwSignal<HashMap<AgentRef, TaskList>>,
    pub agent_message_queue: RwSignal<HashMap<AgentRef, Vec<QueuedMessageEntry>>>,
    pub agent_turn_active: RwSignal<HashMap<AgentRef, bool>>,
    pub transient_events: RwSignal<HashMap<AgentRef, Vec<TransientEvent>>>,
    pub agent_session_settings: RwSignal<HashMap<AgentRef, SessionSettingsValues>>,
    pub agent_compactions: RwSignal<HashMap<AgentRef, AgentCompactNotifyPayload>>,

    // Teams
    pub teams_by_host: RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, Team>>>,
    pub team_members_by_host: RwSignal<HashMap<LocalHostId, HashMap<TeamMemberId, TeamMember>>>,
    pub team_bindings_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<TeamMemberId, TeamMemberBindingPayload>>>,
    pub team_compactions_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, TeamCompactNotifyPayload>>>,
    pub team_preset_catalog_by_host: RwSignal<HashMap<LocalHostId, TeamPresetCatalog>>,
    pub team_drafts_by_host: RwSignal<HashMap<LocalHostId, HashMap<TeamDraftId, TeamDraft>>>,
    pub team_shuffle_suggestions_by_host:
        RwSignal<HashMap<LocalHostId, HashMap<protocol::TeamId, TeamMemberShuffleSuggestion>>>,

    // Host filesystem browsing
    pub host_browses: RwSignal<HashMap<(LocalHostId, StreamPath), HostBrowseSession>>,

    // Sessions
    pub sessions: RwSignal<Vec<SessionInfo>>,
    pub session_lists_by_host: RwSignal<HashMap<LocalHostId, SessionListLoadState>>,

    // Draft state for new agent
    pub draft_backend_override: RwSignal<Option<BackendKind>>,
    pub draft_custom_agent_id: RwSignal<Option<CustomAgentId>>,
    pub draft_session_settings: RwSignal<SessionSettingsValues>,

    // Appearance
    pub theme: RwSignal<String>,
    pub tool_output_mode: RwSignal<ToolOutputMode>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            app_mode: RwSignal::new(AppMode::Onboarding),
            active_local_host_id: RwSignal::new(None),

            paired_hosts: RwSignal::new(Vec::new()),
            connection_statuses: RwSignal::new(HashMap::new()),
            active_connection_instance_ids: RwSignal::new(HashMap::new()),
            host_streams: RwSignal::new(HashMap::new()),
            bootstrapped_host_streams: RwSignal::new(HashMap::new()),
            heartbeat_pending_since_by_host: RwSignal::new(HashMap::new()),
            heartbeat_round_trip_ms_by_host: RwSignal::new(HashMap::new()),
            host_settings_by_host: RwSignal::new(HashMap::new()),
            command_errors_by_host: RwSignal::new(HashMap::new()),
            backend_setup_by_host: RwSignal::new(HashMap::new()),
            backend_capacity_by_host: RwSignal::new(HashMap::new()),
            session_schemas_by_host: RwSignal::new(HashMap::new()),
            custom_agents_by_host: RwSignal::new(HashMap::new()),
            mcp_servers_by_host: RwSignal::new(HashMap::new()),
            steering_by_host: RwSignal::new(HashMap::new()),
            skills_by_host: RwSignal::new(HashMap::new()),

            mobile_shell_error: RwSignal::new(None),

            active_tab: RwSignal::new(MobileTab::Home),
            viewing_chat: RwSignal::new(false),

            projects: RwSignal::new(Vec::new()),
            active_project: RwSignal::new(None),
            file_tree: RwSignal::new(HashMap::new()),
            git_status: RwSignal::new(HashMap::new()),
            project_file_contents: RwSignal::new(HashMap::new()),
            project_diffs: RwSignal::new(HashMap::new()),
            review_summaries: RwSignal::new(HashMap::new()),
            reviews: RwSignal::new(HashMap::new()),
            review_errors: RwSignal::new(HashMap::new()),
            review_streams: RwSignal::new(HashMap::new()),

            agents: RwSignal::new(Vec::new()),
            active_agent: RwSignal::new(None),
            agent_load_requests: RwSignal::new(HashSet::new()),
            agent_loaded: RwSignal::new(HashSet::new()),
            agent_load_errors: RwSignal::new(HashMap::new()),
            chat_messages: RwSignal::new(HashMap::new()),
            chat_message_index: RwSignal::new(HashMap::new()),
            session_history: RwSignal::new(HashMap::new()),
            streaming_text: RwSignal::new(HashMap::new()),
            chat_input: RwSignal::new(String::new()),
            pending_submissions: RwSignal::new(HashMap::new()),
            withdrawn_submissions: RwSignal::new(HashMap::new()),
            next_submission_origin: RwSignal::new(0),
            task_lists: RwSignal::new(HashMap::new()),
            agent_message_queue: RwSignal::new(HashMap::new()),
            agent_turn_active: RwSignal::new(HashMap::new()),
            transient_events: RwSignal::new(HashMap::new()),
            agent_session_settings: RwSignal::new(HashMap::new()),
            agent_compactions: RwSignal::new(HashMap::new()),

            teams_by_host: RwSignal::new(HashMap::new()),
            team_members_by_host: RwSignal::new(HashMap::new()),
            team_bindings_by_host: RwSignal::new(HashMap::new()),
            team_compactions_by_host: RwSignal::new(HashMap::new()),
            team_preset_catalog_by_host: RwSignal::new(HashMap::new()),
            team_drafts_by_host: RwSignal::new(HashMap::new()),
            team_shuffle_suggestions_by_host: RwSignal::new(HashMap::new()),

            host_browses: RwSignal::new(HashMap::new()),

            sessions: RwSignal::new(Vec::new()),
            session_lists_by_host: RwSignal::new(HashMap::new()),

            draft_backend_override: RwSignal::new(None),
            draft_custom_agent_id: RwSignal::new(None),
            draft_session_settings: RwSignal::new(SessionSettingsValues::default()),

            theme: RwSignal::new("dark".to_owned()),
            tool_output_mode: RwSignal::new(ToolOutputMode::Compact),
        }
    }

    pub fn host_stream_untracked(&self, host: &LocalHostId) -> Option<StreamPath> {
        self.host_streams.get_untracked().get(host).cloned()
    }

    pub fn host_bootstrap_applied_for_current_stream(&self, host: &LocalHostId) -> bool {
        let current = self.host_streams.get().get(host).cloned();
        let bootstrapped = self.bootstrapped_host_streams.get().get(host).cloned();
        matches!((current, bootstrapped), (Some(current), Some(bootstrapped)) if current == bootstrapped)
    }

    pub fn host_bootstrap_applied_for_current_stream_untracked(&self, host: &LocalHostId) -> bool {
        let current = self.host_streams.get_untracked().get(host).cloned();
        let bootstrapped = self
            .bootstrapped_host_streams
            .get_untracked()
            .get(host)
            .cloned();
        matches!((current, bootstrapped), (Some(current), Some(bootstrapped)) if current == bootstrapped)
    }

    /// Append a chat row for `agent_ref` and, if the message carries a
    /// server-issued `message_id`, register it in `chat_message_index` so
    /// a later `ChatEvent::MessageMetadataUpdated` can patch the row in
    /// place. The two writes are performed under separate signal updates
    /// because they target separate signals — there is no torn-state
    /// window for any single observer (each signal is internally
    /// consistent), and consumers only ever read the index after they
    /// can see the row that produced it.
    /// Drop server-provided prior-history state for a single agent. Call
    /// wherever `chat_messages` is cleared for that agent so a re-bootstrap
    /// starts from the server's new authoritative indicator.
    pub fn forget_session_history(&self, agent_ref: &AgentRef) {
        self.session_history.update(|map| {
            map.remove(agent_ref);
        });
    }

    pub fn push_chat_message_entry(&self, agent_ref: &AgentRef, entry: ChatMessageEntry) {
        let message_id = entry.message.message_id.clone();
        self.chat_messages.update(|messages| {
            messages.entry(agent_ref.clone()).or_default().push(entry);
        });
        if let Some(message_id) = message_id {
            let position = self
                .chat_messages
                .with_untracked(|m| m.get(agent_ref).map(|v| v.len().saturating_sub(1)));
            if let Some(position) = position {
                self.chat_message_index.update(|indexes| {
                    indexes
                        .entry(agent_ref.clone())
                        .or_default()
                        .entry(message_id)
                        .or_insert(position);
                });
            }
        }
    }

    /// Patch the row matching `update.message_id` with whichever of
    /// `model_info` / `token_usage` / `context_breakdown` are `Some`.
    /// Same semantics as the desktop `apply_chat_message_metadata`.
    pub fn apply_chat_message_metadata(
        &self,
        agent_ref: &AgentRef,
        update: MessageMetadataUpdateData,
    ) {
        let position = self.chat_message_index.with_untracked(|indexes| {
            indexes
                .get(agent_ref)
                .and_then(|index| index.get(&update.message_id).copied())
        });
        let Some(position) = position else {
            log::warn!(
                "chat_event message_metadata_updated unknown message_id host={} agent_id={} message_id={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                update.message_id
            );
            return;
        };
        let MessageMetadataUpdateData {
            message_id,
            model_info,
            token_usage,
            context_breakdown,
        } = update;
        let mut patched = false;
        self.chat_messages.update(|messages| {
            if let Some(agent_messages) = messages.get_mut(agent_ref)
                && let Some(entry) = agent_messages.get_mut(position)
                && entry.message.message_id.as_ref() == Some(&message_id)
            {
                if let Some(model_info) = model_info {
                    entry.message.model_info = Some(model_info);
                }
                if let Some(token_usage) = token_usage {
                    entry.message.token_usage = Some(token_usage);
                }
                if let Some(context_breakdown) = context_breakdown {
                    entry.message.context_breakdown = Some(context_breakdown);
                }
                patched = true;
            }
        });
        if !patched {
            log::warn!(
                "chat_event message_metadata_updated row missing after lookup host={} agent_id={} message_id={} position={}",
                agent_ref.local_host_id,
                agent_ref.agent_id,
                message_id,
                position
            );
        }
    }

    // ── Outbound submission recovery ───────────────────────────────────

    /// How many submissions are currently held for `host`.
    pub fn held_submission_count_untracked(&self, host: &LocalHostId) -> usize {
        self.pending_submissions.with_untracked(|records| {
            records
                .values()
                .filter(|record| record.local_host_id == *host)
                .count()
        })
    }

    /// Whether another submission can be taken into custody for `host`.
    ///
    /// This is a **hard gate, checked before the frame reaches the transport**.
    /// Once a frame is admitted the client is committed — it cannot be un-sent —
    /// so a cap enforced *after* admission has only one way to make room:
    /// destroying a record. That is the exact data loss this model exists to
    /// prevent. The refusal therefore has to happen while the text is still
    /// safely in the composer.
    ///
    /// **No record is ever evicted**, including a `QueuedLocally` one. An
    /// in-flight submission is *unresolved*, not safe: it can still come back
    /// `NotSent` or `DeliveryUnknown`, and its record is the only holder of the
    /// text the user would then need back.
    pub fn can_hold_submission_untracked(&self, host: &LocalHostId) -> bool {
        self.held_submission_count_untracked(host) < MAX_PENDING_SUBMISSIONS_PER_HOST
    }

    /// Take custody of an admitted submission.
    ///
    /// Never evicts. The cap is enforced by [`Self::can_hold_submission_untracked`]
    /// *before* admission; by the time we are here the frame is already gone and
    /// refusing to hold its text would simply lose it.
    ///
    /// A record leaves this store by exactly three routes: `BrokerAcknowledged`
    /// retires it, the user discards it, or the host is forgotten.
    pub fn hold_submission(&self, submission: PendingSubmission) {
        self.pending_submissions.update(|records| {
            records.insert(submission.local_submission_id, submission);
        });
    }

    /// Apply a transport fact reported by the connection layer for one
    /// submission.
    ///
    /// `BrokerAcknowledged` **retires the record silently** — no dismissal, no
    /// accumulation, no announcement. It is a transport fact only: it does not
    /// claim the host received or applied the frame, and the record is not kept
    /// around pretending otherwise.
    ///
    /// An outcome from a connection instance other than the one that admitted
    /// the frame is ignored: a dead connection cannot speak for a live one's
    /// submissions.
    ///
    /// **No outcome ever writes the composer.** `NotSent` is provably safe to
    /// resend, and it is still not injected: the composer belongs to the user,
    /// the only control that moves text back into it is the explicit **Edit**
    /// button, and a record carries images the composer cannot hold — so an
    /// automatic restore would quietly drop them.
    pub fn apply_submission_outcome(
        &self,
        local_submission_id: LocalSubmissionId,
        connection_instance_id: u64,
        outcome: SubmissionTransportOutcome,
    ) {
        self.pending_submissions.update(|records| {
            let Some(record) = records.get_mut(&local_submission_id) else {
                // Already discarded by the user, or retired. Not an error.
                return;
            };
            if record.connection_instance_id != connection_instance_id {
                log::warn!(
                    "submission outcome for foreign connection instance \
                     local_submission_id={} record_instance={} \
                     outcome_instance={connection_instance_id}",
                    local_submission_id.0,
                    record.connection_instance_id
                );
                return;
            }
            match outcome {
                SubmissionTransportOutcome::QueuedLocally => {
                    record.state = PendingSubmissionState::QueuedLocally;
                }
                SubmissionTransportOutcome::NotSent => {
                    record.state = PendingSubmissionState::NotSent;
                }
                SubmissionTransportOutcome::DeliveryUnknown => {
                    record.state = PendingSubmissionState::DeliveryUnknown;
                }
                SubmissionTransportOutcome::BrokerAcknowledged => {
                    records.remove(&local_submission_id);
                }
            }
        });
    }

    /// Mint the next logical-submission identity.
    pub fn mint_submission_origin(&self) -> SubmissionOriginId {
        let next = self.next_submission_origin.get_untracked();
        self.next_submission_origin.set(next + 1);
        SubmissionOriginId(next)
    }

    /// Retire one transport *attempt* without saying anything about the logical
    /// submission it belonged to.
    ///
    /// Used when a deliberate resend supersedes an attempt: the message is still
    /// very much in play — it just has a new record now, carrying the same
    /// `origin`. Marking the lineage terminal here would make the originating card
    /// announce that the user withdrew a message they had in fact just re-sent.
    pub fn retire_submission_attempt(&self, local_submission_id: LocalSubmissionId) {
        self.pending_submissions.update(|records| {
            records.remove(&local_submission_id);
        });
    }

    /// The user took a submission back — discarded it, or pulled it into the
    /// composer. The only path that destroys held text.
    ///
    /// Leaves a **tombstone** against the *lineage*, so the UI that created the
    /// submission can say what happened instead of falling back on "queued
    /// locally" — which, after a discard, is simply false.
    ///
    /// The tombstone is never evicted by count: see [`Self::withdrawn_submissions`].
    pub fn withdraw_submission(
        &self,
        local_submission_id: LocalSubmissionId,
        withdrawal: SubmissionWithdrawal,
    ) {
        let withdrawn = self.pending_submissions.with_untracked(|records| {
            records.get(&local_submission_id).map(|record| {
                (
                    record.origin,
                    WithdrawnSubmission {
                        local_host_id: record.local_host_id.clone(),
                        withdrawal,
                    },
                )
            })
        });
        self.pending_submissions.update(|records| {
            records.remove(&local_submission_id);
        });
        let Some((origin, tombstone)) = withdrawn else {
            return;
        };
        self.withdrawn_submissions.update(|map| {
            map.insert(origin, tombstone);
            if map.len() == IMPLAUSIBLE_WITHDRAWAL_COUNT {
                // Loud, and still lossless. Evicting here would mean a tool card
                // still sitting in the transcript quietly going back to claiming a
                // discarded message is on its way.
                log::error!(
                    "withdrawn_submissions reached {IMPLAUSIBLE_WITHDRAWAL_COUNT} entries — \
                     that is far more explicit discards than a person makes, so something \
                     is minting withdrawals in a loop. Not evicting: a dropped tombstone \
                     makes a rendered card lie."
                );
            }
        });
    }

    /// What became of a logical submission, for the UI that created it.
    ///
    /// Reads the live attempt first, then the terminal record of a withdrawal.
    /// **An absent record is not a delivery claim**: it falls back to
    /// `QueuedLocally`, which is the last thing the client actually knew — a
    /// broker ack retires a record and proves nothing about the host.
    ///
    /// This is a lookup by client-local identity. It never correlates a server
    /// event with a submission.
    pub fn submission_lifecycle(&self, origin: SubmissionOriginId) -> SubmissionLifecycle {
        let live = self.pending_submissions.with(|records| {
            records
                .values()
                .find(|record| record.origin == origin)
                .map(|record| record.state)
        });
        match live {
            Some(PendingSubmissionState::QueuedLocally) => SubmissionLifecycle::QueuedLocally,
            Some(PendingSubmissionState::NotSent) => SubmissionLifecycle::NotSent,
            Some(PendingSubmissionState::DeliveryUnknown) => SubmissionLifecycle::DeliveryUnknown,
            None => self
                .withdrawn_submissions
                .with(|withdrawn| withdrawn.get(&origin).map(|entry| entry.withdrawal))
                .map(SubmissionLifecycle::Withdrawn)
                .unwrap_or(SubmissionLifecycle::QueuedLocally),
        }
    }

    /// Surfaced new-chat records for `host`, oldest first.
    ///
    /// New-chat records are host-scoped because the agent does not exist yet and
    /// the client must not guess which `NewAgent` was "ours".
    pub fn surfaced_new_chat_submissions(&self, host: &LocalHostId) -> Vec<PendingSubmission> {
        self.surfaced_submissions(|record| {
            record.local_host_id == *host && record.target == SubmissionTarget::NewChat
        })
    }

    /// Surfaced records addressed to a specific agent, oldest first. Ownership
    /// is known here — that is the agent we sent to.
    pub fn surfaced_agent_submissions(&self, agent_ref: &AgentRef) -> Vec<PendingSubmission> {
        self.surfaced_submissions(|record| {
            record.target == SubmissionTarget::Agent(agent_ref.clone())
        })
    }

    fn surfaced_submissions(
        &self,
        matches: impl Fn(&PendingSubmission) -> bool,
    ) -> Vec<PendingSubmission> {
        self.pending_submissions.with(|records| {
            let mut found: Vec<PendingSubmission> = records
                .values()
                .filter(|record| record.state.is_surfaced() && matches(record))
                .cloned()
                .collect();
            found.sort_by_key(|record| record.local_submission_id.0);
            found
        })
    }

    pub fn active_host_settings(&self) -> Option<HostSettings> {
        let host = self.active_local_host_id.get()?;
        self.host_settings_by_host.get().get(&host).cloned()
    }

    pub fn active_host_settings_untracked(&self) -> Option<HostSettings> {
        let host = self.active_local_host_id.get_untracked()?;
        self.host_settings_by_host
            .get_untracked()
            .get(&host)
            .cloned()
    }

    pub fn active_host_connection_status(&self) -> ConnectionStatus {
        let Some(host) = self.active_local_host_id.get() else {
            return ConnectionStatus::Disconnected;
        };
        self.host_connection_status(&host)
    }

    /// Connection status for an **explicitly named** host.
    ///
    /// Anything scoped to a specific agent must use this with the host carried on
    /// that agent's own `AgentRef`, never `active_host_connection_status()`.
    /// `active_local_host_id` is mutable navigation state: reading it to decide
    /// what an agent's chat should render infers the agent's host from wherever
    /// the user happens to be pointing, which is a different question and can be
    /// a different answer.
    pub fn host_connection_status(&self, host: &LocalHostId) -> ConnectionStatus {
        self.connection_statuses
            .get()
            .get(host)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    /// Whether `host` is in a state where a server snapshot could still arrive.
    ///
    /// This is the spinner's liveness test: if it is false, anything still
    /// waiting on the server is waiting for something that will never come, and
    /// must render a terminal, actionable state instead.
    pub fn host_can_deliver(&self, host: &LocalHostId) -> bool {
        matches!(
            self.host_connection_status(host),
            ConnectionStatus::Connecting
                | ConnectionStatus::Bootstrapping
                | ConnectionStatus::Connected
        )
    }

    /// True while the active host is connecting/connected but its
    /// `HostBootstrap` snapshot for the active host stream (the source of the
    /// agent + session lists) hasn't been applied yet. Returns false once the
    /// current stream's snapshot lands (even if the lists are genuinely empty)
    /// and on a failed/disconnected host, so a loading spinner never outlives
    /// a connection that won't deliver data.
    pub fn host_snapshot_pending(&self) -> bool {
        let Some(host) = self.active_local_host_id.get() else {
            return false;
        };
        if self.host_bootstrap_applied_for_current_stream(&host) {
            return false;
        }
        matches!(
            self.active_host_connection_status(),
            ConnectionStatus::Connecting
                | ConnectionStatus::Bootstrapping
                | ConnectionStatus::Connected
        )
    }

    pub fn active_host_command_error(&self) -> Option<String> {
        let host = self.active_local_host_id.get()?;
        self.command_errors_by_host.get().get(&host).cloned()
    }

    pub fn active_host_backend_setup(&self) -> Vec<BackendSetupInfo> {
        let Some(host) = self.active_local_host_id.get() else {
            return Vec::new();
        };
        self.backend_setup_by_host
            .get()
            .get(&host)
            .cloned()
            .unwrap_or_default()
    }

    pub fn active_session_list_state(&self) -> Option<SessionListLoadState> {
        let host = self.active_local_host_id.get()?;
        self.session_lists_by_host.get().get(&host).cloned()
    }

    pub fn active_host_custom_agents(&self) -> HashMap<CustomAgentId, CustomAgent> {
        let Some(host) = self.active_local_host_id.get() else {
            return HashMap::new();
        };
        self.custom_agents_by_host
            .get()
            .get(&host)
            .cloned()
            .unwrap_or_default()
    }

    pub fn active_paired_host(&self) -> Option<PairedHostSummary> {
        let host = self.active_local_host_id.get()?;
        self.paired_hosts
            .get()
            .into_iter()
            .find(|h| h.local_host_id == host)
    }

    /// Drops every per-host signal entry for `host`. Called when a host is
    /// forgotten (the user removed the pairing) or fully disconnects in a way
    /// that should clear cached snapshots.
    pub fn clear_host_runtime(&self, host: &LocalHostId) {
        self.active_connection_instance_ids.update(|m| {
            m.remove(host);
        });
        self.host_streams.update(|m| {
            m.remove(host);
        });
        self.bootstrapped_host_streams.update(|m| {
            m.remove(host);
        });
        self.heartbeat_pending_since_by_host.update(|m| {
            m.remove(host);
        });
        self.heartbeat_round_trip_ms_by_host.update(|m| {
            m.remove(host);
        });
        self.host_settings_by_host.update(|m| {
            m.remove(host);
        });
        self.command_errors_by_host.update(|m| {
            m.remove(host);
        });
        self.backend_setup_by_host.update(|m| {
            m.remove(host);
        });
        self.session_schemas_by_host.update(|m| {
            m.remove(host);
        });
        self.custom_agents_by_host.update(|m| {
            m.remove(host);
        });
        self.mcp_servers_by_host.update(|m| {
            m.remove(host);
        });
        self.steering_by_host.update(|m| {
            m.remove(host);
        });
        self.skills_by_host.update(|m| {
            m.remove(host);
        });

        self.projects
            .update(|projects| projects.retain(|p| p.local_host_id != *host));
        self.agents
            .update(|agents| agents.retain(|a| a.local_host_id != *host));
        self.agent_load_requests.update(|m| {
            m.retain(|k| k.local_host_id != *host);
        });
        self.agent_loaded.update(|m| {
            m.retain(|k| k.local_host_id != *host);
        });
        self.agent_load_errors.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.sessions
            .update(|sessions| sessions.retain(|s| s.local_host_id != *host));
        self.session_lists_by_host.update(|m| {
            m.remove(host);
        });

        self.file_tree.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.git_status.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.project_file_contents.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.project_diffs.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_summaries.update(|m| {
            m.retain(|(h, _), _| h != host);
        });
        self.reviews.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_errors.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.review_streams.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });

        self.chat_messages.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.chat_message_index.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.session_history.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.streaming_text.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        // Forgetting a host is the one path that legitimately destroys held
        // text: there is no longer a host to resend to. A *disconnect* must not
        // do this — the records are exactly what the user recovers from.
        self.pending_submissions.update(|m| {
            m.retain(|_, record| record.local_host_id != *host);
        });
        // …and it is the one point at which a withdrawal tombstone can safely go.
        //
        // This tears down the host's entire workspace — every agent, every
        // transcript, and therefore every tool card that could still be asking what
        // became of its reply. Nothing is left that can consult these, so keeping
        // them would be hoarding. Anywhere short of this (a disconnect, a count
        // cap, a scroll) the originating card is still mounted and still reading,
        // and dropping its tombstone puts the "queued locally" lie straight back.
        self.withdrawn_submissions.update(|m| {
            m.retain(|_, entry| entry.local_host_id != *host);
        });
        self.task_lists.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_message_queue.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_turn_active.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.transient_events.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_session_settings.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.agent_compactions.update(|m| {
            m.retain(|k, _| k.local_host_id != *host);
        });
        self.teams_by_host.update(|m| {
            m.remove(host);
        });
        self.team_members_by_host.update(|m| {
            m.remove(host);
        });
        self.team_bindings_by_host.update(|m| {
            m.remove(host);
        });
        self.team_compactions_by_host.update(|m| {
            m.remove(host);
        });
        self.team_preset_catalog_by_host.update(|m| {
            m.remove(host);
        });
        self.team_drafts_by_host.update(|m| {
            m.remove(host);
        });
        self.team_shuffle_suggestions_by_host.update(|m| {
            m.remove(host);
        });
        self.host_browses.update(|m| {
            m.retain(|(h, _), _| h != host);
        });

        if self
            .active_project
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.local_host_id == *host)
        {
            self.active_project.set(None);
        }
        if self
            .active_agent
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active.local_host_id == *host)
        {
            self.active_agent.set(None);
            self.viewing_chat.set(false);
        }
        if self
            .active_local_host_id
            .get_untracked()
            .as_ref()
            .is_some_and(|active| active == host)
        {
            self.active_local_host_id.set(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{GitBranchName, ProjectSource, WorkbenchRoot};

    fn top_level_project(host: &str, id: &str, name: &str, sort_order: u64) -> ProjectInfo {
        ProjectInfo {
            local_host_id: LocalHostId(host.to_owned()),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: name.to_owned(),
                sort_order,
                source: ProjectSource::Standalone {
                    roots: vec![ProjectRootPath(format!("/x/{id}"))],
                },
            },
        }
    }

    fn workbench_project(
        host: &str,
        id: &str,
        name: &str,
        sort_order: u64,
        parent_id: &str,
    ) -> ProjectInfo {
        ProjectInfo {
            local_host_id: LocalHostId(host.to_owned()),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: name.to_owned(),
                sort_order,
                source: ProjectSource::GitWorkbench {
                    parent_project_id: ProjectId(parent_id.to_owned()),
                    branch: GitBranchName(format!("branch-{id}")),
                    roots: vec![WorkbenchRoot {
                        parent_root: ProjectRootPath(format!("/x/{parent_id}")),
                        worktree_root: ProjectRootPath(format!("/x/wb/{id}")),
                    }],
                },
            },
        }
    }

    fn sorted_ids(projects: &[ProjectInfo]) -> Vec<&str> {
        projects.iter().map(|p| p.project.id.0.as_str()).collect()
    }

    /// Workbench children carry an independent per-parent sort_order
    /// sequence starting at 0, so a flat sort by raw sort_order would
    /// interleave them among top-level projects (A(0), wb(0), B(1)).
    /// The grouped sort must keep each workbench directly beneath its
    /// parent instead.
    #[test]
    fn sort_project_infos_groups_workbenches_under_parent() {
        let mut projects = vec![
            workbench_project("h-1", "wb-b", "Bench B", 0, "p-b"),
            top_level_project("h-1", "p-b", "B", 1),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(sorted_ids(&projects), vec!["p-a", "p-b", "wb-b"]);
    }

    /// Multiple children of one parent order by their own sort_order,
    /// and siblings of different parents never interleave.
    #[test]
    fn sort_project_infos_orders_children_per_parent() {
        let mut projects = vec![
            workbench_project("h-1", "wb-a2", "Bench A2", 1, "p-a"),
            top_level_project("h-1", "p-b", "B", 1),
            workbench_project("h-1", "wb-b1", "Bench B1", 0, "p-b"),
            workbench_project("h-1", "wb-a1", "Bench A1", 0, "p-a"),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(
            sorted_ids(&projects),
            vec!["p-a", "wb-a1", "wb-a2", "p-b", "wb-b1"]
        );
    }

    /// A workbench whose parent hasn't arrived yet (out-of-order
    /// upserts) sorts after all known top-level groups rather than
    /// panicking or landing somewhere arbitrary in the middle.
    #[test]
    fn sort_project_infos_pushes_orphan_workbenches_to_end() {
        let mut projects = vec![
            workbench_project("h-1", "wb-orphan", "Bench Orphan", 0, "p-missing"),
            top_level_project("h-1", "p-b", "B", 1),
            top_level_project("h-1", "p-a", "A", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(sorted_ids(&projects), vec!["p-a", "p-b", "wb-orphan"]);
    }

    /// Hosts stay segregated: grouping happens within a host, never
    /// across two paired hosts that reuse project ids.
    #[test]
    fn sort_project_infos_keeps_hosts_separate() {
        let mut projects = vec![
            workbench_project("h-2", "wb-2", "Bench", 0, "p-1"),
            top_level_project("h-2", "p-1", "Same Id Other Host", 0),
            top_level_project("h-1", "p-1", "First Host", 0),
        ];
        sort_project_infos(&mut projects);
        assert_eq!(projects[0].local_host_id.0, "h-1");
        assert_eq!(projects[1].local_host_id.0, "h-2");
        assert_eq!(sorted_ids(&projects), vec!["p-1", "p-1", "wb-2"]);
    }

    #[test]
    fn local_host_id_serializes_transparent() {
        let id = LocalHostId("h-1".to_owned());
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(encoded, "\"h-1\"");
    }

    #[test]
    fn paired_host_connection_status_maps_to_connection_status() {
        assert_eq!(
            ConnectionStatus::Connecting,
            PairedHostConnectionStatus::Connecting.into()
        );
        assert_eq!(
            ConnectionStatus::Bootstrapping,
            PairedHostConnectionStatus::Connected.into()
        );
        assert!(matches!(
            ConnectionStatus::from(PairedHostConnectionStatus::Disconnected {
                reason: "x".to_owned(),
            }),
            ConnectionStatus::Disconnected
        ));
        assert!(matches!(
            ConnectionStatus::from(PairedHostConnectionStatus::Failed {
                code: protocol::MobileAccessErrorCode::TransportFailed,
                message: "boom".to_owned(),
            }),
            ConnectionStatus::Error(_)
        ));
    }

    fn held(state: &AppState, id: u64, target: SubmissionTarget, instance: u64) {
        state.hold_submission(PendingSubmission {
            local_submission_id: LocalSubmissionId(id),
            origin: SubmissionOriginId(id),
            local_host_id: LocalHostId("h-1".to_owned()),
            connection_instance_id: instance,
            target,
            text: format!("message {id}"),
            images: Vec::new(),
            tool_response: None,
            state: PendingSubmissionState::QueuedLocally,
        });
    }

    fn held_with_images(state: &AppState, id: u64, target: SubmissionTarget, instance: u64) {
        state.hold_submission(PendingSubmission {
            local_submission_id: LocalSubmissionId(id),
            origin: SubmissionOriginId(id),
            local_host_id: LocalHostId("h-1".to_owned()),
            connection_instance_id: instance,
            target,
            text: format!("message {id}"),
            images: vec![protocol::ImageData {
                media_type: "image/png".to_owned(),
                data: "AAAA".to_owned(),
            }],
            tool_response: None,
            state: PendingSubmissionState::QueuedLocally,
        });
    }

    /// The happy path must leave nothing behind. A broker ACK is a transport
    /// fact — it does not prove the host received or applied anything — so it
    /// retires the record silently rather than surfacing a "sent" artifact the
    /// user would have to dismiss.
    #[test]
    fn broker_acknowledged_retires_the_record_silently() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        held(&state, 1, SubmissionTarget::NewChat, 7);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::BrokerAcknowledged,
        );

        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "a broker-acknowledged submission must be retired, not kept"
        );
        assert!(
            state.surfaced_new_chat_submissions(&host).is_empty(),
            "the happy path must never surface an artifact"
        );
    }

    /// While a submission is on its way out it is held, but silent: no banner,
    /// no artifact, nothing to dismiss.
    #[test]
    fn queued_submissions_are_held_but_never_surfaced() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        held(&state, 1, SubmissionTarget::NewChat, 7);

        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "the text must have a holder while it is in flight"
        );
        assert!(
            state.surfaced_new_chat_submissions(&host).is_empty(),
            "an in-flight submission must not be shown to the user"
        );
    }

    /// A failure is the only thing that surfaces a record.
    #[test]
    fn transport_failures_surface_with_their_typed_state() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        held(&state, 1, SubmissionTarget::NewChat, 7);
        held(&state, 2, SubmissionTarget::NewChat, 7);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::DeliveryUnknown,
        );
        state.apply_submission_outcome(
            LocalSubmissionId(2),
            7,
            SubmissionTransportOutcome::NotSent,
        );

        let surfaced = state.surfaced_new_chat_submissions(&host);
        assert_eq!(surfaced.len(), 2, "both failures must be surfaced");
        assert_eq!(surfaced[0].state, PendingSubmissionState::DeliveryUnknown);
        assert_eq!(surfaced[1].state, PendingSubmissionState::NotSent);
        assert_ne!(
            surfaced[0].state_label(),
            surfaced[1].state_label(),
            "'may not have been received' and 'not sent' are different claims and must read differently"
        );
    }

    /// `NotSent` is provably never transmitted — and it is *still* not injected
    /// back into the composer.
    ///
    /// The earlier implementation auto-restored it whenever the composer happened
    /// to be empty. That was wrong twice over: it wrote back only `text`, silently
    /// destroying any `images` the record carried (the composer cannot hold them),
    /// and it was the sole path that put words in the composer without the user
    /// asking. The composer belongs to the user; **Edit** is the only way text
    /// comes back.
    #[test]
    fn not_sent_surfaces_for_recovery_and_never_injects_into_the_composer() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "precondition: an empty composer is exactly when the old auto-restore fired"
        );
        held_with_images(&state, 1, SubmissionTarget::NewChat, 7);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::NotSent,
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "no transport outcome may write the composer — not even a provably-unsent one"
        );
        let surfaced = state.surfaced_new_chat_submissions(&host);
        assert_eq!(
            surfaced.len(),
            1,
            "the message must stay recoverable through the explicit controls"
        );
        assert_eq!(surfaced[0].state, PendingSubmissionState::NotSent);
        assert_eq!(
            surfaced[0].images.len(),
            1,
            "the record must still hold its images — the old auto-restore dropped them"
        );
    }

    /// A draft the user is in the middle of typing is never touched by an
    /// arriving outcome.
    #[test]
    fn an_outcome_never_overwrites_a_newer_draft() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.chat_input.set("something new".to_owned());
        held(&state, 1, SubmissionTarget::NewChat, 7);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::NotSent,
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "something new",
            "the user's in-progress text must survive any arriving transport outcome"
        );
        assert_eq!(
            state.surfaced_new_chat_submissions(&host).len(),
            1,
            "the failed message must surface for manual recovery instead"
        );
    }

    /// A new-chat record has no agent, and the client must never invent one.
    /// It surfaces on the host, and never inside somebody else's chat.
    #[test]
    fn new_chat_recovery_is_host_scoped_and_never_attributed_to_an_agent() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());
        let agent = AgentRef {
            local_host_id: host.clone(),
            agent_id: AgentId("a-1".to_owned()),
        };
        held(&state, 1, SubmissionTarget::NewChat, 7);
        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::DeliveryUnknown,
        );

        assert_eq!(
            state.surfaced_new_chat_submissions(&host).len(),
            1,
            "a new-chat failure must be recoverable at the host level"
        );
        assert!(
            state.surfaced_agent_submissions(&agent).is_empty(),
            "a new-chat submission must never be claimed by any agent's chat"
        );
    }

    /// A record belongs to the connection that admitted it. An outcome reported
    /// by some other connection instance must not touch it.
    #[test]
    fn an_outcome_from_a_foreign_connection_instance_is_ignored() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            999,
            SubmissionTransportOutcome::BrokerAcknowledged,
        );

        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "a dead connection must not be able to retire a live connection's submission"
        );
    }

    /// A record leaving the store is ambiguous on its own — acknowledged, or
    /// thrown away? The lifecycle has to say which, because "queued locally" is the
    /// last true statement about the first and a flat lie about the second.
    #[test]
    fn a_withdrawn_submission_is_never_reported_as_still_queued() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);
        let origin = SubmissionOriginId(1);

        assert_eq!(
            state.submission_lifecycle(origin),
            SubmissionLifecycle::QueuedLocally,
            "in flight"
        );

        state.withdraw_submission(LocalSubmissionId(1), SubmissionWithdrawal::Discarded);

        assert_eq!(
            state.submission_lifecycle(origin),
            SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::Discarded),
            "a discarded message must not report as queued — the user threw it away"
        );
    }

    /// **No number of later withdrawals may cost an earlier one its truth.**
    ///
    /// The tombstones were LRU-capped at 128 on the theory that an evicted entry
    /// belonged to UI that had "scrolled away". A tool card does not unmount when it
    /// scrolls away — it sits in the transcript re-reading its lifecycle forever —
    /// so the cap dropped facts belonging to *rendered* UI, and the card resumed
    /// telling the user a message they discarded was on its way.
    #[test]
    fn a_withdrawal_is_never_evicted_by_a_later_one() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);
        let first = SubmissionOriginId(1);
        state.withdraw_submission(LocalSubmissionId(1), SubmissionWithdrawal::Discarded);

        // Far past the cap that used to exist.
        for id in 100..600u64 {
            state.hold_submission(PendingSubmission {
                local_submission_id: LocalSubmissionId(id),
                origin: SubmissionOriginId(id),
                local_host_id: LocalHostId("h-1".to_owned()),
                connection_instance_id: 7,
                target: SubmissionTarget::NewChat,
                text: format!("junk {id}"),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::NotSent,
            });
            state.withdraw_submission(LocalSubmissionId(id), SubmissionWithdrawal::Discarded);
        }

        assert_eq!(
            state.submission_lifecycle(first),
            SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::Discarded),
            "500 later discards must not resurrect the 'queued locally' lie about the first"
        );
        assert_eq!(
            state.withdrawn_submissions.get_untracked().len(),
            501,
            "every tombstone is kept — they are the user's own destructive actions, \
             not a cache to be trimmed"
        );
    }

    /// Tombstones do have a lifecycle — it is just tied to teardown, not to a count.
    /// Forgetting a host unmounts its entire workspace, so nothing is left that could
    /// consult them.
    #[test]
    fn forgetting_a_host_drops_its_withdrawal_tombstones_and_only_its_own() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);
        state.hold_submission(PendingSubmission {
            local_submission_id: LocalSubmissionId(2),
            origin: SubmissionOriginId(2),
            local_host_id: LocalHostId("h-2".to_owned()),
            connection_instance_id: 7,
            target: SubmissionTarget::NewChat,
            text: "other host".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: PendingSubmissionState::NotSent,
        });
        state.withdraw_submission(LocalSubmissionId(1), SubmissionWithdrawal::Discarded);
        state.withdraw_submission(LocalSubmissionId(2), SubmissionWithdrawal::Discarded);

        state.clear_host_runtime(&LocalHostId("h-1".to_owned()));

        assert_eq!(
            state.submission_lifecycle(SubmissionOriginId(1)),
            SubmissionLifecycle::QueuedLocally,
            "the forgotten host's UI is gone, so its tombstone has nothing left to \
             tell — keeping it would be hoarding"
        );
        assert_eq!(
            state.submission_lifecycle(SubmissionOriginId(2)),
            SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::Discarded),
            "another host's cards are still on screen and must keep their truth"
        );
    }

    /// A broker ack retires the record and proves nothing about the host. Absence
    /// therefore falls back to the last true statement, and never becomes a
    /// delivery claim.
    #[test]
    fn a_broker_ack_leaves_the_lifecycle_at_queued_locally_and_claims_nothing_more() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);
        let origin = SubmissionOriginId(1);

        state.apply_submission_outcome(
            LocalSubmissionId(1),
            7,
            SubmissionTransportOutcome::BrokerAcknowledged,
        );

        assert!(state.pending_submissions.get_untracked().is_empty());
        assert_eq!(
            state.submission_lifecycle(origin),
            SubmissionLifecycle::QueuedLocally,
            "a PUBACK is a transport fact — it must not be promoted into delivery, \
             and it must not be mistaken for a withdrawal either"
        );
    }

    /// Superseding an attempt is not the user withdrawing the message. The lineage
    /// survives, carried by the replacement.
    #[test]
    fn retiring_a_superseded_attempt_does_not_withdraw_the_message() {
        let state = AppState::new();
        held(&state, 1, SubmissionTarget::NewChat, 7);
        let origin = SubmissionOriginId(1);

        // A resend: new attempt, same logical message.
        state.hold_submission(PendingSubmission {
            local_submission_id: LocalSubmissionId(2),
            origin,
            local_host_id: LocalHostId("h-1".to_owned()),
            connection_instance_id: 8,
            target: SubmissionTarget::NewChat,
            text: "message 1".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: PendingSubmissionState::QueuedLocally,
        });
        state.retire_submission_attempt(LocalSubmissionId(1));

        assert!(
            state.withdrawn_submissions.get_untracked().is_empty(),
            "a resend must not be recorded as the user taking the message back"
        );
        state.apply_submission_outcome(
            LocalSubmissionId(2),
            8,
            SubmissionTransportOutcome::DeliveryUnknown,
        );
        assert_eq!(
            state.submission_lifecycle(origin),
            SubmissionLifecycle::DeliveryUnknown,
            "the replacement's outcome is the message's outcome — the lineage is what \
             the originating UI tracks"
        );
    }

    /// `SendMessagePayload::images` skips serializing when `None`, so `None` and
    /// `Some(vec![])` are different bytes. One mapping, used by every send, or a
    /// resend is not the same message.
    #[test]
    fn wire_images_maps_empty_to_absent_and_non_empty_to_a_list() {
        assert_eq!(
            wire_images(&[]),
            None,
            "no images means the field is absent"
        );
        let images = vec![protocol::ImageData {
            media_type: "image/png".to_owned(),
            data: "AAAA".to_owned(),
        }];
        assert_eq!(
            wire_images(&images),
            Some(images.clone()),
            "held images must go on the wire verbatim"
        );
    }

    /// A resend is only ever offered once the connection that swallowed the
    /// original is gone.
    #[test]
    fn resend_is_only_offered_on_a_genuinely_new_connection() {
        let record = PendingSubmission {
            local_submission_id: LocalSubmissionId(1),
            origin: SubmissionOriginId(1),
            local_host_id: LocalHostId("h-1".to_owned()),
            connection_instance_id: 7,
            target: SubmissionTarget::NewChat,
            text: "hi".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: PendingSubmissionState::DeliveryUnknown,
        };
        assert!(
            !record.can_resend_on(Some(7)),
            "resending on the same connection that lost it is not recovery"
        );
        assert!(
            !record.can_resend_on(None),
            "there is nothing to resend on while disconnected"
        );
        assert!(
            record.can_resend_on(Some(8)),
            "a new connection is what makes a deliberate resend possible"
        );
    }

    /// The cap is a **hard pre-admission gate**, and it never evicts.
    ///
    /// The earlier implementation evicted the oldest `QueuedLocally` record to
    /// make room. That destroyed the sole copy of a message that was still *in
    /// flight* — an in-flight submission is unresolved, not safe, and can still
    /// come back `NotSent` or `DeliveryUnknown` with nothing left to recover
    /// from. Worse, the eviction happened *after* the replacement frame had
    /// already been handed to the transport, so it could not be taken back.
    ///
    /// The only honest place to refuse is before admission, while the text is
    /// still safely in the composer.
    #[test]
    fn the_cap_refuses_new_submissions_and_never_evicts_an_unresolved_one() {
        let state = AppState::new();
        let host = LocalHostId("h-1".to_owned());

        for id in 0..(MAX_PENDING_SUBMISSIONS_PER_HOST as u64 - 1) {
            held(&state, id, SubmissionTarget::NewChat, 7);
        }
        assert!(
            state.can_hold_submission_untracked(&host),
            "below the cap, a new submission is holdable"
        );

        // The cap boundary: one below, at, and the record that would have been
        // evicted is still in flight.
        held(
            &state,
            MAX_PENDING_SUBMISSIONS_PER_HOST as u64 - 1,
            SubmissionTarget::NewChat,
            7,
        );
        assert_eq!(
            state.held_submission_count_untracked(&host),
            MAX_PENDING_SUBMISSIONS_PER_HOST
        );
        assert!(
            !state.can_hold_submission_untracked(&host),
            "at the cap the *new* send must be refused — before it reaches the transport"
        );

        // Every record is still QueuedLocally, i.e. exactly the state the old
        // code considered fair game to evict. Holding one more must not drop any.
        state.hold_submission(PendingSubmission {
            local_submission_id: LocalSubmissionId(9_999),
            origin: SubmissionOriginId(9_999),
            local_host_id: host.clone(),
            connection_instance_id: 7,
            target: SubmissionTarget::NewChat,
            text: "over cap".to_owned(),
            images: Vec::new(),
            tool_response: None,
            state: PendingSubmissionState::QueuedLocally,
        });
        assert!(
            state
                .pending_submissions
                .get_untracked()
                .contains_key(&LocalSubmissionId(0)),
            "the oldest in-flight record must survive: it is unresolved, and its text \
             has no other holder"
        );
        assert_eq!(
            state.held_submission_count_untracked(&host),
            MAX_PENDING_SUBMISSIONS_PER_HOST + 1,
            "holding never evicts — going one over is strictly better than losing a message"
        );

        // And every one of them is still recoverable once they fail.
        for id in 0..MAX_PENDING_SUBMISSIONS_PER_HOST as u64 {
            state.apply_submission_outcome(
                LocalSubmissionId(id),
                7,
                SubmissionTransportOutcome::DeliveryUnknown,
            );
        }
        assert_eq!(
            state.surfaced_new_chat_submissions(&host).len(),
            MAX_PENDING_SUBMISSIONS_PER_HOST,
            "every failure must still be recoverable"
        );
    }

    #[test]
    fn update_required_message_names_host_build_when_present() {
        let version = TydeReleaseVersion::parse("0.8.19-beta.15").unwrap();
        let with_build = update_required_message(31, 30, Some(&version));
        assert!(with_build.contains("0.8.19-beta.15"), "{with_build}");
        assert!(with_build.contains("protocol 31"), "{with_build}");
        assert!(with_build.contains("app protocol 30"), "{with_build}");

        let without = update_required_message(31, 30, None);
        assert_eq!(
            without,
            "Update required — host protocol 31, app protocol 30"
        );
    }
}
