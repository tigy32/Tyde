use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{
    AgentAnnotationTarget, AgentGroupMode, AgentId, AgentManualTagAssignment,
    AgentManualTagDescriptor, AgentManualTagId, AgentOrderKey, AgentOrigin, AgentPinsSnapshot,
    AgentPinsUpdate, AgentSortMode, AgentStatusFilter, AgentTagColor, AgentTagRef,
    AgentTagsSnapshot, AgentTagsUpdate, AgentsSmartViewsSnapshot, AgentsSmartViewsUpdate,
    AgentsViewFilters, AgentsViewPreferences, AgentsViewPreferencesSnapshot,
    AgentsViewPreferencesStoreError, AgentsViewPreferencesStoreErrorKind,
    AgentsViewPreferencesUpdate, BackendKind, BuiltInSmartViewId, HostFilterId, SessionId,
    SmartView, SmartViewId, UserSmartViewId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u32 = 3;
const SMART_VIEWS_STORE_VERSION: u32 = 2;
const LEGACY_STORE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    preferences: AgentsViewPreferences,
    #[serde(default)]
    smart_views: PersistedSmartViews,
    #[serde(default)]
    tags: PersistedTags,
    #[serde(default)]
    pins: AgentPinsSnapshot,
}

#[derive(Debug, Deserialize)]
struct LegacyStoreFile {
    preferences: AgentsViewPreferences,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedSmartViews {
    #[serde(default)]
    user: Vec<SmartView>,
    #[serde(default)]
    active_view_id: Option<SmartViewId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedTags {
    #[serde(default)]
    manual: Vec<AgentManualTagDescriptor>,
    #[serde(default)]
    manual_assignments: Vec<AgentManualTagAssignment>,
}

#[derive(Debug, Clone)]
struct StoreState {
    preferences: AgentsViewPreferences,
    user_smart_views: Vec<SmartView>,
    active_view_id: Option<SmartViewId>,
    manual_tags: Vec<AgentManualTagDescriptor>,
    manual_tag_assignments: Vec<AgentManualTagAssignment>,
    pins: AgentPinsSnapshot,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            preferences: AgentsViewPreferences::default(),
            user_smart_views: Vec::new(),
            active_view_id: Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)),
            manual_tags: Vec::new(),
            manual_tag_assignments: Vec::new(),
            pins: AgentPinsSnapshot::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SmartViewQuery {
    filters: AgentsViewFilters,
    sort_mode: AgentSortMode,
    group_mode: AgentGroupMode,
    hide_finished: bool,
}

#[derive(Debug)]
pub struct AgentsViewPreferencesStore {
    path: PathBuf,
    preferences: AgentsViewPreferences,
    user_smart_views: Vec<SmartView>,
    active_view_id: Option<SmartViewId>,
    manual_tags: Vec<AgentManualTagDescriptor>,
    manual_tag_assignments: Vec<AgentManualTagAssignment>,
    pins: AgentPinsSnapshot,
    load_error: Option<AgentsViewPreferencesStoreError>,
}

impl AgentsViewPreferencesStore {
    pub fn load(path: PathBuf) -> Self {
        match Self::read_from_disk(&path) {
            Ok(state) => Self {
                path,
                preferences: state.preferences,
                user_smart_views: state.user_smart_views,
                active_view_id: state.active_view_id,
                manual_tags: state.manual_tags,
                manual_tag_assignments: state.manual_tag_assignments,
                pins: state.pins,
                load_error: None,
            },
            Err(load_error) => {
                let state = StoreState::default();
                Self {
                    path,
                    preferences: state.preferences,
                    user_smart_views: state.user_smart_views,
                    active_view_id: state.active_view_id,
                    manual_tags: state.manual_tags,
                    manual_tag_assignments: state.manual_tag_assignments,
                    pins: state.pins,
                    load_error: Some(load_error),
                }
            }
        }
    }

    pub fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("TYDE_AGENTS_VIEW_PREFERENCES_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()?
            .join(".tyde")
            .join("agents_view_preferences.json"))
    }

    pub fn snapshot(&self) -> AgentsViewPreferencesSnapshot {
        AgentsViewPreferencesSnapshot {
            preferences: self.preferences.clone(),
            load_error: self.load_error.clone(),
            smart_views: self.smart_views_snapshot(),
            tags: self.tags_snapshot(),
            pins: self.pins.clone(),
        }
    }

    pub fn apply(
        &mut self,
        update: AgentsViewPreferencesUpdate,
    ) -> Result<AgentsViewPreferencesSnapshot, String> {
        let mut state = self.read_current_state_or_default();
        apply_update(&mut state, update)?;
        let state = validate_state(state)?;
        Self::save(&self.path, &state)?;
        self.set_state(state);
        self.load_error = None;
        Ok(self.snapshot())
    }

    pub fn apply_smart_views(
        &mut self,
        update: AgentsSmartViewsUpdate,
    ) -> Result<AgentsViewPreferencesSnapshot, String> {
        let mut state = self.read_current_state_or_default();
        apply_smart_views_update(&mut state, update)?;
        let state = validate_state(state)?;
        Self::save(&self.path, &state)?;
        self.set_state(state);
        self.load_error = None;
        Ok(self.snapshot())
    }

    pub fn apply_tags<F>(
        &mut self,
        update: AgentTagsUpdate,
        canonicalize_target: F,
    ) -> Result<AgentsViewPreferencesSnapshot, String>
    where
        F: FnMut(AgentAnnotationTarget) -> Result<Option<AgentAnnotationTarget>, String>,
    {
        let mut state = self.read_current_state_or_default();
        apply_tags_update(&mut state, update)?;
        canonicalize_annotation_targets(&mut state, canonicalize_target)?;
        let state = validate_state(state)?;
        Self::save(&self.path, &state)?;
        self.set_state(state);
        self.load_error = None;
        Ok(self.snapshot())
    }

    pub fn apply_pins<F>(
        &mut self,
        update: AgentPinsUpdate,
        canonicalize_target: F,
    ) -> Result<AgentsViewPreferencesSnapshot, String>
    where
        F: FnMut(AgentAnnotationTarget) -> Result<Option<AgentAnnotationTarget>, String>,
    {
        let mut state = self.read_current_state_or_default();
        apply_pins_update(&mut state, update)?;
        canonicalize_annotation_targets(&mut state, canonicalize_target)?;
        let state = validate_state(state)?;
        Self::save(&self.path, &state)?;
        self.set_state(state);
        self.load_error = None;
        Ok(self.snapshot())
    }

    pub fn promote_transient_agent(
        &mut self,
        host_id: HostFilterId,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> Result<bool, String> {
        let from = AgentAnnotationTarget::TransientAgent {
            host_id: host_id.clone(),
            agent_id,
        };
        let to = AgentAnnotationTarget::Session {
            host_id,
            session_id,
        };
        self.replace_annotation_target(from, Some(to))
    }

    pub fn remove_transient_agent(
        &mut self,
        host_id: HostFilterId,
        agent_id: AgentId,
    ) -> Result<bool, String> {
        self.replace_annotation_target(
            AgentAnnotationTarget::TransientAgent { host_id, agent_id },
            None,
        )
    }

    pub fn remove_session(
        &mut self,
        host_id: HostFilterId,
        session_id: SessionId,
    ) -> Result<bool, String> {
        self.replace_annotation_target(
            AgentAnnotationTarget::Session {
                host_id,
                session_id,
            },
            None,
        )
    }

    fn replace_annotation_target(
        &mut self,
        from: AgentAnnotationTarget,
        to: Option<AgentAnnotationTarget>,
    ) -> Result<bool, String> {
        let mut state = self.read_current_state_or_default();
        let changed = replace_annotation_target_in_state(&mut state, &from, to.as_ref());
        if !changed {
            return Ok(false);
        }
        let state = validate_state(state)?;
        Self::save(&self.path, &state)?;
        self.set_state(state);
        self.load_error = None;
        Ok(true)
    }

    fn read_current_state_or_default(&mut self) -> StoreState {
        match Self::read_from_disk(&self.path) {
            Ok(state) => state,
            Err(load_error) => {
                self.load_error = Some(load_error);
                StoreState::default()
            }
        }
    }

    fn set_state(&mut self, state: StoreState) {
        self.preferences = state.preferences;
        self.user_smart_views = state.user_smart_views;
        self.active_view_id = state.active_view_id;
        self.manual_tags = state.manual_tags;
        self.manual_tag_assignments = state.manual_tag_assignments;
        self.pins = state.pins;
    }

    fn smart_views_snapshot(&self) -> AgentsSmartViewsSnapshot {
        AgentsSmartViewsSnapshot {
            built_in: built_in_smart_views(),
            user: self.user_smart_views.clone(),
            active_view_id: self.active_view_id.clone(),
        }
    }

    fn tags_snapshot(&self) -> AgentTagsSnapshot {
        AgentTagsSnapshot {
            manual: self.manual_tags.clone(),
            system: Vec::new(),
            manual_assignments: self.manual_tag_assignments.clone(),
            system_assignments: Vec::new(),
        }
    }

    fn read_from_disk(path: &Path) -> Result<StoreState, AgentsViewPreferencesStoreError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoreState::default());
            }
            Err(err) => {
                return Err(store_error(
                    AgentsViewPreferencesStoreErrorKind::Io,
                    format!(
                        "Failed to read agents view preferences store {}: {err}",
                        path.display()
                    ),
                ));
            }
        };

        let value = serde_json::from_str::<Value>(&contents).map_err(|err| {
            store_error(
                AgentsViewPreferencesStoreErrorKind::Corrupt,
                format!(
                    "Failed to parse agents view preferences store {}: {err}",
                    path.display()
                ),
            )
        })?;
        let version = value
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                store_error(
                    AgentsViewPreferencesStoreErrorKind::Corrupt,
                    format!(
                        "Invalid agents view preferences store {}: version must be an integer",
                        path.display()
                    ),
                )
            })?;

        match version {
            matched_version if matched_version == u64::from(LEGACY_STORE_VERSION) => {
                let store = serde_json::from_value::<LegacyStoreFile>(value).map_err(|err| {
                    store_error(
                        AgentsViewPreferencesStoreErrorKind::Corrupt,
                        format!(
                            "Failed to parse agents view preferences store {}: {err}",
                            path.display()
                        ),
                    )
                })?;
                validate_state(StoreState {
                    preferences: store.preferences,
                    user_smart_views: Vec::new(),
                    active_view_id: Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)),
                    manual_tags: Vec::new(),
                    manual_tag_assignments: Vec::new(),
                    pins: AgentPinsSnapshot::default(),
                })
                .map_err(|err| {
                    store_error(
                        AgentsViewPreferencesStoreErrorKind::Corrupt,
                        format!(
                            "Invalid agents view preferences store {}: {err}",
                            path.display()
                        ),
                    )
                })
            }
            matched_version
                if matched_version == u64::from(SMART_VIEWS_STORE_VERSION)
                    || matched_version == u64::from(STORE_VERSION) =>
            {
                let active_view_id_was_present = value
                    .get("smart_views")
                    .and_then(Value::as_object)
                    .is_some_and(|smart_views| smart_views.contains_key("active_view_id"));
                let store = serde_json::from_value::<StoreFile>(value).map_err(|err| {
                    store_error(
                        AgentsViewPreferencesStoreErrorKind::Corrupt,
                        format!(
                            "Failed to parse agents view preferences store {}: {err}",
                            path.display()
                        ),
                    )
                })?;
                let active_view_id = if active_view_id_was_present {
                    store.smart_views.active_view_id
                } else {
                    Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All))
                };
                validate_state(StoreState {
                    preferences: store.preferences,
                    user_smart_views: store.smart_views.user,
                    active_view_id,
                    manual_tags: store.tags.manual,
                    manual_tag_assignments: store.tags.manual_assignments,
                    pins: store.pins,
                })
                .map_err(|err| {
                    store_error(
                        AgentsViewPreferencesStoreErrorKind::Corrupt,
                        format!(
                            "Invalid agents view preferences store {}: {err}",
                            path.display()
                        ),
                    )
                })
            }
            version => Err(store_error(
                AgentsViewPreferencesStoreErrorKind::UnsupportedVersion,
                format!(
                    "Unsupported agents view preferences store version {version} in {}; expected {STORE_VERSION}",
                    path.display()
                ),
            )),
        }
    }

    fn save(path: &Path, state: &StoreState) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            version: STORE_VERSION,
            preferences: state.preferences.clone(),
            smart_views: PersistedSmartViews {
                user: state.user_smart_views.clone(),
                active_view_id: state.active_view_id.clone(),
            },
            tags: PersistedTags {
                manual: state.manual_tags.clone(),
                manual_assignments: state.manual_tag_assignments.clone(),
            },
            pins: state.pins.clone(),
        })
        .map_err(|err| format!("Failed to serialize agents view preferences store: {err}"))?;

        let parent = path.parent().ok_or_else(|| {
            format!(
                "Agents view preferences store path has no parent: {}",
                path.display()
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|err| {
            format!("Failed to create agents view preferences store directory: {err}")
        })?;

        let tmp_path = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path).map_err(|err| {
            format!("Failed to create temp agents view preferences store file: {err}")
        })?;
        file.write_all(json.as_bytes()).map_err(|err| {
            format!("Failed to write temp agents view preferences store file: {err}")
        })?;
        file.sync_all().map_err(|err| {
            format!("Failed to sync temp agents view preferences store file: {err}")
        })?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            format!(
                "Failed to atomically replace agents view preferences store {}: {err}",
                path.display()
            )
        })?;
        Ok(())
    }
}

fn store_error(
    kind: AgentsViewPreferencesStoreErrorKind,
    message: String,
) -> AgentsViewPreferencesStoreError {
    AgentsViewPreferencesStoreError { kind, message }
}

fn apply_update(state: &mut StoreState, update: AgentsViewPreferencesUpdate) -> Result<(), String> {
    match update {
        AgentsViewPreferencesUpdate::SetFilters { filters } => {
            let filters = canonicalize_filters(filters)?;
            if state.preferences.filters != filters {
                state.active_view_id = None;
            }
            state.preferences.filters = filters;
        }
        AgentsViewPreferencesUpdate::SetSortMode { sort_mode } => {
            if state.preferences.sort_mode != sort_mode {
                state.active_view_id = None;
            }
            state.preferences.sort_mode = sort_mode;
        }
        AgentsViewPreferencesUpdate::SetGroupMode { group_mode } => {
            if state.preferences.group_mode != group_mode {
                state.active_view_id = None;
            }
            state.preferences.group_mode = group_mode;
        }
        AgentsViewPreferencesUpdate::SetDensity { density } => {
            state.preferences.density = density;
        }
        AgentsViewPreferencesUpdate::SetHideFinished { hide_finished } => {
            if state.preferences.hide_finished != hide_finished {
                state.active_view_id = None;
            }
            state.preferences.hide_finished = hide_finished;
        }
        AgentsViewPreferencesUpdate::SetManualOrder { manual_order } => {
            state.preferences.manual_order = manual_order;
        }
        AgentsViewPreferencesUpdate::Reset => {
            let default_preferences = AgentsViewPreferences::default();
            if query_from_preferences(&state.preferences)
                != query_from_preferences(&default_preferences)
            {
                state.active_view_id = None;
            }
            state.preferences = default_preferences;
        }
    }
    Ok(())
}

fn apply_smart_views_update(
    state: &mut StoreState,
    update: AgentsSmartViewsUpdate,
) -> Result<(), String> {
    match update {
        AgentsSmartViewsUpdate::SaveCurrent { name } => {
            let name = normalize_smart_view_name(name)?;
            let id = next_user_smart_view_id(&name, &state.user_smart_views);
            state.user_smart_views.push(SmartView {
                id: SmartViewId::User(id),
                name,
                filters: state.preferences.filters.clone(),
                sort_mode: state.preferences.sort_mode,
                group_mode: state.preferences.group_mode,
                hide_finished: state.preferences.hide_finished,
            });
        }
        AgentsSmartViewsUpdate::Rename { id, name } => {
            let id = require_user_smart_view_id(id, "renamed")?;
            let name = normalize_smart_view_name(name)?;
            let view = find_user_smart_view_mut(&mut state.user_smart_views, &id)?;
            view.name = name;
        }
        AgentsSmartViewsUpdate::Update { id } => {
            let id = require_user_smart_view_id(id, "updated")?;
            let query = query_from_preferences(&state.preferences);
            let view = find_user_smart_view_mut(&mut state.user_smart_views, &id)?;
            apply_query_to_smart_view(view, query);
        }
        AgentsSmartViewsUpdate::Delete { id } => {
            let id = require_user_smart_view_id(id, "deleted")?;
            let position = user_smart_view_position(&state.user_smart_views, &id)
                .ok_or_else(|| unknown_user_smart_view_message(&id))?;
            state.user_smart_views.remove(position);
            let deleted_active_id = SmartViewId::User(id);
            if state.active_view_id.as_ref() == Some(&deleted_active_id) {
                let all_id = SmartViewId::BuiltIn(BuiltInSmartViewId::All);
                let query = smart_view_query(state, &all_id)?;
                state.active_view_id = Some(all_id);
                apply_query_to_preferences(&mut state.preferences, query);
            }
        }
        AgentsSmartViewsUpdate::Reorder { user_ids } => {
            let user_ids = user_ids
                .into_iter()
                .map(|id| require_user_smart_view_id(id, "reordered"))
                .collect::<Result<Vec<_>, _>>()?;
            reorder_user_smart_views(&mut state.user_smart_views, user_ids)?;
        }
        AgentsSmartViewsUpdate::SetActive { id } => {
            let query = smart_view_query(state, &id)?;
            state.active_view_id = Some(id);
            apply_query_to_preferences(&mut state.preferences, query);
        }
    }
    Ok(())
}

fn validate_state(state: StoreState) -> Result<StoreState, String> {
    let preferences = validate_preferences(state.preferences)?;
    let user_smart_views = validate_user_smart_views(state.user_smart_views)?;
    let manual_tags = validate_manual_tags(state.manual_tags)?;
    let manual_tag_ids = manual_tags
        .iter()
        .map(|tag| tag.id.clone())
        .collect::<HashSet<_>>();
    let manual_tag_assignments =
        validate_manual_tag_assignments(state.manual_tag_assignments, &manual_tag_ids)?;
    let pins = validate_pins(state.pins)?;
    let user_ids = user_smart_views
        .iter()
        .filter_map(user_smart_view_id)
        .cloned()
        .collect::<HashSet<_>>();
    validate_active_view_id(state.active_view_id.as_ref(), &user_ids)?;
    Ok(StoreState {
        preferences,
        user_smart_views,
        active_view_id: state.active_view_id,
        manual_tags,
        manual_tag_assignments,
        pins,
    })
}

fn validate_preferences(
    preferences: AgentsViewPreferences,
) -> Result<AgentsViewPreferences, String> {
    let filters = canonicalize_filters(preferences.filters)?;
    validate_manual_order(&preferences.manual_order)?;
    Ok(AgentsViewPreferences {
        filters,
        sort_mode: preferences.sort_mode,
        group_mode: preferences.group_mode,
        density: preferences.density,
        hide_finished: preferences.hide_finished,
        manual_order: preferences.manual_order,
    })
}

fn validate_manual_tags(
    tags: Vec<AgentManualTagDescriptor>,
) -> Result<Vec<AgentManualTagDescriptor>, String> {
    let mut seen = HashSet::new();
    let mut validated = Vec::with_capacity(tags.len());
    for tag in tags {
        validate_manual_tag_id(&tag.id)?;
        if !seen.insert(tag.id.clone()) {
            return Err(format!("duplicate manual tag id {}", tag.id));
        }
        validated.push(AgentManualTagDescriptor {
            id: tag.id,
            name: normalize_tag_name(tag.name)?,
            color: normalize_optional_tag_color(tag.color)?,
        });
    }
    Ok(validated)
}

fn validate_manual_tag_assignments(
    assignments: Vec<AgentManualTagAssignment>,
    known_tag_ids: &HashSet<AgentManualTagId>,
) -> Result<Vec<AgentManualTagAssignment>, String> {
    let mut by_target = HashMap::<AgentAnnotationTarget, Vec<AgentManualTagId>>::new();
    for assignment in assignments {
        validate_annotation_target(&assignment.target)?;
        let entry = by_target.entry(assignment.target).or_default();
        for tag_id in assignment.tag_ids {
            validate_manual_tag_id(&tag_id)?;
            if !known_tag_ids.contains(&tag_id) {
                return Err(format!(
                    "manual tag assignment references unknown tag {tag_id}"
                ));
            }
            entry.push(tag_id);
        }
    }

    let mut validated = Vec::with_capacity(by_target.len());
    for (target, tag_ids) in by_target {
        let tag_ids = canonicalize_manual_tag_ids(tag_ids)?;
        if !tag_ids.is_empty() {
            validated.push(AgentManualTagAssignment { target, tag_ids });
        }
    }
    validated.sort_by(|left, right| compare_annotation_targets(&left.target, &right.target));
    Ok(validated)
}

fn validate_pins(pins: AgentPinsSnapshot) -> Result<AgentPinsSnapshot, String> {
    let mut pinned = pins.pinned;
    for target in &pinned {
        validate_annotation_target(target)?;
    }
    pinned.sort_by(compare_annotation_targets);
    pinned.dedup();
    Ok(AgentPinsSnapshot { pinned })
}

fn validate_user_smart_views(views: Vec<SmartView>) -> Result<Vec<SmartView>, String> {
    let mut seen = HashSet::new();
    let mut validated = Vec::with_capacity(views.len());
    for view in views {
        let SmartViewId::User(user_id) = view.id else {
            return Err("persisted user smart views must use user ids".to_owned());
        };
        validate_user_smart_view_id(&user_id)?;
        if !seen.insert(user_id.clone()) {
            return Err(format!("duplicate smart view id {}", user_id.0));
        }
        validated.push(SmartView {
            id: SmartViewId::User(user_id),
            name: normalize_smart_view_name(view.name)?,
            filters: canonicalize_filters(view.filters)?,
            sort_mode: view.sort_mode,
            group_mode: view.group_mode,
            hide_finished: view.hide_finished,
        });
    }
    Ok(validated)
}

fn validate_active_view_id(
    active_view_id: Option<&SmartViewId>,
    user_ids: &HashSet<UserSmartViewId>,
) -> Result<(), String> {
    match active_view_id {
        Some(SmartViewId::BuiltIn(_)) | None => Ok(()),
        Some(SmartViewId::User(user_id)) => {
            validate_user_smart_view_id(user_id)?;
            if user_ids.contains(user_id) {
                Ok(())
            } else {
                Err(format!(
                    "active smart view id references unknown user view {}",
                    user_id.0
                ))
            }
        }
    }
}

fn canonicalize_filters(filters: AgentsViewFilters) -> Result<AgentsViewFilters, String> {
    let mut host_ids = filters.host_ids;
    for host_id in &host_ids {
        ensure_non_empty("filters.host_ids", host_id.0.as_str())?;
    }
    host_ids.sort_by(|left, right| left.0.cmp(&right.0));
    host_ids.dedup();

    let mut project_ids = filters.project_ids;
    for project_filter in &project_ids {
        ensure_non_empty(
            "filters.project_ids.host_id",
            project_filter.host_id.0.as_str(),
        )?;
        ensure_non_empty(
            "filters.project_ids.project_id",
            project_filter.project_id.0.as_str(),
        )?;
    }
    project_ids.sort_by(|left, right| {
        left.host_id
            .0
            .cmp(&right.host_id.0)
            .then_with(|| left.project_id.0.cmp(&right.project_id.0))
    });
    project_ids.dedup();

    let statuses = canonicalize_status_filters(filters.statuses);
    let backends = canonicalize_backends(filters.backends);
    let origins = canonicalize_origins(filters.origins);
    let tags = canonicalize_tag_filters(filters.tags)?;

    Ok(AgentsViewFilters {
        host_ids,
        project_ids,
        statuses,
        backends,
        origins,
        tags,
    })
}

fn canonicalize_status_filters(mut statuses: Vec<AgentStatusFilter>) -> Vec<AgentStatusFilter> {
    statuses.sort_by_key(|status| match *status {
        AgentStatusFilter::Initializing => 0,
        AgentStatusFilter::Thinking => 1,
        AgentStatusFilter::Compacting => 2,
        AgentStatusFilter::Idle => 3,
        AgentStatusFilter::Terminated => 4,
    });
    statuses.dedup();
    statuses
}

fn canonicalize_backends(mut backends: Vec<BackendKind>) -> Vec<BackendKind> {
    backends.sort_by_key(|backend| match *backend {
        BackendKind::Tycode => 0,
        BackendKind::Kiro => 1,
        BackendKind::Claude => 2,
        BackendKind::Codex => 3,
        BackendKind::Antigravity => 4,
    });
    backends.dedup();
    backends
}

fn canonicalize_origins(mut origins: Vec<AgentOrigin>) -> Vec<AgentOrigin> {
    origins.sort_by_key(|origin| match *origin {
        AgentOrigin::User => 0,
        AgentOrigin::AgentControl => 1,
        AgentOrigin::SideQuestion => 2,
        AgentOrigin::BackendNative => 3,
        AgentOrigin::TeamMember => 4,
        AgentOrigin::Workflow => 5,
    });
    origins.dedup();
    origins
}

fn canonicalize_tag_filters(mut tags: Vec<AgentTagRef>) -> Result<Vec<AgentTagRef>, String> {
    for tag in &tags {
        match tag {
            AgentTagRef::Manual(tag_id) => {
                ensure_non_empty("filters.tags.manual", tag_id.0.as_str())?;
            }
            AgentTagRef::System(tag_id) => {
                ensure_non_empty("filters.tags.system", tag_id.0.as_str())?;
            }
        }
    }
    tags.sort_by(compare_tag_refs);
    tags.dedup();
    Ok(tags)
}

fn compare_tag_refs(left: &AgentTagRef, right: &AgentTagRef) -> std::cmp::Ordering {
    tag_ref_key(left).cmp(&tag_ref_key(right))
}

fn tag_ref_key(tag: &AgentTagRef) -> (u8, &str) {
    match tag {
        AgentTagRef::Manual(tag_id) => (0, tag_id.0.as_str()),
        AgentTagRef::System(tag_id) => (1, tag_id.0.as_str()),
    }
}

fn validate_manual_order(manual_order: &[AgentOrderKey]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for key in manual_order {
        match key {
            AgentOrderKey::Session { session_id } => {
                ensure_non_empty("manual_order.session_id", session_id.0.as_str())?;
            }
            AgentOrderKey::TransientAgent { host_id, agent_id } => {
                ensure_non_empty("manual_order.host_id", host_id.0.as_str())?;
                ensure_non_empty("manual_order.agent_id", agent_id.0.as_str())?;
            }
        }
        if !seen.insert(key) {
            return Err(format!("manual_order contains duplicate key {key:?}"));
        }
    }
    Ok(())
}

fn apply_tags_update(state: &mut StoreState, update: AgentTagsUpdate) -> Result<(), String> {
    match update {
        AgentTagsUpdate::CreateTag { name, color } => {
            let name = normalize_tag_name(name)?;
            let color = normalize_optional_tag_color(color)?;
            let id = next_manual_tag_id(&name, &state.manual_tags);
            state
                .manual_tags
                .push(AgentManualTagDescriptor { id, name, color });
        }
        AgentTagsUpdate::RenameTag { tag_id, name } => {
            validate_manual_tag_id(&tag_id)?;
            let name = normalize_tag_name(name)?;
            let tag = find_manual_tag_mut(&mut state.manual_tags, &tag_id)?;
            tag.name = name;
        }
        AgentTagsUpdate::SetTagColor { tag_id, color } => {
            validate_manual_tag_id(&tag_id)?;
            let color = normalize_optional_tag_color(color)?;
            let tag = find_manual_tag_mut(&mut state.manual_tags, &tag_id)?;
            tag.color = color;
        }
        AgentTagsUpdate::DeleteTag { tag_id } => {
            validate_manual_tag_id(&tag_id)?;
            let position = manual_tag_position(&state.manual_tags, &tag_id)
                .ok_or_else(|| unknown_manual_tag_message(&tag_id))?;
            state.manual_tags.remove(position);
            let deleted_ref = AgentTagRef::Manual(tag_id.clone());
            state
                .preferences
                .filters
                .tags
                .retain(|tag| tag != &deleted_ref);
            for view in &mut state.user_smart_views {
                view.filters.tags.retain(|tag| tag != &deleted_ref);
            }
            for assignment in &mut state.manual_tag_assignments {
                assignment.tag_ids.retain(|assigned| assigned != &tag_id);
            }
            state
                .manual_tag_assignments
                .retain(|assignment| !assignment.tag_ids.is_empty());
        }
        AgentTagsUpdate::AssignTag { target, tag_id } => {
            validate_manual_tag_id(&tag_id)?;
            ensure_manual_tag_exists(&state.manual_tags, &tag_id)?;
            validate_annotation_target(&target)?;
            let assignment =
                find_or_insert_manual_assignment(&mut state.manual_tag_assignments, target);
            if !assignment.tag_ids.contains(&tag_id) {
                assignment.tag_ids.push(tag_id);
            }
        }
        AgentTagsUpdate::RemoveTag { target, tag_id } => {
            validate_manual_tag_id(&tag_id)?;
            ensure_manual_tag_exists(&state.manual_tags, &tag_id)?;
            validate_annotation_target(&target)?;
            if let Some(assignment) = state
                .manual_tag_assignments
                .iter_mut()
                .find(|assignment| assignment.target == target)
            {
                assignment.tag_ids.retain(|assigned| assigned != &tag_id);
            }
            state
                .manual_tag_assignments
                .retain(|assignment| !assignment.tag_ids.is_empty());
        }
    }
    Ok(())
}

fn apply_pins_update(state: &mut StoreState, update: AgentPinsUpdate) -> Result<(), String> {
    match update {
        AgentPinsUpdate::Pin { target } => {
            validate_annotation_target(&target)?;
            if !state.pins.pinned.contains(&target) {
                state.pins.pinned.push(target);
            }
        }
        AgentPinsUpdate::Unpin { target } => {
            validate_annotation_target(&target)?;
            state.pins.pinned.retain(|pinned| pinned != &target);
        }
    }
    Ok(())
}

fn canonicalize_annotation_targets<F>(
    state: &mut StoreState,
    mut canonicalize_target: F,
) -> Result<(), String>
where
    F: FnMut(AgentAnnotationTarget) -> Result<Option<AgentAnnotationTarget>, String>,
{
    let mut assignments = Vec::new();
    for assignment in std::mem::take(&mut state.manual_tag_assignments) {
        if let Some(target) = canonicalize_target(assignment.target)? {
            assignments.push(AgentManualTagAssignment {
                target,
                tag_ids: assignment.tag_ids,
            });
        }
    }
    state.manual_tag_assignments = assignments;

    let mut pinned = Vec::new();
    for target in std::mem::take(&mut state.pins.pinned) {
        if let Some(target) = canonicalize_target(target)? {
            pinned.push(target);
        }
    }
    state.pins.pinned = pinned;
    Ok(())
}

fn replace_annotation_target_in_state(
    state: &mut StoreState,
    from: &AgentAnnotationTarget,
    to: Option<&AgentAnnotationTarget>,
) -> bool {
    let mut changed = false;
    for assignment in &mut state.manual_tag_assignments {
        if &assignment.target == from {
            match to {
                Some(target) => assignment.target = target.clone(),
                None => assignment.tag_ids.clear(),
            }
            changed = true;
        }
    }
    state
        .manual_tag_assignments
        .retain(|assignment| !assignment.tag_ids.is_empty());

    for target in &mut state.pins.pinned {
        if target == from {
            if let Some(replacement) = to {
                *target = replacement.clone();
            }
            changed = true;
        }
    }
    if to.is_none() {
        let before = state.pins.pinned.len();
        state.pins.pinned.retain(|target| target != from);
        changed |= state.pins.pinned.len() != before;
    }
    changed
}

fn validate_annotation_target(target: &AgentAnnotationTarget) -> Result<(), String> {
    match target {
        AgentAnnotationTarget::Session {
            host_id,
            session_id,
        } => {
            ensure_non_empty("agent annotation target host_id", host_id.0.as_str())?;
            ensure_non_empty("agent annotation target session_id", session_id.0.as_str())
        }
        AgentAnnotationTarget::TransientAgent { host_id, agent_id } => {
            ensure_non_empty("agent annotation target host_id", host_id.0.as_str())?;
            ensure_non_empty("agent annotation target agent_id", agent_id.0.as_str())
        }
    }
}

fn compare_annotation_targets(
    left: &AgentAnnotationTarget,
    right: &AgentAnnotationTarget,
) -> std::cmp::Ordering {
    annotation_target_key(left).cmp(&annotation_target_key(right))
}

fn annotation_target_key(target: &AgentAnnotationTarget) -> (u8, &str, &str) {
    match target {
        AgentAnnotationTarget::Session {
            host_id,
            session_id,
        } => (0, host_id.0.as_str(), session_id.0.as_str()),
        AgentAnnotationTarget::TransientAgent { host_id, agent_id } => {
            (1, host_id.0.as_str(), agent_id.0.as_str())
        }
    }
}

fn find_or_insert_manual_assignment(
    assignments: &mut Vec<AgentManualTagAssignment>,
    target: AgentAnnotationTarget,
) -> &mut AgentManualTagAssignment {
    if let Some(position) = assignments
        .iter()
        .position(|assignment| assignment.target == target)
    {
        return &mut assignments[position];
    }
    assignments.push(AgentManualTagAssignment {
        target,
        tag_ids: Vec::new(),
    });
    let last_index = assignments.len() - 1;
    &mut assignments[last_index]
}

fn built_in_smart_views() -> Vec<SmartView> {
    vec![
        built_in_smart_view(BuiltInSmartViewId::All),
        built_in_smart_view(BuiltInSmartViewId::Active),
        built_in_smart_view(BuiltInSmartViewId::FailedTerminated),
    ]
}

fn built_in_smart_view(id: BuiltInSmartViewId) -> SmartView {
    match id {
        BuiltInSmartViewId::All => SmartView {
            id: SmartViewId::BuiltIn(BuiltInSmartViewId::All),
            name: "All".to_owned(),
            filters: AgentsViewFilters::default(),
            sort_mode: AgentSortMode::default(),
            group_mode: AgentGroupMode::default(),
            hide_finished: false,
        },
        BuiltInSmartViewId::Active => SmartView {
            id: SmartViewId::BuiltIn(BuiltInSmartViewId::Active),
            name: "Active".to_owned(),
            filters: AgentsViewFilters {
                host_ids: Vec::new(),
                project_ids: Vec::new(),
                statuses: vec![
                    AgentStatusFilter::Initializing,
                    AgentStatusFilter::Thinking,
                    AgentStatusFilter::Compacting,
                ],
                backends: Vec::new(),
                origins: Vec::new(),
                tags: Vec::new(),
            },
            sort_mode: AgentSortMode::default(),
            group_mode: AgentGroupMode::default(),
            hide_finished: true,
        },
        BuiltInSmartViewId::FailedTerminated => SmartView {
            id: SmartViewId::BuiltIn(BuiltInSmartViewId::FailedTerminated),
            name: "Failed / terminated".to_owned(),
            filters: AgentsViewFilters {
                host_ids: Vec::new(),
                project_ids: Vec::new(),
                // The UI's DerivedAgentState collapses fatal terminal backend
                // failures into AgentStatusFilter::Terminated; there is no
                // separate fatal status on the protocol filter enum.
                statuses: vec![AgentStatusFilter::Terminated],
                backends: Vec::new(),
                origins: Vec::new(),
                tags: Vec::new(),
            },
            sort_mode: AgentSortMode::default(),
            group_mode: AgentGroupMode::default(),
            hide_finished: false,
        },
    }
}

fn smart_view_query(state: &StoreState, id: &SmartViewId) -> Result<SmartViewQuery, String> {
    match id {
        SmartViewId::BuiltIn(id) => Ok(query_from_smart_view(&built_in_smart_view(*id))),
        SmartViewId::User(user_id) => state
            .user_smart_views
            .iter()
            .find(|view| user_smart_view_id(view) == Some(user_id))
            .map(query_from_smart_view)
            .ok_or_else(|| unknown_user_smart_view_message(user_id)),
    }
}

fn query_from_preferences(preferences: &AgentsViewPreferences) -> SmartViewQuery {
    SmartViewQuery {
        filters: preferences.filters.clone(),
        sort_mode: preferences.sort_mode,
        group_mode: preferences.group_mode,
        hide_finished: preferences.hide_finished,
    }
}

fn query_from_smart_view(view: &SmartView) -> SmartViewQuery {
    SmartViewQuery {
        filters: view.filters.clone(),
        sort_mode: view.sort_mode,
        group_mode: view.group_mode,
        hide_finished: view.hide_finished,
    }
}

fn apply_query_to_preferences(preferences: &mut AgentsViewPreferences, query: SmartViewQuery) {
    preferences.filters = query.filters;
    preferences.sort_mode = query.sort_mode;
    preferences.group_mode = query.group_mode;
    preferences.hide_finished = query.hide_finished;
}

fn apply_query_to_smart_view(view: &mut SmartView, query: SmartViewQuery) {
    view.filters = query.filters;
    view.sort_mode = query.sort_mode;
    view.group_mode = query.group_mode;
    view.hide_finished = query.hide_finished;
}

fn normalize_smart_view_name(name: String) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("smart view name must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn next_user_smart_view_id(name: &str, views: &[SmartView]) -> UserSmartViewId {
    let base = sanitize_user_smart_view_id(name);
    let existing = views
        .iter()
        .filter_map(user_smart_view_id)
        .cloned()
        .collect::<HashSet<_>>();
    if !existing.contains(&UserSmartViewId(base.clone())) {
        return UserSmartViewId(base);
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = UserSmartViewId(format!("{base}-{suffix}"));
        if !existing.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn sanitize_user_smart_view_id(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !output.is_empty() && !last_was_dash {
            output.push('-');
            last_was_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "view".to_owned()
    } else {
        output
    }
}

fn validate_user_smart_view_id(id: &UserSmartViewId) -> Result<(), String> {
    ensure_non_empty("smart view id", id.0.as_str())?;
    let sanitized = sanitize_user_smart_view_id(&id.0);
    if id.0 != sanitized {
        return Err(format!(
            "smart view id {} must be sanitized as {}",
            id.0, sanitized
        ));
    }
    Ok(())
}

fn require_user_smart_view_id(id: SmartViewId, operation: &str) -> Result<UserSmartViewId, String> {
    match id {
        SmartViewId::User(id) => Ok(id),
        SmartViewId::BuiltIn(_) => Err(format!("built-in smart views cannot be {operation}")),
    }
}

fn find_user_smart_view_mut<'a>(
    views: &'a mut [SmartView],
    id: &UserSmartViewId,
) -> Result<&'a mut SmartView, String> {
    views
        .iter_mut()
        .find(|view| user_smart_view_id(view) == Some(id))
        .ok_or_else(|| unknown_user_smart_view_message(id))
}

fn user_smart_view_position(views: &[SmartView], id: &UserSmartViewId) -> Option<usize> {
    views
        .iter()
        .position(|view| user_smart_view_id(view) == Some(id))
}

fn user_smart_view_id(view: &SmartView) -> Option<&UserSmartViewId> {
    match &view.id {
        SmartViewId::User(id) => Some(id),
        SmartViewId::BuiltIn(_) => None,
    }
}

fn unknown_user_smart_view_message(id: &UserSmartViewId) -> String {
    format!("unknown smart view id {}", id.0)
}

fn reorder_user_smart_views(
    views: &mut Vec<SmartView>,
    user_ids: Vec<UserSmartViewId>,
) -> Result<(), String> {
    if user_ids.len() != views.len() {
        return Err(
            "smart view reorder must contain every user smart view id exactly once".to_owned(),
        );
    }

    let mut seen = HashSet::new();
    for id in &user_ids {
        validate_user_smart_view_id(id)?;
        if !seen.insert(id.clone()) {
            return Err(format!("smart view reorder contains duplicate id {}", id.0));
        }
        if user_smart_view_position(views, id).is_none() {
            return Err(unknown_user_smart_view_message(id));
        }
    }

    let mut reordered = Vec::with_capacity(views.len());
    for id in user_ids {
        let position = user_smart_view_position(views, &id)
            .ok_or_else(|| unknown_user_smart_view_message(&id))?;
        reordered.push(views[position].clone());
    }
    *views = reordered;
    Ok(())
}

fn normalize_tag_name(name: String) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("manual tag name must not be empty".to_owned());
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional_tag_color(
    color: Option<AgentTagColor>,
) -> Result<Option<AgentTagColor>, String> {
    color
        .map(|color| normalize_tag_color(color).map(AgentTagColor))
        .transpose()
}

fn normalize_tag_color(color: AgentTagColor) -> Result<String, String> {
    let value = color.0.trim();
    let valid_len = matches!(value.len(), 4 | 5 | 7 | 9);
    if !valid_len
        || !value.starts_with('#')
        || !value
            .as_bytes()
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("manual tag color must be a hex RGB or RGBA string".to_owned());
    }
    Ok(value.to_owned())
}

fn next_manual_tag_id(name: &str, tags: &[AgentManualTagDescriptor]) -> AgentManualTagId {
    let base = sanitize_manual_tag_id(name);
    let existing = tags
        .iter()
        .map(|tag| tag.id.clone())
        .collect::<HashSet<_>>();
    if !existing.contains(&AgentManualTagId(base.clone())) {
        return AgentManualTagId(base);
    }
    let mut suffix = 2_u64;
    loop {
        let candidate = AgentManualTagId(format!("{base}-{suffix}"));
        if !existing.contains(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn sanitize_manual_tag_id(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !output.is_empty() && !last_was_dash {
            output.push('-');
            last_was_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "tag".to_owned()
    } else {
        output
    }
}

fn validate_manual_tag_id(id: &AgentManualTagId) -> Result<(), String> {
    ensure_non_empty("manual tag id", id.0.as_str())?;
    let sanitized = sanitize_manual_tag_id(&id.0);
    if id.0 != sanitized {
        return Err(format!(
            "manual tag id {} must be sanitized as {}",
            id.0, sanitized
        ));
    }
    Ok(())
}

fn canonicalize_manual_tag_ids(
    mut tag_ids: Vec<AgentManualTagId>,
) -> Result<Vec<AgentManualTagId>, String> {
    for tag_id in &tag_ids {
        validate_manual_tag_id(tag_id)?;
    }
    tag_ids.sort_by(|left, right| left.0.cmp(&right.0));
    tag_ids.dedup();
    Ok(tag_ids)
}

fn manual_tag_position(tags: &[AgentManualTagDescriptor], id: &AgentManualTagId) -> Option<usize> {
    tags.iter().position(|tag| &tag.id == id)
}

fn find_manual_tag_mut<'a>(
    tags: &'a mut [AgentManualTagDescriptor],
    id: &AgentManualTagId,
) -> Result<&'a mut AgentManualTagDescriptor, String> {
    tags.iter_mut()
        .find(|tag| &tag.id == id)
        .ok_or_else(|| unknown_manual_tag_message(id))
}

fn ensure_manual_tag_exists(
    tags: &[AgentManualTagDescriptor],
    id: &AgentManualTagId,
) -> Result<(), String> {
    if manual_tag_position(tags, id).is_some() {
        Ok(())
    } else {
        Err(unknown_manual_tag_message(id))
    }
}

fn unknown_manual_tag_message(id: &AgentManualTagId) -> String {
    format!("unknown manual tag id {}", id.0)
}

fn ensure_non_empty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::{
        AgentId, AgentListDensity, AgentOrderKey, AgentsViewPreferencesUpdate, HostFilterId,
        SessionId,
    };

    use super::*;

    #[test]
    fn corrupt_load_uses_defaults_and_reports_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents_view_preferences.json");
        std::fs::write(&path, "not json").expect("write corrupt store");

        let store = AgentsViewPreferencesStore::load(path);
        let snapshot = store.snapshot();

        assert_eq!(snapshot.preferences, AgentsViewPreferences::default());
        assert_eq!(
            snapshot.load_error.as_ref().map(|error| error.kind),
            Some(AgentsViewPreferencesStoreErrorKind::Corrupt)
        );
        assert_eq!(snapshot.smart_views.built_in, built_in_smart_views());
        assert!(snapshot.smart_views.user.is_empty());
        assert_eq!(
            snapshot.smart_views.active_view_id,
            Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All))
        );
        assert!(snapshot.tags.manual.is_empty());
        assert!(snapshot.tags.manual_assignments.is_empty());
        assert!(snapshot.pins.pinned.is_empty());
    }

    #[test]
    fn valid_mutation_overwrites_corrupt_file_and_clears_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents_view_preferences.json");
        std::fs::write(&path, "not json").expect("write corrupt store");
        let mut store = AgentsViewPreferencesStore::load(path.clone());

        let snapshot = store
            .apply(AgentsViewPreferencesUpdate::SetManualOrder {
                manual_order: vec![AgentOrderKey::Session {
                    session_id: SessionId("session-1".to_owned()),
                }],
            })
            .expect("apply valid update");

        assert!(snapshot.load_error.is_none());
        assert_eq!(
            snapshot.preferences.manual_order,
            vec![AgentOrderKey::Session {
                session_id: SessionId("session-1".to_owned()),
            }]
        );
        assert!(
            std::fs::read_to_string(path)
                .expect("read rewritten store")
                .contains("\"version\": 3")
        );
    }

    #[test]
    fn duplicate_manual_order_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents_view_preferences.json");
        let mut store = AgentsViewPreferencesStore::load(path);
        let key = AgentOrderKey::TransientAgent {
            host_id: HostFilterId("local".to_owned()),
            agent_id: AgentId("agent-1".to_owned()),
        };

        let err = store
            .apply(AgentsViewPreferencesUpdate::SetManualOrder {
                manual_order: vec![key.clone(), key],
            })
            .expect_err("duplicate manual order should fail");

        assert!(
            err.contains("duplicate"),
            "duplicate rejection should be explicit: {err}"
        );
    }

    #[test]
    fn legacy_store_migrates_to_empty_smart_views() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents_view_preferences.json");
        let legacy = serde_json::json!({
            "version": 1,
            "preferences": {
                "filters": {
                    "host_ids": ["local"],
                    "project_ids": [],
                    "statuses": ["idle"],
                    "backends": ["codex"],
                    "origins": ["user"]
                },
                "sort_mode": "name_asc",
                "group_mode": "status",
                "density": "compact",
                "hide_finished": true,
                "manual_order": []
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).expect("json"))
            .expect("write legacy store");

        let store = AgentsViewPreferencesStore::load(path);
        let snapshot = store.snapshot();

        assert_eq!(snapshot.preferences.sort_mode, AgentSortMode::NameAsc);
        assert_eq!(snapshot.preferences.group_mode, AgentGroupMode::Status);
        assert_eq!(snapshot.preferences.density, AgentListDensity::Compact);
        assert!(snapshot.preferences.hide_finished);
        assert_eq!(snapshot.smart_views.built_in, built_in_smart_views());
        assert!(snapshot.smart_views.user.is_empty());
        assert_eq!(
            snapshot.smart_views.active_view_id,
            Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All))
        );
        assert!(snapshot.tags.manual.is_empty());
        assert!(snapshot.tags.manual_assignments.is_empty());
        assert!(snapshot.pins.pinned.is_empty());
    }
}
