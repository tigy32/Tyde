use std::sync::Arc;

use protocol::{
    AgentId, AgentStartPayload, AgentWorkflowMetadata, MessageOrigin, ProjectAddRootPayload,
    ProjectDeletePayload, ProjectDeleteRootPayload, ProjectId, SpawnAgentPayload,
    WorkbenchCreatePayload, WorkbenchRemovePayload,
};
use tokio::sync::{mpsc, oneshot};

use super::{BaseRevision, CreatedWorkbench, HostHandle};
use crate::error::AppResult;

pub(crate) struct LifecycleHandle {
    tx: mpsc::UnboundedSender<QueuedCommand>,
}

struct QueuedCommand {
    command: Command,
    // Cancellation of a caller must not allow a second actor for this scope.
    owner: Arc<LifecycleHandle>,
}

pub(crate) enum Command {
    Create {
        payload: WorkbenchCreatePayload,
        base: Option<BaseRevision>,
        reply: oneshot::Sender<AppResult<CreatedWorkbench>>,
    },
    Remove {
        payload: WorkbenchRemovePayload,
        reply: oneshot::Sender<AppResult<()>>,
    },
    AddRoot {
        payload: ProjectAddRootPayload,
        reply: oneshot::Sender<AppResult<()>>,
    },
    DeleteRoot {
        payload: ProjectDeleteRootPayload,
        reply: oneshot::Sender<AppResult<()>>,
    },
    DeleteProject {
        payload: ProjectDeletePayload,
        reply: oneshot::Sender<AppResult<()>>,
    },
    Move {
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        reply: oneshot::Sender<Result<AgentStartPayload, String>>,
    },
    ScheduleMove {
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        lifecycle: Arc<LifecycleHandle>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Spawn {
        payload: Box<SpawnAgentPayload>,
        caller_agent_id: Option<AgentId>,
        input_origin: MessageOrigin,
        workflow: Option<AgentWorkflowMetadata>,
        reply: oneshot::Sender<Result<AgentId, String>>,
    },
    EnterScheduledProject {
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        workspace_roots: Vec<String>,
        reply: oneshot::Sender<Result<ProjectEntryExecution, String>>,
    },
}

// The lifecycle actor resolves the authoritative roots; the agent alone can
// apply its backend/session mutation. Completion lets the next lifecycle command
// run without ever requesting work from an agent waiting for this actor.
pub(crate) struct ProjectEntryExecution {
    pub(crate) roots: Vec<String>,
    completion: Option<oneshot::Sender<ProjectEntrySettlement>>,
}

struct ProjectEntrySettlement {
    outcome: ProjectEntryOutcome,
    retired: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Debug)]
pub(crate) enum ProjectEntryOutcome {
    Committed,
    BackendBusy,
    Rejected,
    Unreconciled,
    Cancelled,
}

impl ProjectEntryExecution {
    pub(crate) fn complete(mut self, outcome: ProjectEntryOutcome) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(ProjectEntrySettlement {
                outcome,
                retired: None,
            });
        }
    }

    pub(crate) async fn retire(mut self) -> Result<(), String> {
        let completion = self
            .completion
            .take()
            .ok_or_else(|| "Project entry already settled".to_owned())?;
        let (reply, retired) = oneshot::channel();
        completion
            .send(ProjectEntrySettlement {
                outcome: ProjectEntryOutcome::Unreconciled,
                retired: Some(reply),
            })
            .map_err(|_| "Project lifecycle stopped before retiring the conversation".to_owned())?;
        retired
            .await
            .map_err(|_| "Project lifecycle retirement did not complete".to_owned())?
    }
}

impl Drop for ProjectEntryExecution {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(ProjectEntrySettlement {
                outcome: ProjectEntryOutcome::Cancelled,
                retired: None,
            });
        }
    }
}

impl LifecycleHandle {
    pub(crate) fn spawn(host: HostHandle) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<QueuedCommand>();
        let host = super::WeakHostHandle::downgrade(&host);
        tokio::spawn(async move {
            while let Some(queued) = rx.recv().await {
                let Some(host) = host.upgrade() else {
                    tracing::error!("Host stopped before project lifecycle operation");
                    break;
                };
                match queued.command {
                    Command::Create {
                        payload,
                        base,
                        reply,
                    } => {
                        let _ = reply.send(host.create_workbench_serialized(payload, base).await);
                    }
                    Command::Remove { payload, reply } => {
                        let _ = reply.send(host.remove_workbench_serialized(payload).await);
                    }
                    Command::AddRoot { payload, reply } => {
                        let _ = reply.send(host.add_project_root_serialized(payload).await);
                    }
                    Command::DeleteRoot { payload, reply } => {
                        let _ = reply.send(host.delete_project_root_serialized(payload).await);
                    }
                    Command::DeleteProject { payload, reply } => {
                        let _ = reply.send(host.delete_project_serialized(payload).await);
                    }
                    Command::Move {
                        agent_id,
                        project_id,
                        reply,
                    } => {
                        let _ = reply.send(
                            host.move_agent_to_project_serialized(&agent_id, project_id.as_ref())
                                .await,
                        );
                    }
                    Command::ScheduleMove {
                        agent_id,
                        project_id,
                        lifecycle,
                        reply,
                    } => {
                        let _ = reply.send(
                            host.schedule_agent_project_move_serialized(
                                &agent_id,
                                project_id.as_ref(),
                                lifecycle,
                            )
                            .await,
                        );
                    }
                    Command::Spawn {
                        payload,
                        caller_agent_id,
                        input_origin,
                        workflow,
                        reply,
                    } => {
                        let _ = reply.send(
                            host.spawn_agent_from_agent_control_serialized(
                                *payload,
                                caller_agent_id.as_ref(),
                                input_origin,
                                workflow,
                            )
                            .await,
                        );
                    }
                    Command::EnterScheduledProject {
                        agent_id,
                        project_id,
                        workspace_roots,
                        reply,
                    } => {
                        let store = host.state.lock().await.project_store.clone();
                        let roots = match project_id {
                            None => Ok(workspace_roots),
                            Some(id) => store.lock().await.get(&id).ok_or_else(|| "Destination project was removed before entry".to_owned())
                            .and_then(|project| {
                                if project.is_removing_workbench() {
                                    return Err("Destination workbench is being removed".to_owned());
                                }
                                if crate::workbench::bubblewrap_environment(&project).is_some() {
                                    return Err("Moving live conversations into Bubblewrap workbenches is unsupported".to_owned());
                                }
                                crate::backend::validate_local_workspace_roots(project.root_paths().into_iter().map(|root| root.0).collect())
                            }),
                        };
                        match roots {
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                            Ok(roots) => {
                                let (completion, completed) = oneshot::channel();
                                if reply
                                    .send(Ok(ProjectEntryExecution {
                                        roots,
                                        completion: Some(completion),
                                    }))
                                    .is_ok()
                                {
                                    match completed.await {
                                        Ok(settlement) => {
                                            tracing::debug!(outcome = ?settlement.outcome, "Scheduled project entry settled");
                                            let result = match settlement.outcome {
                                                ProjectEntryOutcome::Unreconciled => {
                                                    host.withdraw_failed_project_entry_restoration(
                                                        &agent_id,
                                                    )
                                                    .await
                                                }
                                                ProjectEntryOutcome::Committed
                                                | ProjectEntryOutcome::BackendBusy
                                                | ProjectEntryOutcome::Rejected
                                                | ProjectEntryOutcome::Cancelled => Ok(()),
                                            };
                                            if let Some(retired) = settlement.retired {
                                                let _ = retired.send(result);
                                            }
                                        }
                                        Err(_) => tracing::error!(
                                            "Scheduled project entry completion channel closed"
                                        ),
                                    }
                                }
                            }
                        }
                    }
                }
                drop(queued.owner);
            }
        });
        Arc::new(Self { tx })
    }

    pub(crate) async fn request<T>(
        self: &Arc<Self>,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(QueuedCommand {
                command: command(reply),
                owner: self.clone(),
            })
            .map_err(|_| "Project lifecycle actor stopped".to_owned())?;
        result
            .await
            .map_err(|_| "Project lifecycle operation stopped before completion".to_owned())
    }

    pub(crate) fn enter_scheduled_project(
        self: &Arc<Self>,
        agent_id: AgentId,
        project_id: Option<ProjectId>,
        workspace_roots: Vec<String>,
    ) -> Result<oneshot::Receiver<Result<ProjectEntryExecution, String>>, String> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(QueuedCommand {
                command: Command::EnterScheduledProject {
                    agent_id,
                    project_id,
                    workspace_roots,
                    reply,
                },
                owner: self.clone(),
            })
            .map_err(|_| "Project lifecycle actor stopped before project entry".to_owned())?;
        Ok(result)
    }
}
