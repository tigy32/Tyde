use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{
    AgentOrderKey, AgentOrigin, AgentStatusFilter, AgentsViewFilters, AgentsViewPreferences,
    AgentsViewPreferencesSnapshot, AgentsViewPreferencesStoreError,
    AgentsViewPreferencesStoreErrorKind, AgentsViewPreferencesUpdate, BackendKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    preferences: AgentsViewPreferences,
}

#[derive(Debug)]
pub struct AgentsViewPreferencesStore {
    path: PathBuf,
    preferences: AgentsViewPreferences,
    load_error: Option<AgentsViewPreferencesStoreError>,
}

impl AgentsViewPreferencesStore {
    pub fn load(path: PathBuf) -> Self {
        match Self::read_from_disk(&path) {
            Ok(preferences) => Self {
                path,
                preferences,
                load_error: None,
            },
            Err(load_error) => Self {
                path,
                preferences: AgentsViewPreferences::default(),
                load_error: Some(load_error),
            },
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
        }
    }

    pub fn apply(
        &mut self,
        update: AgentsViewPreferencesUpdate,
    ) -> Result<AgentsViewPreferencesSnapshot, String> {
        let mut preferences = match Self::read_from_disk(&self.path) {
            Ok(preferences) => preferences,
            Err(load_error) => {
                self.load_error = Some(load_error);
                AgentsViewPreferences::default()
            }
        };
        apply_update(&mut preferences, update)?;
        let preferences = validate_preferences(preferences)?;
        Self::save(&self.path, &preferences)?;
        self.preferences = preferences;
        self.load_error = None;
        Ok(self.snapshot())
    }

    fn read_from_disk(
        path: &Path,
    ) -> Result<AgentsViewPreferences, AgentsViewPreferencesStoreError> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AgentsViewPreferences::default());
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
        if version != u64::from(STORE_VERSION) {
            return Err(store_error(
                AgentsViewPreferencesStoreErrorKind::UnsupportedVersion,
                format!(
                    "Unsupported agents view preferences store version {version} in {}; expected {STORE_VERSION}",
                    path.display()
                ),
            ));
        }

        let store = serde_json::from_value::<StoreFile>(value).map_err(|err| {
            store_error(
                AgentsViewPreferencesStoreErrorKind::Corrupt,
                format!(
                    "Failed to parse agents view preferences store {}: {err}",
                    path.display()
                ),
            )
        })?;
        validate_preferences(store.preferences).map_err(|err| {
            store_error(
                AgentsViewPreferencesStoreErrorKind::Corrupt,
                format!(
                    "Invalid agents view preferences store {}: {err}",
                    path.display()
                ),
            )
        })
    }

    fn save(path: &Path, preferences: &AgentsViewPreferences) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&StoreFile {
            version: STORE_VERSION,
            preferences: preferences.clone(),
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

fn apply_update(
    preferences: &mut AgentsViewPreferences,
    update: AgentsViewPreferencesUpdate,
) -> Result<(), String> {
    match update {
        AgentsViewPreferencesUpdate::SetFilters { filters } => {
            preferences.filters = canonicalize_filters(filters)?;
        }
        AgentsViewPreferencesUpdate::SetSortMode { sort_mode } => {
            preferences.sort_mode = sort_mode;
        }
        AgentsViewPreferencesUpdate::SetGroupMode { group_mode } => {
            preferences.group_mode = group_mode;
        }
        AgentsViewPreferencesUpdate::SetDensity { density } => {
            preferences.density = density;
        }
        AgentsViewPreferencesUpdate::SetHideFinished { hide_finished } => {
            preferences.hide_finished = hide_finished;
        }
        AgentsViewPreferencesUpdate::SetManualOrder { manual_order } => {
            preferences.manual_order = manual_order;
        }
        AgentsViewPreferencesUpdate::Reset => {
            *preferences = AgentsViewPreferences::default();
        }
    }
    Ok(())
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

fn ensure_non_empty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::{AgentId, AgentOrderKey, AgentsViewPreferencesUpdate, HostFilterId, SessionId};

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
                .contains("\"version\": 1")
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
}
