use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    BackendKind, DeleteSessionPayload, FrameKind, ListSessionsPayload, SessionListPageStatus,
    StreamPath,
};

use crate::actions::resume_session;
use crate::send::send_frame;
use std::collections::HashSet;

use crate::state::{
    ActiveProjectRef, AppState, ConnectionStatus, SessionInfo, SessionsPanelFilters,
};

fn backend_class(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "backend-badge tycode",
        BackendKind::Acp => "backend-badge kiro",
        BackendKind::Claude => "backend-badge claude",
        BackendKind::Codex => "backend-badge codex",
        BackendKind::Antigravity => "backend-badge antigravity",
        BackendKind::Hermes => "backend-badge hermes",
    }
}

fn backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Acp => "ACP",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
    }
}

/// `pub(crate)` so DOM tests can assert on the exact rendered date rather than
/// reimplementing the formatting and drifting from it.
pub(crate) fn format_date(ms: u64) -> String {
    let date = js_sys::Date::new_0();
    date.set_time(ms as f64);
    let month = date.get_month() + 1;
    let day = date.get_date();
    let year = date.get_full_year();
    let hours = date.get_hours();
    let mins = date.get_minutes();
    format!("{year}-{month:02}-{day:02} {hours:02}:{mins:02}")
}

fn last_path_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn session_title(s: &SessionInfo) -> String {
    if let Some(ref ua) = s.summary.user_alias
        && !ua.is_empty()
    {
        return ua.clone();
    }
    if let Some(ref a) = s.summary.alias
        && !a.is_empty()
    {
        return a.clone();
    }
    let id_str = s.summary.id.0.clone();
    id_str.chars().take(50).collect()
}

fn session_id_short(s: &SessionInfo) -> String {
    s.summary.id.0.chars().take(8).collect()
}

/// Pure predicate used by the Sessions/History panel filter memo. Extracted
/// so the filter behavior can be unit-tested without a Leptos runtime.
pub fn session_passes_filters(
    session: &SessionInfo,
    filters: &SessionsPanelFilters,
    active_project: Option<&ActiveProjectRef>,
    lowercase_query: &str,
) -> bool {
    if !filters.show_child_sessions && session.summary.parent_id.is_some() {
        return false;
    }
    if !filters.show_other_projects {
        let matches = match active_project {
            None => session.summary.project_id.is_none(),
            Some(ap) => {
                session.host_id == ap.host_id
                    && session.summary.project_id.as_ref() == Some(&ap.project_id)
            }
        };
        if !matches {
            return false;
        }
    }
    if !lowercase_query.is_empty() {
        let title = session_title(session).to_lowercase();
        let workspace_match = session
            .summary
            .workspace_roots
            .iter()
            .any(|w| w.to_lowercase().contains(lowercase_query));
        let backend_match = backend_label(session.summary.backend_kind)
            .to_lowercase()
            .contains(lowercase_query);
        if !title.contains(lowercase_query) && !workspace_match && !backend_match {
            return false;
        }
    }
    true
}

#[component]
pub fn SessionsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let search = RwSignal::new(String::new());

    // Per-project filter values. Falls back to context-aware defaults when
    // the user hasn't toggled anything yet for this project.
    let filters_state = state.clone();
    let current_filters = Memo::new(move |_| {
        let active = filters_state.active_project.get();
        let overrides = filters_state.sessions_panel_filters.get();
        overrides
            .get(&active)
            .cloned()
            .unwrap_or_else(|| SessionsPanelFilters::defaults_for(active.as_ref()))
    });

    let update_filters = {
        let state = state.clone();
        move |mutate: Box<dyn FnOnce(&mut SessionsPanelFilters)>| {
            let active = state.active_project.get_untracked();
            state.sessions_panel_filters.update(|map| {
                let entry = map
                    .entry(active.clone())
                    .or_insert_with(|| SessionsPanelFilters::defaults_for(active.as_ref()));
                mutate(entry);
            });
        }
    };

    let filter_state = state.clone();
    let filtered_sessions = Memo::new(move |_| {
        let active_project = filter_state.active_project.get();
        let query = search.get().to_lowercase();
        let filters = current_filters.get();

        filter_state.sessions.with(|sessions| {
            sessions
                .iter()
                .filter(|s| session_passes_filters(s, &filters, active_project.as_ref(), &query))
                .cloned()
                .collect::<Vec<_>>()
        })
    });

    let on_search = move |ev: leptos::ev::Event| {
        let val = event_target_value(&ev);
        search.set(val);
    };

    let toggle_children = move |_| {
        update_filters(Box::new(|f: &mut SessionsPanelFilters| {
            f.show_child_sessions = !f.show_child_sessions;
        }));
    };

    let toggle_other_projects = move |_| {
        update_filters(Box::new(|f: &mut SessionsPanelFilters| {
            f.show_other_projects = !f.show_other_projects;
        }));
    };

    // Hosts the panel is responsible for: those whose rows are rendered, plus
    // those with pages still unfetched. A host in the second group has no cards
    // on screen yet its history is part of this view, so leaving it out is what
    // made later pages unreachable.
    let rendered_hosts = {
        let state = state.clone();
        Memo::new(move |_| {
            let active_project = state.active_project.get();
            let filters = current_filters.get();

            // Scope first. A project-scoped view can only ever show sessions on
            // that project's host, so that host *is* the responsible set —
            // whatever other hosts happen to have unfetched pages. Deriving the
            // set from page state first let an unrelated host displace the
            // project the user is looking at whenever no local row matched.
            if !filters.show_other_projects
                && let Some(project) = active_project.as_ref()
            {
                return vec![project.host_id.clone()];
            }

            // Unscoped (Home, or "show all projects"): every host that holds
            // rows or still has history to fetch. Derived from the held rows
            // rather than the filtered ones, so a search matching nothing does
            // not silently drop a host from its own view.
            let mut hosts: Vec<String> = state
                .sessions
                .with(|sessions| sessions.iter().map(|s| s.host_id.clone()).collect());
            state.session_list_pages.with(|pages| {
                for ((host_id, _), page) in pages.iter() {
                    if matches!(page.status, SessionListPageStatus::More { .. }) {
                        hosts.push(host_id.clone());
                    }
                }
            });
            if hosts.is_empty()
                && let Some(host_id) = active_project
                    .map(|project| project.host_id)
                    .or_else(|| state.selected_host_id.get())
            {
                hosts.push(host_id);
            }
            hosts.sort();
            hosts.dedup();
            hosts
        })
    };

    // The hosts this panel is responsible for, each paired with the connection
    // it is currently reachable on.
    //
    // Pairing with the live `StreamPath` — rather than tracking host IDs alone
    // — is what makes the request effect below correct on the real startup
    // order. `app.rs` awaits `refresh_configured_hosts`, which publishes
    // `selected_host_id`, and only *afterwards* loops into `connect_one_host`,
    // which inserts the stream. A host-ID-only dependency therefore settles
    // while the host is still unreachable, and — `rendered_hosts` being a memo
    // whose value does not change when the stream appears — is never woken
    // again. Reading `host_streams` here makes connecting a host an observable
    // event, and makes reconnecting one a *different* observable event.
    let connected_hosts = {
        let state = state.clone();
        Memo::new(move |_| {
            let hosts = rendered_hosts.get();
            state.host_streams.with(|streams| {
                hosts
                    .into_iter()
                    .filter_map(|host_id| {
                        streams
                            .get(&host_id)
                            .map(|stream| (host_id, stream.clone()))
                    })
                    .collect::<Vec<(String, StreamPath)>>()
            })
        })
    };

    // Ask every host this panel is showing for its authoritative first page.
    //
    // Sending only to `selected_host_id` refreshed whichever host Settings
    // happened to have selected, which in project A with host B selected
    // updated B while the user watched A — and made History look stale or
    // current depending on an unrelated choice.
    //
    // Everything the send needs is resolved *before* spawning: reading a signal
    // after an await means reading it after the panel may have unmounted, and a
    // disposed signal panics rather than returning `None`.
    fn request_authoritative_pages(
        state: &AppState,
        targets: &[(String, StreamPath)],
        force: bool,
        requested: RwSignal<HashSet<(String, StreamPath)>>,
    ) {
        let requests: Vec<(String, StreamPath, ListSessionsPayload)> = targets
            .iter()
            .filter_map(|(host_id, host_stream)| {
                // Automatic requests take one slot per host, so a burst of
                // reasons to refresh cannot become a burst of requests. An
                // explicit Refresh always sends: a user who presses it and gets
                // nothing has been silently ignored.
                let claimed = state
                    .session_list_refresh_in_flight
                    .try_update(|hosts| hosts.insert(host_id.clone()))
                    .unwrap_or(false);
                if !claimed && !force {
                    return None;
                }
                // Ask for the scope/limit this host's view is actually using.
                let payload = state.session_list_pages.with_untracked(|pages| {
                    pages
                        .iter()
                        .find(|((page_host, _), _)| page_host == host_id)
                        .map(|((_, _), page)| ListSessionsPayload {
                            scope: Some(page.scope),
                            cursor: None,
                            limit: Some(page.limit),
                        })
                        .unwrap_or_default()
                });
                Some((host_id.clone(), host_stream.clone(), payload))
            })
            .collect();
        for (host_id, host_stream, payload) in requests {
            let refresh_flag = state.session_list_refresh_in_flight;
            // Claim the connection now that a frame is genuinely going out.
            // Claiming earlier — while merely *considering* a host — burned the
            // one-shot on hosts that were then never asked at all.
            let claim = (host_id.clone(), host_stream.clone());
            let _ = requested.try_update(|seen| {
                seen.insert(claim);
            });
            spawn_local(async move {
                if let Err(e) =
                    send_frame(&host_id, host_stream, FrameKind::ListSessions, &payload).await
                {
                    log::error!(
                        "failed to send ListSessions to {host_id}: {e}; waiting for reconnect"
                    );
                    // Release the *host-level* gate so teardown and an explicit
                    // Refresh are not wedged behind a request that will never
                    // be answered.
                    //
                    // The connection claim deliberately stays. A failed send
                    // here means this stream is already dead — `router.rs`
                    // `send_line` fails only when the host is absent from the
                    // registry or its writer task has exited, and there is no
                    // backpressure path on its unbounded channel. `send.rs`
                    // has by then consumed a sequence number the server never
                    // saw, so any later frame on this stream trips the server's
                    // `SeqValidator`. Worse, the router routes by `host_id`: a
                    // frame carrying the dead `StreamPath` could land on a
                    // replacement transport and tear down the very connection
                    // that would have recovered. Making the tuple eligible
                    // again buys nothing and risks that. The recovery is the
                    // disconnect this failure implies, which drops the stream
                    // and reconnects with a new `StreamPath` — a new target,
                    // which gets its own single request.
                    let _ = refresh_flag.try_update(|hosts| {
                        hosts.remove(&host_id);
                    });
                }
            });
        }
    }

    // History must not show whatever the bootstrap happened to leave behind.
    // Bootstrap counts are a snapshot from connect time, and the live run
    // showed a session sitting at `0 responses` after four completed turns
    // because nothing on this path ever asked for anything newer. This request
    // is also what establishes the server-side session-summary subscription,
    // so subsequent turn updates arrive without a manual Refresh.
    //
    // Not gated on the panel being visible, and deliberately so: `RightDock`
    // mounts every panel permanently and hides them with `display: none`, and
    // the subscription this request establishes has to exist *before* the
    // turns whose counts it carries. Waiting for the user to look at History
    // would drop every update sent before they did.
    //
    // Requested once per host connection, whatever the outcome. The one-shot is
    // keyed by `(host_id, StreamPath)` so that a reconnect — a genuinely new
    // server-side stream with no subscription on it — is asked in its own
    // right, while neither the page this request produces nor a failed send is
    // a reason to ask the same connection twice. The next automatic request
    // happens when the connection identity changes; see the failure branch
    // above for why the transport cannot answer a second attempt on a dead one.
    let requested_connections: RwSignal<HashSet<(String, StreamPath)>> =
        RwSignal::new(HashSet::new());
    let state_for_request = state.clone();
    Effect::new(move |_| {
        let targets = connected_hosts.get();
        // Untracked: the claims recorded by the dispatch below must not be a
        // dependency of the effect that records them, or the panel would
        // request itself in a loop.
        let fresh: Vec<(String, StreamPath)> = requested_connections
            .try_with_untracked(|requested| {
                targets
                    .into_iter()
                    .filter(|target| !requested.contains(target))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if fresh.is_empty() {
            return;
        }
        request_authoritative_pages(&state_for_request, &fresh, false, requested_connections);
    });

    let state_for_refresh = state.clone();
    let on_refresh = move |_| {
        // Explicit Refresh consults no claim: a user who presses it and gets
        // nothing has been silently ignored.
        request_authoritative_pages(
            &state_for_refresh,
            &connected_hosts.get_untracked(),
            true,
            requested_connections,
        );
    };

    view! {
        <div class="panel sessions-panel">
            <div class="panel-search">
                <input
                    type="text"
                    class="panel-search-input"
                    placeholder="Filter sessions..."
                    prop:value=search
                    on:input=on_search
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
            </div>
            <div class="panel-filters">
                <button
                    class=move || if current_filters.get().show_child_sessions { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_children
                >
                    "Show sub-agents"
                </button>
                <button
                    class=move || if current_filters.get().show_other_projects { "filter-toggle active" } else { "filter-toggle" }
                    on:click=toggle_other_projects
                >
                    "Show all projects"
                </button>
                <button
                    class="filter-toggle refresh-btn"
                    data-test="sessions-refresh"
                    on:click=on_refresh
                >
                    "Refresh"
                </button>
            </div>
            <div class="panel-content">
                {move || {
                    let sessions = filtered_sessions.get();
                    if sessions.is_empty() {
                        let msg = if search.get().is_empty() {
                            "No saved sessions"
                        } else {
                            "No matching sessions"
                        };
                        // Still offer the continuation: a filter that matches
                        // nothing on the pages fetched so far says nothing
                        // about the pages that have not been.
                        view! {
                            <div class="panel-empty">{msg}</div>
                            {load_more_buttons(state.clone(), rendered_hosts)}
                        }.into_any()
                    } else {
                        view! {
                            <div class="session-card-list">
                                {sessions.into_iter().map(|session| {
                                    session_card(state.clone(), session)
                                }).collect_view()}
                                {load_more_buttons(state.clone(), rendered_hosts)}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}

/// One "Load more" per rendered host that still has unfetched history.
///
/// Previously this picked a single host, so in a multi-host view — Home, or
/// "show all projects" — every other host's later pages were unreachable no
/// matter how much history existed. Each control carries the exact stored
/// cursor, scope and limit for its own host, which is also what the reducer
/// requires before it will append rather than ignore the response.
fn load_more_buttons(state: AppState, hosts: Memo<Vec<String>>) -> impl IntoView {
    let pages_state = state.clone();
    let continuations = Memo::new(move |_| {
        let hosts = hosts.get();
        pages_state.session_list_pages.with(|pages| {
            hosts
                .iter()
                .filter_map(|host_id| {
                    pages
                        .iter()
                        .find(|((page_host, _), _)| page_host == host_id)
                        .and_then(|((_, _), page)| match page.status {
                            SessionListPageStatus::More { next_cursor } => {
                                Some((host_id.clone(), page.scope, next_cursor, page.limit))
                            }
                            SessionListPageStatus::Complete => None,
                        })
                })
                .collect::<Vec<_>>()
        })
    });

    move || {
        let entries = continuations.get();
        // Only name the host when there is more than one control to tell apart.
        let label_hosts = entries.len() > 1;
        let host_labels = state.configured_hosts.get();
        entries
            .into_iter()
            .map(|(host_id, scope, cursor, limit)| {
                let label = if label_hosts {
                    let name = host_labels
                        .iter()
                        .find(|host| host.id == host_id)
                        .map(|host| host.label.clone())
                        .unwrap_or_else(|| host_id.clone());
                    format!("Load more from {name}")
                } else {
                    "Load more".to_owned()
                };
                let state = state.clone();
                let click_host = host_id.clone();
                let on_click = move |_| {
                    let host_id = click_host.clone();
                    // Resolved before the spawn: after an await the panel may be
                    // gone, and reading a disposed signal panics.
                    let Some(host_stream) = state.host_stream_untracked(&host_id) else {
                        return;
                    };
                    spawn_local(async move {
                        if let Err(error) = send_frame(
                            &host_id,
                            host_stream,
                            FrameKind::ListSessions,
                            &ListSessionsPayload {
                                scope: Some(scope),
                                cursor: Some(cursor),
                                limit: Some(limit),
                            },
                        )
                        .await
                        {
                            log::error!("failed to request more sessions: {error}");
                        }
                    });
                };
                view! {
                    <button
                        type="button"
                        class="session-load-more"
                        data-test="session-load-more"
                        data-host=host_id.clone()
                        on:click=on_click
                    >
                        {label}
                    </button>
                }
            })
            .collect_view()
    }
}

/// Singular/plural label for the per-session response counter.
///
/// "Responses", not "completed turns": the store increments on every
/// `StreamEnd`, and cancelled and protocol-failed streams emit one too. Calling
/// those completed would present an abandoned partial answer as a finished one.
fn format_turn_count(turns: u32) -> String {
    if turns == 1 {
        "1 response".to_owned()
    } else {
        format!("{turns} responses")
    }
}

fn session_card(state: AppState, session: SessionInfo) -> impl IntoView {
    let title = session_title(&session);
    let short_id = session_id_short(&session);
    let full_id = session.summary.id.0.clone();
    let backend = session.summary.backend_kind;
    let last_active = format_date(session.summary.updated_at_ms);
    let workspace = session
        .summary
        .workspace_roots
        .first()
        .map(|w| last_path_component(w).to_string())
        .unwrap_or_default();
    // The store increments this once per persisted assistant stream (a
    // StreamEnd), never for the user's own message, so it counts responses and
    // not messages. Labelled for what it is rather than inheriting a name that
    // overstates it.
    let turn_count = session.summary.message_count;
    let session_id = session.summary.id.clone();
    let resumable = session.summary.resumable;
    let session_host_id = session.host_id.clone();
    let session_project_id = session.summary.project_id.clone();

    // Per-row connection status keyed on this session's host, not the selected host.
    let host_id_for_connected = session_host_id.clone();
    let state_for_connected = state.clone();
    let is_connected = Memo::new(move |_| {
        state_for_connected
            .connection_statuses
            .get()
            .get(&host_id_for_connected)
            .is_some_and(|s| matches!(s, ConnectionStatus::Connected))
    });

    // Reactive project name: resolve the session's `project_id` against
    // `state.projects` at render time so a rename (which updates
    // `state.projects` via `ProjectNotify`) immediately re-renders this
    // badge. Sessions without a project_id, or whose project is no longer
    // in `state.projects`, render no badge.
    let project_state = state.clone();
    let project_host_for_name = session_host_id.clone();
    let project_id_for_name = session_project_id.clone();
    let project_name = move || {
        let pid = project_id_for_name.as_ref()?;
        project_state.projects.with(|projects| {
            projects
                .iter()
                .find(|p| p.host_id == project_host_for_name && &p.project.id == pid)
                .map(|p| p.project.name.clone())
        })
    };

    // Clone before closures move session_id, session_host_id, and state.
    let delete_host_id = session_host_id.clone();
    let delete_session_id = session_id.clone();
    let state_for_delete = state.clone();

    // Shared resume action used by both click and keydown handlers.
    let resume_state = state.clone();
    let resume_sid = session_id.clone();
    let resume_host = session_host_id.clone();
    let resume_project_id = session_project_id.clone();
    let do_resume = std::rc::Rc::new(move || {
        resume_session(
            &resume_state,
            resume_host.clone(),
            backend,
            resume_sid.clone(),
            resume_project_id.clone(),
        );
    });

    let do_resume2 = do_resume.clone();
    let on_click = move |_: web_sys::MouseEvent| {
        if !is_connected.get() || !resumable {
            return;
        }
        do_resume();
    };

    let on_keydown_card = move |ev: web_sys::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Enter" | " ") {
            ev.prevent_default();
            if !is_connected.get() || !resumable {
                return;
            }
            do_resume2();
        }
    };

    let disabled_class = move || {
        if !is_connected.get() || !resumable {
            "session-card disabled"
        } else {
            "session-card"
        }
    };

    view! {
        <div
            class=disabled_class
            title=full_id
            tabindex="0"
            role="button"
            on:click=on_click
            on:keydown=on_keydown_card
        >
            <div class="session-card-top">
                <span class="session-card-title">{title}</span>
                <div>
                    {move || {
                        if !is_connected.get() {
                            return None;
                        }
                        // Create the handler fresh each time so the move closure
                        // doesn't exhaust its captured values across invocations.
                        let state = state_for_delete.clone();
                        let host_id = delete_host_id.clone();
                        let sid = delete_session_id.clone();
                        let on_delete = move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation();
                            let state = state.clone();
                            let host_id = host_id.clone();
                            let sid = sid.clone();
                            spawn_local(async move {
                                if let Some(host_stream) = state.host_stream_untracked(&host_id)
                                    && let Err(e) = send_frame(
                                        &host_id,
                                        host_stream,
                                        FrameKind::DeleteSession,
                                        &DeleteSessionPayload { session_id: sid },
                                    )
                                    .await
                                {
                                    log::error!("failed to send DeleteSession: {e}");
                                }
                            });
                        };
                        Some(view! {
                            <button type="button" class="filter-toggle" on:click=on_delete>
                                "Delete"
                            </button>
                        })
                    }}
                    <span class={backend_class(backend)}>{backend_label(backend)}</span>
                </div>
            </div>
            <div class="session-card-meta">
                <span class="session-card-date" title="Last active">{last_active}</span>
                {move || project_name().map(|n| view! {
                    <span class="session-card-project">{n}</span>
                })}
                {(!workspace.is_empty()).then(|| view! {
                    <span class="session-card-workspace">{workspace}</span>
                })}
                <span
                    class="session-card-msgs"
                    title="Assistant responses persisted for this session, including \
                           any that were cancelled or failed part-way"
                >{format_turn_count(turn_count)}</span>
            </div>
            <div class="session-card-id">{short_id}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionInfo;
    use protocol::{BackendKind, ProjectId, SessionId, SessionSummary};

    /// The badge must describe the counter it renders, and the counter is not
    /// "messages" and not "completed turns".
    ///
    /// Evidence for both halves: `apply_runtime_session_updates` bumps the
    /// stored count once per `ChatEvent::StreamEnd` and never for the user's
    /// own message — so "messages" overcounts nothing and undercounts the
    /// user's side. And Hermes reaches `finish_stream_events`, which emits a
    /// `StreamEnd`, on its cancellation and protocol-failure paths too — so a
    /// cancelled or part-way-failed response is counted, and calling those
    /// "completed" presents an abandoned answer as a finished one.
    ///
    /// This assertion previously read "turns", from before that second point
    /// was established; the label was corrected in the same change that
    /// documented it, and the test is corrected here to match. The contract it
    /// was reaching for is preserved and widened: the label must not overstate
    /// the counter in *either* direction.
    #[test]
    fn count_badge_does_not_overstate_the_counter() {
        assert_eq!(format_turn_count(0), "0 responses");
        assert_eq!(
            format_turn_count(1),
            "1 response",
            "singular reads naturally"
        );
        assert_eq!(format_turn_count(4), "4 responses");
        for count in [0_u32, 1, 4] {
            let label = format_turn_count(count);
            assert!(
                !label.contains("msg") && !label.contains("message"),
                "the counter never counts the user's message, so the badge must \
                 not call these messages: {label}"
            );
            assert!(
                !label.contains("turn"),
                "a cancelled or failed stream is counted too, so the badge must \
                 not claim completed turns: {label}"
            );
        }
    }

    fn mk_session(
        id: &str,
        host: &str,
        project_id: Option<&str>,
        parent: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            host_id: host.to_string(),
            summary: SessionSummary {
                id: SessionId(id.to_string()),
                backend_kind: BackendKind::Tycode,
                launch_profile_id: None,
                workspace_roots: vec![],
                project_id: project_id.map(|s| ProjectId(s.to_string())),
                alias: None,
                user_alias: None,
                parent_id: parent.map(|p| SessionId(p.to_string())),
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

    fn active(host: &str, project: &str) -> ActiveProjectRef {
        ActiveProjectRef {
            host_id: host.to_string(),
            project_id: ProjectId(project.to_string()),
        }
    }

    #[test]
    fn defaults_for_home_shows_other_projects_true() {
        assert!(SessionsPanelFilters::defaults_for(None).show_other_projects);
    }

    #[test]
    fn defaults_for_specific_project_shows_other_projects_false() {
        let ap = active("h", "p");
        assert!(!SessionsPanelFilters::defaults_for(Some(&ap)).show_other_projects);
    }

    #[test]
    fn defaults_hide_child_sessions_by_default() {
        assert!(!SessionsPanelFilters::defaults_for(None).show_child_sessions);
        let ap = active("h", "p");
        assert!(!SessionsPanelFilters::defaults_for(Some(&ap)).show_child_sessions);
    }

    #[test]
    fn child_sessions_hidden_unless_toggled_on() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let parent = mk_session("p", "h", Some("proj"), None);
        let child = mk_session("c", "h", Some("proj"), Some("p"));
        assert!(session_passes_filters(
            &parent,
            &filters,
            Some(&active("h", "proj")),
            ""
        ));
        assert!(!session_passes_filters(
            &child,
            &filters,
            Some(&active("h", "proj")),
            ""
        ));

        let allow_children = SessionsPanelFilters {
            show_child_sessions: true,
            show_other_projects: true,
        };
        assert!(session_passes_filters(
            &child,
            &allow_children,
            Some(&active("h", "proj")),
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_on_home_keeps_only_none_project() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: false,
        };
        let home_session = mk_session("home", "h", None, None);
        let project_session = mk_session("proj", "h", Some("p1"), None);
        assert!(session_passes_filters(&home_session, &filters, None, ""));
        assert!(!session_passes_filters(
            &project_session,
            &filters,
            None,
            ""
        ));
    }

    #[test]
    fn show_other_projects_off_in_project_requires_host_and_project_match() {
        let filters = SessionsPanelFilters::defaults_for(Some(&active("h1", "p1")));
        assert!(!filters.show_other_projects);

        let same = mk_session("same", "h1", Some("p1"), None);
        let other_project = mk_session("other_p", "h1", Some("p2"), None);
        let other_host = mk_session("other_h", "h2", Some("p1"), None);
        let home_session = mk_session("home", "h1", None, None);
        let active_ref = active("h1", "p1");
        assert!(session_passes_filters(
            &same,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(!session_passes_filters(
            &home_session,
            &filters,
            Some(&active_ref),
            ""
        ));
    }

    #[test]
    fn show_other_projects_on_bypasses_project_check() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let other_project = mk_session("other_p", "h1", Some("p2"), None);
        let other_host = mk_session("other_h", "h2", Some("p1"), None);
        let home_session = mk_session("home", "h1", None, None);
        let active_ref = active("h1", "p1");
        assert!(session_passes_filters(
            &other_project,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(session_passes_filters(
            &other_host,
            &filters,
            Some(&active_ref),
            ""
        ));
        assert!(session_passes_filters(
            &home_session,
            &filters,
            Some(&active_ref),
            ""
        ));
    }

    #[test]
    fn search_matches_alias_workspace_and_backend_case_insensitively() {
        let filters = SessionsPanelFilters {
            show_child_sessions: false,
            show_other_projects: true,
        };
        let mut s = mk_session("id", "h", None, None);
        s.summary.user_alias = Some("My Cool Chat".to_string());
        s.summary.workspace_roots = vec!["/Users/me/Projects/foo".to_string()];
        s.summary.backend_kind = BackendKind::Claude;
        assert!(session_passes_filters(&s, &filters, None, "cool"));
        assert!(session_passes_filters(&s, &filters, None, "foo"));
        assert!(session_passes_filters(&s, &filters, None, "claude"));
        assert!(!session_passes_filters(&s, &filters, None, "nope"));
        // Empty query passes all (subject to other filters).
        assert!(session_passes_filters(&s, &filters, None, ""));
    }
}

/// Lifecycle coverage for the authoritative first-page request.
///
/// These mount the panel **before** the host connects, which is the order
/// production actually produces: `app.rs` awaits `refresh_configured_hosts`
/// (publishing `selected_host_id`) and only then loops into `connect_one_host`
/// (inserting the stream). The pre-existing tests in `dispatch.rs` seed
/// `host_streams` before mounting — the inverse order — so they could not
/// observe a host that becomes reachable late. They still hold; this module
/// covers the ordering they cannot express.
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::{SessionId, SessionSummary};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Let the reactive graph and any spawned send settle.
    async fn settle() {
        for _ in 0..4 {
            next_tick().await;
        }
    }

    fn listed_session(host: &str, id: &str, count: u32, updated: u64) -> SessionInfo {
        SessionInfo {
            host_id: host.to_owned(),
            summary: SessionSummary {
                id: SessionId(id.to_owned()),
                backend_kind: BackendKind::Hermes,
                launch_profile_id: None,
                workspace_roots: vec![],
                project_id: None,
                alias: None,
                user_alias: None,
                parent_id: None,
                created_at_ms: 0,
                updated_at_ms: updated,
                message_count: count,
                token_count: None,
                resumable: true,
                compacted_from_session_id: None,
                compacted_to_session_id: None,
                compacted_at_ms: None,
                compaction_summary_preview: None,
            },
        }
    }

    /// Records `[cmd, args, outcome]` per invoke, and rejects the next
    /// `__test_fail_next_sends` host lines. Rejection is how a dead or absent
    /// host connection surfaces: `tauri_invoke` is declared with `catch`, so
    /// the frontend sees `Err` rather than a panic.
    fn install_send_stub() {
        js_sys::eval(
            r#"
            (function() {
                window.__test_send_calls = [];
                window.__test_fail_next_sends = 0;
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    const fail = cmd === "send_host_line"
                        && window.__test_fail_next_sends > 0;
                    if (fail) { window.__test_fail_next_sends -= 1; }
                    window.__test_send_calls.push([
                        cmd, JSON.stringify(args || {}), fail ? "err" : "ok",
                    ]);
                    return fail
                        ? Promise.reject("simulated dead host connection")
                        : Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
            })();
            "#,
        )
        .expect("install send stub");
    }

    /// Arm the stub so the next `count` host lines fail to send.
    fn install_send_stub_failing(count: u32) {
        install_send_stub();
        let arm = format!("window.__test_fail_next_sends = {count};");
        js_sys::eval(&arm).expect("arm send failures");
    }

    /// Every `list_sessions` frame that actually went out, as
    /// `(host_id, stream)`, in order. Both halves are read back off the
    /// outbound envelope rather than from what the UI intended, so a request
    /// aimed at a connection that no longer exists is visible as one.
    fn list_sessions_targets() -> Vec<(String, String)> {
        let raw = js_sys::eval(
            r#"
            (function() {
                const out = [];
                for (const [cmd, args] of (window.__test_send_calls || [])) {
                    if (cmd !== "send_host_line") continue;
                    const parsed = JSON.parse(args);
                    const env = JSON.parse(parsed.line);
                    if (env.kind !== "list_sessions") continue;
                    out.push([parsed.hostId || parsed.host_id, env.stream]);
                }
                return JSON.stringify(out);
            })()
            "#,
        )
        .expect("probe list_sessions frames")
        .as_string()
        .unwrap_or_else(|| "[]".to_owned());
        serde_json::from_str(&raw).expect("probe returns [host, stream] pairs")
    }

    /// What the transport reported for each `list_sessions` attempt, in order:
    /// `"ok"` or `"err"`. Attempts and outcomes are separate probes because a
    /// retry is only correct if the first attempt genuinely failed.
    fn list_sessions_outcomes() -> Vec<String> {
        let raw = js_sys::eval(
            r#"
            (function() {
                const out = [];
                for (const call of (window.__test_send_calls || [])) {
                    if (call[0] !== "send_host_line") continue;
                    const parsed = JSON.parse(call[1]);
                    const env = JSON.parse(parsed.line);
                    if (env.kind !== "list_sessions") continue;
                    out.push(call[2]);
                }
                return JSON.stringify(out);
            })()
            "#,
        )
        .expect("probe list_sessions outcomes")
        .as_string()
        .unwrap_or_else(|| "[]".to_owned());
        serde_json::from_str(&raw).expect("probe returns outcome strings")
    }

    /// Takes the state by value so the returned mount handle borrows nothing:
    /// the handle must outlive the call, and every test mutates the same state
    /// afterwards to drive connects and disconnects.
    fn mount_panel(state: AppState) -> (HtmlElement, impl Sized) {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document
            .create_element("div")
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SessionsPanel /> }
        });
        (container, handle)
    }

    fn connect(state: &AppState, host_id: &str, stream: &str) {
        state.host_streams.update(|streams| {
            streams.insert(host_id.to_owned(), StreamPath(stream.to_owned()));
        });
    }

    /// The run-27 defect. On the real startup order the panel is already
    /// mounted when the host connects, so a request keyed to the host alone is
    /// decided while nothing is reachable and never reconsidered: no
    /// `ListSessions` is sent for the lifetime of the page, the server-side
    /// summary subscription is never established, and History sits on the
    /// bootstrap snapshot until a reload — exactly the live M7 symptom.
    #[wasm_bindgen_test]
    async fn a_host_connecting_after_mount_is_still_asked_for_its_first_page() {
        install_send_stub();
        let state = AppState::new();
        state.active_project.set(None);
        state.selected_host_id.set(Some("host-a".to_owned()));
        // Bootstrap left a stale row: the session existed at connect time with
        // no completed turns.
        state
            .sessions
            .set(vec![listed_session("host-a", "session-1", 0, 100)]);

        let (container, _handle) = mount_panel(state.clone());
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            Vec::<(String, String)>::new(),
            "nothing is reachable yet, so nothing may be claimed as requested"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("0 responses"),
            "precondition: the bootstrap snapshot is what is on screen"
        );

        // `connect_one_host` inserts the stream — after the panel mounted.
        connect(&state, "host-a", "/host-1");
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![("host-a".to_owned(), "/host-1".to_owned())],
            "a host that becomes reachable after mount must still be asked for \
             its authoritative page on the connection it is reachable on"
        );

        settle().await;
        assert_eq!(
            list_sessions_targets().len(),
            1,
            "and asked exactly once — the request must not repeat while the \
             connection is unchanged"
        );
    }

    /// A reconnect is a new server-side stream with no subscription on it.
    /// Keying the one-shot by host ID alone left the client permanently silent
    /// after any reconnect; keying it by connection asks the live stream and
    /// never the dead one.
    #[wasm_bindgen_test]
    async fn reconnecting_a_host_asks_the_new_connection_and_not_the_dead_one() {
        install_send_stub();
        let state = AppState::new();
        state.active_project.set(None);
        state.selected_host_id.set(Some("host-a".to_owned()));
        state
            .sessions
            .set(vec![listed_session("host-a", "session-1", 0, 100)]);

        let (_container, _handle) = mount_panel(state.clone());
        settle().await;
        connect(&state, "host-a", "/host-1");
        settle().await;
        assert_eq!(
            list_sessions_targets(),
            vec![("host-a".to_owned(), "/host-1".to_owned())],
            "precondition: the first connection was asked"
        );

        // Disconnect exactly as `app.rs:1010` does — one call. It drops the
        // stream itself (`state.rs:4711`), releases the in-flight slot whose
        // answer will now never arrive (`:4482`), and resets the outbound seq
        // counters (`:4718`).
        state.clear_host_runtime("host-a");
        settle().await;

        // `connect_one_host` generates a fresh stream path per connection.
        connect(&state, "host-a", "/host-2");
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![
                ("host-a".to_owned(), "/host-1".to_owned()),
                ("host-a".to_owned(), "/host-2".to_owned()),
            ],
            "the reconnected stream must be asked in its own right, and the \
             request must go to the new stream rather than the dead one"
        );
    }

    /// Multi-host, staggered connects: hosts are asked independently, each on
    /// its own connection, and one host's request neither covers nor suppresses
    /// another's.
    #[wasm_bindgen_test]
    async fn each_host_is_asked_once_as_it_connects() {
        install_send_stub();
        let state = AppState::new();
        state.active_project.set(None);
        state.selected_host_id.set(Some("host-a".to_owned()));
        state.sessions.set(vec![
            listed_session("host-a", "a1", 0, 300),
            listed_session("host-b", "b1", 2, 200),
        ]);

        let (_container, _handle) = mount_panel(state.clone());
        settle().await;
        connect(&state, "host-a", "/host-a-1");
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![("host-a".to_owned(), "/host-a-1".to_owned())],
            "only the connected host is asked; the other is not yet reachable"
        );

        connect(&state, "host-b", "/host-b-1");
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![
                ("host-a".to_owned(), "/host-a-1".to_owned()),
                ("host-b".to_owned(), "/host-b-1".to_owned()),
            ],
            "a host connecting later gets its own request, and the host that \
             already had one is not asked again"
        );
    }

    /// The failure path, end to end, on the only recovery the transport
    /// actually offers.
    ///
    /// A failed automatic send means this stream is already dead: `router.rs`
    /// `send_line` fails only when the host is gone from the registry or its
    /// writer task has exited, and `send.rs` has by then consumed a sequence
    /// number the server never saw. So the panel must *not* try the same
    /// `StreamPath` again — a second frame there would trip the server's
    /// `SeqValidator`, and since the router routes by `host_id` it could land
    /// on a replacement transport and tear down the connection that was about
    /// to recover. It must wait for the disconnect this failure implies, and
    /// ask the new connection exactly once.
    #[wasm_bindgen_test]
    async fn a_failed_send_waits_for_the_reconnect_rather_than_retrying() {
        install_send_stub_failing(1);
        let state = AppState::new();
        state.active_project.set(None);
        state.selected_host_id.set(Some("host-a".to_owned()));
        state
            .sessions
            .set(vec![listed_session("host-a", "session-1", 0, 100)]);

        let (container, _handle) = mount_panel(state.clone());
        settle().await;
        connect(&state, "host-a", "/host-1");
        for _ in 0..6 {
            settle().await;
        }

        assert_eq!(
            list_sessions_outcomes(),
            vec!["err".to_owned()],
            "the send failed, and the dead connection must not be asked again \
             — no retry burst on a stream the transport has given up on"
        );
        assert!(
            !state
                .session_list_refresh_in_flight
                .get_untracked()
                .contains("host-a"),
            "but the host-level gate must be released, or teardown and an \
             explicit Refresh stay wedged behind an answer that never comes"
        );

        // The disconnect that this send failure implies, exactly as
        // `app.rs:1010` handles it: the stream is dropped and the inbound and
        // outbound sequence state is reset (`state.rs:4711-4719`).
        state.clear_host_runtime("host-a");
        settle().await;
        state
            .sessions
            .set(vec![listed_session("host-a", "session-1", 0, 100)]);
        connect(&state, "host-a", "/host-2");
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![
                ("host-a".to_owned(), "/host-1".to_owned()),
                ("host-a".to_owned(), "/host-2".to_owned()),
            ],
            "the reconnected stream is enrolled exactly once, and nothing was \
             ever sent to the dead path again"
        );
        assert_eq!(
            list_sessions_outcomes(),
            vec!["err".to_owned(), "ok".to_owned()],
            "and that second request is the one that actually reached a host"
        );

        // The authoritative answer for the new connection must not look like a
        // reason to ask again.
        let page = protocol::SessionListPayload {
            sessions: vec![listed_session("host-a", "session-1", 4, 500).summary],
            page: protocol::SessionListPageInfo {
                total_count: 1,
                ..Default::default()
            },
        };
        let envelope = protocol::Envelope::from_payload(
            StreamPath("/host-2".to_owned()),
            FrameKind::SessionList,
            0,
            &page,
        )
        .expect("build SessionList envelope");
        crate::dispatch::dispatch_envelope(&state, "host-a", envelope);
        for _ in 0..4 {
            settle().await;
        }

        assert_eq!(
            list_sessions_targets().len(),
            2,
            "the response must not retrigger the request that produced it"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("4 responses"),
            "and the recovered connection's page is what History shows"
        );
    }

    /// Refresh is a user action and consults no claim: a live connection that
    /// has already been enrolled must still answer an explicit press, on the
    /// connection it is actually reachable on.
    #[wasm_bindgen_test]
    async fn refresh_asks_the_live_connection_again() {
        install_send_stub();
        let state = AppState::new();
        state.active_project.set(None);
        state.selected_host_id.set(Some("host-a".to_owned()));
        state
            .sessions
            .set(vec![listed_session("host-a", "session-1", 0, 100)]);

        let (container, _handle) = mount_panel(state.clone());
        settle().await;
        connect(&state, "host-a", "/host-1");
        settle().await;
        assert_eq!(
            list_sessions_targets(),
            vec![("host-a".to_owned(), "/host-1".to_owned())],
            "precondition: the connection was claimed by a successful send"
        );

        let refresh = container
            .query_selector("[data-test=\"sessions-refresh\"]")
            .unwrap()
            .expect("the Refresh button is rendered")
            .dyn_into::<HtmlElement>()
            .unwrap();
        refresh.click();
        settle().await;

        assert_eq!(
            list_sessions_targets(),
            vec![
                ("host-a".to_owned(), "/host-1".to_owned()),
                ("host-a".to_owned(), "/host-1".to_owned()),
            ],
            "an already-claimed connection must not swallow an explicit Refresh"
        );
        assert_eq!(
            list_sessions_outcomes(),
            vec!["ok".to_owned(), "ok".to_owned()],
            "and both reached the host"
        );
    }
}
