use leptos::prelude::*;
use protocol::{ContextBreakdown, CurrentContextUsage, TaskList, TaskStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryView {
    Context,
    Tasks,
}

#[derive(Clone, Copy)]
struct ContextCategory {
    label: &'static str,
    css_class: &'static str,
    dot_class: &'static str,
    percent: f64,
}

#[component]
pub fn TaskListView(
    task_list: Option<TaskList>,
    context_breakdown: Option<ContextBreakdown>,
    #[prop(optional)] current_context_usage: Option<CurrentContextUsage>,
) -> impl IntoView {
    let active_view = RwSignal::new(SummaryView::Context);
    let collapsed = RwSignal::new(false);
    let task_list_for_context = task_list.clone();
    let context_breakdown_for_context = context_breakdown.clone();
    let current_context_usage_for_context = current_context_usage.clone();

    let has_context = Memo::new(move |_| {
        current_context_usage_for_context.is_some()
            || context_breakdown_for_context
                .as_ref()
                .is_some_and(|bd| bd.input_tokens > 0)
    });
    let has_tasks = Memo::new(move |_| {
        task_list_for_context
            .as_ref()
            .is_some_and(|tl| !tl.tasks.is_empty())
    });

    Effect::new(move |_| {
        let view = active_view.get();
        if view == SummaryView::Tasks && !has_tasks.get() {
            active_view.set(SummaryView::Context);
        }
    });

    view! {
        <div class=move || {
            let show = has_context.get() || has_tasks.get();
            if show { "task-list-panel" } else { "task-list-panel hidden" }
        }>
            {move || {
                let has_context_now = has_context.get();
                let has_tasks_now = has_tasks.get();
                let view_mode = active_view.get();

                if view_mode == SummaryView::Tasks && has_tasks_now {
                    let tl = task_list.clone().expect("task list should exist when showing tasks");
                    render_task_view(
                        tl,
                        context_breakdown_for_display(
                            current_context_usage.as_ref(),
                            context_breakdown.as_ref(),
                        ).filter(|_| has_context_now),
                        collapsed,
                        active_view,
                    ).into_any()
                } else {
                    render_context_view(
                        current_context_usage.clone().filter(|_| has_context_now),
                        context_breakdown_for_display(
                            current_context_usage.as_ref(),
                            context_breakdown.as_ref(),
                        ).filter(|_| has_context_now),
                        task_list.clone().filter(|tl| !tl.tasks.is_empty()),
                        active_view,
                    ).into_any()
                }
            }}
        </div>
    }
}

fn render_context_view(
    current_context_usage: Option<CurrentContextUsage>,
    breakdown: Option<ContextBreakdown>,
    task_list: Option<TaskList>,
    active_view: RwSignal<SummaryView>,
) -> impl IntoView {
    let metrics = breakdown.as_ref().map(compute_context_metrics);
    let has_detailed_breakdown = breakdown.as_ref().is_some_and(has_detailed_breakdown);
    let context_is_unknown = matches!(current_context_usage, Some(CurrentContextUsage::Unknown));

    view! {
        <div class="summary-panel">
            <div class="summary-context-view">
                <div class="summary-context-header">
                    // Scope-bearing title: this panel is the *latest prompt's*
                    // occupancy, while the session footer shows task-wide
                    // cumulative usage. Two unlabelled token figures of
                    // different scope read as an arithmetic bug.
                    <span
                        class="summary-context-title"
                        title="Tokens occupied by the most recent request. The \
                               session footer shows cumulative usage across the \
                               whole task instead."
                    >"Current context"</span>
                    {metrics.as_ref().map(|m| {
                        let usage_class = if m.utilization_pct > 95.0 {
                            "summary-context-usage context-danger"
                        } else if m.utilization_pct > 80.0 {
                            "summary-context-usage context-warning"
                        } else {
                            "summary-context-usage"
                        };
                        view! {
                            <span class=usage_class data-testid="context-usage">
                                {format!(
                                    "{} / {} tokens ({:.1}%)",
                                    format_token_count(m.total_used),
                                    format_token_count(m.context_window),
                                    m.utilization_pct
                                )}
                            </span>
                        }
                    })}
                    {context_is_unknown.then(|| {
                        view! {
                            <span
                                class="summary-context-usage context-unknown"
                                data-testid="context-usage"
                            >
                                "Unavailable"
                            </span>
                        }
                    })}
                </div>
                <div
                    class=if context_is_unknown {
                        "summary-context-bar context-unknown"
                    } else {
                        "summary-context-bar"
                    }
                    data-testid="context-bar"
                    role="progressbar"
                    aria-label="Context utilization"
                    aria-valuemin="0"
                    aria-valuemax="100"
                    aria-valuenow=(!context_is_unknown).then(|| {
                        metrics
                            .as_ref()
                            .map(|m| format!("{}", m.utilization_pct.round() as i32))
                            .unwrap_or_else(|| "0".to_owned())
                    })
                    aria-valuetext=metrics
                        .as_ref()
                        .map(|m| format!(
                            "{} of {} tokens ({:.1}%)",
                            format_token_count(m.total_used),
                            format_token_count(m.context_window),
                            m.utilization_pct,
                        ))
                        .unwrap_or_else(|| "Context usage unknown".to_owned())
                >
                    {metrics.as_ref().map(|m| {
                        m.categories.iter().filter(|cat| cat.percent > 0.0).map(|cat| {
                            // Explicitly presentational. A progressbar's
                            // descendants are presentational per ARIA, so a
                            // role/name here would not reach the accessibility
                            // tree however it were written. The readable
                            // breakdown lives in the sibling legend below;
                            // `title` still serves a sighted pointer user.
                            let name = format!("{}: {:.0}% of context", cat.label, cat.percent);
                            view! {
                                <span
                                    class=format!("summary-context-segment {}", cat.css_class)
                                    data-testid="context-segment"
                                    aria-hidden="true"
                                    title=name
                                    style=format!("width: {:.2}%", cat.percent)
                                ></span>
                            }
                        }).collect::<Vec<_>>()
                    })}
                </div>
                {move || {
                    match (metrics.as_ref(), task_list.as_ref()) {
                        (Some(m), Some(tl)) => view! {
                            <div class="summary-context-meta">
                                {render_breakdown_or_note(&m.categories, has_detailed_breakdown)}
                                <button
                                    type="button"
                                    class="context-task-hint"
                                    on:click=move |_| active_view.set(SummaryView::Tasks)
                                >
                                    {build_task_hint_text(&tl.tasks)}
                                </button>
                            </div>
                        }.into_any(),
                        (Some(m), None) => view! {
                            <div class="summary-context-meta">
                                {render_breakdown_or_note(&m.categories, has_detailed_breakdown)}
                            </div>
                        }.into_any(),
                        (None, Some(tl)) => view! {
                            <div class="summary-context-meta">
                                <button
                                    type="button"
                                    class="context-task-hint"
                                    on:click=move |_| active_view.set(SummaryView::Tasks)
                                >
                                    {build_task_hint_text(&tl.tasks)}
                                </button>
                            </div>
                        }.into_any(),
                        _ => {
                            let _: () = view! { <></> };
                            ().into_any()
                        },
                    }
                }}
            </div>
        </div>
    }
}

fn context_breakdown_for_display(
    current: Option<&CurrentContextUsage>,
    estimated: Option<&ContextBreakdown>,
) -> Option<ContextBreakdown> {
    match current {
        None => estimated.cloned(),
        Some(CurrentContextUsage::Unknown) => None,
        Some(CurrentContextUsage::Known {
            input_tokens,
            context_window,
        }) => {
            let mut breakdown = estimated
                .filter(|estimate| {
                    estimate.input_tokens == *input_tokens
                        && estimate.context_window == *context_window
                })
                .cloned()
                .unwrap_or(ContextBreakdown {
                    system_prompt_bytes: 0,
                    tool_io_bytes: 0,
                    conversation_history_bytes: 0,
                    reasoning_bytes: 0,
                    context_injection_bytes: 0,
                    input_tokens: *input_tokens,
                    context_window: *context_window,
                });
            breakdown.input_tokens = *input_tokens;
            breakdown.context_window = *context_window;
            Some(breakdown)
        }
    }
}

fn render_task_view(
    task_list: TaskList,
    context_breakdown: Option<ContextBreakdown>,
    collapsed: RwSignal<bool>,
    active_view: RwSignal<SummaryView>,
) -> impl IntoView {
    let completed_count = task_list
        .tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Completed))
        .count();
    let total_count = task_list.tasks.len();
    let task_title = if task_list.title.is_empty() {
        "Tasks".to_owned()
    } else {
        task_list.title.clone()
    };
    let metrics = context_breakdown.as_ref().map(compute_context_metrics);

    view! {
        <div class="summary-panel">
            <div class="summary-task-view">
                <button
                    type="button"
                    class="task-list-header"
                    aria-expanded=move || (!collapsed.get()).to_string()
                    on:click=move |_| collapsed.update(|v| *v = !*v)
                >
                    <div class="task-list-title">
                        <span class="task-list-chevron">
                            {move || if collapsed.get() { "▶" } else { "▼" }}
                        </span>
                        <span class="task-list-heading">{task_title.clone()}</span>
                        <span class="task-list-progress">
                            {format!("{completed_count}/{total_count} tasks completed")}
                        </span>
                    </div>
                </button>
                <div class="task-list-items" role="list">
                    {move || task_rows_for_display(&task_list.tasks, collapsed.get()).into_iter().map(|row| {
                        let (icon, status_class) = status_meta(row.status);
                        view! {
                            <div class=format!("task-item-row {status_class}") role="listitem">
                                <span class="task-item-icon">{icon}</span>
                                <span class="task-item-desc">{row.description}</span>
                            </div>
                        }
                    }).collect::<Vec<_>>()}
                </div>
                {metrics.as_ref().map(|m| view! {
                    <button
                        type="button"
                        class="context-mini-bar"
                        aria-label="View context usage"
                        on:click=move |_| active_view.set(SummaryView::Context)
                    >
                        {m.categories.iter().filter(|cat| cat.percent > 0.0).map(|cat| {
                            view! {
                                <span
                                    class=format!("summary-context-segment {}", cat.css_class)
                                    style=format!("width: {:.2}%", cat.percent)
                                ></span>
                            }
                        }).collect::<Vec<_>>()}
                    </button>
                })}
            </div>
        </div>
    }
}

#[derive(Clone)]
struct TaskRow {
    description: String,
    status: TaskStatus,
}

fn task_rows_for_display(tasks: &[protocol::Task], collapsed: bool) -> Vec<TaskRow> {
    if !collapsed {
        return tasks
            .iter()
            .map(|task| TaskRow {
                description: task.description.clone(),
                status: task.status.clone(),
            })
            .collect();
    }

    if let Some(task) = tasks
        .iter()
        .find(|task| matches!(task.status, TaskStatus::InProgress))
    {
        return vec![TaskRow {
            description: task.description.clone(),
            status: task.status.clone(),
        }];
    }
    if let Some(task) = tasks
        .iter()
        .find(|task| matches!(task.status, TaskStatus::Pending))
    {
        return vec![TaskRow {
            description: task.description.clone(),
            status: task.status.clone(),
        }];
    }
    if let Some(task) = tasks
        .iter()
        .find(|task| matches!(task.status, TaskStatus::Failed))
    {
        return vec![TaskRow {
            description: task.description.clone(),
            status: task.status.clone(),
        }];
    }
    if tasks
        .iter()
        .all(|task| matches!(task.status, TaskStatus::Completed))
    {
        return vec![TaskRow {
            description: "All tasks completed!".to_owned(),
            status: TaskStatus::Completed,
        }];
    }

    tasks
        .first()
        .map(|task| {
            vec![TaskRow {
                description: task.description.clone(),
                status: task.status.clone(),
            }]
        })
        .unwrap_or_default()
}

fn build_task_hint_text(tasks: &[protocol::Task]) -> String {
    let total = tasks.len().max(1);
    let completed = tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Completed))
        .count();
    let has_in_progress = tasks
        .iter()
        .any(|task| matches!(task.status, TaskStatus::InProgress));
    if has_in_progress {
        let current = (completed + 1).min(total);
        format!("Task {current} of {total} in progress →")
    } else {
        format!("{completed}/{total} tasks done →")
    }
}

/// The legend when the backend attributed anything, and an explicit note when
/// it did not. Rendering nothing in the second case left a real, non-zero
/// occupancy with no explanation at all — for sighted and assistive-technology
/// users alike — which reads as "no context in use" rather than "attribution
/// was not reported".
fn render_breakdown_or_note(categories: &[ContextCategory], has_detail: bool) -> AnyView {
    if has_detail {
        return render_context_legend(categories).into_any();
    }
    view! {
        <div
            class="summary-context-breakdown summary-context-breakdown-empty"
            role="note"
            data-testid="context-breakdown-unavailable"
        >
            <span class="context-breakdown-row">
                "Breakdown unavailable \u{2014} this backend reported no context categories"
            </span>
        </div>
    }
    .into_any()
}

fn render_context_legend(categories: &[ContextCategory]) -> impl IntoView {
    let rows = categories
        .iter()
        .filter(|cat| cat.percent > 0.5) // Only show categories that are meaningful
        .map(|cat| {
            let pct_display = format!("{:.0}%", cat.percent);
            view! {
                <div class="context-breakdown-row">
                    <span class="context-breakdown-label">
                        <span class=format!("context-breakdown-dot {}", cat.dot_class)></span>
                        {cat.label}
                    </span>
                    <span class="context-breakdown-pct">{pct_display}</span>
                </div>
            }
        })
        .collect::<Vec<_>>();

    view! {
        // Named for what it actually measures. These shares are a partition of
        // the categories the backend reported, which is not necessarily the
        // whole prompt — `ContextBreakdown` carries no same-unit total against
        // which the remainder could be derived.
        <div
            class="summary-context-breakdown"
            role="group"
            aria-label="Share of the context categories this backend reported"
            title="Share of the context categories this backend reported"
        >
            {rows}
        </div>
    }
}

fn status_meta(status: TaskStatus) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Pending => ("•", "status-pending"),
        TaskStatus::InProgress => ("⟳", "status-in_progress"),
        TaskStatus::Completed => ("✓", "status-completed"),
        TaskStatus::Failed => ("✗", "status-failed"),
    }
}

struct ContextMetrics {
    categories: Vec<ContextCategory>,
    total_used: u64,
    context_window: u64,
    utilization_pct: f64,
}

fn compute_context_metrics(bd: &ContextBreakdown) -> ContextMetrics {
    let input_tokens = bd.input_tokens;
    let system_bytes = bd.system_prompt_bytes;
    let tool_bytes = bd.tool_io_bytes;
    let history_bytes = bd.conversation_history_bytes;
    let reasoning_bytes = bd.reasoning_bytes;
    let context_bytes = bd.context_injection_bytes;
    // Sum of everything the backend attributed. The category fields are bytes
    // and `input_tokens` is tokens: the protocol carries no same-unit total, so
    // the two cannot be compared and a *partial* breakdown is indistinguishable
    // from a complete one here. The segments below are therefore a partition of
    // what the backend reported — nothing more is inferable without a
    // same-unit remainder in `ContextBreakdown`.
    let total_bytes = system_bytes
        .saturating_add(tool_bytes)
        .saturating_add(history_bytes)
        .saturating_add(reasoning_bytes)
        .saturating_add(context_bytes);
    let context_window = bd.context_window.max(1);
    let utilization_pct = ((input_tokens as f64 / context_window as f64) * 100.0).clamp(0.0, 100.0);

    let mut categories = vec![
        ContextCategory {
            label: "System",
            css_class: "segment-system",
            dot_class: "dot-system",
            percent: 0.0,
        },
        ContextCategory {
            label: "Tools",
            css_class: "segment-tools",
            dot_class: "dot-tools",
            percent: 0.0,
        },
        ContextCategory {
            label: "History",
            css_class: "segment-history",
            dot_class: "dot-history",
            percent: 0.0,
        },
        ContextCategory {
            label: "Reasoning",
            css_class: "segment-reasoning",
            dot_class: "dot-reasoning",
            percent: 0.0,
        },
        ContextCategory {
            label: "Context",
            css_class: "segment-context",
            dot_class: "dot-context",
            percent: 0.0,
        },
    ];

    // No category reported anything. Previously this assigned the whole bar to
    // "Context", inventing an attribution the payload never carried; now it
    // simply leaves every segment at zero, and `has_detailed_breakdown` keeps
    // the legend suppressed so nothing claims to explain the occupancy.
    if total_bytes > 0 {
        let share = |bytes: u64| bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[0].percent = share(system_bytes);
        categories[1].percent = share(tool_bytes);
        categories[2].percent = share(history_bytes);
        categories[3].percent = share(reasoning_bytes);
        categories[4].percent = share(context_bytes);
    }

    ContextMetrics {
        categories,
        total_used: input_tokens,
        context_window,
        utilization_pct,
    }
}

fn has_detailed_breakdown(bd: &ContextBreakdown) -> bool {
    bd.system_prompt_bytes
        + bd.tool_io_bytes
        + bd.conversation_history_bytes
        + bd.reasoning_bytes
        + bd.context_injection_bytes
        > 0
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod native_tests {
    use super::*;

    /// Category fields are **bytes**; `input_tokens` is **tokens**. The two are
    /// deliberately never compared — the protocol carries no same-unit total,
    /// so any remainder derived from them would be fabricated.
    fn breakdown(
        system_bytes: u64,
        tool_bytes: u64,
        history_bytes: u64,
        reasoning_bytes: u64,
        context_bytes: u64,
        input_tokens: u64,
        context_window: u64,
    ) -> ContextBreakdown {
        ContextBreakdown {
            system_prompt_bytes: system_bytes,
            tool_io_bytes: tool_bytes,
            conversation_history_bytes: history_bytes,
            reasoning_bytes,
            context_injection_bytes: context_bytes,
            input_tokens,
            context_window,
        }
    }

    fn percent(metrics: &ContextMetrics, label: &str) -> f64 {
        metrics
            .categories
            .iter()
            .find(|category| category.label == label)
            .unwrap_or_else(|| panic!("category {label} should exist"))
            .percent
    }

    #[test]
    fn exact_context_without_matching_estimate_stays_neutral() {
        let known = CurrentContextUsage::Known {
            input_tokens: 120,
            context_window: 400_000,
        };
        let mismatched = breakdown(10, 20, 30, 40, 50, 119, 400_000);
        let display = context_breakdown_for_display(Some(&known), Some(&mismatched))
            .expect("exact occupancy remains renderable");
        assert_eq!(display.input_tokens, 120);
        assert_eq!(display.context_window, 400_000);
        assert!(!has_detailed_breakdown(&display));
        assert_eq!(
            context_breakdown_for_display(Some(&CurrentContextUsage::Unknown), Some(&mismatched)),
            None
        );
        assert_eq!(
            context_breakdown_for_display(None, Some(&mismatched)),
            Some(mismatched),
            "non-Codex breakdown rendering must remain unchanged"
        );
    }

    /// Guards against re-deriving an "unaccounted" remainder by comparing the
    /// byte-denominated categories with the token-denominated input. The mock
    /// backend's 250K payload (`server/src/backend/mock.rs`) is the concrete
    /// counter-example: its categories total 250,000 bytes against 250,000
    /// input tokens, so any bytes-per-token factor invents a shortfall and
    /// steals bar width from the categories the backend actually reported.
    #[test]
    fn categories_partition_the_bar_without_inferring_a_remainder() {
        // Exactly the mock's 250K fixture.
        let metrics = compute_context_metrics(&breakdown(
            100_000, 40_000, 60_000, 20_000, 30_000, 250_000, 300_000,
        ));

        let total: f64 = metrics.categories.iter().map(|c| c.percent).sum();
        assert!(
            (total - metrics.utilization_pct).abs() < 0.001,
            "reported categories must partition exactly the utilization, with no \
             invented remainder: total={total}, utilization={}",
            metrics.utilization_pct
        );
        // 100_000 of 250_000 reported bytes is 40% of the occupied bar.
        assert!(
            (percent(&metrics, "System") - metrics.utilization_pct * 0.4).abs() < 0.001,
            "a category's share must be its share of the reported bytes"
        );
        assert!(
            metrics.categories.iter().all(|c| c.label != "Unaccounted"),
            "no remainder category may exist: the protocol has no same-unit total \
             from which one could be derived"
        );
    }

    /// No category reported anything. The bar still shows real occupancy, but
    /// nothing may claim to explain it — this previously attributed the whole
    /// bar to "Context".
    #[test]
    fn empty_breakdown_attributes_nothing_to_a_real_category() {
        let bd = breakdown(0, 0, 0, 0, 0, 5_000, 10_000);
        let metrics = compute_context_metrics(&bd);

        assert_eq!(
            percent(&metrics, "Context"),
            0.0,
            "an empty breakdown must not be reported as context injection"
        );
        assert!(
            metrics.categories.iter().all(|c| c.percent == 0.0),
            "no segment may be drawn when nothing was attributed"
        );
        assert!(
            (metrics.utilization_pct - 50.0).abs() < 0.001,
            "real occupancy is still reported: 5K of a 10K window"
        );
        assert!(
            !has_detailed_breakdown(&bd),
            "an empty breakdown is not a detailed one, so no legend is claimed"
        );
    }

    /// A single reported category still fills the bar. That is a faithful
    /// rendering of a partial backend payload, not a frontend defect: without a
    /// same-unit total the frontend cannot distinguish "tools are the whole
    /// prompt" from "tools are all we recognized". Pinned so the distinction
    /// stays a documented backend dependency rather than being re-guessed here.
    #[test]
    fn a_single_reported_category_is_rendered_as_reported() {
        let bd = breakdown(0, 4_000, 0, 0, 0, 8_100, 1_048_000);
        let metrics = compute_context_metrics(&bd);

        assert!(
            (percent(&metrics, "Tools") - metrics.utilization_pct).abs() < 0.001,
            "the only reported category accounts for the whole reported total"
        );
        assert!(
            has_detailed_breakdown(&bd),
            "one reported category is still a breakdown, and the legend names it"
        );
    }

    /// Utilization is clamped, so a prompt exceeding its window cannot paint
    /// segments past the end of the bar.
    #[test]
    fn overfull_context_stays_within_the_bar() {
        let metrics = compute_context_metrics(&breakdown(50, 50, 0, 0, 0, 20_000, 10_000));

        assert!(
            (metrics.utilization_pct - 100.0).abs() < 0.001,
            "utilization clamps at 100%"
        );
        let total: f64 = metrics.categories.iter().map(|c| c.percent).sum();
        assert!(
            total <= 100.0 + 0.001,
            "segments must never exceed the bar they sit inside: total={total}"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: absolute; top: 0; left: 0; width: 600px; height: 400px;",
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

    fn mount_context(container: &HtmlElement, breakdown: ContextBreakdown) -> impl Sized {
        mount_to(container.clone(), move || {
            view! {
                <TaskListView task_list=None context_breakdown=Some(breakdown.clone()) />
            }
        })
    }

    fn segments(container: &HtmlElement) -> Vec<web_sys::Element> {
        let list = container
            .query_selector_all("[data-testid='context-segment']")
            .unwrap();
        (0..list.length())
            .filter_map(|i| list.item(i))
            .map(|node| node.dyn_into::<web_sys::Element>().unwrap())
            .collect()
    }

    /// The panel names its own scope. It measures the latest request, while the
    /// session footer measures the whole task; two unlabelled token figures of
    /// different scope read as an arithmetic error rather than as two different
    /// measurements.
    #[wasm_bindgen_test]
    async fn context_panel_names_its_scope() {
        let container = make_container();
        let _handle = mount_context(
            &container,
            ContextBreakdown {
                system_prompt_bytes: 100,
                tool_io_bytes: 100,
                conversation_history_bytes: 0,
                reasoning_bytes: 0,
                context_injection_bytes: 0,
                input_tokens: 5_000,
                context_window: 10_000,
            },
        );
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Current context"),
            "the panel must say which scope it measures, got: {text}"
        );
    }

    /// Every drawn slice carries its own accessible name. A bare coloured span
    /// conveys nothing without sight, and the parent progressbar reports only
    /// the overall percentage.
    #[wasm_bindgen_test]
    async fn every_context_segment_is_individually_named() {
        let container = make_container();
        let _handle = mount_context(
            &container,
            ContextBreakdown {
                system_prompt_bytes: 100,
                tool_io_bytes: 300,
                conversation_history_bytes: 0,
                reasoning_bytes: 0,
                context_injection_bytes: 0,
                input_tokens: 5_000,
                context_window: 10_000,
            },
        );
        next_tick().await;

        let drawn = segments(&container);
        assert_eq!(drawn.len(), 2, "two reported categories draw two segments");
        // Segments are descendants of a progressbar, whose children ARIA
        // defines as presentational. Claiming a role/name on them would not
        // reach the accessibility tree, so they must not pretend to.
        for segment in &drawn {
            assert_eq!(
                segment.get_attribute("aria-hidden").as_deref(),
                Some("true"),
                "a presentational segment must be hidden from assistive technology"
            );
            assert!(
                segment.get_attribute("role").is_none(),
                "a progressbar descendant must not claim a role it cannot expose"
            );
        }

        // The readable breakdown therefore lives outside the progressbar.
        let bar = container
            .query_selector("[data-testid='context-bar']")
            .unwrap()
            .expect("the utilization bar renders");
        assert!(
            bar.query_selector(".summary-context-breakdown")
                .unwrap()
                .is_none(),
            "the legend must be a sibling of the progressbar, not a descendant"
        );
        let legend = container
            .query_selector(".summary-context-breakdown")
            .unwrap()
            .expect("a reported breakdown renders a legend");
        let legend_text = legend.text_content().unwrap_or_default();
        assert!(
            legend_text.contains("System") && legend_text.contains("Tools"),
            "the legend must name each reported category in text, got: {legend_text}"
        );
        assert!(
            legend
                .get_attribute("aria-label")
                .unwrap_or_default()
                .contains("reported"),
            "the legend must scope its shares to what the backend reported"
        );

        // The bar itself carries the readable figure, since its children cannot.
        let value_text = bar.get_attribute("aria-valuetext").unwrap_or_default();
        assert!(
            value_text.contains("tokens"),
            "the progressbar must expose a human-readable value, got: {value_text}"
        );
    }

    /// A breakdown with no reported categories draws no segments and claims no
    /// legend, rather than attributing the occupancy to an arbitrary category.
    #[wasm_bindgen_test]
    async fn unattributed_context_draws_no_segment_and_no_legend() {
        let container = make_container();
        let _handle = mount_context(
            &container,
            ContextBreakdown {
                system_prompt_bytes: 0,
                tool_io_bytes: 0,
                conversation_history_bytes: 0,
                reasoning_bytes: 0,
                context_injection_bytes: 0,
                input_tokens: 5_000,
                context_window: 10_000,
            },
        );
        next_tick().await;

        assert!(
            segments(&container).is_empty(),
            "nothing was attributed, so no slice may be drawn"
        );

        // Rendering nothing here left a real occupancy wholly unexplained. The
        // absence of attribution is itself information and must be stated.
        let note = container
            .query_selector("[data-testid='context-breakdown-unavailable']")
            .unwrap()
            .expect("the missing-attribution state must be named, not blank");
        let note_text = note.text_content().unwrap_or_default();
        assert!(
            note_text.contains("Breakdown unavailable"),
            "the note must say attribution is missing, got: {note_text}"
        );
        assert_eq!(
            note.get_attribute("role").as_deref(),
            Some("note"),
            "the explanation must reach the accessibility tree"
        );
        assert!(
            container
                .query_selector(".context-breakdown-dot")
                .unwrap()
                .is_none(),
            "no category legend row may appear when nothing was attributed"
        );

        // The occupancy itself is still reported, in both forms.
        let bar = container
            .query_selector("[data-testid='context-bar']")
            .unwrap()
            .expect("the utilization bar still renders");
        assert_eq!(
            bar.get_attribute("aria-valuenow").as_deref(),
            Some("50"),
            "real occupancy is still exposed even with no attribution"
        );
        assert!(
            bar.get_attribute("aria-valuetext")
                .unwrap_or_default()
                .contains("tokens"),
            "the readable occupancy must survive the missing breakdown"
        );
    }
}
