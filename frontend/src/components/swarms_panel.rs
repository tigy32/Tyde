//! The Swarms dock panel: the host's swarms, saved drafts, and explicit access
//! to legacy teams. Selecting a swarm opens its boards as a center tab; a
//! member row opens that member's own conversation.

use leptos::prelude::*;

use protocol::{
    Swarm, SwarmBoard, SwarmCommandPayload, SwarmDraft, SwarmDraftId, SwarmId, SwarmMemberState,
    TeamId,
};

use crate::components::swarm_dialogs::SwarmDraftDialog;
use crate::components::swarm_view::{
    avatar_style, backend_name, board_unread, initials, lifecycle_label, lifecycle_tone,
    member_is_working, member_status_label, member_status_tone, mint_id, open_swarm_member_chat,
    open_swarm_tab, project_name, send_swarm_command, swarm_error_presentation,
};
use crate::components::teams_panel::TeamsPanel;
use crate::state::{AppState, SwarmErrorEntry, TabContent};

/// Which draft dialog is open. A new swarm's draft id is minted when the
/// dialog opens, so the draft the host creates for it is known exactly.
#[derive(Clone, Debug, PartialEq)]
pub enum DraftDialogTarget {
    New(SwarmDraftId),
    Draft(SwarmDraftId),
}

impl DraftDialogTarget {
    pub fn draft_id(&self) -> &SwarmDraftId {
        match self {
            Self::New(id) | Self::Draft(id) => id,
        }
    }
}

#[component]
pub fn SwarmsPanel() -> impl IntoView {
    let state = expect_context::<AppState>();
    let selected_host = state.selected_host_id;
    let swarms_signal = state.swarms;
    let drafts_signal = state.swarm_drafts;
    let errors_signal = state.swarm_errors;
    let teams_signal = state.teams;
    let host_streams = state.host_streams;

    let dialog: RwSignal<Option<(String, DraftDialogTarget)>> = RwSignal::new(None);
    let send_error: RwSignal<Option<String>> = RwSignal::new(None);
    // A launched draft's swarm names it in `source_draft_id`; open its boards
    // when that record arrives.
    let awaiting_launch: RwSignal<Option<(String, SwarmDraftId)>> = RwSignal::new(None);
    let state_sv = StoredValue::new_local(state.clone());
    Effect::new(move |_| {
        let Some((host_id, draft_id)) = awaiting_launch.get() else {
            return;
        };
        let launched = swarms_signal.with(|map| {
            map.get(&host_id).and_then(|swarms| {
                swarms
                    .values()
                    .find(|swarm| swarm.source_draft_id.as_ref() == Some(&draft_id))
                    .map(|swarm| (swarm.id.clone(), swarm.name.clone()))
            })
        });
        if let Some((swarm_id, name)) = launched {
            awaiting_launch.set(None);
            state_sv.with_value(|state| open_swarm_tab(state, host_id, swarm_id, name));
        }
    });

    let swarm_ids: Memo<Vec<(String, SwarmId)>> = Memo::new(move |_| {
        let Some(host_id) = selected_host.get() else {
            return Vec::new();
        };
        swarms_signal.with(|map| {
            let mut swarms: Vec<&Swarm> = map
                .get(&host_id)
                .map(|m| m.values().collect())
                .unwrap_or_default();
            swarms.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then(a.id.0.cmp(&b.id.0))
            });
            swarms
                .into_iter()
                .map(|swarm| (host_id.clone(), swarm.id.clone()))
                .collect()
        })
    });
    let drafts: Memo<Vec<(String, SwarmDraft)>> = Memo::new(move |_| {
        let Some(host_id) = selected_host.get() else {
            return Vec::new();
        };
        drafts_signal.with(|map| {
            let mut drafts: Vec<SwarmDraft> = map
                .get(&host_id)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default();
            drafts.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then(a.id.0.cmp(&b.id.0))
            });
            drafts
                .into_iter()
                .map(|draft| (host_id.clone(), draft))
                .collect()
        })
    });
    // Errors not tied to one swarm (draft generation, migration) surface here.
    let host_errors: Memo<Vec<SwarmErrorEntry>> = Memo::new(move |_| {
        let Some(host_id) = selected_host.get() else {
            return Vec::new();
        };
        errors_signal.with(|errors| {
            errors
                .iter()
                .filter(|entry| {
                    entry.host_id == host_id
                        && entry.error.swarm_id.is_none()
                        && !dialog.with(|open| {
                            open.as_ref().is_some_and(|(open_host, target)| {
                                open_host == &host_id
                                    && entry.error.draft_id.as_ref() == Some(target.draft_id())
                            })
                        })
                })
                .cloned()
                .collect()
        })
    });
    let legacy_team_count = Memo::new(move |_| {
        let Some(host_id) = selected_host.get() else {
            return 0;
        };
        teams_signal.with(|teams| teams.get(&host_id).map(|m| m.len()).unwrap_or(0))
    });

    let on_convert_team = Callback::new(move |(host_id, team_id): (String, TeamId)| {
        send_error.set(None);
        send_swarm_command(
            host_streams,
            &host_id,
            SwarmCommandPayload::PreviewMigration { team_id },
            Some(Callback::new(move |message: String| {
                send_error.set(Some(message))
            })),
        );
    });

    view! {
        <div class="panel swarms-panel">
            <div class="panel-filters swarms-panel-toolbar">
                <button
                    class="swarm-btn swarm-btn-primary"
                    disabled=move || selected_host.get().is_none()
                    on:click=move |_| {
                        if let Some(host_id) = selected_host.get_untracked() {
                            dialog.set(Some((host_id, DraftDialogTarget::New(SwarmDraftId(mint_id())))));
                        }
                    }
                >
                    "+ New swarm"
                </button>
            </div>
            <div class="panel-content swarms-panel-content">
                {move || send_error.get().map(|message| view! {
                    <div class="swarm-banner" data-tone="error" role="alert">
                        <span class="swarm-banner-text">{format!("Could not reach the host: {message}")}</span>
                        <button class="swarm-btn swarm-btn-quiet" on:click=move |_| send_error.set(None)>"Dismiss"</button>
                    </div>
                })}
                <For
                    each=move || host_errors.get()
                    key=|entry| (entry.host_id.clone(), entry.serial)
                    let:entry
                >
                    {
                        let serial = entry.serial;
                        let host_id = entry.host_id.clone();
                        let (tone, role) = swarm_error_presentation(entry.error.code);
                        view! {
                            <div class="swarm-banner" data-tone=tone role=role>
                                <span class="swarm-banner-text">{entry.error.message.clone()}</span>
                                <button
                                    class="swarm-btn swarm-btn-quiet"
                                    on:click=move |_| errors_signal.update(|errors| {
                                        errors.retain(|e| !(e.serial == serial && e.host_id == host_id))
                                    })
                                >
                                    "Dismiss"
                                </button>
                            </div>
                        }
                    }
                </For>

                <section class="swarms-section" aria-label="Swarms">
                    <For
                        each=move || swarm_ids.get()
                        key=|entry| entry.clone()
                        let:entry
                    >
                        <SwarmCard host_id=entry.0 swarm_id=entry.1 />
                    </For>
                    <Show when=move || swarm_ids.with(|ids| ids.is_empty())>
                        <div class="swarm-empty swarm-empty-panel">
                            <p class="swarm-empty-title">"No swarms yet"</p>
                            <p>"A swarm is a group of peer agents sharing one briefing and coordination board. Start one to give several agents the same goal."</p>
                        </div>
                    </Show>
                </section>

                <Show when=move || drafts.with(|d| !d.is_empty())>
                    <section class="swarms-section" aria-label="Drafts">
                        <h3 class="swarms-section-title">"Drafts"</h3>
                        <For
                            each=move || drafts.get()
                            key=|(host_id, draft)| (host_id.clone(), draft.id.clone(), draft.revision)
                            let:entry
                        >
                            {
                                let (host_id, draft) = entry;
                                let draft_id = draft.id.clone();
                                let is_migration = draft.legacy_team_id.is_some();
                                let conflicts = draft.conflicts.len();
                                let name = if draft.name.trim().is_empty() { "Untitled swarm".to_owned() } else { draft.name.clone() };
                                view! {
                                    <button
                                        class="swarm-draft-row"
                                        on:click=move |_| dialog.set(Some((host_id.clone(), DraftDialogTarget::Draft(draft_id.clone()))))
                                    >
                                        <span class="swarm-draft-name">{name}</span>
                                        <span class="swarm-draft-meta">
                                            {if is_migration { "Team conversion" } else { "Draft" }}
                                            {format!(" · {} member{}", draft.members.len(), if draft.members.len() == 1 { "" } else { "s" })}
                                            {(conflicts > 0).then(|| format!(" · {conflicts} conflict{}", if conflicts == 1 { "" } else { "s" }))}
                                        </span>
                                    </button>
                                }
                            }
                        </For>
                    </section>
                </Show>

                <Show when=move || { legacy_team_count.get() > 0 }>
                    <details class="swarms-legacy">
                        <summary class="swarms-section-title">
                            {move || format!("Legacy teams ({})", legacy_team_count.get())}
                        </summary>
                        <p class="swarms-legacy-note">
                            "Teams keep working as before. Converting one prepares a swarm draft you review before anything changes."
                        </p>
                        <TeamsPanel on_convert_team=on_convert_team />
                    </details>
                </Show>
            </div>
            {move || dialog.get().map(|(host_id, target)| {
                let launched_host = host_id.clone();
                view! {
                    <SwarmDraftDialog
                        host_id=host_id
                        target=target
                        on_close=Callback::new(move |_| dialog.set(None))
                        on_launched=Callback::new(move |draft_id: SwarmDraftId| {
                            awaiting_launch.set(Some((launched_host.clone(), draft_id)));
                        })
                    />
                }
            })}
        </div>
    }
}

#[component]
fn SwarmCard(host_id: String, swarm_id: SwarmId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let swarms_signal = state.swarms;
    let projects_signal = state.projects;
    let center_zone = state.center_zone;
    let state_sv = StoredValue::new_local(state.clone());
    let host = StoredValue::new(host_id);
    let sid = StoredValue::new(swarm_id);
    let expanded = RwSignal::new(false);

    let swarm: Memo<Option<Swarm>> = Memo::new(move |_| {
        swarms_signal.with(|map| {
            map.get(&host.get_value())
                .and_then(|m| m.get(&sid.get_value()).cloned())
        })
    });
    let is_open_tab = Memo::new(move |_| {
        center_zone.with(|cz| {
            cz.active_tab().is_some_and(|tab| {
                matches!(&tab.content, TabContent::Swarm { host_id, swarm_id }
                    if *host_id == host.get_value() && *swarm_id == sid.get_value())
            })
        })
    });

    let open = move || {
        if let Some(current) = swarm.get_untracked() {
            state_sv.with_value(|state| {
                open_swarm_tab(state, host.get_value(), sid.get_value(), current.name)
            });
        }
    };

    let summary = move || {
        swarm.get().map(|current| {
            let live = current
                .members
                .iter()
                .filter(|m| m.state != SwarmMemberState::Retired)
                .count();
            let working = current
                .members
                .iter()
                .filter(|m| member_is_working(m))
                .count();
            let failed = current
                .members
                .iter()
                .filter(|m| m.state == SwarmMemberState::Failed)
                .count();
            let project = projects_signal.with(|projects| {
                project_name(projects, &host.get_value(), &current.constraints.project_id)
            });
            let mut text = format!(
                "{live} member{} · {working} working",
                if live == 1 { "" } else { "s" }
            );
            if failed > 0 {
                text.push_str(&format!(" · {failed} failed"));
            }
            (text, project)
        })
    };
    let unread = move |board: SwarmBoard| {
        swarm.with(|s| s.as_ref().map(|s| board_unread(s, board)).unwrap_or(0))
    };
    let lifecycle = move || swarm.with(|s| s.as_ref().map(|s| s.lifecycle));

    view! {
        <div class="swarm-card" class:active=move || is_open_tab.get()>
            <div class="swarm-card-head">
                <button
                    class="swarm-card-open"
                    on:click=move |_| open()
                    title="Open the swarm's boards"
                >
                    <span class="swarm-status-dot" data-tone=move || lifecycle().map(lifecycle_tone).unwrap_or("unknown") aria-hidden="true"></span>
                    <span class="swarm-card-text">
                        <span class="swarm-card-name">{move || swarm.with(|s| s.as_ref().map(|s| s.name.clone()).unwrap_or_default())}</span>
                        <span class="swarm-card-meta">
                            {move || summary().map(|(text, _)| text)}
                        </span>
                        <span class="swarm-card-meta swarm-card-project">
                            {move || summary().map(|(_, project)| project)}
                            " · "
                            {move || lifecycle().map(lifecycle_label).unwrap_or("Unavailable")}
                        </span>
                    </span>
                    <span class="swarm-card-unread">
                        {move || (unread(SwarmBoard::Briefing) > 0).then(|| view! {
                            <span class="swarm-unread-badge" title="Unread on Briefing">{format!("B {}", unread(SwarmBoard::Briefing))}</span>
                        })}
                        {move || (unread(SwarmBoard::Coordination) > 0).then(|| view! {
                            <span class="swarm-unread-badge" title="Unread on Coordination">{format!("C {}", unread(SwarmBoard::Coordination))}</span>
                        })}
                    </span>
                </button>
                <button
                    class="swarm-card-expand"
                    aria-expanded=move || expanded.get().to_string()
                    aria-label=move || if expanded.get() { "Hide members" } else { "Show members" }
                    on:click=move |_| expanded.update(|e| *e = !*e)
                >
                    {move || if expanded.get() { "▾" } else { "▸" }}
                </button>
            </div>
            <Show when=move || expanded.get()>
                <ul class="swarm-card-members">
                    {move || swarm.get().map(|current| {
                        current.members.into_iter().filter(|m| m.state != SwarmMemberState::Retired).map(|member| {
                            let label = member_status_label(&member);
                            let tone = member_status_tone(&member);
                            let name = member.spec.name.clone();
                            let backend = backend_name(member.spec.backend_kind);
                            let row = view! {
                                <span class="swarm-avatar swarm-avatar-xs" style=avatar_style(&member.spec.id.0) aria-hidden="true">{initials(&name)}</span>
                                <span class="swarm-card-member-name">{name.clone()}</span>
                                <span class="swarm-card-member-backend">{backend}</span>
                                <span class="swarm-card-member-status" data-tone=tone>{label}</span>
                            };
                            if member.agent_id.is_some() {
                                view! {
                                    <li>
                                        <button
                                            class="swarm-card-member"
                                            title=format!("Open {name}'s conversation")
                                            on:click=move |_| state_sv.with_value(|state| open_swarm_member_chat(state, host.get_value(), &member))
                                        >
                                            {row}
                                        </button>
                                    </li>
                                }.into_any()
                            } else {
                                view! {
                                    <li class="swarm-card-member swarm-card-member-static" title="No conversation yet">{row}</li>
                                }.into_any()
                            }
                        }).collect_view()
                    })}
                </ul>
            </Show>
        </div>
    }
}
