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
        <div class="task-list-panel">
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
    let has_detailed_breakdown = breakdown
        .as_ref()
        .is_some_and(has_detailed_breakdown);

    view! {
        <div class="summary-panel">
            <div class="summary-context-view">
                <div class="summary-context-header">
                    <span class="summary-context-title">"Context Usage"</span>
                    {metrics.as_ref().map(|m| view! {
                        <span class="summary-context-usage" data-testid="context-usage">
                            {format!(
                                "{} / {} tokens ({:.1}%)",
                                format_token_count(m.total_used),
                                format_token_count(m.context_window),
                                m.utilization_pct
                            )}
                        </span>
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
                        _ => view! { <></> }.into_any(),
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

    tasks.first()
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
    let rows = categories.iter().map(|cat| {
        view! {
            <div class="context-breakdown-row">
                <span class="context-breakdown-label">
                    <span class=format!("context-breakdown-dot {}", cat.dot_class)></span>
                    {cat.label}
                </span>
            </div>
        }
    }).collect::<Vec<_>>();

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

fn compute_context_metrics(bd: &ContextBreakdown) -> ContextMetrics {
    let input_tokens = bd.input_tokens;
    let system_bytes = bd.system_prompt_bytes;
    let tool_bytes = bd.tool_io_bytes;
    let history_bytes = bd.conversation_history_bytes;
    let reasoning_bytes = bd.reasoning_bytes;
    let context_bytes = bd.context_injection_bytes;
    let total_bytes =
        system_bytes + tool_bytes + history_bytes + reasoning_bytes + context_bytes;
    let context_window = bd.context_window.max(1);
    let utilization_pct =
        ((input_tokens as f64 / context_window as f64) * 100.0).clamp(0.0, 100.0);

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

    if total_bytes == 0 {
        categories[4].percent = utilization_pct;
    } else {
        categories[0].percent = system_bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[1].percent = tool_bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[2].percent = history_bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[3].percent = reasoning_bytes as f64 / total_bytes as f64 * utilization_pct;
        categories[4].percent = context_bytes as f64 / total_bytes as f64 * utilization_pct;
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
