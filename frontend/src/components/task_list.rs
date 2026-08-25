use leptos::prelude::*;
use protocol::{AgentId, ContextBreakdown, CurrentContextUsage, TaskList, TaskStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_SUMMARY_ID: AtomicUsize = AtomicUsize::new(1);

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

struct TaskViewContext {
    breakdown: Option<ContextBreakdown>,
    is_unavailable: bool,
    is_available: bool,
    preferred_view: RwSignal<Option<SummaryView>>,
    panel_id: String,
}

#[component]
pub fn TaskListView(
    agent_id: Signal<Option<AgentId>>,
    task_list: Signal<Option<TaskList>>,
    context_breakdown: Memo<Option<ContextBreakdown>>,
    current_context_usage: Signal<Option<CurrentContextUsage>>,
) -> impl IntoView {
    // `None` is "the user has not chosen", not "Context". The distinction is
    // the whole point: an unchosen view may follow what data exists, a chosen
    // one may not be taken away from the user by arriving or departing data.
    let preferred_view = RwSignal::new(None::<SummaryView>);
    let collapsed = RwSignal::new(false);
    let last_agent_id = RwSignal::new(agent_id.get_untracked());
    let summary_id = NEXT_SUMMARY_ID.fetch_add(1, Ordering::Relaxed);
    let context_panel_id = format!("conversation-summary-{summary_id}-context-panel");
    let tasks_panel_id = format!("conversation-summary-{summary_id}-tasks-panel");

    let has_context = Memo::new(move |_| {
        current_context_usage.get().is_some()
            || context_breakdown
                .get()
                .as_ref()
                .is_some_and(|bd| bd.input_tokens > 0)
    });
    let has_tasks = Memo::new(move |_| {
        task_list
            .get()
            .as_ref()
            .is_some_and(|tl| !tl.tasks.is_empty())
    });

    Effect::new(move |_| {
        let current_agent_id = agent_id.get();
        if current_agent_id != last_agent_id.get_untracked() {
            last_agent_id.set(current_agent_id);
            preferred_view.set(None);
            collapsed.set(false);
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
                let preferred = preferred_view.get();
                let view_mode = match preferred {
                    // A context reader stays a context reader. Occupancy that
                    // stops being reported mid-session is a gap in the data,
                    // not a request to go read the task list — and because the
                    // control that switches back lives *inside* the context
                    // view, flipping away on a data gap stranded the user with
                    // no way back at all.
                    Some(SummaryView::Context) => SummaryView::Context,
                    Some(SummaryView::Tasks) if has_tasks_now => SummaryView::Tasks,
                    Some(SummaryView::Tasks) => SummaryView::Context,
                    // Unchosen: open on whichever view can actually say
                    // something, preferring context.
                    None if has_context_now => SummaryView::Context,
                    None if has_tasks_now => SummaryView::Tasks,
                    None => SummaryView::Context,
                };

                let task_list_now = task_list.get();
                let current_context_usage_now = current_context_usage.get();
                let context_breakdown_now = context_breakdown.get();
                let breakdown = context_breakdown_for_display(
                    current_context_usage_now.as_ref(),
                    context_breakdown_now.as_ref(),
                ).filter(|_| has_context_now);
                let context_is_unavailable = breakdown.is_none();

                view! {
                    <div class="summary-panel">
                        {render_context_view(
                            breakdown.clone(),
                            task_list_now.clone().filter(|tasks| !tasks.tasks.is_empty()),
                            preferred_view,
                            context_panel_id.clone(),
                            tasks_panel_id.clone(),
                            view_mode != SummaryView::Context,
                        )}
                        {task_list_now
                            .filter(|tasks| !tasks.tasks.is_empty())
                            .map(|tasks| {
                                render_task_view(
                                    tasks,
                                    collapsed,
                                    tasks_panel_id.clone(),
                                    view_mode != SummaryView::Tasks,
                                    TaskViewContext {
                                        breakdown,
                                        is_unavailable: context_is_unavailable,
                                        is_available: has_context_now,
                                        preferred_view,
                                        panel_id: context_panel_id.clone(),
                                    },
                                )
                                    .into_any()
                            })
                            .unwrap_or_else(|| {
                                render_empty_task_panel(tasks_panel_id.clone())
                            })}
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

fn render_context_view(
    breakdown: Option<ContextBreakdown>,
    task_list: Option<TaskList>,
    preferred_view: RwSignal<Option<SummaryView>>,
    panel_id: String,
    tasks_panel_id: String,
    hidden: bool,
) -> impl IntoView {
    let metrics = breakdown.as_ref().map(compute_context_metrics);
    let has_detailed_breakdown = breakdown.as_ref().is_some_and(has_detailed_breakdown);
    // Unavailable covers every reason the panel has no figure to draw: the
    // backend reported `Unknown`, or no request has reported occupancy yet.
    // Since the view now stays put when the user pinned it, this panel can be
    // the visible one with nothing to show, and an empty bar with no figure
    // beside it reads as "the context is empty" — the opposite of the truth.
    let context_is_unavailable = metrics.is_none();

    view! {
        <div
            class="summary-context-view"
            id=panel_id
            role="region"
            aria-label="Current context"
            hidden=hidden
        >
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
                    {context_is_unavailable.then(|| {
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
                    class=if context_is_unavailable {
                        "summary-context-bar context-unknown"
                    } else {
                        "summary-context-bar"
                    }
                    data-testid="context-bar"
                    role="progressbar"
                    aria-label="Context utilization"
                    aria-valuemin="0"
                    aria-valuemax="100"
                    aria-valuenow=(!context_is_unavailable).then(|| {
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
                    {metrics.as_ref().map(|m| render_bar_fill(m, has_detailed_breakdown))}
                </div>
                {(metrics.is_some() || task_list.is_some()).then(|| view! {
                    <div class="summary-context-meta">
                        {metrics.as_ref().map(|m| {
                            if has_detailed_breakdown {
                                render_context_legend(&m.categories).into_any()
                            } else {
                                ().into_any()
                            }
                        })}
                        {task_list.map(|tasks| view! {
                            <button
                                type="button"
                                class="context-task-hint"
                                data-summary-action="tasks"
                                aria-controls=tasks_panel_id
                                on:click=move |_| preferred_view.set(Some(SummaryView::Tasks))
                            >
                                {build_task_hint_text(&tasks.tasks)}
                            </button>
                        })}
                    </div>
                })}
            </div>
    }
}

/// Most backends report how full the window is but never what fills it. Drawing
/// only attributed categories left those sessions with an empty track beside a
/// real "40% of context" figure, which reads as an empty context window. Fill
/// the measured occupancy in one neutral colour instead: it is the same
/// measurement the header states, and no colour claims an attribution nobody
/// reported.
fn render_bar_fill(metrics: &ContextMetrics, has_detailed_breakdown: bool) -> AnyView {
    if !has_detailed_breakdown {
        if metrics.utilization_pct <= 0.0 {
            return ().into_any();
        }
        return view! {
            <span
                class="summary-context-segment segment-occupied"
                data-testid="context-occupancy"
                aria-hidden="true"
                title=format!("{:.0}% of the context window in use", metrics.utilization_pct)
                style=format!("width: {:.2}%", metrics.utilization_pct)
            ></span>
        }
        .into_any();
    }

    metrics
        .categories
        .iter()
        .filter(|cat| cat.percent > 0.0)
        .map(|cat| {
            // Explicitly presentational. A progressbar's descendants are
            // presentational per ARIA, so a role/name here would not reach the
            // accessibility tree however it were written. The readable
            // breakdown lives in the sibling legend below; `title` still serves
            // a sighted pointer user.
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
        })
        .collect::<Vec<_>>()
        .into_any()
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
    collapsed: RwSignal<bool>,
    panel_id: String,
    hidden: bool,
    context: TaskViewContext,
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
    let metrics = context.breakdown.as_ref().map(compute_context_metrics);
    view! {
        <div
            class="summary-task-view"
            id=panel_id
            role="region"
            aria-label="Tasks"
            hidden=hidden
        >
            <div class="task-list-header">
                <div class="task-list-title">
                    <span class="task-list-heading">{task_title.clone()}</span>
                    <span class="task-list-progress">
                        {format!("{completed_count}/{total_count} tasks completed")}
                    </span>
                    <button
                        type="button"
                        class="task-list-collapse"
                        aria-label=move || if collapsed.get() {
                            "Expand task list"
                        } else {
                            "Collapse task list"
                        }
                        aria-expanded=move || (!collapsed.get()).to_string()
                        on:click=move |_| collapsed.update(|value| *value = !*value)
                    >
                        <span aria-hidden="true">
                            {move || if collapsed.get() { "▶" } else { "▼" }}
                        </span>
                    </button>
                </div>
            </div>
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
            {context.is_available.then(|| view! {
                <button
                    type="button"
                    class=if context.is_unavailable {
                        "context-mini-bar context-unknown"
                    } else {
                        "context-mini-bar"
                    }
                    data-summary-action="context"
                    aria-label="View current context"
                    aria-controls=context.panel_id
                    on:click=move |_| context.preferred_view.set(Some(SummaryView::Context))
                >
                    {metrics.as_ref().map(|m| {
                        m.categories.iter().filter(|cat| cat.percent > 0.0).map(|cat| {
                            view! {
                                <span
                                    class=format!("summary-context-segment {}", cat.css_class)
                                    aria-hidden="true"
                                    style=format!("width: {:.2}%", cat.percent)
                                ></span>
                            }
                        }).collect::<Vec<_>>()
                    })}
                </button>
            })}
        </div>
    }
}

fn render_empty_task_panel(panel_id: String) -> AnyView {
    view! {
        <div
            class="summary-task-view"
            id=panel_id
            role="region"
            aria-label="Tasks"
            hidden
        ></div>
    }
    .into_any()
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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use protocol::Task;
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");

    fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-task-list")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-task-list");
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
            let context_breakdown = Memo::new(move |_| Some(breakdown.clone()));
            view! {
                <TaskListView
                    agent_id=Signal::derive(|| Some(AgentId("context-test".to_owned())))
                    task_list=Signal::derive(|| None)
                    context_breakdown=context_breakdown
                    current_context_usage=Signal::derive(|| None)
                />
            }
        })
    }

    #[derive(Clone, Copy)]
    struct SummarySignals {
        agent_id: RwSignal<Option<AgentId>>,
        task_list: RwSignal<Option<TaskList>>,
        breakdown: RwSignal<Option<ContextBreakdown>>,
        current_usage: RwSignal<Option<CurrentContextUsage>>,
    }

    fn task_list(title: &str, description: &str, status: TaskStatus) -> TaskList {
        TaskList {
            title: title.to_owned(),
            tasks: vec![Task {
                id: 1,
                description: description.to_owned(),
                status,
            }],
        }
    }

    fn breakdown(input_tokens: u64) -> ContextBreakdown {
        ContextBreakdown {
            system_prompt_bytes: 100,
            tool_io_bytes: 100,
            conversation_history_bytes: 0,
            reasoning_bytes: 0,
            context_injection_bytes: 0,
            input_tokens,
            context_window: 10_000,
        }
    }

    fn mount_summary(
        container: &HtmlElement,
        slots: Rc<RefCell<Option<SummarySignals>>>,
    ) -> impl Sized {
        mount_to(container.clone(), move || {
            let agent_id = RwSignal::new(Some(AgentId("agent-a".to_owned())));
            let task_list = RwSignal::new(Some(task_list(
                "Initial tasks",
                "Initial task",
                TaskStatus::InProgress,
            )));
            let breakdown = RwSignal::new(Some(breakdown(2_000)));
            let current_usage = RwSignal::new(None);
            *slots.borrow_mut() = Some(SummarySignals {
                agent_id,
                task_list,
                breakdown,
                current_usage,
            });
            let context_breakdown = Memo::new(move |_| breakdown.get());
            view! {
                <TaskListView
                    agent_id=agent_id.into()
                    task_list=task_list.into()
                    context_breakdown=context_breakdown
                    current_context_usage=current_usage.into()
                />
            }
        })
    }

    fn button(container: &HtmlElement, selector: &str) -> HtmlElement {
        container
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("missing button: {selector}"))
            .dyn_into::<HtmlElement>()
            .unwrap()
    }

    fn computed_display(element: &web_sys::Element) -> String {
        web_sys::window()
            .unwrap()
            .get_computed_style(element)
            .unwrap()
            .expect("computed style")
            .get_property_value("display")
            .unwrap()
    }

    fn assert_active_panel(panel: &web_sys::Element, name: &str) {
        let rect = panel.get_bounding_client_rect();
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "the active {name} panel must have nonzero geometry"
        );
        assert_eq!(
            computed_display(panel),
            "flex",
            "the active {name} panel must be visibly rendered"
        );
    }

    fn assert_inactive_panel(panel: &web_sys::Element, name: &str) {
        let rect = panel.get_bounding_client_rect();
        assert_eq!(
            rect.width(),
            0.0,
            "the inactive {name} panel must have zero width"
        );
        assert_eq!(
            rect.height(),
            0.0,
            "the inactive {name} panel must have zero height"
        );
        assert_eq!(
            computed_display(panel),
            "none",
            "the inactive {name} panel must generate no layout box"
        );
    }

    fn signals(slots: &Rc<RefCell<Option<SummarySignals>>>) -> SummarySignals {
        slots.borrow().as_ref().copied().expect("summary signals")
    }

    fn panel_is_active(container: &HtmlElement, selector: &str) -> bool {
        !container
            .query_selector(selector)
            .unwrap()
            .expect("summary panel")
            .has_attribute("hidden")
    }

    #[wasm_bindgen_test]
    async fn compact_controls_switch_both_directions() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots);
        next_tick().await;

        assert!(panel_is_active(&container, ".summary-context-view"));
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-task-view"));
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Initial task")
        );

        button(&container, "[data-summary-action='context']").click();
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-context-view"));
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Current context")
        );
    }

    #[wasm_bindgen_test]
    async fn collapse_control_is_separate_from_view_controls() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots);
        next_tick().await;

        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        let collapse = button(&container, ".task-list-collapse");
        assert_eq!(
            collapse.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );

        collapse.click();
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-task-view"));
        assert_eq!(
            button(&container, ".task-list-collapse")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false")
        );

        button(&container, "[data-summary-action='context']").click();
        next_tick().await;
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        assert_eq!(
            button(&container, ".task-list-collapse")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("false"),
            "switching views must not double as the task collapse control"
        );
    }

    #[wasm_bindgen_test]
    async fn task_and_message_updates_preserve_preferred_view() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;

        let signals = signals(&slots);
        signals.task_list.set(Some(task_list(
            "Updated tasks",
            "Reactive task update",
            TaskStatus::Completed,
        )));
        signals.breakdown.set(Some(breakdown(7_000)));
        next_tick().await;

        assert!(panel_is_active(&container, ".summary-task-view"));
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Reactive task update"), "{text}");
        assert!(
            container
                .query_selector(".summary-context-view")
                .unwrap()
                .expect("context panel")
                .has_attribute("hidden"),
            "the unselected context panel must remain visually hidden"
        );
    }

    #[wasm_bindgen_test]
    async fn temporary_task_gap_returns_to_preferred_tasks() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;

        let signals = signals(&slots);
        signals.task_list.set(None);
        next_tick().await;
        assert!(
            panel_is_active(&container, ".summary-context-view"),
            "the available context view must be shown during the task gap"
        );
        assert!(
            container
                .query_selector("[data-summary-action='tasks']")
                .unwrap()
                .is_none(),
            "an unavailable task view must not have a switch control"
        );

        signals.task_list.set(Some(task_list(
            "Returned tasks",
            "Task after gap",
            TaskStatus::InProgress,
        )));
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-task-view"));
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Task after gap")
        );
    }

    /// A context gap must never move a reader who chose the context view.
    ///
    /// The regression this pins: occupancy stopped being reported mid-session,
    /// the panel silently swapped itself to the task list, and because the
    /// control that switches back lives inside the context view, there was no
    /// control left anywhere to undo it.
    #[wasm_bindgen_test]
    async fn a_context_gap_never_moves_a_chosen_context_view() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        button(&container, "[data-summary-action='context']").click();
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-context-view"));

        signals(&slots).breakdown.set(None);
        next_tick().await;

        assert!(
            panel_is_active(&container, ".summary-context-view"),
            "a chosen context view must survive a gap in reported occupancy"
        );
        assert!(
            !panel_is_active(&container, ".summary-task-view"),
            "a data gap must not hand the panel to the task list unasked"
        );
        let context_text = container
            .query_selector(".summary-context-view")
            .unwrap()
            .expect("mounted context panel")
            .text_content()
            .unwrap_or_default();
        assert!(
            context_text.contains("Unavailable"),
            "a context view with no figure must say so, not draw an empty bar; got {context_text:?}"
        );
        assert!(
            container
                .query_selector("[data-summary-action='tasks']")
                .unwrap()
                .is_some(),
            "the chosen view must keep its control back to the task list"
        );
    }

    #[wasm_bindgen_test]
    async fn only_available_view_is_selected_honestly() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;

        signals(&slots).breakdown.set(None);
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-task-view"));
        assert!(
            container
                .query_selector("[data-summary-action='context']")
                .unwrap()
                .is_none(),
            "an unavailable context view must not have a switch control"
        );
        assert!(
            container
                .query_selector(".summary-context-view")
                .unwrap()
                .expect("mounted context panel")
                .has_attribute("hidden"),
            "the unavailable context panel stays mounted without taking space"
        );
    }

    #[wasm_bindgen_test]
    async fn in_place_agent_swap_resets_view_and_collapse() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;
        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        button(&container, ".task-list-collapse").click();
        next_tick().await;

        signals(&slots)
            .agent_id
            .set(Some(AgentId("agent-b".to_owned())));
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-context-view"));

        button(&container, "[data-summary-action='tasks']").click();
        next_tick().await;
        assert_eq!(
            button(&container, ".task-list-collapse")
                .get_attribute("aria-expanded")
                .as_deref(),
            Some("true"),
            "the new agent must not inherit the prior agent's collapsed state"
        );
    }

    #[wasm_bindgen_test]
    async fn compact_controls_replace_the_permanent_tab_strip() {
        ensure_styles_loaded();
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots);
        next_tick().await;

        assert!(
            container
                .query_selector(".summary-view-tabs")
                .unwrap()
                .is_none()
        );
        assert!(container.query_selector("[role='tab']").unwrap().is_none());
        assert!(
            container
                .query_selector("[role='tabpanel']")
                .unwrap()
                .is_none()
        );
        let hint = button(&container, "[data-summary-action='tasks']");
        assert!(hint.get_bounding_client_rect().height() < 30.0);

        hint.click();
        next_tick().await;
        let mini = button(&container, "[data-summary-action='context']");
        assert!(mini.get_bounding_client_rect().height() <= 10.0);
    }

    #[wasm_bindgen_test]
    async fn compact_controls_and_panels_have_stable_linkage() {
        ensure_styles_loaded();
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots);
        next_tick().await;
        let document = web_sys::window().unwrap().document().unwrap();

        let tasks_control = button(&container, "[data-summary-action='tasks']");
        let tasks_panel_id = tasks_control
            .get_attribute("aria-controls")
            .expect("task hint controls a panel");
        let context_panel = container
            .query_selector(".summary-context-view")
            .unwrap()
            .expect("context panel");
        let context_panel_id = context_panel.id();
        assert_active_panel(&context_panel, "context");

        let tasks_panel = document
            .get_element_by_id(&tasks_panel_id)
            .expect("task hint aria-controls resolves");
        assert!(
            tasks_panel.has_attribute("hidden"),
            "the inactive but linked task panel stays in the DOM"
        );
        assert_inactive_panel(&tasks_panel, "task");

        tasks_control.click();
        next_tick().await;
        let context_control = button(&container, "[data-summary-action='context']");
        assert_eq!(
            context_control.get_attribute("aria-controls").as_deref(),
            Some(context_panel_id.as_str())
        );
        let context_panel = document
            .get_element_by_id(&context_panel_id)
            .expect("context panel remains linked");
        let tasks_panel = document
            .get_element_by_id(&tasks_panel_id)
            .expect("task panel remains linked");
        assert_inactive_panel(&context_panel, "context");
        assert_active_panel(&tasks_panel, "task");

        context_control.click();
        next_tick().await;
        let context_panel = document
            .get_element_by_id(&context_panel_id)
            .expect("context panel remains linked");
        let tasks_panel = document
            .get_element_by_id(&tasks_panel_id)
            .expect("task panel remains linked");
        assert_active_panel(&context_panel, "context");
        assert_inactive_panel(&tasks_panel, "task");
    }

    #[wasm_bindgen_test]
    async fn task_hint_reports_progress_and_survives_unknown_context() {
        let container = make_container();
        let slots = Rc::new(RefCell::new(None));
        let _handle = mount_summary(&container, slots.clone());
        next_tick().await;
        let signals = signals(&slots);
        signals.task_list.set(Some(TaskList {
            title: "Five tasks".to_owned(),
            tasks: (0..5)
                .map(|id| Task {
                    id,
                    description: format!("Task {id}"),
                    status: if id == 0 {
                        TaskStatus::InProgress
                    } else {
                        TaskStatus::Pending
                    },
                })
                .collect(),
        }));
        signals.breakdown.set(None);
        signals
            .current_usage
            .set(Some(CurrentContextUsage::Unknown));
        next_tick().await;
        let hint = button(&container, "[data-summary-action='tasks']");
        assert_eq!(
            hint.text_content().as_deref(),
            Some("Task 1 of 5 in progress →")
        );

        hint.click();
        next_tick().await;
        let mini = button(&container, "[data-summary-action='context']");
        assert!(
            mini.get_attribute("class").is_some_and(|classes| classes
                .split_whitespace()
                .any(|class| class == "context-unknown")),
            "unknown context must keep an honest return control"
        );
        mini.click();
        next_tick().await;
        assert!(panel_is_active(&container, ".summary-context-view"));
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

    /// Unattributed occupancy is drawn as one neutral fill, sized to the
    /// measurement, and claims no category legend.
    ///
    /// This replaces `unattributed_context_draws_no_segment_and_no_legend`,
    /// which asserted `segments().is_empty()` for this same input. That
    /// assertion described a bar that rendered *nothing* for a 50%-full window:
    /// the occupancy reached only `aria-valuenow` and a text note, so a sighted
    /// user saw an empty track next to the header's "5.0K / 10.0K tokens
    /// (50.0%)" — the empty-context reading the old test's own comment set out
    /// to prevent. The contract it was reaching for was "no category colour may
    /// claim occupancy nobody attributed", and that is preserved and sharpened
    /// below: the fill must carry the neutral `segment-occupied` class, must be
    /// the only segment, and must be sized to the measured percentage. The
    /// count and width assertions are new; nothing the old test guaranteed has
    /// been dropped except the note, which was removed deliberately because it
    /// reads as a malfunction when a backend simply does not report categories.
    #[wasm_bindgen_test]
    async fn unattributed_context_draws_one_neutral_fill_and_no_legend() {
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

        let fills = container
            .query_selector_all("[data-testid='context-occupancy']")
            .unwrap();
        assert_eq!(
            fills.length(),
            1,
            "unattributed occupancy draws exactly one fill, not one per category"
        );
        let fill = fills
            .item(0)
            .unwrap()
            .dyn_into::<web_sys::Element>()
            .unwrap();
        assert!(
            fill.class_name().contains("segment-occupied"),
            "the fill must be the neutral class, not a category colour, got: {}",
            fill.class_name()
        );
        let width = fill.get_attribute("style").unwrap_or_default();
        assert!(
            width.contains("width: 50.00%"),
            "the fill must be sized to the measured occupancy, got: {width}"
        );
        assert!(
            container
                .query_selector("[data-testid='context-segment']")
                .unwrap()
                .is_none(),
            "no category slice may be drawn when nothing was attributed"
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
