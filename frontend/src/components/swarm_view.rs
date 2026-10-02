//! A swarm's shared conversation: the Briefing and Coordination boards.
//!
//! Everything rendered here is a projection of server records in
//! `AppState::{swarms, swarm_posts, swarm_errors}`. Posts arrive through
//! board/thread pages and live `SwarmPostNotify`; membership, lifecycle,
//! unread positions and delivery outcomes arrive through `SwarmNotify`.
//! Members' own transcripts are never shown on a board — a member's
//! conversation opens as its ordinary chat tab.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use protocol::{
    AgentControlStatus, BackendKind, ProjectId, ProjectPath, StreamPath, Swarm, SwarmAttachment,
    SwarmAuthor, SwarmBoard, SwarmBoardRead, SwarmBodySegment, SwarmCommandPayload,
    SwarmDeliveryState, SwarmId, SwarmLifecycle, SwarmMember, SwarmMemberId, SwarmMemberState,
    SwarmNotificationId, SwarmPost, SwarmPostId, SwarmPublication, SwarmPublicationId,
    SwarmReadCursor, SwarmThreadId, SwarmThreadRead, SwarmWorkspacePolicy,
};

use crate::components::swarm_dialogs::ManageSwarmDialog;
use crate::state::{ActiveAgentRef, AppState, ProjectInfo, SwarmErrorEntry, TabContent};

// ── Shared helpers (also used by the dock panel and dialogs) ───────────────

/// Send one `SwarmCommand` on the host stream. A transport failure is a
/// client-side fact and is reported through `on_error`; every domain outcome
/// comes back from the server as notify frames.
pub(crate) fn send_swarm_command(
    host_streams: RwSignal<HashMap<String, StreamPath>>,
    host_id: &str,
    command: SwarmCommandPayload,
    on_error: Option<Callback<String>>,
) {
    let Some(stream) = host_streams.with_untracked(|streams| streams.get(host_id).cloned()) else {
        log::error!("swarm command: no host stream for {host_id}");
        if let Some(on_error) = on_error {
            on_error.run("Not connected to this host.".to_owned());
        }
        return;
    };
    let host_id = host_id.to_owned();
    spawn_local(async move {
        if let Err(error) = crate::send::swarm_command(&host_id, stream, command).await {
            log::error!("swarm command send failed: {error}");
            if let Some(on_error) = on_error {
                on_error.run(error);
            }
        }
    });
}

pub(crate) fn open_swarm_tab(state: &AppState, host_id: String, swarm_id: SwarmId, name: String) {
    state.open_tab(TabContent::Swarm { host_id, swarm_id }, name, true);
}

/// Open a member's own conversation. Only a member with a live agent has one.
pub(crate) fn open_swarm_member_chat(state: &AppState, host_id: String, member: &SwarmMember) {
    let Some(agent_id) = member.agent_id.clone() else {
        log::error!(
            "open_swarm_member_chat: member {} has no agent",
            member.spec.id
        );
        return;
    };
    state.open_tab(
        TabContent::chat_with_agent(ActiveAgentRef { host_id, agent_id }),
        member.spec.name.clone(),
        true,
    );
}

pub(crate) fn lifecycle_label(lifecycle: SwarmLifecycle) -> &'static str {
    match lifecycle {
        SwarmLifecycle::Launching => "Launching",
        SwarmLifecycle::Running => "Running",
        SwarmLifecycle::Pausing => "Pausing…",
        SwarmLifecycle::Paused => "Paused",
        SwarmLifecycle::AttentionRequired => "Needs attention",
        SwarmLifecycle::Transitioning => "Applying changes",
    }
}

pub(crate) fn lifecycle_tone(lifecycle: SwarmLifecycle) -> &'static str {
    match lifecycle {
        SwarmLifecycle::Running => "ok",
        SwarmLifecycle::Launching | SwarmLifecycle::Transitioning => "busy",
        SwarmLifecycle::Pausing | SwarmLifecycle::Paused => "muted",
        SwarmLifecycle::AttentionRequired => "warn",
    }
}

/// Human label for a member from its server-owned state and runtime status.
pub(crate) fn member_status_label(member: &SwarmMember) -> &'static str {
    match member.state {
        SwarmMemberState::Proposed => "Not started",
        SwarmMemberState::Dormant => "Ready to resume",
        SwarmMemberState::Reserved => "Starting",
        SwarmMemberState::Live => match member.runtime_status {
            Some(AgentControlStatus::Thinking) => "Working",
            Some(AgentControlStatus::AwaitingUser) => "Needs your answer",
            Some(AgentControlStatus::Idle) => "Idle",
            Some(AgentControlStatus::Failed) => "Failed",
            None => "Status unavailable",
        },
        SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved => "Retiring",
        SwarmMemberState::Retired => "Retired",
        SwarmMemberState::Failed => "Failed",
    }
}

pub(crate) fn member_status_tone(member: &SwarmMember) -> &'static str {
    match member.state {
        SwarmMemberState::Proposed | SwarmMemberState::Reserved => "busy",
        SwarmMemberState::Live => match member.runtime_status {
            Some(AgentControlStatus::Thinking) => "active",
            Some(AgentControlStatus::AwaitingUser) => "warn",
            Some(AgentControlStatus::Failed) => "error",
            Some(AgentControlStatus::Idle) => "ok",
            None => "unknown",
        },
        SwarmMemberState::Dormant
        | SwarmMemberState::Retiring
        | SwarmMemberState::RetiringReserved
        | SwarmMemberState::Retired => "muted",
        SwarmMemberState::Failed => "error",
    }
}

pub(crate) fn member_is_working(member: &SwarmMember) -> bool {
    member.state == SwarmMemberState::Live
        && member.runtime_status == Some(AgentControlStatus::Thinking)
}

pub(crate) fn board_label(board: SwarmBoard) -> &'static str {
    match board {
        SwarmBoard::Briefing => "Briefing",
        SwarmBoard::Coordination => "Coordination",
    }
}

pub(crate) fn swarm_error_presentation(
    code: protocol::SwarmErrorCode,
) -> (&'static str, &'static str) {
    match code {
        protocol::SwarmErrorCode::CommittedDurabilityUncertain => ("warn", "status"),
        protocol::SwarmErrorCode::Busy
        | protocol::SwarmErrorCode::Invalid
        | protocol::SwarmErrorCode::NotFound
        | protocol::SwarmErrorCode::Conflict
        | protocol::SwarmErrorCode::Unauthorized
        | protocol::SwarmErrorCode::Storage
        | protocol::SwarmErrorCode::Lifecycle
        | protocol::SwarmErrorCode::Unsupported => ("error", "alert"),
    }
}

pub(crate) fn board_unread(swarm: &Swarm, board: SwarmBoard) -> u64 {
    swarm
        .board_positions
        .iter()
        .find(|position| position.board == board)
        .map(|position| position.unread_count)
        .unwrap_or(0)
}

pub(crate) fn workspace_policy_label(policy: SwarmWorkspacePolicy) -> &'static str {
    match policy {
        SwarmWorkspacePolicy::ReadOnly => "Read-only project access",
        SwarmWorkspacePolicy::SharedWorkbench { .. } => "Shared writable workbench",
    }
}

pub(crate) fn backend_name(kind: BackendKind) -> &'static str {
    crate::components::teams_panel::backend_kind_label(kind)
}

/// Stable per-member avatar hue so a member reads the same everywhere.
pub(crate) fn avatar_style(member_id: &str) -> String {
    let hash = member_id.bytes().fold(2166136261u32, |acc, byte| {
        (acc ^ u32::from(byte)).wrapping_mul(16777619)
    });
    format!("background-color: hsl({} 42% 38%);", hash % 360)
}

pub(crate) fn initials(name: &str) -> String {
    let mut out: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect();
    if out.is_empty() {
        out.push('?');
    }
    out.to_uppercase()
}

pub(crate) fn project_name(
    projects: &[ProjectInfo],
    host_id: &str,
    project_id: &ProjectId,
) -> String {
    projects
        .iter()
        .find(|info| info.host_id == host_id && &info.project.id == project_id)
        .map(|info| info.project.name.clone())
        .unwrap_or_else(|| "Unknown project".to_owned())
}

pub(crate) fn mint_id() -> String {
    crate::state::new_history_request_id()
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn format_time(ms: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let today = js_sys::Date::new_0();
    let clock = format!("{:02}:{:02}", date.get_hours(), date.get_minutes());
    if date.to_date_string() == today.to_date_string() {
        clock
    } else {
        format!(
            "{} {} {clock}",
            MONTHS[(date.get_month() as usize).min(11)],
            date.get_date()
        )
    }
}

fn author_name(swarm: Option<&Swarm>, author: &SwarmAuthor) -> String {
    match author {
        SwarmAuthor::Human => "You".to_owned(),
        SwarmAuthor::Member { member_id } => member_name(swarm, member_id),
    }
}

fn member_name(swarm: Option<&Swarm>, member_id: &SwarmMemberId) -> String {
    swarm
        .and_then(|swarm| {
            swarm
                .members
                .iter()
                .find(|member| &member.spec.id == member_id)
        })
        .map(|member| member.spec.name.clone())
        .unwrap_or_else(|| "Former member".to_owned())
}

fn recipient_name(swarm: &Swarm, member: &SwarmMember, number: usize) -> String {
    if swarm
        .members
        .iter()
        .filter(|other| other.spec.name == member.spec.name)
        .count()
        > 1
    {
        format!(
            "{} · {} · {} · Member {number}",
            member.spec.name,
            backend_name(member.spec.backend_kind),
            member.spec.focus.as_deref().unwrap_or("Generalist")
        )
    } else {
        member.spec.name.clone()
    }
}

fn post_excerpt(post: &SwarmPost, swarm: Option<&Swarm>) -> String {
    let mut text = String::new();
    for segment in &post.body {
        match segment {
            SwarmBodySegment::Text { text: chunk } => text.push_str(chunk),
            SwarmBodySegment::MemberMention { member_id } => {
                text.push('@');
                text.push_str(&member_name(swarm, member_id));
            }
            SwarmBodySegment::PostLink { .. } => {}
        }
    }
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 {
        format!("{}…", flat.chars().take(60).collect::<String>())
    } else {
        flat
    }
}

/// Render a post body as markdown with typed mention and post-link segments.
/// Segments travel through the parser as placeholders and are substituted on
/// parser events, so literal text can never produce a typed mention and a
/// segment next to code, links, or attributes degrades to escaped text.
fn render_body(
    post: &SwarmPost,
    swarm: Option<&Swarm>,
    posts: &HashMap<SwarmPostId, SwarmPost>,
) -> String {
    use crate::markdown::{InlineToken, inline_token_placeholder, strip_token_markers};
    let mut source = String::new();
    let mut tokens: Vec<InlineToken> = Vec::new();
    for segment in &post.body {
        let token = match segment {
            SwarmBodySegment::Text { text } => {
                source.push_str(&strip_token_markers(text));
                continue;
            }
            SwarmBodySegment::MemberMention { member_id } => {
                let label = format!("@{}", member_name(swarm, member_id));
                InlineToken {
                    html: format!(
                        "<button type=\"button\" class=\"swarm-mention\" data-member-id=\"{}\" aria-label=\"Open {}'s conversation\"{}>{}</button>",
                        escape_html(&member_id.0),
                        escape_html(&member_name(swarm, member_id)),
                        if swarm.is_some_and(|swarm| swarm.members.iter().any(|member| member
                            .spec
                            .id
                            == *member_id
                            && member.agent_id.is_some()))
                        {
                            ""
                        } else {
                            " disabled"
                        },
                        escape_html(&label)
                    ),
                    text: label,
                }
            }
            SwarmBodySegment::PostLink { post_id } => match posts.get(post_id) {
                Some(linked) => {
                    let label = format!(
                        "↪ {}: {}",
                        author_name(swarm, &linked.author),
                        post_excerpt(linked, swarm)
                    );
                    InlineToken {
                        html: format!(
                            "<a href=\"#\" class=\"swarm-post-link\" data-post-id=\"{}\">{}</a>",
                            escape_html(&post_id.0),
                            escape_html(&label)
                        ),
                        text: label,
                    }
                }
                // Not in any loaded page yet: the host resolves it by id.
                None => InlineToken {
                    html: format!(
                        "<button type=\"button\" class=\"swarm-post-link swarm-post-link-unloaded\" data-post-id=\"{}\">↪ Open linked post</button>",
                        escape_html(&post_id.0)
                    ),
                    text: "↪ linked post".to_owned(),
                },
            },
        };
        source.push_str(&inline_token_placeholder(tokens.len()));
        tokens.push(token);
    }
    crate::markdown::render_markdown_with_tokens(&source, &tokens)
}

fn attachment_name(path: &ProjectPath) -> String {
    path.relative_path
        .rsplit('/')
        .next()
        .unwrap_or(&path.relative_path)
        .to_owned()
}

fn delivery_phrase(state: SwarmDeliveryState) -> &'static str {
    match state {
        SwarmDeliveryState::Pending => "queued for",
        SwarmDeliveryState::Dispatching => "sending to",
        SwarmDeliveryState::Accepted => "delivered to",
        SwarmDeliveryState::Uncertain => "delivery uncertain for",
        SwarmDeliveryState::Undeliverable => "undeliverable to",
        SwarmDeliveryState::Failed => "delivery failed for",
    }
}

type DeliveryGroup = (
    SwarmDeliveryState,
    Vec<(SwarmMemberId, String, Option<String>)>,
    Vec<SwarmNotificationId>,
);

fn delivery_rank(state: SwarmDeliveryState) -> u8 {
    match state {
        SwarmDeliveryState::Failed => 0,
        SwarmDeliveryState::Uncertain => 1,
        SwarmDeliveryState::Undeliverable => 2,
        SwarmDeliveryState::Pending => 3,
        SwarmDeliveryState::Dispatching => 4,
        SwarmDeliveryState::Accepted => 5,
    }
}

// ── The board view ─────────────────────────────────────────────────────────

#[component]
pub fn SwarmView(
    host_id: String,
    swarm_id: SwarmId,
    #[prop(into)] visible: Signal<bool>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let host = StoredValue::new(host_id);
    let sid = StoredValue::new(swarm_id);
    let host_streams = state.host_streams;
    let swarms_signal = state.swarms;
    let posts_signal = state.swarm_posts;
    let errors_signal = state.swarm_errors;
    let projects_signal = state.projects;
    let state_sv = StoredValue::new_local(state.clone());

    let swarm: Memo<Option<Swarm>> = Memo::new(move |_| {
        swarms_signal.with(|map| {
            map.get(&host.get_value())
                .and_then(|swarms| swarms.get(&sid.get_value()).cloned())
        })
    });
    let board = RwSignal::new(SwarmBoard::Briefing);
    let manage_open = RwSignal::new(false);
    let action_error: RwSignal<Option<String>> = RwSignal::new(None);
    let on_action_error = Callback::new(move |message: String| action_error.set(Some(message)));

    let send = move |command: SwarmCommandPayload| {
        action_error.set(None);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            command,
            Some(on_action_error),
        );
    };

    // Board loading. The first page is requested when the board is shown; while
    // the server reports more, the next page follows from its `next_cursor`.
    // Requests are remembered per stream so a reattach (new stream, cleared
    // posts) reads again.
    let requested: RwSignal<Vec<(StreamPath, SwarmBoard, Option<SwarmReadCursor>)>> =
        RwSignal::new(Vec::new());
    Effect::new(move |_| {
        if !visible.get() || swarm.with(|swarm| swarm.is_none()) {
            return;
        }
        let Some(stream) = host_streams.with(|streams| streams.get(&host.get_value()).cloned())
        else {
            return;
        };
        let current = board.get();
        let page = posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.page(current))
        });
        let after_cursor = match page {
            None => None,
            Some(meta) if meta.has_more => Some(meta.next_cursor),
            Some(_) => return,
        };
        let key = (stream, current, after_cursor.clone());
        if requested.with_untracked(|sent| sent.contains(&key)) {
            return;
        }
        requested.update(|sent| sent.push(key));
        send(SwarmCommandPayload::ReadBoard {
            swarm_id: sid.get_value(),
            query: SwarmBoardRead {
                board: current,
                after_cursor,
                limit: None,
            },
        });
    });

    // Unread: once the visible board is fully loaded, advance the shared human
    // read position to the board's server-reported high-water mark.
    let marked: RwSignal<Vec<(SwarmBoard, u64)>> = RwSignal::new(Vec::new());
    Effect::new(move |_| {
        if !visible.get() {
            return;
        }
        let current = board.get();
        let Some((unread, high_water)) = swarm.with(|swarm| {
            swarm.as_ref().and_then(|swarm| {
                swarm
                    .board_positions
                    .iter()
                    .find(|position| position.board == current)
                    .map(|position| (position.unread_count, position.high_water))
            })
        }) else {
            return;
        };
        let loaded = posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.page(current))
                .is_some_and(|meta| !meta.has_more)
        });
        if unread == 0 || !loaded || high_water == 0 {
            return;
        }
        if marked.with_untracked(|sent| sent.contains(&(current, high_water))) {
            return;
        }
        marked.update(|sent| sent.push((current, high_water)));
        send(SwarmCommandPayload::MarkRead {
            swarm_id: sid.get_value(),
            board: current,
            cursor: high_water,
        });
    });

    let loaded_threads: Memo<Vec<(SwarmThreadId, SwarmBoard)>> = Memo::new(move |_| {
        posts_signal.with(|map| {
            let Some(posts) = map.get(&(host.get_value(), sid.get_value())) else {
                return Vec::new();
            };
            let mut latest: HashMap<SwarmThreadId, (SwarmBoard, u64)> = HashMap::new();
            for post in posts.posts.values() {
                let entry = latest
                    .entry(post.thread_id.clone())
                    .or_insert((post.board, 0));
                entry.1 = entry.1.max(post.cursor);
            }
            let mut threads: Vec<_> = latest.into_iter().collect();
            threads.sort_by_key(|(_, (_, cursor))| *cursor);
            threads
                .into_iter()
                .map(|(thread, (board, _))| (thread, board))
                .collect()
        })
    });
    let board_empty = Memo::new(move |_| {
        let current = board.get();
        loaded_threads.with(|threads| !threads.iter().any(|(_, target)| *target == current))
    });
    let board_loading = Memo::new(move |_| {
        let current = board.get();
        posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.page(current))
                .is_none_or(|meta| meta.has_more)
        })
    });

    // A publication error is shown by the composer still holding that
    // publication; only errors no mounted composer owns surface here.
    let claimed = ClaimedPublications(RwSignal::new(HashSet::new()));
    provide_context(claimed);
    let swarm_errors: Memo<Vec<SwarmErrorEntry>> = Memo::new(move |_| {
        let claimed = claimed.0.get();
        errors_signal.with(|errors| {
            errors
                .iter()
                .filter(|entry| {
                    entry.host_id == host.get_value()
                        && entry.error.swarm_id.as_ref() == Some(&sid.get_value())
                        && entry.error.publication_id.as_ref().is_none_or(|id| {
                            !claimed.contains(id)
                                || entry.error.code
                                    == protocol::SwarmErrorCode::CommittedDurabilityUncertain
                        })
                })
                .cloned()
                .collect()
        })
    });

    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    // A linked post outside every loaded page, requested from the host by its
    // exact id, plus the swarm errors already present at request time so only
    // a new error ends the wait.
    let resolving: RwSignal<Option<(SwarmPostId, Vec<u64>)>> = RwSignal::new(None);
    let scroll_to_post = move |post_id: SwarmPostId| {
        let target_board = posts_signal.with_untracked(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.posts.get(&post_id).map(|post| post.board))
        });
        let Some(target_board) = target_board else {
            let known = errors_signal.with_untracked(|errors| {
                errors
                    .iter()
                    .filter(|entry| entry.host_id == host.get_value())
                    .map(|entry| entry.serial)
                    .collect()
            });
            resolving.set(Some((post_id.clone(), known)));
            send(SwarmCommandPayload::ReadPost {
                swarm_id: sid.get_value(),
                post_id,
            });
            return;
        };
        resolving.set(None);
        board.set(target_board);
        let element_id = format!("swarm-post-{}", post_id.0);
        request_animation_frame(move || {
            let Some(element) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id(&element_id))
            else {
                return;
            };
            element.scroll_into_view();
            if let Some(html) = element.dyn_ref::<web_sys::HtmlElement>() {
                let _ = html.focus();
            }
        });
    };
    let on_link = Callback::new(scroll_to_post);
    Effect::new(move |_| {
        let Some((post_id, known)) = resolving.get() else {
            return;
        };
        let arrived = posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .is_some_and(|posts| posts.posts.contains_key(&post_id))
        });
        if arrived {
            scroll_to_post(post_id);
            return;
        }
        let failed = errors_signal.with(|errors| {
            errors.iter().any(|entry| {
                !known.contains(&entry.serial)
                    && entry.host_id == host.get_value()
                    && entry.error.swarm_id.as_ref() == Some(&sid.get_value())
            })
        });
        if failed || action_error.with(|error| error.is_some()) {
            resolving.set(None);
        }
    });

    let header = move || {
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let lifecycle = current.lifecycle;
        let members = current
            .members
            .iter()
            .filter(|member| member.state != SwarmMemberState::Retired)
            .count();
        let working = current
            .members
            .iter()
            .filter(|member| member_is_working(member))
            .count();
        let project = projects_signal.with(|projects| {
            project_name(projects, &host.get_value(), &current.constraints.project_id)
        });
        let policy = current.constraints.workspace_policy;
        let can_pause = matches!(
            lifecycle,
            SwarmLifecycle::Running | SwarmLifecycle::Launching | SwarmLifecycle::Transitioning
        );
        let can_resume = matches!(
            lifecycle,
            SwarmLifecycle::Paused | SwarmLifecycle::AttentionRequired
        );
        let pausing = lifecycle == SwarmLifecycle::Pausing;
        view! {
            <header class="swarm-header">
                <div class="swarm-header-text">
                    <h1 class="swarm-title">{current.name.clone()}</h1>
                    <div class="swarm-subtitle">
                        <span>{project}</span>
                        <span class="swarm-dot-sep">"·"</span>
                        <span
                            class="swarm-scope-label"
                            data-policy=match policy {
                                SwarmWorkspacePolicy::ReadOnly => "read_only",
                                SwarmWorkspacePolicy::SharedWorkbench { .. } => "shared_workbench",
                            }
                        >
                            {workspace_policy_label(policy)}
                        </span>
                        <span class="swarm-dot-sep">"·"</span>
                        <span>{format!("{members} member{} · {working} working", if members == 1 { "" } else { "s" })}</span>
                    </div>
                </div>
                <div class="swarm-header-actions">
                    <span class="swarm-lifecycle-pill" data-tone=lifecycle_tone(lifecycle)>
                        {lifecycle_label(lifecycle)}
                    </span>
                    {can_pause.then(|| view! {
                        <button
                            class="swarm-btn"
                            on:click=move |_| send(SwarmCommandPayload::Pause { swarm_id: sid.get_value() })
                        >
                            "Pause"
                        </button>
                    })}
                    {pausing.then(|| view! {
                        <button class="swarm-btn" disabled=true title="Waiting for running turns to stop">
                            "Pausing…"
                        </button>
                    })}
                    {can_resume.then(|| view! {
                        <button
                            class="swarm-btn swarm-btn-primary"
                            on:click=move |_| send(SwarmCommandPayload::Resume { swarm_id: sid.get_value() })
                        >
                            "Resume"
                        </button>
                    })}
                    <button class="swarm-btn" on:click=move |_| manage_open.set(true)>
                        {move || if swarm.with(|s| s.as_ref().is_some_and(|s| s.change_preview.is_some())) {
                            "Manage • preview pending"
                        } else {
                            "Manage"
                        }}
                    </button>
                </div>
            </header>
        }
        .into_any()
    };

    let member_strip = move || {
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let rows = current
            .members
            .iter()
            .filter(|member| member.state != SwarmMemberState::Retired)
            .map(|member| {
                let label = member_status_label(member);
                let tone = member_status_tone(member);
                let name = member.spec.name.clone();
                let detail = match &member.spec.focus {
                    Some(focus) if !focus.is_empty() => format!("{} · {}", backend_name(member.spec.backend_kind), focus),
                    _ => backend_name(member.spec.backend_kind).to_owned(),
                };
                let avatar = view! {
                    <span class="swarm-avatar swarm-avatar-sm" style=avatar_style(&member.spec.id.0) aria-hidden="true">
                        {initials(&name)}
                    </span>
                };
                let body = view! {
                    {avatar}
                    <span class="swarm-member-chip-text">
                        <span class="swarm-member-chip-name">{name.clone()}</span>
                        <span class="swarm-member-chip-status" data-tone=tone>{label}</span>
                    </span>
                };
                if member.agent_id.is_some() {
                    let member_for_click = member.clone();
                    view! {
                        <button
                            class="swarm-member-chip"
                            title=format!("Open {name}'s conversation — {detail}")
                            on:click=move |_| state_sv.with_value(|state| {
                                open_swarm_member_chat(state, host.get_value(), &member_for_click)
                            })
                        >
                            {body}
                        </button>
                    }
                    .into_any()
                } else {
                    view! {
                        <div class="swarm-member-chip swarm-member-chip-static" title=format!("{detail} — no conversation yet")>
                            {body}
                        </div>
                    }
                    .into_any()
                }
            })
            .collect_view();
        view! { <div class="swarm-member-strip" aria-label="Members">{rows}</div> }.into_any()
    };

    // Launch / partial-failure / attention states, from server records only.
    let status_banners = move || {
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let failed: Vec<SwarmMember> = current
            .members
            .iter()
            .filter(|member| member.state == SwarmMemberState::Failed || member.error.is_some())
            .cloned()
            .collect();
        let starting = current
            .members
            .iter()
            .filter(|member| {
                matches!(
                    member.state,
                    SwarmMemberState::Proposed | SwarmMemberState::Reserved
                )
            })
            .count();
        let attention = (current.lifecycle == SwarmLifecycle::AttentionRequired).then(|| {
            let reason = current
                .error
                .clone()
                .unwrap_or_else(|| "The swarm stopped waking agents and is waiting for you.".to_owned());
            view! {
                <div class="swarm-banner" data-tone="warn" role="status">
                    <span class="swarm-banner-text">{reason}</span>
                    <button
                        class="swarm-btn swarm-btn-primary"
                        on:click=move |_| send(SwarmCommandPayload::Resume { swarm_id: sid.get_value() })
                    >
                        "Resume"
                    </button>
                </div>
            }
        });
        let swarm_error = (current.lifecycle != SwarmLifecycle::AttentionRequired)
            .then_some(current.error.clone())
            .flatten()
            .map(|message| {
                view! {
                    <div class="swarm-banner" data-tone="error" role="alert">
                        <span class="swarm-banner-text">{message}</span>
                    </div>
                }
            });
        let launching =
            (current.lifecycle == SwarmLifecycle::Launching && starting > 0).then(|| {
                view! {
                    <div class="swarm-banner" data-tone="info" role="status">
                        <span class="swarm-banner-text">
                            {format!("Starting {starting} of {} member{}…", current.members.len(), if current.members.len() == 1 { "" } else { "s" })}
                        </span>
                    </div>
                }
            });
        let failures = (!failed.is_empty()).then(|| {
            let summary = if failed.len() == 1 {
                "A member needs attention. Review the error below."
            } else {
                "Some members need attention. Review the errors below."
            };
            let rows = failed
                .into_iter()
                .map(|member| {
                    let member_id = member.spec.id.clone();
                    let retirable = !matches!(member.state, SwarmMemberState::Retiring | SwarmMemberState::RetiringReserved | SwarmMemberState::Retired);
                    view! {
                        <li class="swarm-failure-row">
                            <span class="swarm-failure-name">{member.spec.name.clone()}</span>
                            <span class="swarm-failure-reason">
                                {member.error.clone().unwrap_or_else(|| "Member unavailable".to_owned())}
                            </span>
                            {retirable.then(|| view! {
                                <button
                                    class="swarm-btn"
                                    on:click=move |_| send(SwarmCommandPayload::RetryMember {
                                        swarm_id: sid.get_value(),
                                        member_id: member_id.clone(),
                                    })
                                >
                                    "Retry"
                                </button>
                            })}
                        </li>
                    }
                })
                .collect_view();
            view! {
                <div class="swarm-banner swarm-banner-list" data-tone="error" role="alert">
                    <span class="swarm-banner-text">{summary}</span>
                    <ul class="swarm-failure-list">{rows}</ul>
                </div>
            }
        });
        view! { {attention} {swarm_error} {launching} {failures} }.into_any()
    };

    let error_banners = move || {
        let resolving_banner = resolving.get().map(|_| view! {
            <div class="swarm-banner" data-tone="info" role="status">
                <span class="swarm-banner-text">"Opening linked post…"</span>
                <button class="swarm-btn swarm-btn-quiet" on:click=move |_| resolving.set(None)>"Cancel"</button>
            </div>
        });
        let action = action_error.get().map(|message| view! {
            <div class="swarm-banner" data-tone="error" role="alert">
                <span class="swarm-banner-text">{format!("Could not reach the host: {message}")}</span>
                <button class="swarm-btn swarm-btn-quiet" on:click=move |_| action_error.set(None)>"Dismiss"</button>
            </div>
        });
        let server = swarm_errors
            .get()
            .into_iter()
            .map(|entry| {
                let serial = entry.serial;
                let host_for_dismiss = entry.host_id.clone();
                let (tone, role) = swarm_error_presentation(entry.error.code);
                view! {
                    <div class="swarm-banner swarm-server-error" data-tone=tone role=role>
                        <span class="swarm-banner-text">{entry.error.message.clone()}</span>
                        <button
                            class="swarm-btn swarm-btn-quiet"
                            on:click=move |_| errors_signal.update(|errors| {
                                errors.retain(|e| !(e.serial == serial && e.host_id == host_for_dismiss))
                            })
                        >
                            "Dismiss"
                        </button>
                    </div>
                }
            })
            .collect_view();
        view! { {resolving_banner} {action} {server} }
    };

    let view_identity = StoredValue::new(format!(
        "{}-{}-{}",
        host.get_value(),
        sid.get_value().0,
        mint_id()
    ));
    let panel_id = StoredValue::new(format!("swarm-board-panel-{}", view_identity.get_value()));
    let tab_id = move |target: SwarmBoard| {
        format!(
            "swarm-board-tab-{}-{}",
            view_identity.get_value(),
            board_label(target)
        )
    };
    let briefing_tab_ref = NodeRef::<leptos::html::Button>::new();
    let coordination_tab_ref = NodeRef::<leptos::html::Button>::new();
    let board_tab = move |target: SwarmBoard| {
        let unread =
            move || swarm.with(|s| s.as_ref().map(|s| board_unread(s, target)).unwrap_or(0));
        let tab_ref = match target {
            SwarmBoard::Briefing => briefing_tab_ref,
            SwarmBoard::Coordination => coordination_tab_ref,
        };
        let on_keydown = move |ev: web_sys::KeyboardEvent| {
            let next = match ev.key().as_str() {
                "ArrowLeft" | "ArrowRight" => match target {
                    SwarmBoard::Briefing => SwarmBoard::Coordination,
                    SwarmBoard::Coordination => SwarmBoard::Briefing,
                },
                "Home" => SwarmBoard::Briefing,
                "End" => SwarmBoard::Coordination,
                _ => return,
            };
            ev.prevent_default();
            board.set(next);
            let next_ref = match next {
                SwarmBoard::Briefing => briefing_tab_ref,
                SwarmBoard::Coordination => coordination_tab_ref,
            };
            if let Some(element) = next_ref.get_untracked()
                && element.focus().is_err()
            {
                action_error.set(Some("Could not focus the selected board tab.".to_owned()));
            }
        };
        view! {
            <button
                class="swarm-board-tab"
                role="tab"
                id=tab_id(target)
                node_ref=tab_ref
                aria-controls=panel_id.get_value()
                tabindex=move || if board.get() == target { "0" } else { "-1" }
                aria-selected=move || (board.get() == target).to_string()
                class:active=move || board.get() == target
                data-board=board_label(target)
                on:click=move |_| board.set(target)
                on:keydown=on_keydown
            >
                <span>{board_label(target)}</span>
                {move || (unread() > 0).then(|| view! {
                    <span class="swarm-unread-badge" aria-label=format!("{} unread", unread())>{unread()}</span>
                })}
            </button>
        }
    };

    let board_hint = move || match board.get() {
        SwarmBoard::Briefing => "Requests, questions, and results for the swarm.",
        SwarmBoard::Coordination => "Peer coordination and working discussion.",
    };

    let missing = move || swarm.with(|s| s.is_none());

    view! {
        <div class="swarm-view">
            <Show
                when=move || !missing()
                fallback=|| view! {
                    <div class="swarm-empty swarm-empty-page">
                        <h2>"Swarm not available"</h2>
                        <p>"This swarm is not on the connected host."</p>
                    </div>
                }
            >
                {header}
                {member_strip}
                <div class="swarm-banners">{status_banners}{error_banners}</div>
                <nav class="swarm-board-tabs" role="tablist" aria-label="Boards">
                    {board_tab(SwarmBoard::Briefing)}
                    {board_tab(SwarmBoard::Coordination)}
                    <span class="swarm-board-hint">{board_hint}</span>
                </nav>
                <div class="swarm-board-scroll" node_ref=scroll_ref role="tabpanel" id=panel_id.get_value() aria-labelledby=move || tab_id(board.get()) tabindex="0">
                    <div class="swarm-board-column">
                        <Show when=move || board_empty.get()>
                            <div class="swarm-empty">
                                {move || if board_loading.get() {
                                    "Loading board…"
                                } else if board.get() == SwarmBoard::Briefing {
                                    "No briefing posts yet. Post direction for the swarm below."
                                } else {
                                    "Members haven't coordinated here yet."
                                }}
                            </div>
                        </Show>
                        <For
                            each=move || loaded_threads.get()
                            key=|entry| entry.0.clone()
                            let:entry
                        >
                            <SwarmThread
                                host=host
                                sid=sid
                                thread_id=entry.0
                                swarm=swarm
                                board=entry.1
                                selected_board=Signal::derive(move || board.get())
                                on_link=on_link
                            />
                        </For>
                    </div>
                </div>
                // Root drafts stay mounted with fixed destinations; switching
                // boards must never reroute unsent text or a pending retry.
                {[SwarmBoard::Briefing, SwarmBoard::Coordination].into_iter().map(|target| view! {
                    <div class="swarm-root-composer" hidden=move || board.get() != target>
                        <SwarmComposer
                            host=host
                            sid=sid
                            swarm=swarm
                            board=Signal::derive(move || target)
                            thread_id=None
                        />
                    </div>
                }).collect_view()}
            </Show>
            {move || manage_open.get().then(|| view! {
                <ManageSwarmDialog
                    host_id=host.get_value()
                    swarm_id=sid.get_value()
                    on_close=Callback::new(move |_| manage_open.set(false))
                />
            })}
        </div>
    }
}

#[component]
fn SwarmThread(
    host: StoredValue<String>,
    sid: StoredValue<SwarmId>,
    thread_id: SwarmThreadId,
    swarm: Memo<Option<Swarm>>,
    board: SwarmBoard,
    selected_board: Signal<SwarmBoard>,
    on_link: Callback<SwarmPostId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let posts_signal = state.swarm_posts;
    let host_streams = state.host_streams;
    let thread = StoredValue::new(thread_id);
    let reply_open = RwSignal::new(false);

    // Root first, then replies in board order; posts are immutable records.
    let thread_posts: Memo<(Option<SwarmPost>, Vec<SwarmPost>)> = Memo::new(move |_| {
        posts_signal.with(|map| {
            let Some(posts) = map.get(&(host.get_value(), sid.get_value())) else {
                return (None, Vec::new());
            };
            let thread_id = thread.get_value();
            let mut root = None;
            let mut replies = Vec::new();
            for post in posts
                .posts
                .values()
                .filter(|post| post.thread_id == thread_id)
            {
                if post.id.0 == thread_id.0 {
                    root = Some(post.clone());
                } else {
                    replies.push(post.clone());
                }
            }
            replies.sort_by_key(|post| post.cursor);
            (root, replies)
        })
    });
    let thread_meta = Memo::new(move |_| {
        posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.threads.get(&thread.get_value()).cloned())
        })
    });
    let load_replies = move |_| {
        let after_cursor = thread_meta.get_untracked().map(|meta| meta.next_cursor);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::ReadThread {
                swarm_id: sid.get_value(),
                query: SwarmThreadRead {
                    thread_id: thread.get_value(),
                    after_cursor,
                    limit: None,
                },
            },
            None,
        );
    };
    let show_load = move || thread_meta.get().is_none_or(|meta| meta.has_more);
    let load_label = move || {
        if thread_meta.get().is_some() {
            "Load more replies"
        } else {
            "Load full thread"
        }
    };

    view! {
        // A thread's board comes from immutable posts, not the active tab;
        // keeping its keyed component mounted preserves local reply drafts.
        <section class="swarm-thread" data-thread-id=thread.get_value().0 hidden=move || selected_board.get() != board>
            {move || match thread_posts.with(|(root, _)| root.clone()) {
                Some(root) => view! {
                    <SwarmPostCard host=host post=root swarm=swarm on_link=on_link />
                }.into_any(),
                None => view! {
                    <div class="swarm-thread-orphan">"Reply to an earlier post"</div>
                }.into_any(),
            }}
            <div class="swarm-replies">
                <For
                    each=move || thread_posts.with(|(_, replies)| replies.clone())
                    key=|post| post.id.clone()
                    let:post
                >
                    <SwarmPostCard host=host post=post swarm=swarm on_link=on_link />
                </For>
            </div>
            <div class="swarm-thread-actions">
                <Show when=move || !reply_open.get()>
                    <button class="swarm-link-btn" on:click=move |_| reply_open.set(true)>"Reply"</button>
                </Show>
                <Show when=show_load>
                    <button class="swarm-link-btn" on:click=load_replies>{load_label}</button>
                </Show>
            </div>
            <Show when=move || reply_open.get()>
                <div class="swarm-reply-composer">
                    <SwarmComposer
                        host=host
                        sid=sid
                        swarm=swarm
                        board=Signal::derive(move || board)
                        thread_id=Some(thread.get_value())
                        on_cancel=Callback::new(move |_| reply_open.set(false))
                    />
                </div>
            </Show>
        </section>
    }
}

#[component]
fn SwarmPostCard(
    host: StoredValue<String>,
    post: SwarmPost,
    swarm: Memo<Option<Swarm>>,
    on_link: Callback<SwarmPostId>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let posts_signal = state.swarm_posts;
    let host_streams = state.host_streams;
    let state_sv = StoredValue::new_local(state.clone());
    let post = StoredValue::new(post);

    let author = move || {
        swarm.with(|swarm| post.with_value(|post| author_name(swarm.as_ref(), &post.author)))
    };
    let author_meta = move || {
        swarm.with(|swarm| {
            post.with_value(|post| match &post.author {
                SwarmAuthor::Human => None,
                SwarmAuthor::Member { member_id } => swarm
                    .as_ref()
                    .and_then(|swarm| {
                        swarm
                            .members
                            .iter()
                            .find(|member| &member.spec.id == member_id)
                    })
                    .map(|member| backend_name(member.spec.backend_kind)),
            })
        })
    };
    let avatar_key = post.with_value(|post| match &post.author {
        SwarmAuthor::Human => "human".to_owned(),
        SwarmAuthor::Member { member_id } => member_id.0.clone(),
    });
    let is_human = post.with_value(|post| matches!(post.author, SwarmAuthor::Human));
    let body_html = move || {
        let swarm_value = swarm.get();
        posts_signal.with(|map| {
            let empty = HashMap::new();
            let posts = map
                .get(&(
                    host.get_value(),
                    post.with_value(|post| post.swarm_id.clone()),
                ))
                .map(|posts| &posts.posts)
                .unwrap_or(&empty);
            post.with_value(|post| render_body(post, swarm_value.as_ref(), posts))
        })
    };

    let on_body_click = move |ev: web_sys::MouseEvent| {
        let Some(target) = ev
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        // The post card itself carries `data-post-id`; only a link element navigates.
        if let Ok(Some(link)) = target.closest(".swarm-post-link") {
            ev.prevent_default();
            if let Some(post_id) = link.get_attribute("data-post-id") {
                on_link.run(SwarmPostId(post_id));
            }
            return;
        }
        if let Ok(Some(mention)) = target.closest("[data-member-id]") {
            let Some(member_id) = mention.get_attribute("data-member-id") else {
                return;
            };
            let member = swarm.with_untracked(|swarm| {
                swarm.as_ref().and_then(|swarm| {
                    swarm
                        .members
                        .iter()
                        .find(|member| member.spec.id.0 == member_id)
                        .cloned()
                })
            });
            if let Some(member) = member.filter(|member| member.agent_id.is_some()) {
                state_sv
                    .with_value(|state| open_swarm_member_chat(state, host.get_value(), &member));
            }
        }
    };
    let on_body_keydown = move |ev: web_sys::KeyboardEvent| {
        if !matches!(ev.key().as_str(), "Enter" | " ") {
            return;
        }
        let Some(target) = ev
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        if let Ok(Some(reference)) =
            target.closest("button.swarm-mention:not(:disabled), .swarm-post-link")
            && let Some(reference) = reference.dyn_ref::<web_sys::HtmlElement>()
        {
            ev.prevent_default();
            reference.click();
        }
    };

    let attachments = move || {
        let items = post.with_value(|post| post.attachments.clone());
        if items.is_empty() {
            return ().into_any();
        }
        let chips = items
            .into_iter()
            .map(|attachment| {
                let label = attachment_name(&attachment.path);
                let full = attachment.path.relative_path.clone();
                let path = attachment.path.clone();
                let project_id = attachment.project_id.clone();
                view! {
                    <button
                        class="swarm-attachment"
                        title=full
                        on:click=move |_| {
                            let path = path.clone();
                            let project_id = project_id.clone();
                            state_sv.with_value(|state| {
                                crate::actions::open_project_path_for(state, host.get_value(), project_id, path);
                            });
                        }
                    >
                        <span class="swarm-attachment-icon" aria-hidden="true">"📄"</span>
                        {label}
                    </button>
                }
            })
            .collect_view();
        view! { <div class="swarm-attachments">{chips}</div> }.into_any()
    };

    // Delivery outcomes for this post, grouped by state. "Delivered" means the
    // backend accepted the turn — never that the member read or acted on it.
    let delivery = move || {
        let post_id = post.with_value(|post| post.id.clone());
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let mut groups: Vec<DeliveryGroup> = Vec::new();
        for notification in current
            .notifications
            .iter()
            .filter(|notification| notification.post_ids.contains(&post_id))
        {
            let name = member_name(Some(&current), &notification.member_id);
            let entry = (
                notification.member_id.clone(),
                name,
                notification.error.clone(),
            );
            match groups
                .iter_mut()
                .find(|(state, _, _)| *state == notification.state)
            {
                Some((_, members, notification_ids)) => {
                    notification_ids.push(notification.id.clone());
                    if !members.iter().any(|(id, _, _)| id == &entry.0) {
                        members.push(entry);
                    }
                }
                None => groups.push((
                    notification.state,
                    vec![entry],
                    vec![notification.id.clone()],
                )),
            }
        }
        if groups.is_empty() {
            return ().into_any();
        }
        groups.sort_by_key(|(state, _, _)| delivery_rank(*state));
        let rows = groups
            .into_iter()
            .map(|(delivery_state, members, notification_ids)| {
                let names = members.iter().map(|(_, name, _)| name.clone()).collect::<Vec<_>>().join(", ");
                let reason = members.iter().find_map(|(_, _, error)| error.clone());
                let retry = matches!(delivery_state, SwarmDeliveryState::Uncertain | SwarmDeliveryState::Failed)
                    .then(|| {
                        let swarm_id = current.id.clone();
                        view! {
                            <button
                                class="swarm-link-btn"
                                on:click=move |_| {
                                    for notification_id in &notification_ids {
                                        send_swarm_command(
                                            host_streams,
                                            &host.get_value(),
                                            SwarmCommandPayload::RetryNotification {
                                                swarm_id: swarm_id.clone(),
                                                notification_id: notification_id.clone(),
                                            },
                                            None,
                                        );
                                    }
                                }
                            >
                                "Retry delivery"
                            </button>
                        }
                    });
                let tone = match delivery_state {
                    SwarmDeliveryState::Accepted => "ok",
                    SwarmDeliveryState::Pending | SwarmDeliveryState::Dispatching => "muted",
                    SwarmDeliveryState::Uncertain | SwarmDeliveryState::Undeliverable => "warn",
                    SwarmDeliveryState::Failed => "error",
                };
                view! {
                    <span class="swarm-delivery-group" data-tone=tone data-state=format!("{delivery_state:?}")>
                        {format!("{} {names}", delivery_phrase(delivery_state))}
                        {reason.map(|reason| format!(" — {reason}"))}
                        {retry}
                    </span>
                }
            })
            .collect_view();
        view! { <div class="swarm-delivery">{rows}</div> }.into_any()
    };

    let (post_id, created_at_ms) = post.with_value(|post| (post.id.0.clone(), post.created_at_ms));
    view! {
        <article
            class="swarm-post"
            class:swarm-post-human=is_human
            id=format!("swarm-post-{post_id}")
            data-post-id=post_id
            tabindex="-1"
        >
            <span class="swarm-avatar" style=avatar_style(&avatar_key) aria-hidden="true">
                {move || initials(&author())}
            </span>
            <div class="swarm-post-main">
                <header class="swarm-post-meta">
                    <span class="swarm-post-author">{author}</span>
                    {move || author_meta().map(|meta| view! { <span class="swarm-post-backend">{meta}</span> })}
                    <time class="swarm-post-time">{format_time(created_at_ms)}</time>
                </header>
                <div class="swarm-post-body chat-card-body" on:click=on_body_click on:keydown=on_body_keydown inner_html=body_html></div>
                {attachments}
                {delivery}
            </div>
        </article>
    }
}

// ── Composer ───────────────────────────────────────────────────────────────

/// Publication ids currently held by a mounted composer in this swarm view.
#[derive(Clone, Copy)]
struct ClaimedPublications(RwSignal<HashSet<SwarmPublicationId>>);

#[derive(Clone, Debug, PartialEq)]
struct PendingPublication {
    swarm_id: SwarmId,
    publication: SwarmPublication,
}

#[derive(Clone, Debug, PartialEq)]
struct ComposerReference {
    start: usize,
    end: usize,
    segment: SwarmBodySegment,
}

fn compose_segments(text: &str, references: &[ComposerReference]) -> Vec<SwarmBodySegment> {
    let mut segments = Vec::new();
    let mut position = 0;
    for reference in references {
        if reference.start > position {
            segments.push(SwarmBodySegment::Text {
                text: text[position..reference.start].to_owned(),
            });
        }
        segments.push(reference.segment.clone());
        position = reference.end;
    }
    if position < text.len() {
        segments.push(SwarmBodySegment::Text {
            text: text[position..].to_owned(),
        });
    }
    segments
}

fn edit_references(
    references: &mut Vec<ComposerReference>,
    start: usize,
    end: usize,
    inserted: usize,
) {
    #[cfg(all(test, target_arch = "wasm32"))]
    let previous_count = references.len();
    references.retain_mut(|reference| {
        if reference.end <= start {
            true
        } else if reference.start >= end {
            reference.start = reference.start - (end - start) + inserted;
            reference.end = reference.end - (end - start) + inserted;
            true
        } else {
            false
        }
    });
    #[cfg(all(test, target_arch = "wasm32"))]
    log::debug!(
        "swarm composer edit references_before={} references_after={}",
        previous_count,
        references.len()
    );
}

fn changed_text_range(previous: &str, next: &str) -> (usize, usize, usize) {
    let prefix = previous
        .chars()
        .zip(next.chars())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    let suffix = previous[prefix..]
        .chars()
        .rev()
        .zip(next[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(ch, _)| ch.len_utf8())
        .sum::<usize>();
    (
        prefix,
        previous.len() - suffix,
        next.len() - prefix - suffix,
    )
}

/// The `@query` immediately before the caret, if the caret is inside one.
fn mention_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let before = text.get(..caret)?;
    let at = before.rfind('@')?;
    if before[..at]
        .chars()
        .next_back()
        .is_some_and(|ch| !ch.is_whitespace() && ch != '(')
    {
        return None;
    }
    let query = &before[at + 1..];
    if query.contains('\n') || query.chars().count() > 32 {
        return None;
    }
    Some((at, query.to_owned()))
}

fn utf16_to_byte(text: &str, utf16: u32) -> usize {
    let mut units = 0u32;
    for (index, ch) in text.char_indices() {
        if units >= utf16 {
            return index;
        }
        units += ch.len_utf16() as u32;
    }
    text.len()
}

fn byte_to_utf16(text: &str, byte: usize) -> u32 {
    text[..byte.min(text.len())].encode_utf16().count() as u32
}

#[component]
fn SwarmComposer(
    host: StoredValue<String>,
    sid: StoredValue<SwarmId>,
    swarm: Memo<Option<Swarm>>,
    board: Signal<SwarmBoard>,
    thread_id: Option<SwarmThreadId>,
    #[prop(optional)] on_cancel: Option<Callback<()>>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let posts_signal = state.swarm_posts;
    let host_streams = state.host_streams;
    let open_files = state.open_files;
    let errors_signal = state.swarm_errors;
    let claimed = expect_context::<ClaimedPublications>();
    let thread = StoredValue::new(thread_id);
    let is_reply = thread.with_value(|thread| thread.is_some());

    let text = RwSignal::new(String::new());
    let references: RwSignal<Vec<ComposerReference>> = RwSignal::new(Vec::new());
    let input_range: StoredValue<Option<(usize, usize)>> = StoredValue::new(None);
    let attachments: RwSignal<Vec<SwarmAttachment>> = RwSignal::new(Vec::new());
    let pending: RwSignal<Option<PendingPublication>> = RwSignal::new(None);
    let send_error: RwSignal<Option<String>> = RwSignal::new(None);
    let completion: RwSignal<Option<(usize, String)>> = RwSignal::new(None);
    let selected = RwSignal::new(0usize);
    let attach_open = RwSignal::new(false);
    let link_open = RwSignal::new(false);
    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
    let reply_root_author = Memo::new(move |_| {
        let thread_id = thread.get_value()?;
        posts_signal.with(|map| {
            map.get(&(host.get_value(), sid.get_value()))
                .and_then(|posts| posts.posts.get(&SwarmPostId(thread_id.0.clone())))
                .filter(|post| post.thread_id == thread_id)
                .map(|post| post.author.clone())
        })
    });
    let routing_available = Memo::new(move |_| {
        swarm.with(|swarm| swarm.is_some())
            && (!is_reply || reply_root_author.with(|author| author.is_some()))
    });

    #[cfg(all(test, target_arch = "wasm32"))]
    log::debug!(
        "swarm composer mounted reply={is_reply} board={:?}",
        board.get_untracked()
    );

    // While a publication is outstanding this composer owns its errors.
    Effect::new(move |previous: Option<Option<SwarmPublicationId>>| {
        let current = pending.with(|pending| {
            pending
                .as_ref()
                .map(|p| p.publication.publication_id.clone())
        });
        if let Some(Some(old)) = previous
            .as_ref()
            .filter(|old| old.as_ref() != current.as_ref())
        {
            claimed.0.update(|set| {
                set.remove(old);
            });
        }
        if let Some(id) = current.clone() {
            claimed.0.update(|set| {
                set.insert(id);
            });
        }
        current
    });
    on_cleanup(move || {
        #[cfg(all(test, target_arch = "wasm32"))]
        log::debug!(
            "swarm composer unmounted reply={} draft_chars={} references={} pending={}",
            is_reply,
            text.with_untracked(|text| text.chars().count()),
            references.with_untracked(Vec::len),
            pending.with_untracked(Option::is_some)
        );
        if let Some(waiting) = pending.get_untracked() {
            claimed.0.update(|set| {
                set.remove(&waiting.publication.publication_id);
            });
        }
    });
    let publication_error = Memo::new(move |_| {
        let id = pending.with(|pending| {
            pending
                .as_ref()
                .map(|p| p.publication.publication_id.clone())
        })?;
        errors_signal.with(|errors| {
            errors
                .iter()
                .rev()
                .find(|entry| {
                    entry.host_id == host.get_value()
                        && entry.error.publication_id.as_ref() == Some(&id)
                        && entry.error.code
                            != protocol::SwarmErrorCode::CommittedDurabilityUncertain
                })
                .map(|entry| entry.error.message.clone())
        })
    });
    let clear_publication_errors = move |id: &SwarmPublicationId| {
        let host_id = host.get_value();
        errors_signal.update(|errors| {
            errors.retain(|entry| {
                !(entry.host_id == host_id
                    && entry.error.publication_id.as_ref() == Some(id)
                    && entry.error.code != protocol::SwarmErrorCode::CommittedDurabilityUncertain)
            })
        });
    };

    // The server's post record with our publication id is the only signal that
    // a publication happened; until then the text stays put.
    Effect::new(move |_| {
        let Some(waiting) = pending.get() else {
            return;
        };
        let published = posts_signal.with(|map| {
            map.get(&(host.get_value(), waiting.swarm_id.clone()))
                .is_some_and(|posts| {
                    posts
                        .posts
                        .values()
                        .any(|post| post.publication_id == waiting.publication.publication_id)
                })
        });
        if published {
            clear_publication_errors(&waiting.publication.publication_id);
            pending.set(None);
            text.set(String::new());
            references.set(Vec::new());
            attachments.set(Vec::new());
            send_error.set(None);
            if let Some(on_cancel) = on_cancel {
                on_cancel.run(());
            }
        }
    });

    let candidates = Memo::new(move |_| {
        let Some((_, query)) = completion.get() else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        swarm.with(|swarm| {
            swarm
                .as_ref()
                .map(|swarm| {
                    swarm
                        .members
                        .iter()
                        .enumerate()
                        .filter(|(_, member)| member.state != SwarmMemberState::Retired)
                        .filter(|(_, member)| member.spec.name.to_lowercase().starts_with(&query))
                        .map(|(index, member)| {
                            (
                                member.spec.id.clone(),
                                member.spec.name.clone(),
                                member_status_label(member),
                                format!(
                                    "{} · {}{}",
                                    backend_name(member.spec.backend_kind),
                                    member.spec.focus.as_deref().unwrap_or("Generalist"),
                                    if swarm
                                        .members
                                        .iter()
                                        .filter(|other| other.spec.name == member.spec.name)
                                        .count()
                                        > 1
                                    {
                                        format!(" · Member {}", index + 1)
                                    } else {
                                        String::new()
                                    }
                                ),
                            )
                        })
                        .take(8)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });

    let refresh_completion = move || {
        let Some(textarea) = textarea_ref.get_untracked() else {
            return;
        };
        let value = textarea.value();
        let caret = textarea
            .selection_start()
            .ok()
            .flatten()
            .map(|utf16| utf16_to_byte(&value, utf16))
            .unwrap_or(value.len());
        let next = mention_query(&value, caret);
        if next.as_ref().map(|(at, _)| *at) != completion.get_untracked().map(|(at, _)| at) {
            selected.set(0);
        }
        completion.set(next);
    };

    let insert_reference =
        move |start: usize, end: usize, label: String, segment: SwarmBodySegment| {
            if pending.get_untracked().is_some() {
                return;
            }
            let current = text.get_untracked();
            let insert = format!("{label} ");
            let next = format!("{}{}{}", &current[..start], insert, &current[end..]);
            let caret = byte_to_utf16(&next, start + insert.len());
            references.update(|list| {
                edit_references(list, start, end, insert.len());
                list.push(ComposerReference {
                    start,
                    end: start + label.len(),
                    segment,
                });
                list.sort_by_key(|reference| reference.start);
            });
            text.set(next);
            completion.set(None);
            input_range.set_value(None);
            if let Some(textarea) = textarea_ref.get_untracked() {
                request_animation_frame(move || {
                    let _ = textarea.focus();
                    let _ = textarea.set_selection_range(caret, caret);
                });
            }
        };
    let accept = move |member_id: SwarmMemberId, name: String| {
        let Some((at, query)) = completion.get_untracked() else {
            return;
        };
        insert_reference(
            at,
            at + 1 + query.len(),
            format!("@{name}"),
            SwarmBodySegment::MemberMention { member_id },
        );
    };

    let dispatch_pending = move |waiting: PendingPublication| {
        send_error.set(None);
        clear_publication_errors(&waiting.publication.publication_id);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::Post {
                swarm_id: waiting.swarm_id,
                publication: waiting.publication,
            },
            Some(Callback::new(move |message: String| {
                send_error.set(Some(message))
            })),
        );
    };

    let submit = move || {
        if pending.get_untracked().is_some() || !routing_available.get_untracked() {
            return;
        }
        let body_text = text.get_untracked();
        if body_text.trim().is_empty() {
            return;
        }
        let waiting = PendingPublication {
            swarm_id: sid.get_value(),
            publication: SwarmPublication {
                board: board.get_untracked(),
                publication_id: SwarmPublicationId(mint_id()),
                body: compose_segments(&body_text, &references.get_untracked()),
                thread_id: thread.get_value(),
                attachments: attachments.get_untracked(),
            },
        };
        pending.set(Some(waiting.clone()));
        completion.set(None);
        dispatch_pending(waiting);
    };

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let options = candidates.get_untracked();
        if completion.get_untracked().is_some() && !options.is_empty() {
            match ev.key().as_str() {
                "ArrowDown" => {
                    ev.prevent_default();
                    selected.update(|index| *index = (*index + 1) % options.len());
                    return;
                }
                "ArrowUp" => {
                    ev.prevent_default();
                    selected.update(|index| *index = (*index + options.len() - 1) % options.len());
                    return;
                }
                "Enter" | "Tab" => {
                    ev.prevent_default();
                    let (id, name, _, _) =
                        options[selected.get_untracked().min(options.len() - 1)].clone();
                    accept(id, name);
                    return;
                }
                "Escape" => {
                    ev.prevent_default();
                    ev.stop_propagation();
                    completion.set(None);
                    return;
                }
                _ => {}
            }
        }
        if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() {
            ev.prevent_default();
            submit();
        } else if ev.key() == "Escape"
            && pending.get_untracked().is_none()
            && let Some(on_cancel) = on_cancel
        {
            ev.stop_propagation();
            on_cancel.run(());
        }
    };

    let mention_chips = move || {
        let present = references
            .get()
            .into_iter()
            .filter_map(|reference| match reference.segment {
                SwarmBodySegment::MemberMention { member_id } => {
                    Some(swarm.with(|swarm| member_name(swarm.as_ref(), &member_id)))
                }
                SwarmBodySegment::PostLink { .. } | SwarmBodySegment::Text { .. } => None,
            })
            .collect::<Vec<_>>();
        (!present.is_empty()).then(|| view! {
            <div class="swarm-composer-mentions">
                <span class="swarm-composer-mentions-label">"Mentions"</span>
                {present.into_iter().map(|label| view! { <span class="swarm-mention-chip">{format!("@{label}")}</span> }).collect_view()}
            </div>
        })
    };
    let linkable = Memo::new(move |_| {
        posts_signal.with(|map| {
            let mut posts = map
                .get(&(host.get_value(), sid.get_value()))
                .map(|posts| posts.posts.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            posts.sort_by(|a, b| {
                b.created_at_ms
                    .cmp(&a.created_at_ms)
                    .then_with(|| a.id.0.cmp(&b.id.0))
            });
            posts
        })
    });
    let insert_post_link = move |post: SwarmPost| {
        let Some(textarea) = textarea_ref.get_untracked() else {
            return;
        };
        let current = text.get_untracked();
        let start = textarea
            .selection_start()
            .ok()
            .flatten()
            .map(|position| utf16_to_byte(&current, position));
        let end = textarea
            .selection_end()
            .ok()
            .flatten()
            .map(|position| utf16_to_byte(&current, position));
        let (Some(start), Some(end)) = (start, end) else {
            send_error.set(Some("Could not read the insertion position.".to_owned()));
            return;
        };
        let label = swarm.with_untracked(|swarm| {
            format!(
                "↪ {}: {}",
                author_name(swarm.as_ref(), &post.author),
                post_excerpt(&post, swarm.as_ref())
            )
        });
        insert_reference(
            start,
            end,
            label,
            SwarmBodySegment::PostLink { post_id: post.id },
        );
        link_open.set(false);
    };

    let routing_preview = move || {
        let Some(current) = swarm.get() else {
            return view! { <p class="swarm-routing-preview" role="status">"Notification preview unavailable: swarm state is missing."</p> }.into_any();
        };
        let waiting = pending.get();
        let target_board = waiting
            .as_ref()
            .map(|waiting| waiting.publication.board)
            .unwrap_or_else(|| board.get());
        let body = waiting
            .as_ref()
            .map(|waiting| waiting.publication.body.clone())
            .unwrap_or_else(|| compose_segments(&text.get(), &references.get()));
        let root_author = reply_root_author.get();
        if is_reply && root_author.is_none() {
            return view! { <p class="swarm-routing-preview" role="status">"Notification preview unavailable until the thread root is loaded."</p> }.into_any();
        }
        let recipients = protocol::swarm_publication_recipients(
            &current,
            &SwarmAuthor::Human,
            target_board,
            root_author.as_ref(),
            &body,
        );
        if recipients.is_empty() {
            return view! { <p class="swarm-routing-preview" role="status">"Shared context only — no members notified."</p> }.into_any();
        }
        let labels = recipients
            .into_iter()
            .map(|member_id| {
                match current
                    .members
                    .iter()
                    .enumerate()
                    .find(|(_, member)| member.spec.id == member_id)
                {
                    Some((index, member)) => match member.state {
                        SwarmMemberState::Retired
                        | SwarmMemberState::Retiring
                        | SwarmMemberState::RetiringReserved => format!(
                            "{} — undeliverable ({})",
                            recipient_name(&current, member, index + 1),
                            member_status_label(member).to_lowercase()
                        ),
                        SwarmMemberState::Proposed
                        | SwarmMemberState::Dormant
                        | SwarmMemberState::Reserved
                        | SwarmMemberState::Live
                        | SwarmMemberState::Failed => format!(
                            "{} ({})",
                            recipient_name(&current, member, index + 1),
                            member_status_label(member).to_lowercase()
                        ),
                    },
                    None => "Unknown recipient — delivery preview unavailable".to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let lifecycle_note = match current.lifecycle {
            SwarmLifecycle::Pausing => "Pausing; posting does not resume delivery.",
            SwarmLifecycle::Paused => "Paused; posting does not resume delivery.",
            SwarmLifecycle::AttentionRequired => {
                "Needs attention; posting does not resume delivery."
            }
            SwarmLifecycle::Running | SwarmLifecycle::Launching | SwarmLifecycle::Transitioning => {
                "Notification is not confirmation of delivery or execution."
            }
        };
        view! {
            <p class="swarm-routing-preview" role="status">
                <span>{format!("Notification recipients: {labels}.")}</span>
                <span class="swarm-routing-note">{lifecycle_note}</span>
            </p>
        }
        .into_any()
    };

    // Files the user already has open from this swarm's project.
    let attachable = Memo::new(move |_| {
        let Some(project_id) = swarm.with(|swarm| {
            swarm
                .as_ref()
                .map(|swarm| swarm.constraints.project_id.clone())
        }) else {
            return Vec::new();
        };
        let host_id = host.get_value();
        let mut files: Vec<SwarmAttachment> = open_files.with(|files| {
            files
                .keys()
                .filter(|key| key.host_id == host_id && key.project_id == project_id)
                .map(|key| SwarmAttachment {
                    project_id: key.project_id.clone(),
                    path: key.path.clone(),
                })
                .collect()
        });
        files.sort_by(|a, b| a.path.relative_path.cmp(&b.path.relative_path));
        files.dedup();
        files
    });

    let placeholder = move || {
        if is_reply {
            "Reply in thread…  (@ to mention, Enter to send, Esc to close)".to_owned()
        } else {
            format!(
                "Post to {}…  (@ to mention, Enter to send, Shift+Enter for newline)",
                board_label(board.get())
            )
        }
    };

    view! {
        <div class="swarm-composer" class:swarm-composer-reply=is_reply>
            <Show when=move || completion.get().is_some() && !candidates.with(|c| c.is_empty())>
                <ul class="swarm-mention-menu" role="listbox" aria-label="Mention a member">
                    {move || candidates.get().into_iter().enumerate().map(|(index, (id, name, status, detail))| {
                        let id_for_click = id.clone();
                        let name_for_click = name.clone();
                        view! {
                            <li
                                class="swarm-mention-option"
                                role="option"
                                title=id.0.clone()
                                aria-label=format!("{name} · {detail} · {}", id.0)
                                aria-selected=move || (selected.get() == index).to_string()
                                class:active=move || selected.get() == index
                                on:mousedown=move |ev: web_sys::MouseEvent| {
                                    ev.prevent_default();
                                    accept(id_for_click.clone(), name_for_click.clone());
                                }
                            >
                                <span class="swarm-avatar swarm-avatar-xs" style=avatar_style(&id.0) aria-hidden="true">{initials(&name)}</span>
                                <span class="swarm-mention-option-name">{name.clone()}</span>
                                <span class="swarm-mention-option-detail">{detail.clone()}</span>
                                <span class="swarm-mention-option-status">{status}</span>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            </Show>
            {mention_chips}
            {move || {
                let list = attachments.get();
                (!list.is_empty()).then(|| view! {
                    <div class="swarm-attachments swarm-composer-attachments">
                        {list.into_iter().map(|attachment| {
                            let label = attachment_name(&attachment.path);
                            let remove = attachment.clone();
                            view! {
                                <span class="swarm-attachment" title=attachment.path.relative_path.clone()>
                                    <span class="swarm-attachment-icon" aria-hidden="true">"📄"</span>
                                    {label.clone()}
                                    <button
                                        class="swarm-attachment-remove"
                                        aria-label=format!("Remove {label}")
                                        disabled=move || pending.get().is_some()
                                        on:click=move |_| attachments.update(|list| list.retain(|item| item != &remove))
                                    >
                                        "×"
                                    </button>
                                </span>
                            }
                        }).collect_view()}
                    </div>
                })
            }}
            <textarea
                class="swarm-composer-input"
                node_ref=textarea_ref
                rows=if is_reply { "2" } else { "3" }
                placeholder=placeholder
                aria-label=move || if is_reply { "Reply".to_owned() } else { format!("Post to {}", board_label(board.get())) }
                readonly=move || pending.get().is_some()
                prop:value=move || text.get()
                on:beforeinput=move |_| {
                    if let Some(textarea) = textarea_ref.get_untracked() {
                        let value = text.get_untracked();
                        input_range.set_value(textarea.selection_start().ok().flatten().zip(textarea.selection_end().ok().flatten()).map(|(start, end)| (utf16_to_byte(&value, start), utf16_to_byte(&value, end))));
                    }
                }
                on:input=move |ev| {
                    if pending.get_untracked().is_some() { return; }
                    let next = event_target_value(&ev);
                    let previous = text.get_untracked();
                    let captured = input_range.get_value();
                    input_range.set_value(None);
                    let precise = captured.and_then(|(mut start, mut end)| {
                        if next.len() < previous.len() - (end - start) {
                            let removed = previous.len() - next.len();
                            let caret = textarea_ref.get_untracked()?.selection_start().ok().flatten().map(|position| utf16_to_byte(&next, position))?;
                            if caret < start { start = caret; } else { end = start + removed; }
                        }
                        let inserted = next.len().checked_sub(previous.len() - (end - start))?;
                        (next.get(..start) == previous.get(..start) && next.get(start + inserted..) == previous.get(end..)).then_some((start, end, inserted))
                    });
                    let (start, end, inserted) = precise.unwrap_or_else(|| changed_text_range(&previous, &next));
                    references.update(|list| edit_references(list, start, end, inserted));
                    text.set(next);
                    refresh_completion();
                }
                on:click=move |_| refresh_completion()
                on:keydown=on_keydown
                on:blur=move |_| completion.set(None)
            ></textarea>
            {routing_preview}
            <div class="swarm-composer-footer">
                <div class="swarm-composer-status">
                    {move || match (pending.get().is_some(), publication_error.get()) {
                        (true, Some(message)) => Some(view! {
                            <span class="swarm-composer-error" role="alert">{format!("Not published: {message}")}</span>
                        }.into_any()),
                        (true, None) => Some(view! {
                            <span class="swarm-composer-pending" role="status">"Publishing — waiting for the host to record the post…"</span>
                        }.into_any()),
                        (false, _) => None,
                    }}
                    {move || send_error.get().map(|message| view! {
                        <span class="swarm-composer-error" role="alert">{format!("Not sent: {message}")}</span>
                    })}
                </div>
                <div class="swarm-composer-buttons">
                    <Show when=move || pending.get().is_none()>
                        <div class="swarm-attach-wrap">
                            <button
                                class="swarm-btn swarm-btn-quiet"
                                aria-haspopup="true"
                                aria-expanded=move || link_open.get().to_string()
                                on:click=move |_| link_open.update(|open| *open = !*open)
                            >
                                "Link a post"
                            </button>
                            <Show when=move || link_open.get()>
                                <ul class="swarm-attach-menu swarm-link-menu" aria-label="Choose a post to link">
                                    <Show when=move || linkable.with(Vec::is_empty)>
                                        <li class="swarm-field-help">"Read a board to choose a post."</li>
                                    </Show>
                                    {move || linkable.get().into_iter().map(|post| {
                                        let label = swarm.with(|swarm| format!("{} · {}: {}", board_label(post.board), author_name(swarm.as_ref(), &post.author), post_excerpt(&post, swarm.as_ref())));
                                        view! {
                                            <li>
                                                <button class="swarm-attach-option" on:click=move |_| insert_post_link(post.clone())>{label}</button>
                                            </li>
                                        }
                                    }).collect_view()}
                                </ul>
                            </Show>
                        </div>
                    </Show>
                    <Show when=move || pending.get().is_none() && !attachable.with(|files| files.is_empty())>
                        <div class="swarm-attach-wrap">
                            <button
                                class="swarm-btn swarm-btn-quiet"
                                aria-haspopup="listbox"
                                aria-expanded=move || attach_open.get().to_string()
                                disabled=move || attachments.with(|list| list.len() >= protocol::SWARM_MAX_ATTACHMENTS)
                                title=move || attachments
                                    .with(|list| list.len() >= protocol::SWARM_MAX_ATTACHMENTS)
                                    .then(|| format!("A post can carry at most {} attachments", protocol::SWARM_MAX_ATTACHMENTS))
                                on:click=move |_| attach_open.update(|open| *open = !*open)
                            >
                                "Attach open file"
                            </button>
                            <Show when=move || attach_open.get()>
                                <ul class="swarm-attach-menu" role="listbox">
                                    {move || attachable.get().into_iter().map(|attachment| {
                                        let label = attachment.path.relative_path.clone();
                                        let chosen = attachment.clone();
                                        view! {
                                            <li>
                                                <button
                                                    class="swarm-attach-option"
                                                    on:click=move |_| {
                                                        let chosen = chosen.clone();
                                                        attachments.update(|list| if !list.contains(&chosen) { list.push(chosen) });
                                                        attach_open.set(false);
                                                    }
                                                >
                                                    {label}
                                                </button>
                                            </li>
                                        }
                                    }).collect_view()}
                                </ul>
                            </Show>
                        </div>
                    </Show>
                    {move || pending.get().map(|waiting| {
                        let retry = waiting.clone();
                        view! {
                            <button class="swarm-btn" on:click=move |_| dispatch_pending(retry.clone())>"Retry"</button>
                            <button
                                class="swarm-btn swarm-btn-quiet"
                                on:click=move |_| {
                                    clear_publication_errors(&waiting.publication.publication_id);
                                    pending.set(None);
                                }
                            >
                                "Edit"
                            </button>
                        }
                    })}
                    {on_cancel.map(|on_cancel| view! {
                        <button
                            class="swarm-btn swarm-btn-quiet"
                            disabled=move || pending.get().is_some()
                            on:click=move |_| on_cancel.run(())
                        >
                            "Cancel"
                        </button>
                    })}
                    <button
                        class="swarm-btn swarm-btn-primary swarm-composer-send"
                        disabled=move || pending.get().is_some() || !routing_available.get() || text.with(|t| t.trim().is_empty())
                        on:click=move |_| submit()
                    >
                        {if is_reply { "Reply" } else { "Post" }}
                    </button>
                </div>
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod wasm_tests {
    use super::*;
    use std::cell::Cell;

    use crate::components::center_zone::CenterZone;
    use crate::components::swarms_panel::SwarmsPanel;
    use crate::state::{ConnectionStatus, FileResourceKey, OpenFile};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, Envelope, FrameKind, HostFilterId, LaunchProfileId,
        SWARM_DEFAULT_AGENT_WAKE_BUDGET, SwarmBackendAllocation, SwarmBoardNotifyPayload,
        SwarmBoardPage, SwarmBoardPosition, SwarmConstraints, SwarmCursorTarget, SwarmErrorCode,
        SwarmErrorNotifyPayload, SwarmMemberSpec, SwarmNotification, SwarmNotifyPayload,
        SwarmPostNotifyPayload, SwarmRoundId, SwarmThreadNotifyPayload, SwarmThreadPage,
    };
    use serde_json::{Value, json};
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const PROD_STYLES: &str = include_str!("../../styles.css");
    pub(crate) const PROJECT: &str = "proj-swarm-view";

    pub(crate) fn ensure_styles_loaded() {
        let document = web_sys::window().unwrap().document().unwrap();
        if document
            .get_element_by_id("test-prod-styles-swarm-view")
            .is_none()
        {
            let style = document.create_element("style").unwrap();
            style.set_id("test-prod-styles-swarm-view");
            style.set_text_content(Some(PROD_STYLES));
            document.head().unwrap().append_child(&style).unwrap();
        }
    }

    pub(crate) fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        container
            .set_attribute(
                "style",
                "position: fixed; top: 0; left: 0; width: 1100px; height: 800px;",
            )
            .unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    pub(crate) async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    pub(crate) async fn next_frame() {
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            web_sys::window()
                .unwrap()
                .request_animation_frame(&resolve)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Reactive effects, spawned sends and animation-frame focus all flushed.
    pub(crate) async fn settle() {
        next_tick().await;
        next_tick().await;
        next_frame().await;
        next_tick().await;
    }

    pub(crate) fn install_send_stub() -> js_sys::Array {
        let code = r#"
            (function() {
                window.__test_send_calls = [];
                window.__TAURI__ = window.__TAURI__ || {};
                window.__TAURI__.core = window.__TAURI__.core || {};
                window.__TAURI__.core.invoke = function(cmd, args) {
                    window.__test_send_calls.push([cmd, JSON.stringify(args || {})]);
                    return Promise.resolve();
                };
                window.__TAURI__.event = window.__TAURI__.event || {};
                window.__TAURI__.event.listen = function() { return Promise.resolve(null); };
                return window.__test_send_calls;
            })();
        "#;
        js_sys::eval(code)
            .expect("install tauri stub")
            .dyn_into::<js_sys::Array>()
            .expect("array")
    }

    /// A host connection driven only by real protocol envelopes; every swarm
    /// command the UI sends is read back from the recorded host lines.
    pub(crate) struct Harness {
        pub(crate) state: AppState,
        pub(crate) host: String,
        seq: Cell<u64>,
        calls: js_sys::Array,
    }

    impl Harness {
        pub(crate) fn new(host: &str) -> Self {
            let calls = install_send_stub();
            let state = AppState::new();
            state.selected_host_id.set(Some(host.to_owned()));
            state.host_streams.update(|streams| {
                streams.insert(host.to_owned(), StreamPath(format!("/host/{host}")));
            });
            state.connection_statuses.update(|statuses| {
                statuses.insert(host.to_owned(), ConnectionStatus::Connected);
            });
            crate::dispatch::prime_host_for_tests(&state, host);
            Self {
                state,
                host: host.to_owned(),
                seq: Cell::new(0),
                calls,
            }
        }

        pub(crate) fn emit<T: serde::Serialize>(&self, kind: FrameKind, payload: &T) {
            let seq = self.seq.get();
            self.seq.set(seq + 1);
            let envelope = Envelope::from_payload(
                StreamPath(format!("/host/{}", self.host)),
                kind,
                seq,
                payload,
            )
            .expect("envelope");
            crate::dispatch::dispatch_envelope(&self.state, &self.host, envelope);
        }

        pub(crate) fn swarm(&self, swarm: &Swarm) {
            self.emit(
                FrameKind::SwarmNotify,
                &SwarmNotifyPayload {
                    swarm: swarm.clone(),
                },
            );
        }

        pub(crate) fn post(&self, post: &SwarmPost) {
            self.emit(
                FrameKind::SwarmPostNotify,
                &SwarmPostNotifyPayload { post: post.clone() },
            );
        }

        pub(crate) fn board_page(&self, page: SwarmBoardPage) {
            self.emit(
                FrameKind::SwarmBoardNotify,
                &SwarmBoardNotifyPayload { page },
            );
        }

        pub(crate) fn thread_page(&self, page: SwarmThreadPage) {
            self.emit(
                FrameKind::SwarmThreadNotify,
                &SwarmThreadNotifyPayload { page },
            );
        }

        pub(crate) fn error(
            &self,
            swarm_id: &str,
            publication: Option<&SwarmPublicationId>,
            code: SwarmErrorCode,
            message: &str,
        ) {
            self.emit(
                FrameKind::SwarmErrorNotify,
                &SwarmErrorNotifyPayload {
                    publication_id: publication.cloned(),
                    swarm_id: Some(SwarmId(swarm_id.to_owned())),
                    draft_id: None,
                    code,
                    message: message.to_owned(),
                },
            );
        }

        /// Every `swarm_command` payload sent so far, oldest first.
        pub(crate) fn commands(&self) -> Vec<Value> {
            let mut out = Vec::new();
            for entry in self.calls.iter() {
                let entry = entry.dyn_into::<js_sys::Array>().expect("entry");
                if entry.get(0).as_string().as_deref() != Some("send_host_line") {
                    continue;
                }
                let args: Value = serde_json::from_str(&entry.get(1).as_string().expect("args"))
                    .expect("args json");
                let envelope: Value =
                    serde_json::from_str(args["line"].as_str().expect("line")).expect("envelope");
                if envelope["kind"] == "swarm_command" {
                    out.push(envelope["payload"].clone());
                }
            }
            out
        }

        pub(crate) fn commands_of(&self, kind: &str) -> Vec<Value> {
            self.commands()
                .into_iter()
                .filter(|command| command["kind"] == kind)
                .collect()
        }
    }

    pub(crate) fn spec(id: &str, name: &str) -> SwarmMemberSpec {
        SwarmMemberSpec {
            session_settings: Default::default(),
            id: SwarmMemberId(id.to_owned()),
            name: name.to_owned(),
            focus: None,
            backend_kind: BackendKind::Claude,
            launch_profile_id: LaunchProfileId("claude-default".to_owned()),
            project_id: ProjectId(PROJECT.to_owned()),
            pinned: false,
        }
    }

    pub(crate) fn member(
        id: &str,
        name: &str,
        state: SwarmMemberState,
        agent: Option<&str>,
        status: Option<AgentControlStatus>,
    ) -> SwarmMember {
        SwarmMember {
            spec: spec(id, name),
            state,
            agent_id: agent.map(|agent| AgentId(agent.to_owned())),
            session_id: None,
            runtime_status: status,
            context_cursor: 0,
            current_round_id: None,
            error: None,
        }
    }

    pub(crate) fn idle(id: &str, name: &str) -> SwarmMember {
        member(
            id,
            name,
            SwarmMemberState::Live,
            Some(&format!("agent-{id}")),
            Some(AgentControlStatus::Idle),
        )
    }

    pub(crate) fn make_swarm(id: &str, name: &str, members: Vec<SwarmMember>) -> Swarm {
        Swarm {
            recovery_requirement: protocol::SwarmRecoveryRequirement::None,
            change_preview_revision: 0,
            source_draft_id: None,
            id: SwarmId(id.to_owned()),
            host_id: HostFilterId("local".to_owned()),
            name: name.to_owned(),
            revision: 1,
            constraints: SwarmConstraints {
                project_id: ProjectId(PROJECT.to_owned()),
                workspace_policy: SwarmWorkspacePolicy::ReadOnly,
                max_live_agents: 4,
                allocations: vec![SwarmBackendAllocation {
                    session_settings: Default::default(),
                    backend_kind: BackendKind::Claude,
                    launch_profile_id: LaunchProfileId("claude-default".to_owned()),
                    count: 2,
                }],
                shared_guidance: String::new(),
                agent_wake_budget: SWARM_DEFAULT_AGENT_WAKE_BUDGET,
            },
            lifecycle: SwarmLifecycle::Running,
            members,
            opening_post_id: None,
            board_positions: [SwarmBoard::Briefing, SwarmBoard::Coordination]
                .into_iter()
                .map(|board| SwarmBoardPosition {
                    board,
                    high_water: 0,
                    human_read_cursor: 0,
                    unread_count: 0,
                })
                .collect(),
            notifications: Vec::new(),
            rounds: Vec::new(),
            change_preview: None,
            error: None,
            legacy_team_id: None,
        }
    }

    pub(crate) fn text(value: &str) -> SwarmBodySegment {
        SwarmBodySegment::Text {
            text: value.to_owned(),
        }
    }

    pub(crate) fn mention(member_id: &str) -> SwarmBodySegment {
        SwarmBodySegment::MemberMention {
            member_id: SwarmMemberId(member_id.to_owned()),
        }
    }

    pub(crate) fn make_post(
        swarm_id: &str,
        id: &str,
        thread: &str,
        board: SwarmBoard,
        cursor: u64,
        author: SwarmAuthor,
        body: Vec<SwarmBodySegment>,
    ) -> SwarmPost {
        SwarmPost {
            id: SwarmPostId(id.to_owned()),
            swarm_id: SwarmId(swarm_id.to_owned()),
            thread_id: SwarmThreadId(thread.to_owned()),
            board,
            cursor,
            author,
            publication_id: SwarmPublicationId(format!("pub-{id}")),
            body,
            attachments: Vec::new(),
            round_id: SwarmRoundId("round-1".to_owned()),
            created_at_ms: 1_700_000_000_000 + cursor,
        }
    }

    pub(crate) fn by(member_id: &str) -> SwarmAuthor {
        SwarmAuthor::Member {
            member_id: SwarmMemberId(member_id.to_owned()),
        }
    }

    pub(crate) fn board_cursor(
        swarm_id: &str,
        board: SwarmBoard,
        position: u64,
        high_water: u64,
    ) -> SwarmReadCursor {
        SwarmReadCursor {
            swarm_id: SwarmId(swarm_id.to_owned()),
            target: SwarmCursorTarget::Board { board },
            position,
            snapshot_high_water: high_water,
        }
    }

    pub(crate) fn thread_cursor(
        swarm_id: &str,
        thread: &str,
        position: u64,
        high_water: u64,
    ) -> SwarmReadCursor {
        SwarmReadCursor {
            swarm_id: SwarmId(swarm_id.to_owned()),
            target: SwarmCursorTarget::Thread {
                thread_id: SwarmThreadId(thread.to_owned()),
            },
            position,
            snapshot_high_water: high_water,
        }
    }

    pub(crate) fn page(
        swarm_id: &str,
        board: SwarmBoard,
        posts: Vec<SwarmPost>,
        high_water: u64,
        has_more: bool,
    ) -> SwarmBoardPage {
        let position = posts.iter().map(|post| post.cursor).max().unwrap_or(0);
        SwarmBoardPage {
            swarm_id: SwarmId(swarm_id.to_owned()),
            board,
            posts,
            next_cursor: board_cursor(swarm_id, board, position, high_water),
            high_water,
            has_more,
        }
    }

    pub(crate) fn mount_view(harness: &Harness, swarm_id: &str) -> (HtmlElement, impl Sized) {
        ensure_styles_loaded();
        let container = make_container();
        let state = harness.state.clone();
        let host = harness.host.clone();
        let swarm_id = SwarmId(swarm_id.to_owned());
        let handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SwarmView host_id=host.clone() swarm_id=swarm_id.clone() visible=Signal::derive(|| true) /> }
        });
        (container, handle)
    }

    pub(crate) fn all(root: &web_sys::Element, selector: &str) -> Vec<HtmlElement> {
        let list = root.query_selector_all(selector).expect("selector");
        (0..list.length())
            .filter_map(|index| list.item(index))
            .map(|node| node.dyn_into::<HtmlElement>().expect("html element"))
            .collect()
    }

    pub(crate) fn one(root: &web_sys::Element, selector: &str) -> HtmlElement {
        root.query_selector(selector)
            .expect("selector")
            .unwrap_or_else(|| panic!("{selector} not rendered"))
            .dyn_into::<HtmlElement>()
            .expect("html element")
    }

    pub(crate) fn text_of(element: &web_sys::Element) -> String {
        element.text_content().unwrap_or_default()
    }

    pub(crate) fn button(root: &web_sys::Element, label: &str) -> HtmlElement {
        all(root, "button")
            .into_iter()
            .find(|button| text_of(button).trim() == label)
            .unwrap_or_else(|| panic!("button {label:?} not rendered; text: {}", text_of(root)))
    }

    pub(crate) fn has_button(root: &web_sys::Element, label: &str) -> bool {
        all(root, "button")
            .iter()
            .any(|button| text_of(button).trim() == label)
    }

    /// Replace a text control's value with the caret at the end, as typing does.
    pub(crate) fn type_into(control: &HtmlElement, value: &str) {
        if let Some(textarea) = control.dyn_ref::<web_sys::HtmlTextAreaElement>() {
            textarea.set_value(value);
            let length = value.encode_utf16().count() as u32;
            textarea.set_selection_range(length, length).unwrap();
        } else {
            control
                .dyn_ref::<web_sys::HtmlInputElement>()
                .expect("text control")
                .set_value(value);
        }
        control
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    fn replace_selection(control: &HtmlElement, start: usize, end: usize, replacement: &str) {
        let textarea = control.dyn_ref::<web_sys::HtmlTextAreaElement>().unwrap();
        let current = textarea.value();
        textarea
            .set_selection_range(byte_to_utf16(&current, start), byte_to_utf16(&current, end))
            .unwrap();
        control
            .dispatch_event(&web_sys::Event::new("beforeinput").unwrap())
            .unwrap();
        let next = format!("{}{}{}", &current[..start], replacement, &current[end..]);
        textarea.set_value(&next);
        let caret = byte_to_utf16(&next, start + replacement.len());
        textarea.set_selection_range(caret, caret).unwrap();
        control
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    pub(crate) fn press(target: &HtmlElement, key: &str) {
        press_with(target, key, false);
    }

    pub(crate) fn press_with(target: &HtmlElement, key: &str, shift: bool) {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(key);
        init.set_shift_key(shift);
        init.set_bubbles(true);
        init.set_cancelable(true);
        target
            .dispatch_event(
                &web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init)
                    .unwrap(),
            )
            .unwrap();
    }

    /// The swarm opens as its own center tab from the dock card — no agent is
    /// selected for it — and a member chip drills into that member's chat.
    #[wasm_bindgen_test]
    async fn swarm_card_opens_board_tab_and_member_drilldown_without_status_inference() {
        let harness = Harness::new("host-swarm-tab");
        let swarm = make_swarm(
            "sw-tab",
            "Release crew",
            vec![
                idle("ada", "Ada"),
                member("bo", "Bo", SwarmMemberState::Live, Some("agent-bo"), None),
                member("cy", "Cy", SwarmMemberState::Proposed, None, None),
            ],
        );
        harness.swarm(&swarm);
        ensure_styles_loaded();
        let container = make_container();
        let state = harness.state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state.clone());
            view! { <SwarmsPanel /> <CenterZone /> }
        });
        settle().await;

        assert!(text_of(&container).contains("Release crew"));
        harness
            .state
            .open_tab(TabContent::empty_chat(), "Personal draft".to_owned(), true);
        settle().await;
        let personal_draft = one(&container, "textarea");
        type_into(&personal_draft, "Keep this private draft separate");
        let mut announced =
            crate::dispatch::restore_fixtures::restore_agent_payload("agent-bo", None);
        announced.name = "Bo".to_owned();
        announced.origin = protocol::AgentOrigin::SwarmMember;
        announced.swarm_membership = Some(protocol::SwarmMembership {
            swarm_id: swarm.id.clone(),
            member_id: SwarmMemberId("bo".to_owned()),
        });
        harness.emit(FrameKind::NewAgent, &announced);
        settle().await;
        assert!(
            text_of(&container).contains("Personal draft"),
            "a swarm launch must not rename an unrelated draft tab"
        );
        assert!(
            personal_draft.is_same_node(Some(&one(&container, "textarea"))),
            "the unrelated visible draft must not be replaced by a member stream"
        );
        assert!(
            personal_draft
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value()
                == "Keep this private draft separate"
        );
        assert_eq!(
            harness.state.active_agent.get_untracked(),
            None,
            "a swarm member announcement must not consume ordinary chat intent"
        );
        one(&container, ".swarm-card-open").click();
        settle().await;

        let expected = TabContent::Swarm {
            host_id: harness.host.clone(),
            swarm_id: SwarmId("sw-tab".into()),
        };
        assert_eq!(
            harness
                .state
                .center_zone
                .with_untracked(|cz| cz.active_content().cloned()),
            Some(expected.clone())
        );
        assert_eq!(
            harness.state.active_agent.get_untracked(),
            None,
            "a swarm tab selects no agent"
        );
        let view = one(&container, ".swarm-view");
        assert_eq!(text_of(&one(&view, ".swarm-title")), "Release crew");
        assert_eq!(
            text_of(&one(&view, ".swarm-scope-label")),
            "Read-only project access"
        );
        let reads = harness.commands_of("read_board");
        assert_eq!(reads.len(), 1, "first page requested once: {reads:?}");
        assert_eq!(reads[0]["query"]["board"], "briefing");
        assert_eq!(reads[0]["query"]["after_cursor"], Value::Null);

        let chips = all(&view, ".swarm-member-chip");
        let status = |name: &str| {
            chips
                .iter()
                .find(|chip| text_of(&one(chip, ".swarm-member-chip-name")) == name)
                .map(|chip| text_of(&one(chip, ".swarm-member-chip-status")))
                .unwrap_or_else(|| panic!("chip {name} missing"))
        };
        assert_eq!(status("Ada"), "Idle");
        assert_eq!(
            status("Bo"),
            "Status unavailable",
            "missing runtime status is never shown as idle"
        );
        assert_eq!(status("Cy"), "Not started");
        assert!(
            chips
                .iter()
                .all(|chip| text_of(chip).contains("Cy") == (chip.tag_name() == "DIV")),
            "only members with a conversation are clickable"
        );

        chips
            .iter()
            .find(|chip| text_of(chip).contains("Ada"))
            .unwrap()
            .click();
        settle().await;
        assert_eq!(
            harness.state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                host_id: harness.host.clone(),
                agent_id: AgentId("agent-ada".into())
            })
        );
        assert_eq!(
            harness
                .state
                .center_zone
                .with_untracked(|cz| cz.occurrences(&expected).len()),
            1,
            "the swarm tab stays open beside the member chat"
        );

        one(&container, ".swarm-card-open").click();
        settle().await;
        assert!(text_of(&one(&container, ".swarm-subtitle")).contains("3 members · 0 working"));
        assert_eq!(
            text_of(&one(&container, ".swarm-card-meta")),
            "3 members · 0 working"
        );
        let mut draft = protocol::SwarmDraft {
            legacy_source: None,
            retained_sessions: Default::default(),
            id: protocol::SwarmDraftId("draft-count-copy".to_owned()),
            revision: 1,
            name: "Reviewed crew".to_owned(),
            opening_brief: String::new(),
            constraints: swarm.constraints.clone(),
            members: swarm
                .members
                .iter()
                .map(|member| member.spec.clone())
                .collect(),
            conflicts: Vec::new(),
            generation: protocol::SwarmDraftGeneration::DeterministicGeneralists,
            legacy_team_id: None,
        };
        draft.constraints.allocations[0].count = 3;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &protocol::SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(draft.clone()),
            },
        );
        settle().await;
        assert_eq!(
            text_of(&one(&container, ".swarm-draft-meta")),
            "Draft · 3 members"
        );

        let mut retired_peers = swarm.clone();
        retired_peers.revision += 1;
        for member in retired_peers.members.iter_mut().skip(1) {
            member.state = SwarmMemberState::Retired;
            member.agent_id = None;
            member.runtime_status = None;
        }
        harness.swarm(&retired_peers);
        draft.revision += 1;
        draft.members.truncate(1);
        draft.constraints.allocations[0].count = 1;
        harness.emit(
            FrameKind::SwarmDraftNotify,
            &protocol::SwarmDraftNotifyPayload::Upsert {
                draft: Box::new(draft.clone()),
            },
        );
        settle().await;
        // The live one-member UI displayed "1 members"; canonical counts must
        // stay unchanged while all three visible labels react to singular wording.
        assert!(text_of(&one(&container, ".swarm-subtitle")).contains("1 member · 0 working"));
        assert_eq!(
            text_of(&one(&container, ".swarm-card-meta")),
            "1 member · 0 working"
        );
        assert_eq!(
            text_of(&one(&container, ".swarm-draft-meta")),
            "Draft · 1 member"
        );
        for (conflicts, expected) in [
            (vec!["Scope conflict"], "Draft · 1 member · 1 conflict"),
            (
                vec!["Scope conflict", "Capacity conflict"],
                "Draft · 1 member · 2 conflicts",
            ),
        ] {
            draft.revision += 1;
            draft.conflicts = conflicts.into_iter().map(str::to_owned).collect();
            harness.emit(
                FrameKind::SwarmDraftNotify,
                &protocol::SwarmDraftNotifyPayload::Upsert {
                    draft: Box::new(draft.clone()),
                },
            );
            settle().await;
            assert_eq!(text_of(&one(&container, ".swarm-draft-meta")), expected);
        }
    }

    /// Paging submits the server's cursor unchanged, live posts land in their
    /// thread under the root, and switching boards keeps loaded threads.
    #[wasm_bindgen_test]
    async fn board_threads_keep_root_and_replies_while_paging_and_live_posts_arrive() {
        let harness = Harness::new("host-swarm-threads");
        let sid = "sw-threads";
        harness.swarm(&make_swarm(
            sid,
            "Threads",
            vec![idle("ada", "Ada"), idle("bo", "Bo")],
        ));
        let (container, _handle) = mount_view(&harness, sid);
        settle().await;
        assert!(text_of(&container).contains("Loading board…"));

        let root = make_post(
            sid,
            "r1",
            "r1",
            SwarmBoard::Briefing,
            1,
            SwarmAuthor::Human,
            vec![text("Ship the **docs**")],
        );
        let first = page(sid, SwarmBoard::Briefing, vec![root.clone()], 2, true);
        let first_cursor = first.next_cursor.clone();
        harness.board_page(first);
        settle().await;
        let reads = harness.commands_of("read_board");
        assert_eq!(reads.len(), 2, "{reads:?}");
        assert_eq!(
            reads[1]["query"]["after_cursor"],
            serde_json::to_value(&first_cursor).unwrap(),
            "continuation submits the page's cursor unchanged"
        );

        let reply = make_post(
            sid,
            "p2",
            "r1",
            SwarmBoard::Briefing,
            2,
            by("ada"),
            vec![text("Drafting the outline")],
        );
        harness.board_page(page(sid, SwarmBoard::Briefing, vec![reply], 2, false));
        settle().await;
        assert_eq!(
            harness.commands_of("read_board").len(),
            2,
            "a complete board is not re-read"
        );

        let attachment_path = ProjectPath {
            root: protocol::ProjectRootPath("/tmp/swarm-view".to_owned()),
            relative_path: "notes.md".to_owned(),
        };
        harness.state.open_files.update(|files| {
            files.insert(
                FileResourceKey {
                    host_id: harness.host.clone(),
                    project_id: ProjectId(PROJECT.to_owned()),
                    path: attachment_path.clone(),
                },
                OpenFile {
                    path: attachment_path.clone(),
                    version: protocol::ProjectFileVersion(1),
                    contents: Some(String::new()),
                    is_binary: false,
                    missing: false,
                },
            );
        });
        let editing_thread = one(&container, "[data-thread-id='r1']");
        button(&editing_thread, "Reply").click();
        settle().await;
        let composer = one(&editing_thread, ".swarm-reply-composer");
        let draft_input = one(&composer, "textarea");
        assert_eq!(
            text_of(&one(&composer, ".swarm-routing-preview")),
            "Shared context only — no members notified.",
            "a reply to a human root is not a Briefing broadcast"
        );
        draft_input.focus().unwrap();
        type_into(&draft_input, "Draft @B");
        settle().await;
        press(&draft_input, "Enter");
        settle().await;
        type_into(&draft_input, "Draft @Bo please review");
        button(&composer, "Attach open file").click();
        settle().await;
        button(&composer, "notes.md").click();
        draft_input.focus().unwrap();
        settle().await;

        let coordination_tab = one(&container, "[data-board='Coordination']");
        coordination_tab.focus().unwrap();
        coordination_tab.click();
        settle().await;
        assert_eq!(editing_thread.get_bounding_client_rect().height(), 0.0);
        assert!(
            draft_input.is_same_node(Some(&one(
                &container,
                "[data-thread-id='r1'] .swarm-reply-composer textarea"
            ))),
            "switching boards hides the thread without disposing its reply editor"
        );
        draft_input.focus().unwrap();
        assert!(
            coordination_tab.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            ),
            "a hidden reply composer cannot take focus from the active board tab"
        );
        harness.board_page(page(sid, SwarmBoard::Coordination, Vec::new(), 0, false));
        settle().await;
        assert_eq!(
            text_of(&one(&container, ".swarm-empty")),
            "Members haven't coordinated here yet.",
            "retained hidden Briefing threads do not suppress Coordination's empty state"
        );
        let coordination_input = one(&container, ".swarm-root-composer:not([hidden]) textarea");
        coordination_input.focus().unwrap();

        harness.post(&make_post(
            sid,
            "p3",
            "r1",
            SwarmBoard::Briefing,
            3,
            by("bo"),
            vec![text("Taking the API section")],
        ));
        settle().await;
        assert!(
            coordination_input.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            ),
            "a live reply to a hidden thread must not focus its preserved composer"
        );
        assert_eq!(editing_thread.get_bounding_client_rect().height(), 0.0);
        press(&coordination_tab, "ArrowLeft");
        settle().await;
        assert!(editing_thread.get_bounding_client_rect().height() > 0.0);
        assert!(
            draft_input.is_same_node(Some(&one(
                &container,
                "[data-thread-id='r1'] .swarm-reply-composer textarea"
            ))),
            "returning to Briefing restores the SAME reply editor after a hidden live update"
        );
        assert!(
            draft_input
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value()
                == "Draft @Bo please review",
            "unsent reply text survives switching away and back"
        );
        assert_eq!(all(&composer, ".swarm-mention-chip").len(), 1);
        assert!(text_of(&composer).contains("notes.md"));
        draft_input.focus().unwrap();
        harness.post(&make_post(
            sid,
            "r4",
            "r4",
            SwarmBoard::Briefing,
            4,
            SwarmAuthor::Human,
            vec![text("Second topic")],
        ));
        settle().await;

        let current_input = one(
            &container,
            "[data-thread-id='r1'] .swarm-reply-composer textarea",
        );
        assert!(
            draft_input.is_same_node(Some(&current_input)),
            "live reply does not replace the editor DOM node"
        );
        assert_eq!(
            draft_input
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "Draft @Bo please review"
        );
        assert!(
            draft_input.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            ),
            "live reply preserves editor focus"
        );
        assert!(text_of(&composer).contains("@Bo"));
        assert!(text_of(&composer).contains("notes.md"));
        button(&composer, "Reply").click();
        settle().await;
        let publications = harness.commands_of("post");
        assert_eq!(publications.len(), 1);
        let publication = publications[0]["publication"].clone();
        assert!(
            publication["body"]
                == json!([{"type":"text","text":"Draft "}, {"type":"member_mention","member_id":"bo"}, {"type":"text","text":" please review"}]),
            "live reply preserves the selected member occurrence"
        );
        assert!(
            publication["attachments"]
                == json!([{ "project_id": PROJECT, "path": attachment_path }]),
            "live reply preserves attachments"
        );
        assert_eq!(publication["thread_id"], "r1");
        assert_eq!(publication["board"], "briefing");

        let threads = all(&container, ".swarm-thread");
        assert_eq!(threads.len(), 2);
        let first_thread = &threads[0];
        let ids: Vec<String> = all(first_thread, ".swarm-post")
            .iter()
            .map(|post| post.get_attribute("data-post-id").unwrap())
            .collect();
        assert_eq!(
            ids,
            ["r1", "p2", "p3"],
            "root first, then replies in board order"
        );
        assert_eq!(
            text_of(&one(first_thread, ".swarm-post .swarm-post-author")),
            "You"
        );
        assert!(text_of(first_thread).contains("Drafting the outline"));
        assert_eq!(text_of(&one(&container, "#swarm-post-r1 strong")), "docs");
        assert_eq!(
            threads[1].get_attribute("data-thread-id").as_deref(),
            Some("r4")
        );

        button(first_thread, "Load full thread").click();
        settle().await;
        let thread_reads = harness.commands_of("read_thread");
        assert_eq!(thread_reads[0]["query"]["thread_id"], "r1");
        assert_eq!(thread_reads[0]["query"]["after_cursor"], Value::Null);
        let continuation = thread_cursor(sid, "r1", 3, 3);
        harness.thread_page(SwarmThreadPage {
            swarm_id: SwarmId(sid.into()),
            thread_id: SwarmThreadId("r1".into()),
            root: root.clone(),
            posts: Vec::new(),
            next_cursor: continuation.clone(),
            high_water: 3,
            has_more: true,
        });
        settle().await;
        let preserved_input = one(
            &container,
            "[data-thread-id='r1'] .swarm-reply-composer textarea",
        );
        assert!(
            draft_input.is_same_node(Some(&preserved_input)),
            "thread page keeps pending composer mounted"
        );
        assert!(
            preserved_input
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .read_only()
        );
        let publication_id =
            SwarmPublicationId(publication["publication_id"].as_str().unwrap().to_owned());
        harness.error(
            sid,
            Some(&publication_id),
            SwarmErrorCode::Storage,
            "Publication temporarily unavailable",
        );
        settle().await;
        assert!(text_of(&composer).contains("Not published: Publication temporarily unavailable"));
        assert!(
            all(&container, ".swarm-server-error").is_empty(),
            "pending reply still owns its error after page updates"
        );
        button(&composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post")[1]["publication"] == publication,
            "page update preserves full pending publication identity and content"
        );
        let first_thread = &all(&container, ".swarm-thread")[0];
        button(first_thread, "Load more replies").click();
        settle().await;
        assert_eq!(
            harness.commands_of("read_thread")[1]["query"]["after_cursor"],
            serde_json::to_value(&continuation).unwrap()
        );

        one(&container, "[data-board='Coordination']").click();
        settle().await;
        let reads = harness.commands_of("read_board");
        assert_eq!(reads.last().unwrap()["query"]["board"], "coordination");
        // Loaded threads now retain hidden draft owners; the board contract
        // counts visible threads, not these deliberately preserved DOM nodes.
        assert_eq!(all(&container, ".swarm-thread:not([hidden])").len(), 0);
        assert_eq!(editing_thread.get_bounding_client_rect().height(), 0.0);
        assert!(
            draft_input.is_same_node(Some(&one(
                &container,
                "[data-thread-id='r1'] .swarm-reply-composer textarea"
            ))) && draft_input
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .read_only(),
            "switching boards also retains the pending reply publication and mounted editor"
        );
        harness.board_page(page(sid, SwarmBoard::Coordination, Vec::new(), 0, false));
        settle().await;
        assert!(text_of(&container).contains("Members haven't coordinated here yet."));
        let briefing_tab = one(&container, "[data-board='Briefing']");
        let coordination_tab = one(&container, "[data-board='Coordination']");
        let panel = one(&container, "[role='tabpanel']");
        assert!(briefing_tab.id() != coordination_tab.id());
        assert!(
            briefing_tab.get_attribute("aria-controls").as_deref() == Some(panel.id().as_str())
        );
        assert!(
            coordination_tab.get_attribute("aria-controls").as_deref() == Some(panel.id().as_str())
        );
        assert!(
            panel.get_attribute("aria-labelledby").as_deref()
                == Some(coordination_tab.id().as_str())
        );
        assert_eq!(briefing_tab.tab_index(), -1);
        assert_eq!(coordination_tab.tab_index(), 0);
        coordination_tab.focus().unwrap();
        for (key, target, expected) in [
            ("ArrowLeft", &coordination_tab, &briefing_tab),
            ("End", &briefing_tab, &coordination_tab),
            ("ArrowRight", &coordination_tab, &briefing_tab),
            ("ArrowLeft", &briefing_tab, &coordination_tab),
            ("Home", &coordination_tab, &briefing_tab),
        ] {
            press(target, key);
            settle().await;
            assert_eq!(
                expected.get_attribute("aria-selected").as_deref(),
                Some("true")
            );
            assert_eq!(expected.tab_index(), 0);
            let other = if expected.is_same_node(Some(&briefing_tab)) {
                &coordination_tab
            } else {
                &briefing_tab
            };
            assert_eq!(other.tab_index(), -1);
            assert!(
                expected.is_same_node(
                    web_sys::window()
                        .unwrap()
                        .document()
                        .unwrap()
                        .active_element()
                        .as_ref()
                        .map(|element| element.as_ref())
                ),
                "board keyboard navigation moves focus with selection"
            );
            assert!(
                panel.get_attribute("aria-labelledby").as_deref() == Some(expected.id().as_str())
            );
        }

        one(&container, "[data-board='Briefing']").click();
        settle().await;
        assert_eq!(
            all(&container, ".swarm-thread").len(),
            2,
            "loaded threads survive a board switch"
        );
        assert_eq!(
            all(&all(&container, ".swarm-thread")[0], ".swarm-post").len(),
            3
        );
        assert!(
            draft_input.is_same_node(Some(&one(
                &container,
                "[data-thread-id='r1'] .swarm-reply-composer textarea"
            ))),
            "pending reply editor identity survives repeated keyboard board switches"
        );
        button(&composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["publication"] == publication,
            "switching back preserves the exact pending reply ID, fixed board, typed mention, and attachments"
        );

        let orphan_reply = make_post(
            sid,
            "orphan-reply",
            "orphan-root",
            SwarmBoard::Briefing,
            6,
            by("bo"),
            vec![text("Earlier discussion reply")],
        );
        harness.post(&orphan_reply);
        settle().await;
        let orphan_thread = one(&container, "[data-thread-id='orphan-root']");
        button(&orphan_thread, "Reply").click();
        settle().await;
        let orphan_composer = one(&orphan_thread, ".swarm-reply-composer");
        let orphan_input = one(&orphan_composer, "textarea");
        assert_eq!(
            text_of(&one(&orphan_composer, ".swarm-routing-preview")),
            "Notification preview unavailable until the thread root is loaded."
        );
        orphan_input.focus().unwrap();
        type_into(&orphan_input, "For @B");
        settle().await;
        press(&orphan_input, "Enter");
        settle().await;
        assert_eq!(
            text_of(&one(&orphan_composer, ".swarm-routing-preview")),
            "Notification preview unavailable until the thread root is loaded.",
            "a reply author is not inferred from a participant or missing root"
        );
        assert!(
            one(&orphan_composer, ".swarm-composer-send")
                .dyn_ref::<web_sys::HtmlButtonElement>()
                .unwrap()
                .disabled(),
            "posting waits until its routing consequence can be shown"
        );
        let posts_before_root = harness.commands_of("post").len();
        press(&orphan_input, "Enter");
        settle().await;
        assert_eq!(harness.commands_of("post").len(), posts_before_root);
        let orphan_root = make_post(
            sid,
            "orphan-root",
            "orphan-root",
            SwarmBoard::Briefing,
            5,
            by("ada"),
            vec![text("Earlier discussion")],
        );
        harness.thread_page(SwarmThreadPage {
            swarm_id: SwarmId(sid.into()),
            thread_id: SwarmThreadId("orphan-root".into()),
            root: orphan_root,
            posts: vec![orphan_reply],
            next_cursor: thread_cursor(sid, "orphan-root", 6, 6),
            high_water: 6,
            has_more: false,
        });
        settle().await;
        assert!(
            orphan_input.is_same_node(Some(&one(&orphan_composer, "textarea"))),
            "root arrival changes only the preview, not editor identity"
        );
        assert!(
            text_of(&one(&orphan_composer, ".swarm-routing-preview"))
                .starts_with("Notification recipients: Ada (idle), Bo (idle)."),
            "reply preview combines the real root author with the selected member"
        );
        assert!(
            !one(&orphan_composer, ".swarm-composer-send")
                .dyn_ref::<web_sys::HtmlButtonElement>()
                .unwrap()
                .disabled()
        );
        let (other_container, _other_handle) = mount_view(&harness, sid);
        settle().await;
        assert!(
            one(&container, "[role='tabpanel']").id()
                != one(&other_container, "[role='tabpanel']").id(),
            "duplicate swarm views have unique accessible control targets"
        );
        assert!(
            one(&container, "[data-board='Briefing']").id()
                != one(&other_container, "[data-board='Briefing']").id()
        );
    }

    /// The keyboard mention picker produces a typed mention; text the user
    /// typed without choosing stays literal. A rejected publication keeps the
    /// text and shows the host's reason on the composer that sent it.
    #[wasm_bindgen_test]
    async fn mention_picker_posts_typed_segments_and_publication_errors_stay_on_composer() {
        let harness = Harness::new("host-swarm-mentions");
        let sid = "sw-mentions";
        harness.swarm(&make_swarm(
            sid,
            "Mentions",
            vec![idle("ada", "Ada"), idle("alan", "Alan"), idle("bo", "Bo")],
        ));
        let (container, _handle) = mount_view(&harness, sid);
        harness.board_page(page(sid, SwarmBoard::Briefing, Vec::new(), 0, false));
        settle().await;

        let input = one(&container, ".swarm-root-composer:not([hidden]) textarea");
        let root_composer = one(&container, ".swarm-root-composer:not([hidden])");
        let notification_preview = || text_of(&one(&root_composer, ".swarm-routing-preview"));
        assert!(
            notification_preview()
                .starts_with("Notification recipients: Ada (idle), Alan (idle), Bo (idle)."),
            "Briefing without selected mentions previews the canonical broadcast recipients"
        );
        input.focus().unwrap();
        type_into(&input, "cc @Ada and @A");
        settle().await;
        let options = all(&container, ".swarm-mention-option");
        let names: Vec<String> = options
            .iter()
            .map(|option| text_of(&one(option, ".swarm-mention-option-name")))
            .collect();
        assert_eq!(names, ["Ada", "Alan"]);
        assert_eq!(
            options[0].get_attribute("aria-selected").as_deref(),
            Some("true")
        );
        press(&input, "ArrowDown");
        settle().await;
        assert_eq!(
            all(&container, ".swarm-mention-option")[1]
                .get_attribute("aria-selected")
                .as_deref(),
            Some("true")
        );
        press(&input, "Enter");
        settle().await;
        assert!(
            all(&container, ".swarm-mention-option").is_empty(),
            "picker closes after choosing"
        );
        let textarea = input.dyn_ref::<web_sys::HtmlTextAreaElement>().unwrap();
        assert_eq!(textarea.value(), "cc @Ada and @Alan ");
        assert!(
            notification_preview().starts_with("Notification recipients: Alan (idle)."),
            "only the selected occurrence narrows the audience; literal Ada does not route"
        );
        assert!(
            harness.commands_of("post").is_empty(),
            "Enter in the picker chooses, it does not send"
        );

        type_into(&input, "cc @Ada and @Alan please review");
        press(&input, "Enter");
        settle().await;
        let posts = harness.commands_of("post");
        assert_eq!(posts.len(), 1);
        let publication = &posts[0]["publication"];
        assert_eq!(publication["board"], "briefing");
        assert_eq!(
            publication["body"],
            json!([
                {"type": "text", "text": "cc @Ada and "},
                {"type": "member_mention", "member_id": "alan"},
                {"type": "text", "text": " please review"},
            ]),
            "only the chosen member is a typed mention"
        );
        let publication_id =
            SwarmPublicationId(publication["publication_id"].as_str().unwrap().to_owned());
        assert!(
            text_of(&container).contains("Publishing — waiting for the host to record the post…")
        );

        harness.error(
            sid,
            Some(&publication_id),
            SwarmErrorCode::Invalid,
            "Post body is too large",
        );
        settle().await;
        assert_eq!(
            text_of(&one(
                &container,
                ".swarm-root-composer:not([hidden]) .swarm-composer-error"
            )),
            "Not published: Post body is too large"
        );
        assert!(
            all(&container, ".swarm-server-error").is_empty(),
            "the composer owns its publication error"
        );
        assert_eq!(
            textarea.value(),
            "cc @Ada and @Alan please review",
            "rejected text is kept"
        );

        button(&container, "Retry").click();
        settle().await;
        let posts = harness.commands_of("post");
        assert_eq!(posts.len(), 2);
        assert_eq!(
            posts[1]["publication"]["publication_id"], publication["publication_id"],
            "retry is idempotent"
        );
        assert!(all(&container, ".swarm-composer-error").is_empty());

        let mut recorded = make_post(
            sid,
            "m1",
            "m1",
            SwarmBoard::Briefing,
            1,
            SwarmAuthor::Human,
            serde_json::from_value(publication["body"].clone()).unwrap(),
        );
        recorded.publication_id = publication_id.clone();
        harness.post(&recorded);
        let mut durability_attention = make_swarm(
            sid,
            "Mentions",
            vec![idle("ada", "Ada"), idle("alan", "Alan"), idle("bo", "Bo")],
        );
        durability_attention.lifecycle = SwarmLifecycle::AttentionRequired;
        durability_attention.error = Some(
            "Directory durability is uncertain. New execution is withheld until explicit Resume."
                .to_owned(),
        );
        harness.swarm(&durability_attention);
        harness.error(
            sid,
            Some(&publication_id),
            SwarmErrorCode::CommittedDurabilityUncertain,
            "Post recorded; directory durability could not be confirmed",
        );
        settle().await;
        assert_eq!(
            textarea.value(),
            "",
            "the composer clears once the host records the post"
        );
        let warning = one(&container, ".swarm-server-error");
        assert_eq!(warning.get_attribute("role").as_deref(), Some("status"));
        assert!(
            text_of(&warning)
                .contains("Post recorded; directory durability could not be confirmed"),
            "committed durability warning remains visible after acceptance"
        );
        assert!(
            all(&root_composer, ".swarm-composer-error").is_empty(),
            "already recorded post is never labeled not published"
        );
        assert!(
            !has_button(&root_composer, "Retry"),
            "accepted publication has no failed-publication retry controls"
        );
        assert_eq!(
            text_of(&one(&container, ".swarm-lifecycle-pill")),
            "Needs attention"
        );
        assert!(
            notification_preview().contains("Needs attention; posting does not resume delivery.")
        );
        assert!(
            has_button(&container, "Resume"),
            "server attention state, not warning text, controls execution recovery"
        );
        let mentions = all(&one(&container, "#swarm-post-m1"), ".swarm-mention");
        assert_eq!(mentions.len(), 1);
        assert_eq!(text_of(&mentions[0]), "@Alan");
        assert!(
            text_of(&one(&container, "#swarm-post-m1")).contains("cc @Ada and @Alan please review")
        );

        assert_eq!(mentions[0].tag_name(), "BUTTON");
        assert_eq!(
            mentions[0].get_attribute("aria-label").as_deref(),
            Some("Open Alan's conversation")
        );
        mentions[0].focus().unwrap();
        press(&mentions[0], "Enter");
        settle().await;
        assert_eq!(
            harness.state.active_agent.get_untracked(),
            Some(ActiveAgentRef {
                host_id: harness.host.clone(),
                agent_id: AgentId("agent-alan".into())
            }),
            "a mention opens that member's conversation"
        );

        let mut first_nova = idle("nova-a", "Nova");
        first_nova.spec.focus = Some("Accessibility".to_owned());
        let mut second_nova = idle("nova-b", "Nova");
        second_nova.spec.backend_kind = BackendKind::Codex;
        second_nova.spec.launch_profile_id = LaunchProfileId("codex-default".to_owned());
        second_nova.spec.focus = Some("Delivery".to_owned());
        let mut same_names = make_swarm(sid, "Mentions", vec![first_nova, second_nova]);
        same_names.constraints.allocations[0].count = 1;
        same_names
            .constraints
            .allocations
            .push(SwarmBackendAllocation {
                backend_kind: BackendKind::Codex,
                launch_profile_id: LaunchProfileId("codex-default".to_owned()),
                session_settings: Default::default(),
                count: 1,
            });
        harness.swarm(&same_names);
        input.focus().unwrap();
        type_into(&input, "literal @Nova; selected @N");
        settle().await;
        let options = all(&container, ".swarm-mention-option");
        assert_eq!(options.len(), 2);
        assert!(text_of(&options[0]).contains("Accessibility"));
        assert!(text_of(&options[1]).contains("Delivery"));
        assert_ne!(
            options[0].get_attribute("aria-label"),
            options[1].get_attribute("aria-label"),
            "same-named members are distinguishable before selection"
        );
        press(&input, "Enter");
        settle().await;
        type_into(&input, "literal @Nova; selected @Nova and @N");
        settle().await;
        press(&input, "ArrowDown");
        press(&input, "Tab");
        settle().await;
        type_into(
            &input,
            "literal @Nova; selected @Nova and @Nova literal again @Nova",
        );
        replace_selection(&input, 0, 0, "🧭 ");
        same_names.members[0].spec.name = "Quasar".to_owned();
        harness.swarm(&same_names);
        settle().await;
        assert_eq!(
            all(
                &container,
                ".swarm-root-composer:not([hidden]) .swarm-mention-chip"
            )
            .len(),
            2
        );
        press(&input, "Escape");
        press(&input, "Enter");
        settle().await;
        let duplicate_commands = harness.commands_of("post");
        let duplicate_publication = duplicate_commands.last().unwrap()["publication"].clone();
        assert!(
            duplicate_publication["body"]
                == json!([
                    {"type":"text","text":"🧭 literal @Nova; selected "},
                    {"type":"member_mention","member_id":"nova-a"},
                    {"type":"text","text":" and "},
                    {"type":"member_mention","member_id":"nova-b"},
                    {"type":"text","text":" literal again @Nova"}
                ]),
            "same-name choices retain distinct typed IDs and unselected literals after editing and a rename"
        );

        button(
            &one(&container, ".swarm-root-composer:not([hidden])"),
            "Edit",
        )
        .click();
        settle().await;
        let value = textarea.value();
        let chosen_at = value.find("selected ").unwrap() + "selected ".len();
        replace_selection(&input, chosen_at, chosen_at + "@Nova".len(), "@Nova");
        settle().await;
        assert_eq!(
            all(
                &container,
                ".swarm-root-composer:not([hidden]) .swarm-mention-chip"
            )
            .len(),
            1,
            "replacing a selected occurrence by pasted text removes only its typed identity"
        );
        same_names.members[1].state = SwarmMemberState::Retiring;
        harness.swarm(&same_names);
        settle().await;
        assert!(
            notification_preview()
                .starts_with("Notification recipients: Nova — undeliverable (retiring)."),
            "selected retiring reference remains an undeliverable recipient, not a replacement"
        );
        same_names.members[1].state = SwarmMemberState::Retired;
        same_names.members[1].agent_id = None;
        same_names.members[1].runtime_status = None;
        same_names.lifecycle = SwarmLifecycle::Paused;
        harness.swarm(&same_names);
        settle().await;
        assert!(
            notification_preview()
                .starts_with("Notification recipients: Nova — undeliverable (retired).")
        );
        assert!(notification_preview().contains("Paused; posting does not resume delivery."));
        press(&input, "Escape");
        press(&input, "Enter");
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["publication"]["body"]
                == json!([
                    {"type":"text","text":"🧭 literal @Nova; selected @Nova and "},
                    {"type":"member_mention","member_id":"nova-b"},
                    {"type":"text","text":" literal again @Nova"}
                ]),
            "only the untouched occurrence stays a mention after same-text replacement"
        );

        let briefing_body =
            harness.commands_of("post").last().unwrap()["publication"]["body"].clone();
        button(&root_composer, "Edit").click();
        settle().await;
        let briefing_draft = textarea.value();
        let briefing_tab = one(&container, "[role='tab'][data-board='Briefing']");
        let coordination_tab = one(&container, "[role='tab'][data-board='Coordination']");
        press(&briefing_tab, "ArrowRight");
        settle().await;
        let coordination_composer = one(&container, ".swarm-root-composer:not([hidden])");
        let coordination_input = one(&coordination_composer, "textarea");
        let coordination_textarea = coordination_input
            .dyn_ref::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        assert!(
            coordination_textarea.value().is_empty(),
            "switching to Coordination must never carry unsent Briefing text across boards"
        );
        assert!(!input.is_same_node(Some(&coordination_input)));
        assert_eq!(
            all(&container, ".swarm-root-composer")
                .iter()
                .filter(|composer| composer.get_bounding_client_rect().height() > 0.0)
                .count(),
            1,
            "only the active board's composer is visible"
        );
        assert_eq!(
            text_of(&one(&coordination_composer, ".swarm-routing-preview")),
            "Shared context only — no members notified.",
            "Coordination starts with its own unmentioned draft and canonical audience"
        );
        coordination_input.focus().unwrap();
        type_into(
            &coordination_input,
            "Coordination literal @Quasar and selected @Q",
        );
        settle().await;
        press(&coordination_input, "Enter");
        settle().await;
        type_into(
            &coordination_input,
            "Coordination literal @Quasar and selected @Quasar working note",
        );
        let coordination_draft = coordination_textarea.value();
        press(&coordination_tab, "ArrowLeft");
        settle().await;
        assert!(
            input.is_same_node(Some(&one(
                &container,
                ".swarm-root-composer:not([hidden]) textarea"
            ))),
            "returning to Briefing restores its original mounted editor"
        );
        assert!(textarea.value() == briefing_draft);
        assert_eq!(all(&root_composer, ".swarm-mention-chip").len(), 1);
        button(&root_composer, "Post").click();
        settle().await;
        let briefing_publication =
            harness.commands_of("post").last().unwrap()["publication"].clone();
        assert_eq!(briefing_publication["board"], "briefing");
        assert!(
            briefing_publication["body"] == briefing_body,
            "the unsent Briefing draft keeps the exact selected member occurrence across board switches"
        );

        press(&briefing_tab, "ArrowRight");
        settle().await;
        assert!(coordination_input.is_same_node(Some(&one(
            &container,
            ".swarm-root-composer:not([hidden]) textarea"
        ))));
        assert!(coordination_textarea.value() == coordination_draft);
        assert!(
            !coordination_textarea.read_only(),
            "a pending Briefing publication must not lock the independent Coordination draft"
        );
        coordination_input.focus().unwrap();
        same_names.members[0].state = SwarmMemberState::Dormant;
        same_names.members[0].agent_id = None;
        same_names.members[0].session_id = Some(protocol::SessionId("retained-nova-a".to_owned()));
        same_names.members[0].runtime_status = None;
        harness.swarm(&same_names);
        harness.post(&make_post(
            sid,
            "board-live",
            "board-live",
            SwarmBoard::Coordination,
            2,
            SwarmAuthor::Human,
            vec![text("Live shared context")],
        ));
        settle().await;
        let ready_chip = all(&container, ".swarm-member-chip")
            .into_iter()
            .find(|chip| text_of(&one(chip, ".swarm-member-chip-name")) == "Quasar")
            .unwrap();
        assert_eq!(
            text_of(&one(&ready_chip, ".swarm-member-chip-status")),
            "Ready to resume"
        );
        assert!(coordination_textarea.value() == coordination_draft);
        assert_eq!(all(&coordination_composer, ".swarm-mention-chip").len(), 1);
        assert!(
            coordination_input.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            ),
            "live board/member updates preserve the active board's draft focus"
        );
        assert!(
            text_of(&one(&coordination_composer, ".swarm-routing-preview"))
                .starts_with("Notification recipients: Quasar (ready to resume)."),
            "canonical Dormant members are distinct from never-started members in the routing preview"
        );
        button(&coordination_composer, "Post").click();
        settle().await;
        let coordination_publication =
            harness.commands_of("post").last().unwrap()["publication"].clone();
        assert_eq!(coordination_publication["board"], "coordination");
        assert!(
            coordination_publication["body"]
                == json!([
                    {"type":"text","text":"Coordination literal @Quasar and selected "},
                    {"type":"member_mention","member_id":"nova-a"},
                    {"type":"text","text":" working note"}
                ]),
            "Coordination preserves its own typed ID without selecting the identical literal name"
        );
        assert!(
            briefing_publication["publication_id"] != coordination_publication["publication_id"]
        );
        press(&coordination_tab, "ArrowLeft");
        settle().await;
        assert!(textarea.read_only());
        button(&root_composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["publication"] == briefing_publication,
            "Briefing retry retains the entire original publication identity, board, and typed body"
        );
        press(&briefing_tab, "ArrowRight");
        settle().await;
        button(&coordination_composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["publication"] == coordination_publication,
            "Coordination retry retains its independent pending publication"
        );
        let mut accepted_briefing = make_post(
            sid,
            "board-briefing-accepted",
            "board-briefing-accepted",
            SwarmBoard::Briefing,
            3,
            SwarmAuthor::Human,
            serde_json::from_value(briefing_publication["body"].clone()).unwrap(),
        );
        accepted_briefing.publication_id = SwarmPublicationId(
            briefing_publication["publication_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        harness.post(&accepted_briefing);
        settle().await;
        assert!(
            textarea.value().is_empty(),
            "canonical acceptance clears the hidden owning draft"
        );
        assert!(coordination_textarea.value() == coordination_draft);
        assert!(
            coordination_textarea.read_only(),
            "another board's acknowledgment must not accept this pending publication"
        );
        press(&coordination_tab, "ArrowLeft");
        settle().await;
        input.focus().unwrap();
        type_into(&input, "Fresh Briefing draft @Q");
        settle().await;
        press(&input, "Tab");
        settle().await;
        let fresh_briefing = textarea.value();
        let mut accepted_coordination = make_post(
            sid,
            "board-coordination-accepted",
            "board-coordination-accepted",
            SwarmBoard::Coordination,
            4,
            SwarmAuthor::Human,
            serde_json::from_value(coordination_publication["body"].clone()).unwrap(),
        );
        accepted_coordination.publication_id = SwarmPublicationId(
            coordination_publication["publication_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        harness.post(&accepted_coordination);
        settle().await;
        assert!(
            textarea.value() == fresh_briefing,
            "hidden Coordination acceptance cannot erase fresh unsent Briefing text"
        );
        assert_eq!(all(&root_composer, ".swarm-mention-chip").len(), 1);
        assert!(
            input.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            )
        );
        press(&briefing_tab, "ArrowRight");
        settle().await;
        assert!(coordination_textarea.value().is_empty());
        assert!(!coordination_textarea.read_only());
        assert!(all(&coordination_composer, ".swarm-mention-chip").is_empty());
        press(&coordination_tab, "ArrowLeft");
        settle().await;
        assert!(textarea.value() == fresh_briefing);
        button(&root_composer, "Post").click();
        settle().await;
        let fresh_publication = harness.commands_of("post").last().unwrap()["publication"].clone();
        assert_eq!(fresh_publication["board"], "briefing");
        assert!(
            fresh_publication["body"]
                == json!([
                    {"type":"text","text":"Fresh Briefing draft "},
                    {"type":"member_mention","member_id":"nova-a"},
                    {"type":"text","text":" "}
                ])
        );
    }

    /// Unread counts come from the swarm record; a fully loaded board marks
    /// read exactly at the server's high-water mark.
    #[wasm_bindgen_test]
    async fn unread_badges_mark_read_at_high_water_once_board_is_loaded() {
        let harness = Harness::new("host-swarm-unread");
        let sid = "sw-unread";
        let mut swarm = make_swarm(sid, "Unread", vec![idle("ada", "Ada")]);
        swarm.board_positions = vec![
            SwarmBoardPosition {
                board: SwarmBoard::Briefing,
                high_water: 5,
                human_read_cursor: 3,
                unread_count: 2,
            },
            SwarmBoardPosition {
                board: SwarmBoard::Coordination,
                high_water: 4,
                human_read_cursor: 3,
                unread_count: 1,
            },
        ];
        harness.swarm(&swarm);
        let (container, _handle) = mount_view(&harness, sid);
        settle().await;
        assert_eq!(
            text_of(&one(
                &container,
                "[data-board='Briefing'] .swarm-unread-badge"
            )),
            "2"
        );
        assert_eq!(
            text_of(&one(
                &container,
                "[data-board='Coordination'] .swarm-unread-badge"
            )),
            "1"
        );
        assert!(
            harness.commands_of("mark_read").is_empty(),
            "nothing is read before the board loads"
        );

        let posts = (1..=5)
            .map(|cursor| {
                let id = format!("u{cursor}");
                make_post(
                    sid,
                    &id,
                    &id,
                    SwarmBoard::Briefing,
                    cursor,
                    by("ada"),
                    vec![text("update")],
                )
            })
            .collect();
        harness.board_page(page(sid, SwarmBoard::Briefing, posts, 5, false));
        settle().await;
        let marks = harness.commands_of("mark_read");
        assert_eq!(
            marks,
            vec![json!({"kind": "mark_read", "swarm_id": sid, "board": "briefing", "cursor": 5})]
        );

        swarm.board_positions[0].human_read_cursor = 5;
        swarm.board_positions[0].unread_count = 0;
        harness.swarm(&swarm);
        settle().await;
        assert!(
            container
                .query_selector("[data-board='Briefing'] .swarm-unread-badge")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            text_of(&one(
                &container,
                "[data-board='Coordination'] .swarm-unread-badge"
            )),
            "1"
        );
        assert_eq!(harness.commands_of("mark_read").len(), 1);
    }

    /// Launch, partial failure, uncertain delivery and pause states render
    /// from server records, and each recovery control sends its command.
    #[wasm_bindgen_test]
    async fn lifecycle_failure_and_delivery_recovery_send_typed_commands() {
        let harness = Harness::new("host-swarm-lifecycle");
        let sid = "sw-life";
        let mut failed = member("cy", "Cy", SwarmMemberState::Failed, None, None);
        failed.error = Some("Backend binary not found".to_owned());
        let mut swarm = make_swarm(
            sid,
            "Lifecycle",
            vec![
                idle("ada", "Ada"),
                member("bo", "Bo", SwarmMemberState::Reserved, None, None),
                failed,
            ],
        );
        swarm.lifecycle = SwarmLifecycle::Launching;
        swarm.notifications = vec![SwarmNotification {
            id: SwarmNotificationId("n1".into()),
            member_id: SwarmMemberId("ada".into()),
            post_ids: vec![SwarmPostId("l1".into())],
            round_id: SwarmRoundId("round-1".into()),
            state: SwarmDeliveryState::Uncertain,
            error: Some("Backend did not confirm".to_owned()),
        }];
        let mut single_launch = swarm.clone();
        single_launch.members = vec![swarm.members[1].clone()];
        single_launch.constraints.allocations[0].count = 1;
        single_launch.notifications.clear();
        harness.swarm(&single_launch);
        let (container, _handle) = mount_view(&harness, sid);
        settle().await;
        assert_eq!(
            text_of(&one(&container, ".swarm-banners .swarm-banner-text")),
            "Starting 1 of 1 member…"
        );
        swarm.revision += 1;
        harness.swarm(&swarm);
        harness.board_page(page(
            sid,
            SwarmBoard::Briefing,
            vec![make_post(
                sid,
                "l1",
                "l1",
                SwarmBoard::Briefing,
                1,
                SwarmAuthor::Human,
                vec![text("Go")],
            )],
            1,
            false,
        ));
        settle().await;

        let banners = text_of(&one(&container, ".swarm-banners"));
        assert!(banners.contains("Starting 1 of 3 members…"), "{banners}");
        assert!(banners.contains("A member needs attention. Review the error below."));
        assert!(banners.contains("Backend binary not found"));
        button(&one(&container, ".swarm-failure-row"), "Retry").click();
        settle().await;
        assert_eq!(
            harness.commands_of("retry_member"),
            vec![json!({"kind": "retry_member", "swarm_id": sid, "member_id": "cy"})]
        );

        let delivery = one(&container, "#swarm-post-l1 .swarm-delivery");
        assert!(
            text_of(&delivery).contains("delivery uncertain for Ada — Backend did not confirm")
        );
        button(&delivery, "Retry delivery").click();
        settle().await;
        assert_eq!(
            harness.commands_of("retry_notification"),
            vec![json!({"kind": "retry_notification", "swarm_id": sid, "notification_id": "n1"})]
        );

        button(&container, "Pause").click();
        settle().await;
        assert_eq!(
            harness.commands_of("pause"),
            vec![json!({"kind": "pause", "swarm_id": sid})]
        );
        swarm.lifecycle = SwarmLifecycle::Pausing;
        harness.swarm(&swarm);
        settle().await;
        assert!(!has_button(&container, "Pause"));
        let pausing = button(&container, "Pausing…");
        assert!(
            pausing
                .dyn_ref::<web_sys::HtmlButtonElement>()
                .unwrap()
                .disabled()
        );
        assert_eq!(
            text_of(&one(&container, ".swarm-lifecycle-pill")),
            "Pausing…"
        );

        swarm.lifecycle = SwarmLifecycle::Paused;
        harness.swarm(&swarm);
        settle().await;
        button(&container, "Resume").click();
        settle().await;
        assert_eq!(
            harness.commands_of("resume"),
            vec![json!({"kind": "resume", "swarm_id": sid})]
        );

        // A live provider failure is not a failed launch, and a one-member
        // swarm has no remaining peers whose continued work can be promised.
        swarm.revision += 1;
        swarm.lifecycle = SwarmLifecycle::Running;
        swarm.members = vec![swarm.members[2].clone()];
        swarm.members[0].error = Some("Agent backend closed".to_owned());
        swarm.constraints.allocations[0].count = 1;
        swarm.notifications.clear();
        harness.swarm(&swarm);
        settle().await;
        let failure = one(&container, ".swarm-banner-list");
        assert_eq!(
            text_of(&one(&failure, ".swarm-banner-text")),
            "A member needs attention. Review the error below."
        );
        assert!(text_of(&failure).contains("Agent backend closed"));
        assert!(has_button(&failure, "Retry"));
    }

    /// A link to a post outside every loaded page is resolved by the host
    /// from its exact id; the returned thread is shown and the post focused.
    /// A host rejection ends the wait with the host's reason.
    #[wasm_bindgen_test]
    async fn unloaded_post_link_resolves_by_exact_id_and_focuses_the_post() {
        let harness = Harness::new("host-swarm-deeplink");
        let sid = "sw-deeplink";
        harness.swarm(&make_swarm(sid, "Deep links", vec![idle("ada", "Ada")]));
        let (container, _handle) = mount_view(&harness, sid);
        let linking = make_post(
            sid,
            "dl-new",
            "dl-new",
            SwarmBoard::Coordination,
            40,
            by("ada"),
            vec![
                text("Following up on "),
                SwarmBodySegment::PostLink {
                    post_id: SwarmPostId("dl-old-reply".into()),
                },
                text(" and "),
                SwarmBodySegment::PostLink {
                    post_id: SwarmPostId("dl-missing".into()),
                },
            ],
        );
        harness.board_page(page(sid, SwarmBoard::Briefing, Vec::new(), 0, false));
        harness.post(&linking);
        one(&container, "[data-board='Coordination']").click();
        settle().await;

        let links = all(&container, "#swarm-post-dl-new .swarm-post-link-unloaded");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].tag_name(), "BUTTON");
        assert_eq!(text_of(&links[0]), "↪ Open linked post");
        links[0].click();
        settle().await;
        assert_eq!(
            harness.commands_of("read_post"),
            vec![json!({"kind": "read_post", "swarm_id": sid, "post_id": "dl-old-reply"})]
        );
        assert!(text_of(&container).contains("Opening linked post…"));

        let root = make_post(
            sid,
            "dl-root",
            "dl-root",
            SwarmBoard::Briefing,
            3,
            SwarmAuthor::Human,
            vec![text("Old plan")],
        );
        let reply = make_post(
            sid,
            "dl-old-reply",
            "dl-root",
            SwarmBoard::Briefing,
            7,
            by("ada"),
            vec![text("Old answer")],
        );
        harness.thread_page(SwarmThreadPage {
            swarm_id: SwarmId(sid.into()),
            thread_id: SwarmThreadId("dl-root".into()),
            root,
            posts: vec![reply],
            next_cursor: thread_cursor(sid, "dl-root", 7, 7),
            high_water: 7,
            has_more: false,
        });
        settle().await;
        settle().await;
        assert_eq!(
            one(&container, "[data-board='Briefing']")
                .get_attribute("aria-selected")
                .as_deref(),
            Some("true"),
            "the linked post's board is shown"
        );
        let active = web_sys::window()
            .unwrap()
            .document()
            .unwrap()
            .active_element()
            .expect("focus");
        assert_eq!(active.id(), "swarm-post-dl-old-reply");
        let thread = one(&container, "[data-thread-id='dl-root']");
        let ids: Vec<String> = all(&thread, ".swarm-post")
            .iter()
            .map(|p| p.get_attribute("data-post-id").unwrap())
            .collect();
        assert_eq!(ids, ["dl-root", "dl-old-reply"]);
        assert!(!text_of(&container).contains("Opening linked post…"));

        one(&container, "[data-board='Coordination']").click();
        settle().await;
        let resolved = one(
            &container,
            "#swarm-post-dl-new a.swarm-post-link[data-post-id='dl-old-reply']",
        );
        assert_eq!(text_of(&resolved), "↪ Ada: Old answer");

        one(&container, "#swarm-post-dl-new .swarm-post-link-unloaded").click();
        settle().await;
        assert_eq!(
            harness.commands_of("read_post").last().unwrap()["post_id"],
            "dl-missing"
        );
        assert!(text_of(&container).contains("Opening linked post…"));
        harness.error(
            sid,
            None,
            SwarmErrorCode::NotFound,
            "Linked post does not belong to swarm",
        );
        settle().await;
        assert!(!text_of(&container).contains("Opening linked post…"));
        assert_eq!(
            text_of(&one(&container, ".swarm-server-error .swarm-banner-text")),
            "Linked post does not belong to swarm"
        );

        let composer = one(&container, ".swarm-root-composer:not([hidden])");
        let input = one(&composer, "textarea");
        input.focus().unwrap();
        type_into(&input, "See  for context");
        settle().await;
        assert_eq!(
            text_of(&one(&composer, ".swarm-routing-preview")),
            "Shared context only — no members notified.",
            "an unmentioned Coordination post creates no notification recipients"
        );
        input
            .dyn_ref::<web_sys::HtmlTextAreaElement>()
            .unwrap()
            .set_selection_range(4, 4)
            .unwrap();
        button(&composer, "Link a post").click();
        settle().await;
        button(&composer, "Briefing · Ada: Old answer").click();
        settle().await;
        assert_eq!(
            input
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "See ↪ Ada: Old answer  for context"
        );
        assert!(
            input.is_same_node(
                web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .active_element()
                    .as_ref()
                    .map(|element| element.as_ref())
            ),
            "post link insertion returns focus to the editor"
        );
        button(&composer, "Post").click();
        settle().await;
        let commands = harness.commands_of("post");
        let publication = &commands.last().unwrap()["publication"];
        assert_eq!(publication["board"], "coordination");
        assert!(
            publication["body"]
                == json!([
                    {"type":"text","text":"See "},
                    {"type":"post_link","post_id":"dl-old-reply"},
                    {"type":"text","text":"  for context"}
                ]),
            "human picker inserts the exact cross-board post ID at the caret without converting surrounding text"
        );
        assert_eq!(
            text_of(&one(&composer, ".swarm-routing-preview")),
            "Shared context only — no members notified.",
            "post links do not become mentions or notify linked authors"
        );
        let mut human_link = make_post(
            sid,
            "dl-human",
            "dl-human",
            SwarmBoard::Coordination,
            41,
            SwarmAuthor::Human,
            serde_json::from_value(publication["body"].clone()).unwrap(),
        );
        human_link.publication_id =
            SwarmPublicationId(publication["publication_id"].as_str().unwrap().to_owned());
        harness.post(&human_link);
        settle().await;
        let link = one(&container, "#swarm-post-dl-human .swarm-post-link");
        link.focus().unwrap();
        press(&link, "Enter");
        settle().await;
        assert_eq!(
            web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .active_element()
                .unwrap()
                .id(),
            "swarm-post-dl-old-reply"
        );
        assert_eq!(
            one(&container, "[data-board='Briefing']")
                .get_attribute("aria-selected")
                .as_deref(),
            Some("true"),
            "human post link navigates to its canonical board and focuses its target"
        );
    }

    /// Typed segments travel through markdown as tokens: in prose they become
    /// mention elements; inside code, link text, URLs, titles and image alt
    /// text they degrade to escaped plain text, and literal marker characters
    /// in user text can never forge a segment.
    #[wasm_bindgen_test]
    async fn body_segments_render_safely_inside_surrounding_markdown() {
        let harness = Harness::new("host-swarm-markdown");
        let sid = "sw-markdown";
        harness.swarm(&make_swarm(
            sid,
            "Markdown",
            vec![idle("ada", "Ada \"<b>\"")],
        ));
        let (container, _handle) = mount_view(&harness, sid);
        let body = vec![
            text("Plain "),
            mention("ada"),
            text(" and **bold "),
            mention("ada"),
            text("**.\n\nInline `code "),
            mention("ada"),
            text("` done.\n\n```\nfence "),
            mention("ada"),
            text("\n```\n\n[link "),
            mention("ada"),
            text("](https://example.com/"),
            mention("ada"),
            text(" \"title "),
            mention("ada"),
            text("\") and ![alt "),
            mention("ada"),
            text("](i.png)\n\nForged \u{E000}0\u{E001} marker <img src=x onerror=alert(1)>"),
        ];
        harness.board_page(page(
            sid,
            SwarmBoard::Briefing,
            vec![make_post(
                sid,
                "md1",
                "md1",
                SwarmBoard::Briefing,
                1,
                by("ada"),
                body,
            )],
            1,
            false,
        ));
        settle().await;

        let rendered = one(&container, "#swarm-post-md1 .swarm-post-body");
        let mentions = all(&rendered, ".swarm-mention");
        assert_eq!(
            mentions.len(),
            2,
            "only the two prose mentions are elements: {}",
            rendered.inner_html()
        );
        for mention in &mentions {
            assert_eq!(
                mention.get_attribute("data-member-id").as_deref(),
                Some("ada")
            );
            assert_eq!(text_of(mention), "@Ada \"<b>\"");
            assert_eq!(
                mention.child_element_count(),
                0,
                "member names are escaped text"
            );
        }
        assert_eq!(
            text_of(&one(&rendered, "strong .swarm-mention")),
            "@Ada \"<b>\""
        );
        assert!(
            all(
                &rendered,
                "code .swarm-mention, a .swarm-mention, pre .swarm-mention"
            )
            .is_empty()
        );

        let inline_code = all(&rendered, "code")
            .into_iter()
            .find(|code| code.closest("pre").unwrap().is_none())
            .unwrap();
        assert_eq!(text_of(&inline_code), "code @Ada \"<b>\"");
        assert_eq!(inline_code.child_element_count(), 0);
        let fenced = one(&rendered, "pre code");
        assert!(text_of(&fenced).contains("fence @Ada \"<b>\""));
        assert_eq!(fenced.child_element_count(), 0);

        let link = all(&rendered, "a")
            .into_iter()
            .find(|link| text_of(link).starts_with("link"))
            .expect("markdown link rendered");
        assert_eq!(text_of(&link), "link @Ada \"<b>\"");
        assert_eq!(link.child_element_count(), 0);

        for element in all(&rendered, "*") {
            let names = element.get_attribute_names();
            for index in 0..names.length() {
                let name = names.get(index).as_string().unwrap();
                let value = element.get_attribute(&name).unwrap_or_default();
                assert!(
                    !value.contains("swarm-mention") || name == "class",
                    "segment markup leaked into {name}={value:?}"
                );
                assert!(
                    !value.contains('\u{E000}') && !value.contains('\u{E001}'),
                    "token marker in {name}"
                );
            }
        }
        assert!(
            rendered
                .query_selector("img[src='x'], [onerror]")
                .unwrap()
                .is_none(),
            "raw HTML stays inert"
        );
        let visible = text_of(&rendered);
        assert!(!visible.contains('\u{E000}') && !visible.contains('\u{E001}'));
        assert!(
            visible.contains("Forged 0 marker"),
            "a literal marker is plain text, not a segment: {visible}"
        );
    }
}
