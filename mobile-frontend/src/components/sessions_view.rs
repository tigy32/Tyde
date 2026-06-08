use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone};
use crate::state::{AppState, LocalHostId};

/// Past-conversations browser. Filter-by-search (case-insensitive over
/// alias + backend kind) is the load-bearing affordance on mobile —
/// users scan visually but need typed search when the list grows.
#[component]
pub fn SessionsView() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let search: RwSignal<String> = RwSignal::new(String::new());

    let s_for_filter = state.clone();
    let filtered = Memo::new(move |_| {
        let active_host = s_for_filter.active_local_host_id.get();
        let query = search.get().to_lowercase();
        let query_trimmed = query.trim().to_owned();
        let mut sessions: Vec<_> = s_for_filter
            .sessions
            .get()
            .into_iter()
            .filter(|s| active_host.as_ref().is_some_and(|h| s.local_host_id == *h))
            .collect();
        if !query_trimmed.is_empty() {
            sessions.retain(|s| {
                let title = s
                    .summary
                    .alias
                    .as_deref()
                    .or(s.summary.user_alias.as_deref())
                    .unwrap_or("untitled")
                    .to_lowercase();
                let backend = format!("{:?}", s.summary.backend_kind).to_lowercase();
                title.contains(&query_trimmed) || backend.contains(&query_trimmed)
            });
        }
        sessions.sort_by(|a, b| b.summary.updated_at_ms.cmp(&a.summary.updated_at_ms));
        sessions
    });

    let on_search_input = move |ev: web_sys::Event| {
        use wasm_bindgen::JsCast;
        let target = ev.target().unwrap();
        let input: web_sys::HtmlInputElement = target.unchecked_into();
        search.set(input.value());
    };

    view! {
        <div class="view sessions-view" data-mobile-test="sessions-view">
            <header class="view-header">
                <h1 class="view-title">"Sessions"</h1>
                {move || {
                    let count = filtered.get().len();
                    view! {
                        <Pill
                            label=format!("{count}")
                            tone=PillTone::Neutral
                            data_mobile_test="sessions-count-pill"
                        />
                    }
                }}
            </header>
            <div class="search-bar">
                <input
                    type="search"
                    placeholder="Search sessions"
                    autocapitalize="none"
                    autocomplete="off"
                    spellcheck="false"
                    aria-label="Search sessions"
                    data-mobile-test="sessions-search"
                    prop:value=move || search.get()
                    on:input=on_search_input
                />
            </div>
            <div class="view-body">
                {move || {
                    let sessions = filtered.get();
                    if sessions.is_empty() {
                        if search.get().trim().is_empty() {
                            return view! {
                                <EmptyState
                                    title="No sessions yet"
                                    body="Resumed chats show up here so you can pick up where you left off."
                                    icon="\u{1F4DC}"
                                    data_mobile_test="sessions-empty"
                                />
                            }.into_any();
                        }
                        return view! {
                            <EmptyState
                                title="No matches"
                                body="No sessions match that search. Try a different name or backend."
                                icon="\u{1F50D}"
                                data_mobile_test="sessions-empty-search"
                            />
                        }.into_any();
                    }
                    view! {
                        <div class="session-list" data-mobile-test="sessions-list">
                            {sessions.into_iter().map(|session| {
                                let title = session
                                    .summary
                                    .alias
                                    .clone()
                                    .or_else(|| session.summary.user_alias.clone())
                                    .unwrap_or_else(|| "Untitled".to_string());
                                let backend = format!("{:?}", session.summary.backend_kind);
                                let modified = format_relative_time(session.summary.updated_at_ms);
                                let message_count = session.summary.message_count;
                                let session_id = session.summary.id.clone();
                                let host_id = session.local_host_id.clone();
                                // Compacted sessions surface a small ribbon plus the
                                // compaction summary preview (if present) so users can
                                // tell at a glance which branch this is and what got
                                // collapsed.
                                let compacted_from = session.summary.compacted_from_session_id.is_some();
                                let compacted_to = session.summary.compacted_to_session_id.is_some();
                                let compaction_preview = session
                                    .summary
                                    .compaction_summary_preview
                                    .clone();

                                let s_resume = state.clone();
                                let host_resume = host_id.clone();
                                let sid_resume = session_id.clone();
                                let on_resume = Callback::new(move |_: ()| {
                                    let state = s_resume.clone();
                                    let host = host_resume.clone();
                                    let sid = sid_resume.clone();
                                    spawn_local(async move {
                                        if let Err(e) = resume_session(&state, &host, sid).await {
                                            log::error!("failed to resume session: {e}");
                                        }
                                    });
                                });

                                let s_delete = state.clone();
                                let host_delete = host_id.clone();
                                let sid_delete = session_id.clone();
                                let on_delete = Callback::new(move |_: ()| {
                                    let state = s_delete.clone();
                                    let host = host_delete.clone();
                                    let sid = sid_delete.clone();
                                    spawn_local(async move {
                                        if let Err(e) = delete_session(&state, &host, &sid).await {
                                            log::error!("failed to delete session: {e}");
                                        }
                                    });
                                });

                                view! {
                                    <Card
                                        data_mobile_test="session-row"
                                        dense=true
                                    >
                                        <div class="list-row list-row-flush list-row-flush-top">
                                            <div class="list-row-primary">
                                                <div class="list-row-title">{title}</div>
                                                <div class="list-row-subtitle">
                                                    <span>{backend}</span>
                                                    <span style="margin: 0 var(--space-1);">"\u{00B7}"</span>
                                                    <span>{format!("{message_count} msgs")}</span>
                                                </div>
                                            </div>
                                            <div class="list-row-meta">
                                                <span>{modified}</span>
                                            </div>
                                        </div>
                                        {if compacted_from || compacted_to {
                                            let (label, test) = if compacted_to {
                                                ("Compacted into a newer session", "session-compacted-to")
                                            } else {
                                                ("Compacted from an earlier session", "session-compacted-from")
                                            };
                                            view! {
                                                <div class="session-compaction-ribbon" data-mobile-test=test>
                                                    <span class="session-compaction-ribbon-icon" aria-hidden="true">"\u{2702}"</span>
                                                    <span>{label}</span>
                                                </div>
                                            }.into_any()
                                        } else {
                                            view! { <div></div> }.into_any()
                                        }}
                                        {compaction_preview.map(|preview| view! {
                                            <div class="session-compaction-summary" data-mobile-test="session-compaction-summary">
                                                {preview}
                                            </div>
                                        })}
                                        <div style="display: flex; gap: var(--space-2); margin-top: var(--space-3);">
                                            <Button
                                                label="Resume"
                                                variant=ButtonVariant::Primary
                                                size=ButtonSize::Compact
                                                data_mobile_test="session-resume"
                                                on_click=on_resume
                                            />
                                            <Button
                                                label="Delete"
                                                variant=ButtonVariant::Destructive
                                                size=ButtonSize::Compact
                                                data_mobile_test="session-delete"
                                                on_click=on_delete
                                            />
                                        </div>
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

fn format_relative_time(timestamp_ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    let diff_ms = now.saturating_sub(timestamp_ms);
    let minutes = diff_ms / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;

    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else {
        format!("{days}d ago")
    }
}

async fn resume_session(
    state: &AppState,
    host_id: &LocalHostId,
    session_id: protocol::SessionId,
) -> Result<(), String> {
    let host_stream = state
        .host_stream_untracked(host_id)
        .ok_or("no host stream")?;

    let active_project = state.active_project.get_untracked();
    let payload = protocol::SpawnAgentPayload {
        name: None,
        custom_agent_id: None,
        parent_agent_id: None,
        project_id: active_project.map(|ap| ap.project_id),
        params: protocol::SpawnAgentParams::Resume {
            session_id,
            prompt: None,
        },
    };

    crate::send::send_frame(
        host_id,
        host_stream,
        protocol::FrameKind::SpawnAgent,
        &payload,
    )
    .await
}

async fn delete_session(
    state: &AppState,
    host_id: &LocalHostId,
    session_id: &protocol::SessionId,
) -> Result<(), String> {
    let host_stream = state
        .host_stream_untracked(host_id)
        .ok_or("no host stream")?;

    crate::send::send_frame(
        host_id,
        host_stream,
        protocol::FrameKind::DeleteSession,
        &protocol::DeleteSessionPayload {
            session_id: session_id.clone(),
        },
    )
    .await?;

    Ok(())
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId, SessionInfo};
    use leptos::mount::mount_to;
    use protocol::{BackendKind, SessionId, SessionSummary};
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

    fn compacted_fixture(
        host: &LocalHostId,
        id: &str,
        alias: &str,
        compacted_to: Option<&str>,
        compacted_from: Option<&str>,
        preview: Option<&str>,
    ) -> SessionInfo {
        let mut s = fixture(host, id, alias);
        s.summary.compacted_to_session_id = compacted_to.map(|x| SessionId(x.to_owned()));
        s.summary.compacted_from_session_id = compacted_from.map(|x| SessionId(x.to_owned()));
        s.summary.compaction_summary_preview = preview.map(|p| p.to_owned());
        s
    }

    fn fixture(host: &LocalHostId, id: &str, alias: &str) -> SessionInfo {
        SessionInfo {
            local_host_id: host.clone(),
            summary: SessionSummary {
                id: SessionId(id.to_owned()),
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                alias: Some(alias.to_owned()),
                user_alias: None,
                parent_id: None,
                created_at_ms: 0,
                updated_at_ms: 0,
                message_count: 0,
                token_count: None,
                resumable: true,
                compacted_from_session_id: None,
                compacted_to_session_id: None,
                compacted_at_ms: None,
                compaction_summary_preview: None,
            },
        }
    }

    /// Empty list (no sessions, no query) shows the "No sessions yet"
    /// state with its specific selector — distinct from the no-search-
    /// match state so callers can distinguish the two cases.
    #[wasm_bindgen_test]
    async fn sessions_empty_state_renders_when_no_sessions() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            provide_context(state);
            view! { <SessionsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='sessions-empty']")
                .unwrap()
                .is_some()
        );
    }

    /// Typing a query filters the list and renders the search-empty
    /// state when nothing matches. Asserts on user-visible text content
    /// rather than querySelectorAll (web-sys NodeList feature isn't on).
    #[wasm_bindgen_test]
    async fn sessions_search_filters_list_and_shows_search_empty() {
        let host = LocalHostId("host-1".to_owned());
        let container = make_container();
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.sessions.set(vec![
                fixture(&host_for_mount, "s-1", "Code review"),
                fixture(&host_for_mount, "s-2", "Onboarding draft"),
            ]);
            provide_context(state);
            view! { <SessionsView /> }
        });
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Code review") && text.contains("Onboarding draft"),
            "both rows must be visible initially: {text}"
        );

        // Type a query that matches one row.
        let input: web_sys::HtmlInputElement = container
            .query_selector("[data-mobile-test='sessions-search']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("code");
        let ev = web_sys::Event::new("input").unwrap();
        input.dispatch_event(&ev).unwrap();
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Code review"), "matching row must remain");
        assert!(
            !text.contains("Onboarding draft"),
            "non-matching row must filter out"
        );

        // Type a query that matches nothing.
        input.set_value("zzzz-no-match");
        let ev = web_sys::Event::new("input").unwrap();
        input.dispatch_event(&ev).unwrap();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='sessions-empty-search']")
                .unwrap()
                .is_some()
        );
    }

    /// Compacted sessions surface a ribbon so users can distinguish
    /// "this session was compacted into a newer one" (avoid resuming —
    /// you'll fork history) from "this session was compacted from an
    /// older one" (the canonical current head). Plus, when a summary
    /// preview is present, it should render so users can see what was
    /// rolled up.
    #[wasm_bindgen_test]
    async fn sessions_compaction_ribbon_and_summary_render() {
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.sessions.set(vec![
                compacted_fixture(
                    &host_for_mount,
                    "s-old",
                    "Old (compacted forward)",
                    Some("s-new"),
                    None,
                    Some("Summary of older context"),
                ),
                compacted_fixture(
                    &host_for_mount,
                    "s-new",
                    "Continuation",
                    None,
                    Some("s-old"),
                    None,
                ),
            ]);
            provide_context(state);
            view! { <SessionsView /> }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='session-compacted-to']")
                .unwrap()
                .is_some(),
            "compacted-forward session must surface its ribbon"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='session-compacted-from']")
                .unwrap()
                .is_some(),
            "continuation session must surface its 'compacted from' ribbon"
        );
        let summary = container
            .query_selector("[data-mobile-test='session-compaction-summary']")
            .unwrap()
            .expect("compaction summary preview must render when present");
        assert!(
            summary
                .text_content()
                .unwrap_or_default()
                .contains("Summary of older context"),
            "summary preview text must render"
        );
    }
}
