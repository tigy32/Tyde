use leptos::prelude::*;

use crate::components::agents_panel::AgentsPanel;
use crate::components::file_explorer::FileExplorer;
use crate::components::git_panel::GitPanel;
use crate::components::references_panel::ReferencesPanel;
use crate::components::search_panel::SearchPanel;
use crate::components::sessions_panel::SessionsPanel;
use crate::components::teams_panel::TeamsPanel;
use crate::components::terminal_view::TerminalView;
use crate::components::workflows_panel::WorkflowsPanel;
use crate::state::{AppState, LeftTab, RightTab};

#[derive(Clone, Copy, PartialEq)]
pub enum DockPosition {
    Left,
    Right,
    Bottom,
}

#[component]
pub fn DockZone(position: DockPosition) -> impl IntoView {
    match position {
        DockPosition::Right => view! { <RightDock /> }.into_any(),
        DockPosition::Left => view! { <LeftDock /> }.into_any(),
        DockPosition::Bottom => view! { <TerminalView /> }.into_any(),
    }
}

#[component]
fn RightDock() -> impl IntoView {
    let active_tab = expect_context::<AppState>().right_tab;

    let tab_class = move |target: RightTab| {
        move || {
            if active_tab.get() == target {
                "dock-tab active"
            } else {
                "dock-tab"
            }
        }
    };

    // Mount-and-hide both panels so per-component state (selection, scroll
    // position, expanded rows) survives a tab switch. Switching tabs only
    // toggles CSS visibility — no remount, no reactive subscription
    // teardown.
    let agents_style = move || {
        if active_tab.get() == RightTab::Agents {
            ""
        } else {
            "display: none;"
        }
    };
    let sessions_style = move || {
        if active_tab.get() == RightTab::Sessions {
            ""
        } else {
            "display: none;"
        }
    };
    let teams_style = move || {
        if active_tab.get() == RightTab::Teams {
            ""
        } else {
            "display: none;"
        }
    };
    let workflows_style = move || {
        if active_tab.get() == RightTab::Workflows {
            ""
        } else {
            "display: none;"
        }
    };

    view! {
        <div class="dock-inner">
            <div class="dock-tab-bar">
                <button class={tab_class(RightTab::Agents)} on:click=move |_| active_tab.set(RightTab::Agents)>
                    "Agents"
                </button>
                <button class={tab_class(RightTab::Sessions)} on:click=move |_| active_tab.set(RightTab::Sessions)>
                    "History"
                </button>
                <button class={tab_class(RightTab::Teams)} on:click=move |_| active_tab.set(RightTab::Teams)>
                    "Teams"
                </button>
                <button class={tab_class(RightTab::Workflows)} on:click=move |_| active_tab.set(RightTab::Workflows)>
                    "Workflows"
                </button>
            </div>
            <div class="dock-tab-content">
                <div class="dock-tab-mount" style=agents_style>
                    <AgentsPanel />
                </div>
                <div class="dock-tab-mount" style=sessions_style>
                    <SessionsPanel />
                </div>
                <div class="dock-tab-mount" style=teams_style>
                    <TeamsPanel />
                </div>
                <div class="dock-tab-mount" style=workflows_style>
                    <WorkflowsPanel />
                </div>
            </div>
        </div>
    }
}

#[component]
fn LeftDock() -> impl IntoView {
    // The active left tab lives in AppState so the Cmd/Ctrl+Shift+F shortcut
    // and the file-explorer "search in folder" action can switch to Search.
    let active_tab = expect_context::<AppState>().left_tab;

    let tab_class = move |target: LeftTab| {
        move || {
            if active_tab.get() == target {
                "dock-tab active"
            } else {
                "dock-tab"
            }
        }
    };

    let tab_style = move |target: LeftTab| {
        move || {
            if active_tab.get() == target {
                ""
            } else {
                "display: none;"
            }
        }
    };

    view! {
        <div class="dock-inner">
            <div class="dock-tab-bar">
                <button class={tab_class(LeftTab::Files)} on:click=move |_| active_tab.set(LeftTab::Files)>
                    "Files"
                </button>
                <button class={tab_class(LeftTab::Git)} on:click=move |_| active_tab.set(LeftTab::Git)>
                    "Git"
                </button>
                <button class={tab_class(LeftTab::Search)} on:click=move |_| active_tab.set(LeftTab::Search)>
                    "Search"
                </button>
                <button class={tab_class(LeftTab::References)} on:click=move |_| active_tab.set(LeftTab::References)>
                    "Refs"
                </button>
            </div>
            <div class="dock-tab-content">
                <div class="dock-tab-mount" style=tab_style(LeftTab::Files)>
                    <FileExplorer />
                </div>
                <div class="dock-tab-mount" style=tab_style(LeftTab::Git)>
                    <GitPanel />
                </div>
                <div class="dock-tab-mount" style=tab_style(LeftTab::Search)>
                    <SearchPanel />
                </div>
                <div class="dock-tab-mount" style=tab_style(LeftTab::References)>
                    <ReferencesPanel />
                </div>
            </div>
        </div>
    }
}
