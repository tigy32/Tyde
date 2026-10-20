use std::sync::Arc;

use protocol::{AgentId, ProjectId};
use tokio::sync::oneshot;

use super::{HostHandle, ProjectCatalogHandle};

// Roots and retirement are settled by owned work; the agent remains the sole
// owner of its backend mutation, without a catalog/agent request cycle.
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
            .ok_or("Project entry already settled")?;
        let (reply, retired) = oneshot::channel();
        completion
            .send(ProjectEntrySettlement {
                outcome: ProjectEntryOutcome::Unreconciled,
                retired: Some(reply),
            })
            .map_err(|_| {
                "Project entry owner stopped before retiring the conversation".to_owned()
            })?;
        retired
            .await
            .map_err(|_| "Project entry retirement did not complete".to_owned())?
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

pub(crate) struct ProjectEntryRequest {
    pub(crate) agent_id: AgentId,
    pub(crate) source: Option<ProjectId>,
    pub(crate) destination: Option<ProjectId>,
    pub(crate) workspace_roots: Vec<String>,
    pub(crate) reply: oneshot::Sender<Result<ProjectEntryExecution, String>>,
}

pub(crate) async fn run_entry(
    host: HostHandle,
    catalog: Arc<ProjectCatalogHandle>,
    request: ProjectEntryRequest,
) {
    let ProjectEntryRequest {
        agent_id,
        source,
        destination,
        workspace_roots,
        reply,
    } = request;
    let mut projects = source.clone().into_iter().collect::<Vec<_>>();
    projects.extend(destination.clone());
    let binding = match catalog.reserve_bindings(projects).await {
        Ok(binding) => binding,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    let roots = async {
        for id in source.iter().chain(destination.iter()) {
            let project = catalog.get(id).ok_or("Project no longer exists")?;
            if project.is_removing_workbench() { return Err("Workbench removal is in progress".to_owned()); }
            if crate::workbench::bubblewrap_environment(&project).is_some() {
                return Err("Moving live conversations into or out of Bubblewrap workbenches is unsupported; create a new session in the destination project".to_owned());
            }
        }
        match destination {
            None => Ok(workspace_roots),
            Some(id) => {
                let roots = catalog.get(&id).ok_or("Destination project was removed before entry")?
                    .root_paths().into_iter().map(|root| root.0).collect();
                tokio::task::spawn_blocking(move || crate::backend::validate_local_workspace_roots(roots))
                    .await.map_err(|_| "Project root validation worker stopped".to_owned())?
            }
        }
    }.await;
    let roots = match roots {
        Ok(roots) => roots,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
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
                        host.withdraw_failed_project_entry_restoration(&agent_id)
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
            Err(_) => tracing::error!("Scheduled project entry completion channel closed"),
        }
    }
    drop(binding);
}
