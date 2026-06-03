use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{
    Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone, Spinner,
};
use crate::state::{AppState, LocalHostId, ReviewRef};

/// Mobile review detail. Subscribes on mount and renders status / diff
/// summary / comments / suggestions / AI reviewer state. Read-mostly:
/// accept-suggestion + reject-suggestion + cancel are wired in v1; the
/// inline-comment composer is intentionally out of scope (phone-sized
/// text-anchored composer is fragile, defer to desktop).
#[component]
pub fn ReviewDetail(
    host: LocalHostId,
    review_id: protocol::ReviewId,
    on_close: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let key = ReviewRef {
        local_host_id: host.clone(),
        review_id: review_id.clone(),
    };

    {
        let host = host.clone();
        let review_id = review_id.clone();
        let key = key.clone();
        let state = state.clone();
        Effect::new(move |_| {
            // Skip the outbound subscribe when we either already have a
            // snapshot OR have a stream registered for this review. The
            // stream check matters in wasm tests where the bridge is
            // absent — tests can pre-register the stream key to bypass
            // the side effect.
            let already_known = state.reviews.with_untracked(|r| r.contains_key(&key))
                || state
                    .review_streams
                    .with_untracked(|s| s.contains_key(&key));
            if already_known {
                return;
            }
            // Don't dispatch if we're not connected to this host.
            let connected = state
                .host_streams
                .with_untracked(|streams| streams.contains_key(&host));
            if !connected {
                return;
            }
            let host = host.clone();
            let review_id = review_id.clone();
            let state = state.clone();
            spawn_local(async move {
                if let Err(e) = crate::actions::subscribe_review(&state, &host, review_id).await {
                    log::error!("subscribe_review failed: {e}");
                }
            });
        });
    }

    let key_for_render = key.clone();
    let review = move || {
        state
            .reviews
            .with(|reviews| reviews.get(&key_for_render).cloned())
    };
    let key_for_error = key.clone();
    let error = move || {
        state
            .review_errors
            .with(|errors| errors.get(&key_for_error).cloned())
    };

    // Number of live same-project agents that could receive this review.
    // The legacy detail screen has no target picker, so Submit can only
    // auto-deliver when this is exactly 1 (see `on_submit`). Reactive over
    // both the loaded review (for its project_id) and the agent list.
    let host_for_count = host.clone();
    let key_for_count = key.clone();
    let state_for_count = state.clone();
    let candidate_count = move || {
        let Some(project_id) = state_for_count
            .reviews
            .with(|r| r.get(&key_for_count).map(|rv| rv.project_id.clone()))
        else {
            return 0usize;
        };
        state_for_count.agents.with(|agents| {
            agents
                .iter()
                .filter(|a| {
                    a.local_host_id == host_for_count
                        && a.project_id.as_ref() == Some(&project_id)
                        && a.fatal_error.is_none()
                })
                .count()
        })
    };

    // Submit / Cancel actions
    let host_for_actions = host.clone();
    let review_id_for_submit = review_id.clone();
    let state_for_submit = state.clone();
    let on_submit = Callback::new(move |_: ()| {
        let host = host_for_actions.clone();
        let review_id = review_id_for_submit.clone();
        let state = state_for_submit.clone();
        spawn_local(async move {
            // The contract requires a deliverable target (a live
            // same-project agent). The review's origin is NOT usable —
            // project-scoped reviews carry a synthetic origin. This legacy
            // detail screen has no target picker, so it can only auto-submit
            // when there is exactly one live same-project candidate; the
            // primary path is the diff-view picker.
            let key = ReviewRef {
                local_host_id: host.clone(),
                review_id: review_id.clone(),
            };
            let Some(project_id) = state
                .reviews
                .with_untracked(|r| r.get(&key).map(|rv| rv.project_id.clone()))
            else {
                log::error!(
                    "review submit: review {review_id:?} not loaded; cannot resolve target"
                );
                return;
            };
            let live: Vec<_> = state.agents.with_untracked(|agents| {
                agents
                    .iter()
                    .filter(|a| {
                        a.local_host_id == host
                            && a.project_id.as_ref() == Some(&project_id)
                            && a.fatal_error.is_none()
                    })
                    .map(|a| a.agent_id.clone())
                    .collect()
            });
            let target = match live.as_slice() {
                [only] => protocol::ReviewSubmitTarget::ExistingAgent {
                    agent_id: only.clone(),
                },
                _ => {
                    log::error!(
                        "review submit: need exactly one live same-project agent to auto-target \
                         (found {}); use the diff-view picker to choose",
                        live.len()
                    );
                    return;
                }
            };
            if let Err(e) = crate::actions::send_review_action(
                &state,
                &host,
                review_id,
                protocol::ReviewActionPayload::Submit { target },
            )
            .await
            {
                log::error!("review submit failed: {e}");
            }
        });
    });

    let host_for_cancel = host.clone();
    let review_id_for_cancel = review_id.clone();
    let state_for_cancel = state.clone();
    let on_cancel = Callback::new(move |_: ()| {
        let host = host_for_cancel.clone();
        let review_id = review_id_for_cancel.clone();
        let state = state_for_cancel.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &state,
                &host,
                review_id,
                protocol::ReviewActionPayload::Cancel,
            )
            .await
            {
                log::error!("review cancel failed: {e}");
            }
        });
    });

    let host_for_suggestion = host.clone();
    let review_id_for_suggestion = review_id.clone();
    let state_for_suggestion = state.clone();
    let on_accept_suggestion = Callback::new(move |id: protocol::ReviewSuggestionId| {
        let host = host_for_suggestion.clone();
        let review_id = review_id_for_suggestion.clone();
        let state = state_for_suggestion.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &state,
                &host,
                review_id,
                protocol::ReviewActionPayload::AcceptSuggestion {
                    suggestion_id: id,
                    edit: None,
                },
            )
            .await
            {
                log::error!("accept_suggestion failed: {e}");
            }
        });
    });
    let host_for_reject = host.clone();
    let review_id_for_reject = review_id.clone();
    let state_for_reject = state.clone();
    let on_reject_suggestion = Callback::new(move |id: protocol::ReviewSuggestionId| {
        let host = host_for_reject.clone();
        let review_id = review_id_for_reject.clone();
        let state = state_for_reject.clone();
        spawn_local(async move {
            if let Err(e) = crate::actions::send_review_action(
                &state,
                &host,
                review_id,
                protocol::ReviewActionPayload::RejectSuggestion { suggestion_id: id },
            )
            .await
            {
                log::error!("reject_suggestion failed: {e}");
            }
        });
    });

    let on_close_btn = on_close;

    view! {
        <div class="review-detail" data-mobile-test="review-detail">
            <header class="view-header">
                <h1 class="view-title">"Review " {review_id.0.clone()}</h1>
                <Button
                    label="Close"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="review-detail-close"
                    on_click=on_close_btn
                />
            </header>
            <div class="view-body">
                {move || {
                    let err = error();
                    err.map(|e| view! {
                        <div class="review-error" role="alert" data-mobile-test="review-detail-error">
                            <Pill label=format!("{:?}", e.code) tone=PillTone::Error />
                            <p>{e.message}</p>
                        </div>
                    })
                }}
                {move || {
                    let Some(review) = review() else {
                        return view! {
                            <div class="review-loading" data-mobile-test="review-detail-loading">
                                <Spinner aria_label="Loading review".to_string() />
                                <span>"Subscribing to review…"</span>
                            </div>
                        }.into_any();
                    };
                    render_loaded(review, on_submit, on_cancel, on_accept_suggestion, on_reject_suggestion, candidate_count.clone()).into_any()
                }}
            </div>
        </div>
    }
}

fn render_loaded(
    review: protocol::Review,
    on_submit: Callback<()>,
    on_cancel: Callback<()>,
    on_accept_suggestion: Callback<protocol::ReviewSuggestionId>,
    on_reject_suggestion: Callback<protocol::ReviewSuggestionId>,
    candidate_count: impl Fn() -> usize + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let status_label = crate::components::reviews_view::status_label(&review.status);
    let status_tone = crate::components::reviews_view::status_tone(&review.status);
    let comments = review.comments.clone();
    let suggestions = review.suggestions.clone();
    let diffs = review.diffs.clone();
    let ai = review.ai_reviewer.clone();
    let comment_count = comments.len();
    let suggestion_count = suggestions.len();
    let diff_file_count: usize = diffs.iter().map(|d| d.files.len()).sum();
    let is_draft = matches!(review.status, protocol::ReviewStatus::Draft);
    let ai_running = matches!(ai.status, protocol::ReviewAiReviewerStatus::Running);

    view! {
        <div class="review-detail-body">
            <div class="review-status-row" data-mobile-test="review-detail-status-row">
                <Pill
                    label=status_label
                    tone=status_tone
                    data_mobile_test="review-detail-status"
                />
                <Pill
                    label=format!("{comment_count} comment{}", if comment_count == 1 { "" } else { "s" })
                    tone=PillTone::Neutral
                />
                <Pill
                    label=format!("{suggestion_count} suggestion{}", if suggestion_count == 1 { "" } else { "s" })
                    tone=PillTone::Neutral
                />
                <Pill
                    label=format!("{diff_file_count} file{}", if diff_file_count == 1 { "" } else { "s" })
                    tone=PillTone::Neutral
                />
            </div>

            <section class="review-section" data-mobile-test="review-detail-ai">
                <h2 class="review-section-title">"AI Reviewer"</h2>
                <div class="review-ai-row">
                    <Pill
                        label=format!("{:?}", ai.status)
                        tone=ai_status_tone(ai.status)
                        data_mobile_test="review-ai-status"
                    />
                    {ai.error.map(|e| view! {
                        <span class="review-ai-error" data-mobile-test="review-ai-error">{e}</span>
                    })}
                    {if ai_running {
                        view! { <Spinner aria_label="AI reviewer running".to_string() /> }.into_any()
                    } else {
                        view! { <span></span> }.into_any()
                    }}
                </div>
            </section>

            <section class="review-section" data-mobile-test="review-detail-comments">
                <h2 class="review-section-title">"Comments"</h2>
                {if comments.is_empty() {
                    view! {
                        <EmptyState
                            title="No comments yet"
                            body="Comments authored on this review will appear here."
                            icon="\u{1F4AC}"
                            data_mobile_test="review-comments-empty"
                        />
                    }.into_any()
                } else {
                    view! {
                        <div class="review-list">
                            {comments.into_iter().map(|comment| {
                                let source = match comment.source {
                                    protocol::ReviewCommentSource::User => "User",
                                    protocol::ReviewCommentSource::AiSuggestion { .. } => "From AI suggestion",
                                };
                                view! {
                                    <Card dense=true data_mobile_test="review-comment-row">
                                        <div class="review-comment-source">{source}</div>
                                        <div class="review-comment-body">{comment.body}</div>
                                        <div class="review-comment-loc">{comment.location.relative_path}</div>
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </section>

            <section class="review-section" data-mobile-test="review-detail-suggestions">
                <h2 class="review-section-title">"Suggestions"</h2>
                {if suggestions.is_empty() {
                    view! {
                        <EmptyState
                            title="No suggestions"
                            body="The AI reviewer hasn't flagged anything yet."
                            icon="\u{1F4A1}"
                            data_mobile_test="review-suggestions-empty"
                        />
                    }.into_any()
                } else {
                    view! {
                        <div class="review-list">
                            {suggestions.into_iter().map(|s| {
                                let sid = s.id.clone();
                                let pending = matches!(s.state, protocol::ReviewSuggestionState::Pending);
                                let sid_accept = sid.clone();
                                let sid_reject = sid.clone();
                                let on_accept = on_accept_suggestion;
                                let on_reject = on_reject_suggestion;
                                let on_accept_click = Callback::new(move |_: ()| on_accept.run(sid_accept.clone()));
                                let on_reject_click = Callback::new(move |_: ()| on_reject.run(sid_reject.clone()));
                                let sev_tone = match s.severity {
                                    protocol::ReviewSeverity::Info => PillTone::Neutral,
                                    protocol::ReviewSeverity::Warn => PillTone::Warning,
                                    protocol::ReviewSeverity::Bug => PillTone::Error,
                                };
                                view! {
                                    <Card dense=true data_mobile_test="review-suggestion-row">
                                        <div style="display: flex; gap: var(--space-2); align-items: center; flex-wrap: wrap;">
                                            <Pill label=format!("{:?}", s.severity) tone=sev_tone />
                                            <Pill
                                                label=match s.state {
                                                    protocol::ReviewSuggestionState::Pending => "Pending".to_string(),
                                                    protocol::ReviewSuggestionState::Accepted { .. } => "Accepted".to_string(),
                                                    protocol::ReviewSuggestionState::Rejected => "Rejected".to_string(),
                                                }
                                                tone=match s.state {
                                                    protocol::ReviewSuggestionState::Pending => PillTone::Accent,
                                                    protocol::ReviewSuggestionState::Accepted { .. } => PillTone::Success,
                                                    protocol::ReviewSuggestionState::Rejected => PillTone::Warning,
                                                }
                                                data_mobile_test="review-suggestion-state"
                                            />
                                        </div>
                                        <div class="review-suggestion-body">{s.body}</div>
                                        {s.rationale.map(|r| view! { <div class="review-suggestion-rationale">{r}</div> })}
                                        <div class="review-comment-loc">{s.location.relative_path}</div>
                                        <Show when=move || pending>
                                            <div style="display: flex; gap: var(--space-1); margin-top: var(--space-2);">
                                                <Button
                                                    label="Accept"
                                                    variant=ButtonVariant::Primary
                                                    size=ButtonSize::Compact
                                                    data_mobile_test="review-suggestion-accept"
                                                    on_click=on_accept_click
                                                />
                                                <Button
                                                    label="Reject"
                                                    variant=ButtonVariant::Ghost
                                                    size=ButtonSize::Compact
                                                    data_mobile_test="review-suggestion-reject"
                                                    on_click=on_reject_click
                                                />
                                            </div>
                                        </Show>
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </section>

            <section class="review-section" data-mobile-test="review-detail-diffs">
                <h2 class="review-section-title">"Diff snapshot"</h2>
                {if diffs.is_empty() {
                    view! {
                        <EmptyState
                            title="No diff snapshot"
                            body="This review has no diff snapshot attached."
                            icon="\u{1F4DD}"
                            data_mobile_test="review-diffs-empty"
                        />
                    }.into_any()
                } else {
                    view! {
                        <div class="review-list">
                            {diffs.into_iter().map(|d| {
                                let scope = format!("{:?}", d.scope);
                                let file_count = d.files.len();
                                view! {
                                    <Card dense=true data_mobile_test="review-diff-summary">
                                        <div class="list-row-title">{d.root.0.clone()}</div>
                                        <div class="list-row-subtitle">
                                            {scope}" · "{file_count}" file"{if file_count == 1 { "" } else { "s" }}
                                        </div>
                                    </Card>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }}
            </section>

            <Show when=move || is_draft>
                <div class="review-footer" data-mobile-test="review-detail-footer">
                    {
                        // Submit can only auto-deliver to exactly one live
                        // same-project agent (this screen has no target picker).
                        // For zero/multiple candidates, disable Submit and point
                        // the user at the diff-view review picker instead of
                        // presenting an actionable button that silently no-ops.
                        let candidate_count = candidate_count.clone();
                        move || {
                            let count = candidate_count();
                            let can_submit = count == 1;
                            view! {
                                {(!can_submit).then(|| view! {
                                    <p
                                        class="review-footer-hint"
                                        data-mobile-test="review-detail-submit-hint"
                                    >
                                        {if count == 0 {
                                            "No live agent in this project to receive the review. \
                                             Open the diff view to choose a target."
                                        } else {
                                            "Multiple live agents in this project. Open the diff \
                                             view to choose which one receives the review."
                                        }}
                                    </p>
                                })}
                                <Button
                                    label="Submit"
                                    variant=ButtonVariant::Primary
                                    data_mobile_test="review-detail-submit"
                                    disabled=!can_submit
                                    on_click=on_submit
                                />
                            }
                        }
                    }
                    <Button
                        label="Cancel review"
                        variant=ButtonVariant::Destructive
                        data_mobile_test="review-detail-cancel"
                        on_click=on_cancel
                    />
                </div>
            </Show>
        </div>
    }
}

fn ai_status_tone(status: protocol::ReviewAiReviewerStatus) -> PillTone {
    match status {
        protocol::ReviewAiReviewerStatus::Idle => PillTone::Neutral,
        protocol::ReviewAiReviewerStatus::Running => PillTone::Accent,
        protocol::ReviewAiReviewerStatus::Completed => PillTone::Success,
        protocol::ReviewAiReviewerStatus::Failed => PillTone::Error,
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId, ReviewRef};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, ProjectId, ProjectRootPath, Review, ReviewAiReviewerState, ReviewAiReviewerStatus,
        ReviewAnchor, ReviewAnchorStatus, ReviewComment, ReviewCommentId, ReviewCommentSource,
        ReviewDiffSelection, ReviewId, ReviewLocation, ReviewSeverity, ReviewStatus,
        ReviewSuggestedComment, ReviewSuggestionId, ReviewSuggestionState, SessionId,
    };
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

    fn fixture_review(status: ReviewStatus) -> Review {
        Review {
            id: ReviewId("r1".to_owned()),
            project_id: ProjectId("p1".to_owned()),
            origin_agent_id: AgentId("a1".to_owned()),
            origin_session_id: SessionId("s1".to_owned()),
            selection: ReviewDiffSelection::AllUncommitted,
            status,
            diffs: Vec::new(),
            comments: vec![ReviewComment {
                id: ReviewCommentId("c1".to_owned()),
                location: ReviewLocation {
                    root: ProjectRootPath("/x".to_owned()),
                    relative_path: "src/lib.rs".to_owned(),
                    anchor: ReviewAnchor::File,
                },
                anchor_status: ReviewAnchorStatus::Current,
                body: "needs tests".to_owned(),
                source: ReviewCommentSource::User,
                created_at_ms: 0,
                updated_at_ms: 0,
            }],
            suggestions: vec![ReviewSuggestedComment {
                id: ReviewSuggestionId("sg1".to_owned()),
                location: ReviewLocation {
                    root: ProjectRootPath("/x".to_owned()),
                    relative_path: "src/lib.rs".to_owned(),
                    anchor: ReviewAnchor::File,
                },
                anchor_status: ReviewAnchorStatus::Current,
                body: "consider error::Error".to_owned(),
                rationale: Some("idiomatic Rust".to_owned()),
                severity: ReviewSeverity::Warn,
                state: ReviewSuggestionState::Pending,
                reviewer_agent_id: AgentId("ai".to_owned()),
                created_at_ms: 0,
            }],
            ai_reviewer: ReviewAiReviewerState {
                status: ReviewAiReviewerStatus::Idle,
                agent_id: None,
                error: None,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[wasm_bindgen_test]
    async fn review_detail_shows_loading_before_subscription() {
        // We pre-populate `review_streams` (without `reviews`) so the
        // mount-time subscribe Effect short-circuits — there's no Tauri
        // bridge in headless Chrome to absorb the outbound frame.
        // Loading state still shows because `reviews` is empty.
        let host = LocalHostId("h1".to_owned());
        let review_id = ReviewId("r1".to_owned());
        let host_for_mount = host.clone();
        let id_for_mount = review_id.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.review_streams.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host.clone(),
                        review_id: review_id.clone(),
                    },
                    protocol::StreamPath(format!("/review/{}", review_id.0)),
                );
            });
            provide_context(state);
            view! {
                <ReviewDetail
                    host=host_for_mount.clone()
                    review_id=id_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-loading']")
                .unwrap()
                .is_some()
        );
    }

    /// A live same-project agent that Submit could auto-target. `fatal`
    /// marks it dead (filtered out of the candidate count).
    fn fixture_agent(
        host: &LocalHostId,
        id: &str,
        project: &str,
        fatal: Option<&str>,
    ) -> crate::state::AgentInfo {
        crate::state::AgentInfo {
            local_host_id: host.clone(),
            agent_id: AgentId(id.to_owned()),
            name: id.to_owned(),
            origin: protocol::AgentOrigin::User,
            backend_kind: protocol::BackendKind::Codex,
            workspace_roots: vec!["/x".to_owned()],
            project_id: Some(ProjectId(project.to_owned())),
            parent_agent_id: None,
            session_id: Some(SessionId("s1".to_owned())),
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: protocol::StreamPath("/agent/a".to_owned()),
            started: true,
            fatal_error: fatal.map(|s| s.to_owned()),
        }
    }

    fn mount_draft_with_agents(agents: Vec<crate::state::AgentInfo>) -> HtmlElement {
        let host = LocalHostId("h1".to_owned());
        let review_id = ReviewId("r1".to_owned());
        let host_for_mount = host.clone();
        let id_for_mount = review_id.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host.clone(),
                        review_id: review_id.clone(),
                    },
                    fixture_review(ReviewStatus::Draft),
                );
            });
            state.agents.update(|a| *a = agents.clone());
            provide_context(state);
            view! {
                <ReviewDetail
                    host=host_for_mount.clone()
                    review_id=id_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        // Keep the mounted view alive past this helper's return — dropping
        // the handle would unmount before the caller queries the DOM.
        std::mem::forget(_h);
        container
    }

    /// Zero live same-project candidates: this screen can't pick a target,
    /// so Submit must be disabled and visible guidance must point at the
    /// diff-view picker. (The fixture's `project_id` is "p1".)
    #[wasm_bindgen_test]
    async fn review_detail_disables_submit_without_candidate() {
        let container = mount_draft_with_agents(Vec::new());
        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='review-detail-submit']")
            .unwrap()
            .expect("submit button still rendered");
        assert!(
            submit.has_attribute("disabled"),
            "Submit must be disabled when there is no single live same-project agent"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-submit-hint']")
                .unwrap()
                .is_some(),
            "guidance toward the diff-view picker must be visible"
        );
    }

    /// Multiple live same-project candidates is also ambiguous: still
    /// disabled, still guided.
    #[wasm_bindgen_test]
    async fn review_detail_disables_submit_with_multiple_candidates() {
        let host = LocalHostId("h1".to_owned());
        let container = mount_draft_with_agents(vec![
            fixture_agent(&host, "a1", "p1", None),
            fixture_agent(&host, "a2", "p1", None),
        ]);
        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='review-detail-submit']")
            .unwrap()
            .unwrap();
        assert!(
            submit.has_attribute("disabled"),
            "Submit must be disabled when multiple live same-project agents are candidates"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-submit-hint']")
                .unwrap()
                .is_some(),
            "guidance must be visible when the target is ambiguous"
        );
    }

    /// Exactly one live same-project candidate: Submit can auto-target it,
    /// so it is enabled and the guidance hint is absent. A dead agent
    /// (fatal_error) in the same project must not count toward the total.
    #[wasm_bindgen_test]
    async fn review_detail_enables_submit_with_single_candidate() {
        let host = LocalHostId("h1".to_owned());
        let container = mount_draft_with_agents(vec![
            fixture_agent(&host, "a1", "p1", None),
            fixture_agent(&host, "dead", "p1", Some("crashed")),
            fixture_agent(&host, "other", "p2", None),
        ]);
        next_tick().await;
        let submit = container
            .query_selector("[data-mobile-test='review-detail-submit']")
            .unwrap()
            .unwrap();
        assert!(
            !submit.has_attribute("disabled"),
            "Submit must be enabled when exactly one live same-project agent is a candidate"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-submit-hint']")
                .unwrap()
                .is_none(),
            "no guidance hint when Submit can auto-target"
        );
    }

    #[wasm_bindgen_test]
    async fn review_detail_renders_loaded_review_with_sections() {
        let host = LocalHostId("h1".to_owned());
        let review_id = ReviewId("r1".to_owned());
        let host_for_mount = host.clone();
        let id_for_mount = review_id.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.reviews.update(|m| {
                m.insert(
                    ReviewRef {
                        local_host_id: host.clone(),
                        review_id: review_id.clone(),
                    },
                    fixture_review(ReviewStatus::Draft),
                );
            });
            provide_context(state);
            view! {
                <ReviewDetail
                    host=host_for_mount.clone()
                    review_id=id_for_mount.clone()
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        // status row
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-status']")
                .unwrap()
                .is_some()
        );
        // comment row visible
        assert!(
            container
                .query_selector("[data-mobile-test='review-comment-row']")
                .unwrap()
                .is_some()
        );
        // pending suggestion accept/reject visible
        assert!(
            container
                .query_selector("[data-mobile-test='review-suggestion-accept']")
                .unwrap()
                .is_some()
        );
        // draft -> footer with submit + cancel
        assert!(
            container
                .query_selector("[data-mobile-test='review-detail-submit']")
                .unwrap()
                .is_some()
        );
    }
}
