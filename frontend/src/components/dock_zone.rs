use leptos::prelude::*;

use crate::components::agents_panel::AgentsPanel;
use crate::components::file_explorer::FileExplorer;
use crate::components::git_panel::GitPanel;
use crate::components::references_panel::ReferencesPanel;
use crate::components::search_panel::SearchPanel;
use crate::components::sessions_panel::SessionsPanel;
use crate::components::swarms_panel::SwarmsPanel;
use crate::components::terminal_view::TerminalView;
use crate::components::workflows_panel::WorkflowsPanel;
use crate::sidebar_layout::{SidebarSide, SidebarTab};
use crate::state::AppState;

#[derive(Clone, Copy, PartialEq)]
pub enum DockPosition {
    Left,
    Right,
    Bottom,
}

#[component]
pub fn DockZone(position: DockPosition) -> impl IntoView {
    match position {
        DockPosition::Left => view! { <SidebarTabs side=SidebarSide::Left /> }.into_any(),
        DockPosition::Right => view! { <SidebarTabs side=SidebarSide::Right /> }.into_any(),
        DockPosition::Bottom => view! { <TerminalView /> }.into_any(),
    }
}

#[component]
fn SidebarTabs(side: SidebarSide) -> impl IntoView {
    let state = expect_context::<AppState>();
    let layouts = state.sidebar_layout;
    let blocked = state.sidebar_storage_blocked;
    let dragged = state.sidebar_drag;
    let hover = RwSignal::new(None::<SidebarTab>);
    let over = RwSignal::new(false);
    let move_state = state.clone();
    view! {
        <div class="dock-tab-bar" style=move || if blocked.get() { "display:none" } else { "" } class:sidebar-drop-over=move || over.get() && dragged.get().is_some()
            aria-label=format!("{} sidebar tabs", side.label())
            on:dragover=move |ev: web_sys::DragEvent| {
                if dragged.get_untracked().is_some() { ev.prevent_default(); over.set(true); hover.set(None); }
            }
            on:dragleave=move |_| { over.set(false); hover.set(None); }
            on:drop=move |ev: web_sys::DragEvent| {
                if let Some(tab) = dragged.get_untracked() {
                    ev.prevent_default(); ev.stop_propagation();
                    move_state.move_sidebar_tab(tab, side, hover.get_untracked());
                }
                dragged.set(None); over.set(false); hover.set(None);
            }
        >
            <For each=move || layouts.with(|layouts| layouts.current().pane(side).tabs.clone()) key=|tab| *tab children=move |tab| {
                let activate = state.clone();
                let keys = state.clone();
                let menu_left = state.clone();
                let menu_right = state.clone();
                let menu_previous = state.clone();
                let menu_next = state.clone();
                view! {
                    <div class="dock-tab-item" class:sidebar-drop-before=move || dragged.get().is_some() && hover.get() == Some(tab)
                        on:dragover=move |ev: web_sys::DragEvent| {
                            if dragged.get_untracked().is_some() { ev.prevent_default(); ev.stop_propagation(); hover.set(Some(tab)); over.set(true); }
                        }
                    >
                        <button type="button" class="dock-tab" class:active=move || layouts.with(|l| l.current().pane(side).active == Some(tab))
                            draggable="true" aria-label=tab.label()
                            title="Drag to either sidebar. Ctrl+Shift+Left/Right moves sides; Ctrl+Shift+Up/Down reorders."
                            on:click=move |_| activate.activate_sidebar_tab(tab)
                            on:dragstart=move |ev: web_sys::DragEvent| {
                                ev.stop_propagation();
                                if let Some(data) = ev.data_transfer() {
                                    if let Err(error) = data.set_data("application/x-tyde-sidebar-tab", tab.label()) {
                                        ev.prevent_default(); crate::components::header::report_user_error(format!("Cannot drag sidebar tab: {error:?}")); return;
                                    }
                                    data.set_effect_allowed("move");
                                }
                                dragged.set(Some(tab));
                            }
                            on:dragend=move |_| { dragged.set(None); hover.set(None); over.set(false); }
                            on:keydown=move |ev: web_sys::KeyboardEvent| {
                                if ev.key() == "Escape" { dragged.set(None); hover.set(None); over.set(false); }
                                if ev.ctrl_key() && ev.shift_key() {
                                    match ev.key().as_str() {
                                        "ArrowLeft" => { ev.prevent_default(); keys.move_sidebar_tab(tab, SidebarSide::Left, None); },
                                        "ArrowRight" => { ev.prevent_default(); keys.move_sidebar_tab(tab, SidebarSide::Right, None); },
                                        "ArrowUp" => { ev.prevent_default(); reorder(&keys, tab, side, false); },
                                        "ArrowDown" => { ev.prevent_default(); reorder(&keys, tab, side, true); },
                                        _ => {}
                                    }
                                }
                            }
                        >{tab.label()}</button>
                        <details class="sidebar-tab-menu">
                            <summary aria-label=format!("{} tab actions", tab.label()) title="Move or reorder tab">"⋯"</summary>
                            <div class="sidebar-tab-actions">
                                <button on:click=move |_| menu_left.move_sidebar_tab(tab, SidebarSide::Left, None)>"Move left"</button>
                                <button on:click=move |_| menu_right.move_sidebar_tab(tab, SidebarSide::Right, None)>"Move right"</button>
                                <button on:click=move |_| reorder(&menu_previous, tab, side, false)>"Move earlier"</button>
                                <button on:click=move |_| reorder(&menu_next, tab, side, true)>"Move later"</button>
                            </div>
                        </details>
                    </div>
                }
            } />
            <span class="sidebar-empty-target" style=move || if layouts.with(|l| l.current().pane(side).tabs.is_empty()) { "" } else { "display:none" } title="Drag sidebar tabs here">"⋮"</span>
        </div>
    }
}

fn reorder(state: &AppState, tab: SidebarTab, side: SidebarSide, later: bool) {
    let tabs = state
        .sidebar_layout
        .with_untracked(|l| l.current().pane(side).tabs.clone());
    let Some(index) = tabs.iter().position(|item| *item == tab) else {
        return;
    };
    if later {
        if index + 1 < tabs.len() {
            state.move_sidebar_tab(tab, side, tabs.get(index + 2).copied());
        }
    } else if index > 0 {
        state.move_sidebar_tab(tab, side, Some(tabs[index - 1]));
    }
}

#[component]
pub fn SidebarPanels() -> impl IntoView {
    let state = expect_context::<AppState>();
    let layouts = state.sidebar_layout;
    let blocked = state.sidebar_storage_blocked;
    view! {
        <For each=|| SidebarTab::ALL key=|tab| *tab children=move |tab| {
            // Every component has one lifetime; CSS grid placement moves its
            // existing DOM instead of recreating subscriptions or local state.
            let style = move || {
                if blocked.get() { return "display:none".into(); }
                layouts.with(|layouts| {
                match layouts.current().side(tab) {
                    Some(side) => {
                        let pane = layouts.current().pane(side);
                        if pane.visible && pane.active == Some(tab) { format!("grid-column:{};grid-row:2", side.column()) } else { "display:none".into() }
                    },
                    None => "display:none".into(),
                }
                })
            };
            view! {
                <section class="dock-tab-mount sidebar-panel" aria-label=format!("{} panel", tab.label()) style=style>
                    {match tab {
                        SidebarTab::Agents => view! { <AgentsPanel /> }.into_any(),
                        SidebarTab::Sessions => view! { <SessionsPanel /> }.into_any(),
                        SidebarTab::Files => view! { <FileExplorer /> }.into_any(),
                        SidebarTab::Git => view! { <GitPanel /> }.into_any(),
                        SidebarTab::Search => view! { <SearchPanel /> }.into_any(),
                        SidebarTab::References => view! { <ReferencesPanel /> }.into_any(),
                        SidebarTab::Swarms => view! { <SwarmsPanel /> }.into_any(),
                        SidebarTab::Workflows => view! { <WorkflowsPanel /> }.into_any(),
                    }}
                </section>
            }
        } />
    }
}
