use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use protocol::{
    BackendKind, Project, ProjectId, TriggerSurface, WorkflowCoordinatorSpec, WorkflowDiagnostic,
    WorkflowDiagnosticSeverity, WorkflowId, WorkflowInputSpec, WorkflowSource, WorkflowSourceScope,
    WorkflowSummary,
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

#[derive(Debug, Deserialize)]
struct RawWorkflowFrontMatter {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    triggers: Vec<serde_yaml::Value>,
    #[serde(default)]
    inputs: Vec<WorkflowInputSpec>,
    coordinator: WorkflowCoordinatorSpec,
    #[serde(default)]
    declared_backends: Vec<BackendKind>,
    #[serde(default)]
    tags: Vec<String>,
}

impl WorkflowCatalog {
    pub(crate) fn discover(projects: &[Project]) -> Self {
        let mut catalog = Self::default();
        catalog.discover_global();
        for project in projects {
            catalog.discover_project(project);
        }
        catalog.rebuild_summaries();
        catalog
    }

    pub(crate) fn summaries(&self) -> Vec<WorkflowSummary> {
        self.summaries.clone()
    }

    pub(crate) fn diagnostics(&self) -> Vec<WorkflowDiagnostic> {
        self.diagnostics.clone()
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
            let dir = PathBuf::from(&root.0).join(".tyde").join("workflows");
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
        let mut files = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .collect::<Vec<_>>();
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
                        severity: WorkflowDiagnosticSeverity::Warning,
                        message: "duplicate workflow id in the same scope; ignoring later file"
                            .to_owned(),
                    });
                    return;
                }
                self.definitions.insert(key, definition);
            }
            Err(message) => self.diagnostics.push(WorkflowDiagnostic {
                workflow_id: None,
                source: Some(source),
                severity: WorkflowDiagnosticSeverity::Error,
                message,
            }),
        }
    }

    fn rebuild_summaries(&mut self) {
        let project_override_ids = self
            .definitions
            .keys()
            .filter(|key| key.project_id.is_some())
            .map(|key| key.id.clone())
            .collect::<HashSet<_>>();
        let mut ordered = BTreeMap::<(String, String), WorkflowSummary>::new();
        for (key, definition) in &self.definitions {
            if key.project_id.is_none() && project_override_ids.contains(&key.id) {
                continue;
            }
            let source_key = definition.summary.source.path.clone();
            ordered.insert(
                (definition.summary.id.0.clone(), source_key),
                definition.summary.clone(),
            );
        }
        self.summaries = ordered.into_values().collect();
    }
}

fn global_workflows_dir() -> PathBuf {
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

fn project_id_from_scope(scope: &WorkflowSourceScope) -> Option<ProjectId> {
    match scope {
        WorkflowSourceScope::Global => None,
        WorkflowSourceScope::Project { project_id, .. } => Some(project_id.clone()),
    }
}

fn parse_workflow_file(path: &Path, source: WorkflowSource) -> Result<WorkflowDefinition, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read workflow file: {err}"))?;
    let (front_matter, body) = split_front_matter(&contents)?;
    let raw: RawWorkflowFrontMatter = serde_yaml::from_str(front_matter)
        .map_err(|err| format!("failed to parse workflow front matter: {err}"))?;
    let id = raw.id.trim();
    if id.is_empty() {
        return Err("workflow id must not be empty".to_owned());
    }
    let name = raw.name.trim();
    if name.is_empty() {
        return Err("workflow name must not be empty".to_owned());
    }
    let triggers = parse_triggers(raw.triggers)?;
    let summary = WorkflowSummary {
        id: WorkflowId(id.to_owned()),
        name: name.to_owned(),
        description: raw.description.filter(|value| !value.trim().is_empty()),
        triggers,
        inputs: raw.inputs,
        coordinator: raw.coordinator,
        declared_backends: raw.declared_backends,
        tags: raw.tags,
        source,
    };
    Ok(WorkflowDefinition {
        summary,
        body: body.to_owned(),
    })
}

fn split_front_matter(contents: &str) -> Result<(&str, &str), String> {
    let Some(rest) = contents.strip_prefix("---") else {
        return Err("workflow file must start with YAML front matter".to_owned());
    };
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .ok_or_else(|| "workflow front matter opener must be followed by a newline".to_owned())?;
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
    Err("workflow front matter is missing closing ---".to_owned())
}

fn parse_triggers(values: Vec<serde_yaml::Value>) -> Result<Vec<TriggerSurface>, String> {
    if values.is_empty() {
        return Ok(vec![TriggerSurface::Global]);
    }
    values.into_iter().map(parse_trigger).collect()
}

fn parse_trigger(value: serde_yaml::Value) -> Result<TriggerSurface, String> {
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
            .ok_or_else(|| "trigger mapping must include kind".to_owned())?;
        let glob = map
            .get(serde_yaml::Value::String("glob".to_owned()))
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        return trigger_from_name(kind, glob);
    }
    Err("trigger must be a string or mapping".to_owned())
}

fn glob_from_value(value: &serde_yaml::Value) -> Option<String> {
    value
        .as_mapping()
        .and_then(|map| map.get(serde_yaml::Value::String("glob".to_owned())))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn trigger_from_name(name: &str, glob: Option<String>) -> Result<TriggerSurface, String> {
    match name.trim() {
        "git_panel" => Ok(TriggerSurface::GitPanel),
        "review_hub" => Ok(TriggerSurface::ReviewHub),
        "chat_input" => Ok(TriggerSurface::ChatInput),
        "global" => Ok(TriggerSurface::Global),
        "file_view" => Ok(TriggerSurface::FileView {
            glob: glob.ok_or_else(|| "file_view trigger requires glob".to_owned())?,
        }),
        other => Err(format!("unknown trigger surface {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{BackendAccessMode, ProjectSource};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn valid(id: &str, name: &str, body: &str) -> String {
        format!(
            "---\nid: {id}\nname: {name}\ncoordinator:\n  backend: codex\n  access_mode: read_only\ndeclared_backends: [codex]\n---\n{body}\n"
        )
    }

    #[test]
    fn project_overrides_global_and_bad_files_emit_diagnostics() {
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
        unsafe {
            std::env::set_var("TYDE_GLOBAL_WORKFLOWS_DIR", &global);
        }
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
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Project Build");
        assert_eq!(
            summaries[0].coordinator.access_mode,
            BackendAccessMode::ReadOnly
        );
        assert!(!catalog.diagnostics().is_empty());
        assert_eq!(
            catalog
                .resolve(&WorkflowId("build".to_owned()), Some(&project.id))
                .unwrap()
                .body
                .trim(),
            "project"
        );
        unsafe {
            std::env::remove_var("TYDE_GLOBAL_WORKFLOWS_DIR");
        }
    }
}
