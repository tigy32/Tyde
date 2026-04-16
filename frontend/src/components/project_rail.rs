use leptos::prelude::*;

use crate::components::host_browser::open_project_browser;
use crate::state::{ActiveProjectRef, AppState, CenterView};

#[component]
pub fn ProjectRail() -> impl IntoView {
    let state = expect_context::<AppState>();
    let state_for_connected = state.clone();
    let state_for_add = state.clone();
    let state_for_hosts = state.clone();

    let state_for_home = state.clone();
    let go_home = move |_| {
        state_for_home.switch_active_project(None);
    };

    let home_class = move || {
        if state.active_project.get().is_none() && state.center_view.get() == CenterView::Home {
            "rail-item rail-home active"
        } else {
            "rail-item rail-home"
        }
    };

    let connected = Memo::new(move |_| state_for_connected.active_connection_count() > 0);

    let expanded = RwSignal::new(false);
    let toggle_expanded = move |_| expanded.update(|value| *value = !*value);
    let nav_class = move || {
        if expanded.get() {
            "project-rail expanded"
        } else {
            "project-rail"
        }
    };
    let toggle_title = move || {
        if expanded.get() { "Collapse" } else { "Expand" }
    };

    let on_add_click = move |_| open_project_browser(&state_for_add);

    view! {
        <nav class=nav_class>
            <div class="rail-items">
                <button class="rail-toggle" on:click=toggle_expanded title=toggle_title>
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round">
                        <polyline points="9 18 15 12 9 6"/>
                    </svg>
                </button>

                <button class=home_class on:click=go_home title="Home">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <path d="M3 9l9-7 9 7v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>
                        <polyline points="9 22 9 12 15 12 15 22"/>
                    </svg>
                    <span class="rail-label">"Home"</span>
                </button>

                <div class="rail-divider"></div>

                <For
                    each=move || state_for_hosts.configured_hosts.get()
                    key=|host| host.id.clone()
                    let:host
                >
                    {
                        let state = state.clone();
                        let host_id = host.id.clone();
                        let host_label = host.label.clone();

                        let state_for_status = state.clone();
                        let host_id_for_status = host_id.clone();
                        let status = move || {
                            state_for_status.connection_statuses
                                .get()
                                .get(&host_id_for_status)
                                .cloned()
                                .unwrap_or(crate::state::ConnectionStatus::Disconnected)
                        };

                        let state_for_projects = state.clone();
                        let host_id_for_projects = host_id.clone();
                        let projects_view = move || {
                            let state = state_for_projects.clone();
                            let host_id_filter = host_id_for_projects.clone();
                            state.projects.get()
                                .into_iter()
                                .filter(move |project| project.host_id == host_id_filter)
                                .map(|project_info| {
                                    let state = state.clone();
                                    let host_id = project_info.host_id.clone();
                                    let host_id_for_class = host_id.clone();
                                    let project = project_info.project.clone();
                                    let project_id = project.id.clone();
                                    let project_id_for_class = project_id.clone();
                                    let item_class = move || {
                                        if state.active_project.get().as_ref().is_some_and(|active| {
                                            active.host_id == host_id_for_class && active.project_id == project_id_for_class
                                        }) {
                                            "rail-item rail-project active"
                                        } else {
                                            "rail-item rail-project"
                                        }
                                    };
                                    let on_click = move |_| {
                                        state.switch_active_project(Some(ActiveProjectRef {
                                            host_id: host_id.clone(),
                                            project_id: project_id.clone(),
                                        }));
                                    };
                                    let abbrev = abbreviate(&project.name);
                                    let hue = name_to_hue(&project.name);
                                    let tag_style = format!(
                                        "background: hsl({hue}, 45%, 35%); color: hsl({hue}, 60%, 85%)"
                                    );
                                    let project_name = project.name.clone();
                                    let project_name_for_title = project_name.clone();
                                    view! {
                                        <button class=item_class title=project_name_for_title on:click=on_click>
                                            <span class="rail-tag" style=tag_style>{abbrev}</span>
                                            <span class="rail-label">{project_name.clone()}</span>
                                        </button>
                                    }
                                })
                                .collect_view()
                        };

                        view! {
                            <div class="rail-host-group">
                                <div class="rail-host-label" title=host_label.clone()>
                                    {host_label.clone()}
                                    <span class="rail-host-state">
                                        {move || match status() {
                                            crate::state::ConnectionStatus::Connected => "●",
                                            crate::state::ConnectionStatus::Connecting => "◐",
                                            crate::state::ConnectionStatus::Disconnected => "○",
                                            crate::state::ConnectionStatus::Error(_) => "!",
                                        }}
                                    </span>
                                </div>
                                {projects_view}
                            </div>
                        }
                    }
                </For>
            </div>

            <div class="rail-bottom">
                <button
                    class="rail-item rail-add"
                    title="New Project on Selected Host"
                    on:click=on_add_click
                    disabled=move || !connected.get()
                >
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="12" y1="5" x2="12" y2="19"/>
                        <line x1="5" y1="12" x2="19" y2="12"/>
                    </svg>
                </button>
            </div>
        </nav>
    }
}

fn name_to_hue(name: &str) -> u32 {
    let hash = name.bytes().fold(0u32, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte as u32)
    });
    hash % 360
}

/// Produce a short lowercase tag for a project name.
///
/// - Multi-token names (split on non-alphanumerics) take the first char of each
///   token, up to 3.
/// - Single CamelCase tokens take the uppercase letters (`ThingDoStuff` -> `tds`).
/// - Otherwise fall back to the first 3 characters.
fn abbreviate(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "?".into();
    }

    let tokens: Vec<&str> = trimmed
        .split(|c: char| !c.is_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .collect();

    let raw: String = if tokens.len() > 1 {
        tokens
            .iter()
            .take(3)
            .filter_map(|token| token.chars().next())
            .collect()
    } else if let Some(token) = tokens.first() {
        let caps: String = token.chars().filter(|c| c.is_uppercase()).collect();
        if caps.chars().count() >= 2 {
            caps.chars().take(3).collect()
        } else {
            token.chars().take(3).collect()
        }
    } else {
        trimmed.chars().take(3).collect()
    };

    raw.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::abbreviate;

    #[test]
    fn camel_case_uses_capitals() {
        assert_eq!(abbreviate("ThingDoStuff"), "tds");
        assert_eq!(abbreviate("MyCoolApp"), "mca");
    }

    #[test]
    fn multi_token_uses_initials() {
        assert_eq!(abbreviate("api-gateway"), "ag");
        assert_eq!(abbreviate("web_app_server"), "was");
    }

    #[test]
    fn single_lowercase_uses_prefix() {
        assert_eq!(abbreviate("agentflow"), "age");
        assert_eq!(abbreviate("go"), "go");
    }

    #[test]
    fn single_capital_falls_back_to_prefix() {
        assert_eq!(abbreviate("Anthropic"), "ant");
    }

    #[test]
    fn empty_returns_placeholder() {
        assert_eq!(abbreviate(""), "?");
        assert_eq!(abbreviate("   "), "?");
    }
}
