use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat;
use crate::send;
use crate::state::{AppState, DockVisibility, RightTab, TabContent, root_display_name};

use protocol::{
    ProjectFileKind, ProjectId, ProjectPath, WorkflowId, WorkflowInputSpec, WorkflowSourceScope,
};

#[derive(Clone, Debug)]
struct CommandEntry {
    name: &'static str,
    shortcut: Option<&'static str>,
    id: &'static str,
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        name: "New Chat",
        shortcut: Some("Ctrl+N"),
        id: "new_chat",
    },
    CommandEntry {
        name: "Toggle Left Panel",
        shortcut: None,
        id: "toggle_left",
    },
    CommandEntry {
        name: "Toggle Right Panel",
        shortcut: None,
        id: "toggle_right",
    },
    CommandEntry {
        name: "Open Workflows",
        shortcut: None,
        id: "open_workflows",
    },
    CommandEntry {
        name: "Toggle Bottom Panel",
        shortcut: None,
        id: "toggle_bottom",
    },
    CommandEntry {
        name: "Go to Home",
        shortcut: None,
        id: "go_home",
    },
    CommandEntry {
        name: "Go to Chat",
        shortcut: None,
        id: "go_chat",
    },
    CommandEntry {
        name: "Open Settings",
        shortcut: Some("Ctrl+,"),
        id: "open_settings",
    },
    CommandEntry {
        name: "Send Feedback",
        shortcut: None,
        id: "send_feedback",
    },
];

#[derive(Clone, Debug, PartialEq)]
enum PaletteResult {
    File {
        name: String,
        path: ProjectPath,
        display_path: String,
        root_label: String,
    },
    Command {
        entry_index: usize,
    },
    WorkflowRun {
        host_id: String,
        workflow_id: WorkflowId,
        project_id: Option<ProjectId>,
        name: String,
        inputs: Vec<WorkflowInputSpec>,
    },
}

fn fuzzy_score(query: &str, target: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower = query.to_lowercase();
    let target_lower = target.to_lowercase();

    if target_lower.starts_with(&query_lower) {
        return Some(100);
    }

    let words: Vec<&str> = target_lower.split(['/', '.', '_', '-', ' ']).collect();
    for word in &words {
        if word.starts_with(&query_lower) {
            return Some(75);
        }
    }

    if target_lower.contains(&query_lower) {
        return Some(50);
    }

    None
}

fn toggle_dock(signal: RwSignal<DockVisibility>) {
    signal.update(|v: &mut DockVisibility| {
        *v = match v {
            DockVisibility::Visible => DockVisibility::Hidden,
            DockVisibility::Hidden => DockVisibility::Visible,
        };
    });
}

fn execute_command(state: &AppState, id: &str) {
    match id {
        "new_chat" => {
            begin_new_chat(state, None);
        }
        "toggle_left" => toggle_dock(state.left_dock),
        "toggle_right" => toggle_dock(state.right_dock),
        "toggle_bottom" => toggle_dock(state.bottom_dock),
        "open_workflows" => {
            state.right_dock.set(DockVisibility::Visible);
            state.right_tab.set(RightTab::Workflows);
        }
        "go_home" => state.open_tab(TabContent::Home, "Home".to_string(), false),
        "go_chat" => {
            // Activate the most recent chat tab, or open a new chat
            let found = state.center_zone.with_untracked(|cz| {
                cz.tabs
                    .iter()
                    .rev()
                    .find(|t| matches!(t.content, TabContent::Chat { .. }))
                    .map(|t| t.id)
            });
            if let Some(id) = found {
                state.activate_tab(id);
            } else {
                begin_new_chat(state, None);
            }
        }
        "open_settings" => {
            state.command_palette_open.set(false);
            state.settings_open.set(true);
        }
        "send_feedback" => {
            state.command_palette_open.set(false);
            state.feedback_open.set(true);
        }
        _ => log::warn!("unknown command: {id}"),
    }
}

/// Perform the select action for a given result index.
/// Uses expect_context to avoid capturing the non-Copy AppState.
fn do_select(results: Memo<Vec<PaletteResult>>, idx: usize) {
    let state = expect_context::<AppState>();
    let items = results.get();
    if idx >= items.len() {
        return;
    }
    match &items[idx] {
        PaletteResult::File { path, .. } => {
            crate::actions::open_file(&state, path.clone());
        }
        PaletteResult::Command { entry_index } => {
            execute_command(&state, COMMANDS[*entry_index].id);
        }
        PaletteResult::WorkflowRun {
            host_id,
            workflow_id,
            project_id,
            name,
            inputs,
        } => {
            // A workflow that declares inputs must collect them first: route it
            // through the same global inputs modal the panel uses, instead of
            // firing the trigger with an empty input map. Inputless workflows
            // run in one step.
            if inputs.is_empty() {
                let host_stream = state
                    .host_streams
                    .with_untracked(|streams| streams.get(host_id).cloned());
                if let Some(host_stream) = host_stream {
                    let host_id = host_id.clone();
                    let workflow_id = workflow_id.clone();
                    let project_id = project_id.clone();
                    spawn_local(async move {
                        if let Err(error) = send::trigger_workflow(
                            &host_id,
                            host_stream,
                            workflow_id,
                            project_id,
                            std::collections::HashMap::new(),
                        )
                        .await
                        {
                            log::error!("failed to trigger workflow from palette: {error}");
                        }
                    });
                }
            } else {
                state
                    .workflow_run_request
                    .set(Some(crate::state::WorkflowRunRequest {
                        host_id: host_id.clone(),
                        workflow_id: workflow_id.clone(),
                        project_id: project_id.clone(),
                        name: name.clone(),
                        inputs: inputs.clone(),
                    }));
            }
        }
    }
    state.command_palette_open.set(false);
}

#[component]
pub fn CommandPalette() -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = state.command_palette_open;
    let file_tree = state.file_tree;
    let active_project = state.active_project;
    let workflow_state = state.clone();

    let input = RwSignal::new(String::new());
    let selected_index = RwSignal::new(0usize);

    let is_command_mode = Memo::new(move |_| input.get().starts_with('>'));

    let results: Memo<Vec<PaletteResult>> = Memo::new(move |_| {
        let query_raw = input.get();
        let command_mode = query_raw.starts_with('>');

        if command_mode {
            let query = query_raw[1..].trim();
            let mut scored: Vec<(PaletteResult, u32)> = COMMANDS
                .iter()
                .enumerate()
                .filter_map(|(i, cmd)| {
                    if query.is_empty() {
                        Some((PaletteResult::Command { entry_index: i }, 0))
                    } else {
                        fuzzy_score(query, cmd.name)
                            .map(|s| (PaletteResult::Command { entry_index: i }, s))
                    }
                })
                .collect();
            let active_project_ref = workflow_state.active_project.get();
            let active_host_id = active_project_ref
                .as_ref()
                .map(|active| active.host_id.clone())
                .or_else(|| workflow_state.selected_host_id.get());
            if let Some(host_id) = active_host_id {
                let active_project_id = active_project_ref
                    .as_ref()
                    .map(|active| active.project_id.clone());
                let summaries = workflow_state
                    .workflow_summaries
                    .with(|map| map.get(&host_id).cloned().unwrap_or_default());
                // Run only the workflows effective for the active context: a
                // project workflow shadows the same-id global in its project, so
                // the palette never lists or triggers the wrong definition.
                let workflows = crate::components::workflows_panel::effective_summaries(
                    &summaries,
                    active_project_id.as_ref(),
                );
                for workflow in workflows {
                    let label = format!("Run Workflow {}", workflow.name);
                    let Some(score) = (if query.is_empty() {
                        Some(0)
                    } else {
                        fuzzy_score(query, &label).or_else(|| fuzzy_score(query, &workflow.id.0))
                    }) else {
                        continue;
                    };
                    let project_id = match &workflow.source.scope {
                        WorkflowSourceScope::Project { project_id, .. } => Some(project_id.clone()),
                        WorkflowSourceScope::Global => active_project_id.clone(),
                    };
                    scored.push((
                        PaletteResult::WorkflowRun {
                            host_id: host_id.clone(),
                            workflow_id: workflow.id,
                            project_id,
                            name: workflow.name,
                            inputs: workflow.inputs,
                        },
                        score,
                    ));
                }
            }
            scored.sort_by_key(|score| std::cmp::Reverse(score.1));
            scored
                .into_iter()
                .take(10)
                .map(|(result, _)| result)
                .collect()
        } else {
            let query = query_raw.trim();
            let tree = file_tree.get();
            let Some(active_project) = active_project.get() else {
                return Vec::new();
            };
            let mut scored: Vec<(String, ProjectPath, String, String, u32)> = Vec::new();
            if let Some(root_listings) = tree.get(&active_project.project_id) {
                for root_listing in root_listings {
                    let root_label = root_display_name(&root_listing.root);
                    for entry in &root_listing.entries {
                        if entry.kind != ProjectFileKind::File {
                            continue;
                        }
                        let path = &entry.relative_path;
                        let file_name = path.rsplit('/').next().unwrap_or(path);
                        let score = if query.is_empty() {
                            Some(0)
                        } else {
                            fuzzy_score(query, file_name).or_else(|| fuzzy_score(query, path))
                        };
                        if let Some(s) = score {
                            scored.push((
                                file_name.to_owned(),
                                ProjectPath {
                                    root: root_listing.root.clone(),
                                    relative_path: path.clone(),
                                },
                                path.clone(),
                                root_label.clone(),
                                s,
                            ));
                        }
                    }
                }
            }
            scored.sort_by(|a, b| {
                b.4.cmp(&a.4)
                    .then_with(|| a.3.cmp(&b.3))
                    .then_with(|| a.2.cmp(&b.2))
            });
            scored
                .into_iter()
                .take(10)
                .map(
                    |(name, path, display_path, root_label, _)| PaletteResult::File {
                        name,
                        path,
                        display_path,
                        root_label,
                    },
                )
                .collect()
        }
    });

    let result_count = Memo::new(move |_| results.get().len());

    let on_keydown = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Escape" => {
            ev.prevent_default();
            open.set(false);
        }
        "ArrowDown" => {
            ev.prevent_default();
            let count = result_count.get();
            if count > 0 {
                selected_index.update(|i: &mut usize| *i = (*i + 1) % count);
            }
        }
        "ArrowUp" => {
            ev.prevent_default();
            let count = result_count.get();
            if count > 0 {
                selected_index.update(|i: &mut usize| {
                    *i = if *i == 0 { count - 1 } else { *i - 1 };
                });
            }
        }
        "Enter" => {
            ev.prevent_default();
            do_select(results, selected_index.get_untracked());
        }
        _ => {}
    };

    let on_input = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        input.set(el.value());
        selected_index.set(0);
    };

    let on_backdrop_click = move |_| {
        open.set(false);
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if open.get() {
            input.set(String::new());
            selected_index.set(0);
            if let Some(el) = input_ref.get() {
                let _ = el.focus();
            }
        }
    });

    let mode_label = move || {
        if is_command_mode.get() {
            "Commands"
        } else {
            "Files"
        }
    };

    view! {
        <Show when=move || open.get()>
            <div class="cp-overlay" on:click=on_backdrop_click>
                <div class="cp-modal" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                    <div class="cp-header">
                        <input
                            node_ref=input_ref
                            class="cp-input"
                            type="text"
                            placeholder="Search files... (type > for commands)"
                            on:input=on_input
                            on:keydown=on_keydown
                            prop:value=move || input.get()
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                        <span class="cp-mode-badge">{mode_label}</span>
                    </div>
                    <div class="cp-results">
                        {move || {
                            results.get().into_iter().enumerate().map(|(idx, result)| {
                                let is_selected = move || selected_index.get() == idx;
                                let on_click = move |_| {
                                    selected_index.set(idx);
                                    do_select(results, idx);
                                };
                                match result {
                                    PaletteResult::File {
                                        name,
                                        display_path,
                                        root_label,
                                        ..
                                    } => {
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-file-name">{name}</span>
                                                <span class="cp-root-label">{root_label}</span>
                                                <span class="cp-file-path">{display_path}</span>
                                            </div>
                                        }.into_any()
                                    }
                                    PaletteResult::Command { entry_index } => {
                                        let cmd = &COMMANDS[entry_index];
                                        let name = cmd.name;
                                        let shortcut = cmd.shortcut;
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-cmd-name">{name}</span>
                                                {shortcut.map(|s| view! {
                                                    <kbd class="cp-cmd-shortcut">{s}</kbd>
                                                })}
                                            </div>
                                        }.into_any()
                                    }
                                    PaletteResult::WorkflowRun { name, .. } => {
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-cmd-name">{format!("Run Workflow: {name}")}</span>
                                                <span class="cp-file-path">"Workflows"</span>
                                            </div>
                                        }.into_any()
                                    }
                                }
                            }).collect_view()
                        }}
                        <Show when=move || results.get().is_empty()>
                            <div class="cp-empty">"No results"</div>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}
