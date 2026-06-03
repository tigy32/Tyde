//! Reusable inline-review presentational components, extracted from
//! `review_view` so they can be mounted on any diff surface (the
//! standalone review workbench today; a generic uncommitted-diff view
//! later). These are protocol-shape-independent building blocks: they
//! render comments / AI suggestions and dispatch the stable review
//! actions (AddComment / UpdateComment / DeleteComment / AcceptSuggestion
//! / RejectSuggestion). Submit / Clear / target selection deliberately
//! stay in `review_view` so this module never references the
//! still-settling submit-target / clear payload shapes.

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    ProjectRootPath, ReviewActionPayload, ReviewAnchor, ReviewAnchorStatus, ReviewComment,
    ReviewCommentSource, ReviewSuggestedComment, ReviewSuggestionState,
};

use crate::components::review_view::{
    ComposerState, autosize_textarea, format_relative_time, send_review_action_with_failure_clear,
    try_claim_review_action,
};
use crate::state::{AppState, ReviewActionTarget};

/// Generic thread region. Renders all comments + pending suggestions (and
/// the inline composer when its location matches) whose `(root,
/// relative_path)` matches and whose anchor satisfies the matcher.
#[component]
pub(crate) fn ThreadRegionFiltered(
    review_id: protocol::ReviewId,
    root: ProjectRootPath,
    relative_path: String,
    host_id: String,
    composer: RwSignal<Option<ComposerState>>,
    matcher: std::sync::Arc<dyn Fn(&ReviewAnchor) -> bool + Send + Sync>,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let comments_review_id = review_id.clone();
    let cm_root = root.clone();
    let cm_path = relative_path.clone();
    let cm_matcher = matcher.clone();
    let comments: Memo<Vec<ReviewComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&comments_review_id) else {
                return Vec::new();
            };
            review
                .comments
                .iter()
                .filter(|c| {
                    c.location.root == cm_root
                        && c.location.relative_path == cm_path
                        && cm_matcher(&c.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    let suggestions_review_id = review_id.clone();
    let sg_root = root.clone();
    let sg_path = relative_path.clone();
    let sg_matcher = matcher.clone();
    let suggestions: Memo<Vec<ReviewSuggestedComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&suggestions_review_id) else {
                return Vec::new();
            };
            review
                .suggestions
                .iter()
                .filter(|s| {
                    s.location.root == sg_root
                        && s.location.relative_path == sg_path
                        && sg_matcher(&s.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    // Composer matcher: open ⇒ render here if the composer's location
    // (root + relative_path + anchor) matches this thread region.
    let composer_matcher = matcher.clone();
    let composer_root = root.clone();
    let composer_path = relative_path.clone();
    let composer_visible = move || -> bool {
        let Some(c) = composer.get() else {
            return false;
        };
        c.location.root == composer_root
            && c.location.relative_path == composer_path
            && composer_matcher(&c.location.anchor)
    };

    let composer_host = host_id.clone();
    let composer_review = review_id.clone();

    let comment_card_review_id = review_id.clone();
    let comment_card_host = host_id.clone();
    let suggestion_card_review_id = review_id.clone();
    let suggestion_card_host = host_id.clone();

    // Rejected-suggestions toggle for this region.
    let rejected_open: RwSignal<bool> = RwSignal::new(false);
    let sg_rejected_review_id = review_id.clone();
    let sg_rejected_root = root.clone();
    let sg_rejected_path = relative_path.clone();
    let sg_rejected_matcher = matcher.clone();
    let rejected_suggestions: Memo<Vec<ReviewSuggestedComment>> = Memo::new(move |_| {
        state.reviews.with(|map| {
            let Some(review) = map.get(&sg_rejected_review_id) else {
                return Vec::new();
            };
            review
                .suggestions
                .iter()
                .filter(|s| {
                    matches!(s.state, ReviewSuggestionState::Rejected)
                        && s.location.root == sg_rejected_root
                        && s.location.relative_path == sg_rejected_path
                        && sg_rejected_matcher(&s.location.anchor)
                })
                .cloned()
                .collect()
        })
    });

    view! {
        <div class="review-thread-region" data-rel-path={relative_path.clone()}>
            {move || {
                let card_review = comment_card_review_id.clone();
                let card_host = comment_card_host.clone();
                comments.get().into_iter().map(|c| {
                    view! {
                        <CommentCard
                            comment=c
                            host_id=card_host.clone()
                            review_id=card_review.clone()
                            is_draft=is_draft
                        />
                    }
                }).collect::<Vec<_>>()
            }}
            {move || {
                let sg_review = suggestion_card_review_id.clone();
                let sg_host = suggestion_card_host.clone();
                suggestions.get().into_iter()
                    .filter(|s| matches!(s.state, ReviewSuggestionState::Pending))
                    .map(|s| {
                        view! {
                            <SuggestionCard
                                suggestion=s
                                host_id=sg_host.clone()
                                review_id=sg_review.clone()
                                is_draft=is_draft
                            />
                        }
                    }).collect::<Vec<_>>()
            }}
            {move || {
                let rejected = rejected_suggestions.get();
                if rejected.is_empty() {
                    return None;
                }
                let count = rejected.len();
                let open = rejected_open.get();
                let toggle_label = if open {
                    format!("Hide {count} rejected")
                } else {
                    format!("{count} rejected")
                };
                let rejected_clone = rejected.clone();
                Some(view! {
                    <div class="review-rejected-region">
                        <button
                            class="review-rejected-toggle"
                            on:click=move |_| rejected_open.update(|v| *v = !*v)
                        >
                            {toggle_label}
                        </button>
                        {open.then(|| view! {
                            <div class="review-rejected-list">
                                {rejected_clone.into_iter().map(|s| view! {
                                    <RejectedSuggestionCard suggestion=s />
                                }).collect::<Vec<_>>()}
                            </div>
                        })}
                    </div>
                })
            }}
            {move || {
                if !composer_visible() {
                    return None;
                }
                let composer_state = composer.get()?;
                let composer_host = composer_host.clone();
                let composer_review = composer_review.clone();
                Some(view! {
                    <Composer
                        composer=composer
                        composer_state=composer_state
                        host_id=composer_host
                        review_id=composer_review
                        is_draft=is_draft
                    />
                })
            }}
        </div>
    }
}

/// Stale-anchor marker for a review thread. Mirrors the mobile stale pill:
/// when the anchored diff range no longer matches the current diff, the
/// server flags the anchor `Stale { reason }` and we surface it so the user
/// knows the comment may be pointing at moved/changed code.
fn stale_pill(status: &ReviewAnchorStatus) -> Option<impl IntoView + use<>> {
    match status {
        ReviewAnchorStatus::Current => None,
        ReviewAnchorStatus::Stale { reason } => {
            let reason = reason.clone();
            Some(view! {
                <span
                    class="review-stale-pill"
                    title={reason.clone()}
                    data-test="review-anchor-stale"
                >
                    {format!("stale \u{b7} {reason}")}
                </span>
            })
        }
    }
}

#[component]
pub(crate) fn CommentCard(
    comment: ReviewComment,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let pill = match &comment.source {
        ReviewCommentSource::User => view! {
            <span class="review-source-pill user">"You"</span>
        }
        .into_any(),
        ReviewCommentSource::AiSuggestion { edited, .. } => {
            let label = if *edited { "AI (edited)" } else { "AI" };
            view! { <span class="review-source-pill ai">{label}</span> }.into_any()
        }
    };
    let body_for_view = comment.body.clone();
    let comment_id = comment.id.clone();
    let comment_created_at = comment.created_at_ms;
    let comment_updated_at = comment.updated_at_ms;

    let is_user = matches!(comment.source, ReviewCommentSource::User);

    let edit_open = RwSignal::new(false);
    let edit_value = RwSignal::new(comment.body.clone());

    // Reactive pending flags for the per-row gates.
    let update_pending = {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::UpdateComment(cid.clone())))
            })
        }
    };
    let delete_pending = {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::DeleteComment(cid.clone())))
            })
        }
    };

    // Effect: close the edit form when an UpdateComment echo lands —
    // i.e. when the comment's body matches the edited value AND the
    // gate has dropped (was set, now not). Simplest reliable detection:
    // when the gate transitions from set → unset, snap the form shut
    // unless body still doesn't match (Error path keeps form open).
    {
        let rid = review_id.clone();
        let cid = comment_id.clone();
        let s = state.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s.review_action_target_pending.with(|set| {
                set.contains(&(rid.clone(), ReviewActionTarget::UpdateComment(cid.clone())))
            });
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                // Gate cleared. Determine whether the server accepted
                // the edit by comparing the typed value to the live
                // comment body. On accept they match; on error the
                // live body is unchanged and the typed value still
                // differs.
                let typed = edit_value.get_untracked();
                let live_body = s.reviews.with_untracked(|map| {
                    map.get(&rid).and_then(|r| {
                        r.comments
                            .iter()
                            .find(|c| c.id == cid)
                            .map(|c| c.body.clone())
                    })
                });
                if live_body.as_deref() == Some(typed.as_str()) {
                    edit_open.set(false);
                }
            }
        });
    }

    let comment_id_for_save = comment_id.clone();
    let host_id_for_save = host_id.clone();
    let review_id_for_save = review_id.clone();
    let state_for_save = state.clone();
    let do_save_edit: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        let body = edit_value.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let rid = review_id_for_save.clone();
        let cid = comment_id_for_save.clone();
        if !try_claim_review_action(
            &state_for_save,
            &rid,
            &ReviewActionTarget::UpdateComment(cid.clone()),
        ) {
            return;
        }
        let host = host_id_for_save.clone();
        let target_state = state_for_save.clone();
        let target_rid = rid.clone();
        let target_cid = cid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::UpdateComment {
                    comment_id: target_cid.clone(),
                    body,
                },
                ReviewActionTarget::UpdateComment(target_cid),
            )
            .await;
        });
    });

    let host_id_for_delete = host_id.clone();
    let review_id_for_delete = review_id.clone();
    let state_for_delete = state.clone();
    let comment_id_for_del = comment_id.clone();
    let comment_body_for_del = comment.body.clone();
    let on_delete = move |_| {
        let rid = review_id_for_delete.clone();
        let cid = comment_id_for_del.clone();
        let host = host_id_for_delete.clone();
        let target_state = state_for_delete.clone();
        let body_preview = {
            let trimmed = comment_body_for_del.trim();
            if trimmed.len() > 80 {
                format!("{}\u{2026}", &trimmed[..80])
            } else {
                trimmed.to_owned()
            }
        };
        spawn_local(async move {
            let message = if body_preview.is_empty() {
                "Delete this comment?".to_owned()
            } else {
                format!("Delete this comment?\n\n\u{201c}{body_preview}\u{201d}")
            };
            if !crate::bridge::confirm_dialog("Delete comment", &message).await {
                return;
            }
            if !try_claim_review_action(
                &target_state,
                &rid,
                &ReviewActionTarget::DeleteComment(cid.clone()),
            ) {
                return;
            }
            let target_rid = rid.clone();
            let target_cid = cid.clone();
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::DeleteComment {
                    comment_id: target_cid.clone(),
                },
                ReviewActionTarget::DeleteComment(target_cid),
            )
            .await;
        });
    };

    let do_save_edit_for_btn = do_save_edit.clone();
    let on_delete_for_btn = on_delete.clone();
    let comment_body_for_edit_open = comment.body.clone();
    // Wrap in Rc so independent view closures can each hold their own
    // cloneable handle without moving ownership.
    let edit_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let update_pending = update_pending.clone();
        move || !is_draft.get() || update_pending()
    });
    let delete_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let delete_pending = delete_pending.clone();
        move || !is_draft.get() || delete_pending()
    });

    let edit_disabled_for_edit_block = edit_disabled.clone();
    let edit_disabled_for_actions = edit_disabled.clone();
    let delete_disabled_for_actions = delete_disabled.clone();

    view! {
        <div class="review-comment-card" data-comment-id={comment.id.0.clone()}
             data-source={match &comment.source {
                 ReviewCommentSource::User => "user",
                 ReviewCommentSource::AiSuggestion { .. } => "ai",
             }}>
            <div class="review-comment-header">
                {pill}
                {stale_pill(&comment.anchor_status)}
                {(comment_created_at > 0).then(|| {
                    let edited = comment_updated_at > comment_created_at;
                    let title = if edited {
                        format!(
                            "Created {}, edited {}",
                            format_relative_time(comment_created_at),
                            format_relative_time(comment_updated_at)
                        )
                    } else {
                        format!("Created {}", format_relative_time(comment_created_at))
                    };
                    let stamp = format_relative_time(
                        if edited { comment_updated_at } else { comment_created_at }
                    );
                    let label = if edited { format!("{stamp} (edited)") } else { stamp };
                    view! {
                        <span class="review-comment-time" title={title}>{label}</span>
                    }
                })}
            </div>
            {move || {
                if edit_open.get() {
                    let save_disabled = edit_disabled_for_edit_block.clone();
                    let save_disabled_for_keydown = save_disabled.clone();
                    let do_save = do_save_edit_for_btn.clone();
                    let do_save_for_keydown = do_save.clone();
                    let edit_textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                    Effect::new(move |_| {
                        if let Some(el) = edit_textarea_ref.get() {
                            let _ = el.focus();
                            // Caret to end so the user can keep typing
                            // without having to navigate manually.
                            let len = el.value().len() as u32;
                            let _ = el.set_selection_range(len, len);
                        }
                    });
                    autosize_textarea(edit_textarea_ref, edit_value);
                    view! {
                        <div class="review-comment-edit">
                            <textarea
                                node_ref=edit_textarea_ref
                                class="review-textarea"
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                    if ev.key() == "Escape" {
                                        ev.prevent_default();
                                        edit_open.set(false);
                                        return;
                                    }
                                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key())
                                        && !save_disabled_for_keydown()
                                    {
                                        ev.prevent_default();
                                        do_save_for_keydown();
                                    }
                                }
                            />
                            <div class="review-comment-actions">
                                <button class="review-btn primary review-comment-edit-save"
                                        disabled=move || save_disabled()
                                        on:click=move |_| do_save()>
                                    "Save"
                                </button>
                                <button
                                    class="review-btn"
                                    on:click={
                                        let original = body_for_view.clone();
                                        move |_| {
                                            let typed = edit_value.get_untracked();
                                            if typed == original {
                                                edit_open.set(false);
                                                return;
                                            }
                                            spawn_local(async move {
                                                if crate::bridge::confirm_dialog(
                                                    "Discard edit",
                                                    "You have unsaved changes. Discard them?",
                                                ).await {
                                                    edit_open.set(false);
                                                }
                                            });
                                        }
                                    }
                                >
                                    "Cancel"
                                </button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    let body_html = body_for_view.clone();
                    view! {
                        <div class="review-comment-body"
                             inner_html=crate::markdown::render_markdown(&body_html)>
                        </div>
                    }.into_any()
                }
            }}
            {is_user.then(|| {
                let on_delete = on_delete_for_btn.clone();
                let edit_disabled = edit_disabled_for_actions.clone();
                let delete_disabled = delete_disabled_for_actions.clone();
                view! {
                    <div class="review-comment-actions">
                        <button class="review-btn review-edit-btn"
                                disabled=move || edit_disabled()
                                on:click=move |_| {
                                    edit_value.set(comment_body_for_edit_open.clone());
                                    edit_open.set(true);
                                }>"Edit"</button>
                        <button class="review-btn destructive review-delete-btn"
                                disabled=move || delete_disabled()
                                on:click=on_delete>"Delete"</button>
                    </div>
                }
            })}
        </div>
    }
}

#[component]
pub(crate) fn SuggestionCard(
    suggestion: ReviewSuggestedComment,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let edit_open = RwSignal::new(false);
    let edit_value = RwSignal::new(suggestion.body.clone());
    let body_for_view = suggestion.body.clone();
    let suggestion_id = suggestion.id.clone();

    let accept_pending = {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::AcceptSuggestion(sid.clone()),
                ))
            })
        }
    };
    let reject_pending = {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        move || {
            s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::RejectSuggestion(sid.clone()),
                ))
            })
        }
    };

    // Effect: close the edit form once AcceptSuggestion gate clears AND
    // the suggestion has transitioned to Accepted (success path). On
    // error the suggestion stays Pending, leaving the form open.
    {
        let rid = review_id.clone();
        let sid = suggestion_id.clone();
        let s = state.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s.review_action_target_pending.with(|set| {
                set.contains(&(
                    rid.clone(),
                    ReviewActionTarget::AcceptSuggestion(sid.clone()),
                ))
            });
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                let accepted = s.reviews.with_untracked(|map| {
                    map.get(&rid)
                        .and_then(|r| r.suggestions.iter().find(|sg| sg.id == sid))
                        .map(|sg| matches!(sg.state, ReviewSuggestionState::Accepted { .. }))
                        .unwrap_or(false)
                });
                if accepted {
                    edit_open.set(false);
                }
            }
        });
    }

    let host_for_accept = host_id.clone();
    let review_for_accept = review_id.clone();
    let suggestion_for_accept = suggestion_id.clone();
    let state_for_accept = state.clone();
    let on_accept = move |_| {
        let rid = review_for_accept.clone();
        let sid = suggestion_for_accept.clone();
        if !try_claim_review_action(
            &state_for_accept,
            &rid,
            &ReviewActionTarget::AcceptSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_accept.clone();
        let target_state = state_for_accept.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: target_sid.clone(),
                    edit: None,
                },
                ReviewActionTarget::AcceptSuggestion(target_sid),
            )
            .await;
        });
    };

    let host_for_edit_accept = host_id.clone();
    let review_for_edit_accept = review_id.clone();
    let suggestion_for_edit_accept = suggestion_id.clone();
    let state_for_edit_accept = state.clone();
    let do_edit_accept: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        let body = edit_value.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let rid = review_for_edit_accept.clone();
        let sid = suggestion_for_edit_accept.clone();
        if !try_claim_review_action(
            &state_for_edit_accept,
            &rid,
            &ReviewActionTarget::AcceptSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_edit_accept.clone();
        let target_state = state_for_edit_accept.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: target_sid.clone(),
                    edit: Some(body),
                },
                ReviewActionTarget::AcceptSuggestion(target_sid),
            )
            .await;
        });
    });

    let host_for_reject = host_id.clone();
    let review_for_reject = review_id.clone();
    let suggestion_for_reject = suggestion_id.clone();
    let state_for_reject = state.clone();
    let on_reject = move |_| {
        let rid = review_for_reject.clone();
        let sid = suggestion_for_reject.clone();
        if !try_claim_review_action(
            &state_for_reject,
            &rid,
            &ReviewActionTarget::RejectSuggestion(sid.clone()),
        ) {
            return;
        }
        let host = host_for_reject.clone();
        let target_state = state_for_reject.clone();
        let target_rid = rid.clone();
        let target_sid = sid.clone();
        spawn_local(async move {
            send_review_action_with_failure_clear(
                target_state,
                &host,
                target_rid,
                ReviewActionPayload::RejectSuggestion {
                    suggestion_id: target_sid.clone(),
                },
                ReviewActionTarget::RejectSuggestion(target_sid),
            )
            .await;
        });
    };

    let severity_class = match suggestion.severity {
        protocol::ReviewSeverity::Info => "review-severity info",
        protocol::ReviewSeverity::Warn => "review-severity warn",
        protocol::ReviewSeverity::Bug => "review-severity bug",
    };

    let accept_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let accept_pending = accept_pending.clone();
        move || !is_draft.get() || accept_pending()
    });
    let reject_disabled: std::sync::Arc<dyn Fn() -> bool + Send + Sync> = std::sync::Arc::new({
        let reject_pending = reject_pending.clone();
        move || !is_draft.get() || reject_pending()
    });
    let edit_accept_save_disabled = accept_disabled.clone();

    view! {
        <div class="review-suggestion-card pending"
             data-suggestion-id={suggestion.id.0.clone()}
             data-state="pending">
            <div class="review-comment-header">
                <span class="review-source-pill ai-pending">"AI suggestion"</span>
                <span class={severity_class}>{format!("{:?}", suggestion.severity)}</span>
                {stale_pill(&suggestion.anchor_status)}
            </div>
            {move || {
                if edit_open.get() {
                    let do_save = do_edit_accept.clone();
                    let do_save_for_keydown = do_save.clone();
                    let save_disabled = edit_accept_save_disabled.clone();
                    let save_disabled_for_keydown = save_disabled.clone();
                    let edit_textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
                    Effect::new(move |_| {
                        if let Some(el) = edit_textarea_ref.get() {
                            let _ = el.focus();
                            let len = el.value().len() as u32;
                            let _ = el.set_selection_range(len, len);
                        }
                    });
                    autosize_textarea(edit_textarea_ref, edit_value);
                    view! {
                        <div class="review-comment-edit">
                            <textarea
                                node_ref=edit_textarea_ref
                                class="review-textarea"
                                prop:value=move || edit_value.get()
                                on:input=move |ev| edit_value.set(event_target_value(&ev))
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                    if ev.key() == "Escape" {
                                        ev.prevent_default();
                                        edit_open.set(false);
                                        return;
                                    }
                                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key())
                                        && !save_disabled_for_keydown()
                                    {
                                        ev.prevent_default();
                                        do_save_for_keydown();
                                    }
                                }
                            />
                            <div class="review-comment-actions">
                                <button class="review-btn primary review-edit-accept-save"
                                        disabled=move || save_disabled()
                                        on:click=move |_| do_save()>
                                    "Save & Accept"
                                </button>
                                <button class="review-btn" on:click={
                                    let original = body_for_view.clone();
                                    move |_| {
                                        let typed = edit_value.get_untracked();
                                        if typed == original {
                                            edit_open.set(false);
                                            return;
                                        }
                                        spawn_local(async move {
                                            if crate::bridge::confirm_dialog(
                                                "Discard edit",
                                                "You have unsaved changes. Discard them?",
                                            ).await {
                                                edit_open.set(false);
                                            }
                                        });
                                    }
                                }>
                                    "Cancel"
                                </button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    let body_html = body_for_view.clone();
                    view! {
                        <div class="review-comment-body"
                             inner_html=crate::markdown::render_markdown(&body_html)>
                        </div>
                    }.into_any()
                }
            }}
            {move || {
                if edit_open.get() {
                    view! { <span></span> }.into_any()
                } else {
                    let on_accept = on_accept.clone();
                    let on_reject = on_reject.clone();
                    let accept_disabled = accept_disabled.clone();
                    let reject_disabled = reject_disabled.clone();
                    view! {
                        <div class="review-comment-actions">
                            <button class="review-btn primary review-accept-btn"
                                    disabled=move || accept_disabled()
                                    on:click=on_accept>
                                "Accept"
                            </button>
                            <button class="review-btn review-edit-accept-btn"
                                    disabled=move || !is_draft.get()
                                    on:click=move |_| edit_open.set(true)>
                                "Edit & Accept"
                            </button>
                            <button class="review-btn destructive review-reject-btn"
                                    disabled=move || reject_disabled()
                                    on:click=on_reject>
                                "Reject"
                            </button>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// Read-only card for rejected AI suggestions, displayed under the
/// "show N rejected" toggle. No accept/reject buttons — the suggestion
/// is already terminal.
#[component]
pub(crate) fn RejectedSuggestionCard(suggestion: ReviewSuggestedComment) -> impl IntoView {
    let body = suggestion.body.clone();
    let severity_class = match suggestion.severity {
        protocol::ReviewSeverity::Info => "review-severity info",
        protocol::ReviewSeverity::Warn => "review-severity warn",
        protocol::ReviewSeverity::Bug => "review-severity bug",
    };
    view! {
        <div class="review-suggestion-card rejected"
             data-suggestion-id={suggestion.id.0.clone()}
             data-state="rejected">
            <div class="review-comment-header">
                <span class="review-source-pill ai-rejected">"AI (rejected)"</span>
                <span class={severity_class}>{format!("{:?}", suggestion.severity)}</span>
            </div>
            <div class="review-comment-body"
                 inner_html=crate::markdown::render_markdown(&body)>
            </div>
        </div>
    }
}

#[component]
pub(crate) fn Composer(
    composer: RwSignal<Option<ComposerState>>,
    composer_state: ComposerState,
    host_id: String,
    review_id: protocol::ReviewId,
    is_draft: Memo<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let body = composer_state.body;
    let location = composer_state.location.clone();
    let host_for_send = host_id.clone();
    let review_for_send = review_id.clone();
    let location_for_send = location.clone();
    let state_for_send = state.clone();

    // Reactive pending flag. While set, Save button is disabled and
    // composer stays open.
    let rid_for_pending = review_id.clone();
    let pending_state = state.clone();
    let pending = move || {
        pending_state
            .review_action_target_pending
            .with(|set| set.contains(&(rid_for_pending.clone(), ReviewActionTarget::AddComment)))
    };

    // Effect: when an AddComment echo lands, the dispatch handler
    // clears the gate AND the new User comment with our location is
    // present in the live review record. Detect that exact transition
    // and close the composer. On error the gate clears without a
    // matching new comment ⇒ keep composer open with body intact.
    {
        let rid = review_id.clone();
        let s = state.clone();
        let target_location = location.clone();
        let was_pending = RwSignal::new(false);
        Effect::new(move |_| {
            let pending_now = s
                .review_action_target_pending
                .with(|set| set.contains(&(rid.clone(), ReviewActionTarget::AddComment)));
            let prev = was_pending.get_untracked();
            was_pending.set(pending_now);
            if prev && !pending_now {
                let echoed = s.reviews.with_untracked(|map| {
                    map.get(&rid)
                        .map(|r| {
                            r.comments.iter().any(|c| {
                                matches!(c.source, ReviewCommentSource::User)
                                    && c.location == target_location
                            })
                        })
                        .unwrap_or(false)
                });
                if echoed {
                    composer.set(None);
                }
            }
        });
    }

    let do_save: std::sync::Arc<dyn Fn() + Send + Sync> = {
        let host_for_send = host_for_send.clone();
        let review_for_send = review_for_send.clone();
        let location_for_send = location_for_send.clone();
        let state_for_send = state_for_send.clone();
        let body_for_save = body;
        std::sync::Arc::new(move || {
            let body_text = body_for_save.get_untracked().trim().to_owned();
            if body_text.is_empty() {
                return;
            }
            let rid = review_for_send.clone();
            if !try_claim_review_action(&state_for_send, &rid, &ReviewActionTarget::AddComment) {
                return;
            }
            let host = host_for_send.clone();
            let location = location_for_send.clone();
            let target_state = state_for_send.clone();
            let target_rid = rid.clone();
            spawn_local(async move {
                send_review_action_with_failure_clear(
                    target_state,
                    &host,
                    target_rid,
                    ReviewActionPayload::AddComment {
                        location,
                        body: body_text,
                    },
                    ReviewActionTarget::AddComment,
                )
                .await;
            });
        })
    };
    let do_save_for_btn = do_save.clone();
    let do_save_for_keydown = do_save.clone();

    let save_disabled = {
        let pending = pending.clone();
        move || !is_draft.get() || pending()
    };
    let save_disabled_for_keydown = save_disabled.clone();

    let textarea_ref: NodeRef<leptos::html::Textarea> = NodeRef::new();
    Effect::new(move |_| {
        if let Some(el) = textarea_ref.get() {
            let _ = el.focus();
        }
    });
    autosize_textarea(textarea_ref, body);

    view! {
        <div class="review-composer">
            <textarea
                node_ref=textarea_ref
                class="review-textarea"
                placeholder="Comment\u{2026} (\u{2318}/Ctrl+Enter to save, Esc to cancel)"
                prop:value=move || body.get()
                on:input=move |ev| body.set(event_target_value(&ev))
                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                    if ev.key() == "Escape" {
                        ev.prevent_default();
                        composer.set(None);
                        return;
                    }
                    if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) {
                        ev.prevent_default();
                        if !save_disabled_for_keydown() {
                            do_save_for_keydown();
                        }
                    }
                }
            />
            <div class="review-composer-actions">
                <button class="review-btn primary review-composer-save"
                        disabled=save_disabled
                        on:click=move |_| do_save_for_btn()>
                    "Save"
                </button>
                <button class="review-btn review-composer-cancel"
                        on:click=move |_| {
                            let body_now = body.get_untracked();
                            if body_now.trim().is_empty() {
                                composer.set(None);
                                return;
                            }
                            spawn_local(async move {
                                if crate::bridge::confirm_dialog(
                                    "Discard comment",
                                    "You have unsaved text. Discard it?",
                                ).await {
                                    composer.set(None);
                                }
                            });
                        }>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}
