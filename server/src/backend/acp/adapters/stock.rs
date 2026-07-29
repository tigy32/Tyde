//! Specification-only ACP agent.
//!
//! Everything here follows from the Agent Client Protocol alone: spawn the
//! configured command, run it in the workspace root, and rely on the trait
//! defaults for capabilities, notifications, stream text, and tool mapping.
//! This is the adapter an unknown agent gets, and the one a newly added agent
//! should need.
//!
//! Deliberately absent: session enumeration and deletion. The generic backend
//! uses the protocol's own `session/list` when the agent advertises it; this
//! adapter does not go looking for session files on disk, because where an
//! arbitrary agent stores its sessions is not knowable.

use std::collections::HashMap;

use futures_util::future::BoxFuture;
use protocol::{AcpAdapterId, AcpAgentSpec};

use crate::backend::acp::AcpSpawnSpec;
use crate::backend::acp::adapter::{AcpAgentAdapter, AcpSessionKind, AcpSessionRoots};

pub struct StockAdapter {
    spec: AcpAgentSpec,
    display_name: String,
}

impl StockAdapter {
    pub fn new(spec: AcpAgentSpec) -> Self {
        // The command's file stem is the most recognizable name we have for an
        // agent Tyde knows nothing else about.
        let display_name = std::path::Path::new(&spec.command)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "ACP agent".to_string());
        Self { spec, display_name }
    }
}

/// Choose the directory an ACP agent runs in.
///
/// Prefers the first local root. An `ssh://` root is not a local path, so a
/// workspace that has only remote roots is an explicit error rather than a
/// silent fallback to some other directory.
pub(crate) fn pick_local_workspace_root(
    workspace_roots: &[String],
    agent: &str,
) -> Result<String, String> {
    if let Some(root) = workspace_roots
        .iter()
        .find(|root| !root.trim().is_empty() && !root.trim_start().starts_with("ssh://"))
        .cloned()
    {
        return Ok(root);
    }
    if workspace_roots
        .iter()
        .any(|root| !root.trim().is_empty() && root.trim_start().starts_with("ssh://"))
    {
        return Err(format!(
            "{agent} requires at least one local workspace root"
        ));
    }
    crate::backend::tyde_owned_no_root_cwd(agent)
}

impl AcpAgentAdapter for StockAdapter {
    fn id(&self) -> AcpAdapterId {
        AcpAdapterId::Stock
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn resolve_roots<'a>(
        &'a self,
        workspace_roots: &'a [String],
        ssh_host: Option<&'a str>,
        _kind: AcpSessionKind,
    ) -> BoxFuture<'a, Result<AcpSessionRoots, String>> {
        Box::pin(async move {
            let scope_root = if ssh_host.is_some() {
                crate::remote::parse_remote_workspace_roots(workspace_roots)?
                    .ok_or("Expected remote workspace roots for SSH session")?
                    .1
                    .into_iter()
                    .next()
                    .ok_or("No remote workspace root found")?
            } else {
                pick_local_workspace_root(workspace_roots, &self.display_name)?
            };

            // An explicit `cwd` on the spec wins; otherwise run in the
            // workspace root itself.
            let session_cwd = self
                .spec
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| scope_root.clone());

            Ok(AcpSessionRoots {
                session_cwd,
                scope_root,
            })
        })
    }

    fn spawn_spec(
        &self,
        roots: &AcpSessionRoots,
        ssh_host: Option<&str>,
    ) -> Result<AcpSpawnSpec, String> {
        let command = self.spec.command.trim();
        if command.is_empty() {
            return Err(format!(
                "{} has no command configured; set one in its launch profile",
                self.display_name
            ));
        }

        let args: Vec<&str> = self.spec.args.iter().map(String::as_str).collect();
        let mut spawn = AcpSpawnSpec::new(self.display_name.clone(), command, &args)
            .with_local_cwd(roots.session_cwd.clone());
        if ssh_host.is_some() {
            spawn = spawn.with_remote_cwd(roots.session_cwd.clone());
        }
        Ok(spawn)
    }

    fn extra_env(&self) -> HashMap<String, String> {
        self.spec
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(command: &str) -> AcpAgentSpec {
        AcpAgentSpec {
            command: command.to_string(),
            args: vec!["acp".to_string()],
            cwd: None,
            env: BTreeMap::new(),
            adapter: AcpAdapterId::Stock,
        }
    }

    #[test]
    fn display_name_comes_from_the_command_stem() {
        assert_eq!(
            StockAdapter::new(spec("/opt/bin/gemini-cli")).display_name(),
            "gemini-cli"
        );
    }

    #[test]
    fn empty_command_is_a_named_error_not_a_spawn_attempt() {
        let adapter = StockAdapter::new(spec("   "));
        let roots = AcpSessionRoots {
            session_cwd: "/ws".to_string(),
            scope_root: "/ws".to_string(),
        };
        let err = adapter.spawn_spec(&roots, None).expect_err("empty command");
        assert!(
            err.contains("no command configured"),
            "expected a configuration error, got: {err}"
        );
    }

    #[test]
    fn remote_only_workspace_is_rejected_rather_than_silently_relocated() {
        let err = pick_local_workspace_root(&["ssh://host/srv/app".to_string()], "test-agent")
            .expect_err("remote-only roots");
        assert!(err.contains("local workspace root"), "got: {err}");
    }

    #[tokio::test]
    async fn explicit_cwd_overrides_the_workspace_root() {
        let mut s = spec("agent");
        s.cwd = Some("/scratch".to_string());
        let adapter = StockAdapter::new(s);
        let roots = adapter
            .resolve_roots(&["/ws".to_string()], None, AcpSessionKind::default())
            .await
            .expect("roots");
        assert_eq!(roots.session_cwd, "/scratch");
        assert_eq!(roots.scope_root, "/ws", "scope root stays the workspace");
    }
}
