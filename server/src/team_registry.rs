use std::collections::HashMap;

use protocol::{
    AgentControlStatus, AgentId, CustomAgentId, SessionId, Team, TeamCreateFromDraftPayload,
    TeamCreatePayload, TeamDeletePayload, TeamDraft, TeamDraftApplyTemplatePayload,
    TeamDraftCommitPayload, TeamDraftCreatePayload, TeamDraftDiscardPayload, TeamDraftId,
    TeamDraftMember, TeamDraftMemberEdit, TeamDraftMemberId, TeamDraftNotifyPayload,
    TeamDraftShufflePayload, TeamDraftShuffleScope, TeamDraftUpdatePayload, TeamMember,
    TeamMemberBindingNotifyPayload, TeamMemberBindingPayload, TeamMemberCreatePayload,
    TeamMemberCreateSpec, TeamMemberDeletePayload, TeamMemberId, TeamMemberNotifyPayload,
    TeamMemberPresetProfile, TeamMemberRole, TeamMemberShufflePayload, TeamMemberShuffleSuggestion,
    TeamMemberShuffleSuggestionNotifyPayload, TeamMemberState, TeamMemberUpdatePayload,
    TeamNotifyPayload, TeamPersonalityPreset, TeamPersonalityPresetId, TeamPersonalityTrait,
    TeamPersonalityTraitPreset, TeamPresetCatalog, TeamRenamePayload, TeamRolePreset,
    TeamRolePresetId, TeamSetManagerPayload, TeamTemplate, TeamTemplateId, TeamTemplateMember,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agent::now_ms;
use crate::store::agent_teams::{AgentTeamValidationRefs, AgentTeamsStore};
use crate::store::custom_agents::TEAM_LEAD_CUSTOM_AGENT_ID;

const ACTIVATION_RESERVATION_TIMEOUT_MS: u64 = 35_000;

#[derive(Clone)]
pub(crate) struct TeamRegistryHandle {
    tx: mpsc::Sender<TeamRegistryCommand>,
}

#[derive(Debug, Clone)]
pub(crate) struct TeamRegistrySnapshot {
    pub catalog: TeamPresetCatalog,
    pub drafts: Vec<TeamDraft>,
    pub teams: Vec<Team>,
    pub members: Vec<TeamMember>,
    pub bindings: Vec<TeamMemberBindingPayload>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TeamRegistryEvents {
    pub team_notifies: Vec<TeamNotifyPayload>,
    pub member_notifies: Vec<TeamMemberNotifyPayload>,
    pub binding_notifies: Vec<TeamMemberBindingNotifyPayload>,
    pub draft_notifies: Vec<TeamDraftNotifyPayload>,
    pub shuffle_suggestion_notifies: Vec<TeamMemberShuffleSuggestionNotifyPayload>,
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
    RotateMemberAgent {
        member_id: TeamMemberId,
        old_agent_id: AgentId,
        new_agent_id: AgentId,
        old_session_id: SessionId,
        new_session_id: SessionId,
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
    CreateDraft {
        payload: TeamDraftCreatePayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    UpdateDraft {
        payload: TeamDraftUpdatePayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    ShuffleDraft {
        payload: TeamDraftShufflePayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    ShuffleMemberSuggestion {
        payload: TeamMemberShufflePayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    ApplyDraftTemplate {
        payload: TeamDraftApplyTemplatePayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    CommitDraft {
        payload: TeamDraftCommitPayload,
        refs: AgentTeamValidationRefs,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
    DiscardDraft {
        payload: TeamDraftDiscardPayload,
        reply: oneshot::Sender<Result<TeamRegistryEvents, String>>,
    },
}

struct TeamRegistryActor {
    store: AgentTeamsStore,
    drafts: Vec<TeamDraft>,
    bindings: Vec<TeamMemberBindingPayload>,
    pending_activations: HashMap<TeamMemberId, u64>,
    shuffle_counter: u64,
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
            drafts: Vec::new(),
            bindings,
            pending_activations: HashMap::new(),
            shuffle_counter: 0,
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

    pub(crate) async fn rotate_member_agent(
        &self,
        member_id: TeamMemberId,
        old_agent_id: AgentId,
        new_agent_id: AgentId,
        old_session_id: SessionId,
        new_session_id: SessionId,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::RotateMemberAgent {
            member_id,
            old_agent_id,
            new_agent_id,
            old_session_id,
            new_session_id,
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

    pub(crate) async fn create_draft(
        &self,
        payload: TeamDraftCreatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::CreateDraft { payload, reply })
            .await
    }

    pub(crate) async fn update_draft(
        &self,
        payload: TeamDraftUpdatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::UpdateDraft { payload, reply })
            .await
    }

    pub(crate) async fn shuffle_draft(
        &self,
        payload: TeamDraftShufflePayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::ShuffleDraft { payload, reply })
            .await
    }

    pub(crate) async fn shuffle_member_suggestion(
        &self,
        payload: TeamMemberShufflePayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::ShuffleMemberSuggestion { payload, reply })
            .await
    }

    pub(crate) async fn apply_draft_template(
        &self,
        payload: TeamDraftApplyTemplatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::ApplyDraftTemplate { payload, reply })
            .await
    }

    pub(crate) async fn commit_draft(
        &self,
        payload: TeamDraftCommitPayload,
        refs: AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::CommitDraft {
            payload,
            refs,
            reply,
        })
        .await
    }

    pub(crate) async fn discard_draft(
        &self,
        payload: TeamDraftDiscardPayload,
    ) -> Result<TeamRegistryEvents, String> {
        self.mutate(|reply| TeamRegistryCommand::DiscardDraft { payload, reply })
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
                TeamRegistryCommand::RotateMemberAgent {
                    member_id,
                    old_agent_id,
                    new_agent_id,
                    old_session_id,
                    new_session_id,
                    refs,
                    reply,
                } => {
                    let result = self.rotate_member_agent(
                        member_id,
                        old_agent_id,
                        new_agent_id,
                        old_session_id,
                        new_session_id,
                        &refs,
                    );
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
                TeamRegistryCommand::CreateDraft { payload, reply } => {
                    let result = self.create_draft(payload);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::UpdateDraft { payload, reply } => {
                    let result = self.update_draft(payload);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::ShuffleDraft { payload, reply } => {
                    let result = self.shuffle_draft(payload);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::ShuffleMemberSuggestion { payload, reply } => {
                    let result = self.shuffle_member_suggestion(payload);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::ApplyDraftTemplate { payload, reply } => {
                    let result = self.apply_draft_template(payload);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::CommitDraft {
                    payload,
                    refs,
                    reply,
                } => {
                    let result = self.commit_draft(payload, &refs);
                    let _ = reply.send(result);
                }
                TeamRegistryCommand::DiscardDraft { payload, reply } => {
                    let result = self.discard_draft(payload);
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn snapshot(&self) -> TeamRegistrySnapshot {
        TeamRegistrySnapshot {
            catalog: team_preset_catalog(),
            drafts: self.drafts.clone(),
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

    fn rotate_member_agent(
        &mut self,
        member_id: TeamMemberId,
        old_agent_id: AgentId,
        new_agent_id: AgentId,
        old_session_id: SessionId,
        new_session_id: SessionId,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        self.pending_activations.remove(&member_id);
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.member_id == member_id)
            .ok_or_else(|| format!("team member {member_id} has no binding"))?;
        if binding.current_agent_id.as_ref() != Some(&old_agent_id) {
            return Err(format!(
                "team member {member_id} is not bound to agent {old_agent_id}"
            ));
        }

        let member = self.store.replace_member_session_id(
            &member_id,
            &old_session_id,
            new_session_id,
            refs,
        )?;
        let binding =
            self.upsert_binding(member_id, Some(new_agent_id), AgentControlStatus::Thinking)?;
        Ok(TeamRegistryEvents {
            member_notifies: vec![TeamMemberNotifyPayload::Upsert { member }],
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        })
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
        let previous = self
            .bindings
            .iter()
            .find(|binding| binding.member_id == *member_id)
            .cloned();
        let Some(current_agent_id) = previous
            .as_ref()
            .and_then(|binding| binding.current_agent_id.clone())
        else {
            return Err(format!("team member {member_id} has no live binding"));
        };
        let binding = self.upsert_binding(member_id.clone(), Some(current_agent_id), status)?;
        Ok(binding_activity_events(previous.as_ref(), binding))
    }

    fn record_agent_activity(
        &mut self,
        agent_id: &AgentId,
        status: AgentControlStatus,
    ) -> Result<TeamRegistryEvents, String> {
        let previous = self
            .bindings
            .iter()
            .find(|binding| binding.current_agent_id.as_ref() == Some(agent_id))
            .cloned();
        let Some(member_id) = previous.as_ref().map(|binding| binding.member_id.clone()) else {
            return Ok(TeamRegistryEvents::default());
        };
        let binding = self.upsert_binding(member_id, Some(agent_id.clone()), status)?;
        Ok(binding_activity_events(previous.as_ref(), binding))
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
            ..TeamRegistryEvents::default()
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
            ..TeamRegistryEvents::default()
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

    fn create_draft(
        &mut self,
        payload: TeamDraftCreatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        if let Some(existing) = self.drafts.first() {
            return Err(format!("team draft {} already exists", existing.id));
        }
        let draft = build_team_draft(payload.template_id.as_ref(), now_ms())?;
        self.drafts.push(draft.clone());
        Ok(TeamRegistryEvents {
            draft_notifies: vec![TeamDraftNotifyPayload::Upsert { draft }],
            ..TeamRegistryEvents::default()
        })
    }

    fn update_draft(
        &mut self,
        payload: TeamDraftUpdatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        let draft_id = team_draft_update_id(&payload).clone();
        let mut draft = self.take_draft(&draft_id)?;
        let original = draft.clone();
        let result = (|| -> Result<(), String> {
            match payload {
                TeamDraftUpdatePayload::SetName { name, .. } => {
                    draft.name = name;
                }
                TeamDraftUpdatePayload::ReplaceMember { member, .. } => {
                    let index = draft_member_index(&draft, &member.id)?;
                    validate_draft_member_edit(&member)?;
                    apply_draft_member_edit(&mut draft.members[index], member);
                }
                TeamDraftUpdatePayload::AddReport { .. } => {
                    draft
                        .members
                        .push(blank_draft_member(TeamMemberRole::Report));
                }
                TeamDraftUpdatePayload::RemoveMember { member_id, .. } => {
                    let index = draft_member_index(&draft, &member_id)?;
                    if draft.members[index].org_role == TeamMemberRole::Manager {
                        return Err(format!("cannot remove draft manager member {member_id}"));
                    }
                    draft.members.remove(index);
                }
                TeamDraftUpdatePayload::SetMemberProfile {
                    member_id,
                    role_preset_id,
                    personality_preset_id,
                    personality_traits,
                    ..
                } => {
                    let index = draft_member_index(&draft, &member_id)?;
                    let profile = build_profile(
                        role_preset_id.as_ref(),
                        personality_preset_id.as_ref(),
                        personality_traits,
                    )?;
                    apply_profile_to_draft_member(&mut draft.members[index], profile)?;
                }
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.drafts.push(original);
            return Err(err);
        }
        draft.updated_at_ms = now_ms();
        self.drafts.push(draft.clone());
        Ok(TeamRegistryEvents {
            draft_notifies: vec![TeamDraftNotifyPayload::Upsert { draft }],
            ..TeamRegistryEvents::default()
        })
    }

    /// Compute a server-owned shuffle suggestion for the Add-report dialog
    /// of an existing team. Returns a notify event payload; persists
    /// nothing. The frontend applies the suggestion to its open form.
    fn shuffle_member_suggestion(
        &mut self,
        payload: TeamMemberShufflePayload,
    ) -> Result<TeamRegistryEvents, String> {
        // Authorize: the team must exist on this host.
        let _team = self
            .store
            .get_team(&payload.team_id)
            .ok_or_else(|| format!("team {} not found", payload.team_id))?;
        self.shuffle_counter = self.shuffle_counter.saturating_add(1);
        let suggestion = build_member_shuffle_suggestion(self.shuffle_counter as usize)?;
        Ok(TeamRegistryEvents {
            shuffle_suggestion_notifies: vec![TeamMemberShuffleSuggestionNotifyPayload {
                team_id: payload.team_id,
                suggestion,
            }],
            ..TeamRegistryEvents::default()
        })
    }

    fn shuffle_draft(
        &mut self,
        payload: TeamDraftShufflePayload,
    ) -> Result<TeamRegistryEvents, String> {
        let mut draft = self.take_draft(&payload.draft_id)?;
        let original = draft.clone();
        self.shuffle_counter = self.shuffle_counter.saturating_add(1);
        let result = (|| -> Result<(), String> {
            if let Some(member_id) = payload.member_id {
                let index = draft_member_index(&draft, &member_id)?;
                shuffle_draft_member(
                    &mut draft.members[index],
                    payload.scope,
                    self.shuffle_counter as usize,
                )
            } else {
                for (index, member) in draft.members.iter_mut().enumerate() {
                    shuffle_draft_member(
                        member,
                        payload.scope,
                        self.shuffle_counter as usize + index,
                    )?;
                }
                Ok(())
            }
        })();
        if let Err(err) = result {
            self.drafts.push(original);
            return Err(err);
        }
        draft.updated_at_ms = now_ms();
        self.drafts.push(draft.clone());
        Ok(TeamRegistryEvents {
            draft_notifies: vec![TeamDraftNotifyPayload::Upsert { draft }],
            ..TeamRegistryEvents::default()
        })
    }

    fn apply_draft_template(
        &mut self,
        payload: TeamDraftApplyTemplatePayload,
    ) -> Result<TeamRegistryEvents, String> {
        let mut draft = self.take_draft(&payload.draft_id)?;
        let original = draft.clone();
        let result = find_template(&payload.template_id)
            .and_then(|template| draft_members_from_template(&template))
            .map(|members| {
                draft.members = members;
            });
        if let Err(err) = result {
            self.drafts.push(original);
            return Err(err);
        }
        draft.updated_at_ms = now_ms();
        self.drafts.push(draft.clone());
        Ok(TeamRegistryEvents {
            draft_notifies: vec![TeamDraftNotifyPayload::Upsert { draft }],
            ..TeamRegistryEvents::default()
        })
    }

    fn commit_draft(
        &mut self,
        payload: TeamDraftCommitPayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamRegistryEvents, String> {
        let draft = self.take_draft(&payload.draft_id)?;
        let create = match team_create_from_draft(&draft) {
            Ok(create) => create,
            Err(err) => {
                self.drafts.push(draft);
                return Err(err);
            }
        };
        match self.store.create_team_from_draft(create, refs) {
            Ok((team, members)) => {
                let mut member_notifies = Vec::new();
                let mut binding_notifies = Vec::new();
                for member in members {
                    let binding = self.ensure_binding_payload(&member.id)?;
                    binding_notifies.push(TeamMemberBindingNotifyPayload::Upsert { binding });
                    member_notifies.push(TeamMemberNotifyPayload::Upsert { member });
                }
                Ok(TeamRegistryEvents {
                    team_notifies: vec![TeamNotifyPayload::Upsert { team }],
                    member_notifies,
                    binding_notifies,
                    draft_notifies: vec![TeamDraftNotifyPayload::Delete { draft_id: draft.id }],
                    shuffle_suggestion_notifies: Vec::new(),
                })
            }
            Err(err) => {
                self.drafts.push(draft);
                Err(err)
            }
        }
    }

    fn discard_draft(
        &mut self,
        payload: TeamDraftDiscardPayload,
    ) -> Result<TeamRegistryEvents, String> {
        let draft = self.take_draft(&payload.draft_id)?;
        Ok(TeamRegistryEvents {
            draft_notifies: vec![TeamDraftNotifyPayload::Delete { draft_id: draft.id }],
            ..TeamRegistryEvents::default()
        })
    }

    fn take_draft(&mut self, draft_id: &TeamDraftId) -> Result<TeamDraft, String> {
        let index = self
            .drafts
            .iter()
            .position(|draft| draft.id == *draft_id)
            .ok_or_else(|| format!("missing team draft {draft_id}"))?;
        Ok(self.drafts.remove(index))
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

fn binding_activity_events(
    previous: Option<&TeamMemberBindingPayload>,
    binding: TeamMemberBindingPayload,
) -> TeamRegistryEvents {
    let should_notify = previous.is_none_or(|previous| {
        previous.current_agent_id != binding.current_agent_id || previous.status != binding.status
    });
    if should_notify {
        TeamRegistryEvents {
            binding_notifies: vec![TeamMemberBindingNotifyPayload::Upsert { binding }],
            ..TeamRegistryEvents::default()
        }
    } else {
        TeamRegistryEvents::default()
    }
}

pub(crate) fn team_preset_catalog() -> TeamPresetCatalog {
    TeamPresetCatalog {
        role_presets: role_presets(),
        personality_traits: personality_trait_presets(),
        personality_presets: personality_presets(),
        team_templates: team_templates(),
    }
}

pub(crate) fn team_preset_validation_refs() -> (
    std::collections::HashSet<TeamRolePresetId>,
    std::collections::HashSet<TeamPersonalityPresetId>,
) {
    let catalog = team_preset_catalog();
    (
        catalog
            .role_presets
            .into_iter()
            .map(|preset| preset.id)
            .collect(),
        catalog
            .personality_presets
            .into_iter()
            .map(|preset| preset.id)
            .collect(),
    )
}

fn role_presets() -> Vec<TeamRolePreset> {
    vec![
        TeamRolePreset {
            id: role_id("tech-lead-planner"),
            name: "Tech lead / planner".to_owned(),
            summary: "Breaks work into crisp tasks and keeps the team aligned.".to_owned(),
            default_member_name: "Tech Lead".to_owned(),
            default_description:
                "Plans the implementation, coordinates reports, and keeps scope tight.".to_owned(),
            default_custom_agent_id: Some(CustomAgentId(TEAM_LEAD_CUSTOM_AGENT_ID.to_owned())),
        },
        TeamRolePreset {
            id: role_id("senior-reviewer"),
            name: "Senior reviewer".to_owned(),
            summary: "Reviews architecture, correctness, and maintainability.".to_owned(),
            default_member_name: "Senior Reviewer".to_owned(),
            default_description:
                "Reviews the plan and code for correctness, design fit, and maintainability."
                    .to_owned(),
            default_custom_agent_id: None,
        },
        TeamRolePreset {
            id: role_id("frontend-specialist"),
            name: "Frontend specialist".to_owned(),
            summary: "Owns UI, state projection, and interaction polish.".to_owned(),
            default_member_name: "Frontend Specialist".to_owned(),
            default_description:
                "Implements UI behavior, typed frontend state projection, and user-facing polish."
                    .to_owned(),
            default_custom_agent_id: None,
        },
        TeamRolePreset {
            id: role_id("backend-specialist"),
            name: "Backend specialist".to_owned(),
            summary: "Owns server behavior, persistence, and protocol plumbing.".to_owned(),
            default_member_name: "Backend Specialist".to_owned(),
            default_description:
                "Implements server-owned behavior, persistence, validation, and protocol flow."
                    .to_owned(),
            default_custom_agent_id: None,
        },
        TeamRolePreset {
            id: role_id("test-author-qa"),
            name: "Test author / QA".to_owned(),
            summary: "Writes focused tests and checks user-visible behavior.".to_owned(),
            default_member_name: "Test Author".to_owned(),
            default_description:
                "Adds focused tests, exercises edge cases, and verifies observable behavior."
                    .to_owned(),
            default_custom_agent_id: None,
        },
        TeamRolePreset {
            id: role_id("bug-hunter-debugger"),
            name: "Bug hunter / debugger".to_owned(),
            summary: "Gathers evidence, identifies root causes, and fixes defects.".to_owned(),
            default_member_name: "Bug Hunter".to_owned(),
            default_description:
                "Reproduces failures, gathers evidence, identifies root causes, and fixes the bug."
                    .to_owned(),
            default_custom_agent_id: None,
        },
    ]
}

fn personality_trait_presets() -> Vec<TeamPersonalityTraitPreset> {
    vec![
        trait_preset(
            TeamPersonalityTrait::Cautious,
            "Cautious",
            "Surfaces risks before acting.",
        ),
        trait_preset(
            TeamPersonalityTrait::Pragmatic,
            "Pragmatic",
            "Balances quality with delivery.",
        ),
        trait_preset(
            TeamPersonalityTrait::Bold,
            "Bold",
            "Pushes decisive approaches when warranted.",
        ),
        trait_preset(
            TeamPersonalityTrait::Contrarian,
            "Contrarian",
            "Challenges assumptions and weak plans.",
        ),
        trait_preset(
            TeamPersonalityTrait::Terse,
            "Terse",
            "Keeps communication compact.",
        ),
        trait_preset(
            TeamPersonalityTrait::Conversational,
            "Conversational",
            "Explains tradeoffs naturally.",
        ),
        trait_preset(
            TeamPersonalityTrait::Pedagogical,
            "Pedagogical",
            "Teaches while explaining choices.",
        ),
        trait_preset(
            TeamPersonalityTrait::Skeptical,
            "Skeptical",
            "Looks for hidden failure modes.",
        ),
        trait_preset(
            TeamPersonalityTrait::RefactorLeaning,
            "Refactor-leaning",
            "Prefers improving structure when scope allows.",
        ),
        trait_preset(
            TeamPersonalityTrait::ShipIt,
            "Ship-it",
            "Optimizes for a safe shippable slice.",
        ),
        trait_preset(
            TeamPersonalityTrait::TestFirst,
            "Test-first",
            "Starts with observable coverage.",
        ),
        trait_preset(
            TeamPersonalityTrait::TypeSystem,
            "Type-system",
            "Leans on types and invariants.",
        ),
        trait_preset(
            TeamPersonalityTrait::Yagni,
            "YAGNI",
            "Avoids speculative abstractions.",
        ),
    ]
}

fn personality_presets() -> Vec<TeamPersonalityPreset> {
    vec![
        TeamPersonalityPreset {
            id: personality_id("skeptical-reviewer"),
            name: "Skeptical reviewer".to_owned(),
            summary: "Challenges assumptions with concise risk-focused feedback.".to_owned(),
            traits: vec![
                TeamPersonalityTrait::Skeptical,
                TeamPersonalityTrait::Contrarian,
                TeamPersonalityTrait::Terse,
            ],
        },
        TeamPersonalityPreset {
            id: personality_id("pragmatic-shipper"),
            name: "Pragmatic shipper".to_owned(),
            summary: "Finds the smallest safe implementation that can ship.".to_owned(),
            traits: vec![
                TeamPersonalityTrait::Pragmatic,
                TeamPersonalityTrait::ShipIt,
                TeamPersonalityTrait::Yagni,
            ],
        },
        TeamPersonalityPreset {
            id: personality_id("careful-architect"),
            name: "Careful architect".to_owned(),
            summary: "Designs around invariants and long-term maintainability.".to_owned(),
            traits: vec![
                TeamPersonalityTrait::Cautious,
                TeamPersonalityTrait::TypeSystem,
                TeamPersonalityTrait::Pedagogical,
            ],
        },
        TeamPersonalityPreset {
            id: personality_id("test-first-engineer"),
            name: "Test-first engineer".to_owned(),
            summary: "Starts with coverage and verifies behavior before polishing.".to_owned(),
            traits: vec![
                TeamPersonalityTrait::TestFirst,
                TeamPersonalityTrait::Cautious,
                TeamPersonalityTrait::Conversational,
            ],
        },
        TeamPersonalityPreset {
            id: personality_id("refactor-minded-senior"),
            name: "Refactor-minded senior".to_owned(),
            summary: "Improves structure while staying alert to scope.".to_owned(),
            traits: vec![
                TeamPersonalityTrait::RefactorLeaning,
                TeamPersonalityTrait::Pragmatic,
                TeamPersonalityTrait::TypeSystem,
            ],
        },
    ]
}

fn team_templates() -> Vec<TeamTemplate> {
    vec![
        TeamTemplate {
            id: template_id("solo-reviewer"),
            name: "Solo + reviewer".to_owned(),
            summary: "A manager/implementer paired with one senior reviewer.".to_owned(),
            balanced: false,
            members: vec![
                template_member(
                    TeamMemberRole::Manager,
                    "tech-lead-planner",
                    Some("pragmatic-shipper"),
                    "Lead Engineer",
                    "Implements the main slice and coordinates review feedback.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "senior-reviewer",
                    Some("skeptical-reviewer"),
                    "Reviewer",
                    "Reviews the implementation for correctness, architecture, and missing tests.",
                ),
            ],
        },
        TeamTemplate {
            id: template_id("small-feature-team"),
            name: "Small feature team".to_owned(),
            summary: "A balanced planner, frontend, backend, and QA team.".to_owned(),
            balanced: true,
            members: vec![
                template_member(
                    TeamMemberRole::Manager,
                    "tech-lead-planner",
                    Some("careful-architect"),
                    "Feature Lead",
                    "Plans the feature and coordinates implementation across the team.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "frontend-specialist",
                    Some("pragmatic-shipper"),
                    "Frontend Engineer",
                    "Implements UI behavior and keeps the user interaction shippable.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "backend-specialist",
                    Some("careful-architect"),
                    "Backend Engineer",
                    "Implements server-owned behavior, validation, and persistence.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "test-author-qa",
                    Some("test-first-engineer"),
                    "QA Engineer",
                    "Adds focused tests and verifies the feature end to end.",
                ),
            ],
        },
        TeamTemplate {
            id: template_id("review-panel"),
            name: "Review panel".to_owned(),
            summary: "Multiple reviewers with different review lenses.".to_owned(),
            balanced: false,
            members: vec![
                template_member(
                    TeamMemberRole::Manager,
                    "tech-lead-planner",
                    Some("pragmatic-shipper"),
                    "Review Lead",
                    "Coordinates the review panel and resolves conflicting feedback.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "senior-reviewer",
                    Some("skeptical-reviewer"),
                    "Skeptical Reviewer",
                    "Looks for correctness gaps, hidden assumptions, and unsafe shortcuts.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "senior-reviewer",
                    Some("refactor-minded-senior"),
                    "Maintainability Reviewer",
                    "Reviews structure, naming, and maintainability risks.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "test-author-qa",
                    Some("test-first-engineer"),
                    "Test Reviewer",
                    "Checks that behavior is covered by focused tests.",
                ),
            ],
        },
        TeamTemplate {
            id: template_id("debug-squad"),
            name: "Debug squad".to_owned(),
            summary: "Evidence gathering, root-cause analysis, and regression coverage.".to_owned(),
            balanced: false,
            members: vec![
                template_member(
                    TeamMemberRole::Manager,
                    "bug-hunter-debugger",
                    Some("careful-architect"),
                    "Debug Lead",
                    "Coordinates reproduction, evidence gathering, and the final fix.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "bug-hunter-debugger",
                    Some("skeptical-reviewer"),
                    "Root Cause Debugger",
                    "Reproduces the failure and identifies the root cause from evidence.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "backend-specialist",
                    Some("pragmatic-shipper"),
                    "Fix Engineer",
                    "Implements the smallest server or integration fix that addresses the cause.",
                ),
                template_member(
                    TeamMemberRole::Report,
                    "test-author-qa",
                    Some("test-first-engineer"),
                    "Regression Tester",
                    "Adds regression coverage and verifies the bug stays fixed.",
                ),
            ],
        },
    ]
}

fn template_member(
    org_role: TeamMemberRole,
    role_preset_id: &str,
    personality_preset_id: Option<&str>,
    name: &str,
    description: &str,
) -> TeamTemplateMember {
    TeamTemplateMember {
        org_role,
        role_preset_id: role_id(role_preset_id),
        personality_preset_id: personality_preset_id.map(personality_id),
        name: name.to_owned(),
        description: description.to_owned(),
    }
}

fn trait_preset(
    trait_id: TeamPersonalityTrait,
    name: &str,
    summary: &str,
) -> TeamPersonalityTraitPreset {
    TeamPersonalityTraitPreset {
        trait_id,
        name: name.to_owned(),
        summary: summary.to_owned(),
    }
}

fn role_id(id: &str) -> TeamRolePresetId {
    TeamRolePresetId(id.to_owned())
}

fn personality_id(id: &str) -> TeamPersonalityPresetId {
    TeamPersonalityPresetId(id.to_owned())
}

fn template_id(id: &str) -> TeamTemplateId {
    TeamTemplateId(id.to_owned())
}

fn active_draft_id() -> TeamDraftId {
    TeamDraftId("active-team-draft".to_owned())
}

fn build_team_draft(
    template_id: Option<&TeamTemplateId>,
    now_ms: u64,
) -> Result<TeamDraft, String> {
    let members = match template_id {
        Some(template_id) => draft_members_from_template(&find_template(template_id)?)?,
        None => vec![blank_draft_member(TeamMemberRole::Manager)],
    };
    Ok(TeamDraft {
        id: active_draft_id(),
        name: String::new(),
        members,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

fn blank_draft_member(org_role: TeamMemberRole) -> TeamDraftMember {
    TeamDraftMember {
        id: TeamDraftMemberId(Uuid::new_v4().to_string()),
        org_role,
        name: String::new(),
        description: String::new(),
        profile: None,
        custom_agent_id: None,
        backend_kind: None,
        cost_hint: None,
        project_ids: Vec::new(),
    }
}

fn draft_members_from_template(template: &TeamTemplate) -> Result<Vec<TeamDraftMember>, String> {
    let mut members = Vec::new();
    for member in &template.members {
        let profile = build_profile(
            Some(&member.role_preset_id),
            member.personality_preset_id.as_ref(),
            Vec::new(),
        )?;
        members.push(TeamDraftMember {
            id: TeamDraftMemberId(Uuid::new_v4().to_string()),
            org_role: member.org_role,
            name: member.name.clone(),
            description: member.description.clone(),
            profile: Some(profile),
            custom_agent_id: default_custom_agent_id_for_role(&member.role_preset_id),
            backend_kind: None,
            cost_hint: None,
            project_ids: Vec::new(),
        });
    }
    validate_draft_org(&members)?;
    Ok(members)
}

fn find_template(template_id: &TeamTemplateId) -> Result<TeamTemplate, String> {
    team_templates()
        .into_iter()
        .find(|template| template.id == *template_id)
        .ok_or_else(|| format!("missing team template {template_id}"))
}

fn find_role(role_preset_id: &TeamRolePresetId) -> Result<TeamRolePreset, String> {
    role_presets()
        .into_iter()
        .find(|preset| preset.id == *role_preset_id)
        .ok_or_else(|| format!("missing role preset {role_preset_id}"))
}

fn find_personality(
    personality_preset_id: &TeamPersonalityPresetId,
) -> Result<TeamPersonalityPreset, String> {
    personality_presets()
        .into_iter()
        .find(|preset| preset.id == *personality_preset_id)
        .ok_or_else(|| format!("missing personality preset {personality_preset_id}"))
}

fn build_profile(
    role_preset_id: Option<&TeamRolePresetId>,
    personality_preset_id: Option<&TeamPersonalityPresetId>,
    personality_traits: Vec<TeamPersonalityTrait>,
) -> Result<TeamMemberPresetProfile, String> {
    if let Some(role_preset_id) = role_preset_id {
        find_role(role_preset_id)?;
    }
    let traits = match personality_preset_id {
        Some(personality_preset_id) => find_personality(personality_preset_id)?.traits,
        None => personality_traits,
    };
    Ok(TeamMemberPresetProfile {
        role_preset_id: role_preset_id.cloned(),
        personality_preset_id: personality_preset_id.cloned(),
        personality_traits: traits,
    })
}

fn apply_profile_to_draft_member(
    member: &mut TeamDraftMember,
    profile: TeamMemberPresetProfile,
) -> Result<(), String> {
    if profile.role_preset_id.is_none()
        && profile.personality_preset_id.is_none()
        && profile.personality_traits.is_empty()
    {
        member.profile = None;
        return Ok(());
    }
    let role_changed = member
        .profile
        .as_ref()
        .and_then(|previous| previous.role_preset_id.as_ref())
        != profile.role_preset_id.as_ref();
    if let Some(role_preset_id) = profile.role_preset_id.as_ref()
        && role_changed
    {
        let role = find_role(role_preset_id)?;
        member.name = role.default_member_name;
        member.description = role.default_description;
        member.custom_agent_id = default_custom_agent_id_for_role(role_preset_id);
    }
    if let Some(personality_preset_id) = profile.personality_preset_id.as_ref() {
        let personality = find_personality(personality_preset_id)?;
        if role_changed && !member.description.trim().is_empty() {
            member.description = format!("{} {}", member.description, personality.summary);
        }
    }
    member.profile = Some(profile);
    Ok(())
}

fn shuffle_draft_member(
    member: &mut TeamDraftMember,
    scope: TeamDraftShuffleScope,
    seed: usize,
) -> Result<(), String> {
    let roles = role_presets();
    let personalities = personality_presets();
    let role_index = if member.org_role == TeamMemberRole::Manager {
        seed % roles.len().min(2)
    } else {
        1 + (seed % (roles.len() - 1))
    };
    let role_id = match scope {
        TeamDraftShuffleScope::Member => Some(roles[role_index].id.clone()),
        TeamDraftShuffleScope::Personality => member
            .profile
            .as_ref()
            .and_then(|profile| profile.role_preset_id.clone())
            .or_else(|| Some(roles[role_index].id.clone())),
    };
    let personality_id = personalities[seed % personalities.len()].id.clone();
    let profile = build_profile(role_id.as_ref(), Some(&personality_id), Vec::new())?;
    apply_profile_to_draft_member(member, profile)?;
    if scope == TeamDraftShuffleScope::Member {
        member.name = shuffled_member_name(member, seed);
    }
    Ok(())
}

fn default_custom_agent_id_for_role(role_preset_id: &TeamRolePresetId) -> Option<CustomAgentId> {
    role_presets()
        .into_iter()
        .find(|preset| preset.id == *role_preset_id)
        .and_then(|preset| preset.default_custom_agent_id)
}

/// Server-owned shuffle for the Add-report dialog. Picks a non-manager
/// role and a personality preset from the catalog and returns a fully
/// formed suggestion. The frontend renders the result; it does not pick
/// names, agents, or personalities locally.
fn build_member_shuffle_suggestion(seed: usize) -> Result<TeamMemberShuffleSuggestion, String> {
    let roles = role_presets();
    let personalities = personality_presets();
    if roles.is_empty() {
        return Err("team registry has no role presets".to_owned());
    }
    if personalities.is_empty() {
        return Err("team registry has no personality presets".to_owned());
    }
    let role_index = if roles.len() > 1 {
        1 + (seed % (roles.len() - 1))
    } else {
        0
    };
    let role = &roles[role_index];
    let personality_index = seed % personalities.len();
    let personality = &personalities[personality_index];
    let profile = TeamMemberPresetProfile {
        role_preset_id: Some(role.id.clone()),
        personality_preset_id: Some(personality.id.clone()),
        personality_traits: personality.traits.clone(),
    };
    let name = shuffled_role_name(&role.id, seed);
    let description = format!("{} {}", role.default_description, personality.summary);
    Ok(TeamMemberShuffleSuggestion {
        name,
        description,
        profile,
        custom_agent_id: role.default_custom_agent_id.clone(),
    })
}

fn shuffled_role_name(role_preset_id: &TeamRolePresetId, seed: usize) -> String {
    let variants = role_name_variants(role_preset_id);
    variants[seed % variants.len()].to_owned()
}

fn role_name_variants(role_preset_id: &TeamRolePresetId) -> &'static [&'static str] {
    match role_preset_id.0.as_str() {
        "tech-lead-planner" => &["Lead Planner", "Team Coordinator", "Feature Captain"],
        "senior-reviewer" => &["Code Reviewer", "Review Partner", "Quality Reviewer"],
        "frontend-specialist" => &["Frontend Engineer", "UI Builder", "Interaction Engineer"],
        "backend-specialist" => &["Backend Engineer", "Server Engineer", "Protocol Engineer"],
        "test-author-qa" => &["Test Engineer", "QA Partner", "Regression Tester"],
        "bug-hunter-debugger" => &["Bug Hunter", "Root Cause Debugger", "Fix Investigator"],
        _ => &["Team Member", "Teammate", "Agent Teammate"],
    }
}

fn shuffled_member_name(member: &TeamDraftMember, seed: usize) -> String {
    let role_id = member
        .profile
        .as_ref()
        .and_then(|profile| profile.role_preset_id.as_ref());
    match role_id {
        Some(id) => shuffled_role_name(id, seed),
        None => {
            // No role preset on this draft member: fall back to a
            // role-agnostic variant list. role_name_variants(&unknown)
            // returns the same generic list for any non-catalog id.
            shuffled_role_name(&TeamRolePresetId(String::new()), seed)
        }
    }
}

fn draft_member_index(draft: &TeamDraft, member_id: &TeamDraftMemberId) -> Result<usize, String> {
    draft
        .members
        .iter()
        .position(|member| member.id == *member_id)
        .ok_or_else(|| format!("missing team draft member {member_id}"))
}

fn team_draft_update_id(payload: &TeamDraftUpdatePayload) -> &TeamDraftId {
    match payload {
        TeamDraftUpdatePayload::SetName { draft_id, .. }
        | TeamDraftUpdatePayload::ReplaceMember { draft_id, .. }
        | TeamDraftUpdatePayload::AddReport { draft_id }
        | TeamDraftUpdatePayload::RemoveMember { draft_id, .. }
        | TeamDraftUpdatePayload::SetMemberProfile { draft_id, .. } => draft_id,
    }
}

fn validate_draft_member_edit(edit: &TeamDraftMemberEdit) -> Result<(), String> {
    if edit.id.0.trim().is_empty() {
        return Err("team draft member id must not be empty".to_owned());
    }
    Ok(())
}

/// Apply user-supplied editable fields onto the existing draft member.
/// Server-owned fields — `id`, `org_role`, `profile` — are intentionally
/// not overwritten; profile changes only flow through
/// `SetMemberProfile` / shuffle / template-apply, and `org_role` is fixed
/// by the slot the member was created in.
fn apply_draft_member_edit(member: &mut TeamDraftMember, edit: TeamDraftMemberEdit) {
    member.name = edit.name;
    member.description = edit.description;
    member.custom_agent_id = edit.custom_agent_id;
    member.backend_kind = edit.backend_kind;
    member.cost_hint = edit.cost_hint;
    member.project_ids = edit.project_ids;
}

fn validate_draft_org(members: &[TeamDraftMember]) -> Result<(), String> {
    let manager_count = members
        .iter()
        .filter(|member| member.org_role == TeamMemberRole::Manager)
        .count();
    if manager_count != 1 {
        return Err(format!(
            "team draft must have exactly one manager, got {manager_count}"
        ));
    }
    Ok(())
}

fn team_create_from_draft(draft: &TeamDraft) -> Result<TeamCreateFromDraftPayload, String> {
    if draft.name.trim().is_empty() {
        return Err("team draft name must not be empty".to_owned());
    }
    validate_draft_org(&draft.members)?;
    let mut manager = None;
    let mut reports = Vec::new();
    for member in &draft.members {
        let spec = draft_member_create_spec(member)?;
        match member.org_role {
            TeamMemberRole::Manager => manager = Some(spec),
            TeamMemberRole::Report => reports.push(spec),
        }
    }
    let manager = manager.ok_or_else(|| "team draft manager is missing".to_owned())?;
    Ok(TeamCreateFromDraftPayload {
        name: draft.name.trim().to_owned(),
        manager,
        reports,
    })
}

fn draft_member_create_spec(member: &TeamDraftMember) -> Result<TeamMemberCreateSpec, String> {
    let backend_kind = member.backend_kind.ok_or_else(|| {
        format!(
            "team draft member {} must choose a backend before commit",
            member.id
        )
    })?;
    Ok(TeamMemberCreateSpec {
        name: member.name.trim().to_owned(),
        description: member.description.trim().to_owned(),
        profile: member.profile.clone(),
        custom_agent_id: member.custom_agent_id.clone(),
        backend_kind,
        cost_hint: member.cost_hint,
        project_ids: member.project_ids.clone(),
    })
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
    use protocol::{
        BackendKind, CustomAgentId, ProjectId, TeamCreatePayload, TeamMemberCreateSpec,
    };
    use tempfile::TempDir;

    fn refs() -> AgentTeamValidationRefs {
        let (role_preset_ids, personality_preset_ids) = team_preset_validation_refs();
        AgentTeamValidationRefs {
            custom_agent_ids: [CustomAgentId("custom-1".to_owned())].into_iter().collect(),
            project_ids: [ProjectId("project-1".to_owned())].into_iter().collect(),
            enabled_backend_kinds: [BackendKind::Claude, BackendKind::Codex]
                .into_iter()
                .collect(),
            role_preset_ids,
            personality_preset_ids,
            legacy_backend_kind: Some(BackendKind::Claude),
        }
    }

    fn manager_spec() -> TeamMemberCreateSpec {
        TeamMemberCreateSpec {
            name: "Manager".to_owned(),
            description: "Coordinates".to_owned(),
            profile: None,
            custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            project_ids: vec![ProjectId("project-1".to_owned())],
        }
    }

    fn report_spec(name: &str) -> TeamMemberCreateSpec {
        TeamMemberCreateSpec {
            name: name.to_owned(),
            description: format!("{name} description"),
            profile: None,
            custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
            backend_kind: BackendKind::Claude,
            cost_hint: None,
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
            drafts: Vec::new(),
            bindings,
            pending_activations: HashMap::new(),
            shuffle_counter: 0,
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
