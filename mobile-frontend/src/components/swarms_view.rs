use leptos::prelude::*;
use protocol::{
    Swarm, SwarmAuthor, SwarmBoard, SwarmBoardRead, SwarmBodySegment, SwarmCommandPayload,
    SwarmHumanPost, SwarmId, SwarmLifecycle, SwarmMemberState, SwarmPostId, SwarmPublicationId,
    SwarmThreadId, SwarmThreadRead,
};
use wasm_bindgen_futures::spawn_local;

use crate::components::teams_view::TeamsView;
use crate::state::{AppState, LocalHostId, SwarmComposerDraft};

fn lifecycle_label(lifecycle: SwarmLifecycle) -> &'static str {
    match lifecycle {
        SwarmLifecycle::Running => "Ready",
        SwarmLifecycle::Pausing => "Pausing",
        SwarmLifecycle::Paused => "Paused",
        SwarmLifecycle::AttentionRequired => "Needs attention",
        SwarmLifecycle::Transitioning => "Updating agents",
    }
}

fn member_label(member: &protocol::SwarmMember) -> &'static str {
    match member.state {
        SwarmMemberState::Proposed => "Ready to chat",
        SwarmMemberState::Reserved => "Starting",
        SwarmMemberState::Dormant => "Session saved",
        SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved => "Retiring",
        SwarmMemberState::Retired => "Retired",
        SwarmMemberState::Failed => "Failed",
        SwarmMemberState::Live => match member.runtime_status {
            Some(protocol::AgentControlStatus::Thinking) => "Working",
            Some(protocol::AgentControlStatus::AwaitingUser) => "Needs your answer",
            Some(protocol::AgentControlStatus::Idle) => "Idle",
            Some(protocol::AgentControlStatus::Failed) => "Failed",
            None => "Status unavailable",
        },
    }
}

fn send(
    state: AppState,
    host: LocalHostId,
    command: SwarmCommandPayload,
    error: RwSignal<Option<String>>,
) {
    spawn_local(async move {
        if let Err(message) = crate::actions::swarm_command(&state, &host, command).await {
            error.set(Some(message));
        }
    });
}

#[component]
pub fn SwarmsView(selected: RwSignal<Option<(LocalHostId, SwarmId)>>) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    view! {
        <div class="mobile-swarms" data-mobile-test="swarms-view">
            {move || state.active_local_host_id.get().map(|host| view! { <SwarmErrors host=host swarm_id=None /> })}
            {move || match selected.get() {
                Some((host, id)) if state.active_local_host_id.get().as_ref() == Some(&host) => view! {
                    <SwarmConversation host=host swarm_id=id on_back=Callback::new(move |_| selected.set(None)) />
                }.into_any(),
                _ => view! {
                    <div class="mobile-swarm-intro">
                        <h2>"Swarms"</h2>
                        <p>"One conversation. Agents working together."</p>
                    </div>
                    {move || {
                        let Some(host) = state.active_local_host_id.get() else {
                            return view! { <p class="mobile-swarm-muted">"Choose a host to see its swarms."</p> }.into_any();
                        };
                        let swarms = state.swarms_by_host.with(|m| m.get(&host).cloned()).unwrap_or_default();
                        let mut ids = swarms.values().map(|s| (s.id.clone(), s.name.clone())).collect::<Vec<_>>();
                        ids.sort_by(|a, b| a.1.cmp(&b.1));
                        if ids.is_empty() {
                            return view! { <div class="mobile-swarm-empty"><h3>"Your shared conversations live here"</h3><p>"Create a swarm on desktop, then talk to it here in Briefing and follow its Coordination board."</p></div> }.into_any();
                        }
                        ids.into_iter().map(|(id, _)| {
                            let host = host.clone();
                            view! { <SwarmCard host=host swarm_id=id on_open=Callback::new(move |value| selected.set(Some(value))) /> }
                        }).collect_view().into_any()
                    }}
                    <Show when=move || state.active_local_host_id.get().is_some_and(|host| state.teams_by_host.with(|m| m.get(&host).is_some_and(|teams| teams.keys().any(|id| !state.swarms_by_host.with(|m| m.get(&host).is_some_and(|swarms| swarms.values().any(|s| s.legacy_team_id.as_ref() == Some(id))))))))>
                        <details class="mobile-swarm-legacy"><summary>"Legacy teams"</summary><p class="mobile-swarm-muted">"Old team sessions stay separate. Convert them on desktop to use shared boards."</p><TeamsView /></details>
                    </Show>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn SwarmCard(
    host: LocalHostId,
    swarm_id: SwarmId,
    on_open: Callback<(LocalHostId, SwarmId)>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let target = StoredValue::new((host, swarm_id));
    let swarm = Memo::new(move |_| {
        let (host, id) = target.get_value();
        state
            .swarms_by_host
            .with(|m| m.get(&host).and_then(|s| s.get(&id)).cloned())
    });
    view! {
        <button type="button" class="mobile-swarm-card" data-mobile-test="swarm-open" on:click=move |_| on_open.run(target.get_value())>
            <span class="mobile-swarm-card-heading"><strong>{move || swarm.get().map(|s| s.name)}</strong><span class="mobile-swarm-status">{move || swarm.get().map(|s| lifecycle_label(s.lifecycle))}</span></span>
            <span class="mobile-swarm-muted">{move || swarm.get().map(|s| format!("{} agents · Briefing & Coordination", s.members.iter().filter(|m| m.state != SwarmMemberState::Retired).count()))}</span>
            <span class="mobile-swarm-card-foot">{move || swarm.get().map(|s| {
                let unread: u64 = s.board_positions.iter().map(|p| p.unread_count).sum();
                if unread == 0 { "Open conversation →".to_owned() } else { format!("{unread} unread · Open conversation →") }
            })}</span>
        </button>
    }
}

#[component]
fn SwarmConversation(host: LocalHostId, swarm_id: SwarmId, on_back: Callback<()>) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let target = StoredValue::new((host, swarm_id));
    let board = RwSignal::new(SwarmBoard::Briefing);
    let thread = RwSignal::new(None::<SwarmThreadId>);
    let linked_post = RwSignal::new(None::<SwarmPostId>);
    let error = RwSignal::new(None::<String>);
    let swarm = Memo::new(move |_| {
        let (host, id) = target.get_value();
        state
            .swarms_by_host
            .with(|m| m.get(&host).and_then(|s| s.get(&id)).cloned())
    });
    let member_state = state.clone();
    let command_state = state.clone();
    let command = Callback::new(move |command| {
        let (host, _) = target.get_value();
        send(command_state.clone(), host, command, error);
    });
    let compose_open = RwSignal::new(false);
    let connection = Memo::new(move |_| {
        let (host, _) = target.get_value();
        let connected = state
            .connection_statuses
            .with(|m| m.get(&host) == Some(&crate::state::ConnectionStatus::Connected));
        let bootstrapped = state
            .bootstrapped_host_streams
            .with(|m| m.get(&host).cloned());
        let current = state.host_streams.with(|m| m.get(&host).cloned());
        if connected && bootstrapped == current {
            bootstrapped
        } else {
            None
        }
    });
    Effect::new(move |_| {
        let (_, id) = target.get_value();
        let current = board.get();
        let selected_thread = thread.get();
        if connection.get().is_none() {
            return;
        }
        match selected_thread {
            Some(thread_id) => command.run(SwarmCommandPayload::ReadThread {
                swarm_id: id,
                query: SwarmThreadRead {
                    thread_id,
                    after_cursor: None,
                    limit: None,
                },
            }),
            None => command.run(SwarmCommandPayload::ReadBoard {
                swarm_id: id,
                query: SwarmBoardRead {
                    view: protocol::SwarmBoardView::Threads,
                    board: current,
                    after_cursor: None,
                    limit: None,
                },
            }),
        }
    });
    Effect::new(move |_| {
        let Some(post_id) = linked_post.get() else {
            return;
        };
        let (host, id) = target.get_value();
        let post = state
            .swarm_posts
            .with(|m| m.get(&(host, id)).and_then(|p| p.get(&post_id)).cloned());
        if let Some(post) = post {
            board.set(post.board);
            thread.set(Some(post.thread_id));
            linked_post.set(None);
        }
    });
    let posts = Memo::new(move |_| {
        let (host, id) = target.get_value();
        let current = board.get();
        let selected_thread = thread.get();
        let mut posts = state.swarm_posts.with(|m| {
            m.get(&(host, id))
                .map(|p| {
                    p.values()
                        .filter(|post| {
                            post.board == current
                                && selected_thread
                                    .as_ref()
                                    .map_or(post.id.0 == post.thread_id.0, |t| &post.thread_id == t)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        if selected_thread.is_some() {
            posts.sort_by_key(|post| post.cursor);
        } else {
            posts.sort_by_key(|post| std::cmp::Reverse((post.created_at_ms, post.cursor)));
        }
        posts
    });
    let page = Memo::new(move |_| {
        let (host, id) = target.get_value();
        match thread.get() {
            Some(t) => state.swarm_thread_pages.with(|m| {
                m.get(&(host, id, t))
                    .map(|p| (p.next_cursor.clone(), p.has_more))
            }),
            None => state.swarm_board_pages.with(|m| {
                m.get(&(host, id))
                    .and_then(|pages| {
                        pages.iter().find(|p| {
                            p.next_cursor.target
                                == protocol::SwarmCursorTarget::BoardThreads { board: board.get() }
                        })
                    })
                    .map(|p| (p.next_cursor.clone(), p.has_more))
            }),
        }
    });
    let open_link = Callback::new(move |post_id: SwarmPostId| {
        linked_post.set(Some(post_id.clone()));
        command.run(SwarmCommandPayload::ReadPost {
            swarm_id: target.get_value().1,
            post_id,
        });
    });
    view! {
        <section class="mobile-swarm-conversation" data-mobile-test="swarm-conversation">
            <header class="mobile-swarm-header"><button class="mobile-swarm-back mobile-swarm-back-icon" type="button" aria-label="All swarms" title="All swarms" on:click=move |_| on_back.run(())>"←"</button><div><h2>{move || swarm.get().map(|s| s.name)}</h2><span class="mobile-swarm-muted">{move || swarm.get().map(|s| lifecycle_label(s.lifecycle))}</span></div>
                <button type="button" class="mobile-swarm-control" data-mobile-test="swarm-pause-resume" disabled=move || swarm.get().is_none_or(|s| s.lifecycle == SwarmLifecycle::Pausing) on:click=move |_| {
                    if let Some(s) = swarm.get_untracked() {
                        command.run(if matches!(s.lifecycle, SwarmLifecycle::Paused | SwarmLifecycle::AttentionRequired) { SwarmCommandPayload::Resume { swarm_id: s.id } } else { SwarmCommandPayload::Pause { swarm_id: s.id } });
                    }
                }>{move || if swarm.get().is_some_and(|s| matches!(s.lifecycle, SwarmLifecycle::Paused | SwarmLifecycle::AttentionRequired)) { "Resume" } else { "Pause" }}</button>
            </header>


            {move || error.get().map(|message| view! { <div role="alert" class="mobile-swarm-error">{message}<button type="button" on:click=move |_| error.set(None)>"Dismiss"</button></div> })}
            {move || swarm.get().and_then(|s| s.error).map(|message| view! { <p role="alert" class="mobile-swarm-error">{message}</p> })}
            <SwarmErrors host=target.get_value().0 swarm_id=Some(target.get_value().1) />
            <div class="mobile-swarm-navigation">
            <nav class="mobile-swarm-boards" role="tablist" aria-label="Swarm conversations">
                <button type="button" role="tab" aria-selected=move || (board.get() == SwarmBoard::Briefing).to_string() on:click=move |_| { board.set(SwarmBoard::Briefing); thread.set(None); compose_open.set(false); } data-mobile-test="swarm-briefing">"Requests"</button>
                <button type="button" role="tab" aria-selected=move || (board.get() == SwarmBoard::Coordination).to_string() on:click=move |_| { board.set(SwarmBoard::Coordination); thread.set(None); compose_open.set(false); } data-mobile-test="swarm-coordination">"Coordination"</button>
            </nav>
            <details class="mobile-swarm-members"><summary>{move || swarm.get().map(|s| format!("{} agents", s.members.iter().filter(|m| m.state != SwarmMemberState::Retired).count()))}</summary>
                {move || swarm.get().map(|s| s.members.into_iter().map(|member| match member.agent_id.clone() {
                    Some(agent_id) => {
                        let open_state = member_state.clone();
                        let open = move |_| {
                            let (host, _) = target.get_value();
                            open_state.active_agent.set(Some(crate::state::ActiveAgentRef { local_host_id: host, agent_id: agent_id.clone() }));
                            open_state.viewing_chat.set(true);
                        };
                        view! { <button type="button" class="mobile-swarm-member" data-mobile-test="swarm-member-open" on:click=open><strong>{member.spec.name.clone()}</strong><span>{member_label(&member)}" · Open →"</span></button> }.into_any()
                    }
                    None => view! { <div class="mobile-swarm-member"><strong>{member.spec.name.clone()}</strong><span>{member_label(&member)}</span></div> }.into_any(),
                }).collect_view())}
            </details>
            </div>
            <p class="mobile-swarm-board-hint">{move || if board.get() == SwarmBoard::Briefing { "One request, one thread. Newest requests first." } else { "Threads agents opened to coordinate on your requests." }}</p>
            <Show when=move || thread.get().is_some()><button class="mobile-swarm-back" type="button" data-mobile-test="swarm-back-to-board" on:click=move |_| { thread.set(None); compose_open.set(false); }>"← Back to threads"</button></Show>
            <Show when=move || thread.get().is_none()>
                <div class="mobile-swarm-inbox-heading"><span class="mobile-swarm-muted">{move || if board.get() == SwarmBoard::Briefing { "Your requests" } else { "Coordination threads" }}</span><Show when=move || board.get() == SwarmBoard::Briefing><button type="button" class="mobile-swarm-control" data-mobile-test="swarm-new-thread" on:click=move |_| compose_open.set(true)>"+ New request"</button></Show></div>
            </Show>
            <div class="mobile-swarm-posts" role="tabpanel">
                <Show when=move || posts.with(|p| p.is_empty())><div class="mobile-swarm-empty"><h3>{move || if page.get().is_none() { "Loading conversation…" } else if board.get() == SwarmBoard::Briefing { "What would you like to work on?" } else { "Room to coordinate" }}</h3><p>{move || if page.get().is_none() { "Waiting for the host." } else { "Start a new thread when you are ready." }}</p></div></Show>
                <For each={move || posts.get().into_iter().map(|p| p.id).collect::<Vec<_>>()} key={|id| id.clone()} let:post_id>
                    {move || if thread.get().is_some() {
                        view! { <SwarmPostCard target=target post_id=post_id.clone() swarm=swarm on_reply=Callback::new(move |value| thread.set(Some(value))) on_link=open_link /> }.into_any()
                    } else {
                        view! { <SwarmThreadCard target=target post_id=post_id.clone() swarm=swarm on_open=Callback::new(move |value| { compose_open.set(false); thread.set(Some(value)); }) /> }.into_any()
                    }}
                </For>
                <Show when=move || page.get().is_some_and(|(_, more)| more)><button type="button" class="mobile-swarm-control" data-mobile-test="swarm-more" on:click=move |_| {
                    if let Some((cursor, _)) = page.get_untracked() {
                        let id = target.get_value().1;
                        command.run(match thread.get_untracked() {
                            Some(thread_id) => SwarmCommandPayload::ReadThread { swarm_id: id, query: SwarmThreadRead { thread_id, after_cursor: Some(cursor), limit: None } },
                            None => SwarmCommandPayload::ReadBoard { swarm_id: id, query: SwarmBoardRead {
                view: protocol::SwarmBoardView::Threads, board: board.get_untracked(), after_cursor: Some(cursor), limit: None } },
                        });
                    }
                }>"Load more"</button></Show>
            </div>
            <div class="mobile-swarm-compose" hidden=move || thread.get().is_none() && !(compose_open.get() && board.get() == SwarmBoard::Briefing)>
                <Show when=move || thread.get().is_none()><button type="button" class="mobile-swarm-back" on:click=move |_| compose_open.set(false)>"Close composer · keep draft"</button></Show>
                <SwarmComposer target=target board=board thread=thread swarm=swarm on_published=Callback::new(move |post: protocol::SwarmPost| {
                    if thread.get_untracked().is_none() && board.get_untracked() == post.board && post.id.0 == post.thread_id.0
                        && state.swarm_composer_drafts.with_untracked(|drafts| drafts.iter().any(|draft| (draft.host.clone(), draft.swarm_id.clone()) == target.get_value() && draft.board == post.board && draft.thread_id.is_none() && draft.text.is_empty() && draft.mentions.is_empty() && !draft.pending))
                    { compose_open.set(false); }
                }) />
            </div>
        </section>
    }
}

#[component]
fn SwarmErrors(host: LocalHostId, swarm_id: Option<SwarmId>) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let target = StoredValue::new((host, swarm_id));
    move || {
        let (host, id) = target.get_value();
        state.swarm_errors_by_host.with(|m| m.get(&host).cloned()).unwrap_or_default().into_iter().filter(|e| e.swarm_id == id).map(|error| {
            let error = StoredValue::new(error);
            let host = host.clone();
            view! { <div role="alert" class="mobile-swarm-error"><span>{error.get_value().message}</span><button type="button" class="mobile-swarm-link" on:click=move |_| state.swarm_errors_by_host.update(|m| {
                if let Some(errors) = m.get_mut(&host) { errors.retain(|e| e != &error.get_value()); }
            })>"Dismiss"</button></div> }
        }).collect_view()
    }
}

struct ThreadHeading {
    parent: Option<(SwarmThreadId, String)>,
    title: Option<String>,
    naming_error: Option<String>,
    summary: String,
    children: Vec<(SwarmThreadId, String)>,
}

fn thread_heading(swarm: &Swarm, thread_id: &SwarmThreadId) -> Option<ThreadHeading> {
    let thread = swarm
        .threads
        .iter()
        .find(|thread| &thread.thread_id == thread_id)?;
    let title_of = |thread: &protocol::SwarmThread| {
        thread.title.clone().unwrap_or_else(|| "Naming…".to_owned())
    };
    let parent = thread.parent_thread_id.as_ref().and_then(|parent_id| {
        swarm
            .threads
            .iter()
            .find(|parent| &parent.thread_id == parent_id)
            .map(|parent| (parent_id.clone(), title_of(parent)))
    });
    let mut children = swarm
        .threads
        .iter()
        .filter(|child| child.parent_thread_id.as_ref() == Some(thread_id))
        .collect::<Vec<_>>();
    children.sort_by_key(|child| child.creation_cursor);
    Some(ThreadHeading {
        parent,
        title: thread.title.clone(),
        naming_error: thread.naming_error.clone(),
        summary: thread.summary.clone(),
        children: children
            .into_iter()
            .map(|child| (child.thread_id.clone(), title_of(child)))
            .collect(),
    })
}

fn author_name(author: &SwarmAuthor, swarm: Option<&Swarm>) -> String {
    match author {
        SwarmAuthor::Human => "You".into(),
        SwarmAuthor::Member { member_id } => swarm
            .and_then(|s| s.members.iter().find(|m| &m.spec.id == member_id))
            .map(|m| m.spec.name.clone())
            .unwrap_or_else(|| "Unavailable member".into()),
    }
}

#[component]
fn SwarmThreadCard(
    target: StoredValue<(LocalHostId, SwarmId)>,
    post_id: SwarmPostId,
    swarm: Memo<Option<Swarm>>,
    on_open: Callback<SwarmThreadId>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let post_id = StoredValue::new(post_id);
    let root = Memo::new(move |_| {
        state.swarm_posts.with(|map| {
            map.get(&target.get_value())
                .and_then(|posts| posts.get(&post_id.get_value()))
                .cloned()
        })
    });
    view! {
        <button type="button" class="mobile-swarm-thread" data-mobile-test="swarm-thread" on:click=move |_| { if let Some(root) = root.get_untracked() { on_open.run(root.thread_id); } }>
            <span class="mobile-swarm-thread-meta"><span>{move || root.get().map(|root| author_name(&root.author, swarm.get().as_ref()))}</span><time>{move || root.get().map(|root| {
                let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(root.created_at_ms as f64));
                format!("{:02}/{:02} {:02}:{:02}", date.get_month() + 1, date.get_date(), date.get_hours(), date.get_minutes())
            })}</time></span>
            {move || root.get().and_then(|root| swarm.with(|swarm| swarm.as_ref().and_then(|swarm| thread_heading(swarm, &root.thread_id)))).map(|heading| view! {
                <span class="mobile-swarm-thread-state">
                    {heading.parent.map(|(_, title)| view! { <span class="mobile-swarm-thread-parent">{format!("Re: {title}")}</span> })}
                    <strong class:mobile-swarm-naming=heading.title.is_none()>{heading.title.clone().unwrap_or_else(|| "Naming…".to_owned())}</strong>
                    {(!heading.children.is_empty()).then(|| view! { <span class="mobile-swarm-muted">{format!("Coordination · {}", heading.children.len())}</span> })}
                </span>
            })}
            <span class="mobile-swarm-thread-content"><span class="mobile-swarm-thread-title">{move || root.get().map(|root| {
                if root.body.is_empty() { return format!("{} shared images", root.images.len()); }
                root.body.into_iter().map(|segment| match segment {
                    SwarmBodySegment::Text { text } => text,
                    SwarmBodySegment::MemberMention { member_id } => format!("@{} ", author_name(&SwarmAuthor::Member { member_id }, swarm.get().as_ref())),
                    SwarmBodySegment::PostLink { .. } => "Referenced post ↗".to_owned(),
                }).collect::<String>()
            })}</span>
            <span class="mobile-swarm-thread-open">"Open →"</span></span>
        </button>
    }
}

#[component]
fn SwarmPostCard(
    target: StoredValue<(LocalHostId, SwarmId)>,
    post_id: SwarmPostId,
    swarm: Memo<Option<Swarm>>,
    on_reply: Callback<SwarmThreadId>,
    on_link: Callback<SwarmPostId>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let post_id = StoredValue::new(post_id);
    let post = Memo::new(move |_| {
        state.swarm_posts.with(|m| {
            m.get(&target.get_value())
                .and_then(|p| p.get(&post_id.get_value()))
                .cloned()
        })
    });
    view! {
        <article class="mobile-swarm-post" class:mobile-swarm-result=move || post.with(|p| p.as_ref().is_some_and(|p| p.result)) data-mobile-test="swarm-post">
            {move || post.get().filter(|post| post.id.0 == post.thread_id.0).and_then(|post| swarm.with(|swarm| swarm.as_ref().and_then(|swarm| thread_heading(swarm, &post.thread_id)))).map(|heading| view! {
                <div class="mobile-swarm-thread-head">
                    {heading.parent.map(|(parent_id, title)| view! { <button type="button" class="mobile-swarm-link" on:click=move |_| on_link.run(SwarmPostId(parent_id.0.clone()))>{format!("Re: {title}")}</button> })}
                    <h3 class:mobile-swarm-naming=heading.title.is_none()>{heading.title.clone().unwrap_or_else(|| "Naming…".to_owned())}</h3>
                    {heading.naming_error.map(|message| view! { <p class="mobile-swarm-muted" role="status">{format!("Naming failed, retrying: {message}")}</p> })}
                    {(!heading.summary.is_empty()).then(|| view! { <details><summary>"Agent notes"</summary><div class="mobile-swarm-thread-summary">{heading.summary}</div></details> })}
                    {(!heading.children.is_empty()).then(|| view! {
                        <nav class="mobile-swarm-thread-children" aria-label="Coordination threads">
                            <span class="mobile-swarm-muted">{format!("Coordination · {}", heading.children.len())}</span>
                            {heading.children.into_iter().map(|(child_id, title)| view! { <button type="button" class="mobile-swarm-link" on:click=move |_| on_link.run(SwarmPostId(child_id.0.clone()))>{title}</button> }).collect_view()}
                        </nav>
                    })}
                </div>
            })}
            <header><strong>{move || post.get().map(|p| author_name(&p.author, swarm.get().as_ref()))}</strong><span class="mobile-swarm-muted">{move || post.get().map(|p| if p.result { "✓ Result" } else if p.id.0 != p.thread_id.0 { "Thread reply" } else { "New post" })}</span></header>
            <div class="mobile-swarm-post-body">{move || post.get().map(|p| p.body.into_iter().map(|segment| match segment {
                SwarmBodySegment::Text { text } => view! { <span>{text}</span> }.into_any(),
                SwarmBodySegment::MemberMention { member_id } => view! { <span class="mobile-swarm-mention">{format!("@{}", author_name(&SwarmAuthor::Member { member_id }, swarm.get().as_ref()))}</span> }.into_any(),
                SwarmBodySegment::PostLink { post_id } => view! { <button type="button" class="mobile-swarm-link" on:click=move |_| on_link.run(post_id.clone())>"Referenced post ↗"</button> }.into_any(),
            }).collect_view())}</div>
            {move || post.get().map(|p| p.attachments.into_iter().map(|a| view! { <p class="mobile-swarm-attachment">{format!("📄 {}", a.path.relative_path)}</p> }).collect_view())}
            <div class="mobile-swarm-images">{move || post.get().map(|p| p.images.into_iter().map(|image| view! { <SwarmSharedImage target=target image=image /> }).collect_view())}</div>
            <button type="button" class="mobile-swarm-link" data-mobile-test="swarm-reply" on:click=move |_| { if let Some(p) = post.get_untracked() { on_reply.run(p.thread_id); } }>"Open thread / Reply"</button>
        </article>
    }
}

#[component]
fn SwarmSharedImage(
    target: StoredValue<(LocalHostId, SwarmId)>,
    image: protocol::SwarmImage,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let image = StoredValue::new(image);
    let requested = RwSignal::new(None::<protocol::StreamPath>);
    let error = RwSignal::new(None::<String>);
    let outcome = Memo::new(move |_| {
        let (host, swarm) = target.get_value();
        state.swarm_images.with(|images| {
            images
                .get(&(host, swarm, image.with_value(|image| image.id.clone())))
                .cloned()
        })
    });
    let read_state = state.clone();
    let read = Callback::new(move |_| {
        let (host, swarm_id) = target.get_value();
        error.set(None);
        send(
            read_state.clone(),
            host,
            SwarmCommandPayload::ReadImage {
                swarm_id,
                image_id: image.with_value(|image| image.id.clone()),
            },
            error,
        );
    });
    Effect::new(move |_| {
        let (host, _) = target.get_value();
        let Some(stream) = state
            .bootstrapped_host_streams
            .with(|streams| streams.get(&host).cloned())
        else {
            return;
        };
        if state
            .host_streams
            .with(|streams| streams.get(&host).cloned())
            != Some(stream.clone())
            || requested.get_untracked().as_ref() == Some(&stream)
        {
            return;
        }
        requested.set(Some(stream));
        read.run(());
    });
    view! {
        <figure class="mobile-swarm-image" data-mobile-test="swarm-image">
            {move || match outcome.get() {
                Some(protocol::SwarmImageOutcome::Ready { data: Some(data), .. }) => {
                    let src = format!("data:{};base64,{}", data.media_type, data.data);
                    view! { <a href=src.clone() target="_blank" rel="noopener" aria-label="Open shared image full size"><img src=src.clone() alt=image.with_value(|image| image.name.clone()) loading="lazy" on:error=move |_| error.set(Some("This browser could not display the shared image.".into())) /></a> }.into_any()
                },
                Some(protocol::SwarmImageOutcome::Failed { error: failure }) => view! { <p role="alert" class="mobile-swarm-error">{failure.message}<button type="button" class="mobile-swarm-link" on:click=move |_| read.run(())>"Retry image"</button></p> }.into_any(),
                _ => view! { <p class="mobile-swarm-muted">"Loading shared image…"</p> }.into_any(),
            }}
            <figcaption>{image.with_value(|image| image.name.clone())}</figcaption>
            {move || error.get().map(|message| view! { <p role="alert" class="mobile-swarm-error">{message}<button type="button" class="mobile-swarm-link" on:click=move |_| read.run(())>"Retry image"</button></p> })}
        </figure>
    }
}

#[component]
fn SwarmComposer(
    target: StoredValue<(LocalHostId, SwarmId)>,
    board: RwSignal<SwarmBoard>,
    thread: RwSignal<Option<SwarmThreadId>>,
    swarm: Memo<Option<Swarm>>,
    on_published: Callback<protocol::SwarmPost>,
) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let submitted = RwSignal::new(None::<SwarmPublicationId>);
    Effect::new(move |_| {
        let Some(publication_id) = submitted.get() else {
            return;
        };
        let acknowledged = state.swarm_posts.with(|map| {
            map.get(&target.get_value())
                .and_then(|posts| {
                    posts.values().find(|post| {
                        post.author == SwarmAuthor::Human && post.publication_id == publication_id
                    })
                })
                .cloned()
        });
        if let Some(post) = acknowledged {
            submitted.set(None);
            on_published.run(post);
        }
    });
    let draft_index = Memo::new(move |_| {
        let (host, swarm_id) = target.get_value();
        let board = board.get();
        let thread_id = thread.get();
        state.swarm_composer_drafts.with(|drafts| {
            drafts.iter().position(|d| {
                d.host == host
                    && d.swarm_id == swarm_id
                    && d.board == board
                    && d.thread_id == thread_id
            })
        })
    });
    Effect::new(move |_| {
        let (host, swarm_id) = target.get_value();
        let board = board.get();
        let thread_id = thread.get();
        if draft_index.get().is_none() {
            state.swarm_composer_drafts.update(|drafts| {
                drafts.push(SwarmComposerDraft {
                    publication: None,
                    host,
                    swarm_id,
                    board,
                    thread_id,
                    text: String::new(),
                    mentions: Vec::new(),
                    publication_id: None,
                    pending: false,
                    error: None,
                })
            });
        }
    });
    let draft = Memo::new(move |_| {
        draft_index.get().and_then(|index| {
            state
                .swarm_composer_drafts
                .with(|drafts| drafts.get(index).cloned())
        })
    });
    let edit = Callback::new(
        move |(text, member): (Option<String>, Option<protocol::SwarmMemberId>)| {
            if let Some(index) = draft_index.get_untracked() {
                state.swarm_composer_drafts.update(|drafts| {
                    let d = &mut drafts[index];
                    if d.pending {
                        return;
                    }
                    if let Some(text) = text {
                        d.text = text;
                    }
                    if let Some(member) = member {
                        if d.mentions.contains(&member) {
                            d.mentions.retain(|m| m != &member);
                        } else {
                            d.mentions.push(member);
                        }
                    }
                    d.publication_id = None;
                    d.publication = None;
                    d.error = None;
                });
            }
        },
    );
    let body = Memo::new(move |_| {
        draft
            .get()
            .map(|d| {
                let mut body = d
                    .mentions
                    .into_iter()
                    .map(|member_id| SwarmBodySegment::MemberMention { member_id })
                    .collect::<Vec<_>>();
                body.push(SwarmBodySegment::Text { text: d.text });
                body
            })
            .unwrap_or_default()
    });
    let recipient_hint = Memo::new(move |_| {
        let Some(s) = swarm.get() else {
            return String::new();
        };
        let root_author = thread.get().and_then(|t| {
            state.swarm_posts.with(|m| {
                m.get(&target.get_value())
                    .and_then(|posts| posts.get(&SwarmPostId(t.0)))
                    .map(|p| p.author.clone())
            })
        });
        let recipients = protocol::swarm_publication_recipients(
            &s,
            &SwarmAuthor::Human,
            board.get(),
            root_author.as_ref(),
            &body.get(),
        );
        if recipients.is_empty() {
            "Stored on the board. @mention an agent to notify them.".into()
        } else {
            let names = recipients
                .iter()
                .filter_map(|id| {
                    s.members
                        .iter()
                        .find(|m| &m.spec.id == id)
                        .map(|m| m.spec.name.clone())
                })
                .collect::<Vec<_>>();
            format!("Notifies {}", names.join(", "))
        }
    });
    let submit_state = state.clone();
    let submit = Callback::new(move |retry: bool| {
        let Some(index) = draft_index.get_untracked() else {
            return;
        };
        let Some(d) = draft.get_untracked() else {
            return;
        };
        if (d.pending && !retry) || d.text.trim().is_empty() {
            return;
        }
        let was_pending = d.pending;
        let publication_id = d
            .publication_id
            .clone()
            .unwrap_or_else(|| SwarmPublicationId(uuid::Uuid::new_v4().to_string()));
        let publication = if retry || d.publication_id.is_some() {
            match d.publication.clone() {
                Some(publication) => publication,
                None => {
                    state.swarm_composer_drafts.update(|drafts| {
                        drafts[index].error =
                            Some("No frozen publication is available to retry.".into())
                    });
                    return;
                }
            }
        } else {
            SwarmHumanPost {
                publication_id: publication_id.clone(),
                thread_id: d.thread_id,
                body: body.get_untracked(),
                attachments: Vec::new(),
                images: Vec::new(),
            }
        };
        state.swarm_composer_drafts.update(|drafts| {
            drafts[index].publication = Some(publication.clone());
            drafts[index].publication_id = Some(publication_id.clone());
            drafts[index].pending = true;
            drafts[index].error = None;
        });
        submitted.set(Some(publication_id.clone()));
        let (host, swarm_id) = target.get_value();
        let state = submit_state.clone();
        spawn_local(async move {
            if let Err(message) = crate::actions::swarm_command(
                &state,
                &host,
                SwarmCommandPayload::Post {
                    swarm_id,
                    post: publication,
                },
            )
            .await
            {
                state.swarm_composer_drafts.update(|drafts| {
                    if let Some(draft) = drafts
                        .get_mut(index)
                        .filter(|d| d.publication_id.as_ref() == Some(&publication_id))
                    {
                        draft.pending = was_pending;
                        draft.error = Some(message);
                    }
                });
            }
        });
    });
    view! {
        <form class="mobile-swarm-composer" on:submit=move |event| { event.prevent_default(); submit.run(false); }>
            <label for="mobile-swarm-message">{move || if thread.get().is_some() { "Reply to thread" } else { "Message the swarm" }}</label>
            <textarea id="mobile-swarm-message" data-mobile-test="swarm-message" rows="3" placeholder="What’s on your mind?" prop:value=move || draft.get().map(|d| d.text).unwrap_or_default() disabled=move || draft.get().is_none_or(|d| d.pending) on:input=move |event| edit.run((Some(event_target_value(&event)), None))></textarea>
            <details class="mobile-swarm-mention-picker"><summary>"@ Mention an agent"</summary><div>{move || swarm.get().map(|s| s.members.into_iter().filter(|m| !matches!(m.state, SwarmMemberState::Retired | SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved)).map(|m| {
                let id = StoredValue::new(m.spec.id);
                view! { <button type="button" class="mobile-swarm-mention-option" aria-pressed=move || draft.get().is_some_and(|d| d.mentions.contains(&id.get_value())).to_string() disabled=move || draft.get().is_none_or(|d| d.pending) on:click=move |_| edit.run((None, Some(id.get_value())))>{format!("@{}", m.spec.name)}</button> }
            }).collect_view())}</div></details>
            <Show when=move || draft.get().is_some_and(|d| !d.mentions.is_empty())><p class="mobile-swarm-muted">{move || draft.get().map(|d| d.mentions.iter().map(|id| format!("@{}", author_name(&SwarmAuthor::Member { member_id: id.clone() }, swarm.get().as_ref()))).collect::<Vec<_>>().join(" "))}</p></Show>
            <p class="mobile-swarm-muted mobile-swarm-delivery">{recipient_hint}</p>
            <Show when=move || swarm.get().is_some_and(|s| matches!(s.lifecycle, SwarmLifecycle::Paused | SwarmLifecycle::Pausing | SwarmLifecycle::AttentionRequired))><p class="mobile-swarm-muted">"Messages stay on the board. Agents continue when you Resume."</p></Show>
            {move || draft.get().and_then(|d| d.error).map(|message| view! { <p class="mobile-swarm-error" role="alert">{message}" Your message is kept; send again to retry."</p> })}
            <Show when=move || draft.get().is_some_and(|d| d.pending)>
                <p class="mobile-swarm-muted">"Your message is kept until the host confirms it. If confirmation was lost, retrying sends the same message safely."</p>
                <button type="button" class="mobile-swarm-control" data-mobile-test="swarm-retry-delivery" on:click=move |_| submit.run(true)>"Retry delivery"</button>
            </Show>
            <button class="mobile-swarm-send" type="submit" data-mobile-test="swarm-send" disabled=move || draft.get().is_none_or(|d| d.pending || d.text.trim().is_empty())>{move || if draft.get().is_some_and(|d| d.pending) { "Waiting for host…" } else { "Send message" }}</button>
        </form>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod wasm_tests {
    use super::*;
    use crate::components::AgentsView;
    use crate::dispatch::{dispatch_envelope, prime_host_with_bootstrap_for_tests};
    use protocol::{
        Envelope, FrameKind, StreamPath, SwarmBoardNotifyPayload, SwarmBoardPage,
        SwarmCursorTarget, SwarmErrorCode, SwarmErrorNotifyPayload, SwarmNotifyPayload, SwarmPost,
        SwarmPostNotifyPayload, SwarmReadCursor,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlTextAreaElement};
    wasm_bindgen_test_configure!(run_in_browser);

    async fn tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    }
    fn element(container: &HtmlElement, name: &str) -> HtmlElement {
        container
            .query_selector(&format!("[data-mobile-test='{name}']"))
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap()
    }
    fn emit<T: serde::Serialize>(
        state: &AppState,
        host: &LocalHostId,
        seq: &mut u64,
        kind: FrameKind,
        payload: &T,
    ) {
        dispatch_envelope(
            state,
            host,
            Envelope::from_payload(StreamPath(format!("/host/{}", host.0)), kind, *seq, payload)
                .unwrap(),
        );
        *seq += 1;
    }
    fn commands() -> Vec<SwarmCommandPayload> {
        crate::bridge::test_sent_lines()
            .iter()
            .map(|line| serde_json::from_str::<Envelope>(line).unwrap())
            .filter(|e| e.kind == FrameKind::SwarmCommand)
            .map(|e| e.parse_payload().unwrap())
            .collect()
    }
    fn posts() -> Vec<SwarmHumanPost> {
        commands()
            .into_iter()
            .filter_map(|c| match c {
                SwarmCommandPayload::Post { post, .. } => Some(post),
                _ => None,
            })
            .collect()
    }
    async fn input(container: &HtmlElement, text: &str) {
        if element(container, "swarm-message")
            .get_bounding_client_rect()
            .height()
            == 0.0
        {
            element(container, "swarm-new-thread").click();
            tick().await;
        }
        assert!(
            element(container, "swarm-message")
                .get_bounding_client_rect()
                .height()
                > 0.0,
            "typing uses a visibly opened request or reply composer"
        );
        let input: HtmlTextAreaElement = element(container, "swarm-message").dyn_into().unwrap();
        input.set_value(text);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }
    fn acknowledged(
        swarm: &Swarm,
        board: SwarmBoard,
        publication: SwarmHumanPost,
        cursor: u64,
    ) -> SwarmPost {
        let id = SwarmPostId(format!("mobile-post-{cursor}"));
        SwarmPost {
            thread_seq: publication.thread_id.is_none().then_some(1),
            thread_change: None,
            result: false,
            images: Vec::new(),
            thread_id: publication
                .thread_id
                .unwrap_or_else(|| SwarmThreadId(id.0.clone())),
            id,
            swarm_id: swarm.id.clone(),
            board,
            publication_id: publication.publication_id,
            body: publication.body,
            attachments: publication.attachments,
            author: SwarmAuthor::Human,
            cursor,
            round_id: protocol::SwarmRoundId("round".into()),
            created_at_ms: cursor * 1000,
        }
    }
    fn empty_page(swarm: &Swarm, board: SwarmBoard) -> SwarmBoardPage {
        SwarmBoardPage {
            swarm_id: swarm.id.clone(),
            board,
            posts: Vec::new(),
            next_cursor: SwarmReadCursor {
                swarm_id: swarm.id.clone(),
                target: SwarmCursorTarget::BoardThreads { board },
                position: 0,
                snapshot_high_water: 0,
            },
            high_water: 0,
            has_more: false,
        }
    }
    pub(crate) fn fixture_swarm() -> Swarm {
        let project_id = protocol::ProjectId("project".into());
        Swarm {
            threads: Vec::new(),
            id: SwarmId("mobile-swarm".into()),
            host_id: protocol::HostFilterId("local".into()),
            name: "Project companions".into(),
            revision: 1,
            source_draft_id: None,
            constraints: protocol::SwarmConstraints {
                project_id: project_id.clone(),
                workspace_policy: protocol::SwarmWorkspacePolicy::ReadOnly,
                max_live_agents: 1,
                allocations: Vec::new(),
                shared_guidance: String::new(),
                agent_wake_budget: Some(16),
            },
            lifecycle: SwarmLifecycle::Running,
            members: vec![protocol::SwarmMember {
                spec: protocol::SwarmMemberSpec {
                    id: protocol::SwarmMemberId("peer".into()),
                    name: "Nova".into(),
                    focus: None,
                    backend_kind: protocol::BackendKind::Claude,
                    launch_profile_id: protocol::LaunchProfileId("claude".into()),
                    project_id,
                    pinned: false,
                    session_settings: Default::default(),
                },
                state: SwarmMemberState::Proposed,
                agent_id: None,
                session_id: None,
                runtime_status: None,
                context_cursor: 0,
                current_round_id: None,
                error: None,
                guidance_changed: false,
                unfinished_notification_ids: Vec::new(),
                replacement_due_at_ms: None,
                consecutive_replacements: 0,
                last_replacement: None,
                replaced_session_ids: Vec::new(),
            }],
            board_positions: [SwarmBoard::Briefing, SwarmBoard::Coordination]
                .into_iter()
                .map(|board| protocol::SwarmBoardPosition {
                    board,
                    high_water: 0,
                    human_read_cursor: 0,
                    unread_count: 0,
                })
                .collect(),
            notifications: Vec::new(),
            rounds: Vec::new(),
            change_preview_revision: 0,
            change_preview: None,
            error: None,
            legacy_team_id: None,
            recovery_requirement: protocol::SwarmRecoveryRequirement::None,
        }
    }

    /// Swarm members are hidden from the Agents list, so the swarm's member
    /// list is where a phone opens a member's conversation.
    #[wasm_bindgen_test]
    async fn mobile_swarm_member_opens_its_conversation() {
        let state = AppState::new();
        let host = LocalHostId("swarm-mobile-member-open".into());
        let mut swarm = fixture_swarm();
        swarm.members[0].state = SwarmMemberState::Live;
        swarm.members[0].agent_id = Some(protocol::AgentId("nova-agent".into()));
        swarm.members[0].runtime_status = Some(protocol::AgentControlStatus::Idle);
        prime_host_with_bootstrap_for_tests(&state, &host, |bootstrap| {
            bootstrap.swarms = vec![swarm.clone()]
        });
        state.active_local_host_id.set(Some(host.clone()));
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document
            .create_element("div")
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let mounted_state = state.clone();
        let _handle = leptos::mount::mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! { <AgentsView /> }
        });
        tick().await;
        element(&container, "agents-segment-swarms").click();
        tick().await;
        element(&container, "swarm-open").click();
        tick().await;
        let member = element(&container, "swarm-member-open");
        assert!(
            member.text_content().unwrap().contains("Nova"),
            "the live member is listed as openable"
        );
        member.click();
        tick().await;
        assert_eq!(
            state.active_agent.get_untracked(),
            Some(crate::state::ActiveAgentRef {
                local_host_id: host.clone(),
                agent_id: protocol::AgentId("nova-agent".into()),
            }),
            "tapping the member opens its agent"
        );
        assert!(state.viewing_chat.get_untracked(), "the chat is shown");
    }

    #[wasm_bindgen_test]
    async fn mobile_swarm_shared_conversation_preserves_drafts_and_retries_until_host_acknowledges()
    {
        crate::components::test_styles::ensure_styles_loaded();
        let _sends = crate::bridge::test_capture_sends();
        let state = AppState::new();
        let host = LocalHostId("swarm-mobile-dom".into());
        let mut swarm = fixture_swarm();
        prime_host_with_bootstrap_for_tests(&state, &host, |bootstrap| {
            bootstrap.swarms = vec![swarm.clone()]
        });
        state.active_local_host_id.set(Some(host.clone()));
        let mut seq = 0;
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document
            .create_element("div")
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap();
        container
            .set_attribute("style", "width:390px;max-width:100%;position:relative")
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let mounted_state = state.clone();
        let handle = leptos::mount::mount_to(container.clone(), move || {
            provide_context(mounted_state.clone());
            view! { <AgentsView /> }
        });
        tick().await;
        element(&container, "agents-segment-swarms").click();
        tick().await;
        assert!(container.text_content().unwrap().contains("Swarms"));
        assert_eq!(
            container
                .query_selector("h1")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap(),
            "Swarms"
        );
        assert!(
            element(&container, "swarm-open")
                .text_content()
                .unwrap()
                .contains("Project companions")
        );
        swarm.name = "Interactive companions".into();
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        tick().await;
        assert!(
            element(&container, "swarm-open")
                .text_content()
                .unwrap()
                .contains("Interactive companions"),
            "same-key cards react to canonical server updates"
        );
        element(&container, "swarm-open").click();
        tick().await;
        assert!(commands().iter().any(|c| matches!(c, SwarmCommandPayload::ReadBoard { swarm_id, query } if *swarm_id == swarm.id && query.board == SwarmBoard::Briefing)));
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: empty_page(&swarm, SwarmBoard::Briefing),
            },
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("What would you like to work on?")
        );
        input(&container, "Let's discuss this project.").await;
        tick().await;
        element(&container, "swarm-send").click();
        tick().await;
        assert_eq!(posts().len(), 1);
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "Let's discuss this project.",
            "transport acceptance does not invent durable publication"
        );
        element(&container, "swarm-retry-delivery").click();
        tick().await;
        assert_eq!(
            posts()[0],
            posts()[1],
            "explicit retry preserves the unconfirmed publication identity"
        );
        assert_eq!(posts()[0].thread_id, None, "a new request starts a thread");
        let first = acknowledged(&swarm, SwarmBoard::Briefing, posts()[0].clone(), 1);
        swarm.threads.push(protocol::SwarmThread {
            swarm_id: swarm.id.clone(),
            thread_id: first.thread_id.clone(),
            board: SwarmBoard::Briefing,
            parent_thread_id: None,
            title: None,
            description: None,
            naming_error: None,
            summary: String::new(),
            seq: 1,
            child_seq: 0,
            creation_cursor: 1,
        });
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        let shared_image = protocol::SwarmImage {
            id: protocol::SwarmImageId("5e8c5c8e-3aab-43d0-b285-1966331e6f9d".into()),
            name: "Reference.png".into(),
            media_type: "image/png".into(),
            width: 16,
            height: 12,
            byte_len: 78,
        };
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmPostNotify,
            &SwarmPostNotifyPayload {
                post: first.clone(),
            },
        );
        tick().await;
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            ""
        );
        assert!(
            element(&container, "swarm-thread")
                .text_content()
                .unwrap()
                .contains("Let's discuss this project.")
        );
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='swarm-post']")
                .unwrap()
                .length(),
            0
        );
        assert_eq!(
            element(&container, "swarm-message")
                .get_bounding_client_rect()
                .height(),
            0.0,
            "root-only inbox does not reserve a composer when it is closed"
        );
        assert!(
            !commands()
                .iter()
                .any(|command| matches!(command, SwarmCommandPayload::ReadImage { .. })),
            "inbox does not download full-size media before opening its thread"
        );
        let conversation = element(&container, "swarm-conversation").get_bounding_client_rect();
        let first_row = element(&container, "swarm-thread").get_bounding_client_rect();
        assert!(
            first_row.top() - conversation.top() <= 220.0,
            "compact mobile controls leave the viewport for requests"
        );
        input(&container, "Keep this briefing draft").await;
        element(&container, "swarm-coordination").click();
        tick().await;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: empty_page(&swarm, SwarmBoard::Coordination),
            },
        );
        tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='swarm-new-thread']")
                .unwrap()
                .is_none(),
            "only agents open Coordination threads"
        );
        swarm.threads[0].title = Some("Discuss the project".into());
        swarm.threads[0].child_seq = 1;
        swarm.threads.push(protocol::SwarmThread {
            swarm_id: swarm.id.clone(),
            thread_id: SwarmThreadId("coord-1".into()),
            board: SwarmBoard::Coordination,
            parent_thread_id: Some(first.thread_id.clone()),
            title: Some("Review plan".into()),
            description: Some("How the agents split the review".into()),
            naming_error: None,
            summary: String::new(),
            seq: 1,
            child_seq: 0,
            creation_cursor: 2,
        });
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        let mut coordination_root = acknowledged(
            &swarm,
            SwarmBoard::Coordination,
            SwarmHumanPost {
                publication_id: SwarmPublicationId("coord-root".into()),
                thread_id: None,
                body: vec![SwarmBodySegment::Text {
                    text: "Splitting the review".into(),
                }],
                attachments: Vec::new(),
                images: Vec::new(),
            },
            2,
        );
        coordination_root.id = SwarmPostId("coord-1".into());
        coordination_root.thread_id = SwarmThreadId("coord-1".into());
        coordination_root.author = SwarmAuthor::Member {
            member_id: swarm.members[0].spec.id.clone(),
        };
        let mut coordination_page = empty_page(&swarm, SwarmBoard::Coordination);
        coordination_page.posts = vec![coordination_root];
        coordination_page.high_water = 2;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: coordination_page,
            },
        );
        tick().await;
        let coordination_card = element(&container, "swarm-thread");
        assert!(
            coordination_card
                .text_content()
                .unwrap()
                .contains("Re: Discuss the project")
                && coordination_card
                    .text_content()
                    .unwrap()
                    .contains("Review plan"),
            "coordination threads name the request they serve"
        );
        coordination_card.click();
        tick().await;
        input(&container, "Can you review this?").await;
        container
            .query_selector(".mobile-swarm-mention-picker summary")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        tick().await;
        container
            .query_selector(".mobile-swarm-mention-option")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        tick().await;
        assert!(container.text_content().unwrap().contains("Notifies Nova"));
        element(&container, "swarm-send").click();
        tick().await;
        let publication = posts()[2].clone();
        assert_eq!(publication.thread_id, Some(SwarmThreadId("coord-1".into())));
        assert!(publication.body.iter().any(|s| matches!(s, SwarmBodySegment::MemberMention { member_id } if member_id == &swarm.members[0].spec.id)));
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmErrorNotify,
            &SwarmErrorNotifyPayload {
                swarm_id: Some(swarm.id.clone()),
                draft_id: None,
                publication_id: Some(publication.publication_id.clone()),
                code: SwarmErrorCode::Storage,
                message: "Host could not persist this message".into(),
            },
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Your message is kept")
        );
        element(&container, "swarm-send").click();
        tick().await;
        assert_eq!(
            posts()[3],
            publication,
            "retry reuses the exact publication, not a duplicate ID"
        );
        let coordination = acknowledged(&swarm, SwarmBoard::Coordination, publication, 3);
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmPostNotify,
            &SwarmPostNotifyPayload { post: coordination },
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Can you review this?"),
            "the committed reply appears in the thread"
        );
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Host could not persist this message"),
            "the host committing the reply resolves the failed-publication alert"
        );
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "",
            "a committed reply leaves the composer"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='swarm-retry-delivery']")
                .unwrap()
                .is_none(),
            "a committed reply is never offered for redelivery"
        );
        element(&container, "swarm-briefing").click();
        tick().await;
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "Keep this briefing draft"
        );
        state.connection_statuses.update(|m| {
            m.insert(host.clone(), crate::state::ConnectionStatus::Bootstrapping);
        });
        tick().await;
        prime_host_with_bootstrap_for_tests(&state, &host, |bootstrap| {
            bootstrap.swarms = vec![swarm.clone()]
        });
        seq = 0;
        tick().await;
        let mut history = vec![first.clone()];
        history.extend((3..=62).map(|cursor| {
            acknowledged(
                &swarm,
                SwarmBoard::Briefing,
                SwarmHumanPost {
                    images: Vec::new(),
                    publication_id: SwarmPublicationId(format!("past-{cursor}")),
                    body: vec![SwarmBodySegment::Text {
                        text: format!("Earlier discussion {cursor}"),
                    }],
                    thread_id: None,
                    attachments: Vec::new(),
                },
                cursor,
            )
        }));
        let mut first_page = empty_page(&swarm, SwarmBoard::Briefing);
        first_page.posts = history.iter().rev().take(50).cloned().collect();
        first_page.has_more = true;
        first_page.high_water = 62;
        first_page.next_cursor.position = 50;
        first_page.next_cursor.snapshot_high_water = 62;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: first_page.clone(),
            },
        );
        tick().await;
        element(&container, "swarm-more").click();
        tick().await;
        assert!(commands().iter().any(|c| matches!(c, SwarmCommandPayload::ReadBoard { query, .. } if query.after_cursor.as_ref() == Some(&first_page.next_cursor))));
        let mut complete_page = first_page.clone();
        complete_page.posts = history.iter().rev().skip(50).cloned().collect();
        complete_page.has_more = false;
        complete_page.next_cursor.position = 61;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: complete_page,
            },
        );
        tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='swarm-thread']")
                .unwrap()
                .length(),
            61
        );
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='swarm-post']")
                .unwrap()
                .length(),
            0,
            "the inbox contains requests, not every reply"
        );
        let rows = container
            .query_selector_all("[data-mobile-test='swarm-thread']")
            .unwrap();
        assert!(
            rows.item(0)
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Earlier discussion 62")
        );
        let original = rows.item(60).unwrap().dyn_into::<HtmlElement>().unwrap();
        assert!(
            original
                .text_content()
                .unwrap()
                .contains("Let's discuss this project.")
        );
        assert!(
            original.get_bounding_client_rect().height() <= 120.0
                && original.get_bounding_client_rect().height() >= 44.0,
            "compact request cards remain touchable"
        );
        original.click();
        tick().await;
        assert!(
            commands()
                .iter()
                .any(|c| matches!(c, SwarmCommandPayload::ReadThread { .. }))
        );
        let mut reply = acknowledged(
            &swarm,
            SwarmBoard::Briefing,
            SwarmHumanPost {
                images: Vec::new(),
                publication_id: SwarmPublicationId("agent-reply".into()),
                body: vec![SwarmBodySegment::Text {
                    text: "Agent answer is visible on the board".into(),
                }],
                thread_id: Some(SwarmThreadId("mobile-post-1".into())),
                attachments: Vec::new(),
            },
            63,
        );
        reply.author = SwarmAuthor::Member {
            member_id: swarm.members[0].spec.id.clone(),
        };
        reply.result = true;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmPostNotify,
            &SwarmPostNotifyPayload {
                post: reply.clone(),
            },
        );
        input(&container, "Thread draft stays separate").await;
        element(&container, "swarm-coordination").click();
        tick().await;
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            ""
        );
        element(&container, "swarm-briefing").click();
        tick().await;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload { page: first_page },
        );
        tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='swarm-more']")
                .unwrap()
                .is_none(),
            "returning to a board preserves its completed pagination baseline"
        );
        // The inbox now renders roots only. Marking the whole board read
        // here would falsely consume replies in threads the user has not opened.
        assert!(
            !container
                .text_content()
                .unwrap()
                .contains("Agent answer is visible on the board")
        );
        swarm.board_positions[0].high_water = 63;
        swarm.board_positions[0].unread_count = 1;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        tick().await;
        assert!(
            !commands().iter().any(|c| matches!(
                c,
                SwarmCommandPayload::MarkRead {
                    board: SwarmBoard::Briefing,
                    cursor: 63,
                    ..
                }
            )),
            "root-only inbox cannot mark unseen reply activity read"
        );
        let rows = container
            .query_selector_all("[data-mobile-test='swarm-thread']")
            .unwrap();
        assert!(
            rows.item(0)
                .unwrap()
                .text_content()
                .unwrap()
                .contains("Earlier discussion 62"),
            "new replies do not reorder requests by agent activity"
        );
        rows.item(60)
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Agent answer is visible on the board"),
            "all reply activity remains reachable inside its request"
        );
        let answers = {
            let cards = container
                .query_selector_all("[data-mobile-test='swarm-post']")
                .unwrap();
            (0..cards.length())
                .filter_map(|index| cards.item(index).and_then(|node| node.text_content()))
                .filter(|text| text.contains("✓ Result"))
                .collect::<Vec<_>>()
        };
        assert!(
            answers.len() == 1 && answers[0].contains("Agent answer is visible on the board"),
            "the member's typed answer is marked as the request's result: {answers:?}"
        );
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "Thread draft stays separate"
        );
        swarm.threads[0].summary = "Agent reply verified".into();
        swarm.threads[0].seq = 2;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Agent reply verified"),
            "mobile current state updates from the server without replay inference"
        );
        element(&container, "swarm-send").click();
        tick().await;
        let conditional = posts().last().unwrap().clone();
        assert_eq!(
            conditional.thread_id,
            Some(first.thread_id.clone()),
            "mobile replies name only their thread; the host owns its summary"
        );
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmPostNotify,
            &SwarmPostNotifyPayload {
                post: acknowledged(&swarm, SwarmBoard::Briefing, conditional, 63),
            },
        );
        tick().await;
        element(&container, "swarm-back-to-board").click();
        tick().await;
        input(&container, "Lost confirmation, recovered by fetching").await;
        tick().await;
        element(&container, "swarm-send").click();
        tick().await;
        let unconfirmed = posts().last().unwrap().clone();
        let recovered = acknowledged(&swarm, SwarmBoard::Briefing, unconfirmed, 64);
        state.connection_statuses.update(|m| {
            m.insert(host.clone(), crate::state::ConnectionStatus::Bootstrapping);
        });
        tick().await;
        let reads_before = commands()
            .iter()
            .filter(|c| matches!(c, SwarmCommandPayload::ReadBoard { .. }))
            .count();
        prime_host_with_bootstrap_for_tests(&state, &host, |bootstrap| {
            bootstrap.swarms = vec![swarm.clone()]
        });
        seq = 0;
        tick().await;
        assert!(
            commands()
                .iter()
                .filter(|c| matches!(c, SwarmCommandPayload::ReadBoard { .. }))
                .count()
                > reads_before,
            "reconnected open conversation reloads canonical board state"
        );
        let mut recovered_page = empty_page(&swarm, SwarmBoard::Briefing);
        recovered_page.posts = vec![recovered];
        recovered_page.high_water = 64;
        recovered_page.next_cursor.position = 1;
        recovered_page.next_cursor.snapshot_high_water = 64;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmBoardNotify,
            &SwarmBoardNotifyPayload {
                page: recovered_page,
            },
        );
        tick().await;
        assert_eq!(
            element(&container, "swarm-message")
                .dyn_into::<HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "",
            "a fetched canonical human post settles its matching unconfirmed draft"
        );
        let linked_root = acknowledged(
            &swarm,
            SwarmBoard::Coordination,
            SwarmHumanPost {
                images: Vec::new(),
                publication_id: SwarmPublicationId("older-thread".into()),
                body: vec![SwarmBodySegment::Text {
                    text: "Older agent discussion".into(),
                }],
                thread_id: None,
                attachments: Vec::new(),
            },
            1000,
        );
        let replies = (1001..=1060)
            .map(|cursor| {
                acknowledged(
                    &swarm,
                    SwarmBoard::Coordination,
                    SwarmHumanPost {
                        images: Vec::new(),
                        publication_id: SwarmPublicationId(format!("older-reply-{cursor}")),
                        body: vec![SwarmBodySegment::Text {
                            text: format!("Earlier thread reply {cursor}"),
                        }],
                        thread_id: Some(linked_root.thread_id.clone()),
                        attachments: Vec::new(),
                    },
                    cursor,
                )
            })
            .collect::<Vec<_>>();
        let late_reply = replies.last().unwrap().clone();
        let mut link_source = acknowledged(
            &swarm,
            SwarmBoard::Briefing,
            SwarmHumanPost {
                images: vec![shared_image.id.clone()],
                publication_id: SwarmPublicationId("link-source".into()),
                body: vec![SwarmBodySegment::PostLink {
                    post_id: late_reply.id.clone(),
                }],
                thread_id: None,
                attachments: Vec::new(),
            },
            1061,
        );
        link_source.images.push(shared_image.clone());
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmPostNotify,
            &SwarmPostNotifyPayload { post: link_source },
        );
        tick().await;
        element(&container, "swarm-thread").click();
        tick().await;
        assert!(commands().iter().any(|command| matches!(command, SwarmCommandPayload::ReadImage { swarm_id, image_id } if *swarm_id == swarm.id && *image_id == shared_image.id)));
        emit(&state, &host, &mut seq, FrameKind::SwarmImageNotify, &protocol::SwarmImageNotifyPayload { swarm_id: swarm.id.clone(), image_id: shared_image.id.clone(), outcome: protocol::SwarmImageOutcome::Ready { image: shared_image, data: Some(protocol::ImageData { media_type: "image/png".into(), data: "iVBORw0KGgoAAAANSUhEUgAAABAAAAAMCAIAAADkharWAAAAFUlEQVR4nGPg3fKVJMQwqmFUA3YEAK1USJCnamHcAAAAAElFTkSuQmCC".into() }) } });
        tick().await;
        let preview = container
            .query_selector("[data-mobile-test='swarm-image'] img")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlImageElement>()
            .unwrap();
        preview.scroll_into_view();
        tick().await;
        web_sys::console::log_1(
            &format!(
                "Mobile shared image: complete={}, decoded_width={}, visible_height={}",
                preview.complete(),
                preview.natural_width(),
                preview.get_bounding_client_rect().height(),
            )
            .into(),
        );
        wasm_bindgen_futures::JsFuture::from(preview.decode())
            .await
            .expect("visible shared pixels decode in the browser");
        assert_eq!(
            preview.natural_width(),
            16,
            "opened mobile threads display actual shared pixels"
        );
        {
            let buttons = container.query_selector_all("button").unwrap();
            (0..buttons.length()).filter_map(move |index| {
                buttons
                    .item(index)
                    .and_then(|node| node.dyn_into::<HtmlElement>().ok())
            })
        }
        .find(|button| button.text_content().as_deref() == Some("Referenced post ↗"))
        .unwrap()
        .click();
        tick().await;
        assert!(commands().iter().any(|c| matches!(c, SwarmCommandPayload::ReadPost { post_id, .. } if post_id == &late_reply.id)));
        let mut thread_page = protocol::SwarmThreadPage {
            swarm_id: swarm.id.clone(),
            thread_id: linked_root.thread_id.clone(),
            root: linked_root.clone(),
            posts: vec![late_reply],
            next_cursor: SwarmReadCursor {
                swarm_id: swarm.id.clone(),
                target: SwarmCursorTarget::Thread {
                    thread_id: linked_root.thread_id.clone(),
                },
                position: 1060,
                snapshot_high_water: 1060,
            },
            high_water: 1060,
            has_more: false,
        };
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmThreadNotify,
            &protocol::SwarmThreadNotifyPayload {
                page: thread_page.clone(),
            },
        );
        tick().await;
        assert!(commands().iter().any(|c| matches!(c, SwarmCommandPayload::ReadThread { query, .. } if query.thread_id == linked_root.thread_id && query.after_cursor.is_none())));
        thread_page.posts = replies.iter().take(50).cloned().collect();
        thread_page.next_cursor.position = 1050;
        thread_page.has_more = true;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmThreadNotify,
            &protocol::SwarmThreadNotifyPayload {
                page: thread_page.clone(),
            },
        );
        tick().await;
        element(&container, "swarm-more").click();
        tick().await;
        assert!(commands().iter().any(|c| matches!(c, SwarmCommandPayload::ReadThread { query, .. } if query.after_cursor.as_ref() == Some(&thread_page.next_cursor))), "a linked suffix must not hide the missing middle of a thread");
        thread_page.posts = replies.into_iter().skip(50).collect();
        thread_page.next_cursor.position = 1060;
        thread_page.has_more = false;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmThreadNotify,
            &protocol::SwarmThreadNotifyPayload { page: thread_page },
        );
        tick().await;
        assert_eq!(
            container
                .query_selector_all("[data-mobile-test='swarm-post']")
                .unwrap()
                .length(),
            61,
            "root and all 60 replies are reachable after a late post link"
        );
        swarm.lifecycle = SwarmLifecycle::Paused;
        emit(
            &state,
            &host,
            &mut seq,
            FrameKind::SwarmNotify,
            &SwarmNotifyPayload {
                swarm: swarm.clone(),
            },
        );
        tick().await;
        assert_eq!(
            element(&container, "swarm-pause-resume")
                .text_content()
                .unwrap(),
            "Resume"
        );
        element(&container, "swarm-pause-resume").click();
        tick().await;
        assert!(commands().iter().any(
            |c| matches!(c, SwarmCommandPayload::Resume { swarm_id } if swarm_id == &swarm.id)
        ));
        let frame = element(&container, "swarm-conversation").get_bounding_client_rect();
        let composer = element(&container, "swarm-message").get_bounding_client_rect();
        assert!(
            composer.width() >= 250.0 && composer.right() <= frame.right() + 1.0,
            "mobile composer fits the phone viewport"
        );
        assert!(
            element(&container, "swarm-send")
                .get_bounding_client_rect()
                .height()
                >= 44.0
        );
        state
            .active_local_host_id
            .set(Some(LocalHostId("another-host".into())));
        tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='swarm-conversation']")
                .unwrap()
                .is_none(),
            "switching hosts never leaks another host's board"
        );
        drop(handle);
        container.remove();
    }
}
