//! Shared review controls and helpers for the always-on inline review flow.
//!
//! The standalone three-pane workbench has been retired: reviews are now an
//! always-on, workspace-scoped layer over the normal git diff surfaces
//! (`ReviewableDiffView` in `diff_view.rs`). There is one active review per
//! project spanning every root; each per-root diff tab renders its own slice.
//! What remains here is the shared action sidebar (`ReviewSidebar` — live
//! counts, AI-reviewer form, submit-target picker, Clear) that the single
//! git-panel workspace hub mounts, plus the subscribe/diff-open/feedback
//! helpers used across the integrated flow.
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
use crate::components::inline_review::ThreadRegionFiltered;
pub(crate) use crate::components::review_layer::ComposerState;
use crate::send::send_frame;

use crate::state::{
    ActiveAgentRef, AgentInfo, AppState, DiffKey, DiffViewState, ReviewActionGate,
    ReviewActionTarget, TabContent, root_display_name,
};

use protocol::{
    BackendKind, DiffContextMode, FrameKind, ProjectDiffScope, ProjectGitDiffLineKind, ProjectId,
    ProjectReadDiffPayload, ProjectRootPath, Review, ReviewActionPayload, ReviewAiReviewerStatus,
    ReviewAnchor, ReviewCommentSource, ReviewDiffSide, ReviewId, ReviewLocation, ReviewStatus,
    ReviewSuggestionState, ReviewSummary, ReviewSummaryScope, StreamPath,
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
///
/// `can_run_ai` is an optional extra gate on the AI reviewer: when supplied
/// and `false`, "Run AI reviewer" is disabled (e.g. the workspace has no
/// reviewable changes, so the reviewer would see an empty diff). Omitted
/// (`None`) ⇒ no extra gate, the historical behavior.
#[component]
pub(crate) fn ReviewSidebar(
    review: Review,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
    #[prop(optional)] can_run_ai: Option<Memo<bool>>,
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
    //
    // `backend_pick` is an *explicit override*. The AI reviewer otherwise
    // runs against the effective default backend so the user never has to
    // pick: explicit override → `host_settings.default_backend` → first
    // enabled backend. Only when the host has no enabled backends at all is
    // there nothing to run.
    let backend_pick: RwSignal<Option<BackendKind>> = RwSignal::new(None);
    let cost_pick: RwSignal<Option<protocol::SpawnCostHint>> = RwSignal::new(None);
    let instructions: RwSignal<String> = RwSignal::new(String::new());

    let eff_backend_state = state.clone();
    let eff_backend_host = host_id.clone();
    let effective_backend: Memo<Option<BackendKind>> = Memo::new(move |_| {
        backend_pick.get().or_else(|| {
            eff_backend_state.host_settings_by_host.with(|map| {
                map.get(&eff_backend_host).and_then(|s| {
                    s.default_backend
                        .or_else(|| s.enabled_backends.first().copied())
                })
            })
        })
    });

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
            // Extra gate (e.g. workspace has no reviewable changes): an empty
            // diff would spawn a reviewer with nothing to review.
            if !can_run_ai.map(|m| m.get()).unwrap_or(true) {
                return "No reviewable changes";
            }
            if effective_backend.get().is_none() {
                return "No AI backend available";
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
        // Mirror the disabled gate: never spawn a reviewer when the extra
        // gate forbids it (e.g. no reviewable changes — empty diff).
        if !can_run_ai.map(|m| m.get_untracked()).unwrap_or(true) {
            log::info!("review.start_ai.skipped review={rid} reason=no_reviewable_changes");
            return;
        }
        // Gate on there being *some* runnable backend (else nothing to do),
        // but resolve the default server-side: send the explicit picker
        // override when chosen, else `None` so the host applies its
        // `default_backend` (or first enabled).
        if effective_backend.get_untracked().is_none() {
            log::info!("review.start_ai.skipped review={rid} reason=no_backend");
            return;
        }
        let backend_override = backend_pick.get_untracked();
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
            "review.start_ai.click review={} claimed={} gate_before_start_ai={} backend_override={:?} cost={:?} instructions_len={} ai_status={} {}",
            rid,
            claimed,
            gate_before.start_ai,
            backend_override,
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
                backend_kind: backend_override,
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
    // Disclosure arrow matching the app's collapsible pattern (the git
    // panel's CHANGES sections use the same ▾/▸ `fe-chevron`).
    let ai_chevron = move || {
        if ai_open_attr() {
            "\u{25be}"
        } else {
            "\u{25b8}"
        }
    };
    let _ = status; // silence unused if banner moved out of sidebar

    view! {
        <div class="review-sidebar">
            <div class="review-sidebar-section">
                <div class="review-ai-row">
                    <button
                        class="review-btn primary review-run-ai-btn"
                        data-test="gp-workspace-review-all"
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
                    <div
                        class=move || format!("review-ai-status status-{}", ai_status_kind())
                        data-status=ai_status_kind
                        title="AI reviewer status"
                    >
                        <span class="review-ai-status-dot"></span>
                        <span class="review-ai-status-label">{ai_status_text}</span>
                        {move || elapsed_label().map(|e| view! {
                            <span class="review-ai-status-elapsed">{e}</span>
                        })}
                    </div>
                </div>
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
                        <span class="fe-chevron">{ai_chevron}</span>
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
                                {move || match effective_backend.get() {
                                    Some(kind) if backend_pick.get().is_none() => {
                                        format!("Default ({})", backend_kind_label(kind))
                                    }
                                    _ => "Use default backend".to_owned(),
                                }}
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
                <div class="review-action-row">
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
        </div>
    }
}

fn backend_kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
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

/// Whether `agent` is a valid review submit target: a top-level agent a human
/// owns, never a sub-agent. Sub-agents — backend-native subagents, agent-control
/// children, and workflow / team / side-question spawns — carry a
/// `parent_agent_id` and/or a non-`User` [`AgentOrigin`]. Decided purely from
/// server-emitted typed provenance, never from the agent's display name (so a
/// legitimately user-created agent named "Agent" is kept, and a sub-agent with
/// any name is dropped).
fn is_top_level_user_agent(agent: &AgentInfo) -> bool {
    agent.parent_agent_id.is_none() && agent.origin == protocol::AgentOrigin::User
}

/// Live same-project agents on `host_id` for `project_id` — the deliverable
/// review submit targets. "Live" excludes agents with a fatal error
/// (terminated); [`is_top_level_user_agent`] excludes sub-agents so the picker
/// and auto-target only ever consider top-level user agents. The review's own
/// origin is intentionally never consulted: project-scoped reviews use a
/// synthetic, non-deliverable origin. The `tracked` flag selects reactive vs
/// untracked signal reads so the same logic serves both the reactive submit
/// gate and the click handler.
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
                    && is_top_level_user_agent(a)
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
        "Antigravity" => Some(BackendKind::Antigravity),
        "Hermes" => Some(BackendKind::Hermes),
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

/// The single active workspace Draft summary for a project, if present.
/// The server emits exactly one active summary per project with
/// `ReviewSummaryScope::Workspace`, spanning every root. Legacy root-scoped
/// summaries are never active and are ignored. Submitted/Consumed/Cancelled
/// records are filtered out — only an actively editable Draft is a review
/// surface.
pub(crate) fn pick_workspace_draft(summaries: &[ReviewSummary]) -> Option<&ReviewSummary> {
    summaries
        .iter()
        .filter(|s| {
            matches!(s.scope, ReviewSummaryScope::Workspace)
                && matches!(s.status, ReviewStatus::Draft)
        })
        .max_by_key(|s| s.updated_at_ms)
}

/// The single active workspace Draft review id for `project_id`, if any.
/// One review spans all of the project's roots, so this is keyed by project
/// alone (no root).
fn workspace_draft_for_project(
    state: &AppState,
    project_id: &protocol::ProjectId,
) -> Option<protocol::ReviewId> {
    state.review_summaries.with_untracked(|map| {
        map.get(project_id)
            .and_then(|summaries| pick_workspace_draft(summaries).map(|s| s.id.clone()))
    })
}

/// Send a workspace-scoped `ReviewCreate` with create-pending gating, with
/// no tab navigation — callers decide which surface to show. There is at
/// most one active Draft per project: callers must check
/// [`workspace_draft_for_project`] first.
///
/// The selection is `Workspace { Unstaged }` to match the active-review
/// model: there is exactly one active review per project spanning all of its
/// roots, and the server normalizes every active review to
/// `Workspace { scope: Unstaged }`.
///
/// The create-pending gate is keyed by `(host, project)`: a project has one
/// active review, and the `CommandError` clear path in dispatch recovers
/// `(host, project)` from the `/project/{id}` stream. The gate is a transient
/// debounce, not lifecycle state.
fn send_review_create(state: &AppState, host_id: &str, project_id: &protocol::ProjectId) {
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
        selection: protocol::ReviewDiffSelection::Workspace {
            scope: ProjectDiffScope::Unstaged,
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

/// Open (or focus) the compact review-comments surface for the project's
/// single workspace draft review: snippets around each human comment,
/// accepted AI comment, and pending AI suggestion — not the full diff —
/// grouped by root. The full diff stays one click away via the surface's
/// per-root "Open full diff" buttons.
pub fn open_comments_for_project(
    state: &AppState,
    host_id: &str,
    project_id: &protocol::ProjectId,
) {
    state.open_tab(
        TabContent::Comments {
            host_id: host_id.to_owned(),
            project_id: project_id.clone(),
        },
        "Review comments".to_owned(),
        true,
    );
}

/// One reviewable entry in the comments surface: a distinct anchored
/// location that has at least one comment or pending suggestion, with a
/// human-readable label. The diff snippet is computed *reactively in the row*
/// from `state.diff_contents`, not captured here — the path-scoped diff fetch
/// is async, so a captured snippet would render stale (empty) and never update
/// when the response lands under the same `<For>` key.
#[derive(Clone, Debug, PartialEq)]
struct CommentSurfaceEntry {
    location: ReviewLocation,
    /// Stable `<For>` key. Includes the review id so a different draft for the
    /// same path/anchor forces a fresh row instead of reusing one bound to the
    /// old review; the snippet itself updates reactively without a key change.
    key: String,
    label: String,
}

#[derive(Clone, Debug, PartialEq)]
struct SnippetLine {
    marker: char,
    /// New-side line number when present, else old-side — for display only.
    number: Option<u32>,
    text: String,
}

const SNIPPET_CONTEXT: u32 = 2;
const SNIPPET_MAX_LINES: usize = 14;

fn anchor_label(relative_path: &str, anchor: &ReviewAnchor) -> String {
    match anchor {
        ReviewAnchor::File => format!("{relative_path} \u{00b7} file"),
        ReviewAnchor::Hunk { .. } => format!("{relative_path} \u{00b7} hunk"),
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => {
            let side = match side {
                ReviewDiffSide::Old => "old",
                ReviewDiffSide::New => "new",
            };
            if start_line == end_line {
                format!("{relative_path} \u{00b7} {side} L{start_line}")
            } else {
                format!("{relative_path} \u{00b7} {side} L{start_line}\u{2013}{end_line}")
            }
        }
    }
}

/// Pull the small snippet of diff lines around `anchor` from already-loaded
/// project diff files (`state.diff_contents`), NOT from `review.diffs` — the
/// comments surface subscribes lightweight, so the review record carries no
/// diffs. Empty when the file isn't loaded yet, the anchor is file-level, the
/// file is binary, or the anchored range no longer maps to any diff line.
fn snippet_for_anchor(
    files: &[protocol::ProjectGitDiffFile],
    relative_path: &str,
    anchor: &ReviewAnchor,
) -> Vec<SnippetLine> {
    let Some(file) = files.iter().find(|f| f.relative_path == relative_path) else {
        return Vec::new();
    };

    let to_snippet = |line: &protocol::ProjectGitDiffLine| SnippetLine {
        marker: match line.kind {
            ProjectGitDiffLineKind::Added => '+',
            ProjectGitDiffLineKind::Removed => '-',
            ProjectGitDiffLineKind::Context => ' ',
        },
        number: line.new_line_number.or(line.old_line_number),
        text: line.text.clone(),
    };

    match anchor {
        ReviewAnchor::File => Vec::new(),
        // Bound the allocation: take at most `SNIPPET_MAX_LINES` before
        // collecting rather than collecting a whole hunk then truncating.
        ReviewAnchor::Hunk { hunk_id, .. } => file
            .hunks
            .iter()
            .find(|h| &h.hunk_id == hunk_id)
            .map(|h| {
                h.lines
                    .iter()
                    .take(SNIPPET_MAX_LINES)
                    .map(to_snippet)
                    .collect()
            })
            .unwrap_or_default(),
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => {
            let lo = start_line.saturating_sub(SNIPPET_CONTEXT);
            let hi = end_line.saturating_add(SNIPPET_CONTEXT);
            let selected_number = |line: &protocol::ProjectGitDiffLine| match side {
                ReviewDiffSide::Old => line.old_line_number,
                ReviewDiffSide::New => line.new_line_number,
            };
            let in_window = |line: &protocol::ProjectGitDiffLine| matches!(selected_number(line), Some(n) if n >= lo && n <= hi);
            // A change line that carries no selected-side number — i.e. the
            // opposite side of a replacement (a `-` line when anchoring on New,
            // a `+` line when anchoring on Old). These have no selected-side
            // number to fall in the window, so they must be pulled in by
            // adjacency rather than by number.
            let opposite_change = |line: &protocol::ProjectGitDiffLine| {
                selected_number(line).is_none()
                    && !matches!(line.kind, ProjectGitDiffLineKind::Context)
            };
            // Include the positional span between the first and last in-window
            // line in each hunk, then extend it over directly-adjacent
            // opposite-side change lines so a top-of-hunk replacement like
            // `-old` / `+new` anchored on New L1 keeps its `-old` line even
            // with no preceding context. Bounded to `SNIPPET_MAX_LINES`.
            let mut out: Vec<SnippetLine> = Vec::new();
            for hunk in &file.hunks {
                let Some(mut first) = hunk.lines.iter().position(&in_window) else {
                    continue;
                };
                let mut last = hunk.lines.iter().rposition(&in_window).unwrap_or(first);
                while first > 0 && opposite_change(&hunk.lines[first - 1]) {
                    first -= 1;
                }
                while last + 1 < hunk.lines.len() && opposite_change(&hunk.lines[last + 1]) {
                    last += 1;
                }
                for line in &hunk.lines[first..=last] {
                    out.push(to_snippet(line));
                    if out.len() >= SNIPPET_MAX_LINES {
                        return out;
                    }
                }
            }
            out
        }
    }
}

/// Numeric sort key for a comment-surface entry: group by file, then by
/// anchor side, then by line range. Avoids the lexicographic ordering of the
/// Debug-string entry key (which would sort line 10 before line 2).
fn anchor_sort_key(location: &ReviewLocation) -> (String, u8, u32, u32) {
    let (rank, start, end) = match &location.anchor {
        ReviewAnchor::File => (0u8, 0u32, 0u32),
        ReviewAnchor::Hunk {
            new_start,
            old_start,
            ..
        } => (1, *new_start, *old_start),
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => {
            let rank = match side {
                ReviewDiffSide::Old => 2,
                ReviewDiffSide::New => 3,
            };
            (rank, *start_line, *end_line)
        }
    };
    (location.relative_path.clone(), rank, start, end)
}

/// Resolve the project diff files that cover `relative_path` from the project
/// diff cache: prefer a per-file entry, else fall back to the whole-root entry
/// (which the full diff view shares). Empty when neither is loaded yet.
fn resolve_diff_files(
    diffs: &std::collections::HashMap<DiffKey, DiffViewState>,
    host_id: &str,
    project_id: &ProjectId,
    root: &ProjectRootPath,
    relative_path: &str,
) -> Vec<protocol::ProjectGitDiffFile> {
    let per_file = DiffKey::new(
        host_id,
        project_id.clone(),
        root.clone(),
        ProjectDiffScope::Unstaged,
        relative_path,
    );
    if let Some(entry) = diffs.get(&per_file) {
        return entry.files.clone();
    }
    let whole_root = DiffKey::new(
        host_id,
        project_id.clone(),
        root.clone(),
        ProjectDiffScope::Unstaged,
        "",
    );
    diffs
        .get(&whole_root)
        .map(|entry| entry.files.clone())
        .unwrap_or_default()
}

/// Whether the unstaged diff for `relative_path` is already represented in the
/// project diff cache — either a per-file entry or the shared whole-root entry.
/// Used to avoid re-fetching a file the diff view already loaded.
fn file_diff_cached(
    diffs: &std::collections::HashMap<DiffKey, DiffViewState>,
    host_id: &str,
    project_id: &ProjectId,
    root: &ProjectRootPath,
    relative_path: &str,
) -> bool {
    let per_file = DiffKey::new(
        host_id,
        project_id.clone(),
        root.clone(),
        ProjectDiffScope::Unstaged,
        relative_path,
    );
    let whole_root = DiffKey::new(
        host_id,
        project_id.clone(),
        root.clone(),
        ProjectDiffScope::Unstaged,
        "",
    );
    diffs.contains_key(&per_file) || diffs.contains_key(&whole_root)
}

/// Compact, comments-first surface for the project's single workspace draft
/// review, grouped by root.
///
/// Shows only the regions that carry feedback — each human comment, accepted
/// AI comment, and pending AI suggestion — as a small snippet plus its
/// thread, instead of the whole diff. One review spans every root, so entries
/// are grouped under per-root headers; each group has its own "Open full diff"
/// escape hatch. Rejected suggestions are excluded from the entry list (they
/// stay reachable under each thread's existing "N rejected" toggle). An empty
/// state covers the no-feedback case.
#[component]
pub fn ReviewCommentsSurface(host_id: String, project_id: ProjectId) -> impl IntoView {
    let state = expect_context::<AppState>();

    // Resolve the project's single workspace draft review id. Start from the
    // Draft summary (cheap, arrives first), but if we hold the full record
    // require *its* live status to still be Draft: a live `StatusChanged`
    // updates `state.reviews` before `review_summaries` refreshes, so trusting
    // a stale Draft summary alone would keep the comments surface bound to an
    // already-submitted review. Mirrors `ReviewableDiffView`'s guard.
    let draft_state = state.clone();
    let draft_host = host_id.clone();
    let draft_project = project_id.clone();
    let draft: Memo<Option<(String, ReviewId)>> = Memo::new(move |_| {
        let id = draft_state.review_summaries.with(|m| {
            m.get(&draft_project)
                .and_then(|sums| pick_workspace_draft(sums).map(|s| s.id.clone()))
        })?;
        let live_non_draft = draft_state.reviews.with(|r| {
            r.get(&id)
                .map(|rev| !matches!(rev.status, ReviewStatus::Draft))
                .unwrap_or(false)
        });
        if live_non_draft {
            return None;
        }
        Some((draft_host.clone(), id))
    });

    // Keep the draft subscribed so comments/suggestions/diffs are available.
    subscribe_review_reactive(&state, draft);

    let is_draft: Memo<bool> = Memo::new(move |_| draft.get().is_some());

    // A composer signal is required by `ThreadRegionFiltered`, but this
    // surface has no drag-to-comment gutter, so it never opens.
    let composer: RwSignal<Option<ComposerState>> = RwSignal::new(None);

    // Distinct (root, file) pairs that carry feedback (comments or pending
    // suggestions) across all roots. Drives the per-file diff fetch below.
    let files_state = state.clone();
    let commented_files: Memo<Vec<(ProjectRootPath, String)>> = Memo::new(move |_| {
        let Some((_, rid)) = draft.get() else {
            return Vec::new();
        };
        files_state.reviews.with(|map| {
            let Some(review) = map.get(&rid) else {
                return Vec::new();
            };
            let mut seen: std::collections::HashSet<(ProjectRootPath, String)> =
                std::collections::HashSet::new();
            let mut files = Vec::new();
            let comment_files = review.comments.iter().map(|c| &c.location);
            let suggestion_files = review
                .suggestions
                .iter()
                .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                .map(|s| &s.location);
            for loc in comment_files.chain(suggestion_files) {
                let pair = (loc.root.clone(), loc.relative_path.clone());
                if seen.insert(pair.clone()) {
                    files.push(pair);
                }
            }
            files
        })
    });

    // Fetch the unstaged diff for each commented (root, file) not already in
    // the project diff cache (per-file or shared whole-root). Lightweight:
    // only commented files, Hunks context. Snippets render reactively from
    // `state.diff_contents` once these responses land. `requested` dedupes so
    // a file is fetched at most once per surface mount.
    {
        let fetch_state = state.clone();
        let fetch_host = host_id.clone();
        let fetch_pid = project_id.clone();
        let requested: StoredValue<
            std::collections::HashSet<(ProjectRootPath, String)>,
            LocalStorage,
        > = StoredValue::new_local(std::collections::HashSet::new());
        Effect::new(move |_| {
            for (root, path) in commented_files.get() {
                let cached = fetch_state.diff_contents.with_untracked(|diffs| {
                    file_diff_cached(diffs, &fetch_host, &fetch_pid, &root, &path)
                });
                let pair = (root.clone(), path.clone());
                if cached || requested.with_value(|set| set.contains(&pair)) {
                    continue;
                }
                requested.update_value(|set| {
                    set.insert(pair.clone());
                });
                let key = DiffKey::new(
                    fetch_host.clone(),
                    fetch_pid.clone(),
                    root.clone(),
                    ProjectDiffScope::Unstaged,
                    path.clone(),
                );
                fetch_state.diff_contents.update(|diffs| {
                    let previous = diffs.get(&key);
                    let next = DiffViewState::for_request(
                        previous,
                        root.clone(),
                        ProjectDiffScope::Unstaged,
                        Some(path.clone()),
                        DiffContextMode::Hunks,
                    );
                    diffs.insert(key, next);
                });
                let stream = StreamPath(format!("/project/{}", fetch_pid.0));
                let host = fetch_host.clone();
                let payload = ProjectReadDiffPayload {
                    root: root.clone(),
                    scope: ProjectDiffScope::Unstaged,
                    path: Some(path.clone()),
                    context_mode: DiffContextMode::Hunks,
                };
                spawn_local(async move {
                    if let Err(e) =
                        send_frame(&host, stream, FrameKind::ProjectReadDiff, &payload).await
                    {
                        log::error!("review.comments.read_diff.err path={path} error={e}");
                    }
                });
            }
        });
    }

    // Distinct anchored locations carrying feedback, grouped by root. Depends
    // only on `state.reviews` + the draft id: the entry *set* changes when
    // comments or pending suggestions are added/removed, not when diffs
    // arrive. Each row then computes its snippet reactively from
    // `state.diff_contents`, so the async path-scoped diff fetch fills the
    // snippet in without changing the entry set or the `<For>` key. Comments
    // (human + accepted-AI) and pending suggestions define the set; rejected
    // suggestions are excluded.
    let entries_state = state.clone();
    let groups: Memo<Vec<(ProjectRootPath, Vec<CommentSurfaceEntry>)>> = Memo::new(move |_| {
        let Some((_, rid)) = draft.get() else {
            return Vec::new();
        };
        let mut entries: Vec<CommentSurfaceEntry> = entries_state.reviews.with(|map| {
            let Some(review) = map.get(&rid) else {
                return Vec::new();
            };
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut entries = Vec::new();
            let comment_locs = review.comments.iter().map(|c| &c.location);
            let suggestion_locs = review
                .suggestions
                .iter()
                .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                .map(|s| &s.location);
            for loc in comment_locs.chain(suggestion_locs) {
                // Key includes the review id and root so a different draft, or
                // the same path/anchor in another root, forces a fresh row
                // rather than reusing one bound to the old review/root.
                let key = format!(
                    "{}|{}|{}|{:?}",
                    rid.0, loc.root.0, loc.relative_path, loc.anchor
                );
                if seen.insert(key.clone()) {
                    entries.push(CommentSurfaceEntry {
                        label: anchor_label(&loc.relative_path, &loc.anchor),
                        key,
                        location: loc.clone(),
                    });
                }
            }
            entries
        });
        // Stable order: by root, then by file/anchor within the root.
        entries.sort_by(|a, b| {
            a.location
                .root
                .0
                .cmp(&b.location.root.0)
                .then_with(|| anchor_sort_key(&a.location).cmp(&anchor_sort_key(&b.location)))
        });
        let mut groups: Vec<(ProjectRootPath, Vec<CommentSurfaceEntry>)> = Vec::new();
        for entry in entries {
            let root = entry.location.root.clone();
            match groups.last_mut() {
                Some((r, items)) if *r == root => items.push(entry),
                _ => groups.push((root, vec![entry])),
            }
        }
        groups
    });

    let has_entries = Memo::new(move |_| !groups.get().is_empty());

    // Toolbar escape hatch: always available, opens the project's first
    // reviewable root's full diff (per-root groups offer their own opener).
    let toolbar_state = state.clone();
    let toolbar_host = host_id.clone();
    let toolbar_pid = project_id.clone();
    let open_full_diff = move |_| {
        open_changed_diff_for_project(&toolbar_state, &toolbar_host, &toolbar_pid);
    };

    view! {
        <div class="review-comments-surface" data-test="review-comments-surface">
            <div class="review-comments-toolbar">
                <span class="review-comments-title">"Review comments"</span>
                <button
                    class="review-btn review-comments-open-full"
                    data-test="review-comments-open-full"
                    title="Open the full diff for the first changed root"
                    on:click=open_full_diff
                >
                    "Open full diff"
                </button>
            </div>
            <Show
                when=move || has_entries.get()
                fallback=move || view! {
                    <div class="review-comments-empty" data-test="review-comments-empty">
                        "No review comments yet. Comments, accepted AI suggestions, and "
                        "pending suggestions show up here as you review."
                    </div>
                }
            >
                <div class="review-comments-list">
                    <For
                        each=move || groups.get()
                        key=|(root, entries)| {
                            // Re-key the group when its root or its entry set
                            // changes so added/removed rows re-render.
                            let ids: String = entries.iter().map(|e| e.key.as_str()).collect::<Vec<_>>().join(",");
                            format!("{}::{ids}", root.0)
                        }
                        children={
                            let host_id = host_id.clone();
                            let project_id = project_id.clone();
                            let state = state.clone();
                            move |(root, entries): (ProjectRootPath, Vec<CommentSurfaceEntry>)| {
                                let open_state = state.clone();
                                let open_host = host_id.clone();
                                let open_pid = project_id.clone();
                                let open_root = root.clone();
                                let open_full_diff = move |_| {
                                    open_changed_diff_for_root(
                                        &open_state,
                                        &open_host,
                                        &open_pid,
                                        &open_root,
                                    );
                                };
                                let row_host = host_id.clone();
                                let row_pid = project_id.clone();
                                let row_state = state.clone();
                                let rows = entries.into_iter().map(|entry| {
                                    review_comment_entry_row(
                                        &row_state,
                                        &row_host,
                                        &row_pid,
                                        draft,
                                        composer,
                                        is_draft,
                                        entry,
                                    )
                                }).collect::<Vec<_>>();
                                view! {
                                    <div
                                        class="review-comments-root-group"
                                        data-test="review-comments-root-group"
                                        data-root=root.0.clone()
                                    >
                                        <div class="review-comments-root-header">
                                            <span class="review-comments-root-name">
                                                {root_display_name(&root)}
                                            </span>
                                            <button
                                                class="review-btn review-comments-open-root"
                                                data-test="review-comments-open-root"
                                                title="Open this root's full diff"
                                                on:click=open_full_diff
                                            >
                                                "Open diff"
                                            </button>
                                        </div>
                                        {rows}
                                    </div>
                                }.into_any()
                            }
                        }
                    />
                </div>
            </Show>
        </div>
    }
}

/// Render one comment-surface entry row: its label, a reactive diff snippet
/// pulled from the project diff cache, and the anchored comment thread. The
/// snippet is computed reactively (not captured) so the async path-scoped
/// diff fetch fills it in without remounting the row.
fn review_comment_entry_row(
    state: &AppState,
    host_id: &str,
    project_id: &ProjectId,
    draft: Memo<Option<(String, ReviewId)>>,
    composer: RwSignal<Option<ComposerState>>,
    is_draft: Memo<bool>,
    entry: CommentSurfaceEntry,
) -> impl IntoView {
    let Some((_, rid)) = draft.get_untracked() else {
        return view! { <div></div> }.into_any();
    };
    let anchor = entry.location.anchor.clone();
    let matcher: std::sync::Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync> =
        std::sync::Arc::new(move |a: &ReviewAnchor| *a == anchor);

    let snip_state = state.clone();
    let snip_pid = project_id.clone();
    let snip_host = host_id.to_owned();
    let snip_root = entry.location.root.clone();
    let snip_path = entry.location.relative_path.clone();
    let snip_anchor = entry.location.anchor.clone();
    let snippet = move || {
        snip_state.diff_contents.with(|diffs| {
            let files = resolve_diff_files(diffs, &snip_host, &snip_pid, &snip_root, &snip_path);
            snippet_for_anchor(&files, &snip_path, &snip_anchor)
        })
    };

    let region_root = entry.location.root.clone();
    let region_host = host_id.to_owned();
    view! {
        <div class="review-comments-entry" data-test="review-comments-entry">
            <div class="review-comments-entry-label">{entry.label.clone()}</div>
            {move || {
                let lines = snippet();
                (!lines.is_empty()).then(|| view! {
                    <pre class="review-comments-snippet">
                        {lines.into_iter().map(|line| {
                            let cls = match line.marker {
                                '+' => "review-snippet-line added",
                                '-' => "review-snippet-line removed",
                                _ => "review-snippet-line context",
                            };
                            let num = line.number.map(|n| n.to_string()).unwrap_or_default();
                            view! {
                                <div class=cls>
                                    <span class="review-snippet-num">{num}</span>
                                    <span class="review-snippet-marker">
                                        {line.marker.to_string()}
                                    </span>
                                    <span class="review-snippet-text">{line.text}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>()}
                    </pre>
                })
            }}
            <ThreadRegionFiltered
                review_id=rid
                root=region_root
                relative_path=entry.location.relative_path.clone()
                host_id=region_host
                composer=composer
                matcher=matcher
                is_draft=is_draft
            />
        </div>
    }
    .into_any()
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
                // The frontend never renders from `review.diffs` — diffs come
                // from the project diff pipeline (`state.diff_contents`), and
                // inline comments anchor off per-comment line numbers, not the
                // review's diff copy. Subscribe lightweight so bootstrap and
                // later Snapshot/Cleared payloads omit the redundant diffs.
                &protocol::ReviewSubscribePayload {
                    include_diffs: false,
                },
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
/// Reviews are always-on and workspace-scoped server-side, so this is
/// primarily navigation: it opens/focuses the project's normal changed-file
/// diff tab (the canonical inline review surface). The active workspace
/// review's decorations resolve on that diff tab from the bootstrap/summary
/// stream. If no Draft summary has arrived yet, it also fires a legacy
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

    // Create only when the project has reviewable changes and no active
    // workspace Draft yet. One review spans all roots, so the check and the
    // create are both project-scoped.
    if review_root_for_project(state, &project_id).is_some()
        && workspace_draft_for_project(state, &project_id).is_none()
    {
        send_review_create(state, &host_id, &project_id);
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
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath("s".to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    /// A same-project sub-agent: carries a typed `parent_agent_id` and a
    /// non-`User` `origin`, exactly as the server marks orchestrated children.
    fn make_sub_agent(name: &str, agent_id: &str, origin: AgentOrigin, parent: &str) -> AgentInfo {
        AgentInfo {
            parent_agent_id: Some(AgentId(parent.to_owned())),
            origin,
            ..make_agent(name, agent_id, Some("proj-1"))
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
    /// git-panel workspace hub wires it. The mount handle is leaked so the
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

    /// Sub-agents (backend-native subagents, agent-control children, workflow
    /// spawns) must never appear in the reviewer picker — they are excluded by
    /// their typed `parent_agent_id`/`origin`, not by their name. Asserted by
    /// option identity so a name change can't mask a regression.
    #[wasm_bindgen_test]
    async fn submit_target_picker_hides_sub_agents() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review(); // project_id = "proj-1", host = "h1"
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let state = state_holder.borrow().clone().unwrap();
        state.agents.update(|agents| {
            // One legitimate top-level user agent — must be offered.
            agents.push(make_agent("Top Level", "a-top", Some("proj-1")));
            // Assorted sub-agents matching the screenshot — must be hidden.
            agents.push(make_sub_agent(
                "Agent",
                "a-native",
                AgentOrigin::BackendNative,
                "a-top",
            ));
            agents.push(make_sub_agent(
                "Agent2 Stats Design Plan",
                "a-control",
                AgentOrigin::AgentControl,
                "a-top",
            ));
            agents.push(make_sub_agent(
                "Agent2 Stats Wiring Implementer",
                "a-workflow",
                AgentOrigin::Workflow,
                "a-top",
            ));
        });
        next_tick().await;

        let select = container
            .query_selector("[data-test=\"review-submit-target\"]")
            .unwrap()
            .expect("submit target picker rendered");

        assert!(
            select
                .query_selector("option[value=\"existing:a-top\"]")
                .unwrap()
                .is_some(),
            "the top-level user agent must remain a reviewer target"
        );
        for hidden in ["a-native", "a-control", "a-workflow"] {
            assert!(
                select
                    .query_selector(&format!("option[value=\"existing:{hidden}\"]"))
                    .unwrap()
                    .is_none(),
                "sub-agent {hidden} must not appear in the reviewer picker"
            );
        }
    }

    /// The filter is driven by typed provenance, never the display name: a
    /// top-level user agent literally named "Agent" is kept, while a sub-agent
    /// with a friendly name is still dropped.
    #[wasm_bindgen_test]
    async fn submit_target_picker_keeps_user_agent_named_agent() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let state_holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        let state = state_holder.borrow().clone().unwrap();
        state.agents.update(|agents| {
            agents.push(make_agent("Agent", "a-user-named-agent", Some("proj-1")));
            agents.push(make_sub_agent(
                "Helpful Reviewer",
                "a-child",
                AgentOrigin::AgentControl,
                "a-user-named-agent",
            ));
        });
        next_tick().await;

        let select = container
            .query_selector("[data-test=\"review-submit-target\"]")
            .unwrap()
            .expect("submit target picker rendered");
        assert!(
            select
                .query_selector("option[value=\"existing:a-user-named-agent\"]")
                .unwrap()
                .is_some(),
            "a top-level user agent named \"Agent\" must be kept — no name inference"
        );
        assert!(
            select
                .query_selector("option[value=\"existing:a-child\"]")
                .unwrap()
                .is_none(),
            "a sub-agent must be dropped even with a friendly display name"
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

    /// Live review counts derive reactively from the comments/suggestions
    /// signal. UPDATED (panel density redesign, flagged): the sidebar's
    /// two-line COUNTS block was removed; the surviving compact indicator is
    /// the workspace hub's top-right `gp-workspace-review-counts`
    /// ("{c} comments · {s} AI"). This mounts the real hub (`GitPanel`) and
    /// asserts that single indicator reflects live counts — same reactive
    /// guarantee, repointed at the compact element.
    #[wasm_bindgen_test]
    async fn sidebar_counts_derive_reactively() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let review_id = review.id.clone();

        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let review_for_mount = review.clone();
        let summary_id = review_id.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state
                .active_project
                .set(Some(crate::state::ActiveProjectRef {
                    host_id: "h1".to_owned(),
                    project_id: ProjectId("proj-1".to_owned()),
                }));
            state.git_status.update(|m| {
                m.insert(
                    ProjectId("proj-1".to_owned()),
                    vec![protocol::ProjectRootGitStatus {
                        root: root_path(),
                        branch: Some("main".to_owned()),
                        ahead: 0,
                        behind: 0,
                        clean: false,
                        files: vec![protocol::ProjectGitFileStatus {
                            relative_path: "src/foo.rs".to_owned(),
                            staged: None,
                            unstaged: Some(protocol::ProjectGitChangeKind::Modified),
                            untracked: false,
                        }],
                    }],
                );
            });
            state.review_summaries.update(|m| {
                m.insert(
                    ProjectId("proj-1".to_owned()),
                    vec![protocol::ReviewSummary {
                        id: summary_id.clone(),
                        scope: protocol::ReviewSummaryScope::Workspace,
                        status: ReviewStatus::Draft,
                        origin_session_id: SessionId("s".to_owned()),
                        origin_agent_id: AgentId("a".to_owned()),
                        created_at_ms: 0,
                        updated_at_ms: 1,
                        user_comment_count: 0,
                        pending_suggestion_count: 0,
                        file_comment_counts: vec![],
                    }],
                );
            });
            // Seed the full record so the hub doesn't fire a network subscribe.
            state.reviews.update(|m| {
                m.insert(review_for_mount.id.clone(), review_for_mount.clone());
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <crate::components::git_panel::GitPanel /> }
        });
        std::mem::forget(handle);

        next_tick().await;
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"gp-workspace-review-counts\"]")
            .unwrap()
            .expect("workspace counts indicator mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        assert!(
            counts_el
                .text_content()
                .unwrap_or_default()
                .contains("0 comment"),
            "expected initial '0 comment', got: {}",
            counts_el.text_content().unwrap_or_default()
        );

        let state = holder.borrow().clone().unwrap();
        state.reviews.update(|map| {
            if let Some(r) = map.get_mut(&review_id) {
                r.comments.push(comment_at_line(2, "first"));
                r.comments.push(comment_at_line(3, "second"));
                r.suggestions.push(pending_suggestion_at_line(2, "ai"));
            }
        });
        next_tick().await;

        let counts = container
            .query_selector("[data-test=\"gp-workspace-review-counts\"]")
            .unwrap()
            .expect("workspace counts indicator mounted");
        let counts_el: HtmlElement = counts.dyn_into().unwrap();
        let txt = counts_el.text_content().unwrap_or_default();
        assert!(
            txt.contains("2 comments"),
            "expected '2 comments' in: {txt}"
        );
        assert!(txt.contains("1 AI"), "expected '1 AI' in: {txt}");
    }

    /// AI review uses the host's default backend automatically — the user no
    /// longer has to choose one first. Run AI is disabled only when the host
    /// has *no* enabled backend (nothing to run); once a default backend
    /// exists it enables without any explicit picker choice. We assert via
    /// the `disabled` attribute, which is user-perceived.
    #[wasm_bindgen_test]
    async fn run_ai_uses_default_backend_without_explicit_pick() {
        ensure_styles_loaded();
        let container = make_container();
        let review = make_review();
        let holder = mount_sidebar(container.clone(), review);

        next_tick().await;
        next_tick().await;

        // No enabled backends yet ⇒ nothing to run, button disabled.
        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        assert!(
            run_btn.has_attribute("disabled"),
            "Run AI must be disabled when the host has no enabled backend"
        );

        // Seed host settings with a default backend. No explicit picker choice.
        let state = holder.borrow().clone().unwrap();
        state.host_settings_by_host.update(|m| {
            m.insert(
                "h1".to_owned(),
                protocol::HostSettings {
                    enabled_backends: vec![BackendKind::Codex],
                    default_backend: Some(BackendKind::Codex),
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: false,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
        next_tick().await;

        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        assert!(
            !run_btn.has_attribute("disabled"),
            "Run AI must enable using the host default backend without an explicit pick"
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

    /// Workspace model (replaces the old per-root `existing_draft_for_root`
    /// test): `pick_workspace_draft` returns the single active Workspace draft
    /// spanning all roots, and ignores legacy `Root`-scoped summaries (which
    /// the server never emits as active).
    #[wasm_bindgen_test]
    fn pick_workspace_draft_ignores_legacy_root_summaries() {
        // A workspace draft is the active summary — found regardless of root.
        let workspace = protocol::ReviewSummary {
            id: ReviewId("rev-ws".to_owned()),
            scope: protocol::ReviewSummaryScope::Workspace,
            status: ReviewStatus::Draft,
            origin_session_id: SessionId("s".to_owned()),
            origin_agent_id: AgentId("project-review:rev-ws".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 1,
            user_comment_count: 0,
            pending_suggestion_count: 0,
            file_comment_counts: vec![],
        };
        assert_eq!(
            pick_workspace_draft(std::slice::from_ref(&workspace)).map(|s| s.id.clone()),
            Some(ReviewId("rev-ws".to_owned())),
            "the active workspace draft must be found"
        );

        // A legacy root-scoped summary is never an active workspace draft.
        let legacy_root = protocol::ReviewSummary {
            id: ReviewId("rev-root".to_owned()),
            scope: protocol::ReviewSummaryScope::Root {
                root: ProjectRootPath("/repo-a".to_owned()),
            },
            ..workspace.clone()
        };
        assert_eq!(
            pick_workspace_draft(std::slice::from_ref(&legacy_root)),
            None,
            "a legacy root-scoped summary must not be treated as the active draft"
        );
    }

    /// Mounts the `ReviewCommentsSurface` for `review`, seeding both the full
    /// review record and a matching Draft `ReviewSummary` so the surface
    /// resolves its draft id. Returns the captured `AppState`.
    fn mount_comments_surface(
        container: HtmlElement,
        review: Review,
        seed_diff: bool,
    ) -> std::rc::Rc<std::cell::RefCell<Option<AppState>>> {
        let holder: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_for_mount = holder.clone();
        let handle = mount_to(container, move || {
            let state = AppState::new();
            let rid = review.id.clone();
            let pid = review.project_id.clone();
            let root = root_path();
            state.reviews.update(|m| {
                m.insert(rid.clone(), review.clone());
            });
            // Snippets source from the project diff cache, not review.diffs.
            // Seed a whole-root unstaged entry the surface can read from.
            if seed_diff {
                let key = DiffKey::new(
                    "h1",
                    pid.clone(),
                    root.clone(),
                    ProjectDiffScope::Unstaged,
                    "",
                );
                state.diff_contents.update(|m| {
                    m.insert(
                        key,
                        DiffViewState {
                            root: root.clone(),
                            scope: ProjectDiffScope::Unstaged,
                            path: None,
                            context_mode: DiffContextMode::Hunks,
                            pending: false,
                            files: diff_payload().files,
                        },
                    );
                });
            }
            state.review_summaries.update(|m| {
                m.insert(
                    pid.clone(),
                    vec![protocol::ReviewSummary {
                        id: rid.clone(),
                        scope: protocol::ReviewSummaryScope::Workspace,
                        status: ReviewStatus::Draft,
                        origin_session_id: SessionId("s".to_owned()),
                        origin_agent_id: AgentId("a".to_owned()),
                        created_at_ms: 0,
                        updated_at_ms: 1,
                        user_comment_count: 0,
                        pending_suggestion_count: 0,
                        file_comment_counts: vec![],
                    }],
                );
            });
            *holder_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state.clone());
            view! {
                <ReviewCommentsSurface
                    host_id="h1".to_owned()
                    project_id=pid.clone()
                />
            }
        });
        std::mem::forget(handle);
        holder
    }

    fn rejected_suggestion_at_line(line: u32, body: &str) -> ReviewSuggestedComment {
        ReviewSuggestedComment {
            state: ReviewSuggestionState::Rejected,
            ..pending_suggestion_at_line(line, body)
        }
    }

    /// Comments surface with no feedback shows the empty state and zero
    /// entries, while still offering the "Open full diff" escape hatch.
    #[wasm_bindgen_test]
    async fn comments_surface_shows_empty_state() {
        ensure_styles_loaded();
        let container = make_container();
        let _ = mount_comments_surface(container.clone(), make_review(), true);

        next_tick().await;
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"review-comments-empty\"]")
                .unwrap()
                .is_some(),
            "empty state must render when there is no feedback"
        );
        let entries = container
            .query_selector_all("[data-test=\"review-comments-entry\"]")
            .unwrap();
        assert_eq!(entries.length(), 0, "no entries when there is no feedback");
        assert!(
            find_button_by_text(&container, "Open full diff").is_some(),
            "the full-diff escape hatch must always be available"
        );
    }

    /// The surface renders exactly one bounded entry per distinct anchored
    /// location that carries a comment or a *pending* suggestion. Rejected
    /// suggestions do not create their own entry, and the entry shows a small
    /// diff snippet pulled from the project diff cache — not from review.diffs,
    /// and not the whole file.
    #[wasm_bindgen_test]
    async fn comments_surface_one_entry_per_location_excludes_rejected() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        review.comments.push(comment_at_line(2, "a"));
        review.comments.push(comment_at_line(3, "b"));
        review
            .suggestions
            .push(pending_suggestion_at_line(4, "pending"));
        // A rejected suggestion at a location with no other feedback must not
        // produce an entry.
        review
            .suggestions
            .push(rejected_suggestion_at_line(5, "rejected"));
        let _ = mount_comments_surface(container.clone(), review, true);

        next_tick().await;
        next_tick().await;

        let entries = container
            .query_selector_all("[data-test=\"review-comments-entry\"]")
            .unwrap();
        assert_eq!(
            entries.length(),
            3,
            "one entry per distinct location for the two comments and the \
             pending suggestion; the rejected-only location is excluded"
        );

        // The snippet around the L2 comment is present and bounded to the
        // anchored region (not the unrelated src/bar.rs file).
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let x = 1;"),
            "snippet around the comment anchor must render; got: {text}"
        );
        assert!(
            !text.contains("bar();"),
            "an unrelated file's diff must not appear in the comments surface; got: {text}"
        );
    }

    /// NEW: the workspace comments surface groups entries by root — a review
    /// with comments in two roots renders one group per root, each tagged with
    /// its `data-root`, and the entries land under their own root's group.
    #[wasm_bindgen_test]
    async fn comments_surface_groups_by_root() {
        ensure_styles_loaded();
        let container = make_container();
        let mut review = make_review();
        // One comment in the default root (/repo)...
        review.comments.push(comment_at_line(2, "in repo"));
        // ...and one in a second root.
        let mut other = comment_at_line(3, "in other");
        other.id = ReviewCommentId("c-other".to_owned());
        other.location.root = ProjectRootPath("/other".to_owned());
        other.location.relative_path = "src/other.rs".to_owned();
        review.comments.push(other);
        let _ = mount_comments_surface(container.clone(), review, true);

        next_tick().await;
        next_tick().await;

        let groups = container
            .query_selector_all("[data-test=\"review-comments-root-group\"]")
            .unwrap();
        assert_eq!(
            groups.length(),
            2,
            "a review spanning two roots must render one group per root"
        );
        let roots: Vec<String> = (0..groups.length())
            .filter_map(|i| groups.item(i))
            .filter_map(|el| {
                el.dyn_into::<Element>()
                    .ok()
                    .and_then(|e| e.get_attribute("data-root"))
            })
            .collect();
        assert!(
            roots.iter().any(|r| r == "/repo") && roots.iter().any(|r| r == "/other"),
            "both roots must have a group; got: {roots:?}"
        );
    }

    /// Recording bridge: captures the serialized envelope `line` of every
    /// `send_host_line` invoke into `window.__sent_lines` so a test can assert
    /// on the exact frame payloads that went out.
    fn record_bridge() {
        let _ = js_sys::eval(
            "(function(){ \
               window.__sent_lines = []; \
               window.__TAURI__ = window.__TAURI__ || {}; \
               window.__TAURI__.core = window.__TAURI__.core || {}; \
               window.__TAURI__.core.invoke = function(cmd, args){ \
                 try { \
                   if (cmd === 'send_host_line' && args) { \
                     var line = (args.line !== undefined) ? args.line \
                       : (args.get ? args.get('line') : undefined); \
                     if (line !== undefined) { window.__sent_lines.push(line); } \
                   } \
                 } catch (e) {} \
                 return Promise.resolve(); }; \
               window.__TAURI__.event = window.__TAURI__.event || {}; \
               window.__TAURI__.event.listen = function(){ return Promise.resolve(function(){}); }; \
             })();",
        );
    }

    fn sent_lines_joined() -> String {
        js_sys::eval("(window.__sent_lines||[]).join('\\n')")
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default()
    }

    fn seed_host_settings(
        state: &AppState,
        default: Option<BackendKind>,
        enabled: Vec<BackendKind>,
    ) {
        state.host_settings_by_host.update(|m| {
            m.insert(
                "h1".to_owned(),
                protocol::HostSettings {
                    enabled_backends: enabled,
                    default_backend: default,
                    enable_mobile_connections: false,
                    mobile_broker_url: None,
                    tyde_debug_mcp_enabled: false,
                    tyde_agent_control_mcp_enabled: false,
                    complexity_tiers_enabled: false,
                    backend_tier_configs: std::collections::HashMap::new(),
                    background_agent_features: Default::default(),
                    code_intel: Default::default(),
                    backend_config: std::collections::HashMap::new(),
                    launch_profiles: Vec::new(),
                },
            );
        });
    }

    /// The comments surface subscribes lightweight: the `ReviewSubscribe`
    /// frame carries `include_diffs:false` so bootstrap/snapshots omit the
    /// redundant review diffs.
    #[wasm_bindgen_test]
    async fn comments_surface_subscribes_lightweight() {
        record_bridge();
        let container = make_container();
        // Summary only (no full record yet) + connected host ⇒ the surface
        // actually sends a ReviewSubscribe.
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.connection_statuses.update(|m| {
                m.insert("h1".to_owned(), crate::state::ConnectionStatus::Connected);
            });
            state.review_summaries.update(|m| {
                m.insert(
                    ProjectId("proj-1".to_owned()),
                    vec![protocol::ReviewSummary {
                        id: ReviewId("rev-1".to_owned()),
                        scope: protocol::ReviewSummaryScope::Workspace,
                        status: ReviewStatus::Draft,
                        origin_session_id: SessionId("s".to_owned()),
                        origin_agent_id: AgentId("a".to_owned()),
                        created_at_ms: 0,
                        updated_at_ms: 1,
                        user_comment_count: 0,
                        pending_suggestion_count: 0,
                        file_comment_counts: vec![],
                    }],
                );
            });
            provide_context(state);
            view! {
                <ReviewCommentsSurface
                    host_id="h1".to_owned()
                    project_id=ProjectId("proj-1".to_owned())
                />
            }
        });

        next_tick().await;
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("\"include_diffs\":false"),
            "ReviewSubscribe must request include_diffs:false; sent: {sent}"
        );
    }

    /// When a commented file's diff is not cached, the surface fetches just
    /// that file's unstaged diff (path-scoped), not the whole root.
    #[wasm_bindgen_test]
    async fn comments_surface_fetches_missing_commented_file_diff() {
        record_bridge();
        let container = make_container();
        let mut review = make_review();
        review.comments.push(comment_at_line(2, "a"));
        // seed_diff = false ⇒ no cached project diff, so a fetch must fire.
        let _ = mount_comments_surface(container.clone(), review, false);

        next_tick().await;
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("\"path\":\"src/foo.rs\""),
            "a path-scoped ProjectReadDiff for the commented file must be sent; \
             sent: {sent}"
        );
        // The lightweight surface must never request the whole-root diff
        // (path=None serializes as `\"path\":null`); only the missing
        // commented files are fetched.
        assert!(
            !sent.contains("\"path\":null"),
            "comments surface must not request a whole-root diff; sent: {sent}"
        );
    }

    /// Regression: a row created before its path-scoped diff arrives must show
    /// the snippet once the response lands — the snippet is reactive, not
    /// captured at row-creation time under a stable `<For>` key.
    #[wasm_bindgen_test]
    async fn comments_surface_snippet_appears_after_path_diff_response() {
        ensure_styles_loaded();
        record_bridge();
        let container = make_container();
        let mut review = make_review();
        review.comments.push(comment_at_line(2, "a"));
        // seed_diff = false ⇒ the row is created with no cached diff yet.
        let holder = mount_comments_surface(container.clone(), review, false);

        next_tick().await;
        next_tick().await;

        assert!(
            container
                .query_selector("[data-test=\"review-comments-entry\"]")
                .unwrap()
                .is_some(),
            "the entry row must render even before its diff loads"
        );
        assert!(
            container
                .query_selector(".review-comments-snippet")
                .unwrap()
                .is_none(),
            "no snippet should render before the path diff response lands"
        );

        // Inject the path-scoped diff response exactly as dispatch would store
        // it, under the per-file cache key.
        let state = holder.borrow().clone().unwrap();
        let key = DiffKey::new(
            "h1",
            ProjectId("proj-1".to_owned()),
            root_path(),
            ProjectDiffScope::Unstaged,
            "src/foo.rs",
        );
        state.diff_contents.update(|m| {
            m.insert(
                key,
                DiffViewState {
                    root: root_path(),
                    scope: ProjectDiffScope::Unstaged,
                    path: Some("src/foo.rs".to_owned()),
                    context_mode: DiffContextMode::Hunks,
                    pending: false,
                    files: diff_payload().files,
                },
            );
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("let x = 1;"),
            "snippet must appear once the path diff response lands; got: {text}"
        );
    }

    /// Boundary case for blocker #2: a top-of-hunk replacement (`-old` / `+new`)
    /// with no preceding context, anchored on New L1, must keep the `-old`
    /// line. The selected-side span starts at the `+new` line, so the adjacent
    /// removed line is pulled in by the opposite-side adjacency extension.
    #[wasm_bindgen_test]
    fn snippet_keeps_removed_line_at_hunk_top() {
        let files = vec![protocol::ProjectGitDiffFile {
            relative_path: "src/foo.rs".to_owned(),
            is_binary: false,
            hunks: vec![protocol::ProjectGitDiffHunk {
                hunk_id: "h1".to_owned(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    // No preceding context: the removed line is the very first.
                    protocol::ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Removed,
                        text: "old top".to_owned(),
                        old_line_number: Some(1),
                        new_line_number: None,
                    },
                    protocol::ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Added,
                        text: "new top".to_owned(),
                        old_line_number: None,
                        new_line_number: Some(1),
                    },
                ],
            }],
        }];
        let anchor = ReviewAnchor::LineRange {
            side: ReviewDiffSide::New,
            start_line: 1,
            end_line: 1,
        };
        let snippet = snippet_for_anchor(&files, "src/foo.rs", &anchor);
        assert!(
            snippet
                .iter()
                .any(|l| l.marker == '-' && l.text == "old top"),
            "the top-of-hunk removed line must be kept; got: {snippet:?}"
        );
        assert!(
            snippet
                .iter()
                .any(|l| l.marker == '+' && l.text == "new top"),
            "the added line at the anchor must be present; got: {snippet:?}"
        );
    }

    /// A New-side LineRange snippet keeps interleaved removed (`-`) lines in
    /// the window — they carry no new line number but sit positionally inside
    /// the anchored region, so dropping them would lose context.
    #[wasm_bindgen_test]
    fn snippet_keeps_removed_lines_around_new_anchor() {
        let files = vec![protocol::ProjectGitDiffFile {
            relative_path: "src/foo.rs".to_owned(),
            is_binary: false,
            hunks: vec![protocol::ProjectGitDiffHunk {
                hunk_id: "h1".to_owned(),
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    protocol::ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Context,
                        text: "fn f()".to_owned(),
                        old_line_number: Some(1),
                        new_line_number: Some(1),
                    },
                    protocol::ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Removed,
                        text: "old body".to_owned(),
                        old_line_number: Some(2),
                        new_line_number: None,
                    },
                    protocol::ProjectGitDiffLine {
                        kind: ProjectGitDiffLineKind::Added,
                        text: "new body".to_owned(),
                        old_line_number: None,
                        new_line_number: Some(2),
                    },
                ],
            }],
        }];
        let anchor = ReviewAnchor::LineRange {
            side: ReviewDiffSide::New,
            start_line: 2,
            end_line: 2,
        };
        let snippet = snippet_for_anchor(&files, "src/foo.rs", &anchor);
        assert!(
            snippet.iter().any(|l| l.marker == '-'),
            "the removed line interleaved at the anchor must be kept"
        );
        assert!(
            snippet
                .iter()
                .any(|l| l.marker == '+' && l.text == "new body"),
            "the added line at the anchor must be present"
        );
    }

    /// Entry ordering is numeric by line, not lexicographic over the Debug
    /// string (which would sort L10 before L2).
    #[wasm_bindgen_test]
    fn anchor_sort_key_orders_numerically() {
        let loc = |line: u32| ReviewLocation {
            root: root_path(),
            relative_path: "src/foo.rs".to_owned(),
            anchor: ReviewAnchor::LineRange {
                side: ReviewDiffSide::New,
                start_line: line,
                end_line: line,
            },
        };
        assert!(
            anchor_sort_key(&loc(2)) < anchor_sort_key(&loc(10)),
            "line 2 must sort before line 10"
        );
    }

    /// Default AI review delegates backend choice to the server: with no
    /// explicit picker selection, `StartAiReview.backend_kind` is `null`.
    #[wasm_bindgen_test]
    async fn run_ai_sends_none_backend_by_default() {
        ensure_styles_loaded();
        record_bridge();
        let container = make_container();
        let holder = mount_sidebar(container.clone(), make_review());
        // A default backend exists so Run AI is enabled, but the user makes
        // no explicit pick.
        seed_host_settings(
            &holder.borrow().clone().unwrap(),
            Some(BackendKind::Codex),
            vec![BackendKind::Codex],
        );
        next_tick().await;
        next_tick().await;

        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        run_btn.click();
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("start_ai_review"),
            "a StartAiReview frame must be sent; sent: {sent}"
        );
        // `backend_kind: None` is omitted from the wire (skip_serializing_if),
        // so the default path carries no concrete backend — the server picks.
        assert!(
            !sent.contains("\"backend_kind\""),
            "default AI review must omit backend_kind (server resolves the \
             default); sent: {sent}"
        );
    }

    /// An explicit picker selection overrides the default: the chosen backend
    /// is sent as `Some(kind)` even when it differs from the host default.
    #[wasm_bindgen_test]
    async fn run_ai_sends_some_backend_when_explicitly_picked() {
        ensure_styles_loaded();
        record_bridge();
        let container = make_container();
        let holder = mount_sidebar(container.clone(), make_review());
        // Host default is Antigravity; the user explicitly picks Codex.
        seed_host_settings(
            &holder.borrow().clone().unwrap(),
            Some(BackendKind::Antigravity),
            vec![BackendKind::Codex, BackendKind::Antigravity],
        );
        next_tick().await;
        next_tick().await;

        let select = container
            .query_selector(".review-backend-select")
            .unwrap()
            .expect("backend select rendered");
        let select: web_sys::HtmlSelectElement = select.dyn_into().unwrap();
        select.set_value("Codex");
        let ev = web_sys::Event::new("change").unwrap();
        select.dispatch_event(&ev).unwrap();
        next_tick().await;

        let run_btn =
            find_button_by_text(&container, "Run AI reviewer").expect("run AI button rendered");
        run_btn.click();
        next_tick().await;

        let sent = sent_lines_joined();
        assert!(
            sent.contains("\"backend_kind\":\"codex\""),
            "an explicit pick must send the chosen backend as Some(kind); \
             sent: {sent}"
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
