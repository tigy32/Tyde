use std::collections::{HashMap, HashSet};

use crate::components::teams_view::TeamsView;
use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone, StatusDot, StatusTone,
};
use crate::state::{ActiveAgentRef, AgentInfo, AgentRef, AppState};
use leptos::prelude::*;
use protocol::AgentId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentsSegment {
    Agents,
    Teams,
}

/// Per-host agent list. Status dots carry semantic labels so screen
/// readers (and color-blind users) get the same information sighted
/// users get from color. Empty state guides a first-time user to
/// spawn a chat.
///
/// The segmented control toggles between Agents and Teams — Teams
/// share the same host context, so it's natural to colocate them
/// rather than spend a sixth bottom-nav tab.
#[component]
pub fn AgentsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let segment: RwSignal<AgentsSegment> = RwSignal::new(AgentsSegment::Agents);
    let hide_sub_agents = RwSignal::new(false);
    let collapsed_parents: RwSignal<HashSet<AgentId>> = RwSignal::new(HashSet::new());

    let s_new_chat = state.clone();
    let on_new_chat = Callback::new(move |_: ()| {
        s_new_chat.active_agent.set(None);
        s_new_chat.chat_input.set(String::new());
        s_new_chat.viewing_chat.set(true);
    });

    view! {
        <div class="view agents-view" data-mobile-test="agents-view">
            <header class="view-header">
                <h1 class="view-title">"Agents"</h1>
                <Button
                    label="New chat"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="agents-new-chat"
                    on_click=on_new_chat
                />
            </header>
            <div class="agents-segmented" role="tablist" aria-label="Agents and teams" data-mobile-test="agents-segmented">
                {
                    let on_agents = move |_| segment.set(AgentsSegment::Agents);
                    let on_teams = move |_| segment.set(AgentsSegment::Teams);
                    view! {
                        <button
                            type="button"
                            class="agents-segmented-button"
                            class:active=move || segment.get() == AgentsSegment::Agents
                            role="tab"
                            aria-selected=move || (segment.get() == AgentsSegment::Agents).to_string()
                            data-mobile-test="agents-segment-agents"
                            on:click=on_agents
                        >
                            "Agents"
                        </button>
                        <button
                            type="button"
                            class="agents-segmented-button"
                            class:active=move || segment.get() == AgentsSegment::Teams
                            role="tab"
                            aria-selected=move || (segment.get() == AgentsSegment::Teams).to_string()
                            data-mobile-test="agents-segment-teams"
                            on:click=on_teams
                        >
                            "Teams"
                        </button>
                    }
                }
            </div>
            <div class="view-body">
                {move || {
                    if segment.get() == AgentsSegment::Teams {
                        return view! { <TeamsView /> }.into_any();
                    }
                    render_agents_body(&state, hide_sub_agents, collapsed_parents)
                }}
            </div>
        </div>
    }
}

fn group_agents(agents: Vec<AgentInfo>) -> Vec<(AgentInfo, Vec<AgentInfo>)> {
    let visible_ids: HashSet<AgentId> = agents.iter().map(|agent| agent.agent_id.clone()).collect();
    let mut children_by_parent: HashMap<AgentId, Vec<AgentInfo>> = HashMap::new();
    let mut top_level = Vec::new();
    let mut orphans = Vec::new();

    for agent in agents {
        match &agent.parent_agent_id {
            Some(parent_id) if visible_ids.contains(parent_id) => {
                children_by_parent
                    .entry(parent_id.clone())
                    .or_default()
                    .push(agent);
            }
            Some(_) => orphans.push(agent),
            None => top_level.push(agent),
        }
    }

    let mut grouped = Vec::with_capacity(top_level.len() + orphans.len());
    for parent in top_level {
        let children = children_by_parent
            .remove(&parent.agent_id)
            .unwrap_or_default();
        grouped.push((parent, children));
    }
    for orphan in orphans {
        grouped.push((orphan, Vec::new()));
    }
    grouped
}

fn render_agents_body(
    state: &AppState,
    hide_sub_agents: RwSignal<bool>,
    collapsed_parents: RwSignal<HashSet<AgentId>>,
) -> AnyView {
    let state = state.clone();
    let on_new_chat = {
        let state = state.clone();
        Callback::new(move |_: ()| {
            state.active_agent.set(None);
            state.chat_input.set(String::new());
            state.viewing_chat.set(true);
        })
    };
    let _ = on_new_chat;
    view! {
        <div data-mobile-test="agents-body">
            {move || {
                    let active_host = state.active_local_host_id.get();
                    let agents: Vec<_> = state
                        .agents
                        .get()
                        .into_iter()
                        .filter(|a| {
                            active_host
                                .as_ref()
                                .is_some_and(|h| a.local_host_id == *h)
                        })
                        .collect();

                    if agents.is_empty() {
                        let s_empty = state.clone();
                        let on_cta = Callback::new(move |_: ()| {
                            s_empty.active_agent.set(None);
                            s_empty.chat_input.set(String::new());
                            s_empty.viewing_chat.set(true);
                        });
                        return view! {
                            <EmptyState
                                title="No agents running"
                                body="Tap below to start a new chat. Agents you spawn on your phone show up here and stay in sync with desktop."
                                icon="\u{1F916}"
                                cta_label="Start a chat"
                                cta_test="agents-empty-cta"
                                on_cta=on_cta
                                data_mobile_test="agents-empty"
                            />
                        }.into_any();
                    }

                    let sub_agent_count = agents
                        .iter()
                        .filter(|agent| agent.parent_agent_id.is_some())
                        .count();
                    let visible_agents: Vec<_> = if hide_sub_agents.get() {
                        agents
                            .into_iter()
                            .filter(|agent| agent.parent_agent_id.is_none())
                            .collect()
                    } else {
                        agents
                    };
                    let groups = group_agents(visible_agents);

                    view! {
                        <div>
                            {if sub_agent_count == 0 {
                                view! { <div></div> }.into_any()
                            } else {
                                let toggle_hide = move |_| hide_sub_agents.update(|hidden| *hidden = !*hidden);
                                view! {
                                    <div class="agents-list-controls" data-mobile-test="agents-list-controls">
                                        <span class="agents-subagent-count">
                                            {format!("{sub_agent_count} sub-agent{}", if sub_agent_count == 1 { "" } else { "s" })}
                                        </span>
                                        <button
                                            type="button"
                                            class="agents-filter-toggle"
                                            class:active=move || hide_sub_agents.get()
                                            aria-pressed=move || hide_sub_agents.get().to_string()
                                            data-mobile-test="agents-hide-subagents"
                                            on:click=toggle_hide
                                        >
                                            {move || if hide_sub_agents.get() { "Show sub-agents" } else { "Hide sub-agents" }}
                                        </button>
                                    </div>
                                }.into_any()
                            }}
                            <div class="agent-list" data-mobile-test="agents-list">
                                {groups.into_iter().map(|(parent, children)| {
                                    let parent_id = parent.agent_id.clone();
                                    let child_count = children.len();
                                    let parent_row = agent_row(
                                        &state,
                                        parent,
                                        false,
                                        child_count,
                                        collapsed_parents,
                                    );
                                    let children_visible = !collapsed_parents.with(|collapsed| collapsed.contains(&parent_id));
                                    view! {
                                        <div class="agent-row-group" data-mobile-test="agent-row-group">
                                            {parent_row}
                                            {if children_visible {
                                                children.into_iter().map(|child| {
                                                    agent_row(&state, child, true, 0, collapsed_parents)
                                                }).collect::<Vec<_>>().into_any()
                                            } else {
                                                view! { <div></div> }.into_any()
                                            }}
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        </div>
                    }.into_any()
                }}
        </div>
    }.into_any()
}

fn agent_row(
    state: &AppState,
    agent: AgentInfo,
    indent: bool,
    child_count: usize,
    collapsed_parents: RwSignal<HashSet<AgentId>>,
) -> AnyView {
    let agent_id = agent.agent_id.clone();
    let host_id = agent.local_host_id.clone();
    let name = agent.name.clone();
    let backend = format!("{:?}", agent.backend_kind);
    let is_active = agent.started && agent.fatal_error.is_none();
    let has_error = agent.fatal_error.is_some();
    let error_msg = agent.fatal_error.clone().unwrap_or_default();
    let is_sub = agent.parent_agent_id.is_some();
    let is_side_question = matches!(agent.origin, protocol::AgentOrigin::SideQuestion);
    let agent_ref = AgentRef {
        local_host_id: host_id.clone(),
        agent_id: agent_id.clone(),
    };
    let turn_active = state
        .agent_turn_active
        .with(|m| m.get(&agent_ref).copied().unwrap_or(false));

    let tone = if has_error {
        StatusTone::Error
    } else if turn_active {
        StatusTone::Active
    } else if is_active {
        StatusTone::Online
    } else {
        StatusTone::Muted
    };
    let status_label = if has_error {
        "Error"
    } else if turn_active {
        "Thinking"
    } else if is_active {
        "Idle"
    } else {
        "Stopped"
    };

    let test_selector: &'static str = if has_error {
        "agent-row-error"
    } else if turn_active {
        "agent-row-active"
    } else if is_active {
        "agent-row-idle"
    } else {
        "agent-row-stopped"
    };

    let state_for_click = state.clone();
    let host_id_click = host_id.clone();
    let agent_id_click = agent_id.clone();
    let on_click = Callback::new(move |_: ()| {
        state_for_click.active_agent.set(Some(ActiveAgentRef {
            local_host_id: host_id_click.clone(),
            agent_id: agent_id_click.clone(),
        }));
        state_for_click.viewing_chat.set(true);
    });

    let row_class = if indent {
        "list-row agent-child-row"
    } else {
        "list-row"
    };

    view! {
        <Card
            data_mobile_test=test_selector
            interactive=true
            dense=true
            on_click=on_click
            aria_label=format!("Open chat with {name}")
        >
            <div class=row_class style="border-bottom: none; padding: 0;">
                <StatusDot
                    label=status_label.to_string()
                    tone=tone
                    data_mobile_test="agent-row-status"
                />
                <div class="list-row-primary">
                    <div class="list-row-title">
                        {name}
                    </div>
                    <div class="list-row-subtitle">
                        {backend}
                        {(is_side_question || is_sub).then(|| {
                            // Prefer the compact "BTW" label for side questions
                            // over the generic "Sub-agent" tag — it's both
                            // shorter and more meaningful on a narrow row.
                            let label = if is_side_question { "BTW" } else { "Sub-agent" };
                            view! {
                                <span style="margin-left: var(--space-2);">
                                    <Pill
                                        label=label.to_string()
                                        tone=PillTone::Neutral
                                    />
                                </span>
                            }
                        })}
                    </div>
                    <Show when=move || has_error>
                        <div
                            class="agent-card-error"
                            data-mobile-test="agent-row-error-msg"
                        >
                            {error_msg.clone()}
                        </div>
                    </Show>
                </div>
                {(child_count > 0).then(|| {
                    let agent_id_for_toggle = agent_id.clone();
                    let agent_id_for_icon = agent_id.clone();
                    let on_toggle = move |ev: web_sys::MouseEvent| {
                        ev.stop_propagation();
                        let id = agent_id_for_toggle.clone();
                        collapsed_parents.update(|collapsed| {
                            if collapsed.contains(&id) {
                                collapsed.remove(&id);
                            } else {
                                collapsed.insert(id);
                            }
                        });
                    };
                    view! {
                        <span class="agent-child-controls" data-mobile-test="agent-child-controls">
                            <span class="agent-child-count-badge" data-mobile-test="agent-child-count">
                                {child_count}
                            </span>
                            <button
                                type="button"
                                class="agent-child-collapse"
                                aria-label="Toggle sub-agents"
                                data-mobile-test="agent-child-collapse"
                                on:click=on_toggle
                                on:keydown=|ev: web_sys::KeyboardEvent| ev.stop_propagation()
                            >
                                {move || if collapsed_parents.with(|collapsed| collapsed.contains(&agent_id_for_icon)) {
                                    "\u{25B6}"
                                } else {
                                    "\u{25BE}"
                                }}
                            </button>
                        </span>
                    }
                })}
                <span class="list-row-chevron" aria-hidden="true">"\u{203A}"</span>
            </div>
        </Card>
    }.into_any()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AgentInfo, AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{AgentId, AgentOrigin, BackendKind, StreamPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
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

    fn fixture(host: &LocalHostId, id: &str, name: &str, fatal: Option<&str>) -> AgentInfo {
        AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath(format!("/agent/{id}")),
            started: fatal.is_none(),
            fatal_error: fatal.map(String::from),
        }
    }

    fn child_fixture(host: &LocalHostId, id: &str, name: &str, parent_id: &str) -> AgentInfo {
        let mut agent = fixture(host, id, name, None);
        agent.parent_agent_id = Some(AgentId(parent_id.to_owned()));
        agent
    }

    /// A side question (BTW) fork is shown with the compact "BTW" label
    /// rather than the generic "Sub-agent" tag — both shorter and more
    /// meaningful on a narrow phone row.
    #[wasm_bindgen_test]
    async fn agents_side_question_row_shows_btw_label() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            let parent = fixture(&host_for_mount, "parent", "Parent agent", None);
            let mut btw = child_fixture(&host_for_mount, "btw-1", "Side question", "parent");
            btw.origin = AgentOrigin::SideQuestion;
            state.agents.set(vec![parent, btw]);
            provide_context(state);
            view! { <AgentsView /> }
        });
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("BTW"),
            "side question row must surface the BTW label: {text}"
        );
        assert!(
            !text.contains("Sub-agent"),
            "side question must prefer BTW over the generic Sub-agent tag: {text}"
        );
    }

    /// Empty list renders the dedicated empty state with a CTA, not a
    /// bare list.
    #[wasm_bindgen_test]
    async fn agents_empty_state_renders_when_no_agents() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <AgentsView /> }
        });
        next_tick().await;
        let empty = container
            .query_selector("[data-mobile-test='agents-empty']")
            .unwrap();
        assert!(empty.is_some(), "empty state must render when no agents");
        let cta = container
            .query_selector("[data-mobile-test='agents-empty-cta']")
            .unwrap();
        assert!(
            cta.is_some(),
            "empty state must surface a 'Start a chat' CTA"
        );
    }

    /// Errored agent renders with the error status selector + error
    /// message visible (color isn't the only signal).
    #[wasm_bindgen_test]
    async fn agents_errored_row_surfaces_error_status_and_message() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![fixture(
                &host_for_mount,
                "a-1",
                "Errored agent",
                Some("backend exited non-zero"),
            )]);
            provide_context(state);
            view! { <AgentsView /> }
        });
        next_tick().await;
        let row = container
            .query_selector("[data-mobile-test='agent-row-error']")
            .unwrap()
            .expect("error row must use its semantic test selector");
        let dot = row
            .query_selector("[data-mobile-test='agent-row-status']")
            .unwrap()
            .unwrap();
        assert_eq!(
            dot.get_attribute("aria-label").as_deref(),
            Some("Error"),
            "status dot must carry an accessible label"
        );
        let msg = container
            .query_selector("[data-mobile-test='agent-row-error-msg']")
            .unwrap()
            .expect("fatal error message must be visible on the row");
        assert!(
            msg.text_content()
                .unwrap_or_default()
                .contains("backend exited"),
            "error text must surface the fatal_error string"
        );
    }

    /// Sub-agents are visible by default, but the mobile list exposes a
    /// direct Hide sub-agents control matching the desktop panel's filter.
    #[wasm_bindgen_test]
    async fn agents_hide_subagents_toggle_hides_child_rows() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![
                fixture(&host_for_mount, "parent", "Parent agent", None),
                child_fixture(&host_for_mount, "child", "Child agent", "parent"),
            ]);
            provide_context(state);
            view! { <AgentsView /> }
        });
        next_tick().await;
        let initial_text = container.text_content().unwrap_or_default();
        assert!(
            initial_text.contains("Parent agent") && initial_text.contains("Child agent"),
            "parent and child should be visible before hiding: {initial_text}"
        );

        let toggle: HtmlElement = container
            .query_selector("[data-mobile-test='agents-hide-subagents']")
            .unwrap()
            .expect("hide sub-agents toggle")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;

        let hidden_text = container.text_content().unwrap_or_default();
        assert!(
            hidden_text.contains("Parent agent") && !hidden_text.contains("Child agent"),
            "hiding sub-agents should keep parent and remove child row: {hidden_text}"
        );
        assert!(
            hidden_text.contains("Show sub-agents"),
            "active toggle should offer to show sub-agents again: {hidden_text}"
        );
    }

    /// Parents with visible children get a child count plus a collapse
    /// control so crowded mobile lists can be folded without hiding all
    /// sub-agents globally.
    #[wasm_bindgen_test]
    async fn agents_parent_child_count_can_collapse_children() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![
                fixture(&host_for_mount, "parent", "Parent agent", None),
                child_fixture(&host_for_mount, "child-a", "Child Alpha", "parent"),
                child_fixture(&host_for_mount, "child-b", "Child Beta", "parent"),
            ]);
            provide_context(state);
            view! { <AgentsView /> }
        });
        next_tick().await;

        let count = container
            .query_selector("[data-mobile-test='agent-child-count']")
            .unwrap()
            .expect("child count badge");
        assert_eq!(
            count.text_content().unwrap_or_default().trim(),
            "2",
            "parent badge must show visible child count"
        );
        let expanded_text = container.text_content().unwrap_or_default();
        assert!(
            expanded_text.contains("Child Alpha") && expanded_text.contains("Child Beta"),
            "children should be visible before collapse: {expanded_text}"
        );

        let collapse: HtmlElement = container
            .query_selector("[data-mobile-test='agent-child-collapse']")
            .unwrap()
            .expect("child collapse control")
            .dyn_into()
            .unwrap();
        collapse.click();
        next_tick().await;

        let collapsed_text = container.text_content().unwrap_or_default();
        assert!(
            collapsed_text.contains("Parent agent")
                && !collapsed_text.contains("Child Alpha")
                && !collapsed_text.contains("Child Beta"),
            "collapse should remove child rows but keep parent: {collapsed_text}"
        );
    }
}
