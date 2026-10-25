use leptos::prelude::*;
use protocol::{
    FrameKind, LearningChange, LearningChangeId, LearningChangeStatus, LearningRule,
    SelfImprovementAction,
};
use wasm_bindgen_futures::spawn_local;

use crate::send::send_frame;
use crate::state::{ActiveAgentRef, AppState};

fn send_learning_action(state: &AppState, host: &str, action: SelfImprovementAction) {
    let Some(stream) = state.host_stream_untracked(host) else {
        crate::components::header::report_user_error("Self-improvement host is disconnected");
        return;
    };
    let host = host.to_owned();
    spawn_local(async move {
        if let Err(error) = send_frame(
            &host,
            stream.clone(),
            FrameKind::SelfImprovementAction,
            &action,
        )
        .await
        {
            crate::notices::report_send_failure(
                &host,
                &stream,
                FrameKind::SelfImprovementAction,
                &error,
            );
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
        LearningChangeStatus::Pending if change.requires_approval => {
            "Needs approval: you edited this rule".into()
        }
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
    #[prop(optional)] on_edit_applied: Option<Callback<LearningChange>>,
    #[prop(optional)] on_open_origin: Option<Callback<LearningChange>>,
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
    let editing = RwSignal::new(false);
    let draft_title = RwSignal::new(String::new());
    let draft_text = RwSignal::new(String::new());
    let submitted = RwSignal::new(None::<LearningRule>);
    let copied = RwSignal::new(false);
    Effect::new(move |_| {
        if let (Some(expected), Some(change)) = (submitted.get(), current.get())
            && change.operation.after == expected
        {
            let (_, title, text) = rule_parts(&expected);
            let draft_is_submitted =
                draft_title.get_untracked() == title && draft_text.get_untracked() == text;
            submitted.set(None);
            if draft_is_submitted {
                editing.set(false);
            }
        }
    });
    let heading = move || {
        current.get().map(|change| match &change.operation.before {
            Some(rule) => format!("Update {}", rule_parts(rule).1),
            None => "New rule".into(),
        })
    };
    let action = move |action| send_learning_action(&store.get_value(), &host.get_value(), action);
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
    let edit = move |_| {
        if let Some(change) = current.get_untracked() {
            if pending() {
                let (_, title, text) = rule_parts(&change.operation.after);
                draft_title.set(title.to_owned());
                draft_text.set(text.to_owned());
                editing.set(true);
            } else if let Some(callback) = on_edit_applied {
                callback.run(change);
            }
        }
    };
    let save = move |_| {
        if let Some(change) = current.get_untracked() {
            let mut after = change.operation.after;
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
            submitted.set(Some(after.clone()));
            action(SelfImprovementAction::Edit {
                id: change.id,
                after,
                reason: change.operation.reason,
            });
        }
    };
    view! {
        <article class="learning-change" aria-label=heading>
            <header class="learning-change-header">
                <span class="learning-change-kind">{heading}</span>
                <span class="learning-change-target">{move || current.get().map(|change| rule_parts(&change.operation.after).0)}</span>
                {move || current.get().and_then(|change| status_label(&change)).map(|status| view! { <span class="learning-change-status">{status}</span> })}
            </header>
            <Show when=move || editing.get() fallback=move || view! {
                {move || current.get().and_then(|change| {
                    let (_, title, _) = rule_parts(&change.operation.after);
                    (change.operation.before.as_ref().map(|rule| rule_parts(rule).1) != Some(title))
                        .then(|| view! { <p class="learning-change-title">{title.to_owned()}</p> })
                })}
                <div class="learning-change-text" inner_html=move || current.get().map(|change| crate::markdown::render_markdown_without_images(rule_parts(&change.operation.after).2)).unwrap_or_default() />
            }>
                <div class="learning-change-editor">
                    <input class="settings-text-input" aria-label="Suggestion title"
                        readonly=move || !pending() prop:value=move || draft_title.get() on:input=move |ev| draft_title.set(event_target_value(&ev)) />
                    <textarea class="settings-text-input" aria-label="Suggestion text" rows="3"
                        readonly=move || !pending() prop:value=move || draft_text.get() on:input=move |ev| draft_text.set(event_target_value(&ev)) />
                    <div class="learning-change-actions">
                        <Show when=pending>
                            <button class="settings-btn settings-btn-primary" on:click=save>"Save"</button>
                        </Show>
                        <Show when=move || !pending()>
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
                        <button class="settings-btn" on:click=move |_| { submitted.set(None); editing.set(false); copied.set(false); }>{move || if pending() { "Cancel" } else { "Discard draft" }}</button>
                    </div>
                </div>
            </Show>
            <p class="learning-change-reason">{move || current.get().map(|change| change.operation.reason)}</p>
            <details class="learning-change-details">
                <summary>"Source feedback"</summary>
                {move || current.get().and_then(|change| change.operation.before).map(|before| view! {
                    <div class="learning-change-detail"><span>"Current rule"</span><p>{rule_parts(&before).2.to_owned()}</p></div>
                })}
                <div class="learning-change-detail">
                    <p class="learning-change-origin">{move || current.get().map(|change| change.origin_message)}</p>
                </div>
                {on_open_origin.map(|callback| view! {
                    <button class="learning-link" on:click=move |_| { if let Some(change) = current.get_untracked() { callback.run(change); } }>"Open conversation"</button>
                })}
            </details>
            <Show when=move || !editing.get() && (pending() || applied())>
                <div class="learning-change-actions">
                    <Show when=pending><button class="settings-btn settings-btn-primary" on:click=apply>"Apply"</button></Show>
                    <Show when=move || pending() || (applied() && on_edit_applied.is_some())><button class="settings-btn" on:click=edit>"Edit"</button></Show>
                    <Show when=pending><button class="settings-btn" on:click=dismiss>"Dismiss"</button></Show>
                    <Show when=applied><button class="settings-btn" on:click=undo>"Undo"</button></Show>
                </div>
            </Show>
        </article>
    }
}

#[component]
pub fn LearningTray(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let changes = Memo::new(move |_| {
        let Some(agent) = agent_ref.get() else {
            return Vec::new();
        };
        state.self_improvement_by_host.with(|hosts| {
            hosts
                .get(&agent.host_id)
                .map(|snapshot| {
                    snapshot
                        .changes
                        .iter()
                        .filter(|change| change.origin_agent_id == agent.agent_id)
                        .map(|change| (agent.host_id.clone(), change.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let pending = move || {
        changes
            .get()
            .iter()
            .filter(|(_, change)| matches!(change.status, LearningChangeStatus::Pending))
            .count()
    };
    view! {
        <Show when=move || !changes.get().is_empty()>
            <details class="learning-tray">
                <summary>{move || match pending() { 0 => "Self-improvement".into(), 1 => "1 suggestion".into(), count => format!("{count} suggestions") }}</summary>
                <div class="learning-tray-content" aria-label="Self-improvement suggestions">
                    <For each=move || changes.get() key=|(host, change)| (host.clone(), change.id.clone())
                        children={move |(host_id, change)| view! { <LearningChangeCard host_id id=change.id /> }} />
                </div>
            </details>
        </Show>
    }
}
