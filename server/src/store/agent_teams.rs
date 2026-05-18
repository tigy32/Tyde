use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{
    BackendKind, CustomAgentId, ProjectId, SessionId, Team, TeamCreateFromDraftPayload,
    TeamCreatePayload, TeamId, TeamMember, TeamMemberCreatePayload, TeamMemberDeletePayload,
    TeamMemberId, TeamMemberPresetProfile, TeamMemberRole, TeamMemberState,
    TeamMemberUpdatePayload, TeamPersonalityPresetId, TeamRenamePayload, TeamRolePresetId,
    TeamSetManagerPayload,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const STORE_VERSION: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct AgentTeamValidationRefs {
    pub custom_agent_ids: HashSet<CustomAgentId>,
    pub project_ids: HashSet<ProjectId>,
    pub enabled_backend_kinds: HashSet<BackendKind>,
    pub role_preset_ids: HashSet<TeamRolePresetId>,
    pub personality_preset_ids: HashSet<TeamPersonalityPresetId>,
    pub legacy_backend_kind: Option<BackendKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTeamsStoreFile {
    pub version: u32,
    pub teams: HashMap<TeamId, Team>,
    pub members: HashMap<TeamMemberId, TeamMember>,
}

impl Default for AgentTeamsStoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            teams: HashMap::new(),
            members: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct AgentTeamsStore {
    path: PathBuf,
    file: AgentTeamsStoreFile,
}

impl AgentTeamsStore {
    pub fn load(path: PathBuf, refs: &AgentTeamValidationRefs) -> Result<Self, String> {
        let file = Self::read_from_disk(&path, refs)?;
        validate_store_file(&file, refs)?;
        Ok(Self { path, file })
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_AGENT_TEAMS_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = std::env::var("HOME").map_err(|_| "Cannot determine HOME directory")?;
        Ok(PathBuf::from(home).join(".tyde").join("agent_teams.json"))
    }

    pub fn snapshot(&self) -> AgentTeamsStoreFile {
        self.file.clone()
    }

    pub fn teams(&self) -> Vec<Team> {
        let mut teams = self.file.teams.values().cloned().collect::<Vec<_>>();
        teams.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then(left.id.0.cmp(&right.id.0))
        });
        teams
    }

    pub fn members(&self) -> Vec<TeamMember> {
        let mut members = self.file.members.values().cloned().collect::<Vec<_>>();
        members.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then(left.id.0.cmp(&right.id.0))
        });
        members
    }

    pub fn get_team(&self, id: &TeamId) -> Option<Team> {
        self.file.teams.get(id).cloned()
    }

    pub fn get_member(&self, id: &TeamMemberId) -> Option<TeamMember> {
        self.file.members.get(id).cloned()
    }

    pub fn members_for_team(&self, team_id: &TeamId) -> Vec<TeamMember> {
        let mut members = self
            .file
            .members
            .values()
            .filter(|member| &member.team_id == team_id)
            .cloned()
            .collect::<Vec<_>>();
        members.sort_by(|left, right| {
            member_role_sort_key(left.role)
                .cmp(&member_role_sort_key(right.role))
                .then(left.created_at_ms.cmp(&right.created_at_ms))
                .then(left.id.0.cmp(&right.id.0))
        });
        members
    }

    pub fn create_team(
        &mut self,
        payload: TeamCreatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<(Team, TeamMember), String> {
        validate_team_name(&payload.name)?;
        validate_member_create_fields(&payload.manager)?;
        validate_custom_agent_ref(payload.manager.custom_agent_id.as_ref(), refs)?;
        validate_backend_kind(payload.manager.backend_kind, refs)?;
        validate_member_profile(payload.manager.profile.as_ref(), refs)?;
        validate_project_refs(&payload.manager.project_ids, refs)?;

        let now = now_ms()?;
        let team_id = TeamId(Uuid::new_v4().to_string());
        let manager_member_id = TeamMemberId(Uuid::new_v4().to_string());
        let team = Team {
            id: team_id.clone(),
            name: payload.name,
            manager_member_id: manager_member_id.clone(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        let manager = TeamMember {
            id: manager_member_id,
            team_id: team_id.clone(),
            role: TeamMemberRole::Manager,
            state: TeamMemberState::Active,
            name: payload.manager.name,
            description: payload.manager.description,
            profile: payload.manager.profile,
            custom_agent_id: payload.manager.custom_agent_id,
            backend_kind: payload.manager.backend_kind,
            cost_hint: payload.manager.cost_hint,
            session_id: None,
            project_ids: payload.manager.project_ids,
            created_at_ms: now,
            updated_at_ms: now,
        };

        insert_unique_team(&mut self.file.teams, team.clone())?;
        insert_unique_member(&mut self.file.members, manager.clone())?;
        self.validate_and_save(refs)?;
        Ok((team, manager))
    }

    pub fn create_team_from_draft(
        &mut self,
        payload: TeamCreateFromDraftPayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<(Team, Vec<TeamMember>), String> {
        validate_team_name(&payload.name)?;
        validate_member_create_fields(&payload.manager)?;
        validate_custom_agent_ref(payload.manager.custom_agent_id.as_ref(), refs)?;
        validate_backend_kind(payload.manager.backend_kind, refs)?;
        validate_member_profile(payload.manager.profile.as_ref(), refs)?;
        validate_project_refs(&payload.manager.project_ids, refs)?;
        for report in &payload.reports {
            validate_member_create_fields(report)?;
            validate_custom_agent_ref(report.custom_agent_id.as_ref(), refs)?;
            validate_backend_kind(report.backend_kind, refs)?;
            validate_member_profile(report.profile.as_ref(), refs)?;
            validate_project_refs(&report.project_ids, refs)?;
        }

        let now = now_ms()?;
        let team_id = TeamId(Uuid::new_v4().to_string());
        let manager_member_id = TeamMemberId(Uuid::new_v4().to_string());
        let team = Team {
            id: team_id.clone(),
            name: payload.name,
            manager_member_id: manager_member_id.clone(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        let manager = TeamMember {
            id: manager_member_id,
            team_id: team_id.clone(),
            role: TeamMemberRole::Manager,
            state: TeamMemberState::Active,
            name: payload.manager.name,
            description: payload.manager.description,
            profile: payload.manager.profile,
            custom_agent_id: payload.manager.custom_agent_id,
            backend_kind: payload.manager.backend_kind,
            cost_hint: payload.manager.cost_hint,
            session_id: None,
            project_ids: payload.manager.project_ids,
            created_at_ms: now,
            updated_at_ms: now,
        };

        let mut members = vec![manager];
        for report in payload.reports {
            members.push(TeamMember {
                id: TeamMemberId(Uuid::new_v4().to_string()),
                team_id: team_id.clone(),
                role: TeamMemberRole::Report,
                state: TeamMemberState::Active,
                name: report.name,
                description: report.description,
                profile: report.profile,
                custom_agent_id: report.custom_agent_id,
                backend_kind: report.backend_kind,
                cost_hint: report.cost_hint,
                session_id: None,
                project_ids: report.project_ids,
                created_at_ms: now,
                updated_at_ms: now,
            });
        }

        insert_unique_team(&mut self.file.teams, team.clone())?;
        for member in &members {
            insert_unique_member(&mut self.file.members, member.clone())?;
        }
        self.validate_and_save(refs)?;
        Ok((team, members))
    }

    pub fn rename_team(
        &mut self,
        payload: TeamRenamePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<Team, String> {
        self.assert_team_active(&payload.id)?;
        validate_team_name(&payload.name)?;
        let team = self
            .file
            .teams
            .get_mut(&payload.id)
            .ok_or_else(|| format!("cannot rename missing team {}", payload.id))?;
        team.name = payload.name;
        team.updated_at_ms = now_ms()?;
        let updated = team.clone();
        self.validate_and_save(refs)?;
        Ok(updated)
    }

    pub fn delete_team(
        &mut self,
        id: &TeamId,
        refs: &AgentTeamValidationRefs,
    ) -> Result<(Team, Vec<TeamMember>), String> {
        let team = self
            .file
            .teams
            .remove(id)
            .ok_or_else(|| format!("cannot delete missing team {id}"))?;
        let mut members = self
            .file
            .members
            .values()
            .filter(|member| member.team_id == *id)
            .cloned()
            .collect::<Vec<_>>();
        members.sort_by(|left, right| {
            member_role_sort_key(left.role)
                .cmp(&member_role_sort_key(right.role))
                .then(left.created_at_ms.cmp(&right.created_at_ms))
                .then(left.id.0.cmp(&right.id.0))
        });
        for member in &members {
            self.file.members.remove(&member.id);
        }
        self.validate_and_save(refs)?;
        Ok((team, members))
    }

    pub fn set_manager(
        &mut self,
        payload: TeamSetManagerPayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<(Team, TeamMember, TeamMember), String> {
        self.assert_team_active(&payload.team_id)?;
        let team = self
            .file
            .teams
            .get(&payload.team_id)
            .cloned()
            .ok_or_else(|| format!("cannot set manager for missing team {}", payload.team_id))?;
        if team.manager_member_id == payload.new_manager_member_id {
            return Err(format!(
                "member {} is already the manager for team {}",
                payload.new_manager_member_id, payload.team_id
            ));
        }
        let old_manager_id = team.manager_member_id.clone();
        let new_manager = self
            .file
            .members
            .get(&payload.new_manager_member_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "cannot set missing member {} as manager for team {}",
                    payload.new_manager_member_id, payload.team_id
                )
            })?;
        if new_manager.team_id != payload.team_id {
            return Err(format!(
                "member {} does not belong to team {}",
                payload.new_manager_member_id, payload.team_id
            ));
        }
        if new_manager.role != TeamMemberRole::Report
            || new_manager.state != TeamMemberState::Active
        {
            return Err(format!(
                "new manager {} must be an active report",
                payload.new_manager_member_id
            ));
        }

        let now = now_ms()?;
        let old_manager = self.file.members.get_mut(&old_manager_id).ok_or_else(|| {
            format!(
                "team {} has missing manager {}",
                payload.team_id, old_manager_id
            )
        })?;
        old_manager.role = TeamMemberRole::Report;
        old_manager.updated_at_ms = now;
        let old_manager = old_manager.clone();

        let new_manager = self
            .file
            .members
            .get_mut(&payload.new_manager_member_id)
            .ok_or_else(|| {
                format!(
                    "cannot set missing member {} as manager for team {}",
                    payload.new_manager_member_id, payload.team_id
                )
            })?;
        new_manager.role = TeamMemberRole::Manager;
        new_manager.updated_at_ms = now;
        let new_manager = new_manager.clone();

        let team =
            self.file.teams.get_mut(&payload.team_id).ok_or_else(|| {
                format!("cannot set manager for missing team {}", payload.team_id)
            })?;
        team.manager_member_id = payload.new_manager_member_id;
        team.updated_at_ms = now;
        let team = team.clone();

        self.validate_and_save(refs)?;
        Ok((team, old_manager, new_manager))
    }

    pub fn create_member(
        &mut self,
        payload: TeamMemberCreatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamMember, String> {
        self.assert_team_active(&payload.team_id)?;
        if payload.session_id.is_some() {
            return Err("team_member_create session_id must be absent".to_string());
        }
        validate_member_create_fields(&payload.member)?;
        validate_custom_agent_ref(payload.member.custom_agent_id.as_ref(), refs)?;
        validate_backend_kind(payload.member.backend_kind, refs)?;
        validate_member_profile(payload.member.profile.as_ref(), refs)?;
        validate_project_refs(&payload.member.project_ids, refs)?;

        let now = now_ms()?;
        let member = TeamMember {
            id: TeamMemberId(Uuid::new_v4().to_string()),
            team_id: payload.team_id,
            role: TeamMemberRole::Report,
            state: TeamMemberState::Active,
            name: payload.member.name,
            description: payload.member.description,
            profile: payload.member.profile,
            custom_agent_id: payload.member.custom_agent_id,
            backend_kind: payload.member.backend_kind,
            cost_hint: payload.member.cost_hint,
            session_id: None,
            project_ids: payload.member.project_ids,
            created_at_ms: now,
            updated_at_ms: now,
        };
        insert_unique_member(&mut self.file.members, member.clone())?;
        self.validate_and_save(refs)?;
        Ok(member)
    }

    pub fn update_member(
        &mut self,
        payload: TeamMemberUpdatePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamMember, String> {
        let member = self
            .file
            .members
            .get(&payload.id)
            .cloned()
            .ok_or_else(|| format!("cannot update missing team member {}", payload.id))?;
        self.assert_team_active(&member.team_id)?;
        validate_member_name(&payload.name)?;
        validate_member_description(&payload.description)?;
        validate_member_profile(payload.profile.as_ref(), refs)?;
        validate_project_ids(&payload.project_ids)?;
        validate_project_refs(&payload.project_ids, refs)?;

        let member = self
            .file
            .members
            .get_mut(&payload.id)
            .ok_or_else(|| format!("cannot update missing team member {}", payload.id))?;
        member.name = payload.name;
        member.description = payload.description;
        member.profile = payload.profile;
        member.project_ids = payload.project_ids;
        member.updated_at_ms = now_ms()?;
        let updated = member.clone();
        self.validate_and_save(refs)?;
        Ok(updated)
    }

    pub fn delete_member(
        &mut self,
        payload: TeamMemberDeletePayload,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamMember, String> {
        let member = self
            .file
            .members
            .get(&payload.id)
            .cloned()
            .ok_or_else(|| format!("cannot delete missing team member {}", payload.id))?;
        self.assert_team_active(&member.team_id)?;
        let team = self.file.teams.get(&member.team_id).ok_or_else(|| {
            format!(
                "member {} references missing team {}",
                member.id, member.team_id
            )
        })?;
        if team.manager_member_id == member.id {
            return Err(format!("cannot delete active manager {}", member.id));
        }
        let member_count = self
            .file
            .members
            .values()
            .filter(|candidate| candidate.team_id == member.team_id)
            .count();
        if member_count <= 1 {
            return Err(format!(
                "cannot delete only member {} from team {}; delete the team instead",
                member.id, member.team_id
            ));
        }

        let deleted = self
            .file
            .members
            .remove(&payload.id)
            .ok_or_else(|| format!("cannot delete missing team member {}", payload.id))?;
        self.validate_and_save(refs)?;
        Ok(deleted)
    }

    pub fn set_member_session_id(
        &mut self,
        member_id: &TeamMemberId,
        session_id: SessionId,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamMember, String> {
        if let Some(existing_owner) = self.file.members.values().find(|member| {
            member.id != *member_id && member.session_id.as_ref() == Some(&session_id)
        }) {
            return Err(format!(
                "session {} is already owned by team member {}",
                session_id, existing_owner.id
            ));
        }

        let member = self
            .file
            .members
            .get_mut(member_id)
            .ok_or_else(|| format!("cannot set session for missing team member {member_id}"))?;
        if member.session_id.is_some() {
            return Err(format!("team member {member_id} already has a session_id"));
        }
        member.session_id = Some(session_id);
        member.updated_at_ms = now_ms()?;
        let updated = member.clone();
        self.validate_and_save(refs)?;
        Ok(updated)
    }

    pub fn replace_member_session_id(
        &mut self,
        member_id: &TeamMemberId,
        old_session_id: &SessionId,
        new_session_id: SessionId,
        refs: &AgentTeamValidationRefs,
    ) -> Result<TeamMember, String> {
        if let Some(existing_owner) = self.file.members.values().find(|member| {
            member.id != *member_id && member.session_id.as_ref() == Some(&new_session_id)
        }) {
            return Err(format!(
                "session {} is already owned by team member {}",
                new_session_id, existing_owner.id
            ));
        }

        let member =
            self.file.members.get_mut(member_id).ok_or_else(|| {
                format!("cannot replace session for missing team member {member_id}")
            })?;
        if member.session_id.as_ref() != Some(old_session_id) {
            return Err(format!(
                "team member {member_id} session_id {:?} does not match expected {old_session_id}",
                member.session_id
            ));
        }
        member.session_id = Some(new_session_id);
        member.updated_at_ms = now_ms()?;
        let updated = member.clone();
        self.validate_and_save(refs)?;
        Ok(updated)
    }

    pub fn clear_member_session_id(
        &mut self,
        member_id: &TeamMemberId,
        refs: &AgentTeamValidationRefs,
    ) -> Result<Option<TeamMember>, String> {
        let member =
            self.file.members.get_mut(member_id).ok_or_else(|| {
                format!("cannot clear session for missing team member {member_id}")
            })?;
        if member.session_id.is_none() {
            return Ok(None);
        }
        member.session_id = None;
        member.updated_at_ms = now_ms()?;
        let updated = member.clone();
        self.validate_and_save(refs)?;
        Ok(Some(updated))
    }

    fn assert_team_active(&self, team_id: &TeamId) -> Result<(), String> {
        self.file
            .teams
            .get(team_id)
            .ok_or_else(|| format!("missing team {team_id}"))?;
        Ok(())
    }

    fn validate_and_save(&self, refs: &AgentTeamValidationRefs) -> Result<(), String> {
        validate_store_file(&self.file, refs)?;
        Self::save(&self.path, &self.file)
    }

    fn read_from_disk(
        path: &Path,
        refs: &AgentTeamValidationRefs,
    ) -> Result<AgentTeamsStoreFile, String> {
        match std::fs::read_to_string(path) {
            Ok(contents) => migrate_store_file(path, &contents, refs),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(AgentTeamsStoreFile::default())
            }
            Err(err) => Err(format!(
                "Failed to read agent teams store {}: {err}",
                path.display()
            )),
        }
    }

    fn save(path: &Path, file: &AgentTeamsStoreFile) -> Result<(), String> {
        let json = serde_json::to_string_pretty(file)
            .map_err(|err| format!("Failed to serialize agent teams store: {err}"))?;

        let parent = path
            .parent()
            .ok_or_else(|| format!("Agent teams store path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create agent teams store directory: {err}"))?;

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "Agent teams store path has no file name: {}",
                    path.display()
                )
            })?;
        let tmp_path = parent.join(format!(".{file_name}.tmp.{}", now_ms()?));
        let mut tmp_file = std::fs::File::create(&tmp_path)
            .map_err(|err| format!("Failed to create temp agent teams store file: {err}"))?;
        tmp_file
            .write_all(json.as_bytes())
            .map_err(|err| format!("Failed to write temp agent teams store file: {err}"))?;
        tmp_file
            .sync_all()
            .map_err(|err| format!("Failed to sync temp agent teams store file: {err}"))?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            format!(
                "Failed to atomically replace agent teams store {}: {err}",
                path.display()
            )
        })
    }
}

pub fn validate_store_file(
    file: &AgentTeamsStoreFile,
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    if file.version != STORE_VERSION {
        return Err(format!(
            "agent teams store version must be {STORE_VERSION}, got {}",
            file.version
        ));
    }

    for (id, team) in &file.teams {
        if id != &team.id {
            return Err(format!(
                "team map key {} does not match team id {}",
                id, team.id
            ));
        }
        validate_team(team)?;
    }

    let mut session_ids = HashSet::new();
    for (id, member) in &file.members {
        if id != &member.id {
            return Err(format!(
                "member map key {} does not match member id {}",
                id, member.id
            ));
        }
        validate_member(member)?;
        if !file.teams.contains_key(&member.team_id) {
            return Err(format!(
                "member {} references missing team {}",
                member.id, member.team_id
            ));
        }
        validate_custom_agent_ref(member.custom_agent_id.as_ref(), refs)?;
        validate_backend_kind(member.backend_kind, refs)?;
        validate_member_profile(member.profile.as_ref(), refs)?;
        validate_project_refs(&member.project_ids, refs)?;
        if let Some(session_id) = member.session_id.as_ref()
            && !session_ids.insert(session_id.clone())
        {
            return Err(format!(
                "session {session_id} is owned by multiple team members"
            ));
        }
    }

    for team in file.teams.values() {
        let team_members = file
            .members
            .values()
            .filter(|member| member.team_id == team.id)
            .collect::<Vec<_>>();
        let active_managers = team_members
            .iter()
            .filter(|member| {
                member.role == TeamMemberRole::Manager && member.state == TeamMemberState::Active
            })
            .collect::<Vec<_>>();
        if active_managers.len() != 1 {
            return Err(format!(
                "team {} must have exactly one active manager, got {}",
                team.id,
                active_managers.len()
            ));
        }
        let manager = file.members.get(&team.manager_member_id).ok_or_else(|| {
            format!(
                "team {} manager_member_id {} does not resolve",
                team.id, team.manager_member_id
            )
        })?;
        if manager.team_id != team.id {
            return Err(format!(
                "team {} manager {} belongs to team {}",
                team.id, manager.id, manager.team_id
            ));
        }
        if manager.role != TeamMemberRole::Manager || manager.state != TeamMemberState::Active {
            return Err(format!(
                "team {} manager {} must be active manager",
                team.id, manager.id
            ));
        }
    }

    Ok(())
}

fn migrate_store_file(
    path: &Path,
    contents: &str,
    refs: &AgentTeamValidationRefs,
) -> Result<AgentTeamsStoreFile, String> {
    let mut value = serde_json::from_str::<Value>(contents).map_err(|err| {
        format!(
            "Failed to parse agent teams store {}: {err}",
            path.display()
        )
    })?;
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            format!(
                "Failed to parse agent teams store {}: missing version",
                path.display()
            )
        })?;
    match version {
        1 => {
            migrate_v1_to_v2(path, &mut value)?;
            migrate_v2_to_v3(path, &mut value)?;
            migrate_v3_to_v4(path, &mut value, refs)?;
            migrate_v4_to_v5(&mut value);
        }
        2 => {
            migrate_v2_to_v3(path, &mut value)?;
            migrate_v3_to_v4(path, &mut value, refs)?;
            migrate_v4_to_v5(&mut value);
        }
        3 => {
            migrate_v3_to_v4(path, &mut value, refs)?;
            migrate_v4_to_v5(&mut value);
        }
        4 => migrate_v4_to_v5(&mut value),
        version if version == u64::from(STORE_VERSION) => {}
        other => {
            return Err(format!(
                "agent teams store version must be {STORE_VERSION}, got {other}"
            ));
        }
    }
    serde_json::from_value::<AgentTeamsStoreFile>(value).map_err(|err| {
        format!(
            "Failed to parse agent teams store {}: {err}",
            path.display()
        )
    })
}

fn migrate_v1_to_v2(path: &Path, value: &mut Value) -> Result<(), String> {
    let teams = value
        .get_mut("teams")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: teams must be an object",
                path.display()
            )
        })?;
    let mut deleted_team_ids = HashSet::new();
    teams.retain(|team_id, team| {
        let archived = team
            .get("archived_at_ms")
            .is_some_and(|archived_at_ms| !archived_at_ms.is_null());
        if archived {
            tracing::warn!(
                store_path = %path.display(),
                team_id = %team_id,
                "dropping archived team while migrating agent teams store to v2"
            );
            deleted_team_ids.insert(team_id.clone());
            return false;
        }
        if let Some(team) = team.as_object_mut() {
            team.remove("archived_at_ms");
        }
        true
    });

    let members = value
        .get_mut("members")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: members must be an object",
                path.display()
            )
        })?;
    members.retain(|member_id, member| {
        let team_id = member.get("team_id").and_then(Value::as_str);
        if team_id.is_some_and(|team_id| deleted_team_ids.contains(team_id)) {
            tracing::warn!(
                store_path = %path.display(),
                member_id = %member_id,
                "dropping member of archived team while migrating agent teams store to v2"
            );
            return false;
        }
        if member.get("state").and_then(Value::as_str) == Some("archived") {
            tracing::warn!(
                store_path = %path.display(),
                member_id = %member_id,
                "dropping archived member while migrating agent teams store to v2"
            );
            return false;
        }
        true
    });
    value["version"] = Value::from(2);
    Ok(())
}

fn migrate_v2_to_v3(path: &Path, value: &mut Value) -> Result<(), String> {
    let members = value
        .get_mut("members")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: members must be an object",
                path.display()
            )
        })?;
    let mut kept_member_ids = HashSet::new();
    members.retain(|member_id, member| {
        let Some(project_id) = member
            .get("project_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            tracing::warn!(
                store_path = %path.display(),
                member_id = %member_id,
                "dropping member without project_id while migrating agent teams store to v3"
            );
            return false;
        };
        if let Some(member) = member.as_object_mut() {
            member.insert(
                "project_ids".to_string(),
                Value::Array(vec![Value::String(project_id)]),
            );
            member.remove("project_id");
            member.remove("workspace_roots");
        }
        kept_member_ids.insert(member_id.clone());
        true
    });

    let teams = value
        .get_mut("teams")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: teams must be an object",
                path.display()
            )
        })?;
    let mut deleted_team_ids = HashSet::new();
    teams.retain(|team_id, team| {
        let manager_id = team.get("manager_member_id").and_then(Value::as_str);
        if manager_id.is_some_and(|manager_id| kept_member_ids.contains(manager_id)) {
            return true;
        }
        tracing::warn!(
            store_path = %path.display(),
            team_id = %team_id,
            "dropping team without migrated manager while migrating agent teams store to v3"
        );
        deleted_team_ids.insert(team_id.clone());
        false
    });

    let members = value
        .get_mut("members")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: members must be an object",
                path.display()
            )
        })?;
    members.retain(|member_id, member| {
        let team_id = member.get("team_id").and_then(Value::as_str);
        if team_id.is_some_and(|team_id| deleted_team_ids.contains(team_id)) {
            tracing::warn!(
                store_path = %path.display(),
                member_id = %member_id,
                "dropping member of unmigrated team while migrating agent teams store to v3"
            );
            return false;
        }
        true
    });
    value["version"] = Value::from(3);
    Ok(())
}

fn migrate_v3_to_v4(
    path: &Path,
    value: &mut Value,
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    let members = value
        .get_mut("members")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: members must be an object",
                path.display()
            )
        })?;
    for member in members.values_mut() {
        let member = member.as_object_mut().ok_or_else(|| {
            format!(
                "Failed to migrate agent teams store {}: member must be an object",
                path.display()
            )
        })?;
        if !member.contains_key("backend_kind") {
            let backend_kind = refs.legacy_backend_kind.ok_or_else(|| {
                format!(
                    "Failed to migrate agent teams store {} to v4: legacy team members require a host default_backend",
                    path.display()
                )
            })?;
            let backend_kind = serde_json::to_value(backend_kind).map_err(|err| {
                format!(
                    "Failed to serialize legacy backend_kind while migrating agent teams store {} to v4: {err}",
                    path.display()
                )
            })?;
            member.insert("backend_kind".to_string(), backend_kind);
        }
    }
    value["version"] = Value::from(4);
    Ok(())
}

fn migrate_v4_to_v5(value: &mut Value) {
    value["version"] = Value::from(STORE_VERSION);
}

fn insert_unique_team(records: &mut HashMap<TeamId, Team>, team: Team) -> Result<(), String> {
    if records.insert(team.id.clone(), team.clone()).is_some() {
        return Err(format!("generated duplicate team id {}", team.id));
    }
    Ok(())
}

fn insert_unique_member(
    records: &mut HashMap<TeamMemberId, TeamMember>,
    member: TeamMember,
) -> Result<(), String> {
    if records.insert(member.id.clone(), member.clone()).is_some() {
        return Err(format!("generated duplicate team member id {}", member.id));
    }
    Ok(())
}

fn validate_team(team: &Team) -> Result<(), String> {
    validate_id("team id", &team.id.0)?;
    validate_team_name(&team.name)?;
    validate_id("team manager_member_id", &team.manager_member_id.0)
}

fn validate_member(member: &TeamMember) -> Result<(), String> {
    validate_id("team member id", &member.id.0)?;
    validate_id("team member team_id", &member.team_id.0)?;
    validate_member_name(&member.name)?;
    validate_member_description(&member.description)?;
    if let Some(custom_agent_id) = member.custom_agent_id.as_ref() {
        validate_id("team member custom_agent_id", &custom_agent_id.0)?;
    }
    validate_profile_ids(member.profile.as_ref())?;
    validate_project_ids(&member.project_ids)?;
    Ok(())
}

fn validate_member_create_fields(member: &protocol::TeamMemberCreateSpec) -> Result<(), String> {
    validate_member_name(&member.name)?;
    validate_member_description(&member.description)?;
    validate_profile_ids(member.profile.as_ref())?;
    validate_project_ids(&member.project_ids)
}

fn validate_team_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("team name must not be empty".to_string());
    }
    Ok(())
}

fn validate_member_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("team member name must not be empty".to_string());
    }
    Ok(())
}

fn validate_member_description(description: &str) -> Result<(), String> {
    if description.trim().is_empty() {
        return Err("team member description must not be empty".to_string());
    }
    Ok(())
}

fn validate_id(label: &str, id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    Ok(())
}

fn validate_custom_agent_ref(
    custom_agent_id: Option<&CustomAgentId>,
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    let Some(custom_agent_id) = custom_agent_id else {
        return Ok(());
    };
    if !refs.custom_agent_ids.contains(custom_agent_id) {
        return Err(format!(
            "team member references missing custom agent {}",
            custom_agent_id
        ));
    }
    Ok(())
}

fn validate_backend_kind(
    backend_kind: BackendKind,
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    if !refs.enabled_backend_kinds.contains(&backend_kind) {
        return Err(format!(
            "team member references disabled backend {:?}",
            backend_kind
        ));
    }
    Ok(())
}

fn validate_member_profile(
    profile: Option<&TeamMemberPresetProfile>,
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    validate_profile_ids(profile)?;
    let Some(profile) = profile else {
        return Ok(());
    };
    if let Some(role_preset_id) = profile.role_preset_id.as_ref()
        && !refs.role_preset_ids.contains(role_preset_id)
    {
        return Err(format!(
            "team member profile references missing role preset {}",
            role_preset_id
        ));
    }
    if let Some(personality_preset_id) = profile.personality_preset_id.as_ref()
        && !refs.personality_preset_ids.contains(personality_preset_id)
    {
        return Err(format!(
            "team member profile references missing personality preset {}",
            personality_preset_id
        ));
    }
    Ok(())
}

fn validate_profile_ids(profile: Option<&TeamMemberPresetProfile>) -> Result<(), String> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if let Some(role_preset_id) = profile.role_preset_id.as_ref() {
        validate_id("team member role_preset_id", &role_preset_id.0)?;
    }
    if let Some(personality_preset_id) = profile.personality_preset_id.as_ref() {
        validate_id(
            "team member personality_preset_id",
            &personality_preset_id.0,
        )?;
    }
    Ok(())
}

fn validate_project_ids(project_ids: &[ProjectId]) -> Result<(), String> {
    if project_ids.is_empty() {
        return Err("team member project_ids must not be empty".to_string());
    }
    let mut seen = HashSet::new();
    for project_id in project_ids {
        validate_id("team member project_id", &project_id.0)?;
        if !seen.insert(project_id.clone()) {
            return Err(format!(
                "team member project_ids contains duplicate project {}",
                project_id
            ));
        }
    }
    Ok(())
}

fn validate_project_refs(
    project_ids: &[ProjectId],
    refs: &AgentTeamValidationRefs,
) -> Result<(), String> {
    for project_id in project_ids {
        if !refs.project_ids.contains(project_id) {
            return Err(format!(
                "team member references missing project {}",
                project_id
            ));
        }
    }
    Ok(())
}

fn member_role_sort_key(role: TeamMemberRole) -> u8 {
    match role {
        TeamMemberRole::Manager => 0,
        TeamMemberRole::Report => 1,
    }
}

fn now_ms() -> Result<u64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock is before UNIX epoch: {err}"))?;
    u64::try_from(duration.as_millis()).map_err(|_| "current time overflows u64 ms".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{TeamMemberCreateSpec, TeamMemberUpdatePayload};
    use serde_json::json;

    fn refs() -> AgentTeamValidationRefs {
        AgentTeamValidationRefs {
            custom_agent_ids: [CustomAgentId("custom-1".to_owned())].into_iter().collect(),
            project_ids: [ProjectId("project-1".to_owned())].into_iter().collect(),
            enabled_backend_kinds: [BackendKind::Claude, BackendKind::Codex]
                .into_iter()
                .collect(),
            role_preset_ids: [TeamRolePresetId("tech-lead-planner".to_owned())]
                .into_iter()
                .collect(),
            personality_preset_ids: [TeamPersonalityPresetId("careful-architect".to_owned())]
                .into_iter()
                .collect(),
            legacy_backend_kind: Some(BackendKind::Claude),
        }
    }

    fn manager_spec() -> TeamMemberCreateSpec {
        TeamMemberCreateSpec {
            name: "Manager".to_owned(),
            description: "Coordinates the team".to_owned(),
            profile: None,
            custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
            backend_kind: BackendKind::Claude,
            cost_hint: None,
            project_ids: vec![ProjectId("project-1".to_owned())],
        }
    }

    #[test]
    fn member_create_persists_and_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path.clone(), &refs).expect("load empty store");
        let (team, _manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");
        let member = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id.clone(),
                    member: TeamMemberCreateSpec {
                        name: "Frontend".to_owned(),
                        description: "Builds UI".to_owned(),
                        profile: None,
                        custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        project_ids: vec![ProjectId("project-1".to_owned())],
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect("create report");

        let loaded = AgentTeamsStore::load(path, &refs).expect("reload store");
        assert_eq!(loaded.get_team(&team.id), Some(team));
        assert_eq!(loaded.get_member(&member.id), Some(member));
    }

    fn write_v3_store(path: &Path) {
        let contents = json!({
            "version": 3,
            "teams": {
                "team-1": {
                    "id": "team-1",
                    "name": "Legacy Team",
                    "manager_member_id": "member-manager",
                    "created_at_ms": 1,
                    "updated_at_ms": 1
                }
            },
            "members": {
                "member-manager": {
                    "id": "member-manager",
                    "team_id": "team-1",
                    "role": "manager",
                    "state": "active",
                    "name": "Legacy Manager",
                    "description": "Coordinates legacy work",
                    "custom_agent_id": "custom-1",
                    "project_ids": ["project-1"],
                    "created_at_ms": 1,
                    "updated_at_ms": 1
                },
                "member-report": {
                    "id": "member-report",
                    "team_id": "team-1",
                    "role": "report",
                    "state": "active",
                    "name": "Legacy Report",
                    "description": "Handles legacy work",
                    "custom_agent_id": "custom-1",
                    "project_ids": ["project-1"],
                    "created_at_ms": 1,
                    "updated_at_ms": 1
                }
            }
        });
        std::fs::write(
            path,
            serde_json::to_string_pretty(&contents).expect("serialize v3 store"),
        )
        .expect("write v3 store");
    }

    fn write_v4_store(path: &Path) {
        let contents = json!({
            "version": 4,
            "teams": {
                "team-1": {
                    "id": "team-1",
                    "name": "V4 Team",
                    "manager_member_id": "member-manager",
                    "created_at_ms": 1,
                    "updated_at_ms": 1
                }
            },
            "members": {
                "member-manager": {
                    "id": "member-manager",
                    "team_id": "team-1",
                    "role": "manager",
                    "state": "active",
                    "name": "V4 Manager",
                    "description": "Coordinates v4 work",
                    "custom_agent_id": "custom-1",
                    "backend_kind": "claude",
                    "project_ids": ["project-1"],
                    "created_at_ms": 1,
                    "updated_at_ms": 1
                }
            }
        });
        std::fs::write(
            path,
            serde_json::to_string_pretty(&contents).expect("serialize v4 store"),
        )
        .expect("write v4 store");
    }

    #[test]
    fn v3_migration_assigns_legacy_default_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        write_v3_store(&path);
        let mut refs = refs();
        refs.legacy_backend_kind = Some(BackendKind::Codex);

        let store = AgentTeamsStore::load(path, &refs).expect("load migrated store");
        let snapshot = store.snapshot();

        assert_eq!(snapshot.version, STORE_VERSION);
        let manager = snapshot
            .members
            .get(&TeamMemberId("member-manager".to_owned()))
            .expect("migrated manager");
        let report = snapshot
            .members
            .get(&TeamMemberId("member-report".to_owned()))
            .expect("migrated report");
        assert_eq!(manager.backend_kind, BackendKind::Codex);
        assert_eq!(report.backend_kind, BackendKind::Codex);
        assert_eq!(manager.profile, None);
        assert_eq!(report.profile, None);
        assert_eq!(
            manager.custom_agent_id,
            Some(CustomAgentId("custom-1".to_owned()))
        );
        assert_eq!(
            report.custom_agent_id,
            Some(CustomAgentId("custom-1".to_owned()))
        );
    }

    #[test]
    fn v4_migration_defaults_member_profile_to_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        write_v4_store(&path);

        let store = AgentTeamsStore::load(path, &refs()).expect("load migrated v4 store");
        let snapshot = store.snapshot();

        assert_eq!(snapshot.version, STORE_VERSION);
        let manager = snapshot
            .members
            .get(&TeamMemberId("member-manager".to_owned()))
            .expect("migrated manager");
        assert_eq!(manager.backend_kind, BackendKind::Claude);
        assert_eq!(manager.profile, None);
    }

    #[test]
    fn v3_migration_rejects_missing_legacy_default_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        write_v3_store(&path);
        let mut refs = refs();
        refs.legacy_backend_kind = None;

        let err = AgentTeamsStore::load(path, &refs).expect_err("migration should fail loudly");
        assert!(
            err.contains("legacy team members require a host default_backend"),
            "unexpected migration error: {err}"
        );
    }

    #[test]
    fn member_create_allows_no_custom_agent_with_explicit_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path.clone(), &refs).expect("load empty store");
        let (team, _manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");
        let member = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: TeamMemberCreateSpec {
                        name: "Default Agent".to_owned(),
                        description: "Uses the built-in agent profile".to_owned(),
                        profile: None,
                        custom_agent_id: None,
                        backend_kind: BackendKind::Codex,
                        cost_hint: Some(protocol::SpawnCostHint::Low),
                        project_ids: vec![ProjectId("project-1".to_owned())],
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect("create default-agent member");

        assert_eq!(member.custom_agent_id, None);
        assert_eq!(member.backend_kind, BackendKind::Codex);
        assert_eq!(member.cost_hint, Some(protocol::SpawnCostHint::Low));

        let loaded = AgentTeamsStore::load(path, &refs).expect("reload store");
        assert_eq!(loaded.get_member(&member.id), Some(member));
    }

    #[test]
    fn deleting_active_manager_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path, &refs).expect("load empty store");
        let (_team, manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");

        let err = store
            .delete_member(TeamMemberDeletePayload { id: manager.id }, &refs)
            .expect_err("active manager delete should fail");
        assert!(err.contains("active manager"));
    }

    #[test]
    fn set_member_session_id_rejects_duplicate_session_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path, &refs).expect("load empty store");
        let (team, _manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");
        let member_a = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id.clone(),
                    member: TeamMemberCreateSpec {
                        name: "A".to_owned(),
                        description: "Does A".to_owned(),
                        profile: None,
                        custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        project_ids: vec![ProjectId("project-1".to_owned())],
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect("create member a");
        let member_b = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: TeamMemberCreateSpec {
                        name: "B".to_owned(),
                        description: "Does B".to_owned(),
                        profile: None,
                        custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        project_ids: vec![ProjectId("project-1".to_owned())],
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect("create member b");
        let session_id = SessionId("session-1".to_owned());
        store
            .set_member_session_id(&member_a.id, session_id.clone(), &refs)
            .expect("set session");

        let err = store
            .set_member_session_id(&member_b.id, session_id, &refs)
            .expect_err("duplicate session should fail");
        assert!(err.contains("already owned"));
    }

    #[test]
    fn update_member_validates_project_references() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path, &refs).expect("load empty store");
        let (team, _manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");
        let member = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: TeamMemberCreateSpec {
                        name: "Frontend".to_owned(),
                        description: "Builds UI".to_owned(),
                        profile: None,
                        custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        project_ids: vec![ProjectId("project-1".to_owned())],
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect("create report");
        let err = store
            .update_member(
                TeamMemberUpdatePayload {
                    id: member.id,
                    name: "Frontend".to_owned(),
                    description: "Builds UI".to_owned(),
                    profile: None,
                    project_ids: vec![ProjectId("missing".to_owned())],
                },
                &refs,
            )
            .expect_err("missing project should fail");
        assert!(err.contains("missing project"));
    }

    #[test]
    fn create_member_rejects_empty_project_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_teams.json");
        let refs = refs();
        let mut store = AgentTeamsStore::load(path, &refs).expect("load empty store");
        let (team, _manager) = store
            .create_team(
                TeamCreatePayload {
                    name: "Product Team".to_owned(),
                    manager: manager_spec(),
                },
                &refs,
            )
            .expect("create team");

        let err = store
            .create_member(
                TeamMemberCreatePayload {
                    team_id: team.id,
                    member: TeamMemberCreateSpec {
                        name: "Frontend".to_owned(),
                        description: "Builds UI".to_owned(),
                        profile: None,
                        custom_agent_id: Some(CustomAgentId("custom-1".to_owned())),
                        backend_kind: BackendKind::Claude,
                        cost_hint: None,
                        project_ids: Vec::new(),
                    },
                    session_id: None,
                },
                &refs,
            )
            .expect_err("empty project_ids should fail");
        assert!(err.contains("project_ids must not be empty"));
    }
}
