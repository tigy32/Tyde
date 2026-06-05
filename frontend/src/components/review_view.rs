//! Shared review controls and helpers for the always-on inline review flow.
//!
//! The standalone three-pane workbench has been retired: reviews are now an
//! always-on, root-scoped layer over the normal git diff surfaces
//! (`ReviewableDiffView` in `diff_view.rs`). What remains here is the shared
//! action sidebar (`ReviewSidebar` — live counts, AI-reviewer form,
//! submit-target picker, Clear) that the git-panel per-root hub mounts, plus
//! the subscribe/diff-open/feedback helpers used across the integrated flow.
//!
//! Reactivity rules (`dev-docs/01-philosophy.md`):
//! * No optimistic UI: action buttons disable on click and re-enable when
//!   the corresponding `ReviewEvent` echoes back via dispatch.
//! * No cached counts on the frontend; per-file thread counts derive via
//!   `Memo` from the live `Review` record.
//! * Late-subscribe replay: `ReviewEvent::Snapshot` is the source of truth
//!   on subscribe and replaces any prior partial entry.

use std::collections::BTreeMap;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen_futures::spawn_local;

// `ComposerState` is re-exported because `inline_review` imports it from here.
pub(crate) use crate::components::review_layer::ComposerState;
use crate::send::send_frame;

use crate::state::{
    ActiveAgentRef, AgentInfo, AppState, DiffViewState, ReviewActionGate, ReviewActionTarget,
    TabContent, root_display_name,
};

use protocol::{
    BackendKind, FrameKind, ProjectDiffScope, ProjectReadDiffPayload, Review, ReviewActionPayload,
    ReviewAiReviewerStatus, ReviewCommentSource, ReviewStatus, ReviewSuggestionState, StreamPath,
};

type ReviewAiIntervalSlot = StoredValue<Option<(i32, Closure<dyn Fn()>)>, LocalStorage>;

/// Local copy of the `format_relative_time` pattern used by chat_message
/// and agents_panel — kept inline here so the review surface doesn't
/// depend on a sibling component's private helper.
pub(crate) fn format_relative_time(timestamp_ms: u64) -> String {
    if timestamp_ms == 0 {
        return String::new();
    }
    let now = js_sys::Date::now() as u64;
    let diff_secs = now.saturating_sub(timestamp_ms) / 1000;
    if diff_secs < 60 {
        "just now".to_owned()
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else if diff_secs < 86400 {
        format!("{}h ago", diff_secs / 3600)
    } else {
        format!("{}d ago", diff_secs / 86400)
    }
}

/// Wire a textarea so its height tracks its content. Mounts an Effect
/// that reads the body signal and resizes the element each render. The
/// height is capped to keep extremely long bodies from eating the
/// composer area entirely; users can still scroll inside the textarea
/// past the cap.
pub(crate) fn autosize_textarea(node_ref: NodeRef<leptos::html::Textarea>, body: RwSignal<String>) {
    Effect::new(move |_| {
        let _ = body.get();
        if let Some(el) = node_ref.get() {
            let html_el: web_sys::HtmlElement = (*el).clone();
            let style = html_el.style();
            // Reset to auto first so scrollHeight reflects content
            // truthfully — without this, shrinking the body wouldn't
            // shrink the box.
            let _ = style.set_property("height", "auto");
            let scroll_h = html_el.scroll_height();
            let capped = scroll_h.min(300);
            let _ = style.set_property("height", &format!("{capped}px"));
        }
    });
}

/// "1m 23s" / "12s" — used for the AI reviewer's elapsed-time chip.
fn format_elapsed(start_ms: u64) -> String {
    if start_ms == 0 {
        return String::new();
    }
    let now = js_sys::Date::now() as u64;
    let secs = now.saturating_sub(start_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Action sidebar for a Draft review: live counts, the AI-reviewer form,
/// the submit-target picker (with full gating), and Clear. Public to the
/// crate so the git-panel review hub can mount the exact same controls
/// without re-implementing submit-target gating. Reviews are always-on, so
/// there is no Cancel/discard-review affordance here.
#[component]
pub(crate) fn ReviewSidebar(
    review: Review,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Live re-derivation of the review record so sidebar buttons reflect
    // server-pushed status updates without remount.
    let live: Memo<Option<Review>> = {
        let id = review_id.clone();
        Memo::new(move |_| state.reviews.with(|map| map.get(&id).cloned()))
    };
    let live_for_status = live;
    let live_for_count = live;
    let live_for_ai = live;

    let review_clone = review.clone();
    let status: Memo<ReviewStatus> = Memo::new(move |_| {
        live_for_status
            .get()
            .map(|r| r.status)
            .unwrap_or_else(|| review_clone.status.clone())
    });

    // Reactive counts derived from the live signal — never cached.
    let user_comment_count: Memo<usize> = Memo::new(move |_| {
        live_for_count
            .get()
            .map(|r| {
                r.comments
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.source,
                            ReviewCommentSource::User | ReviewCommentSource::AiSuggestion { .. }
                        )
                    })
                    .count()
            })
            .unwrap_or(0)
    });

    let pending_suggestion_count: Memo<usize> = Memo::new(move |_| {
        live_for_count
            .get()
            .map(|r| {
                r.suggestions
                    .iter()
                    .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                    .count()
            })
            .unwrap_or(0)
    });

    // Submit gate: status is Draft, comments not empty, AI not running,
    // and no submit echo currently pending.
    let review_id_for_pending = review_id.clone();
    let pending_state = state.clone();
    let action_pending = move || {
        pending_state
            .review_action_pending
            .with(|map| map.get(&review_id_for_pending).copied().unwrap_or_default())
    };

    // ── Submit target picker. The user must choose an explicit
    // destination for the feedback bundle: an existing same-project agent
    // or a freshly spawned one. `None` ⇒ nothing chosen yet (Submit is
    // gated off until a target is picked). The protocol `ReviewSubmitTarget`
    // shape is still settling, so its construction is isolated in
    // `parse_submit_target` — this signal only ever holds the result.
    let submit_target: RwSignal<Option<protocol::ReviewSubmitTarget>> = RwSignal::new(None);
    // Optional instructions for a freshly-spawned target agent. Only
    // meaningful when the picked target is `NewAgent`; folded into the
    // target at submit time (see `on_submit`).
    let submit_new_instructions: RwSignal<String> = RwSignal::new(String::new());
    let submit_new_instructions_for_submit = submit_new_instructions;

    // Project whose live agents are the deliverable submit targets. The
    // review's own origin is deliberately NOT a fallback: project-scoped
    // reviews carry a synthetic origin that is not a live agent. Target
    // resolution lives in the module-level `live_same_project_candidates` /
    // `effective_submit_target` helpers (kept as fns, not closures, so the
    // captures stay `Send` for Leptos).
    let target_project = review.project_id.clone();

    // Returns either an empty string (button enabled) or a short reason
    // string suitable for binding to `title` so hovering a disabled
    // button explains why.
    let submit_reason = {
        let action_pending = action_pending.clone();
        let live = live_for_ai;
        let reason_state = state.clone();
        let reason_host = host_id.clone();
        let reason_project = target_project.clone();
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            let ai_running = live
                .get()
                .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
                .unwrap_or(false);
            if ai_running {
                return "Wait for the AI reviewer to finish";
            }
            if user_comment_count.get() == 0 {
                if pending_suggestion_count.get() > 0 {
                    return "Accept or reject the AI suggestions first, or add a comment";
                }
                return "Add at least one comment first";
            }
            if effective_submit_target(
                &reason_state,
                &reason_host,
                &reason_project,
                submit_target,
                true,
            )
            .is_none()
            {
                return "Choose which agent receives the review";
            }
            if action_pending().submit {
                return "Submit in progress\u{2026}";
            }
            ""
        }
    };
    let submit_disabled = {
        let submit_reason = submit_reason.clone();
        move || !submit_reason().is_empty()
    };

    // ── Run AI reviewer form
    let backend_pick: RwSignal<Option<BackendKind>> = RwSignal::new(None);
    let cost_pick: RwSignal<Option<protocol::SpawnCostHint>> = RwSignal::new(None);
    let instructions: RwSignal<String> = RwSignal::new(String::new());

    let host_for_submit = host_id.clone();
    let review_for_submit = review_id.clone();
    let state_for_submit = state.clone();
    let submit_project = target_project.clone();
    let user_count_for_submit = user_comment_count;
    let pending_count_for_submit = pending_suggestion_count;
    let live_for_submit = live_for_ai;
    let on_submit = move |_| {
        let host = host_for_submit.clone();
        let rid = review_for_submit.clone();
        // Effective deliverable target: explicit picker choice, else a
        // single auto-selected live candidate. No origin fallback — a
        // project-scoped review's synthetic origin is not deliverable.
        let Some(mut submit_target_value) = effective_submit_target(
            &state_for_submit,
            &host,
            &submit_project,
            submit_target,
            false,
        ) else {
            log::warn!("review.submit.click review={rid} skipped=no_deliverable_target");
            return;
        };
        // Fold the optional instructions into a NewAgent target.
        if let protocol::ReviewSubmitTarget::NewAgent { instructions, .. } =
            &mut submit_target_value
        {
            let text = submit_new_instructions_for_submit.get_untracked();
            let trimmed = text.trim();
            *instructions = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        }
        let gate_before = state_for_submit
            .review_action_pending
            .with_untracked(|m| m.get(&rid).copied().unwrap_or_default());
        let status = live_for_submit
            .get_untracked()
            .map(|r| format!("{:?}", std::mem::discriminant(&r.status)))
            .unwrap_or_else(|| "none".to_owned());
        let ai_status = live_for_submit
            .get_untracked()
            .map(|r| match r.ai_reviewer.status {
                ReviewAiReviewerStatus::Idle => "idle",
                ReviewAiReviewerStatus::Running => "running",
                ReviewAiReviewerStatus::Completed => "completed",
                ReviewAiReviewerStatus::Failed => "failed",
            })
            .unwrap_or("unknown");
        let mut claimed = false;
        state_for_submit.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.submit {
                gate.submit = true;
                claimed = true;
            }
        });
        log::info!(
            "review.submit.click review={} claimed={} gate_before_submit={} comments={} pending_suggestions={} ai_status={} status_disc={} {}",
            rid,
            claimed,
            gate_before.submit,
            user_count_for_submit.get_untracked(),
            pending_count_for_submit.get_untracked(),
            ai_status,
            status,
            conn_diag(&state_for_submit, &host)
        );
        if !claimed {
            return;
        }
        let target_state = state_for_submit.clone();
        let target_rid = rid.clone();
        spawn_local(async move {
            match send_review_action_inner(
                &host,
                target_rid.clone(),
                ReviewActionPayload::Submit {
                    target: submit_target_value.clone(),
                },
            )
            .await
            {
                Ok(()) => {
                    log::info!("review.submit.send_ok review={target_rid}");
                }
                Err(e) => {
                    log::error!("review.submit.send_err review={target_rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&target_rid) {
                            gate.submit = false;
                            if gate.is_idle() {
                                map.remove(&target_rid);
                            }
                        }
                    });
                    log::info!(
                        "review.submit.local_gate_clear review={target_rid} reason=send_err"
                    );
                }
            }
        });
    };

    // ── Clear: drop all comments + AI suggestions (and reset the AI
    // reviewer) without delivering anything. Distinct from Cancel, which
    // discards the whole review. Server echoes `Cleared` with the reset
    // (empty) review, which dispatch folds in and which clears the gate.
    let host_for_clear = host_id.clone();
    let review_for_clear = review_id.clone();
    let state_for_clear = state.clone();
    let count_for_clear = user_comment_count;
    let pending_for_clear = pending_suggestion_count;
    let on_clear = move |_| {
        let host = host_for_clear.clone();
        let rid = review_for_clear.clone();
        let target_state = state_for_clear.clone();
        let total = count_for_clear.get_untracked() + pending_for_clear.get_untracked();
        spawn_local(async move {
            let message = format!(
                "Clear {total} comment{} and AI suggestion{} from this review? This cannot be undone.",
                if count_for_clear.get_untracked() == 1 {
                    ""
                } else {
                    "s"
                },
                if pending_for_clear.get_untracked() == 1 {
                    ""
                } else {
                    "s"
                },
            );
            if !crate::bridge::confirm_dialog("Clear review", &message).await {
                log::info!("review.clear.click review={rid} dialog=declined");
                return;
            }
            let mut claimed = false;
            target_state.review_action_pending.update(|map| {
                let gate = map.entry(rid.clone()).or_default();
                if !gate.clear {
                    gate.clear = true;
                    claimed = true;
                }
            });
            log::info!("review.clear.click review={rid} claimed={claimed}");
            if !claimed {
                return;
            }
            match send_review_action_inner(&host, rid.clone(), ReviewActionPayload::ClearComments)
                .await
            {
                Ok(()) => log::info!("review.clear.send_ok review={rid}"),
                Err(e) => {
                    log::error!("review.clear.send_err review={rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&rid) {
                            gate.clear = false;
                            if gate.is_idle() {
                                map.remove(&rid);
                            }
                        }
                    });
                }
            }
        });
    };
    let clear_reason = {
        let action_pending = action_pending.clone();
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            if user_comment_count.get() == 0 && pending_suggestion_count.get() == 0 {
                return "Nothing to clear";
            }
            if action_pending().clear {
                return "Clear in progress\u{2026}";
            }
            ""
        }
    };
    let clear_disabled = {
        let clear_reason = clear_reason.clone();
        move || !clear_reason().is_empty()
    };

    let host_for_ai = host_id.clone();
    let review_for_ai = review_id.clone();
    let state_for_ai = state.clone();
    let ai_reason = {
        let action_pending = action_pending.clone();
        let live = live_for_ai;
        move || -> &'static str {
            if !is_draft.get() {
                return "Review is no longer Draft";
            }
            if backend_pick.get().is_none() {
                return "Choose an AI backend first";
            }
            if action_pending().start_ai {
                return "AI reviewer starting\u{2026}";
            }
            let running = live
                .get()
                .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
                .unwrap_or(false);
            if running {
                return "AI reviewer is already running";
            }
            ""
        }
    };
    let ai_disabled = {
        let ai_reason = ai_reason.clone();
        move || !ai_reason().is_empty()
    };
    let live_for_run_ai = live_for_ai;
    let on_run_ai = move |_| {
        let rid = review_for_ai.clone();
        let Some(backend) = backend_pick.get_untracked() else {
            log::info!("review.start_ai.skipped review={rid} reason=no_backend");
            return;
        };
        let host = host_for_ai.clone();
        let cost = cost_pick.get_untracked();
        let inst = {
            let raw = instructions.get_untracked();
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        };
        let inst_len = inst.as_deref().map(|s| s.len()).unwrap_or(0);
        let gate_before = state_for_ai
            .review_action_pending
            .with_untracked(|m| m.get(&rid).copied().unwrap_or_default());
        let ai_status = live_for_run_ai
            .get_untracked()
            .map(|r| match r.ai_reviewer.status {
                ReviewAiReviewerStatus::Idle => "idle",
                ReviewAiReviewerStatus::Running => "running",
                ReviewAiReviewerStatus::Completed => "completed",
                ReviewAiReviewerStatus::Failed => "failed",
            })
            .unwrap_or("unknown");
        let mut claimed = false;
        state_for_ai.review_action_pending.update(|map| {
            let gate = map.entry(rid.clone()).or_default();
            if !gate.start_ai {
                gate.start_ai = true;
                claimed = true;
            }
        });
        log::info!(
            "review.start_ai.click review={} claimed={} gate_before_start_ai={} backend={:?} cost={:?} instructions_len={} ai_status={} {}",
            rid,
            claimed,
            gate_before.start_ai,
            backend,
            cost,
            inst_len,
            ai_status,
            conn_diag(&state_for_ai, &host)
        );
        if !claimed {
            return;
        }
        let target_state = state_for_ai.clone();
        let target_rid = rid.clone();
        spawn_local(async move {
            let payload = ReviewActionPayload::StartAiReview {
                backend_kind: backend,
                cost_hint: cost,
                instructions: inst,
            };
            match send_review_action_inner(&host, target_rid.clone(), payload).await {
                Ok(()) => {
                    log::info!("review.start_ai.send_ok review={target_rid}");
                }
                Err(e) => {
                    log::error!("review.start_ai.send_err review={target_rid} error={e}");
                    target_state.review_action_pending.update(|map| {
                        if let Some(gate) = map.get_mut(&target_rid) {
                            gate.start_ai = false;
                            if gate.is_idle() {
                                map.remove(&target_rid);
                            }
                        }
                    });
                    log::info!(
                        "review.start_ai.local_gate_clear review={target_rid} reason=send_err"
                    );
                }
            }
        });
    };

    // Backends list — read from host settings if available.
    let backends_state = state.clone();
    let host_for_backends = host_id.clone();
    let backends = move || -> Vec<BackendKind> {
        backends_state
            .host_settings_by_host
            .with(|map| {
                map.get(&host_for_backends)
                    .map(|s| s.enabled_backends.clone())
            })
            .unwrap_or_default()
    };

    // Independent backend reader for the submit-target picker (the AI
    // form already consumed the `backends` closure, which isn't `Copy`).
    let target_backends_state = state.clone();
    let host_for_target_backends = host_id.clone();
    let target_backends = move || -> Vec<BackendKind> {
        target_backends_state
            .host_settings_by_host
            .with(|map| {
                map.get(&host_for_target_backends)
                    .map(|s| s.enabled_backends.clone())
            })
            .unwrap_or_default()
    };

    // Live same-project candidate agents for the picker's "Existing agents"
    // group (plain closure so its captures stay `Send`).
    let candidate_state = state.clone();
    let host_for_candidates = host_id.clone();
    let candidate_project = target_project.clone();
    let candidate_agents = move || -> Vec<(protocol::AgentId, String)> {
        live_same_project_candidates(
            &candidate_state,
            &host_for_candidates,
            &candidate_project,
            true,
        )
    };

    let ai_status_kind = {
        let live = live_for_ai;
        move || -> &'static str {
            match live.get().map(|r| r.ai_reviewer.status) {
                Some(ReviewAiReviewerStatus::Idle) => "idle",
                Some(ReviewAiReviewerStatus::Running) => "running",
                Some(ReviewAiReviewerStatus::Completed) => "completed",
                Some(ReviewAiReviewerStatus::Failed) => "failed",
                None => "unknown",
            }
        }
    };
    let ai_status_text = {
        let live = live_for_ai;
        move || -> &'static str {
            match live.get().map(|r| r.ai_reviewer.status) {
                Some(ReviewAiReviewerStatus::Idle) => "Idle",
                Some(ReviewAiReviewerStatus::Running) => "Running",
                Some(ReviewAiReviewerStatus::Completed) => "Completed",
                Some(ReviewAiReviewerStatus::Failed) => "Failed",
                None => "\u{2014}",
            }
        }
    };

    // Elapsed time tick — only meaningful while Running. setInterval
    // bumps `elapsed_tick` every second; the Effect reinstalls when
    // running state flips so we don't keep timers alive after Completed.
    let live_for_elapsed = live_for_ai;
    let agents_for_elapsed = state.clone();
    let host_for_elapsed = host_id.clone();
    let elapsed_tick = RwSignal::new(0u32);
    let live_for_tick = live_for_ai;
    let interval_slot: ReviewAiIntervalSlot = StoredValue::new_local(None);
    let interval_for_cleanup = interval_slot;
    Effect::new(move |_| {
        let running = live_for_tick
            .get()
            .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
            .unwrap_or(false);
        // Always clear any prior interval before deciding whether to
        // install a new one. setInterval ids leak otherwise.
        interval_slot.update_value(|slot| {
            if let Some((id, _cb)) = slot.take()
                && let Some(window) = web_sys::window()
            {
                window.clear_interval_with_handle(id);
            }
        });
        if !running {
            return;
        }
        let tick = elapsed_tick;
        let cb = Closure::<dyn Fn()>::new(move || {
            tick.update(|t| *t = t.wrapping_add(1));
        });
        if let Some(window) = web_sys::window()
            && let Ok(id) = window.set_interval_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                1000,
            )
        {
            interval_slot.update_value(|slot| *slot = Some((id, cb)));
        }
    });
    on_cleanup(move || {
        interval_for_cleanup.update_value(|slot| {
            if let Some((id, _cb)) = slot.take()
                && let Some(window) = web_sys::window()
            {
                window.clear_interval_with_handle(id);
            }
        });
    });
    let elapsed_label = move || -> Option<String> {
        let _ = elapsed_tick.get();
        let r = live_for_elapsed.get()?;
        if !matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running) {
            return None;
        }
        let agent_id = r.ai_reviewer.agent_id?;
        let host = host_for_elapsed.clone();
        let start_ms = agents_for_elapsed.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == host && a.agent_id == agent_id)
                .map(|a| a.created_at_ms)
        })?;
        Some(format_elapsed(start_ms))
    };

    // AI reviewer agent link — open/focus the reviewer's chat tab so the
    // user can watch its reasoning live.
    let host_for_ai_link = host_id.clone();
    let live_for_ai_link = live_for_ai;
    let ai_state_link = state.clone();
    let ai_agent_link = move || -> Option<protocol::AgentId> {
        live_for_ai_link.get().and_then(|r| r.ai_reviewer.agent_id)
    };
    let on_open_ai_agent = {
        let host = host_for_ai_link.clone();
        let state = ai_state_link.clone();
        move |agent_id: protocol::AgentId| {
            let label = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .find(|a| a.host_id == host && a.agent_id == agent_id)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "AI Review".to_owned())
            });
            state.open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: host.clone(),
                    agent_id,
                }),
                label,
                true,
            );
        }
    };

    // AI reviewer error string surfaced from the server. Local
    // `dismissed_error` holds the value of the last error the user
    // explicitly dismissed; when the server pushes a new error string
    // (different value) the dismiss is automatically cleared so the new
    // failure shows.
    let live_for_ai_err = live_for_ai;
    let dismissed_error: RwSignal<Option<String>> = RwSignal::new(None);
    let ai_error = move || -> Option<String> {
        let err = live_for_ai_err.get().and_then(|r| r.ai_reviewer.error)?;
        if dismissed_error.get().as_deref() == Some(err.as_str()) {
            return None;
        }
        Some(err)
    };

    // Disclosure state for the "Run AI reviewer" form (#6 polish).
    // Defaults closed; force-open while the reviewer is running so users
    // can see what's happening without expanding it manually.
    let ai_disclosure_open = RwSignal::new(false);
    let ai_running_for_disclosure = live_for_ai;
    let ai_open_attr = move || {
        let user_open = ai_disclosure_open.get();
        let running = ai_running_for_disclosure
            .get()
            .map(|r| matches!(r.ai_reviewer.status, ReviewAiReviewerStatus::Running))
            .unwrap_or(false);
        user_open || running
    };
    let _ = status; // silence unused if banner moved out of sidebar

    view! {
        <div class="review-sidebar">
            <div class="review-sidebar-section">
                <h4 class="review-sidebar-title">"Counts"</h4>
                <div class="review-counts" data-test="review-counts">
                    <span class="review-count-line"
                          data-count-comments=move || user_comment_count.get().to_string()>
                        {move || {
                            let n = user_comment_count.get();
                            format!("{n} comment{}", if n == 1 { "" } else { "s" })
                        }}
                    </span>
                    <span class="review-count-line"
                          data-count-pending=move || pending_suggestion_count.get().to_string()>
                        {move || {
                            let n = pending_suggestion_count.get();
                            format!("{n} pending AI suggestion{}", if n == 1 { "" } else { "s" })
                        }}
                    </span>
                </div>
            </div>

            <div class="review-sidebar-section">
                <button
                    class="review-btn primary review-run-ai-btn"
                    disabled=ai_disabled
                    title=ai_reason
                    on:click=move |ev| {
                        // Opening the disclosure on Run is a UX nicety —
                        // the user should always see the reviewer's
                        // current configuration when they kick it off.
                        ai_disclosure_open.set(true);
                        on_run_ai(ev);
                    }
                >
                    "Run AI reviewer"
                </button>
                <details
                    class="review-ai-disclosure"
                    prop:open=ai_open_attr
                    on:toggle=move |ev: leptos::ev::Event| {
                        if let Some(target) = ev.target()
                            && let Ok(el) = target.dyn_into::<web_sys::HtmlDetailsElement>() {
                            ai_disclosure_open.set(el.open());
                        }
                    }
                >
                    <summary class="review-ai-disclosure-summary">
                        "Configure AI reviewer"
                    </summary>
                    <div class="review-ai-disclosure-body">
                        <select
                            class="review-backend-select"
                            on:change=move |ev| {
                                let val = event_target_value(&ev);
                                backend_pick.set(parse_backend_kind(&val));
                            }
                        >
                            <option value="" selected=move || backend_pick.get().is_none()>
                                "Choose backend"
                            </option>
                            {move || backends().into_iter().map(|kind| {
                                let label = backend_kind_label(kind);
                                view! {
                                    <option value={label}>{label}</option>
                                }
                            }).collect::<Vec<_>>()}
                        </select>
                        <select
                            class="review-cost-select"
                            on:change=move |ev| {
                                let val = event_target_value(&ev);
                                cost_pick.set(parse_cost_hint(&val));
                            }
                        >
                            <option value="">"Default cost"</option>
                            <option value="low">"Low"</option>
                            <option value="medium">"Medium"</option>
                            <option value="high">"High"</option>
                        </select>
                        {
                            let instructions_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                            autosize_textarea(instructions_ref, instructions);
                            view! {
                                <textarea
                                    node_ref=instructions_ref
                                    class="review-instructions"
                                    placeholder="Optional instructions for the AI reviewer\u{2026}"
                                    prop:value=move || instructions.get()
                                    on:input=move |ev| instructions.set(event_target_value(&ev))
                                />
                            }
                        }
                    </div>
                </details>
                <div
                    class=move || format!("review-ai-status status-{}", ai_status_kind())
                    data-status=ai_status_kind
                >
                    <span class="review-ai-status-dot"></span>
                    <span class="review-ai-status-label">
                        {move || format!("AI: {}", ai_status_text())}
                    </span>
                    {move || elapsed_label().map(|e| view! {
                        <span class="review-ai-status-elapsed">{e}</span>
                    })}
                </div>
                {
                    let agent_state = state.clone();
                    let host_for_label = host_id.clone();
                    move || ai_agent_link().map(|agent_id| {
                        let click_agent = agent_id.clone();
                        let on_open = on_open_ai_agent.clone();
                        let label = agent_state.agents.with(|agents| {
                            agents
                                .iter()
                                .find(|a| a.host_id == host_for_label && a.agent_id == agent_id)
                                .map(|a| a.name.clone())
                        });
                        let label_text = match label {
                            Some(name) if !name.is_empty() => format!("Open {name}"),
                            _ => "Open AI reviewer chat".to_owned(),
                        };
                        view! {
                            <button
                                class="review-ai-agent-link"
                                title="Open the AI reviewer agent's chat"
                                on:click=move |_| on_open(click_agent.clone())
                            >
                                {label_text}
                            </button>
                        }
                    })
                }
                {move || ai_error().map(|err| {
                    let err_for_dismiss = err.clone();
                    view! {
                        <div class="review-ai-error" data-test="review-ai-error">
                            <div class="review-ai-error-message">{err}</div>
                            <div class="review-ai-error-actions">
                                <button
                                    class="review-btn review-ai-error-dismiss"
                                    on:click=move |_| {
                                        dismissed_error.set(Some(err_for_dismiss.clone()));
                                    }
                                >
                                    "Dismiss"
                                </button>
                            </div>
                        </div>
                    }
                })}
            </div>

            <div class="review-sidebar-section">
                <label class="review-submit-target-label" for="review-submit-target">
                    "Send feedback to"
                </label>
                <select
                    id="review-submit-target"
                    class="review-submit-target-select"
                    data-test="review-submit-target"
                    disabled=move || !is_draft.get()
                    on:change=move |ev| {
                        let val = event_target_value(&ev);
                        submit_target.set(parse_submit_target(&val));
                    }
                >
                    <option value="">"Auto (single same-project agent)"</option>
                    {move || {
                        let agents = candidate_agents();
                        (!agents.is_empty()).then(|| view! {
                            <optgroup label="Existing agents">
                                {agents.into_iter().map(|(id, name)| {
                                    view! {
                                        <option value={format!("existing:{}", id.0)}>{name}</option>
                                    }
                                }).collect::<Vec<_>>()}
                            </optgroup>
                        })
                    }}
                    {move || {
                        let kinds = target_backends();
                        (!kinds.is_empty()).then(|| view! {
                            <optgroup label="Spawn new agent">
                                {kinds.into_iter().map(|kind| {
                                    let label = backend_kind_label(kind);
                                    view! {
                                        <option value={format!("new:{label}")}>
                                            {format!("New {label} agent")}
                                        </option>
                                    }
                                }).collect::<Vec<_>>()}
                            </optgroup>
                        })
                    }}
                </select>
                {move || {
                    // Optional instructions only apply when spawning a new
                    // agent — show the field only for a NewAgent selection.
                    let is_new_agent = matches!(
                        submit_target.get(),
                        Some(protocol::ReviewSubmitTarget::NewAgent { .. })
                    );
                    is_new_agent.then(|| {
                        let instructions_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                        autosize_textarea(instructions_ref, submit_new_instructions);
                        view! {
                            <textarea
                                node_ref=instructions_ref
                                class="review-submit-instructions"
                                data-test="review-submit-instructions"
                                placeholder="Optional instructions for the new agent\u{2026}"
                                prop:value=move || submit_new_instructions.get()
                                on:input=move |ev| {
                                    submit_new_instructions.set(event_target_value(&ev))
                                }
                            />
                        }
                    })
                }}
                <button
                    class="review-btn primary review-submit-btn"
                    disabled=submit_disabled
                    title=submit_reason
                    on:click=on_submit
                >
                    "Submit review"
                </button>
                <button
                    class="review-btn review-clear-btn"
                    data-test="review-clear-btn"
                    disabled=clear_disabled
                    title=clear_reason
                    on:click=on_clear
                >
                    "Clear comments"
                </button>
            </div>
        </div>
    }
}

fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Gemini => "Gemini",
    }
}

/// Translate a submit-target picker option value into a `ReviewSubmitTarget`.
/// `None` ⇒ no concrete target selected ("Choose where to send…"). This is
/// the single place that constructs the still-settling `ReviewSubmitTarget`
/// shape, so a protocol change only has to be reconciled here.
fn parse_submit_target(value: &str) -> Option<protocol::ReviewSubmitTarget> {
    if let Some(id) = value.strip_prefix("existing:") {
        Some(protocol::ReviewSubmitTarget::ExistingAgent {
            agent_id: protocol::AgentId(id.to_owned()),
        })
    } else if let Some(label) = value.strip_prefix("new:") {
        parse_backend_kind(label).map(|backend_kind| protocol::ReviewSubmitTarget::NewAgent {
            backend_kind,
            cost_hint: None,
            custom_agent_id: None,
            name: None,
            instructions: None,
        })
    } else {
        None
    }
}

/// Live same-project agents on `host_id` for `project_id` — the deliverable
/// review submit targets. "Live" excludes agents with a fatal error
/// (terminated). The review's own origin is intentionally never consulted:
/// project-scoped reviews use a synthetic, non-deliverable origin. The
/// `tracked` flag selects reactive vs untracked signal reads so the same
/// logic serves both the reactive submit gate and the click handler.
fn live_same_project_candidates(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    tracked: bool,
) -> Vec<(protocol::AgentId, String)> {
    let collect = |agents: &Vec<AgentInfo>| -> Vec<(protocol::AgentId, String)> {
        agents
            .iter()
            .filter(|a| {
                a.host_id == host_id
                    && a.project_id.as_ref() == Some(project_id)
                    && a.fatal_error.is_none()
            })
            .map(|a| {
                let name = if a.name.is_empty() {
                    let short: String = a.agent_id.0.chars().take(8).collect();
                    format!("agent {short}")
                } else {
                    a.name.clone()
                };
                (a.agent_id.clone(), name)
            })
            .collect()
    };
    if tracked {
        state.agents.with(collect)
    } else {
        state.agents.with_untracked(collect)
    }
}

/// The deliverable submit target: an explicit picker choice if present,
/// else the sole live same-project candidate when exactly one exists.
/// `None` ⇒ no deliverable target (Submit is gated off; the user must pick).
fn effective_submit_target(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    submit_target: RwSignal<Option<protocol::ReviewSubmitTarget>>,
    tracked: bool,
) -> Option<protocol::ReviewSubmitTarget> {
    let explicit = if tracked {
        submit_target.get()
    } else {
        submit_target.get_untracked()
    };
    if explicit.is_some() {
        return explicit;
    }
    let candidates = live_same_project_candidates(state, host_id, project_id, tracked);
    if let [(agent_id, _)] = candidates.as_slice() {
        return Some(protocol::ReviewSubmitTarget::ExistingAgent {
            agent_id: agent_id.clone(),
        });
    }
    None
}

fn parse_backend_kind(s: &str) -> Option<BackendKind> {
    match s {
        "Tycode" => Some(BackendKind::Tycode),
        "Kiro" => Some(BackendKind::Kiro),
        "Claude" => Some(BackendKind::Claude),
        "Codex" => Some(BackendKind::Codex),
        "Gemini" => Some(BackendKind::Gemini),
        _ => None,
    }
}

fn parse_cost_hint(s: &str) -> Option<protocol::SpawnCostHint> {
    match s {
        "low" => Some(protocol::SpawnCostHint::Low),
        "medium" => Some(protocol::SpawnCostHint::Medium),
        "high" => Some(protocol::SpawnCostHint::High),
        _ => None,
    }
}

async fn send_review_action_inner(
    host_id: &str,
    review_id: protocol::ReviewId,
    payload: ReviewActionPayload,
) -> Result<(), String> {
    let stream = StreamPath(format!("/review/{}", review_id.0));
    send_frame(host_id, stream, FrameKind::ReviewAction, &payload).await
}

/// Short string describing the current connection state for diagnostics.
/// Format: `conn=<connected|connecting|disconnected|error> has_host_stream=<bool>`
fn conn_diag(state: &AppState, host_id: &str) -> String {
    let status = state
        .connection_statuses
        .with_untracked(|m| m.get(host_id).cloned());
    let label = match status {
        Some(crate::state::ConnectionStatus::Connected) => "connected",
        Some(crate::state::ConnectionStatus::Connecting) => "connecting",
        Some(crate::state::ConnectionStatus::Disconnected) => "disconnected",
        Some(crate::state::ConnectionStatus::Error(_)) => "error",
        None => "unknown",
    };
    let has_stream = state
        .host_streams
        .with_untracked(|m| m.contains_key(host_id));
    format!("conn={label} has_host_stream={has_stream}")
}

/// Fire a `ReviewAction` and clear the corresponding per-target gate on
/// local send failure (so the buttons re-enable). On success the gate
/// remains set until dispatch sees the matching server echo or error.
pub(crate) async fn send_review_action_with_failure_clear(
    state: AppState,
    host_id: &str,
    review_id: protocol::ReviewId,
    payload: ReviewActionPayload,
    target: ReviewActionTarget,
) {
    let target_label = target_label(&target);
    match send_review_action_inner(host_id, review_id.clone(), payload).await {
        Ok(()) => {
            log::info!("review.action.send_ok review={review_id} target={target_label}");
        }
        Err(e) => {
            log::error!(
                "review.action.send_err review={review_id} target={target_label} error={e}"
            );
            state.review_action_target_pending.update(|set| {
                set.remove(&(review_id.clone(), target));
            });
            log::info!(
                "review.action.local_target_gate_clear review={review_id} target={target_label} reason=send_err"
            );
        }
    }
}

fn target_label(target: &ReviewActionTarget) -> &'static str {
    match target {
        ReviewActionTarget::AddComment => "add_comment",
        ReviewActionTarget::UpdateComment(_) => "update_comment",
        ReviewActionTarget::DeleteComment(_) => "delete_comment",
        ReviewActionTarget::AcceptSuggestion(_) => "accept_suggestion",
        ReviewActionTarget::RejectSuggestion(_) => "reject_suggestion",
    }
}

/// Atomic re-entry guard for action handlers. Returns `true` if the
/// caller should proceed (gate was newly set), `false` if a request for
/// this exact `(review_id, target)` is already in flight.
///
/// Synchronous JS event dispatch can fire multiple `on:click` handlers
/// before Leptos flushes the reactive `disabled` attribute to the DOM,
/// so the visual disable alone does not prevent re-entry — handlers
/// must check this guard before sending.
pub(crate) fn try_claim_review_action(
    state: &AppState,
    review_id: &protocol::ReviewId,
    target: &ReviewActionTarget,
) -> bool {
    let mut claimed = false;
    state.review_action_target_pending.update(|set| {
        claimed = set.insert((review_id.clone(), target.clone()));
    });
    log::info!(
        "review.action.claim review={} target={} claimed={}",
        review_id,
        target_label(target),
        claimed
    );
    claimed
}

/// The most-recently-updated Draft review id for `(project_id, root)`, if
/// any. Submitted/Consumed/Cancelled are filtered out — only an actively
/// editable Draft is a review surface. Active reviews are per
/// `(project, root)`, so a Draft on a *different* root of the same project
/// must not be returned here (it would wrongly suppress creating one for
/// this root).
fn existing_draft_for_root(
    state: &AppState,
    project_id: &protocol::ProjectId,
    root: &protocol::ProjectRootPath,
) -> Option<protocol::ReviewId> {
    state.review_summaries.with_untracked(|map| {
        map.get(project_id).and_then(|summaries| {
            summaries
                .iter()
                .filter(|s| s.root == *root && matches!(s.status, ReviewStatus::Draft))
                .max_by_key(|s| s.updated_at_ms)
                .map(|s| s.id.clone())
        })
    })
}

/// Send a root-scoped `ReviewCreate` with create-pending gating, with
/// no tab navigation — callers decide which surface to show. At most one
/// Draft per `(project, root)`: callers must check `existing_draft_for_root`
/// first.
///
/// The selection is root-scoped `Unstaged` to match the active-review
/// model: a project-wide `AllUncommitted` create is ambiguous on multi-root
/// projects and the server now normalizes every active review to
/// `Root { scope: Unstaged }`.
///
/// The create-pending gate is keyed by `(host, project)`, not the root: the
/// only create path ([`create_review_for_active_agent`]) resolves a single
/// root per project (the first unstaged one), and the `CommandError` clear
/// path in dispatch only recovers `(host, project)` from the `/project/{id}`
/// stream — it has no root — so a per-root key could not be released on
/// error. The gate is a transient debounce, not lifecycle state, so
/// project-keying is sufficient.
fn send_review_create(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    root: &protocol::ProjectRootPath,
) {
    let mut claimed = false;
    let key = (host_id.to_owned(), project_id.clone());
    state.review_create_pending.update(|map| {
        let entry = map.entry(key.clone()).or_insert(0);
        if *entry == 0 {
            *entry = 1;
            claimed = true;
        }
    });
    if !claimed {
        return;
    }

    let stream = StreamPath(format!("/project/{}", project_id.0));
    let payload = protocol::ReviewCreatePayload {
        selection: protocol::ReviewDiffSelection::Root {
            root: root.clone(),
            scope: ProjectDiffScope::Unstaged,
            path: None,
        },
    };
    let state_for_failure = state.clone();
    let host = host_id.to_owned();
    spawn_local(async move {
        if let Err(e) = send_frame(&host, stream, FrameKind::ReviewCreate, &payload).await {
            log::error!("failed to send ReviewCreate: {e}");
            state_for_failure.review_create_pending.update(|map| {
                if let Some(count) = map.get_mut(&key) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&key);
                    }
                }
            });
        }
    });
}

/// First root with an unstaged change (modified-but-unstaged or untracked)
/// for `project_id`, or `None` when nothing is reviewable. Reviews track
/// `Unstaged`, so a staged-only root is skipped — it would open an empty
/// review surface. Shared by the diff-surface opener and the legacy
/// `ReviewCreate` fallback so both agree on which root a review covers.
fn review_root_for_project(
    state: &AppState,
    project_id: &protocol::ProjectId,
) -> Option<protocol::ProjectRootPath> {
    state.git_status.with_untracked(|m| {
        m.get(project_id)?
            .iter()
            .find_map(|r| root_has_reviewable_changes(r).then(|| r.root.clone()))
    })
}

/// Whether a root carries changes an always-on (`Unstaged`) review would
/// cover: any file that is modified-but-unstaged or untracked. Staged-only
/// files do not count — they are not part of the active inline review.
fn root_has_reviewable_changes(root: &protocol::ProjectRootGitStatus) -> bool {
    root.files
        .iter()
        .any(|f| f.unstaged.is_some() || f.untracked)
}

/// Open (or focus) the whole-root unstaged diff surface for an explicit
/// `(host, project, root)` — the review surface for *that exact* root.
///
/// An active review covers a root's unstaged changes, so this opens the
/// whole-root diff (empty `path` ⇒ all files) at
/// [`ProjectDiffScope::Unstaged`], not a single file: every changed file
/// in the review must be able to display and accept comments. The empty path
/// is the established "all files" convention `DiffView` already renders and
/// is requested with `ProjectReadDiff { path: None }`.
///
/// The scope is deliberately `Unstaged` (index↔worktree): the server anchors
/// active review diffs and validates comment anchors against `Unstaged`. A
/// `Staged` or `Uncommitted` tab would show line numbers that don't line up
/// with the review's anchors, so `ReviewableDiffView` only binds review
/// affordances on `Unstaged` tabs.
///
/// Unlike [`open_changed_diff_for_project`], the caller picks the root, so a
/// multi-root project opens the clicked root (e.g. the git panel's per-root
/// review hub) rather than the first dirty one.
pub fn open_changed_diff_for_root(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
    root: &protocol::ProjectRootPath,
) {
    let scope = ProjectDiffScope::Unstaged;
    // Empty path ⇒ all files in the root (the whole-root unstaged surface).
    let path = String::new();

    let label = format!("Review: {}", root_display_name(root));
    state.open_tab(
        TabContent::Diff {
            host_id: host_id.to_owned(),
            project_id: project_id.clone(),
            root: root.clone(),
            scope,
            path: path.clone(),
        },
        label,
        true,
    );

    // Seed a pending DiffViewState and request the whole-root diff.
    let context_mode = state.diff_context_mode.get_untracked();
    let key = crate::state::DiffKey::new(host_id, project_id.clone(), root.clone(), scope, path);
    state.diff_contents.update(|diffs| {
        let previous = diffs.get(&key);
        // `None` path ⇒ all files; dispatch keys the response back to the
        // empty-path entry.
        let next = DiffViewState::for_request(previous, root.clone(), scope, None, context_mode);
        diffs.insert(key, next);
    });
    let stream = StreamPath(format!("/project/{}", project_id.0));
    let host = host_id.to_owned();
    let root = root.clone();
    spawn_local(async move {
        let payload = ProjectReadDiffPayload {
            root,
            scope,
            path: None,
            context_mode,
        };
        if let Err(e) = send_frame(&host, stream, FrameKind::ProjectReadDiff, &payload).await {
            log::error!("failed to send ProjectReadDiff for review surface: {e}");
        }
    });
}

/// Open (or focus) the whole-root unstaged diff surface for `project_id`'s
/// first root with reviewable changes — the canonical review surface for the
/// integrated flow. Returns true when a diff tab was opened.
///
/// (Multi-root projects: this opens the first root that has unstaged changes.
/// A single diff tab is per-root; other roots' changes need their own tab —
/// see [`open_changed_diff_for_root`].)
///
/// This is what the chat "Review changes" CTA routes through; reviews live on
/// the ordinary diff surfaces now, never in a standalone `TabContent::Review`
/// workbench.
pub fn open_changed_diff_for_project(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
) -> bool {
    let Some(root) = review_root_for_project(state, project_id) else {
        return false;
    };
    open_changed_diff_for_root(state, host_id, project_id, &root);
    true
}

/// Reactively keep `/review/<id>` subscribed for a (possibly changing)
/// target review, so a surface can render its comments/suggestions. Call
/// once on mount.
///
/// `target` yields `Some((host, review_id))` for the review to track, or
/// `None` when there's nothing to subscribe to (e.g. no draft yet).
///
/// It subscribes whenever the target's full record is absent from
/// `state.reviews`, the host is connected, and no subscribe for that exact
/// id is already in flight. Retry behavior is designed to be robust rather
/// than tight-looping:
/// * a **send failure** schedules a retry behind an exponential backoff
///   timer (250ms, 500ms, 1s … capped at 30s) — never an immediate clear,
///   which would spin on a persistent failure while connected;
/// * a **disconnect** drops the in-flight marker (the server-side
///   subscription is gone with the connection) so **reconnect resubscribes**
///   — this also recovers the "subscribed OK but no bootstrap ever arrived,
///   then reconnected" case, which an in-flight latch would otherwise wedge;
/// * the record being **lost later** (e.g. cleared) re-runs the effect and
///   resubscribes;
/// * the target id **changing** re-subscribes for the new id.
pub(crate) fn subscribe_review_reactive(
    state: &AppState,
    target: Memo<Option<(String, protocol::ReviewId)>>,
) {
    let state = state.clone();
    // The review id we currently have an outstanding `ReviewSubscribe` for
    // (send in flight, or sent and awaiting the bootstrap). Reactive so the
    // backoff timer / disconnect handling can clear it and re-run the effect.
    let in_flight: RwSignal<Option<protocol::ReviewId>> = RwSignal::new(None);
    // Bumped by the backoff timer to re-trigger a retry after a failure.
    let retry_tick: RwSignal<u32> = RwSignal::new(0);
    // Consecutive send-failure count, drives the backoff delay.
    let fail_count: StoredValue<u32, LocalStorage> = StoredValue::new_local(0);
    // Pending backoff timer (handle + its closure), cleared on cleanup or
    // whenever we move on.
    let timer: SubscribeTimerSlot = StoredValue::new_local(None);

    Effect::new(move |_| {
        // Tracked so a backoff-timer fire re-runs this effect.
        let _ = retry_tick.get();
        let Some((host, review_id)) = target.get() else {
            // No target right now (e.g. the owning project's draft resolved
            // away, or `state.projects` was cleared on disconnect). Drop any
            // outstanding subscription state so a later target — even the
            // same review id reappearing — resubscribes cleanly instead of
            // staying latched on a stale in-flight marker.
            clear_subscribe_timer(timer);
            fail_count.set_value(0);
            if in_flight.get_untracked().is_some() {
                in_flight.set(None);
            }
            return;
        };
        let present = state.reviews.with(|m| m.contains_key(&review_id));
        let connected = state.connection_statuses.with(|m| {
            matches!(
                m.get(&host),
                Some(crate::state::ConnectionStatus::Connected)
            )
        });

        if present {
            // Record in hand: reset retry state and drop the in-flight marker
            // so a later loss (reviews change ⇒ re-run) resubscribes.
            clear_subscribe_timer(timer);
            fail_count.set_value(0);
            if in_flight.get_untracked().as_ref() == Some(&review_id) {
                in_flight.set(None);
            }
            return;
        }
        if !connected {
            // No connection ⇒ any server-side subscription is gone. Drop the
            // in-flight marker (and any pending retry) so reconnect — which
            // re-runs this effect — resubscribes.
            clear_subscribe_timer(timer);
            fail_count.set_value(0);
            if in_flight.get_untracked().is_some() {
                in_flight.set(None);
            }
            return;
        }
        if in_flight.get().as_ref() == Some(&review_id) {
            // A subscribe for this exact id is already outstanding.
            return;
        }

        // Subscribe now.
        clear_subscribe_timer(timer);
        in_flight.set(Some(review_id.clone()));
        let stream = StreamPath(format!("/review/{}", review_id.0));
        let host_for_send = host.clone();
        let rid = review_id.clone();
        spawn_local(async move {
            if let Err(e) = send_frame(
                &host_for_send,
                stream,
                FrameKind::ReviewSubscribe,
                &serde_json::json!({}),
            )
            .await
            {
                log::error!("review.subscribe.err review={rid} error={e}");
                // Schedule a backoff retry — only if we're still the
                // in-flight target (a target change may have moved us on).
                if in_flight.get_untracked().as_ref() != Some(&rid) {
                    return;
                }
                let n = fail_count.get_value();
                fail_count.set_value(n.saturating_add(1));
                let delay = subscribe_backoff_ms(n);
                let cb = Closure::<dyn Fn()>::new(move || {
                    // Re-arm: drop the marker and bump the tick so the effect
                    // re-runs and re-attempts (after the backoff, not in a
                    // tight loop).
                    in_flight.set(None);
                    retry_tick.update(|t| *t = t.wrapping_add(1));
                });
                if let Some(window) = web_sys::window()
                    && let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        cb.as_ref().unchecked_ref(),
                        delay,
                    )
                {
                    timer.update_value(|slot| *slot = Some((id, cb)));
                }
            }
        });
    });

    on_cleanup(move || clear_subscribe_timer(timer));
}

type SubscribeTimerSlot = StoredValue<Option<(i32, Closure<dyn Fn()>)>, LocalStorage>;

/// Cancel and drop a pending review-subscribe backoff timer.
fn clear_subscribe_timer(timer: SubscribeTimerSlot) {
    timer.update_value(|slot| {
        if let Some((id, _cb)) = slot.take()
            && let Some(window) = web_sys::window()
        {
            window.clear_timeout_with_handle(id);
        }
    });
}

/// Exponential backoff for review-subscribe retries: 250ms, 500ms, 1s, 2s …
/// capped at 30s.
fn subscribe_backoff_ms(failures: u32) -> i32 {
    const BASE_MS: i64 = 250;
    const CAP_MS: i64 = 30_000;
    let delay = BASE_MS
        .checked_shl(failures.min(20))
        .unwrap_or(CAP_MS)
        .min(CAP_MS);
    delay as i32
}

/// Public entry — used by the agent chat header's "Review changes" button.
///
/// Reviews are always-on and root-scoped server-side, so this is primarily
/// navigation: it opens/focuses the project's normal changed-file diff tab
/// (the canonical inline review surface). The active root review's
/// decorations resolve on that diff tab from the bootstrap/summary stream.
/// If no Draft summary has arrived yet, it also fires a legacy
/// get-or-create `ReviewCreate` as a fallback so the surface is never blank
/// against an older server. We never open a standalone `TabContent::Review`
/// workbench — that surface has been retired.
pub fn create_review_for_active_agent(state: &AppState) {
    let Some(active_agent) = state.active_agent.get_untracked() else {
        return;
    };
    let host_id = active_agent.host_id.clone();
    let agent_id = active_agent.agent_id.clone();

    let project_id = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == host_id && a.agent_id == agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        log::warn!("create_review_for_active_agent: agent has no project — skipping");
        return;
    };

    // Always land the user on the changed-file diff surface.
    open_changed_diff_for_project(state, &host_id, &project_id);

    // Create only when there's an unstaged root to anchor the review to, and
    // only when *that root* has no Draft yet. Active reviews are per
    // `(project, root)`, so a Draft on a different root must not suppress
    // creating one for the root we just opened.
    if let Some(root) = review_root_for_project(state, &project_id)
        && existing_draft_for_root(state, &project_id, &root).is_none()
    {
        send_review_create(state, &host_id, &project_id, &root);
    }
}

/// Returns true when the active agent's project has changes an always-on
/// review would cover — any modified-but-unstaged or untracked file in any
/// root (driving the visibility of the "Review changes" header button).
///
/// Staged-only roots are excluded: reviews track `Unstaged`, and
/// [`review_root_for_project`] (which the click handler uses to pick a root)
/// would skip them, so showing the button there would do nothing.
pub fn active_agent_has_reviewable_changes(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    let project_id = state.agents.with(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        return false;
    };
    state.git_status.with(|map| {
        map.get(&project_id)
            .is_some_and(|roots| roots.iter().any(root_has_reviewable_changes))
    })
}

/// Whether a `ReviewCreate` is currently in flight for the active agent's
/// project — used to disable the "Review changes" button until the
/// server's `ReviewListChanged` echoes back.
pub fn active_agent_review_create_pending(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    let project_id = state.agents.with(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .and_then(|a| a.project_id.clone())
    });
    let Some(project_id) = project_id else {
        return false;
    };
    state
        .review_create_pending
        .with(|map| map.contains_key(&(active_agent.host_id, project_id)))
}

// ── BTreeMap import is referenced by tests below ───────────────────────
#[allow(dead_code)]
fn _btree_marker() -> BTreeMap<u32, u32> {
    BTreeMap::new()
}

// `ReviewActionGate` is referenced via `state.review_action_pending`; touch
// the import so removal of the field fails compilation cleanly.
#[allow(dead_code)]
fn _gate_marker() -> ReviewActionGate {
    ReviewActionGate::default()
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::AgentInfo;
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, DiffContextMode, ProjectDiffScope, ProjectGitDiffFile,
        ProjectGitDiffHunk, ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectGitDiffPayload,
        ProjectId, ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus,
        ReviewAnchor, ReviewAnchorStatus, ReviewComment, ReviewCommentId, ReviewCommentSource,
        ReviewDiffSelection, ReviewDiffSide, ReviewId, ReviewLocation, ReviewSeverity,
        ReviewStatus, ReviewSuggestedComment, ReviewSuggestionId, ReviewSuggestionState, SessionId,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Element, HtmlElement};

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document.get_element_by_id("test-prod-styles-app").is_none() {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-app");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 1100px; height: 800px;",
            )
            .unwrap();
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

    fn root_path() -> ProjectRootPath {
        ProjectRootPath("/repo".to_owned())
    }

    fn diff_payload() -> ProjectGitDiffPayload {
        ProjectGitDiffPayload {
            root: root_path(),
            scope: ProjectDiffScope::Uncommitted,
            path: None,
            context_mode: DiffContextMode::FullFile,
            files: vec![
                ProjectGitDiffFile {
                    relative_path: "src/foo.rs".to_owned(),
                    is_binary: false,
                    hunks: vec![ProjectGitDiffHunk {
                        hunk_id: "src/foo.rs:1".to_owned(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 5,
                        lines: vec![
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Context,
                                text: "fn handle()".to_owned(),
                                old_line_number: Some(1),
                                new_line_number: Some(1),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let x = 1;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(2),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let y = 2;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(3),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let z = 3;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(4),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    let w = 4;".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(5),
                            },
                        ],
                    }],
                },
                ProjectGitDiffFile {
                    relative_path: "src/bar.rs".to_owned(),
                    is_binary: false,
                    hunks: vec![ProjectGitDiffHunk {
                        hunk_id: "src/bar.rs:1".to_owned(),
                        old_start: 1,
                        old_count: 1,
                        new_start: 1,
                        new_count: 2,
                        lines: vec![
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Context,
                                text: "fn other()".to_owned(),
                                old_line_number: Some(1),
                                new_line_number: Some(1),
                            },
                            ProjectGitDiffLine {
                                kind: ProjectGitDiffLineKind::Added,
                                text: "    bar();".to_owned(),
                                old_line_number: None,
                                new_line_number: Some(2),
                            },
                        ],
                    }],
                },
            ],
        }
    }

    fn make_review() -> Review {
        Review {
            id: ReviewId("rev-1".to_owned()),
            project_id: ProjectId("proj-1".to_owned()),
            origin_agent_id: AgentId("agent-1".to_owned()),
            origin_session_id: SessionId("sess-1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status: ReviewStatus::Draft,
            diffs: vec![diff_payload()],
            comments: vec![],
            suggestions: vec![],
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn make_agent(name: &str, agent_id: &str, project: Option<&str>) -> AgentInfo {
        AgentInfo {
            host_id: "h1".to_owned(),
            agent_id: AgentId(agent_id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Tycode,
            workspace_roots: vec![],
            project_id: project.map(|s| ProjectId(s.to_owned())),
            parent_agent_id: None,
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath("s".to_owned()),
            started: true,
            fatal_error: None,
        }
    }

    fn comment_at_line(line: u32, body: &str) -> ReviewComment {
        ReviewComment {
            id: ReviewCommentId(format!("c-{line}-{body}")),
            location: ReviewLocation {
                root: root_path(),
                relative_path: "src/foo.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: line,
                    end_line: line,
                },
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: body.to_owned(),
            source: ReviewCommentSource::User,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn pending_suggestion_at_line(line: u32, body: &str) -> ReviewSuggestedComment {
        ReviewSuggestedComment {
            id: ReviewSuggestionId(format!("s-{line}")),
            location: ReviewLocation {
                root: root_path(),
                relative_path: "src/foo.rs".to_owned(),
                anchor: ReviewAnchor::LineRange {
                    side: ReviewDiffSide::New,
                    start_line: line,
                    end_line: line,
                },
            },
            anchor_status: ReviewAnchorStatus::Current,
            body: body.to_owned(),
            rationale: None,
            severity: ReviewSeverity::Warn,
            state: ReviewSuggestionState::Pending,
            reviewer_agent_id: AgentId("ai-1".to_owned()),
            created_at_ms: 1,
        }
    }

    /// Mounts the shared `ReviewSidebar` (the surviving review-controls
    /// surface — the standalone three-pane workbench has been retired) with
    /// the given review pre-seeded. Returns the captured `AppState` so the
    /// test can drive signal updates that mirror dispatch events. `is_draft`
    /// derives reactively from the live review status, exactly as the
    /// git-panel per-root hub wires it. The mount handle is leaked so the
    /// view stays alive for the duration of the test.
    fn mount_sidebar(
        container: HtmlElement,
        review: Review,
    ) -> std::rc::Rc<std::cell::RefCell<Option<AppState>>> {
        let state_holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_holder_for_mount = state_holder.clone();
        let host_id = "h1".to_owned();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            state.reviews.update(|map| {
                map.insert(review.id.clone(), review.clone());
            });
            *state_holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state.clone());
            let rid = review.id.clone();
            let st = state.clone();
            let is_draft = Memo::new(move |_| {
                st.reviews.with(|m| {
                    m.get(&rid)
                        .map(|r| matches!(r.status, ReviewStatus::Draft))
                        .unwrap_or(false)
                })
            });
            view! {
                <ReviewSidebar
                    review=review.clone()
                    host_id=host_id.clone()
                    review_id=review.id.clone()
                    is_draft=is_draft
                />
            }
        });
        std::mem::forget(handle);
        state_holder
    }

    /// Submit gating: disabled with zero comments, stays disabled with a
    /// comment but no deliverable target, enables with a comment and exactly
    /// one live same-project agent, and disables again once the review
    /// leaves Draft.
    #[wasm_bindgen_test]
    async fn submit_button_disabled_until_draft_with_comments() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must be disabled with zero comments"
        );

        // Add a comment — necessary but NOT sufficient: with no live
        // same-project agent there is no deliverable target, so submit
        // stays disabled.
        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "fix this"));
            }
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must stay disabled with a comment but no deliverable target"
        );

        // Seed exactly one live same-project agent — it auto-selects as the
        // target, so submit now enables.
        state.agents.update(|agents| {
            agents.push(make_agent("Only Candidate", "a-only", Some("proj-1")));
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            !submit.has_attribute("disabled"),
            "submit must enable with a comment and exactly one live same-project agent"
        );

        // Flip the review to Submitted — submit must disable again.
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.status = ReviewStatus::Submitted {
                    submitted_at_ms: 999,
                };
            }
        });
        next_tick().await;

        let submit =
            find_button_by_text(&container, "Submit review").expect("submit button rendered");
        assert!(
            submit.has_attribute("disabled"),
            "submit must disable when status leaves Draft"
        );
    }

    /// The submit-target picker offers only same-project agents as
    /// explicit destinations — an agent belonging to a different project
    /// must never appear, since feedback can only route within the
    /// review's own project.
    #[wasm_bindgen_test]
    async fn submit_target_picker_lists_only_same_project_agents() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review(); // project_id = "proj-1", host = "h1"
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // Seed one same-project agent and one from a different project.
        let state = state_holder.borrow().clone().unwrap();
        state.agents.update(|agents| {
            agents.push(make_agent("Backend Codex", "a-same", Some("proj-1")));
            agents.push(make_agent("Other Project", "a-other", Some("proj-2")));
        });
        next_tick().await;

        let select = container
            .query_selector("[data-test=\"review-submit-target\"]")
            .unwrap()
            .expect("submit target picker rendered");
        let text = select.text_content().unwrap_or_default();
        assert!(
            text.contains("Auto (single same-project agent)"),
            "default auto-target option missing; got: {text}"
        );
        assert!(
            text.contains("Backend Codex"),
            "same-project agent must be offered as a target; got: {text}"
        );
        assert!(
            !text.contains("Other Project"),
            "an agent from a different project must not be offered; got: {text}"
        );
    }

    /// The Clear control is gated on there being something to clear: it
    /// is disabled when the review has no comments or suggestions, and
    /// enables once a comment exists.
    #[wasm_bindgen_test]
    async fn clear_button_enables_with_content() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let clear =
            find_button_by_text(&container, "Clear comments").expect("clear button rendered");
        assert!(
            clear.has_attribute("disabled"),
            "clear must be disabled when there is nothing to clear"
        );

        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "please fix"));
            }
        });
        next_tick().await;

        let clear =
            find_button_by_text(&container, "Clear comments").expect("clear button rendered");
        assert!(
            !clear.has_attribute("disabled"),
            "clear must enable once the review has a comment"
        );
    }

    /// Sidebar count derives reactively from the comments/suggestions
    /// signal — the visible text reflects the live counts.
    #[wasm_bindgen_test]
    async fn sidebar_counts_derive_reactively() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"review-counts\"]")
            .unwrap()
            .expect("counts panel mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        assert!(
            counts_el
                .text_content()
                .unwrap_or_default()
                .contains("0 comments"),
            "expected initial '0 comments', got: {}",
            counts_el.text_content().unwrap_or_default()
        );

        let state = state_holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "first"));
                r.comments.push(comment_at_line(3, "second"));
                r.suggestions.push(pending_suggestion_at_line(2, "ai"));
            }
        });
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"review-counts\"]")
            .unwrap()
            .expect("counts panel mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        let txt = counts_el.text_content().unwrap_or_default();
        assert!(
            txt.contains("2 comments"),
            "expected '2 comments' in: {txt}"
        );
        assert!(
            txt.contains("1 pending AI"),
            "expected '1 pending AI' in: {txt}"
        );
    }

    /// Run AI button is disabled until a backend is chosen. We assert via
    /// the `disabled` attribute, which is user-perceived.
    #[wasm_bindgen_test]
    async fn run_ai_disabled_until_backend_chosen() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let _ = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        assert!(
            run_btn.has_attribute("disabled"),
            "Run AI must be disabled before any backend is chosen"
        );
    }

    fn root_status(
        path: &str,
        staged: Option<protocol::ProjectGitChangeKind>,
        unstaged: Option<protocol::ProjectGitChangeKind>,
        untracked: bool,
    ) -> protocol::ProjectRootGitStatus {
        protocol::ProjectRootGitStatus {
            root: ProjectRootPath(path.to_owned()),
            branch: Some("main".to_owned()),
            ahead: 0,
            behind: 0,
            clean: staged.is_none() && unstaged.is_none() && !untracked,
            files: vec![protocol::ProjectGitFileStatus {
                relative_path: "src/foo.rs".to_owned(),
                staged,
                unstaged,
                untracked,
            }],
        }
    }

    /// Fix 2: the chat "Review changes" button (gated on
    /// `active_agent_has_reviewable_changes`) must stay hidden for a
    /// staged-only root — reviews track `Unstaged`, so there'd be nothing to
    /// open — and appear once an unstaged change exists.
    #[wasm_bindgen_test]
    async fn reviewable_changes_excludes_staged_only() {
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .agents
                .update(|a| a.push(make_agent("Agent", "a1", Some("proj-1"))));
            // `active_agent` is a Memo derived from the active Chat tab.
            state.open_tab(
                TabContent::chat_with_agent(ActiveAgentRef {
                    host_id: "h1".to_owned(),
                    agent_id: AgentId("a1".to_owned()),
                }),
                "chat".to_owned(),
                true,
            );
            // Staged-only root: not reviewable.
            state.git_status.update(|m| {
                m.insert(
                    ProjectId("proj-1".to_owned()),
                    vec![root_status(
                        "/repo",
                        Some(protocol::ProjectGitChangeKind::Modified),
                        None,
                        false,
                    )],
                );
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            let s = state.clone();
            provide_context(state);
            view! {
                <div data-test="rc">
                    {move || if active_agent_has_reviewable_changes(&s) { "yes" } else { "no" }}
                </div>
            }
        });
        std::mem::forget(handle);
        next_tick().await;

        let read = || {
            container
                .query_selector("[data-test=\"rc\"]")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap_or_default()
        };
        assert_eq!(
            read(),
            "no",
            "staged-only changes must not make the project reviewable"
        );

        // Flip the same root to an unstaged change ⇒ now reviewable.
        let state = holder.borrow().clone().unwrap();
        state.git_status.update(|m| {
            m.insert(
                ProjectId("proj-1".to_owned()),
                vec![root_status(
                    "/repo",
                    None,
                    Some(protocol::ProjectGitChangeKind::Modified),
                    false,
                )],
            );
        });
        next_tick().await;
        assert_eq!(
            read(),
            "yes",
            "an unstaged change must make the project reviewable"
        );
    }

    /// Fix 3: `existing_draft_for_root` is per `(project, root)`. A Draft on
    /// root A must NOT be reported for root B — otherwise the fallback create
    /// would wrongly skip creating a review for B.
    #[wasm_bindgen_test]
    async fn existing_draft_lookup_is_root_scoped() {
        let container = make_container();
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            // A single Draft summary anchored to root A only.
            let summary = protocol::ReviewSummary {
                id: ReviewId("rev-a".to_owned()),
                root: ProjectRootPath("/repo-a".to_owned()),
                status: ReviewStatus::Draft,
                origin_session_id: SessionId("s".to_owned()),
                origin_agent_id: AgentId("project-review:rev-a".to_owned()),
                created_at_ms: 0,
                updated_at_ms: 1,
                user_comment_count: 0,
                pending_suggestion_count: 0,
            };
            state.review_summaries.update(|m| {
                m.insert(ProjectId("proj-1".to_owned()), vec![summary]);
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <div></div> }
        });
        std::mem::forget(handle);
        next_tick().await;

        let state = holder.borrow().clone().unwrap();
        let pid = ProjectId("proj-1".to_owned());
        assert_eq!(
            existing_draft_for_root(&state, &pid, &ProjectRootPath("/repo-a".to_owned())),
            Some(ReviewId("rev-a".to_owned())),
            "root A's own draft must be found"
        );
        assert_eq!(
            existing_draft_for_root(&state, &pid, &ProjectRootPath("/repo-b".to_owned())),
            None,
            "root A's draft must NOT satisfy a lookup for root B"
        );
    }

    /// Helper: locate a button whose visible text contains `needle`.
    /// Used so assertions live in user-perceived land (visible text)
    /// rather than internal class names.
    fn find_button_by_text(root: &HtmlElement, needle: &str) -> Option<HtmlElement> {
        let buttons = root.query_selector_all("button").ok()?;
        for i in 0..buttons.length() {
            let btn = buttons.item(i)?;
            let el: HtmlElement = btn.dyn_into().ok()?;
            if el.text_content().unwrap_or_default().contains(needle) {
                return Some(el);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn _silence_unused(_: &Element) {}
}
