use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use protocol::{
    BackendKind, Project, ProjectId, TriggerSurface, WorkflowCatalogLocation,
    WorkflowCoordinatorSpec, WorkflowDiagnostic, WorkflowDiagnosticSeverity, WorkflowId,
    WorkflowInputControl, WorkflowInputOption, WorkflowInputSpec, WorkflowSource,
    WorkflowSourceScope, WorkflowSummary,
};
use serde::Deserialize;

#[derive(Clone, Debug)]
pub(crate) struct WorkflowDefinition {
    pub summary: WorkflowSummary,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowCatalog {
    definitions: HashMap<WorkflowCatalogKey, WorkflowDefinition>,
    summaries: Vec<WorkflowSummary>,
    diagnostics: Vec<WorkflowDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct WorkflowCatalogKey {
    id: WorkflowId,
    project_id: Option<ProjectId>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowParseError {
    message: String,
    severity: WorkflowDiagnosticSeverity,
}

#[derive(Debug, Deserialize)]
struct RawWorkflowFrontMatter {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    triggers: Vec<serde_yaml::Value>,
    #[serde(default)]
    inputs: Vec<RawWorkflowInputSpec>,
    coordinator: WorkflowCoordinatorSpec,
    #[serde(default)]
    declared_backends: Vec<BackendKind>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawWorkflowInputSpec {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default, alias = "input_type")]
    control: Option<String>,
    #[serde(default)]
    options: Vec<WorkflowInputOption>,
    #[serde(default)]
    default: Option<serde_json::Value>,
}

impl WorkflowParseError {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: WorkflowDiagnosticSeverity::Error,
        }
    }

    fn warning(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: WorkflowDiagnosticSeverity::Warning,
        }
    }
}

impl WorkflowCatalog {
    pub(crate) fn discover(projects: &[Project]) -> Self {
        let mut catalog = Self::default();
        catalog.discover_global();
        for project in projects {
            catalog.discover_project(project);
        }
        catalog.add_shadowing_diagnostics();
        catalog.rebuild_summaries();
        catalog
    }

    pub(crate) fn summaries(&self) -> Vec<WorkflowSummary> {
        self.summaries.clone()
    }

    pub(crate) fn diagnostics(&self) -> Vec<WorkflowDiagnostic> {
        self.diagnostics.clone()
    }

    pub(crate) fn push_diagnostic(&mut self, diagnostic: WorkflowDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub(crate) fn diagnostics_for_path(&self, path: &str) -> Vec<WorkflowDiagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .source
                    .as_ref()
                    .is_some_and(|source| source.path == path)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn summary_for_path(&self, path: &str) -> Option<WorkflowSummary> {
        self.definitions
            .values()
            .find(|definition| definition.summary.source.path == path)
            .map(|definition| definition.summary.clone())
    }

    pub(crate) fn has_same_scope_id(
        &self,
        scope: &WorkflowSourceScope,
        workflow_id: &WorkflowId,
    ) -> bool {
        let key = WorkflowCatalogKey {
            id: workflow_id.clone(),
            project_id: project_id_from_scope(scope),
        };
        self.definitions.contains_key(&key)
    }

    pub(crate) fn resolve(
        &self,
        workflow_id: &WorkflowId,
        project_id: Option<&ProjectId>,
    ) -> Option<WorkflowDefinition> {
        if let Some(project_id) = project_id {
            let key = WorkflowCatalogKey {
                id: workflow_id.clone(),
                project_id: Some(project_id.clone()),
            };
            if let Some(definition) = self.definitions.get(&key) {
                return Some(definition.clone());
            }
        }
        let key = WorkflowCatalogKey {
            id: workflow_id.clone(),
            project_id: None,
        };
        self.definitions.get(&key).cloned()
    }

    fn discover_global(&mut self) {
        let dir = global_workflows_dir();
        self.discover_dir(dir, WorkflowSourceScope::Global);
    }

    fn discover_project(&mut self, project: &Project) {
        for root in project.root_paths() {
            let dir = project_workflows_dir(&root);
            self.discover_dir(
                dir,
                WorkflowSourceScope::Project {
                    project_id: project.id.clone(),
                    root,
                },
            );
        }
    }

    fn discover_dir(&mut self, dir: PathBuf, scope: WorkflowSourceScope) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(err) => {
                self.diagnostics.push(WorkflowDiagnostic {
                    workflow_id: None,
                    source: Some(WorkflowSource {
                        scope,
                        path: dir.display().to_string(),
                    }),
                    severity: WorkflowDiagnosticSeverity::Error,
                    message: format!("failed to read workflow directory: {err}"),
                });
                return;
            }
        };
        let mut files = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                        files.push(path);
                    }
                }
                Err(err) => self.diagnostics.push(WorkflowDiagnostic {
                    workflow_id: None,
                    source: Some(WorkflowSource {
                        scope: scope.clone(),
                        path: dir.display().to_string(),
                    }),
                    severity: WorkflowDiagnosticSeverity::Error,
                    message: format!("failed to read workflow directory entry: {err}"),
                }),
            }
        }
        files.sort();
        for path in files {
            self.discover_file(path, scope.clone());
        }
    }

    fn discover_file(&mut self, path: PathBuf, scope: WorkflowSourceScope) {
        let source = WorkflowSource {
            scope: scope.clone(),
            path: path.display().to_string(),
        };
        match parse_workflow_file(&path, source.clone()) {
            Ok(definition) => {
                let key = WorkflowCatalogKey {
                    id: definition.summary.id.clone(),
                    project_id: project_id_from_scope(&scope),
                };
                if self.definitions.contains_key(&key) {
                    self.diagnostics.push(WorkflowDiagnostic {
                        workflow_id: Some(definition.summary.id.clone()),
                        source: Some(source),
                        severity: WorkflowDiagnosticSeverity::Error,
                        message: "duplicate workflow id in the same scope; ignoring later file"
                            .to_owned(),
                    });
                    return;
                }
                self.definitions.insert(key, definition);
            }
            Err(error) => self.diagnostics.push(WorkflowDiagnostic {
                workflow_id: None,
                source: Some(source),
                severity: error.severity,
                message: error.message,
            }),
        }
    }

    fn add_shadowing_diagnostics(&mut self) {
        let global_ids = self
            .definitions
            .keys()
            .filter(|key| key.project_id.is_none())
            .map(|key| key.id.clone())
            .collect::<HashSet<_>>();
        let shadowing = self
            .definitions
            .iter()
            .filter(|(key, _definition)| key.project_id.is_some() && global_ids.contains(&key.id))
            .map(|(key, definition)| (key.id.clone(), definition.summary.source.clone()))
            .collect::<Vec<_>>();

        for (workflow_id, source) in shadowing {
            self.diagnostics.push(WorkflowDiagnostic {
                workflow_id: Some(workflow_id.clone()),
                source: Some(source),
                severity: WorkflowDiagnosticSeverity::Warning,
                message: format!(
                    "project workflow {workflow_id} shadows a global workflow only in this project"
                ),
            });
        }
    }

    fn rebuild_summaries(&mut self) {
        let mut ordered = BTreeMap::<(String, String), WorkflowSummary>::new();
        for definition in self.definitions.values() {
            let source_key = definition.summary.source.path.clone();
            ordered.insert(
                (definition.summary.id.0.clone(), source_key),
                definition.summary.clone(),
            );
        }
        self.summaries = ordered.into_values().collect();
    }
}

pub(crate) fn global_workflows_dir() -> PathBuf {
    if let Ok(path) = std::env::var("TYDE_GLOBAL_WORKFLOWS_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    crate::paths::home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".tyde")
        .join("workflows")
}

pub(crate) fn project_workflows_dir(root: &protocol::ProjectRootPath) -> PathBuf {
    PathBuf::from(&root.0).join(".tyde").join("workflows")
}

pub(crate) fn workflow_catalog_locations(projects: &[Project]) -> Vec<WorkflowCatalogLocation> {
    let global = global_workflows_dir();
    let mut locations = vec![WorkflowCatalogLocation {
        scope: WorkflowSourceScope::Global,
        directory: global.display().to_string(),
        exists: global.is_dir(),
    }];

    for project in projects {
        for root in project.root_paths() {
            let dir = project_workflows_dir(&root);
            locations.push(WorkflowCatalogLocation {
                scope: WorkflowSourceScope::Project {
                    project_id: project.id.clone(),
                    root,
                },
                directory: dir.display().to_string(),
                exists: dir.is_dir(),
            });
        }
    }

    locations
}

pub(crate) fn workflow_watch_dirs(projects: &[Project]) -> Vec<PathBuf> {
    let mut dirs = vec![global_workflows_dir()];
    for project in projects {
        for root in project.root_paths() {
            dirs.push(project_workflows_dir(&root));
        }
    }
    dirs
}

fn project_id_from_scope(scope: &WorkflowSourceScope) -> Option<ProjectId> {
    match scope {
        WorkflowSourceScope::Global => None,
        WorkflowSourceScope::Project { project_id, .. } => Some(project_id.clone()),
    }
}

pub(crate) fn parse_workflow_file(
    path: &Path,
    source: WorkflowSource,
) -> Result<WorkflowDefinition, WorkflowParseError> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| WorkflowParseError::error(format!("failed to read workflow file: {err}")))?;
    parse_workflow_content(&contents, source)
}

pub(crate) fn parse_workflow_content(
    markdown: &str,
    source: WorkflowSource,
) -> Result<WorkflowDefinition, WorkflowParseError> {
    let (front_matter, body) = split_front_matter(markdown)?;
    let raw: RawWorkflowFrontMatter = serde_yaml::from_str(front_matter).map_err(|err| {
        WorkflowParseError::error(format!("failed to parse workflow front matter: {err}"))
    })?;
    let id = raw.id.trim();
    if id.is_empty() {
        return Err(WorkflowParseError::error("workflow id must not be empty"));
    }
    if !valid_workflow_id(id) {
        return Err(WorkflowParseError::warning(
            "workflow id must match ^[a-z0-9][a-z0-9_-]{0,63}$",
        ));
    }
    let name = raw.name.trim();
    if name.is_empty() {
        return Err(WorkflowParseError::error("workflow name must not be empty"));
    }
    if body.trim().is_empty() {
        return Err(WorkflowParseError::error("workflow body must not be empty"));
    }
    let triggers = parse_triggers(raw.triggers)?;
    let inputs = validate_inputs(raw.inputs)?;
    let summary = WorkflowSummary {
        id: WorkflowId(id.to_owned()),
        name: name.to_owned(),
        description: raw.description.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }),
        triggers,
        inputs,
        coordinator: raw.coordinator,
        declared_backends: raw.declared_backends,
        tags: raw
            .tags
            .into_iter()
            .filter_map(|tag| {
                let trimmed = tag.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_owned())
            })
            .collect(),
        source,
    };
    Ok(WorkflowDefinition {
        summary,
        body: body.to_owned(),
    })
}

fn split_front_matter(contents: &str) -> Result<(&str, &str), WorkflowParseError> {
    let Some(rest) = contents.strip_prefix("---") else {
        return Err(WorkflowParseError::error(
            "workflow file must start with YAML front matter",
        ));
    };
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .ok_or_else(|| {
            WorkflowParseError::error("workflow front matter opener must be followed by a newline")
        })?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if line_without_newline == "---" {
            let front = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Ok((front, body));
        }
        offset += line.len();
    }
    Err(WorkflowParseError::error(
        "workflow front matter is missing closing ---",
    ))
}

fn valid_workflow_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if id.len() > 64 || !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn validate_inputs(
    inputs: Vec<RawWorkflowInputSpec>,
) -> Result<Vec<WorkflowInputSpec>, WorkflowParseError> {
    let mut seen = HashSet::new();
    let mut validated = Vec::with_capacity(inputs.len());
    for input in inputs {
        let id = input.id.trim();
        if id.is_empty() {
            return Err(WorkflowParseError::warning(
                "workflow input id must not be empty",
            ));
        }
        if !seen.insert(id.to_owned()) {
            return Err(WorkflowParseError::warning(format!(
                "duplicate workflow input id {id:?}"
            )));
        }
        let control = parse_input_control(input.control.as_deref())?;
        let input = WorkflowInputSpec {
            id: id.to_owned(),
            name: input.name,
            description: input.description,
            required: input.required,
            control,
            options: input.options,
            default: input.default,
        };
        validate_input_options(&input)?;
        validate_input_default(&input)?;
        validated.push(input);
    }
    Ok(validated)
}

fn parse_input_control(control: Option<&str>) -> Result<WorkflowInputControl, WorkflowParseError> {
    let Some(control) = control else {
        return Ok(WorkflowInputControl::Text);
    };
    match control.trim() {
        "" | "text" => Ok(WorkflowInputControl::Text),
        "multiline_text" => Ok(WorkflowInputControl::MultilineText),
        "boolean" => Ok(WorkflowInputControl::Boolean),
        "number" => Ok(WorkflowInputControl::Number),
        "select" => Ok(WorkflowInputControl::Select),
        "file_path" => Ok(WorkflowInputControl::FilePath),
        other => Err(WorkflowParseError::warning(format!(
            "unknown workflow input control kind {other:?}"
        ))),
    }
}

fn validate_input_options(input: &WorkflowInputSpec) -> Result<(), WorkflowParseError> {
    match input.control {
        WorkflowInputControl::Select if input.options.is_empty() => {
            Err(WorkflowParseError::warning(format!(
                "select workflow input {:?} must declare at least one option",
                input.id
            )))
        }
        WorkflowInputControl::Select => Ok(()),
        WorkflowInputControl::Text
        | WorkflowInputControl::MultilineText
        | WorkflowInputControl::Boolean
        | WorkflowInputControl::Number
        | WorkflowInputControl::FilePath => Ok(()),
    }
}

fn validate_input_default(input: &WorkflowInputSpec) -> Result<(), WorkflowParseError> {
    let Some(default) = input.default.as_ref() else {
        return Ok(());
    };
    let valid = match input.control {
        WorkflowInputControl::Text
        | WorkflowInputControl::MultilineText
        | WorkflowInputControl::FilePath => default.is_string(),
        WorkflowInputControl::Boolean => default.is_boolean(),
        WorkflowInputControl::Number => default.is_number(),
        WorkflowInputControl::Select => default
            .as_str()
            .is_some_and(|value| input.options.iter().any(|option| option.value == value)),
    };
    if valid {
        return Ok(());
    }
    Err(WorkflowParseError::warning(format!(
        "default for workflow input {:?} must be {}",
        input.id,
        expected_default_type(input.control)
    )))
}

fn expected_default_type(control: WorkflowInputControl) -> &'static str {
    match control {
        WorkflowInputControl::Boolean => "a boolean",
        WorkflowInputControl::Number => "a number",
        WorkflowInputControl::Text
        | WorkflowInputControl::MultilineText
        | WorkflowInputControl::FilePath => "a string",
        WorkflowInputControl::Select => "one of its select option values",
    }
}

fn parse_triggers(
    values: Vec<serde_yaml::Value>,
) -> Result<Vec<TriggerSurface>, WorkflowParseError> {
    if values.is_empty() {
        return Ok(vec![TriggerSurface::Global]);
    }
    values.into_iter().map(parse_trigger).collect()
}

fn parse_trigger(value: serde_yaml::Value) -> Result<TriggerSurface, WorkflowParseError> {
    if let Some(text) = value.as_str() {
        return trigger_from_name(text, None);
    }
    if let Some(map) = value.as_mapping() {
        if map.len() == 1
            && let Some((key, inner)) = map.iter().next()
            && let Some(name) = key.as_str()
        {
            if name == "file_view" {
                return trigger_from_name(name, glob_from_value(inner));
            }
            return trigger_from_name(name, None);
        }
        let kind = map
            .get(serde_yaml::Value::String("kind".to_owned()))
            .or_else(|| map.get(serde_yaml::Value::String("surface".to_owned())))
            .and_then(|value| value.as_str())
            .ok_or_else(|| WorkflowParseError::error("trigger mapping must include kind"))?;
        let glob = map
            .get(serde_yaml::Value::String("glob".to_owned()))
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        return trigger_from_name(kind, glob);
    }
    Err(WorkflowParseError::error(
        "trigger must be a string or mapping",
    ))
}

fn glob_from_value(value: &serde_yaml::Value) -> Option<String> {
    value
        .as_mapping()
        .and_then(|map| map.get(serde_yaml::Value::String("glob".to_owned())))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn trigger_from_name(
    name: &str,
    glob: Option<String>,
) -> Result<TriggerSurface, WorkflowParseError> {
    match name.trim() {
        "git_panel" => Ok(TriggerSurface::GitPanel),
        "review_hub" => Ok(TriggerSurface::ReviewHub),
        "chat_input" => Ok(TriggerSurface::ChatInput),
        "global" => Ok(TriggerSurface::Global),
        "file_view" => Ok(TriggerSurface::FileView {
            glob: glob
                .ok_or_else(|| WorkflowParseError::error("file_view trigger requires glob"))?,
        }),
        other => Err(WorkflowParseError::error(format!(
            "unknown trigger surface {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{BackendAccessMode, ProjectSource, WorkflowInputControl};
    use std::sync::{Mutex, OnceLock};

    static GLOBAL_WORKFLOWS_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct GlobalWorkflowsEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl GlobalWorkflowsEnv {
        fn set(path: &Path) -> Self {
            let guard = GLOBAL_WORKFLOWS_ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap();
            let previous = std::env::var("TYDE_GLOBAL_WORKFLOWS_DIR").ok();
            unsafe {
                std::env::set_var("TYDE_GLOBAL_WORKFLOWS_DIR", path);
            }
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for GlobalWorkflowsEnv {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var("TYDE_GLOBAL_WORKFLOWS_DIR", previous);
                } else {
                    std::env::remove_var("TYDE_GLOBAL_WORKFLOWS_DIR");
                }
            }
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn valid(id: &str, name: &str, body: &str) -> String {
        format!(
            "---\nid: {id}\nname: {name}\ncoordinator:\n  backend: codex\n  access_mode: read_only\ndeclared_backends: [codex]\n---\n{body}\n"
        )
    }

    fn source(path: &str) -> WorkflowSource {
        WorkflowSource {
            scope: WorkflowSourceScope::Global,
            path: path.to_owned(),
        }
    }

    #[test]
    fn select_control_requires_options() {
        let markdown = "---\nid: select-empty\nname: Select Empty\ncoordinator:\n  backend: codex\ninputs:\n  - id: mode\n    control: select\n---\nBody\n";

        let error = parse_workflow_content(markdown, source("select-empty.md"))
            .expect_err("select without options should fail");

        assert_eq!(error.severity, WorkflowDiagnosticSeverity::Warning);
        assert!(
            error
                .message()
                .contains("select workflow input \"mode\" must declare at least one option"),
            "unexpected select options error: {}",
            error.message()
        );
    }

    #[test]
    fn select_default_must_match_options_and_valid_default_parses() {
        let invalid = "---\nid: select-default-bad\nname: Select Default Bad\ncoordinator:\n  backend: codex\ninputs:\n  - id: mode\n    control: select\n    options:\n      - value: fast\n      - value: safe\n    default: turbo\n---\nBody\n";

        let error = parse_workflow_content(invalid, source("select-default-bad.md"))
            .expect_err("select default outside options should fail");
        assert_eq!(error.severity, WorkflowDiagnosticSeverity::Warning);
        assert!(
            error.message().contains(
                "default for workflow input \"mode\" must be one of its select option values"
            ),
            "unexpected select default error: {}",
            error.message()
        );

        let valid = "---\nid: select-default-ok\nname: Select Default Ok\ncoordinator:\n  backend: codex\ninputs:\n  - id: mode\n    control: select\n    options:\n      - value: fast\n      - value: safe\n    default: safe\n---\nBody\n";
        let definition = parse_workflow_content(valid, source("select-default-ok.md"))
            .expect("select default in options should parse");
        let input = definition
            .summary
            .inputs
            .first()
            .expect("select input should be preserved");
        assert_eq!(input.control, WorkflowInputControl::Select);
        assert_eq!(input.default.as_ref(), Some(&serde_json::json!("safe")));
    }

    #[test]
    fn legacy_input_type_alias_migrates_known_and_warns_on_unknown() {
        let known = "---\nid: legacy-input-type\nname: Legacy Input Type\ncoordinator:\n  backend: codex\ninputs:\n  - id: target\n    input_type: text\n---\nBody\n";
        let definition = parse_workflow_content(known, source("legacy-input-type.md"))
            .expect("known legacy input_type should parse");
        assert_eq!(
            definition.summary.inputs[0].control,
            WorkflowInputControl::Text
        );

        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        write(
            &global.join("unknown-input-type.md"),
            "---\nid: unknown-input-type\nname: Unknown Input Type\ncoordinator:\n  backend: codex\ninputs:\n  - id: target\n    input_type: mystery\n---\nBody\n",
        );
        let _env = GlobalWorkflowsEnv::set(&global);

        let catalog = WorkflowCatalog::discover(&[]);

        assert!(catalog.summaries().is_empty());
        assert!(catalog.diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == WorkflowDiagnosticSeverity::Warning
                && diagnostic
                    .source
                    .as_ref()
                    .is_some_and(|source| source.path.ends_with("unknown-input-type.md"))
                && diagnostic
                    .message
                    .contains("unknown workflow input control kind \"mystery\"")
        }));
    }

    #[test]
    fn project_shadowing_is_scoped_and_bad_files_emit_diagnostics() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let project_root = tmp.path().join("repo");
        write(
            &global.join("build.md"),
            &valid("build", "Global Build", "global"),
        );
        write(
            &project_root.join(".tyde/workflows/build.md"),
            &valid("build", "Project Build", "project"),
        );
        write(
            &project_root.join(".tyde/workflows/bad.md"),
            "---\nid: bad\n",
        );
        let _env = GlobalWorkflowsEnv::set(&global);
        let project = Project {
            id: ProjectId("p1".to_owned()),
            name: "Repo".to_owned(),
            sort_order: 0,
            source: ProjectSource::Standalone {
                roots: vec![protocol::ProjectRootPath(
                    project_root.display().to_string(),
                )],
            },
        };

        let catalog = WorkflowCatalog::discover(std::slice::from_ref(&project));
        let summaries = catalog.summaries();
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries
                .iter()
                .any(|summary| summary.name == "Global Build")
        );
        assert!(
            summaries
                .iter()
                .any(|summary| summary.name == "Project Build")
        );
        let project_summary = summaries
            .iter()
            .find(|summary| summary.name == "Project Build")
            .unwrap();
        assert_eq!(
            project_summary.coordinator.access_mode,
            BackendAccessMode::ReadOnly
        );
        assert!(catalog.diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == WorkflowDiagnosticSeverity::Warning
                && diagnostic.message.contains("shadows")
        }));
        assert!(catalog.diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == WorkflowDiagnosticSeverity::Error
                && diagnostic.message.contains("front matter")
        }));
        assert_eq!(
            catalog
                .resolve(&WorkflowId("build".to_owned()), Some(&project.id))
                .unwrap()
                .body
                .trim(),
            "project"
        );
        assert_eq!(
            catalog
                .resolve(&WorkflowId("build".to_owned()), None)
                .unwrap()
                .body
                .trim(),
            "global"
        );
    }
}
