use leptos::prelude::*;
use protocol::{
    FrameKind, LearningChange, LearningChangeId, LearningChangeStatus, LearningEditId,
    LearningOperation, LearningRule, SelfImprovementAction,
};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame_unreported;
use crate::state::AppState;
use protocol::SteeringScope;

fn send_learning_action(
    state: &AppState,
    host: &str,
    action: SelfImprovementAction,
    error: RwSignal<Option<String>>,
) {
    let Some(stream) = state.host_stream_untracked(host) else {
        error.set(Some("Self-improvement host is disconnected".into()));
        return;
    };
    crate::notices::clear_self_improvement_change(host, action.id());
    let host = host.to_owned();
    spawn_local(async move {
        if let Err(send_error) = send_frame_unreported(
            &host,
            stream.clone(),
            FrameKind::SelfImprovementAction,
            &action,
        )
        .await
        {
            error.set(Some(format!("Cannot send suggestion action: {send_error}")));
        }
    });
}

fn rule_parts(rule: &LearningRule) -> (&'static str, &str, &str) {
    match rule {
        LearningRule::Steering { steering } => ("Steering", &steering.title, &steering.content),
        LearningRule::ReviewAspect { aspect, .. } => {
            ("Review aspect", &aspect.name, &aspect.instructions)
        }
    }
}

fn status_label(change: &LearningChange) -> Option<String> {
    Some(match &change.status {
        LearningChangeStatus::Pending => return None,
        LearningChangeStatus::Applying => "Applying".into(),
        LearningChangeStatus::Applied => "Applied".into(),
        LearningChangeStatus::Undoing => "Undoing".into(),
        LearningChangeStatus::Undone => "Undone".into(),
        LearningChangeStatus::Dismissed => "Dismissed".into(),
        LearningChangeStatus::Conflict { reason } => format!("Conflict: {reason}"),
    })
}

#[component]
pub fn LearningChangeCard(
    host_id: String,
    id: LearningChangeId,
    in_scope: Signal<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let store = StoredValue::new(state.clone());
    let host = StoredValue::new(host_id);
    let current = Memo::new(move |_| {
        state.self_improvement_by_host.with(|hosts| {
            hosts
                .get(&host.get_value())?
                .changes
                .iter()
                .find(|change| change.id == id)
                .cloned()
        })
    });
    let pending = move || {
        current
            .get()
            .is_some_and(|change| matches!(change.status, LearningChangeStatus::Pending))
    };
    let applied = move || {
        current
            .get()
            .is_some_and(|change| matches!(change.status, LearningChangeStatus::Applied))
    };
    let restorable = move || {
        current.get().is_some_and(|change| {
            matches!(
                change.status,
                LearningChangeStatus::Dismissed | LearningChangeStatus::Undone
            )
        })
    };
    let dismissible = move || {
        current.get().is_some_and(|change| {
            matches!(change.status, LearningChangeStatus::Pending)
                || (matches!(change.status, LearningChangeStatus::Conflict { .. })
                    && change.applied_revision.is_none())
        })
    };
    let editing = RwSignal::new(false);
    let initialized = RwSignal::new(false);
    let base = RwSignal::new(None::<LearningOperation>);
    let action_error = RwSignal::new(None::<String>);
    let draft_title = RwSignal::new(String::new());
    let draft_text = RwSignal::new(String::new());
    let submitted = RwSignal::new(None::<(LearningEditId, LearningRule)>);
    let copied = RwSignal::new(false);
    Effect::new(move |_| {
        if let (Some((edit_id, expected)), Some(change)) = (submitted.get(), current.get())
            && change.last_edit_id.as_ref() == Some(&edit_id)
            && change.operation.after == expected
        {
            let (_, title, text) = rule_parts(&expected);
            let draft_is_submitted =
                draft_title.get_untracked() == title && draft_text.get_untracked() == text;
            submitted.set(None);
            base.set(Some(change.operation));
            if draft_is_submitted {
                editing.set(false);
                initialized.set(false);
            }
        }
    });
    let outdated = move || {
        initialized.get()
            && current
                .get()
                .is_some_and(|change| base.get().is_some_and(|base| base != change.operation))
    };
    let dirty = move || {
        base.get().is_some_and(|base| {
            let (_, title, text) = rule_parts(&base.after);
            draft_title.get() != title || draft_text.get() != text
        })
    };
    let heading = move || {
        current.get().map(|change| match &change.operation.before {
            Some(rule) => format!("Update {}", rule_parts(rule).1),
            None => "New rule".into(),
        })
    };
    let action = move |action| {
        if !in_scope.get_untracked() {
            action_error.set(Some(
                "Suggestion scope changed; return to its scope before editing".into(),
            ));
            return;
        }
        action_error.set(None);
        send_learning_action(&store.get_value(), &host.get_value(), action, action_error);
    };
    let apply = move |_| {
        if let Some(change) = current.get_untracked() {
            action(SelfImprovementAction::Apply { id: change.id });
        }
    };
    let dismiss = move |_| {
        if let Some(change) = current.get_untracked() {
            action(SelfImprovementAction::Dismiss { id: change.id });
        }
    };
    let undo = move |_| {
        if let Some(change) = current.get_untracked() {
            action(SelfImprovementAction::Undo { id: change.id });
        }
    };
    let restore = move |_| {
        if let Some(change) = current.get_untracked() {
            action(SelfImprovementAction::Restore { id: change.id });
        }
    };
    let edit = move |_| {
        if let Some(change) = current.get_untracked()
            && pending()
        {
            if !initialized.get_untracked() {
                let (_, title, text) = rule_parts(&change.operation.after);
                draft_title.set(title.to_owned());
                draft_text.set(text.to_owned());
                base.set(Some(change.operation.clone()));
                initialized.set(true);
            }
            editing.set(true);
        }
    };
    let save = move |_| {
        if !dirty() {
            action_error.set(Some("No changes to save".into()));
            return;
        }
        if outdated() {
            action_error.set(Some(
                "Suggestion changed on the host. Your draft is retained.".into(),
            ));
            return;
        }
        if let (Some(change), Some(expected)) = (current.get_untracked(), base.get_untracked()) {
            if !pending() {
                return;
            }
            let mut after = expected.after.clone();
            match &mut after {
                LearningRule::Steering { steering } => {
                    steering.title = draft_title.get_untracked();
                    steering.content = draft_text.get_untracked();
                }
                LearningRule::ReviewAspect { aspect, .. } => {
                    aspect.name = draft_title.get_untracked();
                    aspect.instructions = draft_text.get_untracked();
                }
            }
            let mut bytes = [0_u8; 16];
            let identity = web_sys::window()
                .ok_or("Browser window is unavailable")
                .and_then(|window| window.crypto().map_err(|_| "Browser crypto is unavailable"))
                .and_then(|crypto| {
                    crypto
                        .get_random_values_with_u8_array(&mut bytes)
                        .map_err(|_| "Cannot create edit identity")
                });
            if let Err(error) = identity {
                action_error.set(Some(error.into()));
                return;
            }
            let request_id =
                LearningEditId(bytes.iter().map(|byte| format!("{byte:02x}")).collect());
            submitted.set(Some((request_id.clone(), after.clone())));
            action(SelfImprovementAction::Edit {
                id: change.id,
                request_id,
                reason: expected.reason.clone(),
                expected: Box::new(expected),
                after,
            });
        }
    };
    let open_rule = move |_| {
        if !in_scope.get_untracked() {
            return;
        }
        if let Some(change) = current.get_untracked() {
            let state = store.get_value();
            state.selected_host_id.set(Some(host.get_value()));
            state
                .settings_tab_request
                .set(Some(match change.operation.after {
                    LearningRule::Steering { .. } => "Steering",
                    LearningRule::ReviewAspect { .. } => "Review",
                }));
            state.settings_open.set(true);
        }
    };
    view! {
        <article class="learning-change" aria-label=heading>
            <header class="learning-change-header">
                <span class="learning-change-target">{move || current.get().map(|change| format!("{} {}", if change.operation.before.is_some() { "Update" } else { "New" }, rule_parts(&change.operation.after).0.to_lowercase()))}</span>
                <Show when=move || current.get().is_some_and(|change| change.operation.before.is_some())>
                    <button class="learning-link" title="Open existing rule settings" on:click=open_rule>"Existing rule ↗"</button>
                </Show>
            </header>
            <h3 class="learning-change-title">{move || current.get().map(|change| rule_parts(&change.operation.after).1.to_owned())}</h3>
            {move || current.get().and_then(|change| {
                let conflict = matches!(change.status, LearningChangeStatus::Conflict { .. });
                status_label(&change).map(|status| view! {
                    <p class=if conflict { "learning-change-status inline-notice error" } else { "learning-change-status" }
                        role=if conflict { "alert" } else { "status" }>{status}</p>
                })
            })}
            <crate::notices::InlineNotices scopes=Signal::derive(move || {
                let Some(change) = current.get() else { return Vec::new(); };
                let mut scopes = vec![crate::notices::NoticeScope::SelfImprovementChange(host.get_value(), change.id.clone())];
                if let Some((edit_id, _)) = submitted.get() {
                    scopes.push(crate::notices::NoticeScope::SelfImprovementEdit(host.get_value(), change.id, edit_id));
                }
                scopes
            }) />
            {move || action_error.get().map(|error| view! { <p class="learning-error" role="alert">{error}</p> })}
            <Show when=move || editing.get() && outdated()>
                <p class="learning-error" role="alert">"Suggestion changed on the host. Your draft is retained."</p>
                <details class="learning-change-diff" open>
                    <summary>"Latest proposal"</summary>
                    <pre class="learning-latest">{move || current.get().map(|change| {
                        let (_, title, text) = rule_parts(&change.operation.after);
                        format!("{title}\n\n{text}")
                    })}</pre>
                </details>
                <Show when=pending><button class="settings-btn" disabled=move || !in_scope.get() on:click=move |_| {
                    if let Some(change) = current.get_untracked() && pending() && in_scope.get_untracked() {
                        base.set(Some(change.operation)); submitted.set(None);
                        crate::notices::clear_self_improvement_change(&host.get_value(), &change.id);
                        action_error.set(None);
                    }
                }>"Keep my edit"</button></Show>
            </Show>
            <Show when=move || editing.get() fallback=move || view! {
                <details class="learning-change-diff" open>
                    <summary>"Changes"</summary>
                    <div class="learning-diff-lines" aria-label="Before and proposed diff">
                        {move || current.get().map(|change| {
                            let full = |rule: &LearningRule| {
                                let (_, title, text) = rule_parts(rule);
                                format!("{title}\n\n{text}")
                            };
                            let before = change.operation.before.as_ref().map(full).unwrap_or_default();
                            let after = full(&change.operation.after);
                            similar::TextDiff::from_lines(&before, &after).iter_all_changes().map(|line| {
                                let (class, prefix, label) = match line.tag() {
                                    similar::ChangeTag::Delete => ("learning-diff-remove", "− ", "Before"),
                                    similar::ChangeTag::Insert => ("learning-diff-add", "+ ", "Proposed"),
                                    similar::ChangeTag::Equal => ("learning-diff-context", "  ", "Unchanged"),
                                };
                                view! { <div class=class aria-label=label><span aria-hidden="true">{prefix}</span>{line.to_string()}</div> }
                            }).collect_view()
                        })}
                    </div>
                </details>
            }>
                <div class="learning-change-editor">
                    <input class="settings-text-input" aria-label="Suggestion title"
                        readonly=move || !pending() prop:value=move || draft_title.get() on:input=move |ev| draft_title.set(event_target_value(&ev)) />
                    <textarea class="settings-text-input" aria-label="Suggestion text" rows="6"
                        readonly=move || !pending() prop:value=move || draft_text.get() on:input=move |ev| draft_text.set(event_target_value(&ev)) />
                    <div class="learning-change-actions">
                        <Show when=pending>
                            <button class="settings-btn settings-btn-primary" disabled=move || !in_scope.get() || outdated() || !dirty() on:click=save>"Save"</button>
                        </Show>
                        <Show when=move || !pending() || outdated()>
                            <button class="settings-btn" on:click=move |_| {
                                let text = format!("{}\n\n{}", draft_title.get_untracked(), draft_text.get_untracked());
                                spawn_local(async move {
                                    match crate::bridge::write_clipboard_text(&text).await {
                                        Ok(()) => copied.set(true),
                                        Err(error) => crate::components::header::report_user_error(&error),
                                    }
                                });
                            }>{move || if copied.get() { "Copied" } else { "Copy draft" }}</button>
                        </Show>
                        <button class="settings-btn" on:click=move |_| {
                            submitted.set(None); editing.set(false); copied.set(false);
                            if !pending() || outdated() { initialized.set(false); }
                        }>{move || if pending() && !outdated() { "Cancel" } else { "Discard draft" }}</button>
                    </div>
                </div>
            </Show>
            <details class="learning-change-details">
                <summary>"Why this change"</summary>
                <div class="learning-change-reason" inner_html=move || current.get().map(|change| crate::markdown::render_markdown_without_images(&change.operation.reason)).unwrap_or_default() />
            </details>
            <Show when=move || !editing.get() && (pending() || applied() || restorable() || dismissible())>
                <div class="learning-change-actions">
                    <Show when=pending>
                        <button class="settings-btn settings-btn-primary" disabled=move || !in_scope.get() on:click=apply>"Accept"</button>
                        <button class="settings-btn" disabled=move || !in_scope.get() on:click=edit>"Edit"</button>
                    </Show>
                    <Show when=dismissible><button class=move || if pending() { "learning-dismiss" } else { "settings-btn" } disabled=move || !in_scope.get() on:click=dismiss>"Dismiss"</button></Show>
                    <Show when=restorable><button class="settings-btn" disabled=move || !in_scope.get() on:click=restore>"Restore"</button></Show>
                    <Show when=applied><button class="settings-btn" disabled=move || !in_scope.get() on:click=undo>"Undo"</button></Show>
                </div>
            </Show>
        </article>
    }
}

#[component]
pub fn PendingBadge() -> impl IntoView {
    let state = expect_context::<AppState>();
    let count = Memo::new(move |_| {
        let host = state.chat_context_host_id();
        let project = state.active_project.get();
        state.self_improvement_by_host.with(|hosts| {
            host.as_ref()
                .and_then(|host| hosts.get(host))
                .map_or(0, |snapshot| {
                    snapshot
                        .changes
                        .iter()
                        .filter(|change| {
                            matches!(change.status, LearningChangeStatus::Pending)
                                && match &change.scope {
                                    SteeringScope::Host => true,
                                    SteeringScope::Project(id) => {
                                        project.as_ref().is_some_and(|project| {
                                            Some(&project.host_id) == host.as_ref()
                                                && &project.project_id == id
                                        })
                                    }
                                }
                        })
                        .count()
                })
        })
    });
    view! { <Show when=move || { count.get() > 0 }><span class="learning-count" aria-label=move || format!("{} pending suggestions", count.get())>{move || count.get()}</span></Show> }
}

#[component]
pub fn SelfImprovementSidebar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let store = StoredValue::new(state.clone());
    let selection = RwSignal::new(None::<(String, SteeringScope)>);
    let history = RwSignal::new(false);
    Effect::new(move |_| {
        let state = store.get_value();
        let host = state.chat_context_host_id();
        let project = state.active_project.get();
        selection.set(host.map(|host| {
            let scope = match project.filter(|project| project.host_id == host) {
                Some(project) => SteeringScope::Project(project.project_id),
                None => SteeringScope::Host,
            };
            (host, scope)
        }));
    });
    Effect::new(move |_| {
        let state = store.get_value();
        if let Some(request) = state.self_improvement_scope_request.get() {
            selection.set(Some(request));
            state.self_improvement_scope_request.set(None);
        }
    });
    let snapshot = Memo::new(move |_| {
        let (host, _) = selection.get()?;
        store
            .get_value()
            .self_improvement_by_host
            .with(|hosts| hosts.get(&host).cloned())
    });
    let changes = Memo::new(move |_| {
        let Some((_, scope)) = selection.get() else {
            return Vec::new();
        };
        snapshot
            .get()
            .map(|snapshot| {
                snapshot
                    .changes
                    .into_iter()
                    .filter(|change| change.scope == scope)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let pending = move || {
        changes
            .get()
            .iter()
            .filter(|change| {
                matches!(
                    change.status,
                    LearningChangeStatus::Pending
                        | LearningChangeStatus::Applying
                        | LearningChangeStatus::Conflict { .. }
                ) && change.applied_revision.is_none()
            })
            .count()
    };
    let is_history = |change: &LearningChange| {
        matches!(
            change.status,
            LearningChangeStatus::Applied
                | LearningChangeStatus::Undoing
                | LearningChangeStatus::Undone
                | LearningChangeStatus::Dismissed
        ) || (matches!(change.status, LearningChangeStatus::Conflict { .. })
            && change.applied_revision.is_some())
    };
    let empty = move || {
        !changes
            .get()
            .iter()
            .any(|change| is_history(change) == history.get())
    };
    let all_rows = move || {
        store.get_value().self_improvement_by_host.with(|hosts| {
            let mut rows = hosts
                .iter()
                .flat_map(|(host, snapshot)| {
                    snapshot
                        .changes
                        .iter()
                        .map(|change| (host.clone(), change.id.clone(), change.created_at_ms))
                })
                .collect::<Vec<_>>();
            rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.0.cmp(&b.1.0)));
            rows
        })
    };
    view! {
        <div class="self-improvement-sidebar">
            <header class="learning-sidebar-header">
                <h2>"Self-improvement"</h2>
                <button class="learning-link" aria-label="Self-improvement settings" title="Self-improvement settings" on:click=move |_| {
                    let state = store.get_value();
                    if let Some((host, scope)) = selection.get_untracked() {
                        state.selected_host_id.set(Some(host.clone()));
                        state.self_improvement_settings_scope_request.set(Some((host, scope)));
                    }
                    state.settings_tab_request.set(Some("Self-improvement"));
                    state.settings_open.set(true);
                }>"⚙"</button>
            </header>
            <div class="learning-sidebar-scope">
                <select class="settings-select" aria-label="Suggestions host"
                    prop:value=move || selection.get().map(|(host, _)| host).unwrap_or_default()
                    on:change=move |ev| selection.set(Some((event_target_value(&ev), SteeringScope::Host)))>
                    <option value="" disabled>"Choose a host"</option>
                    {move || store.get_value().configured_hosts.get().into_iter().map(|host| view! { <option value=host.id>{host.label}</option> }).collect_view()}
                </select>
                <select class="settings-select" aria-label="Suggestions scope"
                    prop:value=move || selection.get().map(|(_, scope)| match scope { SteeringScope::Host => String::new(), SteeringScope::Project(id) => id.0 }).unwrap_or_default()
                    on:change=move |ev| {
                        if let Some((host, _)) = selection.get_untracked() {
                            let value = event_target_value(&ev);
                            selection.set(Some((host, if value.is_empty() { SteeringScope::Host } else { SteeringScope::Project(protocol::ProjectId(value)) })));
                        }
                    }>
                    <option value="">"All projects (host-wide)"</option>
                    {move || store.get_value().projects.get().into_iter().filter(|project| selection.get().is_some_and(|(host, _)| host == project.host_id)).map(|project| view! { <option value=project.project.id.0>{project.project.name}</option> }).collect_view()}
                </select>
            </div>
            <div class="learning-sidebar-tabs" role="tablist" aria-label="Suggestions">
                <button role="tab" aria-selected=move || (!history.get()).to_string() on:click=move |_| history.set(false)>"Pending" <span class="learning-count">{pending}</span></button>
                <button role="tab" aria-selected=move || history.get().to_string() on:click=move |_| history.set(true)>"History"</button>
            </div>
            <div class="learning-sidebar-content">
                <Show when=move || selection.get().is_none()><p class="learning-empty">"Choose a host"</p></Show>
                <Show when=move || selection.get().is_some() && snapshot.get().is_none()><p class="learning-empty" role="status">"Suggestions unavailable"</p></Show>
                <crate::notices::InlineNotices scopes=Signal::derive(move || selection.get().map(|(host, _)| vec![crate::notices::NoticeScope::SelfImprovement(host)]).unwrap_or_default()) />
                {move || snapshot.get().and_then(|snapshot| snapshot.error).map(|error| view! { <p class="learning-error" role="alert"><strong>"Host: "</strong>{error}</p> })}
                {move || {
                    let (_, scope) = selection.get()?;
                    let status = snapshot.get()?.scopes.into_iter().find(|entry| entry.scope == scope)?.status;
                    match status {
                        protocol::LearningStatus::Failed { reason } | protocol::LearningStatus::Unsupported { reason } => Some(view! { <p class="learning-error" role="alert">{reason}</p> }),
                        _ => None,
                    }
                }}
                <Show when=move || snapshot.get().is_some() && empty()><p class="learning-empty">"No suggestions"</p></Show>
                <div class="learning-changes">
                    <For each=all_rows key=|(host, id, _)| (host.clone(), id.clone()) children=move |(host, id, _)| {
                        let row_host = StoredValue::new(host.clone());
                        let row_id = StoredValue::new(id.clone());
                        let in_scope = Signal::derive(move || {
                            let Some((host, scope)) = selection.get() else { return false; };
                            host == row_host.get_value() && store.get_value().self_improvement_by_host.with(|hosts| hosts.get(&host).is_some_and(|snapshot| snapshot.changes.iter().any(|change| change.id == row_id.get_value() && change.scope == scope)))
                        });
                        let visible = move || in_scope.get() && store.get_value().self_improvement_by_host.with(|hosts| hosts.get(&row_host.get_value()).is_some_and(|snapshot| snapshot.changes.iter().any(|change| change.id == row_id.get_value() && is_history(change) == history.get())));
                        view! { <div style=move || if visible() { "" } else { "display:none" }><LearningChangeCard host_id=host id in_scope /></div> }
                    } />
                </div>
            </div>
        </div>
    }
}
