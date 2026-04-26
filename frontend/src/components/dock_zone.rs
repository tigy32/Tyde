use leptos::prelude::*;

use crate::components::agents_panel::AgentsPanel;
use crate::components::file_explorer::FileExplorer;
use crate::components::git_panel::GitPanel;
use crate::components::sessions_panel::SessionsPanel;
use crate::components::terminal_view::TerminalView;

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

#[derive(Clone, Copy, PartialEq)]
enum RightTab {
    Agents,
    Sessions,
}

#[component]
fn RightDock() -> impl IntoView {
    let active_tab = RwSignal::new(RightTab::Agents);

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

    view! {
        <div class="dock-inner">
            <div class="dock-tab-bar">
                <button class={tab_class(RightTab::Agents)} on:click=move |_| active_tab.set(RightTab::Agents)>
                    "Agents"
                </button>
                <button class={tab_class(RightTab::Sessions)} on:click=move |_| active_tab.set(RightTab::Sessions)>
                    "Sessions"
                </button>
            </div>
            <div class="dock-tab-content">
                <div class="dock-tab-mount" style=agents_style>
                    <AgentsPanel />
                </div>
                <div class="dock-tab-mount" style=sessions_style>
                    <SessionsPanel />
                </div>
            </div>
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum LeftTab {
    Files,
    Git,
}

#[component]
fn LeftDock() -> impl IntoView {
    let active_tab = RwSignal::new(LeftTab::Files);

    let tab_class = move |target: LeftTab| {
        move || {
            if active_tab.get() == target {
                "dock-tab active"
            } else {
                "dock-tab"
            }
        }
    };

    let files_style = move || {
        if active_tab.get() == LeftTab::Files {
            ""
        } else {
            "display: none;"
        }
    };
    let git_style = move || {
        if active_tab.get() == LeftTab::Git {
            ""
        } else {
            "display: none;"
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
            </div>
            <div class="dock-tab-content">
                <div class="dock-tab-mount" style=files_style>
                    <FileExplorer />
                </div>
                <div class="dock-tab-mount" style=git_style>
                    <GitPanel />
                </div>
            </div>
        </div>
    }
}
