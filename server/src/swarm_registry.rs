use std::io::{Read, Write};
use std::path::PathBuf;

use base64::Engine;
use protocol::*;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::now_ms;

#[derive(Clone)]
pub(crate) struct SwarmRegistryHandle {
    tx: mpsc::Sender<Command>,
    stopped: CancellationToken,
    terminated: CancellationToken,
}

type DispatchReservation = (Vec<SwarmDispatch>, Vec<SwarmEventPayload>);
type PrivateAdmissionOutcome = (Option<SwarmDispatch>, Vec<SwarmEventPayload>);
type SwarmCommitReply<T> = oneshot::Sender<Result<SwarmCommit<T>, SwarmFailure>>;

enum Command {
    #[cfg(feature = "test-support")]
    FailNextDirectorySync(oneshot::Sender<Result<(), SwarmFailure>>),
    PrivateAdmission(AgentId, bool, SwarmCommitReply<PrivateAdmissionOutcome>),
    Snapshot(oneshot::Sender<Result<SwarmStoreSnapshot, SwarmFailure>>),
    ReadImage(
        SwarmId,
        SwarmImageId,
        oneshot::Sender<Result<(SwarmImage, ImageData), SwarmFailure>>,
    ),
    ContainsAgent(AgentId, oneshot::Sender<Result<bool, SwarmFailure>>),
    Apply(
        SwarmCommandPayload,
        SwarmCommitReply<Vec<SwarmEventPayload>>,
    ),
    Publish(
        SwarmId,
        SwarmAuthor,
        SwarmPublication,
        SwarmCommitReply<SwarmPublicationOutcome>,
    ),
    Authorize(
        AgentId,
        oneshot::Sender<Result<SwarmDescribe, SwarmFailure>>,
    ),
    Reserve(Vec<AgentId>, SwarmCommitReply<DispatchReservation>),
    Defer(SwarmDispatch, SwarmCommitReply<Vec<SwarmEventPayload>>),
    Complete(
        SwarmDispatch,
        Result<(AgentId, SessionId), SwarmFailure>,
        SwarmCommitReply<Vec<SwarmEventPayload>>,
    ),
    Status(
        AgentId,
        AgentControlStatus,
        Option<SessionId>,
        bool,
        Option<SwarmFailure>,
        SwarmCommitReply<Vec<SwarmEventPayload>>,
    ),
    Migration(SwarmDraft, SwarmCommitReply<Vec<SwarmEventPayload>>),
    Binding(
        SwarmId,
        SwarmMemberId,
        AgentId,
        SwarmCommitReply<Vec<SwarmEventPayload>>,
    ),
    Error(
        SwarmId,
        SwarmFailure,
        SwarmCommitReply<Vec<SwarmEventPayload>>,
    ),
}

impl SwarmRegistryHandle {
    pub(crate) fn spawn(
        path: PathBuf,
        stopped: CancellationToken,
        sessions: Vec<crate::store::session::SessionRecord>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel(64);
        let terminated = CancellationToken::new();
        let completion = terminated.clone().drop_guard();
        let actor_stopped = stopped.clone();
        let worker = async move {
            let completion = completion;
            let mut actor = Actor::load(path, &sessions);
            loop {
                let command = tokio::select! {
                    biased;
                    () = actor_stopped.cancelled() => break,
                    command = rx.recv() => match command { Some(command) => command, None => break },
                };
                if actor_stopped.is_cancelled() {
                    break;
                }
                match command {
                    #[cfg(feature = "test-support")]
                    Command::FailNextDirectorySync(reply) => {
                        actor.fail_directory_sync = true;
                        let _ = reply.send(Ok(()));
                    }
                    Command::PrivateAdmission(agent, starts_turn, reply) => {
                        let result = actor.transaction(|file| {
                            let Some(swarm) = file.swarms.iter_mut().find(|swarm| swarm.members.iter().any(|member| member.agent_id.as_ref() == Some(&agent))) else { return Ok((None, Vec::new())); };
                            let member = swarm.members.iter_mut().find(|member| member.agent_id.as_ref() == Some(&agent)).ok_or(failure(SwarmErrorCode::NotFound, "Member binding is unavailable"))?;
                            if swarm.recovery_requirement != SwarmRecoveryRequirement::None || !matches!(swarm.lifecycle, SwarmLifecycle::Running | SwarmLifecycle::Launching | SwarmLifecycle::Transitioning) || !matches!(member.state, SwarmMemberState::Live | SwarmMemberState::Reserved | SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved) { return Err(failure(SwarmErrorCode::Conflict, "Member private execution requires a running, admitted swarm member")); }
                            let mut batch = None;
                            if starts_turn {
                                if member.state != SwarmMemberState::Live { return Err(failure(SwarmErrorCode::Conflict, "Retiring members may finish their current turn, not start a private turn")); }
                                if member.runtime_status != Some(AgentControlStatus::Idle) { return Err(failure(SwarmErrorCode::Conflict, "Busy member private input is deferred: use a shared board post, or wait for an idle private turn")); }
                                let previous_round_id = member.current_round_id.clone();
                                let round_id = SwarmRoundId(fresh());
                                swarm.rounds.push(SwarmRound { id: round_id.clone(), agent_activations_remaining: swarm.constraints.agent_wake_budget });
                                member.current_round_id = Some(round_id);
                                member.state = SwarmMemberState::Reserved;
                                batch = Some(SwarmDispatch { previous_round_id, agent_activation_charged: false, notification_post_ids: Vec::new(), swarm_id: swarm.id.clone(), member: member.clone(), constraints: swarm.constraints.clone(), notification_ids: Vec::new(), posts: Vec::new() });
                            }
                            Ok((batch, vec![swarm_event(swarm)]))
                        });
                        let _ = reply.send(result);
                    }
                    Command::ContainsAgent(id, reply) => {
                        let result = actor
                            .file
                            .as_ref()
                            .map(|file| {
                                file.swarms.iter().any(|swarm| {
                                    swarm
                                        .members
                                        .iter()
                                        .any(|member| member.agent_id.as_ref() == Some(&id))
                                })
                            })
                            .map_err(Clone::clone);
                        let _ = reply.send(result);
                    }
                    Command::ReadImage(swarm_id, image_id, reply) => {
                        let _ = reply.send(actor.read_image(&swarm_id, &image_id));
                    }
                    Command::Snapshot(reply) => {
                        let _ = reply.send(actor.snapshot());
                    }
                    Command::Apply(payload, reply) => {
                        let result = actor.apply(payload);
                        let _ = reply.send(result);
                    }
                    Command::Publish(id, author, publication, reply) => {
                        let result =
                            actor.transaction(|file| publish(file, &id, author, publication));
                        let _ = reply.send(result);
                    }
                    Command::Authorize(id, reply) => {
                        let result = actor.snapshot().and_then(|file| {
                            for swarm in file.swarms {
                                if let Some(member) = swarm
                                    .members
                                    .iter()
                                    .find(|member| member.agent_id.as_ref() == Some(&id))
                                {
                                    if !matches!(
                                        member.state,
                                        SwarmMemberState::Live
                                            | SwarmMemberState::Reserved
                                            | SwarmMemberState::Retiring
                                            | SwarmMemberState::RetiringReserved
                                    ) {
                                        return Err(failure(
                                            SwarmErrorCode::Unauthorized,
                                            "swarm caller is not an active member",
                                        ));
                                    }
                                    return Ok(SwarmDescribe {
                                        member_id: member.spec.id.clone(),
                                        swarm,
                                        workspace_projects: Vec::new(),
                                    });
                                }
                            }
                            Err(failure(
                                SwarmErrorCode::Unauthorized,
                                "caller is not a swarm member",
                            ))
                        });
                        let _ = reply.send(result);
                    }
                    Command::Reserve(eligible_live_agents, reply) => {
                        let result = actor.transaction(|file| reserve(file, &eligible_live_agents));
                        let _ = reply.send(result);
                    }
                    Command::Defer(batch, reply) => {
                        let result = actor.transaction(|file| {
                            let swarm = swarm_mut(file, &batch.swarm_id)?;
                            let member = member_mut(swarm, &batch.member.spec.id)?;
                            if member.state == SwarmMemberState::Reserved {
                                member.state = if member.agent_id.is_some() {
                                    SwarmMemberState::Live
                                } else if member.session_id.is_some() {
                                    SwarmMemberState::Dormant
                                } else {
                                    SwarmMemberState::Proposed
                                };
                            }
                            if member.state == SwarmMemberState::RetiringReserved {
                                member.state = SwarmMemberState::Retiring;
                            }
                            member.current_round_id = batch.previous_round_id.clone();
                            let undeliverable = matches!(
                                member.state,
                                SwarmMemberState::Retiring
                                    | SwarmMemberState::RetiringReserved
                                    | SwarmMemberState::Retired
                            );
                            if member.agent_id.is_none() {
                                member.runtime_status = None;
                            }
                            if batch.agent_activation_charged
                                && let Some(round_id) = &batch.member.current_round_id
                                && let Some(round) =
                                    swarm.rounds.iter_mut().find(|round| round.id == *round_id)
                            {
                                round.agent_activations_remaining += 1;
                            }
                            for notification in &mut swarm.notifications {
                                if batch.notification_ids.contains(&notification.id)
                                    && notification.state == SwarmDeliveryState::Dispatching
                                {
                                    notification.state = if undeliverable {
                                        SwarmDeliveryState::Undeliverable
                                    } else {
                                        SwarmDeliveryState::Pending
                                    };
                                    if undeliverable {
                                        notification.error =
                                            Some("Recipient retired before dispatch".into());
                                    }
                                }
                            }
                            if swarm.lifecycle == SwarmLifecycle::Pausing
                                && swarm.members.iter().all(|member| {
                                    !matches!(
                                        member.state,
                                        SwarmMemberState::Reserved
                                            | SwarmMemberState::RetiringReserved
                                    ) && member.runtime_status.is_none_or(|status| {
                                        matches!(
                                            status,
                                            AgentControlStatus::Idle | AgentControlStatus::Failed
                                        )
                                    })
                                })
                            {
                                swarm.lifecycle = SwarmLifecycle::Paused;
                            }
                            Ok(vec![swarm_event(swarm)])
                        });
                        let _ = reply.send(result);
                    }
                    Command::Complete(batch, result, reply) => {
                        let result = actor.transaction(|file| complete(file, batch, result));
                        let _ = reply.send(result);
                    }
                    Command::Status(id, status, session, terminated, failure, reply) => {
                        let result = actor.transaction(|file| {
                            record_status(file, &id, status, session, terminated, failure)
                        });
                        let _ = reply.send(result);
                    }
                    Command::Migration(draft, reply) => {
                        let result = actor.transaction(|file| migration(file, draft));
                        let _ = reply.send(result);
                    }
                    Command::Binding(id, member_id, agent_id, reply) => {
                        let result = actor.transaction(|file| {
                            let swarm = swarm_mut(file, &id)?;
                            if swarm.recovery_requirement != SwarmRecoveryRequirement::None
                                || !matches!(
                                    swarm.lifecycle,
                                    SwarmLifecycle::Launching
                                        | SwarmLifecycle::Running
                                        | SwarmLifecycle::Transitioning
                                )
                            {
                                return Err(failure(
                                    SwarmErrorCode::Conflict,
                                    "Swarm no longer authorizes member startup",
                                ));
                            }
                            let member = member_mut(swarm, &member_id)?;
                            if member.state != SwarmMemberState::Reserved
                                || member.agent_id.is_some()
                            {
                                return Err(failure(
                                    SwarmErrorCode::Conflict,
                                    "Member no longer holds an unbound startup reservation",
                                ));
                            }
                            member.agent_id = Some(agent_id);
                            Ok(vec![swarm_event(swarm)])
                        });
                        let _ = reply.send(result);
                    }
                    Command::Error(id, message, reply) => {
                        let result = actor.transaction(|file| {
                            let swarm = swarm_mut(file, &id)?;
                            swarm.error = Some(message.message);
                            swarm.lifecycle = SwarmLifecycle::AttentionRequired;
                            Ok(vec![swarm_event(swarm)])
                        });
                        let _ = reply.send(result);
                    }
                }
            }
            drop(completion);
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(worker);
        } else if let Err(error) = std::thread::Builder::new()
            .name("tyde-swarms".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(worker),
                    Err(error) => tracing::error!(%error, "failed to start swarm actor runtime"),
                }
            })
        {
            tracing::error!(%error, "failed to start swarm actor thread");
        }
        Self {
            tx,
            stopped,
            terminated,
        }
    }
    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, SwarmFailure>>) -> Command,
    ) -> Result<T, SwarmFailure> {
        let (tx, rx) = oneshot::channel();
        tokio::select! {
            biased;
            () = self.stopped.cancelled() => Err(failure(SwarmErrorCode::Lifecycle, "Swarm host is stopped")),
            result = async {
                self.tx.send(command(tx)).await.map_err(|_| failure(SwarmErrorCode::Lifecycle, "Swarm actor closed"))?;
                rx.await.map_err(|_| failure(SwarmErrorCode::Lifecycle, "Swarm actor reply closed"))?
            } => result,
        }
    }
    #[cfg(feature = "test-support")]
    pub(crate) async fn fail_next_directory_sync(&self) -> Result<(), SwarmFailure> {
        self.request(Command::FailNextDirectorySync).await
    }
    pub(crate) async fn wait_stopped(&self) {
        self.terminated.cancelled().await;
    }
    async fn committed_events(
        &self,
        commit: SwarmCommit<Vec<SwarmEventPayload>>,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        let uncertain = matches!(
            commit.status,
            SwarmCommitStatus::CommittedDurabilityUncertain { .. }
        );
        let mut events = commit.value;
        if uncertain {
            events.extend(self.snapshot().await?.swarms.iter().map(swarm_event));
        }
        events.extend(commit_warning(commit.status));
        Ok(events)
    }
    pub(crate) async fn private_admission(
        &self,
        agent: AgentId,
        starts_turn: bool,
    ) -> Result<PrivateAdmissionOutcome, SwarmFailure> {
        let commit = self
            .request(|reply| Command::PrivateAdmission(agent, starts_turn, reply))
            .await?;
        let (batch, events) = commit.value;
        Ok((
            batch,
            self.committed_events(SwarmCommit {
                value: events,
                status: commit.status,
            })
            .await?,
        ))
    }
    pub(crate) async fn snapshot(&self) -> Result<SwarmStoreSnapshot, SwarmFailure> {
        self.request(Command::Snapshot).await
    }
    pub(crate) async fn read_image(
        &self,
        swarm_id: SwarmId,
        image_id: SwarmImageId,
    ) -> Result<(SwarmImage, ImageData), SwarmFailure> {
        self.request(|reply| Command::ReadImage(swarm_id, image_id, reply))
            .await
    }
    pub(crate) async fn apply(
        &self,
        payload: SwarmCommandPayload,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(self.request(|reply| Command::Apply(payload, reply)).await?)
            .await
    }
    pub(crate) async fn publish(
        &self,
        id: SwarmId,
        author: SwarmAuthor,
        publication: SwarmPublication,
    ) -> Result<SwarmPublicationOutcome, SwarmFailure> {
        let commit = self
            .request(|reply| Command::Publish(id, author, publication, reply))
            .await?;
        let mut value = commit.value;
        value.commit_status = commit.status;
        Ok(value)
    }
    pub(crate) async fn contains_agent(&self, id: AgentId) -> Result<bool, SwarmFailure> {
        self.request(|reply| Command::ContainsAgent(id, reply))
            .await
    }
    pub(crate) async fn describe(&self, id: AgentId) -> Result<SwarmDescribe, SwarmFailure> {
        self.request(|reply| Command::Authorize(id, reply)).await
    }
    pub(crate) async fn reserve(
        &self,
        eligible_live_agents: Vec<AgentId>,
    ) -> Result<DispatchReservation, SwarmFailure> {
        let commit = self
            .request(|reply| Command::Reserve(eligible_live_agents, reply))
            .await?;
        let (batches, events) = commit.value;
        Ok((
            batches,
            self.committed_events(SwarmCommit {
                value: events,
                status: commit.status,
            })
            .await?,
        ))
    }
    pub(crate) async fn defer(
        &self,
        batch: SwarmDispatch,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(self.request(|reply| Command::Defer(batch, reply)).await?)
            .await
    }
    pub(crate) async fn complete(
        &self,
        batch: SwarmDispatch,
        result: Result<(AgentId, SessionId), SwarmFailure>,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(
            self.request(|reply| Command::Complete(batch, result, reply))
                .await?,
        )
        .await
    }
    pub(crate) async fn status(
        &self,
        id: AgentId,
        status: AgentControlStatus,
        session: Option<SessionId>,
        terminated: bool,
        failure: Option<SwarmFailure>,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(
            self.request(|reply| Command::Status(id, status, session, terminated, failure, reply))
                .await?,
        )
        .await
    }
    pub(crate) async fn migration(
        &self,
        draft: SwarmDraft,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(
            self.request(|reply| Command::Migration(draft, reply))
                .await?,
        )
        .await
    }
    pub(crate) async fn bind(
        &self,
        id: SwarmId,
        member: SwarmMemberId,
        agent: AgentId,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(
            self.request(|reply| Command::Binding(id, member, agent, reply))
                .await?,
        )
        .await
    }
    pub(crate) async fn error(
        &self,
        id: SwarmId,
        message: SwarmFailure,
    ) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
        self.committed_events(
            self.request(|reply| Command::Error(id, message, reply))
                .await?,
        )
        .await
    }
}

struct Actor {
    path: PathBuf,
    file: Result<SwarmStoreSnapshot, SwarmFailure>,
    #[cfg(feature = "test-support")]
    fail_directory_sync: bool,
}
impl Actor {
    fn load(path: PathBuf, sessions: &[crate::store::session::SessionRecord]) -> Self {
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<SwarmStoreSnapshot>(&bytes)
                .map_err(|error| failure(SwarmErrorCode::Storage, format!("invalid swarm store: {error}")))
                .and_then(|mut file| {
                    if file.version != 1 { return Err(failure(SwarmErrorCode::Storage, "unsupported swarm store version")); }
                    for swarm in &mut file.swarms {
                        let mut uncertain = false;
                        for notification in &mut swarm.notifications {
                            if notification.state == SwarmDeliveryState::Dispatching {
                                notification.state = SwarmDeliveryState::Uncertain;
                                notification.error = Some("Host restarted before backend acceptance was confirmed; explicit retry required".into());
                                uncertain = true;
                            }
                        }
                        for member in &mut swarm.members {
                            let membership = SwarmMembership { swarm_id: swarm.id.clone(), member_id: member.spec.id.clone() };
                            let owned_sessions = sessions.iter().filter(|record| record.swarm_membership.as_ref() == Some(&membership)).collect::<Vec<_>>();
                            if member.session_id.is_none() {
                                match owned_sessions.as_slice() {
                                    [record] => member.session_id = Some(record.id.clone()),
                                    [] => {},
                                    _ => return Err(failure(SwarmErrorCode::Conflict, "Member has ambiguous owned session records; explicit storage repair required")),
                                }
                            }
                            if matches!(member.state, SwarmMemberState::Live | SwarmMemberState::Reserved | SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved) {
                                if matches!(member.state, SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved) {
                                    member.state = SwarmMemberState::Retired;
                                } else if member.state == SwarmMemberState::Reserved && member.session_id.is_none() {
                                    member.state = SwarmMemberState::Failed;
                                    member.error = Some("Host restarted during member activation; explicit retry required".into());
                                } else {
                                    member.state = if member.session_id.is_some() {
                                        SwarmMemberState::Dormant
                                    } else {
                                        SwarmMemberState::Proposed
                                    };
                                }
                            }
                            if member.state == SwarmMemberState::Proposed && member.session_id.is_some() {
                                member.state = SwarmMemberState::Dormant;
                            }
                            member.agent_id = None;
                            member.runtime_status = None;
                        }
                        if swarm.lifecycle == SwarmLifecycle::Pausing { swarm.lifecycle = SwarmLifecycle::Paused; }
                        if uncertain || !matches!(swarm.lifecycle, SwarmLifecycle::Paused | SwarmLifecycle::AttentionRequired) {
                            swarm.lifecycle = SwarmLifecycle::AttentionRequired;
                            swarm.recovery_requirement = SwarmRecoveryRequirement::ExplicitResume;
                            swarm.error = Some("Host restarted; review deliveries and Resume or explicitly retry uncertain work".into());
                        }
                    }
                    Ok(file)
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SwarmStoreSnapshot { version: 1, ..Default::default() }),
            Err(error) => Err(failure(SwarmErrorCode::Storage, format!("failed to read swarm store: {error}"))),
        };
        let mut actor = Self {
            path,
            file,
            #[cfg(feature = "test-support")]
            fail_directory_sync: false,
        };
        if actor.path.exists()
            && actor.file.is_ok()
            && let Err(error) = actor.persist_snapshot()
        {
            actor.file = Err(error);
        }
        actor
    }
    fn image_path(&self, image_id: &SwarmImageId) -> Result<PathBuf, SwarmFailure> {
        let id = Uuid::parse_str(&image_id.0)
            .map_err(|_| failure(SwarmErrorCode::Invalid, "Image identity must be a UUID"))?;
        Ok(self.path.with_extension("images").join(id.to_string()))
    }
    fn read_image(
        &self,
        swarm_id: &SwarmId,
        image_id: &SwarmImageId,
    ) -> Result<(SwarmImage, ImageData), SwarmFailure> {
        let file = self.file.as_ref().map_err(Clone::clone)?;
        let record = file
            .images
            .iter()
            .find(|record| record.swarm_id == *swarm_id && record.image.id == *image_id)
            .ok_or_else(|| {
                failure(
                    SwarmErrorCode::NotFound,
                    "Image does not belong to this swarm",
                )
            })?;
        let path = self.image_path(image_id)?;
        let metadata = std::fs::metadata(&path).map_err(|_| {
            failure(
                SwarmErrorCode::Storage,
                "Shared image is unavailable on the host",
            )
        })?;
        if !metadata.is_file()
            || metadata.len() != record.image.byte_len
            || metadata.len() > SWARM_MAX_IMAGE_BYTES as u64
        {
            return Err(failure(
                SwarmErrorCode::Storage,
                "Shared image size does not match its record",
            ));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| {
                file.take(SWARM_MAX_IMAGE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| failure(SwarmErrorCode::Storage, "Shared image cannot be read"))?;
        if bytes.len() as u64 != record.image.byte_len
            || format!("{:x}", Sha256::digest(&bytes)) != record.sha256
        {
            return Err(failure(
                SwarmErrorCode::Storage,
                "Shared image integrity check failed",
            ));
        }
        Ok((
            record.image.clone(),
            ImageData {
                media_type: record.image.media_type.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        ))
    }
    fn upload_image(
        &mut self,
        swarm_id: SwarmId,
        upload: SwarmImageUpload,
    ) -> Result<SwarmCommit<Vec<SwarmEventPayload>>, SwarmFailure> {
        let file = self.file.as_ref().map_err(Clone::clone)?;
        if !file.swarms.iter().any(|swarm| swarm.id == swarm_id) {
            return Err(failure(SwarmErrorCode::NotFound, "Swarm does not exist"));
        }
        let path = self.image_path(&upload.image_id)?;
        if upload.name.trim().is_empty()
            || upload.name.len() > 256
            || upload.name.chars().any(char::is_control)
        {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Image name must be 1–256 bytes without control characters",
            ));
        }
        if upload.data.data.len() > SWARM_MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Each image must be at most 4 MiB",
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&upload.data.data)
            .map_err(|_| failure(SwarmErrorCode::Invalid, "Image data is not valid base64"))?;
        if bytes.is_empty() || bytes.len() > SWARM_MAX_IMAGE_BYTES {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Each image must be nonempty and at most 4 MiB",
            ));
        }
        let format = match upload.data.media_type.as_str() {
            "image/png" => image::ImageFormat::Png,
            "image/jpeg" => image::ImageFormat::Jpeg,
            "image/gif" => image::ImageFormat::Gif,
            "image/webp" => image::ImageFormat::WebP,
            _ => {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "Supported images are PNG, JPEG, GIF, and WebP",
                ));
            }
        };
        if image::guess_format(&bytes).ok() != Some(format) {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Image bytes do not match their media type",
            ));
        }
        let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format)
            .into_dimensions()
            .map_err(|_| failure(SwarmErrorCode::Invalid, "Image header is invalid"))?;
        if width == 0
            || height == 0
            || width > SWARM_MAX_IMAGE_DIMENSION
            || height > SWARM_MAX_IMAGE_DIMENSION
            || u64::from(width) * u64::from(height) > SWARM_MAX_IMAGE_PIXELS
        {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Image dimensions exceed the shared-image limit",
            ));
        }
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(SWARM_MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(SWARM_MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(128 * 1024 * 1024);
        let mut reader = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format);
        reader.limits(limits);
        reader.decode().map_err(|_| {
            failure(
                SwarmErrorCode::Invalid,
                "Image cannot be decoded within the shared-image limits",
            )
        })?;
        let image = SwarmImage {
            id: upload.image_id,
            name: upload.name,
            media_type: upload.data.media_type,
            width,
            height,
            byte_len: bytes.len() as u64,
        };
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if let Some(previous) = file
            .images
            .iter()
            .find(|record| record.image.id == image.id)
        {
            if previous.swarm_id != swarm_id || previous.image != image || previous.sha256 != digest
            {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "Image identity reused with different content or ownership",
                ));
            }
            self.read_image(&swarm_id, &image.id)?;
        } else {
            let directory = path.parent().ok_or_else(|| {
                failure(SwarmErrorCode::Storage, "Image directory is unavailable")
            })?;
            std::fs::create_dir_all(directory).map_err(|_| {
                failure(
                    SwarmErrorCode::Storage,
                    "Cannot create shared-image directory",
                )
            })?;
            let mut temporary = tempfile::NamedTempFile::new_in(directory)
                .map_err(|_| failure(SwarmErrorCode::Storage, "Cannot stage shared image"))?;
            temporary
                .write_all(&bytes)
                .and_then(|_| temporary.as_file().sync_all())
                .map_err(|_| failure(SwarmErrorCode::Storage, "Cannot sync shared image"))?;
            // A prior interrupted upload may have published the bytes but not
            // the registry record. Never overwrite different bytes on retry.
            match temporary.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let mut existing = Vec::new();
                    std::fs::File::open(&path)
                        .and_then(|file| {
                            file.take(SWARM_MAX_IMAGE_BYTES as u64 + 1)
                                .read_to_end(&mut existing)
                        })
                        .map_err(|_| {
                            failure(
                                SwarmErrorCode::Storage,
                                "Cannot inspect interrupted image upload",
                            )
                        })?;
                    if existing != bytes {
                        return Err(failure(
                            SwarmErrorCode::Conflict,
                            "Interrupted image upload has different content",
                        ));
                    }
                }
                Err(_) => {
                    return Err(failure(
                        SwarmErrorCode::Storage,
                        "Cannot publish shared image",
                    ));
                }
            }
        }
        let record = SwarmStoredImage {
            swarm_id: swarm_id.clone(),
            image: image.clone(),
            sha256: digest,
        };
        self.transaction(move |file| {
            if !file
                .images
                .iter()
                .any(|previous| previous.image.id == record.image.id)
            {
                file.images.push(record);
            }
            Ok(vec![SwarmEventPayload::Image(SwarmImageNotifyPayload {
                swarm_id,
                image_id: image.id.clone(),
                outcome: SwarmImageOutcome::Ready { image, data: None },
            })])
        })
    }
    fn apply(
        &mut self,
        payload: SwarmCommandPayload,
    ) -> Result<SwarmCommit<Vec<SwarmEventPayload>>, SwarmFailure> {
        match payload {
            SwarmCommandPayload::UploadImage { swarm_id, image } => {
                let image_id = image.image_id.clone();
                match self.upload_image(swarm_id.clone(), image) {
                    Ok(commit) => Ok(commit),
                    Err(error) => {
                        tracing::warn!(code = ?error.code, "Shared image upload rejected");
                        Ok(SwarmCommit {
                            value: vec![SwarmEventPayload::Image(SwarmImageNotifyPayload {
                                swarm_id,
                                image_id,
                                outcome: SwarmImageOutcome::Failed { error },
                            })],
                            status: SwarmCommitStatus::Durable,
                        })
                    }
                }
            }
            SwarmCommandPayload::ReadImage { swarm_id, image_id } => {
                let outcome = match self.read_image(&swarm_id, &image_id) {
                    Ok((image, data)) => SwarmImageOutcome::Ready {
                        image,
                        data: Some(data),
                    },
                    Err(error) => SwarmImageOutcome::Failed { error },
                };
                Ok(SwarmCommit {
                    value: vec![SwarmEventPayload::Image(SwarmImageNotifyPayload {
                        swarm_id,
                        image_id,
                        outcome,
                    })],
                    status: SwarmCommitStatus::Durable,
                })
            }
            payload => self.transaction(|file| apply(file, payload)),
        }
    }
    fn snapshot(&self) -> Result<SwarmStoreSnapshot, SwarmFailure> {
        self.file.clone()
    }
    fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&mut SwarmStoreSnapshot) -> Result<T, SwarmFailure>,
    ) -> Result<SwarmCommit<T>, SwarmFailure> {
        let mut candidate = self.snapshot()?;
        let result = operation(&mut candidate)?;
        if self.file.as_ref().is_ok_and(|file| *file == candidate) {
            return Ok(SwarmCommit {
                value: result,
                status: candidate.commit_status,
            });
        }
        candidate.commit_status = SwarmCommitStatus::Durable;
        let status = self.persist(&candidate)?;
        Ok(SwarmCommit {
            value: result,
            status,
        })
    }
    fn persist_snapshot(&mut self) -> Result<SwarmCommitStatus, SwarmFailure> {
        let mut file = self.snapshot()?;
        file.commit_status = SwarmCommitStatus::Durable;
        self.persist(&file)
    }
    fn persist(
        &mut self,
        candidate: &SwarmStoreSnapshot,
    ) -> Result<SwarmCommitStatus, SwarmFailure> {
        let parent = self.path.parent().ok_or(failure(
            SwarmErrorCode::Storage,
            "swarm store has no parent",
        ))?;
        std::fs::create_dir_all(parent).map_err(|error| {
            failure(
                SwarmErrorCode::Storage,
                format!("failed to create swarm directory: {error}"),
            )
        })?;
        let bytes = serde_json::to_vec_pretty(&candidate).map_err(|error| {
            failure(
                SwarmErrorCode::Storage,
                format!("failed to serialize swarm store: {error}"),
            )
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            failure(
                SwarmErrorCode::Storage,
                format!("failed to stage swarm store: {error}"),
            )
        })?;
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|error| {
                failure(
                    SwarmErrorCode::Storage,
                    format!("failed to sync swarm store: {error}"),
                )
            })?;
        temporary.persist(&self.path).map_err(|error| {
            failure(
                SwarmErrorCode::Storage,
                format!("failed to publish swarm store: {error}"),
            )
        })?;
        let sync_result = std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .and_then(|()| {
                if candidate.images.is_empty() {
                    return Ok(());
                }
                std::fs::File::open(self.path.with_extension("images"))
                    .and_then(|directory| directory.sync_all())
            });
        #[cfg(feature = "test-support")]
        let sync_result = if std::mem::take(&mut self.fail_directory_sync) {
            Err(std::io::Error::other(
                "Injected post-rename directory sync failure",
            ))
        } else {
            sync_result
        };
        let status = match sync_result {
            Ok(()) => SwarmCommitStatus::Durable,
            Err(error) => {
                tracing::warn!(
                    "Swarm write committed; directory durability could not be confirmed"
                );
                SwarmCommitStatus::CommittedDurabilityUncertain {
                    message: format!(
                        "Swarm write committed, but directory durability could not be confirmed: {error}"
                    ),
                }
            }
        };
        let mut committed = candidate.clone();
        committed.commit_status = status.clone();
        if let SwarmCommitStatus::CommittedDurabilityUncertain { message } = &status {
            for swarm in &mut committed.swarms {
                swarm.recovery_requirement = SwarmRecoveryRequirement::ExplicitResume;
                if matches!(
                    swarm.lifecycle,
                    SwarmLifecycle::Running
                        | SwarmLifecycle::Launching
                        | SwarmLifecycle::Transitioning
                        | SwarmLifecycle::Pausing
                ) {
                    swarm.lifecycle = SwarmLifecycle::AttentionRequired;
                }
                swarm.error = Some(format!(
                    "{message} New execution is withheld until explicit Resume."
                ));
            }
        }
        self.file = Ok(committed);
        Ok(status)
    }
}
pub(crate) fn commit_warning(status: SwarmCommitStatus) -> Option<SwarmEventPayload> {
    match status {
        SwarmCommitStatus::Durable => None,
        SwarmCommitStatus::CommittedDurabilityUncertain { message } => {
            Some(SwarmEventPayload::Error(SwarmErrorNotifyPayload {
                publication_id: None,
                swarm_id: None,
                draft_id: None,
                code: SwarmErrorCode::CommittedDurabilityUncertain,
                message,
            }))
        }
    }
}
fn fresh() -> String {
    Uuid::new_v4().to_string()
}
fn swarm_event(swarm: &Swarm) -> SwarmEventPayload {
    SwarmEventPayload::Swarm(Box::new(SwarmNotifyPayload {
        swarm: swarm.clone(),
    }))
}
fn draft_event(draft: &SwarmDraft) -> SwarmEventPayload {
    SwarmEventPayload::Draft(SwarmDraftNotifyPayload::Upsert {
        draft: Box::new(draft.clone()),
    })
}
fn swarm_mut<'a>(
    file: &'a mut SwarmStoreSnapshot,
    id: &SwarmId,
) -> Result<&'a mut Swarm, SwarmFailure> {
    file.swarms
        .iter_mut()
        .find(|swarm| swarm.id == *id)
        .ok_or_else(|| failure(SwarmErrorCode::NotFound, "swarm does not exist"))
}
fn member_mut<'a>(
    swarm: &'a mut Swarm,
    id: &SwarmMemberId,
) -> Result<&'a mut SwarmMember, SwarmFailure> {
    swarm
        .members
        .iter_mut()
        .find(|member| member.spec.id == *id)
        .ok_or_else(|| failure(SwarmErrorCode::NotFound, "member does not belong to swarm"))
}
fn draft_mut<'a>(
    file: &'a mut SwarmStoreSnapshot,
    id: &SwarmDraftId,
    revision: u64,
) -> Result<&'a mut SwarmDraft, SwarmFailure> {
    let draft = file
        .drafts
        .iter_mut()
        .find(|draft| draft.id == *id)
        .ok_or(failure(SwarmErrorCode::NotFound, "draft does not exist"))?;
    if draft.revision != revision {
        return Err(failure(SwarmErrorCode::Conflict, "stale draft revision"));
    }
    Ok(draft)
}
fn validate_constraints(constraints: &SwarmConstraints) -> Result<(), SwarmFailure> {
    if !(1..=SWARM_MAX_LIVE_AGENTS).contains(&constraints.max_live_agents) {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "live-agent limit must be 1–16",
        ));
    }
    if !(1..=SWARM_MAX_AGENT_WAKE_BUDGET).contains(&constraints.agent_wake_budget) {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "agent wake budget must be 1–128",
        ));
    }
    if constraints.allocations.is_empty()
        || constraints
            .allocations
            .iter()
            .any(|allocation| allocation.count == 0 || allocation.count > SWARM_MAX_LIVE_AGENTS)
    {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "choose positive backend allocations",
        ));
    }
    if constraints
        .allocations
        .iter()
        .map(|allocation| allocation.count)
        .sum::<u32>()
        > constraints.max_live_agents
    {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "backend allocations exceed live-agent limit",
        ));
    }
    let mut selections = Vec::new();
    for allocation in &constraints.allocations {
        let selection = (&allocation.launch_profile_id, &allocation.session_settings);
        if selections.contains(&selection) {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Duplicate backend/model allocation",
            ));
        }
        selections.push(selection);
    }
    if constraints.shared_guidance.len() > 65536 {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "shared guidance exceeds 64 KiB",
        ));
    }
    Ok(())
}
fn lineup(
    constraints: &SwarmConstraints,
    previous: &[SwarmMemberSpec],
) -> (Vec<SwarmMemberSpec>, Vec<String>) {
    let mut members = previous
        .iter()
        .filter(|member| member.pinned)
        .cloned()
        .collect::<Vec<_>>();
    let mut conflicts = Vec::new();
    let mut used = vec![0usize; constraints.allocations.len()];
    for member in &members {
        let exact = constraints
            .allocations
            .iter()
            .enumerate()
            .find(|(index, allocation)| {
                allocation.backend_kind == member.backend_kind
                    && allocation.launch_profile_id == member.launch_profile_id
                    && allocation.session_settings == member.session_settings
                    && used[*index] < allocation.count as usize
            });
        let permitted = exact.or_else(|| {
            constraints
                .allocations
                .iter()
                .enumerate()
                .find(|(index, allocation)| {
                    allocation.backend_kind == member.backend_kind
                        && allocation.launch_profile_id == member.launch_profile_id
                        && used[*index] < allocation.count as usize
                })
        });
        match permitted {
            Some((index, _)) if member.project_id == constraints.project_id => {
                used[index] += 1;
            }
            _ => conflicts.push(format!(
                "Pinned member {} conflicts with scope or backend capacity",
                member.name
            )),
        }
    }
    for (allocation_index, allocation) in constraints.allocations.iter().enumerate() {
        for _ in used[allocation_index]..allocation.count as usize {
            let retained = previous.iter().find(|member| {
                !member.pinned
                    && member.project_id == constraints.project_id
                    && member.launch_profile_id == allocation.launch_profile_id
                    && member.session_settings == allocation.session_settings
                    && !members.iter().any(|chosen| chosen.id == member.id)
            });
            let spec = match retained {
                Some(member) => member.clone(),
                None => SwarmMemberSpec {
                    id: SwarmMemberId(fresh()),
                    name: format!("Peer {}", members.len() + 1),
                    focus: None,
                    backend_kind: allocation.backend_kind,
                    launch_profile_id: allocation.launch_profile_id.clone(),
                    project_id: constraints.project_id.clone(),
                    session_settings: allocation.session_settings.clone(),
                    pinned: false,
                },
            };
            members.push(spec);
        }
    }
    if members.len() > constraints.max_live_agents as usize {
        conflicts.push("Pinned lineup exceeds total live-agent capacity".into());
    }
    (members, conflicts)
}

fn apply(
    file: &mut SwarmStoreSnapshot,
    payload: SwarmCommandPayload,
) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
    match payload {
        SwarmCommandPayload::UploadImage { .. } | SwarmCommandPayload::ReadImage { .. } => {
            Err(failure(
                SwarmErrorCode::Invalid,
                "Image commands require the media store",
            ))
        }
        SwarmCommandPayload::GenerateDraft {
            draft_id,
            expected_revision,
            name,
            opening_brief,
            constraints,
        } => {
            validate_constraints(&constraints)?;
            if name.trim().is_empty() || name.len() > 256 {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "swarm name must be 1–256 bytes",
                ));
            }
            if opening_brief.len() > 65536 {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "opening brief exceeds 64 KiB",
                ));
            }
            let previous = match expected_revision {
                Some(revision) => draft_mut(file, &draft_id, revision)?.clone(),
                None => {
                    if draft_id.0.trim().is_empty()
                        || file.drafts.iter().any(|draft| draft.id == draft_id)
                    {
                        return Err(failure(
                            SwarmErrorCode::Conflict,
                            "draft identity is empty or already exists",
                        ));
                    }
                    SwarmDraft {
                        legacy_source: None,
                        id: draft_id,
                        retained_sessions: Default::default(),
                        revision: 0,
                        name: String::new(),
                        opening_brief: String::new(),
                        constraints: constraints.clone(),
                        members: Vec::new(),
                        conflicts: Vec::new(),
                        generation: SwarmDraftGeneration::DeterministicGeneralists,
                        legacy_team_id: None,
                    }
                }
            };
            let (members, mut conflicts) = lineup(&constraints, &previous.members);
            if previous.legacy_team_id.is_some() {
                conflicts.extend(previous.conflicts.clone());
            }
            let draft = SwarmDraft {
                name,
                opening_brief,
                constraints,
                members,
                conflicts,
                revision: previous.revision + 1,
                ..previous
            };
            file.drafts.retain(|item| item.id != draft.id);
            file.drafts.push(draft.clone());
            Ok(vec![draft_event(&draft)])
        }
        SwarmCommandPayload::EditDraftMember {
            draft_id,
            expected_revision,
            member,
        } => {
            if member.name.trim().is_empty()
                || member.name.len() > 256
                || member
                    .focus
                    .as_ref()
                    .is_some_and(|focus| focus.len() > 65536)
            {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "member name/focus is invalid",
                ));
            }
            let draft = draft_mut(file, &draft_id, expected_revision)?;
            let previous = draft
                .members
                .iter_mut()
                .find(|item| item.id == member.id)
                .ok_or(failure(
                    SwarmErrorCode::NotFound,
                    "draft member does not exist",
                ))?;
            *previous = member;
            let (members, conflicts) = lineup(&draft.constraints, &draft.members);
            draft.members = members;
            draft.conflicts = conflicts;
            draft.revision += 1;
            Ok(vec![draft_event(draft)])
        }
        SwarmCommandPayload::Launch {
            draft_id,
            expected_revision,
        }
        | SwarmCommandPayload::ApplyMigration {
            draft_id,
            expected_revision,
        } => {
            let draft = draft_mut(file, &draft_id, expected_revision)?.clone();
            if !draft.conflicts.is_empty() {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "resolve draft conflicts before launch",
                ));
            }
            let has_opening_post = !draft.opening_brief.trim().is_empty();
            if file.swarms.iter().any(|swarm| swarm.id.0 == draft.id.0) {
                return Err(failure(SwarmErrorCode::Conflict, "draft already launched"));
            }
            let swarm = Swarm {
                recovery_requirement: SwarmRecoveryRequirement::None,
                source_draft_id: Some(draft.id.clone()),
                id: SwarmId(draft.id.0.clone()),
                host_id: HostFilterId(LOCAL_HOST_ID.into()),
                name: draft.name.clone(),
                revision: 1,
                constraints: draft.constraints.clone(),
                lifecycle: if has_opening_post {
                    SwarmLifecycle::Launching
                } else {
                    SwarmLifecycle::Running
                },
                members: draft
                    .members
                    .iter()
                    .cloned()
                    .map(|spec| SwarmMember {
                        state: SwarmMemberState::Proposed,
                        agent_id: None,
                        session_id: draft.retained_sessions.get(&spec.id).cloned(),
                        runtime_status: None,
                        context_cursor: 0,
                        current_round_id: None,
                        error: None,
                        spec,
                    })
                    .collect(),
                opening_post_id: None,
                board_positions: [SwarmBoard::Briefing, SwarmBoard::Coordination]
                    .into_iter()
                    .map(|board| SwarmBoardPosition {
                        board,
                        high_water: 0,
                        human_read_cursor: 0,
                        unread_count: 0,
                    })
                    .collect(),
                notifications: Vec::new(),
                rounds: Vec::new(),
                change_preview_revision: 0,
                change_preview: None,
                error: None,
                legacy_team_id: draft.legacy_team_id.clone(),
            };
            if let Some(team_id) = draft.legacy_team_id.as_ref() {
                // Session ownership was stored in the migration snapshot before conversion.
                if let Some(migration) = file
                    .swarms
                    .iter()
                    .find(|item| item.legacy_team_id.as_ref() == Some(team_id))
                {
                    return Err(failure(
                        SwarmErrorCode::Conflict,
                        format!("team already converted to swarm {}", migration.name),
                    ));
                }
            }
            let id = swarm.id.clone();
            file.swarms.push(swarm);
            let mut events = vec![SwarmEventPayload::Draft(SwarmDraftNotifyPayload::Delete {
                draft_id: draft_id.clone(),
            })];
            if has_opening_post {
                let outcome = publish(
                    file,
                    &id,
                    SwarmAuthor::Human,
                    SwarmPublication {
                        images: Vec::new(),
                        board: SwarmBoard::Briefing,
                        publication_id: SwarmPublicationId("opening-brief".into()),
                        body: vec![SwarmBodySegment::Text {
                            text: draft.opening_brief,
                        }],
                        thread_id: None,
                        attachments: Vec::new(),
                    },
                )?;
                swarm_mut(file, &id)?.opening_post_id = Some(outcome.post.id.clone());
                events.push(SwarmEventPayload::Post(SwarmPostNotifyPayload {
                    post: outcome.post,
                }));
            }
            events.push(swarm_event(swarm_mut(file, &id)?));
            file.drafts.retain(|draft| draft.id != draft_id);
            Ok(events)
        }
        SwarmCommandPayload::ReadBoard { swarm_id, query } => {
            Ok(vec![SwarmEventPayload::Board(SwarmBoardNotifyPayload {
                page: read_board(file, &swarm_id, query)?,
            })])
        }
        SwarmCommandPayload::ReadPost { swarm_id, post_id } => {
            let post = file
                .posts
                .iter()
                .find(|post| post.swarm_id == swarm_id && post.id == post_id)
                .ok_or(failure(
                    SwarmErrorCode::NotFound,
                    "Linked post does not belong to swarm",
                ))?;
            let page = read_thread(
                file,
                &swarm_id,
                SwarmThreadRead {
                    thread_id: post.thread_id.clone(),
                    after_cursor: Some(SwarmReadCursor {
                        swarm_id: swarm_id.clone(),
                        target: SwarmCursorTarget::Thread {
                            thread_id: post.thread_id.clone(),
                        },
                        position: post.cursor.saturating_sub(1),
                        snapshot_high_water: file
                            .posts
                            .iter()
                            .filter(|item| {
                                item.swarm_id == swarm_id && item.thread_id == post.thread_id
                            })
                            .map(|item| item.cursor)
                            .max()
                            .ok_or(failure(SwarmErrorCode::NotFound, "Thread has no root"))?,
                    }),
                    limit: None,
                },
            )?;
            Ok(vec![SwarmEventPayload::Thread(SwarmThreadNotifyPayload {
                page,
            })])
        }
        SwarmCommandPayload::ReadThread { swarm_id, query } => {
            Ok(vec![SwarmEventPayload::Thread(SwarmThreadNotifyPayload {
                page: read_thread(file, &swarm_id, query)?,
            })])
        }
        SwarmCommandPayload::Post {
            swarm_id,
            publication,
        } => {
            let outcome = publish(file, &swarm_id, SwarmAuthor::Human, publication)?;
            Ok(vec![
                SwarmEventPayload::Post(SwarmPostNotifyPayload { post: outcome.post }),
                swarm_event(swarm_mut(file, &swarm_id)?),
            ])
        }
        SwarmCommandPayload::MarkRead {
            swarm_id,
            board,
            cursor,
        } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            let position = swarm
                .board_positions
                .iter_mut()
                .find(|position| position.board == board)
                .ok_or(failure(SwarmErrorCode::Invalid, "missing board position"))?;
            if cursor > position.high_water {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "read cursor exceeds board high-water mark",
                ));
            }
            position.human_read_cursor = position.human_read_cursor.max(cursor);
            let read_cursor = position.human_read_cursor;
            let unread = file
                .posts
                .iter()
                .filter(|post| {
                    post.swarm_id == swarm_id && post.board == board && post.cursor > read_cursor
                })
                .count() as u64;
            let swarm = swarm_mut(file, &swarm_id)?;
            let position = swarm
                .board_positions
                .iter_mut()
                .find(|position| position.board == board)
                .ok_or(failure(SwarmErrorCode::Invalid, "missing board position"))?;
            position.unread_count = unread;
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::Pause { swarm_id } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            swarm.lifecycle = if swarm.members.iter().any(|member| {
                matches!(
                    member.state,
                    SwarmMemberState::Reserved | SwarmMemberState::RetiringReserved
                ) || member.runtime_status.is_some_and(|status| {
                    !matches!(
                        status,
                        AgentControlStatus::Idle | AgentControlStatus::Failed
                    )
                })
            }) {
                SwarmLifecycle::Pausing
            } else {
                SwarmLifecycle::Paused
            };
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::Resume { swarm_id } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            validate_constraints(&swarm.constraints)?;
            if swarm
                .notifications
                .iter()
                .any(|notification| notification.state == SwarmDeliveryState::Uncertain)
            {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "explicitly retry uncertain deliveries before resuming",
                ));
            }
            for round in &mut swarm.rounds {
                if round.agent_activations_remaining == 0 {
                    round.agent_activations_remaining = swarm.constraints.agent_wake_budget;
                }
            }
            swarm.lifecycle = SwarmLifecycle::Running;
            swarm.recovery_requirement = SwarmRecoveryRequirement::None;
            swarm.error = None;
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::RetryMember {
            swarm_id,
            member_id,
        } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            let member = member_mut(swarm, &member_id)?;
            if matches!(
                member.state,
                SwarmMemberState::Retired
                    | SwarmMemberState::Retiring
                    | SwarmMemberState::RetiringReserved
            ) {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "retired members cannot retry",
                ));
            }
            if member.state == SwarmMemberState::Failed {
                member.state = if member.agent_id.is_some() {
                    SwarmMemberState::Live
                } else if member.session_id.is_some() {
                    SwarmMemberState::Dormant
                } else {
                    SwarmMemberState::Proposed
                };
            }
            member.error = None;
            for notification in &mut swarm.notifications {
                if notification.member_id == member_id
                    && matches!(
                        notification.state,
                        SwarmDeliveryState::Uncertain | SwarmDeliveryState::Failed
                    )
                {
                    notification.state = SwarmDeliveryState::Pending;
                    notification.error = None;
                }
            }
            if swarm.lifecycle == SwarmLifecycle::AttentionRequired
                && swarm.recovery_requirement == SwarmRecoveryRequirement::None
                && !swarm
                    .notifications
                    .iter()
                    .any(|notification| notification.state == SwarmDeliveryState::Uncertain)
                && swarm
                    .rounds
                    .iter()
                    .all(|round| round.agent_activations_remaining > 0)
            {
                swarm.lifecycle = SwarmLifecycle::Running;
                swarm.error = None;
            }
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::PreviewChange {
            swarm_id,
            expected_revision,
            constraints,
        } => {
            validate_constraints(&constraints)?;
            let swarm = swarm_mut(file, &swarm_id)?;
            if swarm.revision != expected_revision {
                return Err(failure(SwarmErrorCode::Conflict, "stale swarm revision"));
            }
            let previous = swarm
                .members
                .iter()
                .filter(|member| {
                    !matches!(
                        member.state,
                        SwarmMemberState::Retiring
                            | SwarmMemberState::RetiringReserved
                            | SwarmMemberState::Retired
                    )
                })
                .map(|member| {
                    let mut spec = member.spec.clone();
                    spec.pinned = false;
                    spec
                })
                .collect::<Vec<_>>();
            let (lineup, mut conflicts) = lineup(&constraints, &previous);
            if constraints.project_id != swarm.constraints.project_id
                || constraints.workspace_policy != swarm.constraints.workspace_policy
            {
                conflicts.push("Project/workspace policy cannot change for existing sessions; create a separate swarm".into());
            }
            let retained = lineup
                .iter()
                .filter(|member| previous.iter().any(|old| old.id == member.id))
                .map(|member| member.id.clone())
                .collect::<Vec<_>>();
            let additions = lineup
                .into_iter()
                .filter(|member| !retained.contains(&member.id))
                .collect();
            let retirements = previous
                .iter()
                .filter(|member| !retained.contains(&member.id))
                .map(|member| member.id.clone())
                .collect();
            let revision = swarm
                .change_preview_revision
                .max(
                    swarm
                        .change_preview
                        .as_ref()
                        .map_or(0, |preview| preview.revision),
                )
                .checked_add(1)
                .ok_or(failure(
                    SwarmErrorCode::Conflict,
                    "Preview revision exhausted",
                ))?;
            swarm.change_preview_revision = revision;
            swarm.change_preview = Some(SwarmChangePreview {
                revision,
                base_revision: swarm.revision,
                constraints,
                retained,
                additions,
                retirements,
                conflicts,
            });
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::ApplyChange {
            swarm_id,
            preview_revision,
            ..
        } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            let preview = swarm
                .change_preview
                .clone()
                .ok_or(failure(SwarmErrorCode::Conflict, "no change preview"))?;
            if preview.revision != preview_revision || preview.base_revision != swarm.revision {
                return Err(failure(SwarmErrorCode::Conflict, "stale change preview"));
            }
            if !preview.conflicts.is_empty() {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "resolve change preview conflicts",
                ));
            }
            for id in &preview.retirements {
                let member = member_mut(swarm, id)?;
                member.state = if member.agent_id.is_some() {
                    if member.state == SwarmMemberState::Reserved {
                        SwarmMemberState::RetiringReserved
                    } else {
                        SwarmMemberState::Retiring
                    }
                } else {
                    SwarmMemberState::Retired
                };
                for notification in &mut swarm.notifications {
                    if notification.member_id == *id
                        && notification.state == SwarmDeliveryState::Pending
                    {
                        notification.state = SwarmDeliveryState::Undeliverable;
                        notification.error = Some("Member retired before delivery".into());
                    }
                }
            }
            let addition_round = if preview.additions.is_empty() || swarm.opening_post_id.is_none()
            {
                None
            } else {
                let id = SwarmRoundId(fresh());
                swarm.rounds.push(SwarmRound {
                    id: id.clone(),
                    agent_activations_remaining: preview.constraints.agent_wake_budget,
                });
                Some(id)
            };
            for spec in preview.additions {
                let member_id = spec.id.clone();
                swarm.members.push(SwarmMember {
                    spec,
                    state: SwarmMemberState::Proposed,
                    agent_id: None,
                    session_id: None,
                    runtime_status: None,
                    context_cursor: 0,
                    current_round_id: None,
                    error: None,
                });
                if let (Some(post_id), Some(round_id)) =
                    (swarm.opening_post_id.clone(), addition_round.as_ref())
                {
                    swarm.notifications.push(SwarmNotification {
                        id: SwarmNotificationId(fresh()),
                        member_id,
                        post_ids: vec![post_id],
                        round_id: round_id.clone(),
                        state: SwarmDeliveryState::Pending,
                        error: None,
                    });
                }
            }
            swarm.constraints = preview.constraints;
            swarm.revision += 1;
            swarm.change_preview = None;
            if !matches!(
                swarm.lifecycle,
                SwarmLifecycle::Paused
                    | SwarmLifecycle::Pausing
                    | SwarmLifecycle::AttentionRequired
            ) {
                swarm.lifecycle = if has_pending_members(swarm) {
                    SwarmLifecycle::Transitioning
                } else {
                    SwarmLifecycle::Running
                };
            }
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::DiscardDraft { draft_id } => {
            if !file.drafts.iter().any(|draft| draft.id == draft_id) {
                return Err(failure(SwarmErrorCode::NotFound, "draft does not exist"));
            }
            file.drafts.retain(|draft| draft.id != draft_id);
            Ok(vec![SwarmEventPayload::Draft(
                SwarmDraftNotifyPayload::Delete { draft_id },
            )])
        }
        SwarmCommandPayload::DiscardChangePreview { swarm_id } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            swarm.change_preview = None;
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::RetryNotification {
            swarm_id,
            notification_id,
        } => {
            let swarm = swarm_mut(file, &swarm_id)?;
            let index = swarm
                .notifications
                .iter()
                .position(|notification| notification.id == notification_id)
                .ok_or(failure(
                    SwarmErrorCode::NotFound,
                    "notification does not belong to swarm",
                ))?;
            if !matches!(
                swarm.notifications[index].state,
                SwarmDeliveryState::Uncertain | SwarmDeliveryState::Failed
            ) {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "only failed/uncertain delivery can retry",
                ));
            }
            let member_id = swarm.notifications[index].member_id.clone();
            let member = member_mut(swarm, &member_id)?;
            if matches!(
                member.state,
                SwarmMemberState::Retiring
                    | SwarmMemberState::RetiringReserved
                    | SwarmMemberState::Retired
            ) {
                return Err(failure(
                    SwarmErrorCode::Conflict,
                    "retired recipient cannot retry",
                ));
            }
            if member.state == SwarmMemberState::Failed {
                member.state = if member.agent_id.is_some() {
                    SwarmMemberState::Live
                } else if member.session_id.is_some() {
                    SwarmMemberState::Dormant
                } else {
                    SwarmMemberState::Proposed
                };
            }
            member.error = None;
            swarm.notifications[index].state = SwarmDeliveryState::Pending;
            swarm.notifications[index].error = None;
            Ok(vec![swarm_event(swarm)])
        }
        SwarmCommandPayload::PreviewMigration { .. } => Err(failure(
            SwarmErrorCode::Invalid,
            "migration must be prepared with legacy registry ownership",
        )),
    }
}

fn publish(
    file: &mut SwarmStoreSnapshot,
    id: &SwarmId,
    author: SwarmAuthor,
    publication: SwarmPublication,
) -> Result<SwarmPublicationOutcome, SwarmFailure> {
    if publication.publication_id.0.trim().is_empty() || publication.publication_id.0.len() > 256 {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "publication identity must be 1–256 bytes",
        ));
    }
    let swarm = file
        .swarms
        .iter()
        .find(|swarm| swarm.id == *id)
        .ok_or(failure(SwarmErrorCode::NotFound, "swarm does not exist"))?;
    if let SwarmAuthor::Member { member_id } = &author {
        let member = swarm
            .members
            .iter()
            .find(|member| member.spec.id == *member_id)
            .ok_or(failure(
                SwarmErrorCode::Unauthorized,
                "author does not belong to swarm",
            ))?;
        if !matches!(
            member.state,
            SwarmMemberState::Live
                | SwarmMemberState::Reserved
                | SwarmMemberState::Retiring
                | SwarmMemberState::RetiringReserved
        ) {
            return Err(failure(
                SwarmErrorCode::Unauthorized,
                "inactive member cannot publish",
            ));
        }
    }
    if let Some(post) = file.posts.iter().find(|post| {
        post.swarm_id == *id
            && post.author == author
            && post.publication_id == publication.publication_id
    }) {
        let same_thread = match &publication.thread_id {
            Some(thread) => post.thread_id == *thread,
            None => post.thread_id.0 == post.id.0,
        };
        if post.board != publication.board
            || post.body != publication.body
            || post.attachments != publication.attachments
            || post
                .images
                .iter()
                .map(|image| &image.id)
                .collect::<Vec<_>>()
                != publication.images.iter().collect::<Vec<_>>()
            || !same_thread
        {
            return Err(failure(
                SwarmErrorCode::Conflict,
                "publication identity reused with different content",
            ));
        }
        return Ok(SwarmPublicationOutcome {
            commit_status: SwarmCommitStatus::Durable,
            post: post.clone(),
            duplicate: true,
            deliveries: swarm
                .notifications
                .iter()
                .filter(|notification| notification.post_ids.contains(&post.id))
                .cloned()
                .collect(),
        });
    }
    let body_bytes = serde_json::to_vec(&publication.body).map_err(|error| {
        failure(
            SwarmErrorCode::Invalid,
            format!("body cannot serialize: {error}"),
        )
    })?;
    if body_bytes.len() > SWARM_MAX_BODY_BYTES
        || publication.attachments.len() + publication.images.len() > SWARM_MAX_ATTACHMENTS
    {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "body must be <=64 KiB, attachments and images combined <=16",
        ));
    }
    if !publication.body.iter().any(
        |segment| !matches!(segment, SwarmBodySegment::Text { text } if text.trim().is_empty()),
    ) && publication.images.is_empty()
    {
        return Err(failure(SwarmErrorCode::Invalid, "empty post body"));
    }
    let mut images = Vec::new();
    for image_id in &publication.images {
        if images
            .iter()
            .any(|image: &SwarmImage| image.id == *image_id)
        {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Duplicate image reference",
            ));
        }
        let record = file
            .images
            .iter()
            .find(|record| record.swarm_id == *id && record.image.id == *image_id)
            .ok_or_else(|| {
                failure(
                    SwarmErrorCode::Unauthorized,
                    "Image does not belong to this swarm",
                )
            })?;
        images.push(record.image.clone());
    }
    let root = match publication.thread_id.as_ref() {
        Some(thread) => Some(
            file.posts
                .iter()
                .find(|post| {
                    post.swarm_id == *id && post.id.0 == thread.0 && post.thread_id.0 == post.id.0
                })
                .ok_or(failure(
                    SwarmErrorCode::NotFound,
                    "thread does not belong to swarm",
                ))?,
        ),
        None => None,
    };
    if root.is_some_and(|post| post.board != publication.board) {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "reply board does not match thread",
        ));
    }
    for segment in &publication.body {
        match segment {
            SwarmBodySegment::Text { .. } => {}
            SwarmBodySegment::MemberMention { member_id } => {
                if !swarm
                    .members
                    .iter()
                    .any(|member| member.spec.id == *member_id)
                {
                    return Err(failure(
                        SwarmErrorCode::Unauthorized,
                        "mention does not belong to swarm",
                    ));
                }
            }
            SwarmBodySegment::PostLink { post_id } => {
                if !file
                    .posts
                    .iter()
                    .any(|post| post.swarm_id == *id && post.id == *post_id)
                {
                    return Err(failure(
                        SwarmErrorCode::Unauthorized,
                        "linked post does not belong to swarm",
                    ));
                }
            }
        }
    }
    let recipients = swarm_publication_recipients(
        swarm,
        &author,
        publication.board,
        root.map(|post| &post.author),
        &publication.body,
    );
    let new_round = matches!(author, SwarmAuthor::Human);
    let round_id = match &author {
        SwarmAuthor::Human => SwarmRoundId(fresh()),
        SwarmAuthor::Member { member_id } => swarm
            .members
            .iter()
            .find(|member| member.spec.id == *member_id)
            .and_then(|member| member.current_round_id.clone())
            .ok_or(failure(
                SwarmErrorCode::Conflict,
                "member has no delivered causal round",
            ))?,
    };
    let cursor = file
        .posts
        .iter()
        .filter(|post| post.swarm_id == *id)
        .map(|post| post.cursor)
        .max()
        .unwrap_or(0)
        + 1;
    let post_id = SwarmPostId(fresh());
    let post = SwarmPost {
        images,
        id: post_id.clone(),
        swarm_id: id.clone(),
        thread_id: publication
            .thread_id
            .unwrap_or_else(|| SwarmThreadId(post_id.0.clone())),
        board: publication.board,
        cursor,
        author,
        publication_id: publication.publication_id,
        body: publication.body,
        attachments: publication.attachments,
        round_id: round_id.clone(),
        created_at_ms: now_ms(),
    };
    if serde_json::to_vec(&post)
        .map_err(|error| {
            failure(
                SwarmErrorCode::Invalid,
                format!("Cannot encode post: {error}"),
            )
        })?
        .len()
        > SWARM_MAX_POST_BYTES
    {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "Post exceeds serialized byte limit",
        ));
    }
    file.posts.push(post.clone());
    let swarm = swarm_mut(file, id)?;
    if new_round {
        swarm.rounds.push(SwarmRound {
            id: round_id.clone(),
            agent_activations_remaining: swarm.constraints.agent_wake_budget,
        });
    }
    for recipient in recipients {
        let member = swarm
            .members
            .iter()
            .find(|member| member.spec.id == recipient)
            .ok_or(failure(
                SwarmErrorCode::NotFound,
                "recipient does not exist",
            ))?;
        let undeliverable = matches!(
            member.state,
            SwarmMemberState::Retired
                | SwarmMemberState::Retiring
                | SwarmMemberState::RetiringReserved
        );
        swarm.notifications.push(SwarmNotification {
            id: SwarmNotificationId(fresh()),
            member_id: recipient,
            post_ids: vec![post.id.clone()],
            round_id: round_id.clone(),
            state: if undeliverable {
                SwarmDeliveryState::Undeliverable
            } else {
                SwarmDeliveryState::Pending
            },
            error: undeliverable.then(|| "Member retired; notification cannot be delivered".into()),
        });
    }
    let position = swarm
        .board_positions
        .iter_mut()
        .find(|position| position.board == post.board)
        .ok_or(failure(SwarmErrorCode::Invalid, "missing board position"))?;
    position.high_water = cursor;
    position.unread_count += 1;
    Ok(SwarmPublicationOutcome {
        commit_status: SwarmCommitStatus::Durable,
        post: post.clone(),
        duplicate: false,
        deliveries: swarm
            .notifications
            .iter()
            .filter(|notification| notification.post_ids.contains(&post.id))
            .cloned()
            .collect(),
    })
}
fn page_limit(limit: Option<u32>) -> Result<usize, SwarmFailure> {
    match limit {
        None => Ok(SWARM_DEFAULT_PAGE_LIMIT as usize),
        Some(value) if (1..=SWARM_MAX_PAGE_LIMIT).contains(&value) => Ok(value as usize),
        Some(_) => Err(failure(SwarmErrorCode::Invalid, "page limit must be 1–100")),
    }
}
fn page_cursor(
    id: &SwarmId,
    target: SwarmCursorTarget,
    supplied: Option<SwarmReadCursor>,
    actual_high_water: u64,
) -> Result<SwarmReadCursor, SwarmFailure> {
    match supplied {
        Some(cursor) => {
            if cursor.swarm_id != *id || cursor.target != target {
                return Err(failure(
                    SwarmErrorCode::Unauthorized,
                    "Cursor belongs to a different swarm or board/thread",
                ));
            }
            if cursor.position > cursor.snapshot_high_water
                || cursor.snapshot_high_water > actual_high_water
            {
                return Err(failure(
                    SwarmErrorCode::Invalid,
                    "Cursor position/snapshot exceeds scope high-water mark",
                ));
            }
            Ok(cursor)
        }
        None => Ok(SwarmReadCursor {
            swarm_id: id.clone(),
            target,
            position: 0,
            snapshot_high_water: actual_high_water,
        }),
    }
}
fn bounded_posts(
    activity: &[&SwarmPost],
    limit: usize,
    mut bytes: usize,
) -> Result<(Vec<SwarmPost>, bool), SwarmFailure> {
    let mut posts = Vec::new();
    for post in activity.iter().take(limit) {
        let size = serde_json::to_vec(post)
            .map_err(|error| {
                failure(
                    SwarmErrorCode::Storage,
                    format!("Cannot encode stored post: {error}"),
                )
            })?
            .len()
            + 1;
        if bytes + size > SWARM_MAX_READ_PAGE_BYTES {
            if posts.is_empty() {
                return Err(failure(
                    SwarmErrorCode::Storage,
                    "Stored post exceeds read page byte budget",
                ));
            }
            break;
        }
        bytes += size;
        posts.push((*post).clone());
    }
    let has_more = posts.len() < activity.len();
    Ok((posts, has_more))
}
pub(crate) fn read_board(
    file: &SwarmStoreSnapshot,
    id: &SwarmId,
    query: SwarmBoardRead,
) -> Result<SwarmBoardPage, SwarmFailure> {
    let swarm = file
        .swarms
        .iter()
        .find(|swarm| swarm.id == *id)
        .ok_or(failure(SwarmErrorCode::NotFound, "swarm does not exist"))?;
    let actual = swarm
        .board_positions
        .iter()
        .find(|position| position.board == query.board)
        .ok_or(failure(SwarmErrorCode::Invalid, "missing board position"))?
        .high_water;
    let mut cursor = page_cursor(
        id,
        match query.view {
            SwarmBoardView::Posts => SwarmCursorTarget::Board { board: query.board },
            SwarmBoardView::Threads => SwarmCursorTarget::BoardThreads { board: query.board },
        },
        query.after_cursor,
        actual,
    )?;
    let limit = page_limit(query.limit)?;
    let mut activity = file
        .posts
        .iter()
        .filter(|post| {
            post.swarm_id == *id
                && post.board == query.board
                && post.cursor <= cursor.snapshot_high_water
                && match query.view {
                    SwarmBoardView::Posts => post.cursor > cursor.position,
                    SwarmBoardView::Threads => post.id.0 == post.thread_id.0,
                }
        })
        .collect::<Vec<_>>();
    if query.view == SwarmBoardView::Threads {
        activity.sort_by_key(|post| std::cmp::Reverse((post.created_at_ms, post.cursor)));
        let offset = usize::try_from(cursor.position).map_err(|_| {
            failure(
                SwarmErrorCode::Invalid,
                "Thread-list cursor offset exceeds platform bounds",
            )
        })?;
        if offset > activity.len() {
            return Err(failure(
                SwarmErrorCode::Invalid,
                "Thread-list cursor exceeds the snapshot's root count",
            ));
        }
        activity.drain(..offset);
    }
    let (posts, has_more) = bounded_posts(&activity, limit, SWARM_READ_PAGE_CONTAINER_BYTES)?;
    cursor.position = match query.view {
        SwarmBoardView::Posts => posts
            .last()
            .map(|post| post.cursor)
            .unwrap_or(cursor.snapshot_high_water),
        SwarmBoardView::Threads => cursor.position + posts.len() as u64,
    };
    Ok(SwarmBoardPage {
        swarm_id: id.clone(),
        board: query.board,
        posts,
        high_water: cursor.snapshot_high_water,
        next_cursor: cursor,
        has_more,
    })
}
pub(crate) fn read_thread(
    file: &SwarmStoreSnapshot,
    id: &SwarmId,
    query: SwarmThreadRead,
) -> Result<SwarmThreadPage, SwarmFailure> {
    let root = file
        .posts
        .iter()
        .find(|post| {
            post.swarm_id == *id && post.id.0 == query.thread_id.0 && post.thread_id.0 == post.id.0
        })
        .ok_or(failure(
            SwarmErrorCode::NotFound,
            "thread does not belong to swarm",
        ))?
        .clone();
    let actual = file
        .posts
        .iter()
        .filter(|post| post.swarm_id == *id && post.thread_id == query.thread_id)
        .map(|post| post.cursor)
        .max()
        .ok_or(failure(SwarmErrorCode::NotFound, "thread has no root"))?;
    let mut cursor = page_cursor(
        id,
        SwarmCursorTarget::Thread {
            thread_id: query.thread_id.clone(),
        },
        query.after_cursor,
        actual,
    )?;
    if cursor.snapshot_high_water < root.cursor {
        return Err(failure(
            SwarmErrorCode::Invalid,
            "Thread snapshot cannot predate its root",
        ));
    }
    let limit = page_limit(query.limit)?;
    let activity = file
        .posts
        .iter()
        .filter(|post| {
            post.swarm_id == *id
                && post.thread_id == query.thread_id
                && post.id != root.id
                && post.cursor > cursor.position
                && post.cursor <= cursor.snapshot_high_water
        })
        .collect::<Vec<_>>();
    let root_bytes = serde_json::to_vec(&root)
        .map_err(|error| {
            failure(
                SwarmErrorCode::Storage,
                format!("Cannot encode thread root: {error}"),
            )
        })?
        .len();
    let (posts, has_more) = bounded_posts(
        &activity,
        limit,
        SWARM_READ_PAGE_CONTAINER_BYTES + root_bytes,
    )?;
    cursor.position = posts
        .last()
        .map(|post| post.cursor)
        .unwrap_or(cursor.snapshot_high_water);
    Ok(SwarmThreadPage {
        swarm_id: id.clone(),
        thread_id: query.thread_id,
        root,
        posts,
        high_water: cursor.snapshot_high_water,
        next_cursor: cursor,
        has_more,
    })
}

fn reserve(
    file: &mut SwarmStoreSnapshot,
    eligible_live_agents: &[AgentId],
) -> Result<DispatchReservation, SwarmFailure> {
    let mut batches = Vec::new();
    let mut events = Vec::new();
    for swarm in &mut file.swarms {
        if swarm.recovery_requirement != SwarmRecoveryRequirement::None
            || !matches!(
                swarm.lifecycle,
                SwarmLifecycle::Running | SwarmLifecycle::Launching | SwarmLifecycle::Transitioning
            )
        {
            continue;
        }
        let mut changed = false;
        let mut occupancy = swarm
            .members
            .iter()
            .filter(|member| occupies_slot(member))
            .count();
        for index in 0..swarm.members.len() {
            let member = &swarm.members[index];
            if !matches!(
                member.state,
                SwarmMemberState::Proposed | SwarmMemberState::Dormant | SwarmMemberState::Live
            ) || member
                .runtime_status
                .is_some_and(|status| status != AgentControlStatus::Idle)
            {
                continue;
            }
            let starting = member.agent_id.is_none();
            if member
                .agent_id
                .as_ref()
                .is_some_and(|agent| !eligible_live_agents.contains(agent))
            {
                continue;
            }
            if starting && occupancy >= swarm.constraints.max_live_agents as usize {
                continue;
            }
            if starting {
                let capacity = swarm
                    .constraints
                    .allocations
                    .iter()
                    .filter(|allocation| allocation.backend_kind == member.spec.backend_kind)
                    .map(|allocation| allocation.count as usize)
                    .sum::<usize>();
                let used = swarm
                    .members
                    .iter()
                    .filter(|peer| {
                        peer.spec.backend_kind == member.spec.backend_kind && occupies_slot(peer)
                    })
                    .count();
                if used >= capacity {
                    continue;
                }
            }
            let member_id = member.spec.id.clone();
            let Some(round_id) = swarm
                .notifications
                .iter()
                .find(|notification| {
                    notification.member_id == member_id
                        && notification.state == SwarmDeliveryState::Pending
                })
                .map(|notification| notification.round_id.clone())
            else {
                continue;
            };
            let notifications = swarm
                .notifications
                .iter()
                .filter(|notification| {
                    notification.member_id == member_id
                        && notification.state == SwarmDeliveryState::Pending
                        && notification.round_id == round_id
                })
                .take(SWARM_MAX_PAGE_LIMIT as usize)
                .cloned()
                .collect::<Vec<_>>();
            let mut rounds = std::collections::HashSet::new();
            for notification in &notifications {
                let agent_triggered = notification.post_ids.iter().any(|id| {
                    file.posts.iter().any(|post| {
                        post.id == *id && matches!(post.author, SwarmAuthor::Member { .. })
                    })
                });
                if agent_triggered {
                    rounds.insert(notification.round_id.clone());
                }
            }
            if rounds.iter().any(|id| {
                swarm
                    .rounds
                    .iter()
                    .find(|round| round.id == *id)
                    .is_none_or(|round| round.agent_activations_remaining == 0)
            }) {
                changed = true;
                swarm.lifecycle = SwarmLifecycle::AttentionRequired;
                swarm.error = Some("Agent wake budget exhausted; posts remain stored. Review and explicitly Resume to renew the allowance".into());
                break;
            }
            for round in &mut swarm.rounds {
                if rounds.contains(&round.id) {
                    round.agent_activations_remaining -= 1;
                }
            }
            let notification_ids = notifications
                .iter()
                .map(|notification| notification.id.clone())
                .collect::<Vec<_>>();
            for notification in &mut swarm.notifications {
                if notification_ids.contains(&notification.id) {
                    notification.state = SwarmDeliveryState::Dispatching;
                }
            }
            changed = true;
            let member = &mut swarm.members[index];
            member.state = SwarmMemberState::Reserved;
            if starting {
                occupancy += 1;
            }
            let previous_round_id = member.current_round_id.replace(round_id);
            let mut context_bytes = 2usize;
            let mut posts = Vec::new();
            for post in file
                .posts
                .iter()
                .filter(|post| post.swarm_id == swarm.id && post.cursor > member.context_cursor)
                .take(SWARM_MAX_PAGE_LIMIT as usize)
            {
                let size = serde_json::to_vec(post)
                    .map_err(|error| {
                        failure(
                            SwarmErrorCode::Storage,
                            format!("Cannot encode inline swarm post: {error}"),
                        )
                    })?
                    .len()
                    + usize::from(!posts.is_empty());
                if size > SWARM_MAX_INLINE_CONTEXT_BYTES - context_bytes {
                    break;
                }
                context_bytes += size;
                posts.push(post.clone());
            }
            let notification_post_ids = notifications
                .iter()
                .flat_map(|notification| notification.post_ids.clone())
                .collect();
            batches.push(SwarmDispatch {
                previous_round_id,
                agent_activation_charged: !rounds.is_empty(),
                notification_post_ids,
                swarm_id: swarm.id.clone(),
                member: member.clone(),
                constraints: swarm.constraints.clone(),
                notification_ids,
                posts,
            });
        }
        if changed {
            events.push(swarm_event(swarm));
        }
    }
    Ok((batches, events))
}
fn complete(
    file: &mut SwarmStoreSnapshot,
    batch: SwarmDispatch,
    result: Result<(AgentId, SessionId), SwarmFailure>,
) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
    let swarm = swarm_mut(file, &batch.swarm_id)?;
    let member = member_mut(swarm, &batch.member.spec.id)?;
    #[cfg(feature = "test-support")]
    tracing::warn!(
        member_state = ?member.state,
        prior_error_present = member.error.is_some(),
        accepted = result.is_ok(),
        error_code = ?result.as_ref().err().map(|error| error.code),
        "Swarm failure ordering diagnostic: reconciling native admission"
    );
    if member.state == SwarmMemberState::RetiringReserved {
        member.state = SwarmMemberState::Retiring;
    }
    let error = match result {
        Ok((agent, session)) => {
            member.session_id = Some(session);
            if member.agent_id.as_ref() == Some(&agent)
                && member.state == SwarmMemberState::Reserved
            {
                member.state = SwarmMemberState::Live;
                member.error = None;
            }
            member.context_cursor = batch
                .posts
                .last()
                .map(|post| post.cursor)
                .unwrap_or(member.context_cursor);
            None
        }
        Err(error) => {
            if !matches!(
                member.state,
                SwarmMemberState::Retiring
                    | SwarmMemberState::RetiringReserved
                    | SwarmMemberState::Retired
            ) {
                member.state = SwarmMemberState::Failed;
            }
            member.error = Some(error.message.clone());
            Some(error)
        }
    };
    for notification in &mut swarm.notifications {
        if batch.notification_ids.contains(&notification.id) {
            notification.state = if error.is_some() {
                SwarmDeliveryState::Failed
            } else {
                SwarmDeliveryState::Accepted
            };
            notification.error = error.as_ref().map(|error| error.message.clone());
        }
    }
    if let Some(error) = error {
        swarm.error = Some(error.message);
        if !matches!(
            swarm.lifecycle,
            SwarmLifecycle::Paused | SwarmLifecycle::Pausing
        ) {
            swarm.lifecycle = SwarmLifecycle::AttentionRequired;
        }
    } else if matches!(
        swarm.lifecycle,
        SwarmLifecycle::Launching | SwarmLifecycle::Transitioning
    ) && !has_pending_members(swarm)
    {
        swarm.lifecycle = SwarmLifecycle::Running;
    }
    Ok(vec![swarm_event(swarm)])
}
fn record_status(
    file: &mut SwarmStoreSnapshot,
    agent: &AgentId,
    status: AgentControlStatus,
    session: Option<SessionId>,
    terminated: bool,
    failure: Option<SwarmFailure>,
) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
    let mut events = Vec::new();
    for swarm in &mut file.swarms {
        let can_settle = (swarm.lifecycle == SwarmLifecycle::Pausing
            && swarm.members.iter().all(|member| {
                !matches!(
                    member.state,
                    SwarmMemberState::Reserved | SwarmMemberState::RetiringReserved
                ) && member.runtime_status.is_none_or(|status| {
                    matches!(
                        status,
                        AgentControlStatus::Idle | AgentControlStatus::Failed
                    )
                })
            }))
            || (swarm.lifecycle == SwarmLifecycle::Transitioning && !has_pending_members(swarm));
        let Some(member) = swarm
            .members
            .iter_mut()
            .find(|member| member.agent_id.as_ref() == Some(agent))
        else {
            continue;
        };
        if !terminated
            && !can_settle
            && member.runtime_status == Some(status)
            && session
                .as_ref()
                .is_none_or(|id| member.session_id.as_ref() == Some(id))
        {
            continue;
        }
        member.runtime_status = Some(status);
        if let Some(session) = session {
            member.session_id = Some(session);
        }
        if terminated {
            #[cfg(feature = "test-support")]
            tracing::warn!(
                member_state = ?member.state,
                terminal_cause_present = failure.is_some(),
                prior_error_present = member.error.is_some(),
                "Swarm failure ordering diagnostic: recording native termination"
            );
            member.agent_id = None;
            member.runtime_status = None;
            if matches!(
                member.state,
                SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved
            ) {
                member.state = SwarmMemberState::Retired;
            } else {
                member.state = SwarmMemberState::Failed;
                match failure {
                    Some(failure) => member.error = Some(failure.message),
                    None => {
                        member.error.get_or_insert_with(|| {
                            "Member agent terminated; explicit retry retains its session".into()
                        });
                    }
                }
            }
        }
        if swarm.lifecycle == SwarmLifecycle::Pausing
            && swarm.members.iter().all(|member| {
                !matches!(
                    member.state,
                    SwarmMemberState::Reserved | SwarmMemberState::RetiringReserved
                ) && member.runtime_status.is_none_or(|status| {
                    matches!(
                        status,
                        AgentControlStatus::Idle | AgentControlStatus::Failed
                    )
                })
            })
        {
            swarm.lifecycle = SwarmLifecycle::Paused;
        }
        if swarm.lifecycle == SwarmLifecycle::Transitioning && !has_pending_members(swarm) {
            swarm.lifecycle = SwarmLifecycle::Running;
        }
        events.push(swarm_event(swarm));
        break;
    }
    Ok(events)
}
fn migration(
    file: &mut SwarmStoreSnapshot,
    mut draft: SwarmDraft,
) -> Result<Vec<SwarmEventPayload>, SwarmFailure> {
    if let Some(previous) = file.drafts.iter().find(|previous| previous.id == draft.id) {
        draft.revision = previous.revision + 1;
    }
    file.drafts.retain(|previous| previous.id != draft.id);
    file.drafts.push(draft.clone());
    Ok(vec![draft_event(&draft)])
}

fn failure(code: SwarmErrorCode, message: impl Into<String>) -> SwarmFailure {
    SwarmFailure {
        code,
        message: message.into(),
    }
}

fn has_pending_members(swarm: &Swarm) -> bool {
    swarm.members.iter().any(|member| match member.state {
        SwarmMemberState::Reserved
        | SwarmMemberState::Retiring
        | SwarmMemberState::RetiringReserved => true,
        SwarmMemberState::Proposed => swarm.notifications.iter().any(|notification| {
            notification.member_id == member.spec.id
                && matches!(
                    notification.state,
                    SwarmDeliveryState::Pending | SwarmDeliveryState::Dispatching
                )
        }),
        SwarmMemberState::Live
        | SwarmMemberState::Dormant
        | SwarmMemberState::Failed
        | SwarmMemberState::Retired => false,
    })
}

fn occupies_slot(member: &SwarmMember) -> bool {
    member.agent_id.is_some()
        || matches!(
            member.state,
            SwarmMemberState::Reserved
                | SwarmMemberState::Retiring
                | SwarmMemberState::RetiringReserved
        )
}
