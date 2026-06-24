use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{
    AgentGroupMode, AgentOrderKey, AgentOrigin, AgentSortMode, AgentStatusFilter,
    AgentsSmartViewsSnapshot, AgentsSmartViewsUpdate, AgentsViewFilters, AgentsViewPreferences,
    AgentsViewPreferencesSnapshot, AgentsViewPreferencesStoreError,
    AgentsViewPreferencesStoreErrorKind, AgentsViewPreferencesUpdate, BackendKind,
    BuiltInSmartViewId, SmartView, SmartViewId, UserSmartViewId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u32 = 2;
const LEGACY_STORE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    preferences: AgentsViewPreferences,
    #[serde(default)]
    smart_views: PersistedSmartViews,
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

#[derive(Debug, Clone)]
struct StoreState {
    preferences: AgentsViewPreferences,
    user_smart_views: Vec<SmartView>,
    active_view_id: Option<SmartViewId>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            preferences: AgentsViewPreferences::default(),
            user_smart_views: Vec::new(),
            active_view_id: Some(SmartViewId::BuiltIn(BuiltInSmartViewId::All)),
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
                load_error: None,
            },
            Err(load_error) => {
                let state = StoreState::default();
                Self {
                    path,
                    preferences: state.preferences,
                    user_smart_views: state.user_smart_views,
                    active_view_id: state.active_view_id,
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
    }

    fn smart_views_snapshot(&self) -> AgentsSmartViewsSnapshot {
        AgentsSmartViewsSnapshot {
            built_in: built_in_smart_views(),
            user: self.user_smart_views.clone(),
            active_view_id: self.active_view_id.clone(),
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
            matched_version if matched_version == u64::from(STORE_VERSION) => {
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

    Ok(AgentsViewFilters {
        host_ids,
        project_ids,
        statuses,
        backends,
        origins,
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
                .contains("\"version\": 2")
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
    }
}
