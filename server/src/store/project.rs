use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use protocol::{
    GitBranchName, Project, ProjectId, ProjectReorderScope, ProjectRootPath, ProjectSource,
    WorkbenchRoot,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const STORE_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    records: HashMap<String, Project>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFileV1 {
    records: HashMap<String, ProjectRecordV1>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectRecordV1 {
    id: ProjectId,
    name: String,
    roots: Vec<String>,
    #[serde(default)]
    sort_order: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectStoreError {
    NotFound(String),
    InvalidInput(String),
    Conflict(String),
    InvalidStore(String),
    Internal(String),
}

impl ProjectStoreError {
    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    fn invalid_store(message: impl Into<String>) -> Self {
        Self::InvalidStore(message.into())
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn with_message(self, message: impl Into<String>) -> Self {
        let message = message.into();
        match self {
            Self::NotFound(_) => Self::NotFound(message),
            Self::InvalidInput(_) => Self::InvalidInput(message),
            Self::Conflict(_) => Self::Conflict(message),
            Self::InvalidStore(_) => Self::InvalidStore(message),
            Self::Internal(_) => Self::Internal(message),
        }
    }
}

impl std::fmt::Display for ProjectStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message)
            | Self::InvalidInput(message)
            | Self::Conflict(message)
            | Self::InvalidStore(message)
            | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProjectStoreError {}

impl From<ProjectStoreError> for String {
    fn from(error: ProjectStoreError) -> Self {
        error.to_string()
    }
}

#[derive(Debug)]
pub struct ProjectStore {
    path: PathBuf,
    records: HashMap<String, Project>,
}

impl ProjectStore {
    pub fn load(path: PathBuf) -> Result<Self, ProjectStoreError> {
        let (mut records, migrated) = Self::read_from_disk(&path)?;
        let heal_actions = heal_duplicate_standalone_roots(&mut records);
        for action in &heal_actions {
            tracing::warn!("project store heal: {action}");
        }
        // Older builds didn't enforce today's invariants at write time, so a
        // store strict `validate_records` rejects can still be legal legacy
        // data — refusing to start would brick the app until the user
        // hand-edits the file. Load leniently; mutations stay strict via
        // `persist_candidate`, so a kept-but-invalid record fails on the
        // next mutation instead (deleting it is always a valid repair).
        let record_count_before = records.len();
        let load_warnings = validate_records_for_load(&mut records);
        for warning in &load_warnings {
            tracing::warn!("project store load: {warning}");
        }
        let quarantined_records = records.len() != record_count_before;
        let store = Self { path, records };
        if migrated || !heal_actions.is_empty() || quarantined_records {
            store.save_current()?;
        }
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf, ProjectStoreError> {
        if let Ok(path) = std::env::var("TYDE_PROJECT_STORE_PATH") {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        Ok(crate::paths::home_dir()
            .map_err(ProjectStoreError::internal)?
            .join(".tyde")
            .join("projects.json"))
    }

    pub fn list(&self) -> Result<Vec<Project>, ProjectStoreError> {
        Ok(Self::ordered_projects(&self.records))
    }

    pub fn get(&self, id: &ProjectId) -> Option<Project> {
        self.records.get(&id.0).cloned()
    }

    pub fn create(
        &mut self,
        name: String,
        roots: Vec<ProjectRootPath>,
    ) -> Result<Project, ProjectStoreError> {
        validate_project_name(&name).map_err(ProjectStoreError::invalid_input)?;
        validate_standalone_roots_for_input(&roots).map_err(ProjectStoreError::invalid_input)?;
        for root in &roots {
            if let Some(owner) = root_path_owner(&self.records, root, None) {
                return Err(ProjectStoreError::conflict(format!(
                    "project root {} is already registered by project {}",
                    root, owner
                )));
            }
        }

        let id = self.generate_project_id();
        let project = Project {
            id: id.clone(),
            name,
            sort_order: Self::next_top_level_sort_order(&self.records),
            source: ProjectSource::Standalone { roots },
        };
        let mut records = self.records.clone();
        records.insert(id.0.clone(), project.clone());
        self.persist_candidate(records)?;
        Ok(project)
    }

    pub fn rename(&mut self, id: &ProjectId, name: String) -> Result<Project, ProjectStoreError> {
        validate_project_name(&name).map_err(ProjectStoreError::invalid_input)?;
        let mut records = self.records.clone();
        let Some(project) = records.get_mut(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot rename missing project {}",
                id
            )));
        };
        project.name = name;
        let updated = project.clone();
        self.persist_candidate(records)?;
        Ok(updated)
    }

    pub fn reorder(
        &mut self,
        scope: ProjectReorderScope,
        project_ids: Vec<ProjectId>,
    ) -> Result<Vec<Project>, ProjectStoreError> {
        let scope_projects = self.projects_in_scope(&scope)?;
        let scope_ids = scope_projects
            .iter()
            .map(|project| project.id.clone())
            .collect::<HashSet<_>>();

        let mut seen_ids = HashSet::new();
        for project_id in &project_ids {
            if !self.records.contains_key(&project_id.0) {
                return Err(ProjectStoreError::not_found(format!(
                    "cannot reorder missing project {}",
                    project_id
                )));
            }
            if !scope_ids.contains(project_id) {
                return Err(ProjectStoreError::invalid_input(format!(
                    "project {} is not in the requested reorder scope",
                    project_id
                )));
            }
            if !seen_ids.insert(project_id.clone()) {
                return Err(ProjectStoreError::conflict(format!(
                    "project reorder contains duplicate id {}",
                    project_id
                )));
            }
        }

        let mut ordered_ids = project_ids;
        ordered_ids.extend(
            scope_projects
                .iter()
                .filter(|project| !seen_ids.contains(&project.id))
                .map(|project| project.id.clone()),
        );

        let mut records = self.records.clone();
        for (index, project_id) in ordered_ids.into_iter().enumerate() {
            let Some(project) = records.get_mut(&project_id.0) else {
                return Err(ProjectStoreError::not_found(format!(
                    "cannot reorder missing project {}",
                    project_id
                )));
            };
            project.sort_order = index as u64;
        }

        self.persist_candidate(records)?;
        self.projects_in_scope(&scope)
    }

    pub fn add_root(
        &mut self,
        id: &ProjectId,
        root: ProjectRootPath,
    ) -> Result<Project, ProjectStoreError> {
        validate_project_root(&root).map_err(ProjectStoreError::invalid_input)?;
        let children = self.list_children(id);
        if let Some(child) = children.first() {
            return Err(ProjectStoreError::conflict(format!(
                "cannot add root to project {} while referenced by workbench {}",
                id, child
            )));
        }
        if let Some(owner) = root_path_owner(&self.records, &root, Some(id)) {
            return Err(ProjectStoreError::conflict(format!(
                "project root {} is already registered by project {}",
                root, owner
            )));
        }

        let mut records = self.records.clone();
        let Some(project) = records.get_mut(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot add root to missing project {}",
                id
            )));
        };
        match &mut project.source {
            ProjectSource::Standalone { roots } => {
                if roots.iter().any(|existing| existing == &root) {
                    return Err(ProjectStoreError::conflict(format!(
                        "project {} already contains root {}",
                        id, root
                    )));
                }
                roots.push(root);
            }
            ProjectSource::GitWorkbench { .. } => {
                return Err(ProjectStoreError::invalid_input(format!(
                    "cannot add root to workbench project {}",
                    id
                )));
            }
        }
        let updated = project.clone();
        self.persist_candidate(records)?;
        Ok(updated)
    }

    pub fn delete_root(
        &mut self,
        id: &ProjectId,
        root: &ProjectRootPath,
    ) -> Result<Project, ProjectStoreError> {
        let children = self.list_children(id);
        if let Some(child) = children.first() {
            return Err(ProjectStoreError::conflict(format!(
                "cannot delete root from project {} while referenced by workbench {}",
                id, child
            )));
        }

        let mut records = self.records.clone();
        let Some(project) = records.get_mut(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot delete root from missing project {}",
                id
            )));
        };
        match &mut project.source {
            ProjectSource::Standalone { roots } => {
                let original_len = roots.len();
                roots.retain(|existing| existing != root);
                if roots.len() == original_len {
                    return Err(ProjectStoreError::not_found(format!(
                        "project {} does not contain root {}",
                        id, root
                    )));
                }
                if roots.is_empty() {
                    return Err(ProjectStoreError::invalid_input(format!(
                        "cannot delete root {} from project {} because standalone projects require at least one root",
                        root, id
                    )));
                }
            }
            ProjectSource::GitWorkbench { .. } => {
                return Err(ProjectStoreError::invalid_input(format!(
                    "cannot delete root from workbench project {}",
                    id
                )));
            }
        }
        let updated = project.clone();
        self.persist_candidate(records)?;
        Ok(updated)
    }

    pub fn delete(&mut self, id: &ProjectId) -> Result<Project, ProjectStoreError> {
        let Some(existing) = self.records.get(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot delete missing project {}",
                id
            )));
        };
        if existing.is_workbench() {
            return Err(ProjectStoreError::invalid_input(format!(
                "cannot delete workbench project {} with ProjectDelete; use WorkbenchRemove",
                id
            )));
        }
        let children = self.list_children(id);
        if let Some(child) = children.first() {
            return Err(ProjectStoreError::conflict(format!(
                "cannot delete project {} while referenced by workbench {}",
                id, child
            )));
        }

        let mut records = self.records.clone();
        let Some(project) = records.remove(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot delete missing project {}",
                id
            )));
        };
        self.persist_candidate(records)?;
        Ok(project)
    }

    pub fn create_workbench(
        &mut self,
        parent_project_id: ProjectId,
        name: String,
        branch: GitBranchName,
        roots: Vec<WorkbenchRoot>,
    ) -> Result<Project, ProjectStoreError> {
        validate_project_name(&name).map_err(ProjectStoreError::invalid_input)?;
        if branch.0.trim().is_empty() {
            return Err(ProjectStoreError::invalid_input(
                "workbench branch must not be empty",
            ));
        }
        let Some(parent) = self.records.get(&parent_project_id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot create workbench for missing parent project {}",
                parent_project_id
            )));
        };
        let ProjectSource::Standalone {
            roots: parent_roots,
        } = &parent.source
        else {
            return Err(ProjectStoreError::invalid_input(format!(
                "cannot create workbench for non-standalone parent project {}",
                parent_project_id
            )));
        };
        validate_workbench_roots_for_parent(&roots, parent_roots, "new workbench")
            .map_err(ProjectStoreError::invalid_input)?;
        for root in &roots {
            if let Some(owner) = root_path_owner(&self.records, &root.worktree_root, None) {
                return Err(ProjectStoreError::conflict(format!(
                    "worktree root {} is already registered by project {}",
                    root.worktree_root, owner
                )));
            }
        }

        let id = self.generate_project_id();
        let project = Project {
            id: id.clone(),
            name,
            sort_order: Self::next_child_sort_order(&self.records, &parent_project_id),
            source: ProjectSource::GitWorkbench {
                parent_project_id,
                branch,
                roots,
            },
        };
        let mut records = self.records.clone();
        records.insert(id.0.clone(), project.clone());
        self.persist_candidate(records)?;
        Ok(project)
    }

    pub fn delete_workbench(&mut self, id: &ProjectId) -> Result<Project, ProjectStoreError> {
        let Some(existing) = self.records.get(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot delete missing workbench project {}",
                id
            )));
        };
        if !existing.is_workbench() {
            return Err(ProjectStoreError::invalid_input(format!(
                "cannot delete non-workbench project {} as workbench",
                id
            )));
        }

        let mut records = self.records.clone();
        let Some(project) = records.remove(&id.0) else {
            return Err(ProjectStoreError::not_found(format!(
                "cannot delete missing workbench project {}",
                id
            )));
        };
        self.persist_candidate(records)?;
        Ok(project)
    }

    pub fn list_children(&self, parent: &ProjectId) -> Vec<ProjectId> {
        let mut children = self
            .records
            .values()
            .filter(|project| project.parent_project_id() == Some(parent))
            .cloned()
            .collect::<Vec<_>>();
        sort_projects_by_sort_order(&mut children);
        children.into_iter().map(|project| project.id).collect()
    }

    fn read_from_disk(path: &Path) -> Result<(HashMap<String, Project>, bool), ProjectStoreError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::parse_store_file(path, &contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((HashMap::new(), false)),
            Err(err) => Err(ProjectStoreError::internal(format!(
                "Failed to read project store {}: {err}",
                path.display()
            ))),
        }
    }

    fn parse_store_file(
        path: &Path,
        contents: &str,
    ) -> Result<(HashMap<String, Project>, bool), ProjectStoreError> {
        let value = serde_json::from_str::<serde_json::Value>(contents).map_err(|err| {
            ProjectStoreError::internal(format!(
                "Failed to parse project store {}: {err}",
                path.display()
            ))
        })?;
        let version = value.get("version").and_then(serde_json::Value::as_u64);
        match version {
            Some(version) if version == STORE_VERSION as u64 => {
                let store = serde_json::from_value::<StoreFile>(value).map_err(|err| {
                    ProjectStoreError::internal(format!(
                        "Failed to parse project store {}: {err}",
                        path.display()
                    ))
                })?;
                Ok((store.records, false))
            }
            Some(version) => Err(ProjectStoreError::internal(format!(
                "Unsupported project store version {} in {}",
                version,
                path.display()
            ))),
            None => {
                let store = serde_json::from_value::<StoreFileV1>(value).map_err(|err| {
                    ProjectStoreError::internal(format!(
                        "Failed to parse v1 project store {}: {err}",
                        path.display()
                    ))
                })?;
                let records = store
                    .records
                    .into_iter()
                    .map(|(key, record)| {
                        (
                            key,
                            Project {
                                id: record.id,
                                name: record.name,
                                sort_order: record.sort_order,
                                source: ProjectSource::Standalone {
                                    roots: record.roots.into_iter().map(ProjectRootPath).collect(),
                                },
                            },
                        )
                    })
                    .collect::<HashMap<_, _>>();
                Ok((records, true))
            }
        }
    }

    fn persist_candidate(
        &mut self,
        records: HashMap<String, Project>,
    ) -> Result<(), ProjectStoreError> {
        validate_records(&records).map_err(ProjectStoreError::invalid_store)?;
        Self::save_to_path(&self.path, &records)?;
        self.records = records;
        Ok(())
    }

    fn save_current(&self) -> Result<(), ProjectStoreError> {
        Self::save_to_path(&self.path, &self.records)
    }

    fn save_to_path(
        path: &Path,
        records: &HashMap<String, Project>,
    ) -> Result<(), ProjectStoreError> {
        let json = serde_json::to_string_pretty(&StoreFile {
            version: STORE_VERSION,
            records: records.clone(),
        })
        .map_err(|err| {
            ProjectStoreError::internal(format!("Failed to serialize project store: {err}"))
        })?;

        let parent = path.parent().ok_or_else(|| {
            ProjectStoreError::internal(format!(
                "Project store path has no parent: {}",
                path.display()
            ))
        })?;
        std::fs::create_dir_all(parent).map_err(|err| {
            ProjectStoreError::internal(format!("Failed to create project store directory: {err}"))
        })?;

        let tmp_path = path.with_extension("json.tmp");
        let mut file = std::fs::File::create(&tmp_path).map_err(|err| {
            ProjectStoreError::internal(format!("Failed to create temp project store file: {err}"))
        })?;
        file.write_all(json.as_bytes()).map_err(|err| {
            ProjectStoreError::internal(format!("Failed to write temp project store file: {err}"))
        })?;
        file.sync_all().map_err(|err| {
            ProjectStoreError::internal(format!("Failed to sync temp project store file: {err}"))
        })?;
        std::fs::rename(&tmp_path, path).map_err(|err| {
            ProjectStoreError::internal(format!(
                "Failed to atomically replace project store {}: {err}",
                path.display()
            ))
        })?;
        Ok(())
    }

    fn ordered_projects(records: &HashMap<String, Project>) -> Vec<Project> {
        let mut top_level = records
            .values()
            .filter(|project| !project.is_workbench())
            .cloned()
            .collect::<Vec<_>>();
        sort_projects_by_sort_order(&mut top_level);

        let mut projects = top_level.clone();
        for parent in top_level {
            let mut children = records
                .values()
                .filter(|project| project.parent_project_id() == Some(&parent.id))
                .cloned()
                .collect::<Vec<_>>();
            sort_projects_by_sort_order(&mut children);
            projects.extend(children);
        }
        projects
    }

    fn projects_in_scope(
        &self,
        scope: &ProjectReorderScope,
    ) -> Result<Vec<Project>, ProjectStoreError> {
        let mut projects = match scope {
            ProjectReorderScope::TopLevel => self
                .records
                .values()
                .filter(|project| !project.is_workbench())
                .cloned()
                .collect::<Vec<_>>(),
            ProjectReorderScope::WorkbenchChildren { parent_project_id } => {
                let Some(parent) = self.records.get(&parent_project_id.0) else {
                    return Err(ProjectStoreError::not_found(format!(
                        "cannot reorder workbenches for missing parent project {}",
                        parent_project_id
                    )));
                };
                if parent.is_workbench() {
                    return Err(ProjectStoreError::invalid_input(format!(
                        "cannot reorder workbenches for non-standalone parent project {}",
                        parent_project_id
                    )));
                }
                self.records
                    .values()
                    .filter(|project| project.parent_project_id() == Some(parent_project_id))
                    .cloned()
                    .collect::<Vec<_>>()
            }
        };
        sort_projects_by_sort_order(&mut projects);
        Ok(projects)
    }

    fn next_top_level_sort_order(records: &HashMap<String, Project>) -> u64 {
        records
            .values()
            .filter(|project| !project.is_workbench())
            .map(|project| project.sort_order)
            .max()
            .map(|max_order| max_order.saturating_add(1))
            .unwrap_or(0)
    }

    fn next_child_sort_order(records: &HashMap<String, Project>, parent: &ProjectId) -> u64 {
        records
            .values()
            .filter(|project| project.parent_project_id() == Some(parent))
            .map(|project| project.sort_order)
            .max()
            .map(|max_order| max_order.saturating_add(1))
            .unwrap_or(0)
    }

    fn generate_project_id(&self) -> ProjectId {
        loop {
            let id = ProjectId(Uuid::new_v4().to_string());
            if !self.records.contains_key(&id.0) {
                return id;
            }
        }
    }
}

/// Pre-workbench versions of Tyde let two projects register the same root
/// (accidental duplicate projects exist in real stores). Workbenches require
/// unique root ownership, so heal old stores on load instead of refusing to
/// start: the earliest standalone project (by sort order, then id) keeps a
/// contested root, later projects lose it, and a project left with no roots
/// is removed entirely. A root a workbench depends on is never taken from
/// its parent — workbenches postdate the uniqueness rule, so that conflict
/// can only come from new data; load keeps both records with a warning and
/// strict validation rejects it again on the next mutation. Returns a
/// description of every change for logging; empty means nothing healed.
fn heal_duplicate_standalone_roots(records: &mut HashMap<String, Project>) -> Vec<String> {
    let workbench_required: HashSet<(ProjectId, ProjectRootPath)> = records
        .values()
        .filter_map(|project| match &project.source {
            ProjectSource::GitWorkbench {
                parent_project_id,
                roots,
                ..
            } => Some(
                roots
                    .iter()
                    .map(|root| (parent_project_id.clone(), root.parent_root.clone()))
                    .collect::<Vec<_>>(),
            ),
            ProjectSource::Standalone { .. } => None,
        })
        .flatten()
        .collect();
    let workbench_parents: HashSet<ProjectId> = workbench_required
        .iter()
        .map(|(parent_project_id, _)| parent_project_id.clone())
        .collect();

    let mut standalone_order: Vec<(u64, String)> = records
        .values()
        .filter(|project| matches!(project.source, ProjectSource::Standalone { .. }))
        .map(|project| (project.sort_order, project.id.0.clone()))
        .collect();
    standalone_order.sort();

    let mut actions = Vec::new();
    let mut owners = HashMap::<ProjectRootPath, ProjectId>::new();
    let mut emptied = Vec::new();
    for (_, key) in standalone_order {
        let Some(project) = records.get_mut(&key) else {
            continue;
        };
        let project_id = project.id.clone();
        let project_name = project.name.clone();
        let ProjectSource::Standalone { roots } = &mut project.source else {
            continue;
        };
        // A project that already had zero roots is legacy data, not a heal
        // casualty; leave it for the load validator to warn about.
        let started_with_roots = !roots.is_empty();
        let mut kept = Vec::new();
        for root in roots.drain(..) {
            match owners.get(&root) {
                Some(owner) if *owner != project_id => {
                    if workbench_required.contains(&(project_id.clone(), root.clone())) {
                        // A workbench needs this root from this parent;
                        // leave the conflict for validation to report.
                        kept.push(root);
                    } else {
                        actions.push(format!(
                            "dropped root {} from project {} ({}): already owned by project {}",
                            root, project_id, project_name, owner
                        ));
                    }
                }
                _ => {
                    owners.insert(root.clone(), project_id.clone());
                    kept.push(root);
                }
            }
        }
        let now_empty = kept.is_empty();
        *roots = kept;
        if now_empty && started_with_roots && !workbench_parents.contains(&project_id) {
            emptied.push((key, project_id, project_name));
        }
    }

    for (key, project_id, project_name) in emptied {
        records.remove(&key);
        actions.push(format!(
            "removed project {} ({}): every root was a duplicate of an earlier project",
            project_id, project_name
        ));
    }

    actions
}

/// Lenient validation used only at load time; mutations still run the
/// strict `validate_records`. Older builds never enforced some of today's
/// invariants at write time (shared roots across projects, standalone
/// projects with zero roots), so any projects.json a prior build wrote must
/// load. Records that can still function are kept with a warning;
/// quarantined (skipped and dropped on the next save) are only records that
/// cannot work at all: corrupt key/id pairs and workbench records that no
/// longer satisfy workbench invariants (e.g. a half-removed parent) — the
/// worktrees on disk are untouched. Returns the warnings to log.
fn validate_records_for_load(records: &mut HashMap<String, Project>) -> Vec<String> {
    let mut warnings = Vec::new();

    let broken_keys: Vec<String> = records
        .iter()
        .filter(|(key, project)| {
            key.trim().is_empty() || project.id.0.trim().is_empty() || *key != &project.id.0
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in broken_keys {
        records.remove(&key);
        warnings.push(format!(
            "skipped record with key {key:?}: key does not match a valid project id"
        ));
    }

    let snapshot: &HashMap<String, Project> = records;
    let broken_workbenches: Vec<(String, String)> = snapshot
        .values()
        .filter_map(|project| {
            workbench_load_problem(snapshot, project).map(|problem| (project.id.0.clone(), problem))
        })
        .collect();
    for (key, problem) in broken_workbenches {
        records.remove(&key);
        warnings.push(format!("skipped workbench record {key}: {problem}"));
    }

    // Legacy-legal violations: warn but keep, so the app starts.
    let mut owners = HashMap::<ProjectRootPath, ProjectId>::new();
    let mut projects: Vec<&Project> = records.values().collect();
    projects.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    for project in projects {
        if let ProjectSource::Standalone { roots } = &project.source
            && let Err(error) = validate_standalone_roots_for_input(roots)
        {
            warnings.push(format!(
                "standalone project {} {error}; loading it anyway",
                project.id
            ));
        }
        for root in project.root_paths() {
            match owners.get(&root) {
                Some(owner) if *owner != project.id => warnings.push(format!(
                    "root {} is shared by projects {} and {}; loading both",
                    root, owner, project.id
                )),
                _ => {
                    owners.insert(root, project.id.clone());
                }
            }
        }
    }

    warnings
}

/// Why a workbench record cannot be loaded at all, or None if it is (or is
/// not a workbench). Mirrors the workbench checks in `validate_records`.
fn workbench_load_problem(records: &HashMap<String, Project>, project: &Project) -> Option<String> {
    let ProjectSource::GitWorkbench {
        parent_project_id,
        branch,
        roots,
    } = &project.source
    else {
        return None;
    };
    if branch.0.trim().is_empty() {
        return Some("branch must not be empty".to_owned());
    }
    let Some(parent) = records.get(&parent_project_id.0) else {
        return Some(format!(
            "references missing parent project {parent_project_id}"
        ));
    };
    let ProjectSource::Standalone {
        roots: parent_roots,
    } = &parent.source
    else {
        return Some(format!(
            "parent project {parent_project_id} is not standalone"
        ));
    };
    validate_workbench_roots_for_parent(roots, parent_roots, "workbench").err()
}

fn validate_records(records: &HashMap<String, Project>) -> Result<(), String> {
    let mut actual_root_owners = HashMap::<ProjectRootPath, ProjectId>::new();
    let mut standalone_roots = HashMap::<ProjectId, Vec<ProjectRootPath>>::new();

    for (key, project) in records {
        if key.trim().is_empty() {
            return Err("Invalid project store: record key must not be empty".to_owned());
        }
        if project.id.0.trim().is_empty() {
            return Err("Invalid project store: project id must not be empty".to_owned());
        }
        if key != &project.id.0 {
            return Err(format!(
                "Invalid project store: record key {} does not match project id {}",
                key, project.id
            ));
        }
        validate_project_name(&project.name)
            .map_err(|error| format!("Invalid project store: project {} {error}", project.id))?;

        match &project.source {
            ProjectSource::Standalone { roots } => {
                validate_standalone_roots_for_input(roots).map_err(|error| {
                    format!("Invalid project store: project {} {error}", project.id)
                })?;
                standalone_roots.insert(project.id.clone(), roots.clone());
                for root in roots {
                    insert_actual_root_owner(&mut actual_root_owners, root, &project.id)?;
                }
            }
            ProjectSource::GitWorkbench { branch, roots, .. } => {
                if branch.0.trim().is_empty() {
                    return Err(format!(
                        "Invalid project store: workbench {} branch must not be empty",
                        project.id
                    ));
                }
                if roots.is_empty() {
                    return Err(format!(
                        "Invalid project store: workbench {} roots must not be empty",
                        project.id
                    ));
                }
                validate_workbench_root_uniqueness(roots).map_err(|error| {
                    format!("Invalid project store: workbench {} {error}", project.id)
                })?;
                for root in roots {
                    insert_actual_root_owner(
                        &mut actual_root_owners,
                        &root.worktree_root,
                        &project.id,
                    )?;
                }
            }
        }
    }

    for project in records.values() {
        let ProjectSource::GitWorkbench {
            parent_project_id,
            roots,
            ..
        } = &project.source
        else {
            continue;
        };
        let Some(parent) = records.get(&parent_project_id.0) else {
            return Err(format!(
                "Invalid project store: workbench {} references missing parent project {}",
                project.id, parent_project_id
            ));
        };
        if parent.is_workbench() {
            return Err(format!(
                "Invalid project store: workbench {} parent project {} is not standalone",
                project.id, parent_project_id
            ));
        }
        let Some(parent_roots) = standalone_roots.get(parent_project_id) else {
            return Err(format!(
                "Invalid project store: workbench {} parent project {} has no standalone roots",
                project.id, parent_project_id
            ));
        };
        validate_workbench_roots_for_parent(
            roots,
            parent_roots,
            &format!("workbench {}", project.id),
        )
        .map_err(|error| format!("Invalid project store: {error}"))?;
    }

    Ok(())
}

fn validate_project_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("name must not be empty".to_owned());
    }
    Ok(())
}

fn validate_project_root(root: &ProjectRootPath) -> Result<(), String> {
    if root.0.trim().is_empty() {
        return Err("project root must not be empty".to_owned());
    }
    Ok(())
}

fn validate_standalone_roots_for_input(roots: &[ProjectRootPath]) -> Result<(), String> {
    if roots.is_empty() {
        return Err("roots must not be empty".to_owned());
    }
    let mut seen = HashSet::new();
    for root in roots {
        validate_project_root(root)?;
        if !seen.insert(root.clone()) {
            return Err("roots must be unique".to_owned());
        }
    }
    Ok(())
}

fn validate_workbench_root_uniqueness(roots: &[WorkbenchRoot]) -> Result<(), String> {
    let mut parent_roots = HashSet::new();
    let mut worktree_roots = HashSet::new();
    for root in roots {
        validate_project_root(&root.parent_root)?;
        validate_project_root(&root.worktree_root)?;
        if !parent_roots.insert(root.parent_root.clone()) {
            return Err(format!(
                "parent_root {} appears more than once",
                root.parent_root
            ));
        }
        if !worktree_roots.insert(root.worktree_root.clone()) {
            return Err(format!(
                "worktree_root {} appears more than once",
                root.worktree_root
            ));
        }
    }
    Ok(())
}

fn validate_workbench_roots_for_parent(
    roots: &[WorkbenchRoot],
    parent_roots: &[ProjectRootPath],
    label: &str,
) -> Result<(), String> {
    if roots.is_empty() {
        return Err(format!("{label} roots must not be empty"));
    }
    validate_workbench_root_uniqueness(roots)?;

    let parent_root_set = parent_roots.iter().cloned().collect::<HashSet<_>>();
    if roots.len() != parent_root_set.len() {
        return Err(format!(
            "{label} must have one worktree root per parent root"
        ));
    }
    for root in roots {
        if !parent_root_set.contains(&root.parent_root) {
            return Err(format!(
                "{label} parent_root {} does not match a parent project root",
                root.parent_root
            ));
        }
    }
    Ok(())
}

fn insert_actual_root_owner(
    owners: &mut HashMap<ProjectRootPath, ProjectId>,
    root: &ProjectRootPath,
    project_id: &ProjectId,
) -> Result<(), String> {
    if let Some(existing_owner) = owners.insert(root.clone(), project_id.clone()) {
        return Err(format!(
            "Invalid project store: root {} is shared by projects {} and {}",
            root, existing_owner, project_id
        ));
    }
    Ok(())
}

fn root_path_owner(
    records: &HashMap<String, Project>,
    root: &ProjectRootPath,
    except_id: Option<&ProjectId>,
) -> Option<ProjectId> {
    records.values().find_map(|project| {
        if except_id == Some(&project.id) {
            return None;
        }
        project
            .root_paths()
            .into_iter()
            .any(|candidate| candidate == *root)
            .then(|| project.id.clone())
    })
}

fn sort_projects_by_sort_order(projects: &mut [Project]) {
    projects.sort_by(|left, right| {
        left.sort_order
            .cmp(&right.sort_order)
            .then(left.name.cmp(&right.name))
            .then(left.id.0.cmp(&right.id.0))
    });
}
