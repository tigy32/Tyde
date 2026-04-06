use std::fs;
use std::path::{Path, PathBuf};

use crate::backend::BackendKind;
use crate::backend_transport::BackendTransport;
use crate::remote::shell_quote_arg;

/// Result of injecting skills for a backend.
#[derive(Debug)]
pub(crate) struct SkillInjectionResult {
    /// Directory to pass to the backend via `--add-dir` / `--include-directories`.
    /// Set for backends that support additional directories.
    /// `None` for Kiro (workspace symlinks only).
    pub(crate) skill_dir: Option<String>,
    /// Cleanup handle.
    pub(crate) cleanup: SkillCleanup,
}

/// Tracks injected skill artifacts for cleanup.
/// Only Kiro workspace symlinks need cleanup — temp dirs contain harmless
/// symlinks to permanent storage (~/.tyde/skills/) and are reused across
/// conversations with the same agent.
#[derive(Debug)]
pub(crate) struct SkillCleanup {
    pub(crate) transport: BackendTransport,
    /// Symlinks created in the workspace (Kiro-style backends only).
    pub(crate) workspace_paths: Vec<String>,
}

/// Resolve the `~/.tyde/skills/` directory for the local host.
fn resolve_skills_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "HOME environment variable is not set".to_string())?;
    Ok(PathBuf::from(home).join(".tyde").join("skills"))
}

/// List available skill names from `~/.tyde/skills/`.
pub(crate) fn list_available_skills() -> Result<Vec<String>, String> {
    let skills_dir = resolve_skills_dir()?;
    if !skills_dir.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    let entries = fs::read_dir(&skills_dir)
        .map_err(|e| format!("Failed to read {}: {e}", skills_dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").exists() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Skill discovery subdir inside the temp/workspace root for each backend.
fn skill_subdir(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => ".claude/skills",
        BackendKind::Tycode => ".tycode/skills",
        BackendKind::Codex => ".agents/skills",
        BackendKind::Kiro => ".kiro/skills",
        BackendKind::Gemini => ".gemini/skills",
    }
}

/// Inject skills for a backend. Resolves skill names from `~/.tyde/skills/`.
///
/// - **Claude/Tycode**: Creates a per-agent temp directory with backend-native skill
///   subdirs (`.claude/skills/` for Claude, `.tycode/skills/` for Tycode)
///   structure and returns `skill_dir` for `--add-dir` / extra workspace root.
/// - **Codex/Kiro/Gemini**: Symlinks into workspace skill directory (e.g.
///   `.agents/skills/tyde-{name}/`). Returns `skill_dir = None`.
pub(crate) async fn inject_skills_for_backend(
    kind: BackendKind,
    transport: &BackendTransport,
    workspace_root: &str,
    agent_def_id: &str,
    skill_names: &[String],
) -> Result<SkillInjectionResult, String> {
    if skill_names.is_empty() {
        return Ok(SkillInjectionResult {
            skill_dir: None,
            cleanup: SkillCleanup {
                transport: transport.clone(),
                workspace_paths: vec![],
            },
        });
    }

    match transport {
        BackendTransport::Local => {
            inject_skills_local(kind, workspace_root, agent_def_id, skill_names)
        }
        BackendTransport::Ssh { .. } => {
            inject_skills_remote(kind, transport, workspace_root, agent_def_id, skill_names).await
        }
    }
}

// ---------------------------------------------------------------------------
// Local injection
// ---------------------------------------------------------------------------

fn inject_skills_local(
    kind: BackendKind,
    workspace_root: &str,
    agent_def_id: &str,
    skill_names: &[String],
) -> Result<SkillInjectionResult, String> {
    let skills_dir = resolve_skills_dir()?;

    // Verify all skills exist before creating anything.
    for name in skill_names {
        let skill_path = skills_dir.join(name);
        if !skill_path.is_dir() || !skill_path.join("SKILL.md").exists() {
            return Err(format!(
                "Skill '{}' not found in {}",
                name,
                skills_dir.display()
            ));
        }
    }

    // Only Claude/Tycode support --add-dir for isolated skill injection.
    // Codex, Kiro, and Gemini use workspace symlinks.
    if !matches!(kind, BackendKind::Claude | BackendKind::Tycode) {
        return inject_skills_local_workspace(kind, workspace_root, &skills_dir, skill_names);
    }

    // All other backends: per-agent temp dir.
    let temp_dir = std::env::temp_dir().join(format!("tyde-agent-skills-{agent_def_id}"));
    let subdir = skill_subdir(kind);
    let target_base = temp_dir.join(subdir);

    fs::create_dir_all(&target_base).map_err(|e| {
        format!(
            "Failed to create skill directory {}: {e}",
            target_base.display()
        )
    })?;

    for name in skill_names {
        let source = skills_dir.join(name);
        let link_path = target_base.join(name);

        // Recreate symlink if it already exists (shared across conversations).
        if link_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&link_path);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link_path).map_err(|e| {
            format!(
                "Failed to symlink skill {} -> {}: {e}",
                link_path.display(),
                source.display()
            )
        })?;
        #[cfg(not(unix))]
        copy_dir_recursive(&source, &link_path)?;
    }

    Ok(SkillInjectionResult {
        skill_dir: Some(temp_dir.to_string_lossy().to_string()),
        cleanup: SkillCleanup {
            transport: BackendTransport::Local,
            workspace_paths: vec![],
        },
    })
}

/// Codex/Kiro/Gemini: symlink into workspace skill directory.
fn inject_skills_local_workspace(
    kind: BackendKind,
    workspace_root: &str,
    skills_dir: &Path,
    skill_names: &[String],
) -> Result<SkillInjectionResult, String> {
    let subdir = skill_subdir(kind);
    let target_base = Path::new(workspace_root).join(subdir);
    fs::create_dir_all(&target_base).map_err(|e| {
        format!(
            "Failed to create skill directory {}: {e}",
            target_base.display()
        )
    })?;

    let mut workspace_paths = Vec::new();
    for name in skill_names {
        let source = skills_dir.join(name);
        let link_name = format!("tyde-{name}");
        let link_path = target_base.join(&link_name);

        if link_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&link_path);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &link_path).map_err(|e| {
            format!(
                "Failed to symlink skill {} -> {}: {e}",
                link_path.display(),
                source.display()
            )
        })?;
        #[cfg(not(unix))]
        copy_dir_recursive(&source, &link_path)?;
        workspace_paths.push(link_path.to_string_lossy().to_string());
    }

    Ok(SkillInjectionResult {
        skill_dir: None,
        cleanup: SkillCleanup {
            transport: BackendTransport::Local,
            workspace_paths,
        },
    })
}

// ---------------------------------------------------------------------------
// Remote injection (SSH)
// ---------------------------------------------------------------------------

async fn inject_skills_remote(
    kind: BackendKind,
    transport: &BackendTransport,
    workspace_root: &str,
    agent_def_id: &str,
    skill_names: &[String],
) -> Result<SkillInjectionResult, String> {
    // Resolve remote home directory for ~/.tyde/skills/ path.
    let remote_home = {
        let output = transport.run_shell_command("echo ~").await?;
        if !output.status.success() {
            return Err("Failed to resolve home directory on remote host".to_string());
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let remote_skills_dir = format!("{remote_home}/.tyde/skills");

    // Verify all skills exist on the remote host.
    for name in skill_names {
        let skill_path = format!("{remote_skills_dir}/{name}/SKILL.md");
        let check_cmd = format!("[ -f {} ]", shell_quote_arg(&skill_path));
        let output = transport.run_shell_command(&check_cmd).await?;
        if !output.status.success() {
            return Err(format!(
                "Skill '{name}' not found at {remote_skills_dir}/{name}/ on remote host"
            ));
        }
    }

    // Only Claude/Tycode support --add-dir. Others use workspace symlinks.
    if !matches!(kind, BackendKind::Claude | BackendKind::Tycode) {
        return inject_skills_remote_workspace(
            kind,
            transport,
            workspace_root,
            &remote_skills_dir,
            skill_names,
        )
        .await;
    }

    // Claude/Tycode: per-agent temp dir on remote.
    let temp_dir = format!("/tmp/tyde-agent-skills-{agent_def_id}");
    let subdir = skill_subdir(kind);
    let target_base = format!("{temp_dir}/{subdir}");

    let mkdir_cmd = format!("mkdir -p {}", shell_quote_arg(&target_base));
    let output = transport.run_shell_command(&mkdir_cmd).await?;
    if !output.status.success() {
        return Err(format!(
            "Failed to create remote skill directory: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    for name in skill_names {
        let source = format!("{remote_skills_dir}/{name}");
        let link_path = format!("{target_base}/{name}");
        let ln_cmd = format!(
            "ln -sf {} {}",
            shell_quote_arg(&source),
            shell_quote_arg(&link_path),
        );
        let ln_output = transport.run_shell_command(&ln_cmd).await?;
        if !ln_output.status.success() {
            return Err(format!(
                "Failed to symlink remote skill {link_path}: {}",
                String::from_utf8_lossy(&ln_output.stderr)
            ));
        }
    }

    Ok(SkillInjectionResult {
        skill_dir: Some(temp_dir.clone()),
        cleanup: SkillCleanup {
            transport: transport.clone(),
            workspace_paths: vec![],
        },
    })
}

async fn inject_skills_remote_workspace(
    kind: BackendKind,
    transport: &BackendTransport,
    workspace_root: &str,
    remote_skills_dir: &str,
    skill_names: &[String],
) -> Result<SkillInjectionResult, String> {
    let subdir = skill_subdir(kind);
    let target_base = format!("{workspace_root}/{subdir}");
    let mkdir_cmd = format!("mkdir -p {}", shell_quote_arg(&target_base));
    let output = transport.run_shell_command(&mkdir_cmd).await?;
    if !output.status.success() {
        return Err(format!(
            "Failed to create remote {target_base}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let mut workspace_paths = Vec::new();
    for name in skill_names {
        let source = format!("{remote_skills_dir}/{name}");
        let link_name = format!("tyde-{name}");
        let link_path = format!("{target_base}/{link_name}");
        let ln_cmd = format!(
            "ln -sf {} {}",
            shell_quote_arg(&source),
            shell_quote_arg(&link_path),
        );
        let ln_output = transport.run_shell_command(&ln_cmd).await?;
        if !ln_output.status.success() {
            return Err(format!(
                "Failed to symlink remote skill {link_path}: {}",
                String::from_utf8_lossy(&ln_output.stderr)
            ));
        }
        workspace_paths.push(link_path);
    }

    Ok(SkillInjectionResult {
        skill_dir: None,
        cleanup: SkillCleanup {
            transport: transport.clone(),
            workspace_paths,
        },
    })
}

/// Remove injected workspace symlinks (Kiro-style backends).
/// Temp dirs are NOT cleaned up — they contain harmless symlinks to permanent
/// storage and are shared across conversations with the same agent.
pub(crate) async fn cleanup_injected_skills(cleanup: SkillCleanup) {
    if cleanup.workspace_paths.is_empty() {
        return;
    }
    match &cleanup.transport {
        BackendTransport::Local => {
            for path in cleanup.workspace_paths.iter().rev() {
                let p = Path::new(path);
                if p.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(p);
                }
            }
        }
        BackendTransport::Ssh { .. } => {
            let parts: Vec<String> = cleanup
                .workspace_paths
                .iter()
                .rev()
                .map(|path| format!("rm -f {}", shell_quote_arg(path)))
                .collect();
            let cmd = parts.join("; ");
            let _ = cleanup.transport.run_shell_command(&cmd).await;
        }
    }
}

#[cfg(not(unix))]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst)
        .map_err(|e| format!("Failed to create directory {}: {e}", dst.display()))?;
    for entry in
        fs::read_dir(src).map_err(|e| format!("Failed to read directory {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("Failed to read directory entry: {e}"))?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path).map_err(|e| {
                format!(
                    "Failed to copy {} -> {}: {e}",
                    path.display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_skills_dir(tmp: &Path, names: &[&str]) -> PathBuf {
        let skills_dir = tmp.join(".tyde").join("skills");
        for name in names {
            let skill_dir = skills_dir.join(name);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\nDo {name} stuff."),
            )
            .unwrap();
            fs::write(skill_dir.join("helper.sh"), "#!/bin/sh\necho hi").unwrap();
        }
        skills_dir
    }

    #[test]
    fn test_inject_claude_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        setup_skills_dir(tmp.path(), &["my-skill"]);

        // Temporarily override HOME so resolve_skills_dir() finds our test dir.
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let result = inject_skills_local(
            BackendKind::Claude,
            "/workspace",
            "test-agent",
            &["my-skill".to_string()],
        );

        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        let result = result.unwrap();
        assert!(result.skill_dir.is_some());
        let sd = result.skill_dir.unwrap();
        assert!(sd.contains("tyde-agent-skills-test-agent"));

        // Verify skill is accessible through the temp dir.
        let link = Path::new(&sd).join(".claude/skills/my-skill");
        assert!(link.join("SKILL.md").exists());
        assert!(link.join("helper.sh").exists());

        // Cleanup.
        let _ = fs::remove_dir_all(&sd);
    }

    #[test]
    fn test_inject_tycode_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        setup_skills_dir(tmp.path(), &["my-skill"]);

        // Temporarily override HOME so resolve_skills_dir() finds our test dir.
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let result = inject_skills_local(
            BackendKind::Tycode,
            "/workspace",
            "test-agent",
            &["my-skill".to_string()],
        );

        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        let result = result.unwrap();
        assert!(result.skill_dir.is_some());
        let sd = result.skill_dir.unwrap();
        assert!(sd.contains("tyde-agent-skills-test-agent"));

        // Verify skill is accessible through the temp dir.
        let link = Path::new(&sd).join(".tycode/skills/my-skill");
        assert!(link.join("SKILL.md").exists());
        assert!(link.join("helper.sh").exists());

        // Cleanup.
        let _ = fs::remove_dir_all(&sd);
    }

    #[test]
    fn test_inject_codex_workspace_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let skills_dir = setup_skills_dir(tmp.path(), &["my-skill"]);

        let result = inject_skills_local_workspace(
            BackendKind::Codex,
            &workspace.to_string_lossy(),
            &skills_dir,
            &["my-skill".to_string()],
        )
        .unwrap();

        assert!(result.skill_dir.is_none());
        assert_eq!(result.cleanup.workspace_paths.len(), 1);

        let link = workspace.join(".agents/skills/tyde-my-skill");
        assert!(link.join("SKILL.md").exists());
    }

    #[test]
    fn test_inject_kiro_workspace_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let skills_dir = setup_skills_dir(tmp.path(), &["review"]);

        let result = inject_skills_local_workspace(
            BackendKind::Kiro,
            &workspace.to_string_lossy(),
            &skills_dir,
            &["review".to_string()],
        )
        .unwrap();

        assert!(result.skill_dir.is_none());
        assert_eq!(result.cleanup.workspace_paths.len(), 1);

        let link = workspace.join(".kiro/skills/tyde-review");
        assert!(link.join("SKILL.md").exists());
    }

    #[test]
    fn test_missing_skill_errors() {
        let tmp = tempfile::tempdir().unwrap();
        setup_skills_dir(tmp.path(), &[]);

        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let result = inject_skills_local(
            BackendKind::Claude,
            "/workspace",
            "test-agent",
            &["nonexistent".to_string()],
        );

        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_list_available_skills() {
        let tmp = tempfile::tempdir().unwrap();
        setup_skills_dir(tmp.path(), &["beta-skill", "alpha-skill"]);

        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let names = list_available_skills();

        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }

        let names = names.unwrap();
        assert_eq!(names, vec!["alpha-skill", "beta-skill"]);
    }
}
