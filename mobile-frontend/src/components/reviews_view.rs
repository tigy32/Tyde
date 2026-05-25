use leptos::prelude::*;

use crate::components::ui::{Button, ButtonSize, ButtonVariant, Card, EmptyState, Pill, PillTone};
use crate::state::{ActiveProjectRef, AppState};

/// Per-project review list. Surfaced from `ProjectsView` when the
/// active project has at least one `ReviewSummary`. Tapping a row sets
/// the local `viewing_review` signal which mounts `<ReviewDetail/>`.
#[component]
pub fn ReviewsView(
    project: ActiveProjectRef,
    on_open: Callback<protocol::ReviewId>,
    on_close: Callback<()>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let project_for_render = project.clone();
    let key = (project.local_host_id.clone(), project.project_id.clone());

    let summaries = move || {
        state
            .review_summaries
            .with(|map| map.get(&key).cloned())
            .unwrap_or_default()
    };

    view! {
        <div class="reviews-view" data-mobile-test="reviews-view">
            <header class="view-header">
                <h1 class="view-title">"Reviews"</h1>
                <Button
                    label="Back"
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Compact
                    data_mobile_test="reviews-close"
                    on_click=on_close
                />
            </header>
            <div class="view-body">
                {move || {
                    let _ = project_for_render.clone(); // hold the project alive in the closure
                    let items = summaries();
                    if items.is_empty() {
                        return view! {
                            <EmptyState
                                title="No reviews"
                                body="When an agent creates a review for this project, it'll show up here."
                                icon="\u{1F50D}"
                                data_mobile_test="reviews-empty"
                            />
                        }.into_any();
                    }
                    let mut sorted = items;
                    sorted.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
                    view! {
                        <div class="reviews-list" data-mobile-test="reviews-list">
                            {sorted.into_iter().map(|summary| {
                                let review_id = summary.id.clone();
                                let review_id_for_open = review_id.clone();
                                let on_open_row = Callback::new(move |_: ()| {
                                    on_open.run(review_id_for_open.clone());
                                });
                                let status_label = status_label(&summary.status);
                                let status_tone = status_tone(&summary.status);
                                let comment_count = summary.user_comment_count;
                                let suggestion_count = summary.pending_suggestion_count;
                                view! {
                                    <Card
                                        data_mobile_test="reviews-row"
                                        dense=true
                                        interactive=true
                                        aria_label=format!("Open review {}", review_id.0)
                                        on_click=on_open_row
                                    >
                                        <div class="list-row" style="border-bottom: none; padding: 0; align-items: flex-start;">
                                            <div class="list-row-primary">
                                                <div class="list-row-title">
                                                    {review_id.0.clone()}
                                                </div>
                                                <div class="list-row-subtitle">
                                                    {format!("{comment_count} comment{}, {suggestion_count} suggestion{}",
                                                        if comment_count == 1 { "" } else { "s" },
                                                        if suggestion_count == 1 { "" } else { "s" })}
                                                </div>
                                            </div>
                                            <div class="list-row-meta" style="display: flex; gap: var(--space-1);">
                                                <Pill
                                                    label=status_label
                                                    tone=status_tone
                                                    data_mobile_test="reviews-row-status"
                                                />
                                            </div>
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

pub(crate) fn status_label(status: &protocol::ReviewStatus) -> String {
    match status {
        protocol::ReviewStatus::Draft => "Draft".to_string(),
        protocol::ReviewStatus::Submitted { .. } => "Submitted".to_string(),
        protocol::ReviewStatus::Consumed { .. } => "Consumed".to_string(),
        protocol::ReviewStatus::Cancelled { .. } => "Cancelled".to_string(),
    }
}

pub(crate) fn status_tone(status: &protocol::ReviewStatus) -> PillTone {
    match status {
        protocol::ReviewStatus::Draft => PillTone::Neutral,
        protocol::ReviewStatus::Submitted { .. } => PillTone::Accent,
        protocol::ReviewStatus::Consumed { .. } => PillTone::Success,
        protocol::ReviewStatus::Cancelled { .. } => PillTone::Warning,
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{AgentId, ProjectId, ReviewId, ReviewStatus, ReviewSummary, SessionId};
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

    fn project_ref(host: &LocalHostId) -> ActiveProjectRef {
        ActiveProjectRef {
            local_host_id: host.clone(),
            project_id: ProjectId("p-1".to_owned()),
        }
    }

    fn fixture_summary(id: &str, status: ReviewStatus) -> ReviewSummary {
        ReviewSummary {
            id: ReviewId(id.to_owned()),
            status,
            origin_session_id: SessionId("s".to_owned()),
            origin_agent_id: AgentId("a".to_owned()),
            created_at_ms: 0,
            updated_at_ms: 0,
            user_comment_count: 0,
            pending_suggestion_count: 0,
        }
    }

    #[wasm_bindgen_test]
    async fn reviews_view_empty_state_when_no_summaries() {
        let host = LocalHostId("h1".to_owned());
        let project = project_ref(&host);
        let container = make_container();
        let project_for_mount = project.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <ReviewsView
                    project=project_for_mount.clone()
                    on_open=Callback::new(|_| {})
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='reviews-empty']")
                .unwrap()
                .is_some()
        );
    }

    #[wasm_bindgen_test]
    async fn reviews_view_lists_summaries_with_status_pills() {
        let host = LocalHostId("h1".to_owned());
        let project = project_ref(&host);
        let container = make_container();
        let project_for_mount = project.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.review_summaries.update(|m| {
                m.insert(
                    (host.clone(), ProjectId("p-1".to_owned())),
                    vec![
                        fixture_summary("r1", ReviewStatus::Draft),
                        fixture_summary("r2", ReviewStatus::Submitted { submitted_at_ms: 0 }),
                    ],
                );
            });
            provide_context(state);
            view! {
                <ReviewsView
                    project=project_for_mount.clone()
                    on_open=Callback::new(|_| {})
                    on_close=Callback::new(|_| {})
                />
            }
        });
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("r1") && text.contains("r2"),
            "both reviews listed"
        );
        assert!(text.contains("Draft"));
        assert!(text.contains("Submitted"));
    }
}
