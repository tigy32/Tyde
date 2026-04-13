use leptos::prelude::*;
use protocol::{TaskList, TaskStatus};

#[component]
pub fn TaskListView(task_list: TaskList) -> impl IntoView {
    let title = task_list.title;
    let tasks = task_list.tasks;

    view! {
        <div class="task-list-card">
            <div class="task-list-title">{title}</div>
            <div class="task-list-items">
                {tasks.into_iter().map(|task| {
                    let (icon, status_class) = match task.status {
                        TaskStatus::Pending => ("○", "task-pending"),
                        TaskStatus::InProgress => ("◐", "task-in-progress"),
                        TaskStatus::Completed => ("●", "task-completed"),
                        TaskStatus::Failed => ("✕", "task-failed"),
                    };
                    view! {
                        <div class={format!("task-item {status_class}")}>
                            <span class="task-icon">{icon}</span>
                            <span class="task-description">{task.description}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }
}
