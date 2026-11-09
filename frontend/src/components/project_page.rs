//! A project's page: its agents by lane, and everything else about the project.
//!
//! Every value is a projection of existing server-owned state. Cards and rows
//! are keyed by id and look their record up reactively.

use std::collections::HashMap;
use std::time::Duration;

use leptos::prelude::*;
use leptos::tachys::dom::event_target_value;
use protocol::{
    AgentId, BackendKind, BoardCardId, BoardCommandPayload, BoardDestination, BoardLane,
    BoardLaneId, BoardLanesPayload, BoardLocation, BoardMovePayload, BoardNavigationTarget,
    BoardReadPayload, BoardStatePayload, FrameKind, ProjectId, ProjectRootPath, ProjectSource,
    SessionId,
};
use wasm_bindgen_futures::spawn_local;

use crate::components::agents_panel::{
    DerivedAgentState, derive_agent_state_with_background, open_agent_chat, status_label,
};
use crate::components::card_menu::{
    CardContextMenu, CardMenuPosition, open_card_menu, open_card_menu_below,
};
use crate::components::file_explorer::confirm_and_remove_root;
use crate::components::header::report_user_error;
use crate::components::host_browser::open_add_root_browser_for;
use crate::components::project_rail::{
    WorkbenchCreateModal, WorkbenchCreatePrompt, failed_workbench_removal, request_delete_project,
    request_remove_workbench,
};
use crate::components::review_view::{open_comments_for_project, pick_workspace_draft};
use crate::components::settings_panel::steering_in_project;
use crate::components::swarms_panel::project_swarm_count;
use crate::components::workflows_panel::effective_summaries;
use crate::sidebar_layout::SidebarTab;
use crate::state::{
    ActiveProjectRef, AppState, BoardLanesSave, ProjectInfo, TabContent, root_display_name,
};

const THINK: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.2l2 1.3"/>"#;
const AWAIT: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="M6.4 6.5a1.6 1.6 0 1 1 2.4 1.4c-.5.3-.8.6-.8 1.1"/><path d="M8 11h.01"/>"#;
const IDLE: &str = r#"<path d="m3.5 8.5 3 3 6-6.5"/>"#;
const BACKGROUND: &str =
    r#"<path d="M4.5 2.5h7M4.5 13.5h7M5 2.5v2.2L8 8l3-3.3V2.5M5 13.5v-2.2L8 8l3 3.3v2.2"/>"#;
const COMPACT: &str = r#"<path d="M3 5.5V2.8M3 5.5h2.7"/><path d="M3.3 5.3A5.5 5.5 0 1 1 2.5 8"/>"#;
const FAIL: &str = r#"<circle cx="8" cy="8" r="5.5"/><path d="m6 6 4 4M10 6l-4 4"/>"#;
const PLUS: &str = r#"<path d="M8 3.5v9M3.5 8h9"/>"#;
const MORE: &str = r#"<path d="M3.5 8h.01M8 8h.01M12.5 8h.01" stroke-width="2"/>"#;
const CLOSE: &str = r#"<path d="m4.5 4.5 7 7M11.5 4.5l-7 7"/>"#;
const OPEN: &str = r#"<path d="M6 3.5H3.5v9h9V10M9 3.5h3.5V7M12.5 3.5 7.5 8.5"/>"#;
const GRIP: &str =
    r#"<path d="M6 3.5h.01M10 3.5h.01M6 8h.01M10 8h.01M6 12.5h.01M10 12.5h.01" stroke-width="2"/>"#;

fn glyph(paths: &'static str) -> impl IntoView {
    view! { <svg class="pp-ic" viewBox="0 0 16 16" aria-hidden="true" inner_html=paths></svg> }
}

/// Open `project`'s page. A first visit's untouched starter chat gives way to
/// the page instead of sitting beside it.
pub(crate) fn open_project_page(state: &AppState, project: ActiveProjectRef) {
    let first_visit = state.active_project.get_untracked().as_ref() != Some(&project)
        && !state
            .project_view_memory
            .with_untracked(|memory| memory.contains_key(&project));
    let Some(label) = state.projects.with_untracked(|projects| {
        projects
            .iter()
            .find(|info| info.host_id == project.host_id && info.project.id == project.project_id)
            .map(|info| info.project.name.clone())
    }) else {
        log::error!(
            "open_project_page: project {} is not in the catalog",
            project.project_id.0
        );
        return;
    };
    state.switch_active_project(Some(project.clone()));
    let starter = state.center_zone.with_untracked(|center_zone| {
        first_visit
            .then(|| center_zone.active_tab())
            .flatten()
            .filter(|tab| tab.content == TabContent::empty_chat())
            .map(|tab| tab.id)
    });
    state.open_tab(
        TabContent::Project {
            host_id: project.host_id,
            project_id: project.project_id,
        },
        label,
        true,
    );
    if let Some(starter) = starter {
        state.close_tab(starter);
    }
}

/// The clearable mark of a project filter on a surface the page opened.
#[component]
pub(crate) fn ProjectFilterChip(name: String, on_clear: Callback<()>) -> impl IntoView {
    view! {
        <button class="pp-filter-chip" aria-label="Clear project filter" on:click=move |_| on_clear.run(())>
            <span class="pp-filter-chip-name">{name}</span>
            {glyph(CLOSE)}
        </button>
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DropTarget {
    Lane(BoardLaneId),
    Card(BoardCardId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CardTarget {
    Live(AgentId),
    Saved {
        session_id: SessionId,
        backend_kind: BackendKind,
        project_id: Option<ProjectId>,
    },
    Unavailable,
}

/// A project's server-owned board, plus the one piece of local state a drag
/// needs: `dataTransfer` is unreadable until the drop, so dragover cannot tell
/// what is being carried without it.
#[derive(Clone, Copy)]
struct BoardView {
    state: StoredValue<AppState>,
    boards: RwSignal<HashMap<(String, ProjectId), BoardStatePayload>>,
    host_id: StoredValue<String>,
    project_id: Memo<ProjectId>,
    drag: RwSignal<Option<BoardCardId>>,
    drop_target: RwSignal<Option<DropTarget>>,
    now_ms: ReadSignal<u64>,
}

impl BoardView {
    fn read<T>(self, read: impl FnOnce(&BoardStatePayload) -> T) -> Option<T> {
        let key = (self.host_id.get_value(), self.project_id.get());
        self.boards.with(|boards| boards.get(&key).map(read))
    }

    fn read_untracked<T>(self, read: impl FnOnce(&BoardStatePayload) -> T) -> Option<T> {
        let key = (self.host_id.get_value(), self.project_id.get_untracked());
        self.boards
            .with_untracked(|boards| boards.get(&key).map(read))
    }

    fn card_version(self, card_id: &BoardCardId) -> Option<u64> {
        self.read_untracked(|board| {
            board
                .cards
                .iter()
                .find(|card| card.id == *card_id)
                .map(|card| card.version)
        })
        .flatten()
    }

    fn send(self, payload: BoardCommandPayload) {
        let state = self.state.get_value();
        let host_id = self.host_id.get_value();
        spawn_local(async move {
            if let Err(error) = send_board_command(&state, &host_id, &payload).await {
                report_user_error(format!("Board update failed: {error}"));
            }
        });
    }

    fn move_card(self, card_id: &BoardCardId, destination: BoardDestination) {
        let Some(expected_version) = self.card_version(card_id) else {
            return;
        };
        self.send(BoardCommandPayload::Move(BoardMovePayload {
            project_id: self.project_id.get_untracked(),
            card_id: card_id.clone(),
            destination,
            note: None,
            position: None,
            expected_version,
        }));
    }

    fn remove_card(self, card_id: &BoardCardId) {
        let Some(expected_version) = self.card_version(card_id) else {
            return;
        };
        self.send(BoardCommandPayload::Remove {
            project_id: self.project_id.get_untracked(),
            card_id: card_id.clone(),
            expected_version,
        });
    }

    fn set_drop_target(self, target: Option<DropTarget>) {
        if self
            .drop_target
            .with_untracked(|current| *current != target)
        {
            self.drop_target.set(target);
        }
    }

    fn end_drag(self) {
        self.drag.set(None);
        self.set_drop_target(None);
    }
}

async fn send_board_command(
    state: &AppState,
    host_id: &str,
    payload: &BoardCommandPayload,
) -> Result<(), String> {
    let stream = state
        .host_stream_untracked(host_id)
        .ok_or("host is not connected")?;
    crate::send::send_frame(host_id, stream, FrameKind::BoardCommand, payload).await
}

fn allow_move_drop(ev: &web_sys::DragEvent) {
    ev.prevent_default();
    ev.stop_propagation();
    if let Some(transfer) = ev.data_transfer() {
        transfer.set_drop_effect("move");
    }
}

fn location_label(board: &BoardStatePayload, location: &BoardLocation) -> String {
    match location {
        BoardLocation::Lane { lane_id, .. } => board
            .lanes
            .iter()
            .find(|lane| lane.id == *lane_id)
            .map_or_else(|| "Removed lane".to_owned(), |lane| lane.name.clone()),
        BoardLocation::Parent { card_id } => board
            .cards
            .iter()
            .find(|card| card.id == *card_id)
            .map_or_else(|| "Removed card".to_owned(), |card| card.title.clone()),
    }
}

/// One archived card's move, resolved to display text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HistoryRow {
    seq: u64,
    place: String,
    age: String,
    note: Option<String>,
}

fn state_glyph(state: DerivedAgentState) -> &'static str {
    match state {
        DerivedAgentState::Initializing
        | DerivedAgentState::Thinking
        | DerivedAgentState::Cancelling => THINK,
        DerivedAgentState::AwaitingUser => AWAIT,
        DerivedAgentState::CompactionQueued | DerivedAgentState::Compacting => COMPACT,
        DerivedAgentState::Idle => IDLE,
        DerivedAgentState::BackgroundWork => BACKGROUND,
        DerivedAgentState::Cancelled => CLOSE,
        DerivedAgentState::Terminated => FAIL,
    }
}

fn state_class(state: DerivedAgentState) -> &'static str {
    match state {
        DerivedAgentState::Thinking
        | DerivedAgentState::CompactionQueued
        | DerivedAgentState::Compacting => "pp-state pp-state-active",
        DerivedAgentState::AwaitingUser => "pp-state pp-state-await",
        DerivedAgentState::Terminated => "pp-state pp-state-fail",
        DerivedAgentState::Initializing
        | DerivedAgentState::Cancelling
        | DerivedAgentState::Idle
        | DerivedAgentState::BackgroundWork
        | DerivedAgentState::Cancelled => "pp-state",
    }
}

fn compact_age(created_at_ms: u64, now_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(created_at_ms) / 60_000;
    match minutes {
        0 => "now".to_owned(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

fn workbench_branch(
    projects: &[ProjectInfo],
    host_id: &str,
    project_id: &ProjectId,
) -> Option<String> {
    projects.iter().find_map(|info| {
        if info.host_id != host_id || &info.project.id != project_id {
            return None;
        }
        match &info.project.source {
            ProjectSource::GitWorkbench { branch, .. } => Some(branch.0.clone()),
            ProjectSource::Standalone { .. } => None,
        }
    })
}

#[component]
fn CardShell(
    board: BoardView,
    card_id: BoardCardId,
    nested: bool,
    #[prop(optional)] children: Option<ChildrenFn>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let card_id = StoredValue::new(card_id);
    let card = Memo::new(move |_| {
        card_id.with_value(|id| {
            board
                .read(|payload| payload.cards.iter().find(|card| card.id == *id).cloned())
                .flatten()
        })
    });
    let target = Memo::new(move |_| {
        card_id.with_value(|id| {
            board
                .read(|payload| {
                    payload
                        .navigation
                        .iter()
                        .find(|entry| entry.card_id == *id)
                        .map(|entry| match &entry.target {
                            BoardNavigationTarget::Live { agent_id } => {
                                CardTarget::Live(agent_id.clone())
                            }
                            BoardNavigationTarget::Saved {
                                session_id,
                                backend_kind,
                                project_id,
                            } => CardTarget::Saved {
                                session_id: session_id.clone(),
                                backend_kind: *backend_kind,
                                project_id: project_id.clone(),
                            },
                            BoardNavigationTarget::Unavailable => CardTarget::Unavailable,
                        })
                })
                .flatten()
        })
    });
    let agent = {
        let agents = state.agents;
        Memo::new(move |_| {
            let Some(CardTarget::Live(agent_id)) = target.get() else {
                return None;
            };
            let host_id = board.host_id.get_value();
            agents.with(|agents| {
                agents
                    .iter()
                    .find(|agent| agent.host_id == host_id && agent.agent_id == agent_id)
                    .cloned()
            })
        })
    };
    let derived = {
        let state = state.clone();
        Memo::new(move |_| {
            agent
                .get()
                .map(|agent| derive_agent_state_with_background(&state, &agent))
        })
    };
    let branch = {
        let projects = state.projects;
        Memo::new(move |_| {
            let agent = agent.get()?;
            let project_id = agent.project_id?;
            projects.with(|projects| workbench_branch(projects, &agent.host_id, &project_id))
        })
    };
    let title = Memo::new(move |_| card.with(|card| card.as_ref().map(|card| card.title.clone())));
    let archived = Memo::new(move |_| target.get() == Some(CardTarget::Unavailable));
    let age = Memo::new(move |_| {
        let now = board.now_ms.get();
        match agent.get() {
            Some(agent) => Some(compact_age(agent.created_at_ms, now)),
            None => card.with(|card| {
                let moved = card.as_ref()?.history.last()?.at;
                Some(compact_age(moved, now))
            }),
        }
    });
    let note = Memo::new(move |_| {
        if archived.get() {
            return None;
        }
        card.with(|card| card.as_ref()?.history.last()?.note.clone())
    });
    let history = Memo::new(move |_| {
        if !archived.get() {
            return Vec::new();
        }
        let now = board.now_ms.get();
        card_id.with_value(|id| {
            board
                .read(|payload| {
                    payload
                        .cards
                        .iter()
                        .find(|card| card.id == *id)
                        .map_or_else(Vec::new, |card| {
                            card.history
                                .iter()
                                .rev()
                                .map(|entry| HistoryRow {
                                    seq: entry.seq,
                                    place: location_label(payload, &entry.to),
                                    age: compact_age(entry.at, now),
                                    note: entry.note.clone(),
                                })
                                .collect::<Vec<_>>()
                        })
                })
                .unwrap_or_default()
        })
    });
    let lane_choices = Memo::new(move |_| {
        let current = card.with(|card| match card.as_ref().map(|card| &card.location) {
            Some(BoardLocation::Lane { lane_id, .. }) => Some(lane_id.clone()),
            _ => None,
        });
        board
            .read(|payload| {
                payload
                    .lanes
                    .iter()
                    .filter(|lane| Some(&lane.id) != current.as_ref())
                    .map(|lane| lane.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let removable = Memo::new(move |_| {
        matches!(
            target.get(),
            Some(CardTarget::Saved { .. } | CardTarget::Unavailable)
        )
    });

    let open = {
        let state = state.clone();
        move || match target.get_untracked() {
            Some(CardTarget::Live(_)) => {
                if let Some(agent) = agent.get_untracked() {
                    open_agent_chat(&state, &agent);
                }
            }
            Some(CardTarget::Saved {
                session_id,
                backend_kind,
                project_id,
            }) => crate::actions::resume_session(
                &state,
                board.host_id.get_value(),
                backend_kind,
                session_id,
                project_id,
            ),
            Some(CardTarget::Unavailable) | None => {}
        }
    };
    let on_click = {
        let open = open.clone();
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            open();
        }
    };
    let on_title_key = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            ev.stop_propagation();
            open();
        }
    };

    let on_dragstart = move |ev: web_sys::DragEvent| {
        ev.stop_propagation();
        let id = card_id.get_value();
        if let Some(transfer) = ev.data_transfer() {
            transfer.set_effect_allowed("move");
            let _ = transfer.set_data("text/plain", &id.0);
        }
        board.drag.set(Some(id));
    };
    let can_nest_here = move || {
        let dragged = board.drag.get_untracked()?;
        let here = card_id.get_value();
        let is_child = board
            .read_untracked(|payload| {
                payload.cards.iter().any(|card| {
                    card.id == dragged
                        && matches!(&card.location, BoardLocation::Parent { card_id } if *card_id == here)
                })
            })
            .unwrap_or(false);
        Some((dragged != here && !is_child, here))
    };
    // A nested card has no drop behaviour of its own: the event bubbles to the
    // parent it sits in, which is where a card dropped on it would nest.
    let on_dragover = move |ev: web_sys::DragEvent| {
        if nested {
            return;
        }
        let Some((allowed, here)) = can_nest_here() else {
            return;
        };
        if allowed {
            allow_move_drop(&ev);
            board.set_drop_target(Some(DropTarget::Card(here)));
        } else {
            ev.stop_propagation();
            board.set_drop_target(None);
        }
    };
    let on_drop = move |ev: web_sys::DragEvent| {
        if nested {
            return;
        }
        let Some((allowed, here)) = can_nest_here() else {
            return;
        };
        ev.prevent_default();
        ev.stop_propagation();
        if allowed && let Some(dragged) = board.drag.get_untracked() {
            board.move_card(&dragged, BoardDestination::Parent { card_id: here });
        }
        board.end_drag();
    };

    let menu: CardMenuPosition = RwSignal::new(None);
    let anchor = NodeRef::<leptos::html::Div>::new();
    let is_dragging =
        move || card_id.with_value(|id| board.drag.with(|drag| drag.as_ref() == Some(id)));
    let is_drop_target = move || {
        card_id.with_value(|id| {
            board
                .drop_target
                .with(|target| matches!(target, Some(DropTarget::Card(over)) if over == id))
        })
    };

    view! {
        <div class="pp-card-slot" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
            <div
                class="pp-card"
                class:pp-card-nested=nested
                class:pp-card-archived=move || archived.get()
                class:pp-card-drop=is_drop_target
                class:pp-card-dragging=is_dragging
                draggable="true"
                tabindex="-1"
                node_ref=anchor
                on:click=on_click
                on:contextmenu=move |ev| open_card_menu(menu, &ev)
                on:dragstart=on_dragstart
                on:dragend=move |_| board.end_drag()
                on:dragenter=on_dragover
                on:dragover=on_dragover
                on:drop=on_drop
            >
                <span
                    class="pp-card-title"
                    role=move || (!archived.get()).then_some("button")
                    tabindex=move || (!archived.get()).then_some("0")
                    title=move || title.get()
                    on:keydown=on_title_key
                >
                    {move || title.get()}
                </span>
                <span class="pp-card-meta">
                    {move || match target.get() {
                        Some(CardTarget::Live(_)) => derived.get().map(|derived| view! {
                            <span class=state_class(derived)>
                                {glyph(state_glyph(derived))}
                                {status_label(&derived)}
                            </span>
                        }.into_any()),
                        Some(CardTarget::Saved { .. }) => Some(view! { <span class="pp-state">"Closed"</span> }.into_any()),
                        Some(CardTarget::Unavailable) => Some(view! { <span class="pp-state">"Archived"</span> }.into_any()),
                        None => None,
                    }}
                    <span>{move || age.get()}</span>
                </span>
                {move || note.get().map(|note| view! {
                    <span class="pp-note" title=note.clone()>{note.clone()}</span>
                })}
                {move || branch.get().map(|branch| view! {
                    <span class="pp-branch" title=branch.clone()>{branch.clone()}</span>
                })}
                <Show when=move || history.with(|rows| !rows.is_empty())>
                    <ol class="pp-history" aria-label="Move history">
                        <For
                            each=move || history.get()
                            key=|row| row.clone()
                            let:row
                        >
                            <li>
                                <span class="pp-history-row">
                                    <span class="pp-history-place" title=row.place.clone()>{row.place.clone()}</span>
                                    <span>{row.age.clone()}</span>
                                </span>
                                {row.note.clone().map(|note| view! { <span class="pp-note">{note}</span> })}
                            </li>
                        </For>
                    </ol>
                </Show>
                {children.as_ref().map(|children| children())}
            </div>
            <button
                type="button"
                class="pp-card-more"
                aria-label="Card actions"
                title="Card actions"
                on:click=move |ev| open_card_menu_below(menu, &ev)
            >
                {glyph(MORE)}
            </button>
            <CardContextMenu menu=menu label="Card actions" anchor=anchor>
                <For
                    each=move || lane_choices.get()
                    key=|name| name.clone()
                    let:name
                >
                    {
                        let label = format!("Move to {name}");
                        view! {
                            <button
                                type="button"
                                class="context-menu-item"
                                role="menuitem"
                                on:click=move |_| {
                                    menu.set(None);
                                    board.move_card(
                                        &card_id.get_value(),
                                        BoardDestination::Lane { name: name.clone() },
                                    );
                                }
                            >
                                {label}
                            </button>
                        }
                    }
                </For>
                <Show when=move || removable.get()>
                    <button
                        type="button"
                        class="context-menu-item"
                        role="menuitem"
                        on:click=move |_| {
                            menu.set(None);
                            board.remove_card(&card_id.get_value());
                        }
                    >
                        "Remove"
                    </button>
                </Show>
            </CardContextMenu>
        </div>
    }
}

#[component]
fn TopCard(board: BoardView, card_id: BoardCardId) -> impl IntoView {
    let children_of = card_id.clone();
    let child_ids = Memo::new(move |_| {
        board
            .read(|payload| {
                payload
                    .cards
                    .iter()
                    .filter(|card| {
                        matches!(&card.location, BoardLocation::Parent { card_id } if *card_id == children_of)
                    })
                    .map(|card| card.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    view! {
        <CardShell board=board card_id=card_id nested=false>
            <Show when=move || child_ids.with(|ids| !ids.is_empty())>
                <div class="pp-subcards">
                    <For
                        each=move || child_ids.get()
                        key=|id| id.clone()
                        let:child
                    >
                        <CardShell board=board card_id=child nested=true />
                    </For>
                </div>
            </Show>
        </CardShell>
    }
}

#[component]
fn LaneColumn(board: BoardView, lane_id: BoardLaneId) -> impl IntoView {
    let lane_id = StoredValue::new(lane_id);
    let name = Memo::new(move |_| {
        lane_id.with_value(|id| {
            board
                .read(|payload| {
                    payload
                        .lanes
                        .iter()
                        .find(|lane| lane.id == *id)
                        .map(|lane| lane.name.clone())
                })
                .flatten()
                .unwrap_or_default()
        })
    });
    let cards = Memo::new(move |_| {
        lane_id.with_value(|id| {
            board
                .read(|payload| {
                    payload
                        .cards
                        .iter()
                        .filter(|card| {
                            matches!(&card.location, BoardLocation::Lane { lane_id, .. } if lane_id == id)
                        })
                        .map(|card| card.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let count = Memo::new(move |_| {
        cards.with(|tops| {
            let nested = board
                .read(|payload| {
                    payload
                        .cards
                        .iter()
                        .filter(|card| {
                            matches!(&card.location, BoardLocation::Parent { card_id } if tops.contains(card_id))
                        })
                        .count()
                })
                .unwrap_or(0);
            tops.len() + nested
        })
    });
    let accepts_drag = move || {
        let Some(dragged) = board.drag.get_untracked() else {
            return false;
        };
        lane_id.with_value(|id| {
            board
                .read_untracked(|payload| {
                    payload.cards.iter().any(|card| {
                        card.id == dragged
                            && !matches!(&card.location, BoardLocation::Lane { lane_id, .. } if lane_id == id)
                    })
                })
                .unwrap_or(false)
        })
    };
    let on_dragover = move |ev: web_sys::DragEvent| {
        if accepts_drag() {
            allow_move_drop(&ev);
            board.set_drop_target(Some(DropTarget::Lane(lane_id.get_value())));
        }
    };
    let on_drop = move |ev: web_sys::DragEvent| {
        if !accepts_drag() {
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        if let Some(dragged) = board.drag.get_untracked() {
            board.move_card(
                &dragged,
                BoardDestination::Lane {
                    name: name.get_untracked(),
                },
            );
        }
        board.end_drag();
    };
    let is_drop_target = move || {
        lane_id.with_value(|id| {
            board
                .drop_target
                .with(|target| matches!(target, Some(DropTarget::Lane(over)) if over == id))
        })
    };
    view! {
        <section
            class="pp-lane"
            class:pp-lane-drop=is_drop_target
            aria-label=move || name.get()
            on:dragenter=on_dragover
            on:dragover=on_dragover
            on:drop=on_drop
        >
            <div class="pp-lane-head">
                <span class="pp-lane-name" title=move || name.get()>{move || name.get()}</span>
                <span class="pp-count">{move || count.get()}</span>
            </div>
            <For
                each=move || cards.get()
                key=|id| id.clone()
                let:card_id
            >
                <TopCard board=board card_id=card_id />
            </For>
        </section>
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LaneDraft {
    id: StoredValue<BoardLaneId>,
    name: RwSignal<String>,
    instructions: RwSignal<String>,
}

impl LaneDraft {
    fn new(id: BoardLaneId, name: String, instructions: String) -> Self {
        Self {
            id: StoredValue::new(id),
            name: RwSignal::new(name),
            instructions: RwSignal::new(instructions),
        }
    }
}

fn new_lane_id() -> BoardLaneId {
    BoardLaneId(format!(
        "lane-{:x}-{:x}",
        js_sys::Date::now() as u64,
        (js_sys::Math::random() * u32::MAX as f64) as u32
    ))
}

fn fit_to_content(area: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(area);
    let _ = style.set_property("height", "auto");
    let _ = style.set_property("height", &format!("{}px", area.scroll_height()));
}

fn move_draft(drafts: RwSignal<Vec<LaneDraft>>, from: usize, to: usize) {
    drafts.update(|rows| {
        let row = rows.remove(from);
        rows.insert(to, row);
    });
}

fn draft_index(drafts: RwSignal<Vec<LaneDraft>>, id: &BoardLaneId) -> Option<usize> {
    drafts.with_untracked(|rows| {
        rows.iter()
            .position(|row| row.id.with_value(|row_id| row_id == id))
    })
}

#[component]
fn LaneDraftRow(
    draft: LaneDraft,
    drafts: RwSignal<Vec<LaneDraft>>,
    dragging: RwSignal<Option<BoardLaneId>>,
    focus_name: bool,
) -> impl IntoView {
    let id = draft.id;
    let row_ref = NodeRef::<leptos::html::Div>::new();
    let handle_ref = NodeRef::<leptos::html::Span>::new();
    let name_ref = NodeRef::<leptos::html::Input>::new();
    let area_ref = NodeRef::<leptos::html::Textarea>::new();
    Effect::new(move |_| {
        if focus_name && let Some(input) = name_ref.get() {
            let _ = input.focus();
        }
    });
    Effect::new(move |_| {
        if let Some(area) = area_ref.get() {
            fit_to_content(&area);
        }
    });
    let on_dragstart = move |ev: web_sys::DragEvent| {
        ev.stop_propagation();
        if let Some(transfer) = ev.data_transfer() {
            transfer.set_effect_allowed("move");
            let _ = transfer.set_data("text/plain", &id.with_value(|id| id.0.clone()));
            if let Some(row) = row_ref.get_untracked() {
                transfer.set_drag_image(&row, 10, 16);
            }
        }
        dragging.set(Some(id.get_value()));
    };
    // Rows differ in height, so a swap has to wait for the pointer to pass the
    // row's midpoint or it would immediately swap back.
    let on_dragover = move |ev: web_sys::DragEvent| {
        let Some(dragged) = dragging.get_untracked() else {
            return;
        };
        ev.prevent_default();
        let (Some(from), Some(to)) = (
            draft_index(drafts, &dragged),
            id.with_value(|id| draft_index(drafts, id)),
        ) else {
            return;
        };
        if from == to {
            return;
        }
        let Some(row) = row_ref.get_untracked() else {
            return;
        };
        let rect = row.get_bounding_client_rect();
        let middle = rect.top() + rect.height() / 2.0;
        let y = f64::from(ev.client_y());
        if (from < to && y > middle) || (from > to && y < middle) {
            move_draft(drafts, from, to);
        }
    };
    let on_handle_key = move |ev: web_sys::KeyboardEvent| {
        let step: isize = match ev.key().as_str() {
            "ArrowUp" => -1,
            "ArrowDown" => 1,
            _ => return,
        };
        ev.prevent_default();
        let Some(from) = id.with_value(|id| draft_index(drafts, id)) else {
            return;
        };
        let len = drafts.with_untracked(Vec::len);
        let Some(to) = from.checked_add_signed(step).filter(|to| *to < len) else {
            return;
        };
        move_draft(drafts, from, to);
        // Reordering re-inserts DOM nodes, which drops focus from the one moved.
        set_timeout(
            move || {
                if let Some(handle) = handle_ref.get_untracked() {
                    let _ = handle.focus();
                }
            },
            Duration::ZERO,
        );
    };
    let on_remove = move |_| {
        drafts.update(|rows| rows.retain(|row| row.id != id));
    };
    view! {
        <div class="pp-lane-edit" node_ref=row_ref on:dragenter=on_dragover on:dragover=on_dragover on:drop=move |ev: web_sys::DragEvent| {
            ev.prevent_default();
            dragging.set(None);
        }>
            <span
                class="pp-lane-handle"
                role="button"
                tabindex="0"
                aria-label="Reorder lane"
                title="Reorder lane"
                draggable="true"
                node_ref=handle_ref
                on:dragstart=on_dragstart
                on:dragend=move |_| dragging.set(None)
                on:keydown=on_handle_key
            >
                {glyph(GRIP)}
            </span>
            <input
                class="pp-field pp-lane-name-field"
                aria-label="Lane name"
                spellcheck="false"
                autocomplete="off"
                node_ref=name_ref
                prop:value=move || draft.name.get()
                on:input=move |ev| draft.name.set(event_target_value(&ev))
            />
            <button type="button" class="pp-lane-remove" aria-label="Remove lane" title="Remove lane" on:click=on_remove>
                {glyph(CLOSE)}
            </button>
            <textarea
                class="pp-field pp-lane-instructions"
                aria-label="Lane instructions"
                placeholder="Instructions"
                rows="1"
                node_ref=area_ref
                prop:value=move || draft.instructions.get()
                on:input=move |ev: web_sys::Event| {
                    draft.instructions.set(event_target_value(&ev));
                    if let Some(area) = area_ref.get_untracked() {
                        fit_to_content(&area);
                    }
                }
            ></textarea>
        </div>
    }
}

#[component]
fn LanesModal(board: BoardView, on_close: Callback<()>) -> impl IntoView {
    let state = board.state.get_value();
    let save_signal = state.board_lanes_save;
    let (initial, expected_version) = board
        .read_untracked(|payload| (payload.lanes.clone(), payload.version))
        .unwrap_or_default();
    let drafts = RwSignal::new(
        initial
            .iter()
            .map(|lane| {
                LaneDraft::new(
                    lane.id.clone(),
                    lane.name.clone(),
                    lane.instructions.clone().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>(),
    );
    let dragging = RwSignal::new(None::<BoardLaneId>);
    let focus_new = RwSignal::new(None::<BoardLaneId>);
    let dialog_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if let Some(dialog) = dialog_ref.get() {
            let _ = dialog.focus();
        }
    });
    Effect::new(move |submitted: Option<bool>| {
        let pending = save_signal.with(Option::is_some);
        if submitted == Some(true) && !pending {
            on_close.run(());
        }
        pending
    });
    on_cleanup(move || save_signal.set(None));

    let saving = Memo::new(move |_| {
        save_signal.with(|save| save.as_ref().is_some_and(|save| save.error.is_none()))
    });
    let error = Memo::new(move |_| {
        save_signal.with(|save| save.as_ref().and_then(|save| save.error.clone()))
    });
    let cancel = move || {
        save_signal.set(None);
        on_close.run(());
    };
    let on_add = move |_| {
        let id = new_lane_id();
        focus_new.set(Some(id.clone()));
        drafts.update(|rows| rows.push(LaneDraft::new(id, String::new(), String::new())));
    };
    let on_save = move |_| {
        let lanes = drafts.with_untracked(|rows| {
            rows.iter()
                .enumerate()
                .map(|(position, row)| BoardLane {
                    id: row.id.get_value(),
                    name: row.name.get_untracked().trim().to_owned(),
                    position: position as u64,
                    instructions: Some(row.instructions.get_untracked())
                        .filter(|text| !text.trim().is_empty()),
                })
                .collect::<Vec<_>>()
        });
        if board
            .read_untracked(|payload| payload.lanes == lanes)
            .unwrap_or(false)
        {
            cancel();
            return;
        }
        let host_id = board.host_id.get_value();
        let project_id = board.project_id.get_untracked();
        save_signal.set(Some(BoardLanesSave {
            host_id: host_id.clone(),
            project_id: project_id.clone(),
            expected_version,
            lanes: lanes.clone(),
            error: None,
        }));
        let state = board.state.get_value();
        spawn_local(async move {
            let payload = BoardCommandPayload::Lanes(BoardLanesPayload {
                project_id,
                lanes,
                expected_version,
            });
            if let Err(error) = send_board_command(&state, &host_id, &payload).await {
                state.board_lanes_save.update(|save| {
                    if let Some(save) = save.as_mut() {
                        save.error = Some(error);
                    }
                });
            }
        });
    };
    view! {
        <div
            class="pp-scrim"
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "Escape" {
                    ev.stop_propagation();
                    cancel();
                }
            }
        >
            <div class="pp-dialog" role="dialog" aria-modal="true" aria-label="Lanes" tabindex="-1" node_ref=dialog_ref>
                <div class="pp-dialog-title">"Lanes"</div>
                <div class="pp-lane-list">
                    <For
                        each=move || drafts.get()
                        key=|draft| draft.id.get_value()
                        let:draft
                    >
                        {
                            let focus_name = focus_new.with_untracked(|id| id.as_ref() == Some(&draft.id.get_value()));
                            view! { <LaneDraftRow draft=draft drafts=drafts dragging=dragging focus_name=focus_name /> }
                        }
                    </For>
                    <button type="button" class="pp-lane-add" on:click=on_add>
                        {glyph(PLUS)}
                        "Add lane"
                    </button>
                </div>
                {move || error.get().map(|error| view! {
                    <div class="pp-dialog-error" role="alert">{error}</div>
                })}
                <div class="pp-dialog-foot">
                    <button type="button" class="pp-btn pp-btn-plain" on:click=move |_| cancel()>"Cancel"</button>
                    <button type="button" class="pp-btn pp-btn-primary" disabled=move || saving.get() on:click=on_save>"Save"</button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn KvRow(
    #[prop(into)] label: String,
    #[prop(into)] value: Signal<String>,
    on_open: Callback<()>,
) -> impl IntoView {
    view! {
        <button class="pp-kv" on:click=move |_| on_open.run(())>
            <span>{label}</span>
            <span class="pp-kv-value">{move || value.get()}</span>
        </button>
    }
}

fn root_meta(git: &protocol::ProjectRootGitStatus) -> Option<(String, Option<String>)> {
    if git.branch.is_none() && git.head_oid.is_none() {
        return None;
    }
    let branch = git
        .branch
        .as_ref()
        .map_or_else(String::new, |branch| format!("{branch} · "));
    Some(if git.clean {
        (format!("{branch}clean"), None)
    } else {
        (branch, Some(format!("{} changed", git.files.len())))
    })
}

#[component]
fn RootRow(
    project: ActiveProjectRef,
    root: ProjectRootPath,
    can_remove: Signal<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let meta = {
        let git_status = state.git_status;
        let project_id = project.project_id.clone();
        let root = root.clone();
        Memo::new(move |_| {
            git_status.with(|map| {
                map.get(&project_id)?
                    .iter()
                    .find(|status| status.root == root)
                    .and_then(root_meta)
            })
        })
    };
    let name = root_display_name(&root);
    let on_remove = {
        let root = root.clone();
        move |_| {
            confirm_and_remove_root(
                state.clone(),
                project.host_id.clone(),
                project.project_id.clone(),
                root.clone(),
            );
        }
    };
    view! {
        <div class="pp-row" class:pp-row-swap=move || can_remove.get()>
            <span class="pp-row-main" title=root.0.clone()>{name}</span>
            {move || meta.get().map(|(text, dirty)| view! {
                <span class="pp-row-meta">
                    {text}
                    {dirty.map(|dirty| view! { <span class="pp-dirty">{dirty}</span> })}
                </span>
            })}
            <Show when=move || can_remove.get()>
                <button class="pp-row-act" aria-label="Remove root" title="Remove root" on:click=on_remove.clone()>
                    {glyph(CLOSE)}
                </button>
            </Show>
        </div>
    }
}

#[component]
fn WorkbenchRow(host_id: String, workbench_id: ProjectId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let branch = {
        let projects = state.projects;
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            projects.with(|projects| workbench_branch(projects, &host_id, &workbench_id))
        })
    };
    let has_agents = {
        let agents = state.agents;
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            agents.with(|agents| {
                agents.iter().any(|agent| {
                    agent.host_id == host_id && agent.project_id.as_ref() == Some(&workbench_id)
                })
            })
        })
    };
    let remove_disabled = {
        let state = state.clone();
        let host_id = host_id.clone();
        let workbench_id = workbench_id.clone();
        Memo::new(move |_| {
            crate::notices::project_mutation_pending(&state, &host_id, &workbench_id)
                && failed_workbench_removal(&state, &host_id, &workbench_id).is_none()
        })
    };
    let on_open = {
        let state = state.clone();
        let project = ActiveProjectRef {
            host_id: host_id.clone(),
            project_id: workbench_id.clone(),
        };
        move |_| state.switch_active_project(Some(project.clone()))
    };
    let on_remove = move |_| {
        request_remove_workbench(state.clone(), host_id.clone(), workbench_id.clone());
    };
    view! {
        <div class="pp-row pp-row-swap">
            <span class="pp-row-main pp-branch-primary" title=move || branch.get()>{move || branch.get()}</span>
            <Show when=move || !has_agents.get()>
                <span class="pp-row-meta">"No agents"</span>
            </Show>
            <button class="pp-row-act" aria-label="Open workbench" title="Open workbench" on:click=on_open>
                {glyph(OPEN)}
            </button>
            <button class="pp-row-act" aria-label="Remove workbench" title="Remove workbench" disabled=move || remove_disabled.get() on:click=on_remove>
                {glyph(CLOSE)}
            </button>
        </div>
    }
}

#[component]
pub fn ProjectPage(host_id: String, project_id: ProjectId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let target = ActiveProjectRef {
        host_id: host_id.clone(),
        project_id: project_id.clone(),
    };
    let target = StoredValue::new(target);
    let state_sv = StoredValue::new(state.clone());

    let project = {
        let projects = state.projects;
        Memo::new(move |_| {
            target.with_value(|target| {
                projects.with(|projects| {
                    projects
                        .iter()
                        .find(|info| {
                            info.host_id == target.host_id && info.project.id == target.project_id
                        })
                        .map(|info| info.project.clone())
                })
            })
        })
    };
    let name = Memo::new(move |_| {
        project
            .get()
            .map(|project| project.name)
            .unwrap_or_default()
    });

    // Display-only: ages advance with the wall clock, not with server events.
    let (now_ms, set_now_ms) = signal(js_sys::Date::now() as u64);
    let ticker = set_interval_with_handle(
        move || set_now_ms.set(js_sys::Date::now() as u64),
        Duration::from_secs(30),
    );
    on_cleanup(move || {
        if let Ok(handle) = ticker {
            handle.clear();
        }
    });

    let board_project = Memo::new(move |_| {
        project
            .with(|project| {
                project
                    .as_ref()
                    .and_then(|project| project.parent_project_id().cloned())
            })
            .unwrap_or_else(|| target.with_value(|target| target.project_id.clone()))
    });
    let board = BoardView {
        state: state_sv,
        boards: state.boards,
        host_id: StoredValue::new(host_id.clone()),
        project_id: board_project,
        drag: RwSignal::new(None),
        drop_target: RwSignal::new(None),
        now_ms,
    };
    // A subscription lives on one connection, so a new host stream needs a new
    // Read; it also delivers the current board.
    let host_stream = {
        let host_streams = state.host_streams;
        let host_id = host_id.clone();
        Memo::new(move |_| host_streams.with(|streams| streams.get(&host_id).cloned()))
    };
    Effect::new(move |_| {
        if host_stream.get().is_some() {
            board.send(BoardCommandPayload::Read(BoardReadPayload {
                project_id: target.with_value(|target| target.project_id.clone()),
            }));
        }
    });
    let lane_ids = Memo::new(move |_| {
        board
            .read(|payload| {
                payload
                    .lanes
                    .iter()
                    .map(|lane| lane.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let board_loaded = Memo::new(move |_| board.read(|_| ()).is_some());
    let lanes_open = RwSignal::new(false);
    let lanes_button = NodeRef::<leptos::html::Button>::new();
    let close_lanes = Callback::new(move |()| {
        lanes_open.set(false);
        if let Some(button) = lanes_button.get_untracked() {
            let _ = button.focus();
        }
    });

    let review_count = {
        let review_summaries = state.review_summaries;
        Signal::derive(move || {
            let open = target.with_value(|target| {
                review_summaries.with(|map| {
                    map.get(&target.project_id)
                        .and_then(|summaries| pick_workspace_draft(summaries))
                        .map_or(0, |draft| {
                            draft.user_comment_count + draft.pending_suggestion_count
                        })
                })
            });
            format!("{open} open")
        })
    };
    let steering_count = {
        let steering = state.steering;
        Signal::derive(move || {
            target
                .with_value(|target| {
                    steering.with(|map| {
                        map.get(&target.host_id).map_or(0, |items| {
                            items
                                .values()
                                .filter(|item| steering_in_project(item, &target.project_id))
                                .count()
                        })
                    })
                })
                .to_string()
        })
    };
    let workflow_count = {
        let workflow_summaries = state.workflow_summaries;
        Signal::derive(move || {
            target
                .with_value(|target| {
                    workflow_summaries.with(|map| {
                        map.get(&target.host_id).map_or(0, |summaries| {
                            effective_summaries(summaries, Some(&target.project_id)).len()
                        })
                    })
                })
                .to_string()
        })
    };
    let swarm_count = Signal::derive(move || {
        state_sv
            .with_value(|state| target.with_value(|target| project_swarm_count(state, target)))
            .to_string()
    });

    let roots = Memo::new(move |_| {
        project
            .get()
            .map(|project| project.root_paths())
            .unwrap_or_default()
    });
    let can_manage_roots = {
        let state = state.clone();
        Memo::new(move |_| {
            target.with_value(|target| {
                state.can_manage_project_roots(&target.host_id, &target.project_id)
            })
        })
    };
    let workbenches = {
        let projects = state.projects;
        Memo::new(move |_| {
            target.with_value(|target| {
                projects.with(|projects| {
                    projects
                        .iter()
                        .filter(|info| {
                            info.host_id == target.host_id
                                && info.project.parent_project_id() == Some(&target.project_id)
                        })
                        .map(|info| info.project.id.clone())
                        .collect::<Vec<_>>()
                })
            })
        })
    };

    let busy = {
        let state = state.clone();
        Memo::new(move |_| {
            target.with_value(|target| {
                crate::notices::project_mutation_pending(
                    &state,
                    &target.host_id,
                    &target.project_id,
                )
            })
        })
    };
    let has_workbenches = Memo::new(move |_| workbenches.with(|ids| !ids.is_empty()));

    let menu: CardMenuPosition = RwSignal::new(None);
    let menu_anchor = NodeRef::<leptos::html::Div>::new();
    let renaming = RwSignal::new(false);
    let rename_input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if renaming.get()
            && let Some(input) = rename_input.get()
        {
            let _ = input.focus();
            input.select();
        }
    });
    let commit_rename = move |value: String| {
        if !renaming.get_untracked() {
            return;
        }
        renaming.set(false);
        if value.trim().is_empty() || value.trim() == name.get_untracked() {
            return;
        }
        state_sv.with_value(|state| {
            target.with_value(|target| {
                crate::actions::rename_project(
                    state,
                    target.host_id.clone(),
                    target.project_id.clone(),
                    value,
                )
            })
        });
    };

    let new_agent = move |_| {
        let (state, target) = (state_sv.get_value(), target.get_value());
        spawn_local(async move {
            if let Err(error) = crate::actions::begin_new_chat_default_at(
                &state,
                target.host_id.clone(),
                Some(target),
            )
            .await
            {
                log::error!("failed to start a new agent from the project page: {error}");
            }
        });
    };

    let workbench_prompt = RwSignal::new(None::<WorkbenchCreatePrompt>);
    let on_new_workbench = move |_| {
        target.with_value(|target| {
            workbench_prompt.set(Some(WorkbenchCreatePrompt {
                host_id: target.host_id.clone(),
                parent_project_id: target.project_id.clone(),
                parent_name: name.get_untracked(),
            }))
        });
    };
    let on_add_root = move |_| {
        state_sv.with_value(|state| {
            target.with_value(|target| open_add_root_browser_for(state, target.clone()))
        });
    };

    let open_review = Callback::new(move |_| {
        state_sv.with_value(|state| {
            target.with_value(|target| {
                open_comments_for_project(state, &target.host_id, &target.project_id)
            })
        });
    });
    let open_steering = Callback::new(move |_| {
        let (state, target) = (state_sv.get_value(), target.get_value());
        spawn_local(async move {
            if let Err(error) = crate::actions::select_new_chat_host(&state, &target.host_id).await
            {
                report_user_error(format!("Could not switch host: {error}"));
                return;
            }
            state.steering_project_filter.set(Some(target));
            state.settings_tab_request.set(Some("Steering"));
            state.settings_open.set(true);
        });
    });
    let open_workflows = Callback::new(move |_| {
        state_sv.with_value(|state| state.activate_sidebar_tab(SidebarTab::Workflows));
    });
    let open_swarms = Callback::new(move |_| {
        state_sv.with_value(|state| {
            state.swarm_project_filter.set(Some(target.get_value()));
            state.activate_sidebar_tab(SidebarTab::Swarms);
        });
    });

    view! {
        <div class="pp-page">
            <header class="pp-head">
                {move || if renaming.get() {
                    view! {
                        <input
                            class="pp-title-input"
                            aria-label="Project name"
                            node_ref=rename_input
                            prop:value=name.get_untracked()
                            on:keydown=move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
                                "Enter" => commit_rename(event_target_value(&ev)),
                                "Escape" => renaming.set(false),
                                _ => {}
                            }
                            on:blur=move |ev| commit_rename(event_target_value(&ev))
                        />
                    }.into_any()
                } else {
                    view! { <h1 class="pp-title" title=move || name.get()>{move || name.get()}</h1> }.into_any()
                }}
                <div class="pp-actions">
                    <button
                        class="pp-btn pp-btn-plain"
                        node_ref=lanes_button
                        disabled=move || !board_loaded.get()
                        on:click=move |_| lanes_open.set(true)
                    >
                        "Lanes"
                    </button>
                    <div node_ref=menu_anchor>
                        <button
                            class="pp-btn pp-btn-icon"
                            aria-label="Project menu"
                            on:click=move |ev| open_card_menu_below(menu, &ev)
                        >
                            {glyph(MORE)}
                        </button>
                    </div>
                    <button class="pp-btn pp-btn-primary" on:click=new_agent>
                        {glyph(PLUS)}
                        "New agent"
                    </button>
                </div>
            </header>
            <CardContextMenu menu=menu label="Project actions" anchor=menu_anchor>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    disabled=move || busy.get()
                    on:click=move |_| {
                        menu.set(None);
                        renaming.set(true);
                    }
                >
                    "Rename"
                </button>
                <button
                    type="button"
                    class="context-menu-item"
                    role="menuitem"
                    disabled=move || has_workbenches.get() || busy.get()
                    title=move || if has_workbenches.get() { "Remove its workbenches first" } else { "" }
                    on:click=move |_| {
                        menu.set(None);
                        target.with_value(|target| {
                            request_delete_project(
                                state_sv.get_value(),
                                target.host_id.clone(),
                                target.project_id.clone(),
                            )
                        });
                    }
                >
                    "Delete Project"
                </button>
            </CardContextMenu>
            <div class="pp-body">
                <div class="pp-board" on:dragover=move |_| board.set_drop_target(None)>
                    <div class="pp-lanes">
                        <For
                            each=move || lane_ids.get()
                            key=|id| id.clone()
                            let:lane_id
                        >
                            <LaneColumn board=board lane_id=lane_id />
                        </For>
                    </div>
                </div>
                <aside class="pp-details">
                    <KvRow label="Review" value=review_count on_open=open_review />
                    <div class="pp-details-scroll">
                        <section>
                            <div class="pp-sec-head">
                                "Roots"
                                <span class="pp-count">{move || roots.with(Vec::len)}</span>
                                <Show when=move || can_manage_roots.get()>
                                    <button class="pp-sec-add" aria-label="Add root" title="Add root" on:click=on_add_root>
                                        {glyph(PLUS)}
                                    </button>
                                </Show>
                            </div>
                            <div class="pp-list">
                                <For
                                    each=move || roots.get()
                                    key=|root| root.0.clone()
                                    let:root
                                >
                                    <RootRow
                                        project=target.get_value()
                                        root=root
                                        can_remove=Signal::derive(move || can_manage_roots.get() && roots.with(|roots| roots.len() > 1))
                                    />
                                </For>
                            </div>
                        </section>
                        <section>
                            <div class="pp-sec-head">
                                "Workbenches"
                                <span class="pp-count">{move || workbenches.with(Vec::len)}</span>
                                <button
                                    class="pp-sec-add"
                                    aria-label="New workbench"
                                    title="New workbench"
                                    disabled=move || busy.get()
                                    on:click=on_new_workbench
                                >
                                    {glyph(PLUS)}
                                </button>
                            </div>
                            <div class="pp-list">
                                <For
                                    each=move || workbenches.get()
                                    key=|id| id.clone()
                                    let:workbench_id
                                >
                                    <WorkbenchRow host_id=target.with_value(|target| target.host_id.clone()) workbench_id=workbench_id />
                                </For>
                            </div>
                        </section>
                    </div>
                    <div class="pp-details-foot">
                        <KvRow label="Steering" value=steering_count on_open=open_steering />
                        <KvRow label="Workflows" value=workflow_count on_open=open_workflows />
                        <KvRow label="Swarms" value=swarm_count on_open=open_swarms />
                    </div>
                </aside>
            </div>
            <Show when=move || lanes_open.get()>
                <LanesModal board=board on_close=close_lanes />
            </Show>
            {move || workbench_prompt.get().map(|prompt| view! {
                <WorkbenchCreateModal prompt=prompt workbench_prompt=workbench_prompt />
            })}
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::components::header::{Header, reset_user_notice_for_tests};
    use crate::components::settings_panel::SteeringTab;
    use crate::components::swarms_panel::SwarmsPanel;
    use crate::state::{ActiveAgentRef, AgentInfo};
    use crate::wasm_test_support::mount_to;
    use protocol::{
        AgentOrigin, BackendKind, BoardActor, BoardCard, BoardCardNavigation, BoardErrorCode,
        BoardErrorPayload, BoardMove, GitBranchName, Project, ProjectGitChangeKind,
        ProjectGitFileStatus, ProjectRootGitStatus, Steering, SteeringId, SteeringScope,
        StreamPath, SwarmConstraints, SwarmDraft, SwarmDraftGeneration, SwarmDraftId,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "project-page-host";

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("project-page-test-css")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("project-page-test-css");
            style.set_text_content(Some(include_str!("../../styles.css")));
            document.head().unwrap().append_child(&style).unwrap();
        }
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container
            .set_attribute("style", "display:flex;width:1200px;height:680px;")
            .unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn agent(id: &str, name: &str, project: &str, created_at_ms: u64) -> AgentInfo {
        AgentInfo {
            tyde_permissions: Default::default(),
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: Some(ProjectId(project.to_owned())),
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms,
            instance_stream: StreamPath(format!("/agent/{id}/inst")),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
            review_recipient: false,
        }
    }

    fn workbench(id: &str, branch: &str) -> ProjectInfo {
        ProjectInfo {
            host_id: HOST.to_owned(),
            project: Project {
                id: ProjectId(id.to_owned()),
                name: branch.to_owned(),
                sort_order: 0,
                source: ProjectSource::GitWorkbench {
                    parent_project_id: ProjectId("page".to_owned()),
                    branch: GitBranchName(branch.to_owned()),
                    roots: Vec::new(),
                    environment: Default::default(),
                },
            },
        }
    }

    fn root_status(root: &str, files: usize) -> ProjectRootGitStatus {
        ProjectRootGitStatus {
            root: ProjectRootPath(root.to_owned()),
            branch: Some("main".to_owned()),
            head_oid: None,
            empty_tree_oid: None,
            ahead: 0,
            behind: 0,
            clean: files == 0,
            files: (0..files)
                .map(|index| ProjectGitFileStatus {
                    relative_path: format!("src/{index}.rs"),
                    staged: None,
                    unstaged: Some(ProjectGitChangeKind::Modified),
                    untracked: false,
                })
                .collect(),
            recent_commits: Vec::new(),
            history_has_more: false,
        }
    }

    fn seed() -> AppState {
        let state = AppState::new();
        crate::dispatch::prime_host_for_tests(&state, HOST);
        state.host_streams.update(|streams| {
            streams.insert(HOST.to_owned(), StreamPath(format!("/host/{HOST}")));
        });
        state.selected_host_id.set(Some(HOST.to_owned()));
        state.projects.set(vec![
            ProjectInfo {
                host_id: HOST.to_owned(),
                project: Project {
                    id: ProjectId("page".to_owned()),
                    name: "Tyde".to_owned(),
                    sort_order: 0,
                    source: ProjectSource::Standalone {
                        roots: vec![
                            ProjectRootPath("/work/tyde".to_owned()),
                            ProjectRootPath("/work/docs".to_owned()),
                        ],
                    },
                },
            },
            workbench("wb-busy", "feat/busy"),
            workbench("wb-empty", "feat/empty"),
        ]);
        state.agents.set(vec![
            agent("older", "Older card", "page", 10),
            agent("newer", "Workbench card", "wb-busy", 20),
            agent("child", "Nested card", "page", 25),
            agent("elsewhere", "Other project card", "other", 30),
        ]);
        state.agent_turn_active.update(|turns| {
            turns.insert(AgentId("newer".to_owned()), true);
        });
        state.git_status.set(HashMap::from([(
            ProjectId("page".to_owned()),
            vec![root_status("/work/tyde", 3), root_status("/work/docs", 0)],
        )]));
        state.steering.update(|steering| {
            let items = [
                (
                    "Project rule",
                    SteeringScope::Project(ProjectId("page".to_owned())),
                ),
                ("Host rule", SteeringScope::Host),
            ]
            .into_iter()
            .map(|(id, scope)| {
                let id = SteeringId(id.to_owned());
                let item = Steering {
                    id: id.clone(),
                    scope,
                    title: id.0.clone(),
                    content: id.0.clone(),
                };
                (id, item)
            })
            .collect();
            steering.insert(HOST.to_owned(), items);
        });
        state.active_project.set(Some(ActiveProjectRef {
            host_id: HOST.to_owned(),
            project_id: ProjectId("page".to_owned()),
        }));
        state.swarm_drafts.update(|drafts| {
            let items = [
                ("Page crew", "page"),
                ("Workbench crew", "wb-busy"),
                ("Other crew", "other"),
            ]
            .into_iter()
            .map(|(name, project)| {
                let id = SwarmDraftId(name.to_owned());
                let draft = SwarmDraft {
                    legacy_source: None,
                    retained_sessions: Default::default(),
                    id: id.clone(),
                    revision: 1,
                    name: name.to_owned(),
                    constraints: SwarmConstraints {
                        project_id: ProjectId(project.to_owned()),
                        workspace_policy: Default::default(),
                        max_live_agents: 1,
                        allocations: Vec::new(),
                        shared_guidance: String::new(),
                        agent_wake_budget: None,
                    },
                    members: Vec::new(),
                    conflicts: Vec::new(),
                    generation: SwarmDraftGeneration::DeterministicGeneralists,
                    legacy_team_id: None,
                };
                (id, draft)
            })
            .collect();
            drafts.insert(HOST.to_owned(), items);
        });
        state
    }

    fn elements(container: &HtmlElement, selector: &str) -> Vec<Element> {
        let nodes = container.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .filter_map(|index| nodes.item(index)?.dyn_into::<Element>().ok())
            .collect()
    }

    fn text(element: &Element) -> String {
        element.text_content().unwrap_or_default()
    }

    fn row_with(container: &HtmlElement, label: &str) -> Element {
        elements(container, "aside button, aside section > div > div")
            .into_iter()
            .find(|row| text(row).starts_with(label))
            .unwrap_or_else(|| panic!("no details row starts with {label}"))
    }

    fn click(element: &Element) {
        element.dyn_ref::<HtmlElement>().unwrap().click();
    }

    fn install_frame_capture() {
        js_sys::eval(
            r#"
            window.__board_frames = [];
            window.__TAURI__ = window.__TAURI__ || {};
            window.__TAURI__.core = window.__TAURI__.core || {};
            window.__TAURI__.core.invoke = (cmd, args) => {
                if (cmd === 'send_host_line') window.__board_frames.push(JSON.parse(args.line));
                return Promise.resolve();
            };
        "#,
        )
        .unwrap();
    }

    fn sent_frames() -> Vec<protocol::Envelope> {
        serde_json::from_str(
            &js_sys::eval("JSON.stringify(window.__board_frames)")
                .unwrap()
                .as_string()
                .unwrap(),
        )
        .unwrap()
    }

    fn board_commands() -> Vec<BoardCommandPayload> {
        sent_frames()
            .iter()
            .filter(|frame| frame.kind == FrameKind::BoardCommand)
            .map(|frame| frame.parse_payload().unwrap())
            .collect()
    }

    /// Waits for the frame a just-handled event queued to reach the host.
    async fn settled_commands(state: &AppState) -> Vec<BoardCommandPayload> {
        next_tick().await;
        let stream = state.host_stream_untracked(HOST).unwrap();
        crate::send::settle_stream(HOST, &stream).await;
        board_commands()
    }

    fn page_id() -> ProjectId {
        ProjectId("page".to_owned())
    }

    fn in_lane(lane: &str) -> BoardLocation {
        BoardLocation::Lane {
            project_id: page_id(),
            lane_id: BoardLaneId(lane.to_owned()),
        }
    }

    fn in_card(card: &str) -> BoardLocation {
        BoardLocation::Parent {
            card_id: BoardCardId(card.to_owned()),
        }
    }

    fn board_lane(id: &str, name: &str, position: u64) -> BoardLane {
        BoardLane {
            id: BoardLaneId(id.to_owned()),
            name: name.to_owned(),
            position,
            instructions: None,
        }
    }

    fn moved(seq: u64, to: BoardLocation, note: Option<&str>, at: u64) -> BoardMove {
        BoardMove {
            seq,
            from: in_lane("active"),
            to,
            actor: BoardActor::User,
            note: note.map(str::to_owned),
            at,
        }
    }

    fn board_card(
        id: &str,
        title: &str,
        location: BoardLocation,
        version: u64,
        history: Vec<BoardMove>,
    ) -> BoardCard {
        BoardCard {
            retained: !history.is_empty(),
            id: BoardCardId(id.to_owned()),
            agent_id: AgentId(id.to_owned()),
            session_id: None,
            title: title.to_owned(),
            location,
            position: 0,
            version,
            history,
        }
    }

    fn live(id: &str) -> BoardNavigationTarget {
        BoardNavigationTarget::Live {
            agent_id: AgentId(id.to_owned()),
        }
    }

    fn board_state(
        version: u64,
        lanes: Vec<BoardLane>,
        cards: Vec<(BoardCard, BoardNavigationTarget)>,
    ) -> BoardStatePayload {
        BoardStatePayload {
            navigation: cards
                .iter()
                .map(|(card, target)| BoardCardNavigation {
                    card_id: card.id.clone(),
                    target: target.clone(),
                })
                .collect(),
            live_agents: Vec::new(),
            project_id: page_id(),
            version,
            customized: true,
            lanes,
            cards: cards.into_iter().map(|(card, _)| card).collect(),
        }
    }

    struct HostFrames {
        state: AppState,
        seq: u64,
    }

    impl HostFrames {
        async fn send<T: serde::Serialize>(&mut self, kind: FrameKind, payload: &T) {
            let stream = StreamPath(format!("/host/{HOST}"));
            crate::dispatch::dispatch_envelope(
                &self.state,
                HOST,
                protocol::Envelope::from_payload(stream, kind, self.seq, payload).unwrap(),
            );
            self.seq += 1;
            next_tick().await;
        }

        async fn board(&mut self, payload: &BoardStatePayload) {
            self.send(FrameKind::BoardState, payload).await;
        }

        async fn error(&mut self, code: BoardErrorCode, message: &str) {
            let payload = BoardErrorPayload {
                project_id: page_id(),
                code,
                message: message.to_owned(),
            };
            self.send(FrameKind::BoardError, &payload).await;
        }
    }

    fn fire(target: &Element, script: &str) -> bool {
        let event: web_sys::Event = js_sys::eval(script).unwrap().unchecked_into();
        !target.dispatch_event(&event).unwrap()
    }

    /// Returns whether a handler claimed the event, which for dragover is what
    /// makes the element a legal drop target.
    fn drag(target: &Element, kind: &str) -> bool {
        fire(
            target,
            &format!("new DragEvent('{kind}', {{ bubbles: true, cancelable: true }})"),
        )
    }

    fn drag_at(target: &Element, kind: &str, y: f64) -> bool {
        fire(
            target,
            &format!(
                "new DragEvent('{kind}', {{ bubbles: true, cancelable: true, clientY: {y} }})"
            ),
        )
    }

    fn key(target: &Element, name: &str) {
        fire(
            target,
            &format!(
                "new KeyboardEvent('keydown', {{ key: '{name}', bubbles: true, cancelable: true }})"
            ),
        );
    }

    fn lane(container: &HtmlElement, name: &str) -> Element {
        container
            .query_selector(&format!("section[aria-label='{name}']"))
            .unwrap()
            .unwrap_or_else(|| panic!("no lane named {name}"))
    }

    fn card_el(container: &HtmlElement, title: &str) -> Element {
        elements(container, "[draggable='true']")
            .into_iter()
            .find(|card| text(card).starts_with(title))
            .unwrap_or_else(|| panic!("no card titled {title}"))
    }

    async fn drag_card_to(source: &Element, target: &Element) -> bool {
        drag(source, "dragstart");
        next_tick().await;
        let accepted = drag(target, "dragenter") && drag(target, "dragover");
        if accepted {
            drag(target, "drop");
        }
        drag(source, "dragend");
        next_tick().await;
        accepted
    }

    fn open_card_actions(card: &Element) {
        let slot = card.parent_element().unwrap();
        let button = slot
            .query_selector(":scope > button[aria-label='Card actions']")
            .unwrap()
            .unwrap();
        click(&button);
    }

    fn menu_items(container: &HtmlElement) -> Vec<Element> {
        elements(container, "[role='menuitem']")
    }

    fn set_field(field: &Element, value: &str) {
        match field.dyn_ref::<web_sys::HtmlInputElement>() {
            Some(input) => input.set_value(value),
            None => field
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .set_value(value),
        }
        let event = web_sys::Event::new_with_event_init_dict("input", &{
            let init = web_sys::EventInit::new();
            init.set_bubbles(true);
            init
        })
        .unwrap();
        field.dispatch_event(&event).unwrap();
    }

    fn lane_names(container: &HtmlElement) -> Vec<String> {
        elements(container, "input[aria-label='Lane name']")
            .iter()
            .map(|input| {
                input
                    .dyn_ref::<web_sys::HtmlInputElement>()
                    .unwrap()
                    .value()
            })
            .collect()
    }

    fn dialog_button(container: &HtmlElement, label: &str) -> Element {
        elements(container, "[role='dialog'] button")
            .into_iter()
            .find(|button| text(button) == label)
            .unwrap_or_else(|| panic!("dialog has no {label} button"))
    }

    fn header_button(container: &HtmlElement, label: &str) -> Element {
        elements(container, "header button")
            .into_iter()
            .find(|button| text(button) == label)
            .unwrap_or_else(|| panic!("header has no {label} button"))
    }

    #[wasm_bindgen_test]
    async fn page_projects_board_and_details_and_routes_clicks() {
        install_frame_capture();
        reset_user_notice_for_tests();
        let container = make_container();
        let state = seed();
        let banner = make_container();
        let mounted = state.clone();
        let _banner = mount_to(banner.clone(), move || {
            provide_context(mounted.clone());
            view! { <Header /> }
        });
        let mounted = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(mounted.clone());
            view! { <ProjectPage host_id=HOST.to_owned() project_id=ProjectId("page".to_owned()) /> }
        });
        let surfaces = make_container();
        let mounted = state.clone();
        let _surfaces = mount_to(surfaces.clone(), move || {
            provide_context(mounted.clone());
            view! { <SteeringTab /> <SwarmsPanel /> }
        });
        next_tick().await;

        let page_text = container.text_content().unwrap_or_default();
        assert!(page_text.contains("Tyde"), "header names the project");
        assert!(page_text.contains("New agent"), "header offers a new agent");

        assert!(
            matches!(
                settled_commands(&state).await.as_slice(),
                [BoardCommandPayload::Read(read)] if read.project_id == page_id()
            ),
            "the page subscribes to its board"
        );
        assert!(
            header_button(&container, "Lanes")
                .dyn_ref::<web_sys::HtmlButtonElement>()
                .unwrap()
                .disabled(),
            "lanes cannot be edited before the board arrives"
        );
        assert!(
            elements(&container, "section[aria-label]").is_empty(),
            "no lanes are invented before the server sends them"
        );

        let now = js_sys::Date::now() as u64;
        let lanes = vec![
            board_lane("active", "Active", 0),
            board_lane("blocked", "Blocked", 1),
            board_lane("done", "Done", 2),
        ];
        let workbench_card = || {
            (
                board_card("newer", "Workbench card", in_lane("active"), 0, Vec::new()),
                live("newer"),
            )
        };
        let older_at = |location: BoardLocation, version| {
            (
                board_card(
                    "older",
                    "Older card",
                    location,
                    version,
                    vec![moved(1, in_lane("active"), Some("Needs a decision"), now)],
                ),
                live("older"),
            )
        };
        let nested_at = |location: BoardLocation, version| {
            (
                board_card(
                    "child",
                    "Nested card",
                    location,
                    version,
                    vec![moved(1, in_card("older"), None, now)],
                ),
                live("child"),
            )
        };
        let archived = || {
            (
                board_card(
                    "gone",
                    "Archived card",
                    in_lane("blocked"),
                    4,
                    vec![
                        moved(1, in_lane("active"), None, now - 34_200_000),
                        moved(
                            2,
                            in_lane("blocked"),
                            Some("Waiting on the API"),
                            now - 9_000_000,
                        ),
                    ],
                ),
                BoardNavigationTarget::Unavailable,
            )
        };
        let closed = |lane: &'static str| {
            (
                board_card(
                    "saved",
                    "Closed card",
                    in_lane(lane),
                    2,
                    vec![moved(1, in_lane(lane), None, now - 12_600_000)],
                ),
                BoardNavigationTarget::Saved {
                    session_id: SessionId("saved-session".to_owned()),
                    backend_kind: BackendKind::Codex,
                    project_id: Some(page_id()),
                },
            )
        };
        let mut host = HostFrames {
            state: state.clone(),
            seq: 0,
        };
        host.board(&board_state(
            7,
            lanes.clone(),
            vec![
                workbench_card(),
                older_at(in_lane("active"), 3),
                nested_at(in_card("older"), 5),
                archived(),
                closed("done"),
            ],
        ))
        .await;

        let page_text = container.text_content().unwrap_or_default();
        assert!(
            !page_text.contains("Other project card"),
            "the board shows only what the server placed on it"
        );
        assert!(
            text(&lane(&container, "Active")).starts_with("Active3"),
            "lane counts include nested cards: {}",
            text(&lane(&container, "Active"))
        );
        assert!(text(&lane(&container, "Blocked")).starts_with("Blocked1"));
        let lane_tops: Vec<f64> = ["Active", "Blocked", "Done"]
            .iter()
            .map(|name| lane(&container, name).get_bounding_client_rect().top())
            .collect();
        assert!(
            lane_tops.windows(2).all(|pair| pair[0] == pair[1]),
            "every lane starts on the same line: {lane_tops:?}"
        );
        let board_height = container
            .query_selector("aside")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect()
            .height();
        assert!(
            lane(&container, "Done").get_bounding_client_rect().height() > board_height - 96.0,
            "a short lane is still a full-height drop target"
        );

        let workbench = card_el(&container, "Workbench card");
        assert!(
            text(&workbench).contains("Thinking"),
            "card shows server-derived state"
        );
        assert!(
            text(&workbench).contains("feat/busy"),
            "workbench card shows its branch"
        );
        let older = card_el(&container, "Older card");
        assert!(text(&older).contains("Idle"));
        assert!(
            text(&older).contains("Needs a decision"),
            "card shows its latest note"
        );
        assert!(
            !text(&older).contains("feat/"),
            "plain card shows no branch"
        );

        let nested = card_el(&container, "Nested card");
        let (outer, inner) = (
            older.get_bounding_client_rect(),
            nested.get_bounding_client_rect(),
        );
        assert!(
            inner.left() > outer.left()
                && inner.right() < outer.right()
                && inner.top() > outer.top()
                && inner.bottom() < outer.bottom(),
            "a nested card renders inside its parent"
        );

        assert_eq!(text(&row_with(&container, "Review")), "Review0 open");
        let aside = container.query_selector("aside").unwrap().unwrap();
        let aside_text = text(&aside);
        let positions: Vec<usize> = [
            "Review",
            "Roots2",
            "Workbenches2",
            "Steering1",
            "Workflows0",
            "Swarms2",
        ]
        .iter()
        .map(|label| {
            aside_text
                .find(label)
                .unwrap_or_else(|| panic!("details show {label}: {aside_text}"))
        })
        .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "details order: {aside_text}"
        );

        let dirty = row_with(&container, "tyde");
        assert_eq!(text(&dirty), "tydemain · 3 changed");
        assert_eq!(text(&row_with(&container, "docs")), "docsmain · clean");
        assert_eq!(text(&row_with(&container, "feat/busy")), "feat/busy");
        let empty = row_with(&container, "feat/empty");
        assert_eq!(text(&empty), "feat/emptyNo agents");

        for row in [&dirty, &empty] {
            let row_rect = row.get_bounding_client_rect();
            let meta = elements(&container, "aside span")
                .into_iter()
                .filter(|span| {
                    row.contains(Some(span)) && span.get_bounding_client_rect().width() > 0.0
                })
                .last()
                .unwrap();
            let gap = row_rect.right() - meta.get_bounding_client_rect().right();
            assert!(
                (gap - 8.0).abs() < 0.5,
                "metadata is right-aligned, gap {gap}"
            );
        }
        let actions = elements(&container, "aside button[aria-label='Remove workbench']");
        assert_eq!(actions.len(), 2);
        assert!(
            actions
                .iter()
                .all(|action| action.get_bounding_client_rect().width() == 0.0),
            "row actions stay hidden until hover"
        );

        let swarms = row_with(&container, "Swarms").get_bounding_client_rect();
        let aside_rect = aside.get_bounding_client_rect();
        assert!(
            (aside_rect.bottom() - 16.0 - swarms.bottom()).abs() < 0.5,
            "Swarms is pinned to the bottom of the details column"
        );

        click(&card_el(&container, "Older card"));
        next_tick().await;
        assert_eq!(
            state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                host_id: HOST.to_owned(),
                agent_id: AgentId("older".to_owned()),
            }),
            "clicking a card opens that agent"
        );

        let closed_card = card_el(&container, "Closed card");
        assert!(text(&closed_card).contains("Closed"));
        assert!(
            text(&closed_card).contains("3h"),
            "a closed card ages from its last move"
        );

        let opened = state.active_agent.get_untracked();
        let gone = card_el(&container, "Archived card");
        assert!(text(&gone).contains("Archived"));
        let history = gone.query_selector("ol").unwrap().unwrap();
        let moves = elements(history.unchecked_ref(), "li");
        assert_eq!(
            moves.iter().map(text).collect::<Vec<_>>(),
            ["Blocked2hWaiting on the API", "Active9h"],
            "an archived card keeps its history, newest first"
        );
        assert!(!text(&gone).starts_with("Archived cardArchivedWaiting"));
        click(&gone);
        next_tick().await;
        assert_eq!(
            state.active_agent.get_untracked(),
            opened,
            "an archived card has no chat to open"
        );

        let sent = settled_commands(&state).await.len();
        assert!(
            drag_card_to(&older, &lane(&container, "Blocked")).await,
            "another lane accepts a card"
        );
        let commands = settled_commands(&state).await;
        assert_eq!(commands.len(), sent + 1);
        assert!(
            matches!(
                commands.last(),
                Some(BoardCommandPayload::Move(sent)) if sent.card_id.0 == "older"
                    && sent.destination == BoardDestination::Lane { name: "Blocked".to_owned() }
                    && sent.expected_version == 3
                    && sent.project_id == page_id()
                    && sent.position.is_none()
            ),
            "dragging to a lane sends one Move: {commands:?}"
        );
        assert!(
            lane(&container, "Active").contains(Some(&older)),
            "a card stays put until the server moves it"
        );
        assert!(
            !drag_card_to(
                &card_el(&container, "Workbench card"),
                &lane(&container, "Active")
            )
            .await,
            "a card's own lane is not a drop target"
        );
        assert_eq!(
            settled_commands(&state).await.len(),
            sent + 1,
            "no-op drops send nothing"
        );

        host.board(&board_state(
            8,
            lanes.clone(),
            vec![
                workbench_card(),
                older_at(in_lane("blocked"), 4),
                nested_at(in_card("older"), 5),
                archived(),
                closed("done"),
            ],
        ))
        .await;
        assert!(text(&lane(&container, "Active")).starts_with("Active1"));
        assert!(
            text(&lane(&container, "Blocked")).starts_with("Blocked3"),
            "a nested card travels with its parent"
        );

        let older = card_el(&container, "Older card");
        let nested = card_el(&container, "Nested card");
        assert!(
            !drag_card_to(&nested, &older).await,
            "a card cannot nest into the parent it is already in"
        );
        assert!(
            drag_card_to(&nested, &lane(&container, "Done")).await,
            "a nested card can leave for a lane"
        );
        assert!(
            matches!(
                settled_commands(&state).await.last(),
                Some(BoardCommandPayload::Move(sent)) if sent.card_id.0 == "child"
                    && sent.destination == BoardDestination::Lane { name: "Done".to_owned() }
                    && sent.expected_version == 5
            ),
            "dragging a nested card to a lane un-nests it"
        );
        assert!(
            drag_card_to(&card_el(&container, "Workbench card"), &nested).await,
            "a card dropped on a nested card nests in that card's parent"
        );
        assert!(
            matches!(
                settled_commands(&state).await.last(),
                Some(BoardCommandPayload::Move(sent)) if sent.card_id.0 == "newer"
                    && sent.destination == BoardDestination::Parent {
                        card_id: BoardCardId("older".to_owned())
                    }
                    && sent.expected_version == 0
            ),
            "dropping onto a card nests under the top-level card"
        );

        host.error(
            BoardErrorCode::StaleVersion,
            "This card changed elsewhere. Try again.",
        )
        .await;
        assert!(
            text(&banner).contains("This card changed elsewhere. Try again."),
            "a rejected move reaches the user: {}",
            text(&banner)
        );
        reset_user_notice_for_tests();

        let workbench = card_el(&container, "Workbench card");
        open_card_actions(&workbench);
        next_tick().await;
        let items = menu_items(&container);
        assert_eq!(
            items.iter().map(text).collect::<Vec<_>>(),
            ["Move to Blocked", "Move to Done"],
            "the menu offers every other lane and no Remove for a live card"
        );
        let sent = settled_commands(&state).await.len();
        click(&items[1]);
        let commands = settled_commands(&state).await;
        assert_eq!(commands.len(), sent + 1);
        assert!(
            matches!(
                commands.last(),
                Some(BoardCommandPayload::Move(sent)) if sent.card_id.0 == "newer"
                    && sent.destination == BoardDestination::Lane { name: "Done".to_owned() }
            ),
            "the menu moves a card without dragging"
        );
        assert!(
            menu_items(&container).is_empty(),
            "choosing an item closes the menu"
        );

        open_card_actions(&card_el(&container, "Archived card"));
        next_tick().await;
        let items = menu_items(&container);
        assert_eq!(text(items.last().unwrap()), "Remove");
        click(items.last().unwrap());
        assert!(
            matches!(
                settled_commands(&state).await.last(),
                Some(BoardCommandPayload::Remove { card_id, expected_version: 4, .. })
                    if card_id.0 == "gone"
            ),
            "an archived card can be removed"
        );

        let lanes_button = header_button(&container, "Lanes");
        click(&lanes_button);
        next_tick().await;
        assert_eq!(lane_names(&container), ["Active", "Blocked", "Done"]);
        let dialog = container
            .query_selector("[role='dialog']")
            .unwrap()
            .unwrap()
            .get_bounding_client_rect();
        let viewport = f64::from(
            web_sys::window()
                .unwrap()
                .inner_width()
                .unwrap()
                .as_f64()
                .unwrap() as i32,
        );
        assert!(
            (dialog.left() + dialog.width() / 2.0 - viewport / 2.0).abs() < 1.0,
            "the dialog is centered"
        );
        let sent = settled_commands(&state).await.len();
        click(&dialog_button(&container, "Save"));
        next_tick().await;
        assert!(
            container
                .query_selector("[role='dialog']")
                .unwrap()
                .is_none(),
            "saving unchanged lanes just closes"
        );
        assert_eq!(
            settled_commands(&state).await.len(),
            sent,
            "unchanged lanes send nothing"
        );

        click(&lanes_button);
        next_tick().await;
        set_field(
            &elements(&container, "input[aria-label='Lane name']")[0],
            "Discarded",
        );
        click(&dialog_button(&container, "Cancel"));
        next_tick().await;
        click(&lanes_button);
        next_tick().await;
        assert_eq!(
            lane_names(&container)[0],
            "Active",
            "Cancel discards the draft"
        );

        assert_eq!(
            elements(&container, "textarea[aria-label='Lane instructions']")[0]
                .get_attribute("placeholder")
                .as_deref(),
            Some("Instructions"),
            "an empty instructions box is named"
        );
        let names = elements(&container, "input[aria-label='Lane name']");
        set_field(&names[1], "  Waiting  ");
        set_field(
            &elements(&container, "textarea[aria-label='Lane instructions']")[0],
            "Work in a workbench.",
        );
        set_field(
            &elements(&container, "textarea[aria-label='Lane instructions']")[1],
            "   ",
        );
        click(&elements(&container, "button[aria-label='Remove lane']")[2]);
        click(&dialog_button(&container, "Add lane"));
        next_tick().await;
        let names = elements(&container, "input[aria-label='Lane name']");
        assert_eq!(names.len(), 3);
        let focused = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .active_element();
        assert!(
            names[2].is_same_node(focused.as_ref().map(|el| el.as_ref())),
            "a new lane is ready to name"
        );
        set_field(&names[2], "Review");

        key(
            &elements(&container, "[aria-label='Reorder lane']")[0],
            "ArrowDown",
        );
        next_tick().await;
        assert_eq!(lane_names(&container), ["  Waiting  ", "Active", "Review"]);

        let rows = |container: &HtmlElement| elements(container, "[aria-label='Reorder lane']");
        let review_grip = rows(&container)[2].clone();
        drag(&review_grip, "dragstart");
        next_tick().await;
        let first_row = rows(&container)[0].parent_element().unwrap();
        let rect = first_row.get_bounding_client_rect();
        assert!(
            drag_at(&first_row, "dragenter", rect.bottom() - 1.0)
                && drag_at(&first_row, "dragover", rect.bottom() - 1.0),
            "a row is a drop target during a lane drag"
        );
        next_tick().await;
        assert_eq!(
            lane_names(&container),
            ["  Waiting  ", "Active", "Review"],
            "a row does not swap until the pointer passes its midpoint"
        );
        drag_at(&first_row, "dragover", rect.top() + 1.0);
        next_tick().await;
        drag(&review_grip, "dragend");
        next_tick().await;
        assert_eq!(lane_names(&container), ["Review", "  Waiting  ", "Active"]);

        let sent = settled_commands(&state).await.len();
        click(&dialog_button(&container, "Save"));
        let commands = settled_commands(&state).await;
        assert_eq!(commands.len(), sent + 1);
        let Some(BoardCommandPayload::Lanes(saved)) = commands.last() else {
            panic!("Save sends Lanes: {commands:?}");
        };
        assert_eq!(saved.expected_version, 8);
        assert_eq!(saved.project_id, page_id());
        assert_eq!(
            saved
                .lanes
                .iter()
                .map(|lane| lane.name.as_str())
                .collect::<Vec<_>>(),
            ["Review", "Waiting", "Active"]
        );
        assert_eq!(
            saved
                .lanes
                .iter()
                .map(|lane| lane.position)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(saved.lanes[1].id, BoardLaneId("blocked".to_owned()));
        assert!(!saved.lanes[0].id.0.is_empty());
        assert_eq!(
            saved.lanes[1].instructions, None,
            "a blank box is no instructions"
        );
        assert_eq!(
            saved.lanes[2].instructions.as_deref(),
            Some("Work in a workbench.")
        );
        let saved = saved.clone();
        assert!(
            container
                .query_selector("[role='dialog']")
                .unwrap()
                .is_some(),
            "the dialog waits for the server"
        );

        host.error(BoardErrorCode::InvalidInput, "lane names must be unique")
            .await;
        let alert = container.query_selector("[role='alert']").unwrap().unwrap();
        assert_eq!(text(&alert), "lane names must be unique");
        assert!(
            !text(&banner).contains("lane names must be unique"),
            "a rejected save is reported in the dialog, not the app banner"
        );
        click(&dialog_button(&container, "Save"));
        next_tick().await;
        assert!(
            container
                .query_selector("[role='alert']")
                .unwrap()
                .is_none(),
            "a retry clears the previous rejection"
        );
        host.board(&board_state(
            9,
            saved.lanes.clone(),
            vec![closed("blocked")],
        ))
        .await;
        assert!(
            container
                .query_selector("[role='dialog']")
                .unwrap()
                .is_none(),
            "the dialog closes once the server has the lanes"
        );
        let section_labels: Vec<String> = elements(&container, "section[aria-label]")
            .iter()
            .filter_map(|section| section.get_attribute("aria-label"))
            .collect();
        assert_eq!(
            section_labels,
            ["Review", "Waiting", "Active"],
            "lanes render in server order"
        );

        click(&row_with(&container, "Steering"));
        next_tick().await;
        assert_eq!(state.settings_tab_request.get_untracked(), Some("Steering"));
        assert!(state.settings_open.get_untracked());
        let steering_row = row_with(&container, "Steering");
        assert_eq!(
            text(&steering_row).trim_start_matches("Steering").trim(),
            "1"
        );
        let surface_text = text(&surfaces);
        assert!(surface_text.contains("Project rule"));
        assert!(
            !surface_text.contains("Host rule"),
            "Steering shows only what the page counted"
        );

        click(&row_with(&container, "Swarms"));
        next_tick().await;
        let swarms_row = row_with(&container, "Swarms");
        assert_eq!(text(&swarms_row).trim_start_matches("Swarms").trim(), "2");
        let surface_text = text(&surfaces);
        assert!(surface_text.contains("Page crew"));
        assert!(surface_text.contains("Workbench crew"));
        assert!(
            !surface_text.contains("Other crew"),
            "Swarms shows only what the page counted"
        );
        let chips = elements(&surfaces, "button[aria-label='Clear project filter']");
        assert_eq!(chips.iter().map(text).collect::<Vec<_>>(), ["Tyde", "Tyde"]);

        state.selected_host_id.set(Some("another-host".to_owned()));
        next_tick().await;
        assert_eq!(
            elements(&surfaces, "button[aria-label='Clear project filter']").len(),
            1,
            "changing the Settings host drops the Steering project filter"
        );
        assert!(!text(&surfaces).contains("No steering on this project"));

        click(&card_el(&container, "Closed card"));
        settled_commands(&state).await;
        let resume = sent_frames()
            .into_iter()
            .rfind(|frame| frame.kind == FrameKind::SpawnAgent)
            .expect("clicking a closed card resumes it");
        let resume: protocol::SpawnAgentPayload = resume.parse_payload().unwrap();
        assert!(
            matches!(
                &resume.params,
                protocol::SpawnAgentParams::Resume { session_id, .. }
                    if session_id.0 == "saved-session"
            ),
            "a closed card resumes from the server's navigation target"
        );
    }
}
