use super::*;
use protocol::{
    BoardActor, BoardCommandPayload, BoardErrorCode, BoardErrorPayload, BoardReadPayload,
    BoardStatePayload,
};

impl HostHandle {
    pub(crate) async fn board_lane_sections(&self) -> Result<HashMap<ProjectId, String>, String> {
        let board = self.state.with(|state| state.board.clone()).await;
        let mut sections = board.sections().await?;
        for project in self
            .project_catalog
            .list()
            .map_err(|error| error.to_string())?
        {
            if let Some(parent) = project.parent_project_id()
                && let Some(section) = sections.get(parent).cloned()
            {
                sections.insert(project.id, section);
            }
        }
        Ok(sections)
    }
    pub(crate) async fn board_caller_project(
        &self,
        caller: &AgentId,
    ) -> Result<Option<ProjectId>, String> {
        let start = self
            .agent_handle(caller)
            .await
            .ok_or("board caller is not active")?
            .snapshot();
        if start.parent_agent_id.is_some() {
            return Ok(None);
        }
        let Some(id) = start.project_id else {
            return Ok(None);
        };
        let project = self
            .project_catalog
            .get(&id)
            .ok_or("board project not found")?;
        Ok(Some(
            project.parent_project_id().cloned().unwrap_or(project.id),
        ))
    }
    pub(crate) async fn board_tool_allowed(
        &self,
        caller: &AgentId,
        tool: &str,
    ) -> Result<bool, String> {
        let Some(id) = self.board_caller_project(caller).await? else {
            return Ok(false);
        };
        let role = self.tyde_permissions_for_caller(Some(caller)).await?;
        let board = self
            .board_snapshot(id)
            .await
            .map_err(|error| error.message)?;
        Ok(match tool {
            "tyde_board_lanes" => role != protocol::AgentTydePermissions::Agent,
            "tyde_board_read" | "tyde_board_move" => board.customized,
            _ => true,
        })
    }
    pub(crate) async fn board_command(
        &self,
        mut command: BoardCommandPayload,
        caller: Option<&AgentId>,
    ) -> Result<BoardStatePayload, BoardErrorPayload> {
        let requested = crate::board::command_project(&command).clone();
        let error_project = requested.clone();
        let fail = move |code, message: String| BoardErrorPayload {
            project_id: error_project.clone(),
            code,
            message,
        };
        let project = self
            .project_catalog
            .get(&requested)
            .ok_or_else(|| fail(BoardErrorCode::NotFound, "project not found".into()))?;
        let canonical = project.parent_project_id().cloned().unwrap_or(project.id);
        match &mut command {
            BoardCommandPayload::Read(p) => p.project_id = canonical.clone(),
            BoardCommandPayload::Move(p) => p.project_id = canonical.clone(),
            BoardCommandPayload::Lanes(p) => p.project_id = canonical.clone(),
            BoardCommandPayload::Remove { project_id, .. } => *project_id = canonical.clone(),
        }
        let (board, live) = self
            .state
            .with(move |state| {
                let live = live_cards(state);
                (state.board.clone(), live)
            })
            .await;
        let live = live.map_err(|message| fail(BoardErrorCode::NotFound, message))?;
        let actor = if let Some(caller) = caller {
            let own_project = self
                .board_caller_project(caller)
                .await
                .map_err(|message| fail(BoardErrorCode::Forbidden, message))?
                .ok_or_else(|| {
                    fail(
                        BoardErrorCode::Forbidden,
                        "caller has no top-level project card".into(),
                    )
                })?;
            if &own_project != crate::board::command_project(&command) {
                return Err(fail(
                    BoardErrorCode::Forbidden,
                    "caller belongs to another board".into(),
                ));
            }
            let current = board
                .apply(
                    BoardCommandPayload::Read(BoardReadPayload {
                        project_id: own_project,
                    }),
                    live.clone(),
                    BoardActor::User,
                )
                .await?;
            let role = self
                .tyde_permissions_for_caller(Some(caller))
                .await
                .map_err(|message| fail(BoardErrorCode::Forbidden, message))?;
            if !current.customized && !matches!(command, BoardCommandPayload::Lanes(_)) {
                return Err(fail(
                    BoardErrorCode::Forbidden,
                    "board tools are not enabled until lanes are edited".into(),
                ));
            }
            if role == protocol::AgentTydePermissions::Agent {
                match &command {
                    BoardCommandPayload::Read(_) => {}
                    BoardCommandPayload::Move(p)
                        if current
                            .cards
                            .iter()
                            .any(|card| card.id == p.card_id && &card.agent_id == caller) => {}
                    _ => {
                        return Err(fail(
                            BoardErrorCode::Forbidden,
                            "only Orchestrator and Root may edit lanes or other cards".into(),
                        ));
                    }
                }
            }
            BoardActor::Agent {
                agent_id: caller.clone(),
            }
        } else {
            BoardActor::User
        };
        self.state
            .with_async(move |state| {
                Box::pin(async move {
                    let live = live_cards(state)
                        .map_err(|message| fail(BoardErrorCode::NotFound, message))?;
                    let result = board.apply(command, live, actor).await?;
                    let payload = serde_json::to_value(&result)
                        .map_err(|error| fail(BoardErrorCode::Storage, error.to_string()))?;
                    state.host_streams.retain(|path, subscriber| {
                        if state
                            .board_subscriptions
                            .get(path)
                            .is_some_and(|projects| projects.contains(&result.project_id))
                        {
                            emit_or_queue_host_frame(
                                subscriber,
                                FrameKind::BoardState,
                                payload.clone(),
                            )
                            .is_ok()
                        } else {
                            true
                        }
                    });
                    Ok(result)
                })
            })
            .await
    }
    pub(crate) async fn board_snapshot(
        &self,
        project_id: ProjectId,
    ) -> Result<BoardStatePayload, BoardErrorPayload> {
        self.state
            .with_async(move |state| {
                Box::pin(async move {
                    let live = live_cards(state).map_err(|message| BoardErrorPayload {
                        project_id: project_id.clone(),
                        code: BoardErrorCode::NotFound,
                        message,
                    })?;
                    state
                        .board
                        .apply(
                            BoardCommandPayload::Read(BoardReadPayload { project_id }),
                            live,
                            BoardActor::User,
                        )
                        .await
                })
            })
            .await
    }
    pub(crate) async fn subscribe_board(
        &self,
        stream: StreamPath,
        project_id: ProjectId,
    ) -> Result<(), String> {
        let project = self
            .project_catalog
            .get(&project_id)
            .ok_or("project not found")?;
        let id = project.parent_project_id().cloned().unwrap_or(project.id);
        self.state
            .with(move |state| {
                state
                    .board_subscriptions
                    .entry(stream)
                    .or_default()
                    .insert(id);
            })
            .await;
        Ok(())
    }
}

pub(super) fn start_live_updates(
    host: WeakHostHandle,
    mut changes: watch::Receiver<u64>,
    mut agents: watch::Receiver<crate::agent_control_mcp::AgentControlDiscoveryCatalog>,
) -> Result<(), String> {
    use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
    fn changed(
        mut start: watch::Receiver<AgentStartPayload>,
    ) -> futures_util::future::BoxFuture<'static, Option<watch::Receiver<AgentStartPayload>>> {
        async move {
            start.changed().await.ok()?;
            start.borrow_and_update();
            Some(start)
        }
        .boxed()
    }
    let worker = async move {
        let mut starts = FuturesUnordered::new();
        for entry in agents.borrow_and_update().values() {
            starts.push(changed(entry.start.clone()));
        }
        loop {
            tokio::select! {
                result = changes.changed() => { if result.is_err() { break; } }
                result = agents.changed() => {
                    if result.is_err() { break; }
                    starts = FuturesUnordered::new();
                    for entry in agents.borrow_and_update().values() { starts.push(changed(entry.start.clone())); }
                }
                result = starts.next(), if !starts.is_empty() => {
                    if let Some(Some(start)) = result { starts.push(changed(start)); }
                }
            }
            let Some(host) = host.upgrade() else {
                break;
            };
            let projects = host
                .state
                .with(|state| {
                    state
                        .board_subscriptions
                        .values()
                        .flatten()
                        .cloned()
                        .collect::<HashSet<_>>()
                })
                .await;
            for project_id in projects {
                if let Err(error) = host
                    .board_command(
                        BoardCommandPayload::Read(BoardReadPayload { project_id }),
                        None,
                    )
                    .await
                {
                    tracing::error!(code = ?error.code, "Cannot publish board state");
                    let failed_project = error.project_id.clone();
                    match serde_json::to_value(error) {
                        Ok(payload) => {
                            host.state
                                .with(move |state| {
                                    state.host_streams.retain(|path, subscriber| {
                                        if state.board_subscriptions.get_mut(path).is_some_and(
                                            |projects| projects.remove(&failed_project),
                                        ) {
                                            emit_or_queue_host_frame(
                                                subscriber,
                                                FrameKind::BoardError,
                                                payload.clone(),
                                            )
                                            .is_ok()
                                        } else {
                                            true
                                        }
                                    });
                                })
                                .await
                        }
                        Err(error) => {
                            tracing::error!(%error,"Cannot encode board subscription failure")
                        }
                    }
                }
            }
        }
    };
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(worker);
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot create board publication runtime: {error}"))?;
    std::thread::Builder::new()
        .name("tyde-board-publication".into())
        .spawn(move || runtime.block_on(worker))
        .map_err(|error| format!("cannot start board publication actor: {error}"))?;
    Ok(())
}

fn live_cards(state: &HostState) -> Result<Vec<(ProjectId, AgentStartPayload)>, String> {
    let mut cards = Vec::new();
    for start in state
        .registry
        .agent_ids()
        .into_iter()
        .filter_map(|id| state.registry.agent_handle(&id))
        .map(|handle| handle.snapshot())
        .filter(|start| start.parent_agent_id.is_none())
        .filter(|start| start.origin != protocol::AgentOrigin::SwarmMember)
    {
        let Some(id) = start.project_id.as_ref() else {
            continue;
        };
        let project = state
            .project_store
            .get(id)
            .ok_or("live board card references a missing project")?;
        cards.push((
            project.parent_project_id().cloned().unwrap_or(project.id),
            start,
        ));
    }
    Ok(cards)
}
