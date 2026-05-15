use std::collections::HashMap;

use protocol::{
    AgentControlStatus, AgentId, SessionId, Team, TeamCreatePayload, TeamDeletePayload, TeamMember,
    TeamMemberBindingNotifyPayload, TeamMemberBindingPayload, TeamMemberCreatePayload,
    TeamMemberDeletePayload, TeamMemberId, TeamMemberNotifyPayload, TeamMemberRole,
    TeamMemberState, TeamMemberUpdatePayload, TeamNotifyPayload, TeamRenamePayload,
    TeamSetManagerPayload,
};
use tokio::sync::{mpsc, oneshot};

use crate::agent::now_ms;
use crate::store::agent_teams::{AgentTeamValidationRefs, AgentTeamsStore};

const ACTIVATION_RESERVATION_TIMEOUT_MS: u64 = 35_000;

#[derive(Clone)]
pub(crate) struct TeamRegistryHandle {
    tx: mpsc::Sender<TeamRegistryCommand>,
}

#[derive(Debug, Clone)]
pub(crate) struct TeamRegistrySnapshot {
    pub teams: Vec<Team>,
    pub members: Vec<TeamMember>,
    pub bindings: Vec<TeamMemberBindingPayload>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TeamRegistryEvents {
    pub team_notifies: Vec<TeamNotifyPayload>,
    pub member_notifies: Vec<TeamMemberNotifyPayload>,
    pub binding_notifies: Vec<TeamMemberBindingNotifyPayload>,
}

#[derive(Debug, Clone)]
pub(crate) struct TeamDescribeData {
    pub team: Team,
    pub members: Vec<TeamMember>,
    pub bindings: Vec<TeamMemberBindingPayload>,
}

#[derive(Debug, Clone)]
pub(crate) struct TeamMessagePlan {
    pub team: Team,
    pub member: TeamMember,
    pub activation: TeamMemberActivation,
}

#[derive(Debug, Clone)]
pub(crate) enum TeamMemberActivation {
    Reuse { agent_id: AgentId },
    Resume { session_id: SessionId },
    New,
}

enum TeamRegistryCommand {
    Snapshot {
        reply: oneshot::Sender<Result<TeamRegistrySnapshot, String>>,
    },
    DescribeForAgent {
        agent_id: AgentId,
        reply: oneshot::Sender<Result<TeamDescribeData, String>>,
    },
    PlanMessageMember {
        caller_agent_id: AgentId,
        target_member_id: TeamMemberId,
        reply: oneshot::Sender<Result<TeamMessagePlan, String>>,
    },
    PlanUserActivation {
        target_member_id: TeamMemberId,
        reserve: bool,
        reply: oneshot::Sender<Result<TeamMessagePlan, String>>,
    },
    BindMemberAgent {
        member_id: TeamMemberId,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    RecordBindingFailure {
        member_id: TeamMemberId,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    RecordResumeFailure {
        member_id: TeamMemberId,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    RecordMemberActivity {
        member_id: TeamMemberId,
        status: AgentControlStatus,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    RecordAgentActivity {
        agent_id: AgentId,
        status: AgentControlStatus,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    ClearBindingByAgent {
        agent_id: AgentId,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    CreateTeam {
        payload: TeamCreatePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    RenameTeam {
        payload: TeamRenamePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    DeleteTeam {
        payload: TeamDeletePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    SetManager {
        payload: TeamSetManagerPayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    CreateMember {
        payload: TeamMemberCreatePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    UpdateMember {
        payload: TeamMemberUpdatePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    DeleteMember {
        payload: TeamMemberDeletePayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
}

struct TeamRegistryActor {
    store: AgentTeamsStore,
    bindings: Vec<TeamMemberBindingPayload>,
    pending_activations: HashMap<TeamMemberId, u64>,
}

impl TeamRegistryHandle {
    pub(crate) fn spawn(store: AgentTeamsStore) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let bindings = store
            .members()
            .into_iter()
            .map(|member| TeamMemberBindingPayload {
                member_id: member.id,
                current_agent_id: None,
                status: AgentControlStatus::Idle,
                last_active_at_ms: None,
            })
            .collect();
        let actor = TeamRegistryActor {
            store,
            bindings,
            pending_activations: HashMap::new(),
        };
        spawn_team_registry_actor(actor, rx);
        Self { tx }
    }

    pub(crate) async fn snapshot(&self) -> Result<TeamRegistrySnapshot, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(TeamRegistryCommand::Snapshot { reply })
            .await
            .map_err(|_| "team registry actor stopped".to_string())?;
        rx.await
            .map_err(|_| "team registry actor dropped snapshot reply".to_string())?
    }

    pub(crate) async fn describe_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<TeamDescribeData, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(TeamRegistryCommand::DescribeForAgent { agent_id, reply })
            .await
            .map_err(|_| "team registry actor stopped".to_string())?;
        rx.await
            .map_err(|_| "team registry actor dropped describe reply".to_string())?
    }

    pub(crate) async fn plan_message_member(
        &self,
        caller_agent_id: AgentId,
        target_member_id: TeamMemberId,
    ) -> Result<TeamMessagePlan, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(TeamRegistryCommand::PlanMessageMember {
                caller_agent_id,
                target_member_id,
                reply,
            })
            .await
            .map_err(|_| "team registry actor stopped".to_string())?;
        rx.await
            .map_err(|_| "team registry actor dropped message plan reply".to_string())?
    }

    /// Resolve a member for a user-initiated activation. Unlike
    /// `plan_message_member`, there is no caller agent — the user is the
    /// caller, and no manager-only authorization applies. `reserve` controls
    /// whether the activation slot is reserved (used for the spawn-now case,
    /// `prompt: Some`). For "open the chat tab without spawning" (`prompt:
    /// None`) the caller passes `false` so no reservation is held.
    pub(crate) async fn plan_user_activation(
        &self,
        target_member_id: TeamMemberId,
        reserve: bool,
    ) -> Result<TeamMessagePlan, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(TeamRegistryCommand::PlanUserActivation {
                target_member_id,
                reserve,
                reply,
            })
            .await
            .map_err(|_| "team registry actor stopped".to_string())?;
        rx.await
            .map_err(|_| "team registry actor dropped activation plan reply".to_string())?
    }

    pub(crate) async fn bind_member_agent(
        &self,
        member_id: TeamMemberId,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::BindMemberAgent {
            member_id,
            agent_id,
            session_id,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn record_binding_failure(
        &self,
        member_id: TeamMemberId,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RecordBindingFailure { member_id, reply })
            .await
    }

    pub(crate) async fn record_resume_failure(
        &self,
        member_id: TeamMemberId,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RecordResumeFailure {
            member_id,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn record_member_activity(
        &self,
        member_id: TeamMemberId,
        status: AgentControlStatus,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RecordMemberActivity {
            member_id,
            status,
            reply,
        })
        .await
    }

    pub(crate) async fn record_agent_activity(
        &self,
        agent_id: AgentId,
        status: AgentControlStatus,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RecordAgentActivity {
            agent_id,
            status,
            reply,
        })
        .await
    }

    pub(crate) async fn clear_binding_by_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::ClearBindingByAgent { agent_id, reply })
            .await
    }

    pub(crate) async fn create_team(
        &self,
        payload: TeamCreatePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::CreateTeam {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn rename_team(
        &self,
        payload: TeamRenamePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RenameTeam {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn delete_team(
        &self,
        payload: TeamDeletePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::DeleteTeam {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn set_manager(
        &self,
        payload: TeamSetManagerPayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::SetManager {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn create_member(
        &self,
        payload: TeamMemberCreatePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::CreateMember {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn update_member(
        &self,
        payload: TeamMemberUpdatePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::UpdateMember {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn delete_member(
        &self,
        payload: TeamMemberDeletePayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::DeleteMember {
            payload,
            refs,
            reply,
        })
        .await
    }

    async fn mutate<F>(&self, build: F) -> Result<TeamRegistryEvents, String>
    where
        F: FnOnce(oneshot::Sender<Result<TeamRegistryEvents, String>>) -> TeamRegistryCommand,
    {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(build(reply))
            .await
            .map_err(|_| "team registry actor stopped".to_string())?;
        rx.await
            .map_err(|_| "team registry actor dropped mutation reply".to_string())?
    }
}

impl TeamRegistryActor {
    async fn run(mut self, mut rx: mpsc::Receiver<TeamRegistryCommand>) {
        while let Some(command) = rx.recv().await {
            match command {
                TeamRegistryCommand::Snapshot { reply } => {
                    let _ = reply.send(Ok(self.snapshot()));
                }
                TeamRegistryCommand::DescribeForAgent { agent_id, reply } => {
                    let result = self.describe_for_agent(&agent_id);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::PlanMessageMember {
                    caller_agent_id,
                    target_member_id,
                    reply,
                } => {
                    let result = self.plan_message_member(&caller_agent_id, &target_member_id);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::PlanUserActivation {
                    target_member_id,
                    reserve,
                    reply,
                } => {
                    let result = self.plan_user_activation(&target_member_id, reserve);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::BindMemberAgent {
                    member_id,
                    agent_id,
                    session_id,
                    refs,
                    reply,
                } => {
                    let result = self.bind_member_agent(member_id, agent_id, session_id, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::RecordBindingFailure { member_id, reply } => {
                    let result = self.record_binding_failure(&member_id, None);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::RecordResumeFailure {
                    member_id,
                    refs,
                    reply,
                } => {
                    let result = self.record_binding_failure(&member_id, Some(&refs));
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::RecordMemberActivity {
                    member_id,
                    status,
                    reply,
                } => {
                    let result = self.record_member_activity(&member_id, status);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::RecordAgentActivity {
                    agent_id,
                    status,
                    reply,
                } => {
                    let result = self.record_agent_activity(&agent_id, status);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::ClearBindingByAgent { agent_id, reply } => {
                    let result = self.clear_binding_by_agent(&agent_id);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::CreateTeam {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.create_team(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::RenameTeam {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.rename_team(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::DeleteTeam {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.delete_team(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::SetManager {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.set_manager(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::CreateMember {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.create_member(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::UpdateMember {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.update_member(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::DeleteMember {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.delete_member(payload, &refs);
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn snapshot(&self) -> TeamRegistrySnapshot {
        TeamRegistrySnapshot {
            teams: self.store.teams(),
            members: self.store.members(),
            bindings: self.bindings.clone(),
        }
    }

    fn describe_for_agent(&self, agent_id: &AgentId) -> Result<TeamDescribeData, String> {
        let caller = self
            .member_for_agent(agent_id)?
            .ok_or_else(|| format!("caller agent {agent_id} is not a team member"))?;
        let team = self.store.get_team(&caller.team_id).ok_or_else(|| {
            format!(
                "caller member {} references missing team {}",
                caller.id, caller.team_id
            )
        })?;
        let members = self.store.members_for_team(&team.id);
        let member_ids = members
            .iter()
            .map(|member| member.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let bindings = self
            .bindings
            .iter()
            .filter(|binding| member_ids.contains(&binding.member_id))
            .cloned()
            .collect();
        Ok(TeamDescribeData {
            team,
            members,
            bindings,
        })
    }

    fn plan_message_member(
        &mut self,
        caller_agent_id: &AgentId,
        target_member_id: &TeamMemberId,
    ) -> Result<TeamMessagePlan, String> {
        self.expire_pending_activations();
        let caller = self.member_for_agent(caller_agent_id)?.ok_or_else(|| {
            format!("authorization: caller agent {caller_agent_id} is not a team member")
        })?;
        let team = self.store.get_team(&caller.team_id).ok_or_else(|| {
            format!(
                "caller member {} references missing team {}",
                caller.id, caller.team_id
            )
        })?;
        if caller.role != TeamMemberRole::Manager
            || caller.state != TeamMemberState::Active
            || team.manager_member_id != caller.id
        {
            return Err(format!(
                "authorization: caller member {} is not the active manager for team {}",
                caller.id, team.id
            ));
        }
        let target = self
            .store
            .get_member(target_member_id)
            .ok_or_else(|| format!("target team member {target_member_id} does not exist"))?;
        if target.team_id != team.id {
            return Err(format!(
                "authorization: target member {} does not belong to caller team {}",
                target.id, team.id
            ));
        }
        if target.role != TeamMemberRole::Report || target.state != TeamMemberState::Active {
            return Err(format!(
                "target member {} must be an active report",
                target.id
            ));
        }
        let activation = match self
            .bindings
            .iter()
            .find(|binding| binding.member_id == target.id)
            .and_then(|binding| binding.current_agent_id.clone())
        {
            Some(agent_id) => TeamMemberActivation::Reuse { agent_id },
            None => match target.session_id.clone() {
                Some(session_id) => TeamMemberActivation::Resume { session_id },
                None => TeamMemberActivation::New,
            },
        };
        match &activation {
            TeamMemberActivation::Reuse { .. } => {
                self.pending_activations.remove(&target.id);
            }
            TeamMemberActivation::Resume { .. } | TeamMemberActivation::New => {
                if self.pending_activations.contains_key(&target.id) {
                    return Err(format!(
                        "conflict: team member {} activation is already in progress",
                        target.id
                    ));
                }
                self.pending_activations.insert(target.id.clone(), now_ms());
            }
        }
        Ok(TeamMessagePlan {
            team,
            member: target,
            activation,
        })
    }

    fn plan_user_activation(
        &mut self,
        target_member_id: &TeamMemberId,
        reserve: bool,
    ) -> Result<TeamMessagePlan, String> {
        self.expire_pending_activations();
        let target = self
            .store
            .get_member(target_member_id)
            .ok_or_else(|| format!("target team member {target_member_id} does not exist"))?;
        if target.state != TeamMemberState::Active {
            return Err(format!(
                "team member {} is not Active ({:?})",
                target.id, target.state
            ));
        }
        let team = self.store.get_team(&target.team_id).ok_or_else(|| {
            format!(
                "team member {} references missing team {}",
                target.id, target.team_id
            )
        })?;
        let activation = match self
            .bindings
            .iter()
            .find(|binding| binding.member_id == target.id)
            .and_then(|binding| binding.current_agent_id.clone())
        {
            Some(agent_id) => TeamMemberActivation::Reuse { agent_id },
            None => match target.session_id.clone() {
                Some(session_id) => TeamMemberActivation::Resume { session_id },
                None => TeamMemberActivation::New,
            },
        };
        if reserve {
            match &activation {
                TeamMemberActivation::Reuse { .. } => {
                    self.pending_activations.remove(&target.id);
                }
                TeamMemberActivation::Resume { .. } | TeamMemberActivation::New => {
                    if self.pending_activations.contains_key(&target.id) {
                        return Err(format!(
                            "conflict: team member {} activation is already in progress",
                            target.id
                        ));
                    }
                    self.pending_activations.insert(target.id.clone(), now_ms());
                }
            }
        }
        Ok(TeamMessagePlan {
            team,
            member: target,
            activation,
        })
    }

    fn bind_member_agent(
        &mut self,
        member_id: TeamMemberId,
        agent_id: AgentId,
        session_id: Option<SessionId>,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.pending_activations.remove(&member_id);
        let mut events = TeamRegistryEvents::default();
        if let Some(session_id) = session_id {
            let member = self.store.get_member(&member_id).ok_or_else(|| {
                format!("cannot bind missing team member {member_id} to agent {agent_id}")
            })?;
            match member.session_id.as_ref() {
                Some(existing) if existing != &session_id => {
                    return Err(format!(
                        "team member {member_id} session_id {existing} does not match agent session {session_id}"
                    ));
                }
                Some(_) => {}
                None => {
                    let member = self
                        .store
                        .set_member_session_id(&member_id, session_id, refs)?;
                    events
                        .member_notifies
                        .push(TeamMemberNotifyPayload::Upsert { member });
                }
            }
        }

        let binding =
            self.upsert_binding(member_id, Some(agent_id), AgentControlStatus::Thinking)?;
        events
            .binding_notifies
            .push(TeamMemberBindingNotifyPayload::Upsert { binding });
        Ok(events)
    }

    fn record_binding_failure(
        &mut self,
        member_id: &TeamMemberId,
        clear_session_refs: Option<&AgentTeamValidationRefs>,
    ) -> Result<TeamRegistryEvents, String> {
        self.pending_activations.remove(member_id);
        let mut events = TeamRegistryEvents::default();
        if let Some(refs) = clear_session_refs
            && let Some(member) = self.store.clear_member_session_id(member_id, refs)?
        {
            events
                .member_notifies
                .push(TeamMemberNotifyPayload::Upsert { member });
        }
        let binding = self.upsert_binding(member_id.clone(), None, AgentControlStatus::Failed)?;
        events
            .binding_notifies
            .push(TeamMemberBindingNotifyPayload::Upsert { binding });
        Ok(events)
    }

    fn record_member_activity(
        &mut self,
        member_id: &TeamMemberId,
        status: AgentControlStatus,
    ) -> Result<TeamRegistryEvents, String> {
        let Some(current_agent_id) = self
            .bindings
            .iter()
            .find(|binding| binding.member_id == *member_id)
            .and_then(|binding| binding.current_agent_id.clone())
        else {
            return Err(format!("team member {member_id} has no live binding"));
        };
        let binding = self.upsert_binding(member_id.clone(), Some(current_agent_id), status)?;
        Ok(TeamRegistryEvents {
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        })
    }

    fn record_agent_activity(
        &mut self,
        agent_id: &AgentId,
        status: AgentControlStatus,
    ) -> Result<TeamRegistryEvents, String> {
        let Some(member_id) = self
            .bindings
            .iter()
            .find(|binding| binding.current_agent_id.as_ref() == Some(agent_id))
            .map(|binding| binding.member_id.clone())
        else {
            return Ok(TeamRegistryEvents::default());
        };
        let binding = self.upsert_binding(member_id, Some(agent_id.clone()), status)?;
        Ok(TeamRegistryEvents {
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        })
    }

    fn clear_binding_by_agent(&mut self, agent_id: &AgentId) -> Result<TeamRegistryEvents, String> {
        let Some(member_id) = self
            .bindings
            .iter()
            .find(|binding| binding.current_agent_id.as_ref() == Some(agent_id))
            .map(|binding| binding.member_id.clone())
        else {
            return Ok(TeamRegistryEvents::default());
        };
        self.pending_activations.remove(&member_id);
        let binding = self.upsert_binding(member_id, None, AgentControlStatus::Idle)?;
        Ok(TeamRegistryEvents {
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        })
    }

    fn create_team(
        &mut self,
        payload: TeamCreatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let (team, manager) = self.store.create_team(payload, refs)?;
        let binding = self.ensure_binding_payload(&manager.id)?;
        Ok(TeamRegistryEvents {
            team_notifies: vec![TeamNotifyPayload::Upsert { team }],
            member_notifies: vec![TeamMemberNotifyPayload::Upsert { member: manager }],
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
        })
    }

    fn rename_team(
        &mut self,
        payload: TeamRenamePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let team = self.store.rename_team(payload, refs)?;
        Ok(TeamRegistryEvents {
            team_notifies: vec![TeamNotifyPayload::Upsert { team }],
            ..TeamRegistryEvents::default()
        })
    }

    fn delete_team(
        &mut self,
        payload: TeamDeletePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let (team, members) = self.store.delete_team(&payload.id, refs)?;
        let mut member_notifies = Vec::new();
        let mut binding_notifies = Vec::new();
        for member in members {
            self.pending_activations.remove(&member.id);
            if let Some(binding) = self.remove_binding_payload(&member.id) {
                binding_notifies.push(TeamMemberBindingNotifyPayload::Delete { binding });
            }
            member_notifies.push(TeamMemberNotifyPayload::Delete { member });
        }
        Ok(TeamRegistryEvents {
            team_notifies: vec![TeamNotifyPayload::Delete { team }],
            member_notifies,
            binding_notifies,
        })
    }

    fn set_manager(
        &mut self,
        payload: TeamSetManagerPayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let (team, old_manager, new_manager) = self.store.set_manager(payload, refs)?;
        Ok(TeamRegistryEvents {
            team_notifies: vec![TeamNotifyPayload::Upsert { team }],
            member_notifies: vec![
                TeamMemberNotifyPayload::Upsert {
                    member: old_manager,
                },
                TeamMemberNotifyPayload::Upsert {
                    member: new_manager,
                },
            ],
            ..TeamRegistryEvents::default()
        })
    }

    fn create_member(
        &mut self,
        payload: TeamMemberCreatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let member = self.store.create_member(payload, refs)?;
        let binding = self.ensure_binding_payload(&member.id)?;
        Ok(TeamRegistryEvents {
            member_notifies: vec![TeamMemberNotifyPayload::Upsert { member }],
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        })
    }

    fn update_member(
        &mut self,
        payload: TeamMemberUpdatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let member = self.store.update_member(payload, refs)?;
        Ok(TeamRegistryEvents {
            member_notifies: vec![TeamMemberNotifyPayload::Upsert { member }],
            ..TeamRegistryEvents::default()
        })
    }

    fn delete_member(
        &mut self,
        payload: TeamMemberDeletePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        if self
            .bindings
            .iter()
            .any(|binding| binding.member_id == payload.id && binding.current_agent_id.is_some())
        {
            return Err(format!(
                "cannot delete live-bound team member {}",
                payload.id
            ));
        }
        let member = self.store.delete_member(payload, refs)?;
        self.pending_activations.remove(&member.id);
        let binding_notifies = self
            .remove_binding_payload(&member.id)
            .map(|binding| vec![TeamMemberBindingNotifyPayload::Delete { binding }])
            .unwrap_or_default();
        Ok(TeamRegistryEvents {
            member_notifies: vec![TeamMemberNotifyPayload::Delete { member }],
            binding_notifies,
            ..TeamRegistryEvents::default()
        })
    }

    fn member_for_agent(&self, agent_id: &AgentId) -> Result<Option<TeamMember>, String> {
        let Some(binding) = self
            .bindings
            .iter()
            .find(|binding| binding.current_agent_id.as_ref() == Some(agent_id))
        else {
            return Ok(None);
        };
        self.store
            .get_member(&binding.member_id)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "binding references missing team member {}",
                    binding.member_id
                )
            })
    }

    fn upsert_binding(
        &mut self,
        member_id: TeamMemberId,
        current_agent_id: Option<AgentId>,
        status: AgentControlStatus,
    ) -> Result<TeamMemberBindingPayload, String> {
        if self.store.get_member(&member_id).is_none() {
            return Err(format!("cannot bind missing team member {member_id}"));
        }
        if let Some(agent_id) = current_agent_id.as_ref() {
            for binding in &mut self.bindings {
                if binding.member_id != member_id
                    && binding.current_agent_id.as_ref() == Some(agent_id)
                {
                    binding.current_agent_id = None;
                    binding.status = AgentControlStatus::Idle;
                    binding.last_active_at_ms = Some(now_ms());
                }
            }
        }
        if let Some(binding) = self
            .bindings
            .iter_mut()
            .find(|binding| binding.member_id == member_id)
        {
            binding.current_agent_id = current_agent_id;
            binding.status = status;
            binding.last_active_at_ms = Some(now_ms());
            return Ok(binding.clone());
        }
        let binding = TeamMemberBindingPayload {
            member_id,
            current_agent_id,
            status,
            last_active_at_ms: Some(now_ms()),
        };
        self.bindings.push(binding.clone());
        Ok(binding)
    }

    fn ensure_binding_payload(
        &mut self,
        member_id: &TeamMemberId,
    ) -> Result<TeamMemberBindingPayload, String> {
        if self
            .bindings
            .iter()
            .any(|binding| binding.member_id == *member_id)
        {
            return self
                .bindings
                .iter()
                .find(|binding| binding.member_id == *member_id)
                .cloned()
                .ok_or_else(|| format!("binding disappeared for member {member_id}"));
        }
        let payload = TeamMemberBindingPayload {
            member_id: member_id.clone(),
            current_agent_id: None,
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        };
        self.bindings.push(payload.clone());
        Ok(payload)
    }

    fn remove_binding_payload(
        &mut self,
        member_id: &TeamMemberId,
    ) -> Option<TeamMemberBindingPayload> {
        let index = self
            .bindings
            .iter()
            .position(|binding| binding.member_id == *member_id)?;
        Some(self.bindings.remove(index))
    }

    fn expire_pending_activations(&mut self) {
        let cutoff = now_ms().saturating_sub(ACTIVATION_RESERVATION_TIMEOUT_MS);
        self.pending_activations
            .retain(|_, started_at_ms| *started_at_ms >= cutoff);
    }
}

fn spawn_team_registry_actor(actor: TeamRegistryActor, rx: mpsc::Receiver<TeamRegistryCommand>) {
    let worker = actor.run(rx);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker);
        return;
    }

    if let Err(err) = std::thread::Builder::new()
        .name("tyde-team-registry".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(error = %err, "failed to build team registry runtime");
                    return;
                }
            };
            runtime.block_on(worker);
        })
    {
        tracing::error!(error = %err, "failed to spawn team registry worker thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::agent_teams::AgentTeamsStore;
    use protocol::{CustomAgentId, ProjectId, TeamCreatePayload, TeamMemberCreateSpec};
    use tempfile::TempDir;

    fn refs() -> AgentTeamValidationRefs {
        AgentTeamValidationRefs {
            custom_agent_ids: [CustomAgentId("custom-1".to_owned())].into_iter().collect(),
            project_ids: [ProjectId("project-1".to_owned())].into_iter().collect(),
        }
    }

    fn manager_spec() -> TeamMemberCreateSpec {
        TeamMemberCreateSpec {
            name: "Manager".to_owned(),
            description: "Coordinates".to_owned(),
            custom_agent_id: CustomAgentId("custom-1".to_owned()),
            project_ids: vec![ProjectId("project-1".to_owned())],
        }
    }

    fn report_spec(name: &str) -> TeamMemberCreateSpec {
        TeamMemberCreateSpec {
            name: name.to_owned(),
            description: format!("{name} description"),
            custom_agent_id: CustomAgentId("custom-1".to_owned()),
            project_ids: vec![ProjectId("project-1".to_owned())],
        }
    }

    fn build_actor() -> (TeamRegistryActor, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let store = AgentTeamsStore::load(path, &refs()).expect("load empty store");
        let bindings = store
            .members()
            .into_iter()
            .map(|member| TeamMemberBindingPayload {
                member_id: member.id,
                current_agent_id: None,
                status: AgentControlStatus::Idle,
                last_active_at_ms: None,
            })
            .collect();
        let actor = TeamRegistryActor {
            store,
            bindings,
            pending_activations: HashMap::new(),
        };
        (actor, dir)
    }

    #[test]
    fn plan_user_activation_fresh_member_is_new() {
        let (mut actor, _dir) = build_actor();
        let (_team, manager) = actor
            .store
            .create_team(
                TeamCreatePayload {
                    name: "Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs(),
            )
            .expect("create team");
        actor.bindings.push(TeamMemberBindingPayload {
            member_id: manager.id.clone(),
            current_agent_id: None,
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        });

        let plan = actor
            .plan_user_activation(&manager.id, true)
            .expect("plan activation");
        assert!(matches!(plan.activation, TeamMemberActivation::New));
        // reserve=true should hold the slot
        let conflict = actor
            .plan_user_activation(&manager.id, true)
            .expect_err("second reserved activation should conflict");
        assert!(
            conflict.contains("already in progress"),
            "unexpected error: {conflict}"
        );
    }

    #[test]
    fn plan_user_activation_no_reserve_does_not_block_followup() {
        let (mut actor, _dir) = build_actor();
        let (_team, manager) = actor
            .store
            .create_team(
                TeamCreatePayload {
                    name: "Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs(),
            )
            .expect("create team");
        actor.bindings.push(TeamMemberBindingPayload {
            member_id: manager.id.clone(),
            current_agent_id: None,
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        });

        actor
            .plan_user_activation(&manager.id, false)
            .expect("first planning ok");
        actor
            .plan_user_activation(&manager.id, false)
            .expect("second planning ok without reservation");
    }

    #[test]
    fn plan_user_activation_with_session_is_resume() {
        let (mut actor, _dir) = build_actor();
        let (team, manager) = actor
            .store
            .create_team(
                TeamCreatePayload {
                    name: "Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs(),
            )
            .expect("create team");
        let report = actor
            .store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: report_spec("alice"),
                    session_id: None,
                },
                &refs(),
            )
            .expect("create report");
        actor.bindings.push(TeamMemberBindingPayload {
            member_id: manager.id.clone(),
            current_agent_id: None,
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        });
        actor.bindings.push(TeamMemberBindingPayload {
            member_id: report.id.clone(),
            current_agent_id: None,
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        });
        actor
            .store
            .set_member_session_id(&report.id, SessionId("sess-1".to_owned()), &refs())
            .expect("set session id");

        let plan = actor
            .plan_user_activation(&report.id, true)
            .expect("plan activation");
        match plan.activation {
            TeamMemberActivation::Resume { session_id } => {
                assert_eq!(session_id.0, "sess-1");
            }
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    #[test]
    fn plan_user_activation_with_binding_is_reuse() {
        let (mut actor, _dir) = build_actor();
        let (_team, manager) = actor
            .store
            .create_team(
                TeamCreatePayload {
                    name: "Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs(),
            )
            .expect("create team");
        actor.bindings.push(TeamMemberBindingPayload {
            member_id: manager.id.clone(),
            current_agent_id: Some(AgentId("agent-mgr".to_owned())),
            status: AgentControlStatus::Idle,
            last_active_at_ms: None,
        });

        let plan = actor
            .plan_user_activation(&manager.id, true)
            .expect("plan activation");
        match plan.activation {
            TeamMemberActivation::Reuse { agent_id } => {
                assert_eq!(agent_id.0, "agent-mgr");
            }
            other => panic!("expected Reuse, got {other:?}"),
        }
    }

    #[test]
    fn plan_user_activation_rejects_missing_member() {
        let (mut actor, _dir) = build_actor();
        let err = actor
            .plan_user_activation(&TeamMemberId("ghost".to_owned()), true)
            .expect_err("missing member should be rejected");
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    #[test]
    fn plan_user_activation_rejects_deleted_member() {
        let (mut actor, _dir) = build_actor();
        let (team, _manager) = actor
            .store
            .create_team(
                TeamCreatePayload {
                    name: "Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs(),
            )
            .expect("create team");
        let report = actor
            .store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: report_spec("alice"),
                    session_id: None,
                },
                &refs(),
            )
            .expect("create report");
        actor
            .store
            .delete_member(
                TeamMemberDeletePayload {
                    id: report.id.clone(),
                },
                &refs(),
            )
            .expect("delete report");

        let err = actor
            .plan_user_activation(&report.id, true)
            .expect_err("deleted member should be rejected");
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }
}
