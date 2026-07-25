use leptos::prelude::*;
use protocol::{ContextBreakdown, TaskList, TaskStatus};

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
) -> impl IntoView {
    let active_view = RwSignal::new(SummaryView::Context);
    let collapsed = RwSignal::new(false);
    let task_list_for_context = task_list.clone();
    let context_breakdown_for_context = context_breakdown.clone();

    let has_context = Memo::new(move |_| {
        context_breakdown_for_context
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
                        context_breakdown.clone().filter(|_| has_context_now),
                        collapsed,
                        active_view,
                    ).into_any()
                } else {
                    render_context_view(
                        context_breakdown.clone().filter(|_| has_context_now),
                        task_list.clone().filter(|tl| !tl.tasks.is_empty()),
                        active_view,
                    ).into_any()
                }
            }}
        </div>
    }
}

fn render_context_view(
    breakdown: Option<ContextBreakdown>,
    task_list: Option<TaskList>,
    active_view: RwSignal<SummaryView>,
) -> impl IntoView {
    let metrics = breakdown.as_ref().map(compute_context_metrics);
    let has_detailed_breakdown = breakdown.as_ref().is_some_and(has_detailed_breakdown);

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
                </div>
                <div
                    class="summary-context-bar"
                    data-testid="context-bar"
                    role="progressbar"
                    aria-label="Context utilization"
                    aria-valuemin="0"
                    aria-valuemax="100"
                    aria-valuenow=metrics
                        .as_ref()
                        .map(|m| format!("{}", m.utilization_pct.round() as i32))
                        .unwrap_or_else(|| "0".to_owned())
                >
                    {metrics.as_ref().map(|m| {
                        m.categories.iter().filter(|cat| cat.percent > 0.0).map(|cat| {
                            view! {
                                <span
                                    class=format!("summary-context-segment {}", cat.css_class)
                                    data-testid="context-segment"
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
                                {has_detailed_breakdown.then(|| render_context_legend(&m.categories))}
                                <button
                                    type="button"
                                    class="context-task-hint"
                                    on:click=move |_| active_view.set(SummaryView::Tasks)
                                >
                                    {build_task_hint_text(&tl.tasks)}
                                </button>
                            </div>
                        }.into_any(),
                        (Some(m), None) if has_detailed_breakdown => view! {
                            <div class="summary-context-meta">
                                {render_context_legend(&m.categories)}
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
        <div class="summary-context-breakdown">
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

/// Bytes-per-token estimate backends use when projecting category *token*
/// counts into the `*_bytes` fields of [`ContextBreakdown`] (the Hermes mapper
/// multiplies each category's tokens by this factor). It is used here only to
/// put the category buckets and `input_tokens` on one scale so the two can be
/// compared; it never alters a token figure shown to the user.
const BYTES_PER_TOKEN: u64 = 4;

fn compute_context_metrics(bd: &ContextBreakdown) -> ContextMetrics {
    let input_tokens = bd.input_tokens;
    let system_bytes = bd.system_prompt_bytes;
    let tool_bytes = bd.tool_io_bytes;
    let history_bytes = bd.conversation_history_bytes;
    let reasoning_bytes = bd.reasoning_bytes;
    let context_bytes = bd.context_injection_bytes;
    let accounted_bytes = system_bytes
        .saturating_add(tool_bytes)
        .saturating_add(history_bytes)
        .saturating_add(reasoning_bytes)
        .saturating_add(context_bytes);
    let context_window = bd.context_window.max(1);
    let utilization_pct = ((input_tokens as f64 / context_window as f64) * 100.0).clamp(0.0, 100.0);

    // The categories describe *part* of the prompt; `input_tokens` describes
    // all of it. Normalizing the categories against their own sum would let a
    // single recognized category claim the entire bar even when it explains a
    // sliver of real occupancy — which is exactly what a backend whose other
    // category ids we don't recognize produces. Normalize against the input
    // instead, and give whatever is left over its own visible segment rather
    // than silently inflating the categories we did recognize.
    let input_bytes = input_tokens.saturating_mul(BYTES_PER_TOKEN);
    let unaccounted_bytes = input_bytes.saturating_sub(accounted_bytes);
    // Buckets may overshoot the input (they are estimates); denominating by
    // the larger of the two keeps every share in range without clamping.
    let total_bytes = accounted_bytes.max(input_bytes);

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
        ContextCategory {
            label: "Unaccounted",
            css_class: "segment-unaccounted",
            dot_class: "dot-unaccounted",
            percent: 0.0,
        },
    ];

    // `total_bytes == 0` means the prompt is empty *and* no category reported
    // anything — there is genuinely nothing to attribute. Previously this case
    // assigned the whole bar to "Context", which invented an attribution the
    // data never supported.
    if total_bytes > 0 {
        let share = |bytes: u64| bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[0].percent = share(system_bytes);
        categories[1].percent = share(tool_bytes);
        categories[2].percent = share(history_bytes);
        categories[3].percent = share(reasoning_bytes);
        categories[4].percent = share(context_bytes);
        categories[5].percent = share(unaccounted_bytes);
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

    /// `ContextBreakdown` with the category buckets given in *tokens*, using
    /// the same tokens→bytes projection the backends apply.
    fn breakdown(
        system: u64,
        tools: u64,
        history: u64,
        reasoning: u64,
        context: u64,
        input_tokens: u64,
        window: u64,
    ) -> ContextBreakdown {
        ContextBreakdown {
            system_prompt_bytes: system * BYTES_PER_TOKEN,
            tool_io_bytes: tools * BYTES_PER_TOKEN,
            conversation_history_bytes: history * BYTES_PER_TOKEN,
            reasoning_bytes: reasoning * BYTES_PER_TOKEN,
            context_injection_bytes: context * BYTES_PER_TOKEN,
            input_tokens,
            context_window: window,
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

    /// The reported H6 shape: the backend recognized only the Tools category
    /// while the prompt is far larger. Normalizing the categories against their
    /// own sum made Tools claim the entire bar, presenting a sliver of real
    /// attribution as a complete picture.
    #[test]
    fn unrecognized_categories_surface_as_unaccounted_not_as_tools() {
        // 8.1K-token prompt of which only 1K is attributed to tools.
        let metrics = compute_context_metrics(&breakdown(0, 1_000, 0, 0, 0, 8_100, 1_048_000));

        let tools = percent(&metrics, "Tools");
        let unaccounted = percent(&metrics, "Unaccounted");
        assert!(
            tools < metrics.utilization_pct * 0.5,
            "Tools explains ~1K of an 8.1K prompt and must not dominate the bar: \
             tools={tools}, utilization={}",
            metrics.utilization_pct
        );
        assert!(
            unaccounted > tools,
            "the unattributed majority must be the larger segment: \
             tools={tools}, unaccounted={unaccounted}"
        );
        let total: f64 = metrics.categories.iter().map(|c| c.percent).sum();
        assert!(
            (total - metrics.utilization_pct).abs() < 0.001,
            "segments must still account for exactly the utilization: \
             total={total}, utilization={}",
            metrics.utilization_pct
        );
    }

    /// When the categories do explain the whole prompt there is nothing left
    /// over, and the bar is unchanged from its previous behavior.
    #[test]
    fn fully_attributed_breakdown_leaves_nothing_unaccounted() {
        let metrics =
            compute_context_metrics(&breakdown(100, 200, 500, 0, 200, 1_000, 10_000));

        assert_eq!(
            percent(&metrics, "Unaccounted"),
            0.0,
            "a breakdown that sums to the input must show no unaccounted segment"
        );
        assert!(
            (percent(&metrics, "History") - metrics.utilization_pct * 0.5).abs() < 0.001,
            "history is half the prompt and must render as half the bar"
        );
        assert_eq!(metrics.total_used, 1_000);
    }

    /// No category matched at all. The previous code assigned the whole bar to
    /// "Context", inventing an attribution the payload never supported.
    #[test]
    fn empty_breakdown_attributes_nothing_to_a_real_category() {
        let metrics = compute_context_metrics(&breakdown(0, 0, 0, 0, 0, 5_000, 10_000));

        assert_eq!(
            percent(&metrics, "Context"),
            0.0,
            "an empty breakdown must not be reported as context injection"
        );
        assert!(
            (percent(&metrics, "Unaccounted") - metrics.utilization_pct).abs() < 0.001,
            "with no categories reported, all known occupancy is unaccounted"
        );
        assert!(
            !has_detailed_breakdown(&breakdown(0, 0, 0, 0, 0, 5_000, 10_000)),
            "an empty breakdown is not a detailed one, so no legend is claimed"
        );
    }

    /// Category estimates can overshoot the measured input. Shares must stay
    /// within the bar rather than exceeding it or going negative.
    #[test]
    fn buckets_exceeding_input_stay_within_the_bar() {
        let metrics = compute_context_metrics(&breakdown(0, 4_000, 0, 0, 0, 1_000, 10_000));

        assert_eq!(
            percent(&metrics, "Unaccounted"),
            0.0,
            "there is no shortfall when the buckets already exceed the input"
        );
        let total: f64 = metrics.categories.iter().map(|c| c.percent).sum();
        assert!(
            total <= metrics.utilization_pct + 0.001,
            "segments must never exceed the utilization they sit inside: total={total}"
        );
    }
}
