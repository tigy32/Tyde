use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::components::card_menu::{CardContextMenu, open_card_menu};

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

#[component]
pub fn PanelIcon(side: SidebarSide) -> impl IntoView {
    let divider = match side {
        SidebarSide::Left => "M7 3v14",
        SidebarSide::Bottom => "M3 13h14",
        SidebarSide::Right => "M13 3v14",
    };
    view! {
        <svg width="16" height="16" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true">
            <rect x="2.75" y="2.75" width="14.5" height="14.5" rx="1.25" />
            <path d=divider />
        </svg>
    }
}

#[component]
pub fn DockZone(position: SidebarSide) -> impl IntoView {
    view! { <SidebarTabs side=position /> }
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
    let checklist_state = state.clone();
    let empty_move = state.clone();
    let menu = RwSignal::new(None);
    let anchor = NodeRef::<leptos::html::Div>::new();
    view! {
        <div class="dock-tab-bar" style=move || if blocked.get() { "display:none" } else { "" } class:sidebar-drop-over=move || over.get() && dragged.get().is_some()
            aria-label=format!("{} sidebar tabs", side.label())
            node_ref=anchor tabindex="0" aria-haspopup="menu"
            on:contextmenu=move |ev| open_card_menu(menu, &ev)
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if ev.key() == "ContextMenu" || (ev.shift_key() && ev.key() == "F10") {
                    ev.prevent_default(); ev.stop_propagation();
                    if let Some(target) = ev.target().and_then(|target| target.dyn_into::<web_sys::Element>().ok()) {
                        let rect = target.get_bounding_client_rect();
                        menu.set(Some((rect.left(), rect.bottom())));
                    }
                }
            }
            on:dragover=move |ev: web_sys::DragEvent| {
                if dragged.get_untracked().is_some() { ev.prevent_default(); if let Some(data) = ev.data_transfer() { data.set_drop_effect("move"); } over.set(true); hover.set(None); }
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
                view! {
                    <div class="dock-tab-item" style=move || if layouts.with(|l| l.current().pane(side).visible) { "" } else { "display:none" } class:sidebar-drop-before=move || dragged.get().is_some() && hover.get() == Some(tab)
                        on:dragover=move |ev: web_sys::DragEvent| {
                            if dragged.get_untracked().is_some() { ev.prevent_default(); ev.stop_propagation(); if let Some(data) = ev.data_transfer() { data.set_drop_effect("move"); } hover.set(Some(tab)); over.set(true); }
                        }
                    >
                        <button type="button" class="dock-tab" class:active=move || layouts.with(|l| l.current().pane(side).active == Some(tab))
                            draggable="true" aria-label=tab.label()
                            title="Drag to any dock. Ctrl+Shift+Left/Right moves sides; Ctrl+Shift+Up/Down reorders."
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
                    </div>
                }
            } />
            <Show when=move || layouts.with(|l| l.current().pane(side).tabs.is_empty())>
                <button type="button" class="dock-empty-menu" aria-label=format!("{} dock tab menu", side.label()) title="Add tabs"
                    aria-haspopup="menu" aria-expanded=move || menu.get().is_some().to_string()
                    on:click=move |ev| {
                        if let Some(target) = ev.current_target().and_then(|target| target.dyn_into::<web_sys::Element>().ok()) {
                            let rect = target.get_bounding_client_rect();
                            menu.set(Some((rect.left(), rect.bottom())));
                        }
                    }>"⋯"</button>
            </Show>
            <span class="sidebar-empty-target" style=move || if dragged.get().is_some() && layouts.with(|l| { let pane = l.current().pane(side); !pane.visible || pane.tabs.is_empty() }) { "" } else { "display:none" }>"Drop tabs here"</span>
        </div>
        <div class="dock-empty" aria-label=format!("{} dock empty", side.label())
            style=move || layouts.with(|l| {
                let pane = l.current().pane(side);
                if blocked.get() || !pane.visible || !pane.tabs.is_empty() { "display:none" } else { "" }
            })
            on:contextmenu=move |ev| open_card_menu(menu, &ev)
            on:dragover=move |ev: web_sys::DragEvent| {
                if dragged.get_untracked().is_some() { ev.prevent_default(); over.set(true); }
            }
            on:dragleave=move |_| over.set(false)
            class:sidebar-drop-over=move || over.get() && dragged.get().is_some()
            on:drop=move |ev: web_sys::DragEvent| {
                if let Some(tab) = dragged.get_untracked() {
                    ev.prevent_default(); ev.stop_propagation();
                    empty_move.move_sidebar_tab(tab, side, None);
                }
                dragged.set(None); over.set(false); hover.set(None);
            }>
            <PanelIcon side=side />
            <span>"Drag a tab here"</span>
            <small>"Right-click to add tabs"</small>
        </div>
        <CardContextMenu menu=menu label="Sidebar tabs" anchor=anchor>
            <div>
                {SidebarTab::ALL.into_iter().map(|tab| {
                    let toggle = checklist_state.clone();
                    view! {
                        <button type="button" class="context-menu-item sidebar-tab-choice" role="menuitemcheckbox"
                            aria-label=tab.label() aria-checked=move || layouts.with(|layouts| layouts.current().side(tab).is_some()).to_string()
                            on:click=move |_| toggle.toggle_sidebar_tab(tab, side)>
                            <span class="sidebar-tab-check" aria-hidden="true">{move || layouts.with(|layouts| layouts.current().side(tab).is_some()).then_some("✓")}</span>
                            {tab.label()}
                        </button>
                    }
                }).collect_view()}
            </div>
        </CardContextMenu>
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
                match [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom].into_iter().find(|side| layouts.current().pane(*side).active == Some(tab)) {
                    Some(side) => {
                        let pane = layouts.current().pane(side);
                        if pane.visible && pane.active == Some(tab) { match side { SidebarSide::Bottom => "grid-column:1 / -1;grid-row:5".into(), _ => format!("grid-column:{};grid-row:2", side.column()) } } else { "display:none".into() }
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
                        SidebarTab::Terminal => view! { <TerminalView /> }.into_any(),
                    }}
                </section>
            }
        } />
    }
}
