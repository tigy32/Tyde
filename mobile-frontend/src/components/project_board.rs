use leptos::prelude::*;
use protocol::{
    BoardCard, BoardCardId, BoardCommandPayload, BoardDestination, BoardLocation, BoardMovePayload,
    BoardNavigationTarget, BoardReadPayload, BoardStatePayload,
};
use wasm_bindgen_futures::spawn_local;

use super::ui::{Button, ButtonSize, ButtonVariant, Spinner};
use crate::state::{ActiveAgentRef, ActiveProjectRef, AgentRef, AppState, LocalHostId};

fn send(
    state: AppState,
    host: LocalHostId,
    command: BoardCommandPayload,
    error: RwSignal<Option<String>>,
) {
    spawn_local(async move {
        if let Err(message) = crate::actions::board_command(&state, &host, &command).await {
            error.set(Some(message));
        }
    });
}

#[component]
pub fn ProjectBoard(target: ActiveProjectRef) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let error = RwSignal::new(None::<String>);
    let moving = RwSignal::new(None::<(BoardCardId, u64)>);
    let note = RwSignal::new(String::new());
    let sheet_open = Signal::derive(move || moving.get().is_some());
    crate::native::track_modal(sheet_open);
    let host = target.local_host_id.clone();
    let project = target.project_id.clone();
    let subscription_state = state.clone();
    let subscription_host = host.clone();
    let subscription_project = project.clone();
    let connection_state = state.clone();
    let connection_host = host.clone();
    let connection = Memo::new(move |_| {
        (
            connection_state
                .host_streams
                .with(|streams| streams.get(&connection_host).cloned()),
            connection_state
                .bootstrapped_host_streams
                .with(|streams| streams.get(&connection_host).cloned()),
        )
    });
    Effect::new(move |_| {
        let (stream, ready) = connection.get();
        if stream.is_some() && stream == ready {
            send(
                subscription_state.clone(),
                subscription_host.clone(),
                BoardCommandPayload::Read(BoardReadPayload {
                    project_id: subscription_project.clone(),
                }),
                error,
            );
        }
    });
    let board_state = state.clone();
    let board_key = (host.clone(), project.clone());
    let board = Signal::derive(move || {
        board_state
            .boards
            .with(|boards| boards.get(&board_key).cloned())
    });
    let title_state = state.clone();
    let title_target = target.clone();
    let title = move || {
        title_state.projects.with(|projects| {
            projects
                .iter()
                .find(|info| {
                    info.local_host_id == title_target.local_host_id
                        && info.project.id == title_target.project_id
                })
                .map(|info| info.project.name.clone())
                .unwrap_or_else(|| "Project unavailable".into())
        })
    };
    let back_state = state.clone();
    let new_state = state.clone();
    let new_target = target.clone();
    let new_chat = Callback::new(move |_| {
        new_state.active_project.set(Some(new_target.clone()));
        crate::actions::open_new_chat(&new_state);
    });
    let error_state = state.clone();
    let error_key = (host.clone(), project.clone());
    let server_error = Signal::derive(move || {
        error_state
            .board_errors
            .with(|errors| errors.get(&error_key).cloned())
    });
    let storage_failed = Signal::derive(move || {
        server_error
            .get()
            .is_some_and(|error| error.code == protocol::BoardErrorCode::Storage)
    });
    let error_text = move || {
        error
            .get()
            .or_else(|| server_error.get().map(|error| error.message))
    };
    let can_dismiss = move || error.get().is_some() || !storage_failed.get();
    let dismiss_state = state.clone();
    let dismiss_key = (host.clone(), project.clone());
    let dismiss = Callback::new(move |_| {
        error.set(None);
        dismiss_state.board_errors.update(|errors| {
            if errors
                .get(&dismiss_key)
                .is_some_and(|error| error.code != protocol::BoardErrorCode::Storage)
            {
                errors.remove(&dismiss_key);
            }
        });
    });
    let content_state = state.clone();
    let content_target = target.clone();
    view! {
        <div class="view mobile-project-page" data-mobile-test="project-board">
            <div class="shell-header tws-header">
                <header class="view-header">
                    <Button label="Back" variant=ButtonVariant::Ghost size=ButtonSize::Compact
                        on_click=Callback::new(move |_| back_state.board_project.set(None)) />
                    <h1 class="view-title">{title}</h1>
                    <Button label="New agent" variant=ButtonVariant::Primary size=ButtonSize::Compact on_click=new_chat />
                </header>
                <super::ConnectionBanner inline=true />
                {move || error_text().map(|message| view! {
                    <div class="board-error" role="alert"><span>{message}</span>
                        <Show when=can_dismiss><Button label="Dismiss" variant=ButtonVariant::Ghost on_click=dismiss /></Show>
                    </div>
                })}
            </div>
            <div class="view-body"><div class="shell-flow board-lanes">
                {move || match board.get() {
                    None if storage_failed.get() => ().into_any(),
                    None => view! { <Spinner aria_label="Loading board".to_owned() /> }.into_any(),
                    Some(snapshot) => {
                        snapshot.lanes.iter().map(|lane| {
                            let roots = snapshot.cards.iter().filter(|card| matches!(&card.location,
                                BoardLocation::Lane { lane_id, .. } if lane_id == &lane.id)).cloned().collect::<Vec<_>>();
                            let count = roots.len() + snapshot.cards.iter().filter(|card| matches!(&card.location,
                                BoardLocation::Parent { card_id } if roots.iter().any(|root| &root.id == card_id))).count();
                            let name = lane.name.clone();
                            view! { <section class="board-lane" aria-label=name.clone()>
                                <h2 class="board-lane-heading"><span>{name.clone()}</span><span class="board-count">{count}</span></h2>
                                {if roots.is_empty() { view! { <p class="board-empty">"No cards"</p> }.into_any() } else {
                                    roots.into_iter().map(|card| {
                                        let children = snapshot.cards.iter().filter(|child| matches!(&child.location,
                                            BoardLocation::Parent { card_id } if card_id == &card.id)).cloned().collect::<Vec<_>>();
                                        view! { <article class="board-card-group">
                                            {card_row(&content_state, &content_target, &snapshot, card, moving, note, error)}
                                            {(!children.is_empty()).then(|| view! { <div class="board-children">
                                                {children.into_iter().map(|child| card_row(&content_state, &content_target, &snapshot, child, moving, note, error)).collect_view()}
                                            </div> })}
                                        </article> }
                                    }).collect_view().into_any()
                                }}
                            </section> }
                        }).collect_view().into_any()
                    }
                }}
            </div></div>
            {move || moving.get().map(|draft| view! {
                <MoveSheet target=target.clone() board=board draft=draft moving=moving note=note error=error />
            })}
        </div>
    }
}

fn card_row(
    state: &AppState,
    target: &ActiveProjectRef,
    board: &BoardStatePayload,
    card: BoardCard,
    moving: RwSignal<Option<(BoardCardId, u64)>>,
    note: RwSignal<String>,
    error: RwSignal<Option<String>>,
) -> AnyView {
    let Some(nav) = board
        .navigation
        .iter()
        .find(|entry| entry.card_id == card.id)
        .map(|entry| entry.target.clone())
    else {
        return view! { <div class="board-error" role="alert">{format!("{}: card navigation is missing", card.title)}</div> }.into_any();
    };
    let unavailable = matches!(nav, BoardNavigationTarget::Unavailable);
    let state_for_status = state.clone();
    let host_for_status = target.local_host_id.clone();
    let agent_id = card.agent_id.clone();
    let live = matches!(nav, BoardNavigationTarget::Live { .. });
    let status = move || {
        if !live {
            return if unavailable {
                "Chat unavailable"
            } else {
                "Closed"
            };
        }
        let agent_ref = AgentRef {
            local_host_id: host_for_status.clone(),
            agent_id: agent_id.clone(),
        };
        let info = state_for_status.agents.with(|agents| {
            agents
                .iter()
                .find(|agent| agent.agent_ref() == agent_ref)
                .cloned()
        });
        match info {
            None => "Status unavailable",
            Some(agent) if agent.fatal_error.is_some() => "Failed",
            Some(agent) if !agent.started => "Starting",
            Some(_)
                if state_for_status
                    .agent_awaiting_user
                    .with(|agents| agents.contains(&agent_ref)) =>
            {
                "Needs your answer"
            }
            Some(_)
                if state_for_status
                    .agent_turn_active
                    .with(|agents| agents.get(&agent_ref).copied().unwrap_or(false)) =>
            {
                "Thinking"
            }
            Some(_)
                if state_for_status
                    .agents_with_background_work
                    .with(|agents| agents.contains(&agent_ref)) =>
            {
                "Background work"
            }
            Some(_) => "Idle",
        }
    };
    let status = Signal::derive(status);
    let row_id = card.id.0.clone();
    let open_state = state.clone();
    let open_target = target.clone();
    let open = move |_| match nav.clone() {
        BoardNavigationTarget::Live { agent_id } => open_state.open_agent_chat(ActiveAgentRef {
            local_host_id: open_target.local_host_id.clone(),
            agent_id,
        }),
        BoardNavigationTarget::Saved { session_id, .. } => {
            let state = open_state.clone();
            let target = open_target.clone();
            spawn_local(async move {
                let Some(stream) = state.host_stream_untracked(&target.local_host_id) else {
                    error.set(Some("Host is not connected".into()));
                    return;
                };
                let payload = protocol::SpawnAgentPayload {
                    launch_context: protocol::AgentLaunchContext::Standard,
                    name: None,
                    custom_agent_id: None,
                    parent_agent_id: None,
                    project_id: None,
                    params: protocol::SpawnAgentParams::Resume {
                        session_id,
                        prompt: None,
                    },
                };
                if let Err(message) = crate::actions::spawn_chat(
                    &state,
                    &target.local_host_id,
                    stream,
                    &payload,
                    crate::actions::SpawnChatNavigation::Navigate,
                    None,
                )
                .await
                {
                    error.set(Some(message.to_string()));
                }
            });
        }
        BoardNavigationTarget::Unavailable => {}
    };
    let move_label = format!("Move {}", card.title);
    view! { <div class="board-card" data-board-card=row_id>
        <button class="board-card-chat" disabled=unavailable on:click=open>
            <span class="board-card-title">{card.title}</span>
            <span class="board-card-status" data-status=move || status.get()>{move || status.get()}</span>
        </button>
        <button class="board-card-move" aria-label=move_label on:click=move |_| {
            note.set(String::new()); moving.set(Some((card.id.clone(), card.version)));
        }>"Move"</button>
    </div> }
    .into_any()
}

#[component]
fn MoveSheet(
    target: ActiveProjectRef,
    board: Signal<Option<BoardStatePayload>>,
    draft: (BoardCardId, u64),
    moving: RwSignal<Option<(BoardCardId, u64)>>,
    note: RwSignal<String>,
    error: RwSignal<Option<String>>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let (id, version) = draft;
    let title_id = id.clone();
    let title = Signal::derive(move || {
        board.get().and_then(|board| {
            board
                .cards
                .iter()
                .find(|card| card.id == title_id)
                .map(|card| format!("Move {}", card.title))
        })
    });
    let action_id = id.clone();
    let action = Callback::new(move |destination| {
        let text = note.get_untracked();
        send(
            state.clone(),
            target.local_host_id.clone(),
            BoardCommandPayload::Move(BoardMovePayload {
                project_id: target.project_id.clone(),
                card_id: action_id.clone(),
                destination,
                note: (!text.trim().is_empty()).then_some(text),
                position: None,
                expected_version: version,
            }),
            error,
        );
        moving.set(None);
    });
    let focus: NodeRef<leptos::html::Div> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(element) = focus.get() {
            let _ = element.focus();
        }
    });
    view! { <div class="project-sheet-backdrop" role="dialog" aria-modal="true" aria-label=move || title.get()
        tabindex="-1" node_ref=focus on:keydown=move |event| { if event.key() == "Escape" { moving.set(None); } }
        on:click=move |_| moving.set(None)>
        <div class="project-sheet board-move-sheet" on:click=|event| event.stop_propagation()>
            <div class="board-header-actions"><h2>{move || title.get()}</h2>
                <Button label="Cancel" variant=ButtonVariant::Ghost on_click=Callback::new(move |_| moving.set(None)) />
            </div>
            <label class="board-note">"Note"<textarea aria-label="Note" rows="2" prop:value=move || note.get()
                on:input=move |event| note.set(event_target_value(&event)) /></label>
            <div class="project-sheet-list">
                {move || match board.get() {
                    Some(board) => move_options(&board, &id, action),
                    None => view! { <p role="alert">"Board is unavailable"</p> }.into_any(),
                }}
            </div>
        </div>
    </div> }
}

fn move_options(
    board: &BoardStatePayload,
    id: &BoardCardId,
    action: Callback<BoardDestination>,
) -> AnyView {
    let Some(card) = board.cards.iter().find(|card| &card.id == id) else {
        return view! { <p role="alert">"Card is no longer on this board"</p> }.into_any();
    };
    let can_nest = !board.cards.iter().any(
        |child| matches!(&child.location, BoardLocation::Parent { card_id } if card_id == &card.id),
    );
    let (current_lane, current_parent) = match &card.location {
        BoardLocation::Lane { lane_id, .. } => (Some(lane_id), None),
        BoardLocation::Parent { card_id } => (None, Some(card_id)),
    };
    let unnest_lane = match &card.location {
        BoardLocation::Lane { .. } => None,
        BoardLocation::Parent { card_id } => board
            .cards
            .iter()
            .find(|parent| &parent.id == card_id)
            .and_then(|parent| match &parent.location {
                BoardLocation::Lane { lane_id, .. } => board
                    .lanes
                    .iter()
                    .find(|lane| &lane.id == lane_id)
                    .map(|lane| lane.name.clone()),
                BoardLocation::Parent { .. } => None,
            }),
    };
    view! {
        {unnest_lane.map(|name| view! { <button class="project-sheet-row" on:click=move |_| action.run(BoardDestination::Lane { name: name.clone() })>"Un-nest"</button> })}
        <h3>"Lanes"</h3>
        {board.lanes.iter().filter(|lane| Some(&lane.id) != current_lane).map(|lane| { let name = lane.name.clone(); view! {
            <button class="project-sheet-row" on:click=move |_| action.run(BoardDestination::Lane { name: name.clone() })>{lane.name.clone()}</button>
        }}).collect_view()}
        {can_nest.then(|| view! { <h3>"Nest in"</h3>
            {board.cards.iter().filter(|parent| parent.id != card.id && Some(&parent.id) != current_parent && matches!(parent.location, BoardLocation::Lane { .. }))
                .map(|parent| { let id = parent.id.clone(); view! { <button class="project-sheet-row" on:click=move |_| action.run(BoardDestination::Parent { card_id: id.clone() })>{parent.title.clone()}</button> }}).collect_view()}
        })}
    }.into_any()
}
