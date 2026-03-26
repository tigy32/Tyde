use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tokio::process::Command;

#[derive(Serialize, Deserialize, Clone)]
pub struct WorkflowActionEntry {
    #[serde(rename = "type")]
    pub action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "workflowId", skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WorkflowStepEntry {
    pub name: String,
    pub actions: Vec<WorkflowActionEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct WorkflowDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub trigger: String,
    pub steps: Vec<WorkflowStepEntry>,
}

#[derive(Serialize, Clone)]
pub struct WorkflowEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub trigger: String,
    pub steps: Vec<WorkflowStepEntry>,
    pub scope: String,
}

#[derive(Serialize)]
pub struct ShellCommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
}

fn resolve_global_workflows_dir() -> Result<PathBuf, String> {
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".tyde").join("workflows"));
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(profile).join(".tyde").join("workflows"));
    }
    Err("Could not determine home directory for workflows".to_string())
}

fn resolve_project_workflows_dir(workspace_path: &str) -> PathBuf {
    PathBuf::from(workspace_path)
        .join(".tyde")
        .join("workflows")
}

async fn read_workflows_from_dir(dir: &PathBuf, scope: &str) -> Vec<WorkflowEntry> {
    let mut entries = Vec::new();

    let mut reader = match fs::read_dir(dir).await {
        Ok(reader) => reader,
        Err(_) => return entries,
    };

    while let Ok(Some(entry)) = reader.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            match fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str::<WorkflowDefinition>(&content) {
                    Ok(def) => {
                        entries.push(WorkflowEntry {
                            id: def.id,
                            name: def.name,
                            description: def.description,
                            trigger: def.trigger,
                            steps: def.steps,
                            scope: scope.to_string(),
                        });
                    }
                    Err(err) => {
                        tracing::warn!("Failed to parse workflow {}: {err}", path.display());
                    }
                },
                Err(err) => {
                    tracing::warn!("Failed to read workflow file {}: {err}", path.display());
                }
            }
        }
    }

    entries
}

pub async fn list_workflows(workspace_path: Option<String>) -> Result<Vec<WorkflowEntry>, String> {
    let mut all = Vec::new();

    let global_dir = resolve_global_workflows_dir()?;
    all.extend(read_workflows_from_dir(&global_dir, "global").await);

    if let Some(wp) = workspace_path {
        let project_dir = resolve_project_workflows_dir(&wp);
        let project_entries = read_workflows_from_dir(&project_dir, "project").await;
        // Project workflows override global workflows with the same ID
        for entry in project_entries {
            all.retain(|e: &WorkflowEntry| e.id != entry.id);
            all.push(entry);
        }
    }

    Ok(all)
}

pub async fn save_workflow(
    workflow_json: &str,
    scope: &str,
    workspace_path: Option<String>,
) -> Result<(), String> {
    let def: WorkflowDefinition = serde_json::from_str(workflow_json)
        .map_err(|err| format!("Invalid workflow JSON: {err}"))?;

    let dir = match scope {
        "global" => resolve_global_workflows_dir()?,
        "project" => {
            let wp = workspace_path.ok_or_else(|| {
                "workspace_path is required for project-scoped workflows".to_string()
            })?;
            resolve_project_workflows_dir(&wp)
        }
        other => return Err(format!("Invalid scope: {other}")),
    };

    fs::create_dir_all(&dir).await.map_err(|err| {
        format!(
            "Failed to create workflows directory {}: {err}",
            dir.display()
        )
    })?;

    let file_path = dir.join(format!("{}.json", def.id));
    let content = serde_json::to_string_pretty(&def)
        .map_err(|err| format!("Failed to serialize workflow: {err}"))?;

    fs::write(&file_path, content).await.map_err(|err| {
        format!(
            "Failed to write workflow file {}: {err}",
            file_path.display()
        )
    })?;

    Ok(())
}

pub async fn delete_workflow(
    id: &str,
    scope: &str,
    workspace_path: Option<String>,
) -> Result<(), String> {
    let dir = match scope {
        "global" => resolve_global_workflows_dir()?,
        "project" => {
            let wp = workspace_path.ok_or_else(|| {
                "workspace_path is required for project-scoped workflows".to_string()
            })?;
            resolve_project_workflows_dir(&wp)
        }
        other => return Err(format!("Invalid scope: {other}")),
    };

    let file_path = dir.join(format!("{id}.json"));
    if !file_path.exists() {
        return Err(format!("Workflow file not found: {}", file_path.display()));
    }

    fs::remove_file(&file_path).await.map_err(|err| {
        format!(
            "Failed to delete workflow file {}: {err}",
            file_path.display()
        )
    })?;

    Ok(())
}

pub async fn run_shell_command(command: &str, cwd: &str) -> Result<ShellCommandResult, String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| format!("Failed to execute command: {err}"))?;

    Ok(ShellCommandResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
        success: output.status.success(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as sync_fs;

    #[tokio::test]
    async fn test_run_shell_command_success() {
        let result = run_shell_command("echo hello", "/tmp").await.unwrap();
        assert!(result.success);
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, Some(0));
    }

    #[tokio::test]
    async fn test_run_shell_command_failure() {
        let result = run_shell_command("exit 1", "/tmp").await.unwrap();
        assert!(!result.success);
        assert_eq!(result.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_save_and_list_workflows() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let workflow_json = r#"{
            "id": "test-wf",
            "name": "Test Workflow",
            "description": "A test",
            "trigger": "/test",
            "steps": [
                {
                    "name": "Step 1",
                    "actions": [
                        { "type": "run_command", "command": "echo hi" }
                    ]
                }
            ]
        }"#;

        save_workflow(workflow_json, "project", Some(project_path.clone()))
            .await
            .unwrap();

        // Verify file exists
        let wf_path = tmp
            .path()
            .join(".tyde")
            .join("workflows")
            .join("test-wf.json");
        assert!(wf_path.exists());

        // List and verify
        let entries = list_workflows(Some(project_path.clone())).await.unwrap();
        let project_entries: Vec<_> = entries.iter().filter(|e| e.id == "test-wf").collect();
        assert_eq!(project_entries.len(), 1);
        assert_eq!(project_entries[0].name, "Test Workflow");
        assert_eq!(project_entries[0].trigger, "/test");
        assert_eq!(project_entries[0].scope, "project");
        assert_eq!(project_entries[0].steps.len(), 1);
        assert_eq!(project_entries[0].steps[0].name, "Step 1");
    }

    #[tokio::test]
    async fn test_save_and_delete_workflow() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        let workflow_json = r#"{
            "id": "del-test",
            "name": "Delete Test",
            "description": "",
            "trigger": "/del",
            "steps": []
        }"#;

        save_workflow(workflow_json, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let wf_path = tmp
            .path()
            .join(".tyde")
            .join("workflows")
            .join("del-test.json");
        assert!(wf_path.exists());

        delete_workflow("del-test", "project", Some(project_path.clone()))
            .await
            .unwrap();

        assert!(!wf_path.exists());
    }

    #[tokio::test]
    async fn test_project_overrides_global() {
        let tmp = tempfile::tempdir().unwrap();
        let project_path = tmp.path().to_string_lossy().to_string();

        // Save a global workflow
        let global_dir = resolve_global_workflows_dir().unwrap();
        sync_fs::create_dir_all(&global_dir).ok();
        let global_wf = r#"{
            "id": "override-test",
            "name": "Global Version",
            "description": "from global",
            "trigger": "/override",
            "steps": []
        }"#;
        sync_fs::write(global_dir.join("override-test.json"), global_wf).unwrap();

        // Save a project workflow with same ID
        let project_wf = r#"{
            "id": "override-test",
            "name": "Project Version",
            "description": "from project",
            "trigger": "/override",
            "steps": []
        }"#;
        save_workflow(project_wf, "project", Some(project_path.clone()))
            .await
            .unwrap();

        let entries = list_workflows(Some(project_path)).await.unwrap();
        let matching: Vec<_> = entries.iter().filter(|e| e.id == "override-test").collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].name, "Project Version");
        assert_eq!(matching[0].scope, "project");

        // Cleanup global
        sync_fs::remove_file(global_dir.join("override-test.json")).ok();
    }

    #[tokio::test]
    async fn test_reads_existing_global_workflow() {
        let global_dir = resolve_global_workflows_dir().unwrap();
        sync_fs::create_dir_all(&global_dir).ok();

        let wf_json = r#"{
            "id": "test-echo",
            "name": "Test Echo",
            "description": "Echo test workflow",
            "trigger": "/test-echo",
            "steps": [
                { "name": "Greet", "actions": [{ "type": "run_command", "command": "echo hello" }] },
                { "name": "Done", "actions": [{ "type": "run_command", "command": "echo done" }] }
            ]
        }"#;
        sync_fs::write(global_dir.join("test-echo.json"), wf_json).unwrap();

        let entries = list_workflows(None).await.unwrap();
        let echo: Vec<_> = entries.iter().filter(|e| e.id == "test-echo").collect();
        assert_eq!(echo.len(), 1);
        assert_eq!(echo[0].name, "Test Echo");
        assert_eq!(echo[0].trigger, "/test-echo");
        assert_eq!(echo[0].scope, "global");
        assert_eq!(echo[0].steps.len(), 2);

        sync_fs::remove_file(global_dir.join("test-echo.json")).ok();
    }
}
