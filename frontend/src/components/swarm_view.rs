//! A swarm's shared conversation and host-reported agent status.
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
    SwarmDeliveryState, SwarmHumanPost, SwarmId, SwarmImage, SwarmImageId, SwarmImageOutcome,
    SwarmImageUpload, SwarmLifecycle, SwarmMember, SwarmMemberId, SwarmMemberState,
    SwarmNotificationId, SwarmPost, SwarmPostId, SwarmPublicationId, SwarmReadCursor,
    SwarmThreadId, SwarmThreadRead, SwarmWorkspacePolicy,
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
        SwarmLifecycle::Transitioning => "busy",
        SwarmLifecycle::Pausing | SwarmLifecycle::Paused => "muted",
        SwarmLifecycle::AttentionRequired => "warn",
    }
}

/// Human label for a member from its server-owned state and runtime status.
pub(crate) fn member_status_label(member: &SwarmMember) -> &'static str {
    match member.state {
        SwarmMemberState::Proposed => "Ready to chat",
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
        SwarmMemberState::Failed if member.replacement_due_at_ms.is_some() => "Replacing",
        SwarmMemberState::Failed => "Failed",
    }
}

pub(crate) fn member_status_tone(member: &SwarmMember) -> &'static str {
    match member.state {
        SwarmMemberState::Proposed => "muted",
        SwarmMemberState::Reserved => "busy",
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
        SwarmMemberState::Failed if member.replacement_due_at_ms.is_some() => "busy",
        SwarmMemberState::Failed => "error",
    }
}

fn replacement_label(total: u32) -> String {
    if total == 1 {
        "Replaced after an error".to_owned()
    } else {
        format!("Replaced after errors ({total}×)")
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
        SwarmWorkspacePolicy::SharedWorkbench { .. } => "Workbench scope — writable",
        SwarmWorkspacePolicy::SharedProject { .. } => "Project scope — writable",
        SwarmWorkspacePolicy::SharedHost { .. } => "Host scope — writable",
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
    body: &[SwarmBodySegment],
    swarm: Option<&Swarm>,
    posts: &HashMap<SwarmPostId, SwarmPost>,
) -> String {
    use crate::markdown::{InlineToken, inline_token_placeholder, strip_token_markers};
    let mut source = String::new();
    let mut tokens: Vec<InlineToken> = Vec::new();
    for segment in body {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SwarmTab {
    Board(SwarmBoard),
    Agents,
}

impl SwarmTab {
    fn board(self) -> Option<SwarmBoard> {
        match self {
            Self::Board(board) => Some(board),
            Self::Agents => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Board(board) => board_label(board),
            Self::Agents => "Agents",
        }
    }
}

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
    let tab = RwSignal::new(SwarmTab::Board(SwarmBoard::Briefing));
    let board = Memo::new(move |_| tab.get().board());
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
        let Some(current) = board.get() else {
            return;
        };
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
                view: protocol::SwarmBoardView::Posts,
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
        let Some(current) = board.get() else {
            return;
        };
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
        let Some(current) = board.get() else {
            return false;
        };
        loaded_threads.with(|threads| !threads.iter().any(|(_, target)| *target == current))
    });
    let board_loading = Memo::new(move |_| {
        let Some(current) = board.get() else {
            return false;
        };
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
        tab.set(SwarmTab::Board(target_board));
        let element_id = format!("swarm-post-{}", post_id.0);
        request_animation_frame(move || {
            let Some(element) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id(&element_id))
            else {
                return;
            };
            // A linked agent reply may sit in a collapsed discussion group.
            if let Ok(Some(group)) = element.closest("details") {
                let _ = group.set_attribute("open", "");
            }
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
        let faces = current
            .members
            .iter()
            .filter(|member| member.state != SwarmMemberState::Retired)
            .map(|member| {
                let label = format!("{} — {}", member.spec.name, member_status_label(member));
                view! {
                    <span
                        class="swarm-avatar swarm-face"
                        class:swarm-face-working=member_is_working(member)
                        data-tone=member_status_tone(member)
                        style=avatar_style(&member.spec.id.0)
                        title=label.clone()
                        aria-label=label
                        role="img"
                    >
                        {initials(&member.spec.name)}
                    </span>
                }
            })
            .collect_view();
        let project = projects_signal.with(|projects| {
            project_name(projects, &host.get_value(), &current.constraints.project_id)
        });
        let policy = current.constraints.workspace_policy;
        let can_pause = matches!(
            lifecycle,
            SwarmLifecycle::Running | SwarmLifecycle::Transitioning
        );
        let can_resume = matches!(
            lifecycle,
            SwarmLifecycle::Paused | SwarmLifecycle::AttentionRequired
        );
        let preview_pending = current.change_preview.is_some();
        view! {
            <header class="swarm-header">
                <h1 class="swarm-title">{current.name.clone()}</h1>
                <span class="swarm-header-project">{project}</span>
                {(policy != SwarmWorkspacePolicy::default()).then(|| view! {
                    <span
                        class="swarm-scope-label"
                        data-policy=match policy {
                            SwarmWorkspacePolicy::ReadOnly => "read_only",
                            SwarmWorkspacePolicy::SharedWorkbench { .. } => "shared_workbench",
                            SwarmWorkspacePolicy::SharedProject { .. } => "shared_project",
                            SwarmWorkspacePolicy::SharedHost { .. } => "shared_host",
                        }
                    >
                        {workspace_policy_label(policy)}
                    </span>
                })}
                <span class="swarm-faces" aria-label="Swarm agents">{faces}</span>
                <span class="swarm-header-spacer"></span>
                <span class="swarm-run-control" data-tone=lifecycle_tone(lifecycle)>
                    {if lifecycle == SwarmLifecycle::Pausing {
                        view! {
                            <button
                                class="swarm-lifecycle-pill"
                                data-tone=lifecycle_tone(lifecycle)
                                disabled=true
                                title="Waiting for running turns to stop"
                            >
                                {lifecycle_label(lifecycle)}
                            </button>
                        }
                        .into_any()
                    } else {
                        view! {
                            <span class="swarm-lifecycle-pill" data-tone=lifecycle_tone(lifecycle)>
                                {lifecycle_label(lifecycle)}
                            </span>
                        }
                        .into_any()
                    }}
                    {can_pause.then(|| view! {
                        <button on:click=move |_| send(SwarmCommandPayload::Pause { swarm_id: sid.get_value() })>
                            "Pause"
                        </button>
                    })}
                    {can_resume.then(|| view! {
                        <button on:click=move |_| send(SwarmCommandPayload::Resume { swarm_id: sid.get_value() })>
                            "Resume"
                        </button>
                    })}
                </span>
                <button
                    class="swarm-icon-btn swarm-manage-btn"
                    class:swarm-manage-pending=preview_pending
                    aria-label=if preview_pending { "Manage • preview pending" } else { "Manage" }
                    title=if preview_pending { "Manage swarm — change preview pending" } else { "Manage swarm" }
                    on:click=move |_| manage_open.set(true)
                >
                    <span aria-hidden="true">"⚙"</span>
                </button>
            </header>
        }
        .into_any()
    };

    let agents = move || {
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let members = current
            .members
            .iter()
            .filter(|member| member.state != SwarmMemberState::Retired)
            .collect::<Vec<_>>();
        let empty = members.is_empty();
        let rows = members.into_iter().map(|member| {
            let name = member.spec.name.clone();
            let body = view! {
                <span class="swarm-agent-identity">
                    <span class="swarm-avatar swarm-avatar-sm" style=avatar_style(&member.spec.id.0) aria-hidden="true">{initials(&name)}</span>
                    <span class="swarm-member-chip-name">{name.clone()}</span>
                </span>
                <span class="swarm-agent-state">
                    <span class="swarm-member-chip-status" data-tone=member_status_tone(member)>{member_status_label(member)}</span>
                    {member.error.clone().map(|error| view! { <span class="swarm-agent-error">{error}</span> })}
                    {member.last_replacement.clone().map(|replacement| view! {
                        <span class="swarm-agent-replaced" title=format!("Last error: {}", replacement.reason)>
                            {replacement_label(replacement.total)}
                        </span>
                    })}
                </span>
                <span class="swarm-agent-backend">{backend_name(member.spec.backend_kind)}</span>
                <span class="swarm-agent-focus">{member.spec.focus.clone().filter(|focus| !focus.is_empty()).unwrap_or_else(|| "No starting focus set".to_owned())}</span>
                <span class="swarm-agent-open">{if member.agent_id.is_some() { "Open conversation →" } else { "No conversation yet" }}</span>
            };
            if member.agent_id.is_some() {
                let member_for_click = member.clone();
                view! {
                    <button class="swarm-member-chip" title=format!("Open {name}'s conversation") on:click=move |_| state_sv.with_value(|state| {
                        open_swarm_member_chat(state, host.get_value(), &member_for_click)
                    })>{body}</button>
                }.into_any()
            } else {
                view! { <div class="swarm-member-chip swarm-member-chip-static">{body}</div> }.into_any()
            }
        }).collect_view();
        view! {
            <div class="swarm-agents-list" aria-label="Swarm agents">
                <div class="swarm-agent-columns" aria-hidden="true">
                    <span>"Agent"</span><span>"Status"</span><span>"Backend"</span><span>"Starting focus"</span><span>"Conversation"</span>
                </div>
                {rows}
                {empty.then(|| view! { <div class="swarm-empty">"No current agents."</div> })}
            </div>
        }.into_any()
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
            .filter(|member| member.replacement_due_at_ms.is_none())
            .cloned()
            .collect();
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
        view! { {attention} {swarm_error} {failures} }.into_any()
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
    let tab_id = move |target: SwarmTab| {
        format!(
            "swarm-board-tab-{}-{}",
            view_identity.get_value(),
            target.label()
        )
    };
    let briefing_tab_ref = NodeRef::<leptos::html::Button>::new();
    let coordination_tab_ref = NodeRef::<leptos::html::Button>::new();
    let agents_tab_ref = NodeRef::<leptos::html::Button>::new();
    let view_tab = move |target: SwarmTab| {
        let unread = move || {
            target
                .board()
                .map(|board| {
                    swarm.with(|s| s.as_ref().map(|s| board_unread(s, board)).unwrap_or(0))
                })
                .unwrap_or(0)
        };
        let tab_ref = match target {
            SwarmTab::Board(SwarmBoard::Briefing) => briefing_tab_ref,
            SwarmTab::Board(SwarmBoard::Coordination) => coordination_tab_ref,
            SwarmTab::Agents => agents_tab_ref,
        };
        let on_keydown = move |ev: web_sys::KeyboardEvent| {
            let next = match ev.key().as_str() {
                "ArrowRight" => match target {
                    SwarmTab::Board(SwarmBoard::Briefing) => {
                        SwarmTab::Board(SwarmBoard::Coordination)
                    }
                    SwarmTab::Board(SwarmBoard::Coordination) => SwarmTab::Agents,
                    SwarmTab::Agents => SwarmTab::Board(SwarmBoard::Briefing),
                },
                "ArrowLeft" => match target {
                    SwarmTab::Board(SwarmBoard::Briefing) => SwarmTab::Agents,
                    SwarmTab::Board(SwarmBoard::Coordination) => {
                        SwarmTab::Board(SwarmBoard::Briefing)
                    }
                    SwarmTab::Agents => SwarmTab::Board(SwarmBoard::Coordination),
                },
                "Home" => SwarmTab::Board(SwarmBoard::Briefing),
                "End" => SwarmTab::Agents,
                _ => return,
            };
            ev.prevent_default();
            tab.set(next);
            let next_ref = match next {
                SwarmTab::Board(SwarmBoard::Briefing) => briefing_tab_ref,
                SwarmTab::Board(SwarmBoard::Coordination) => coordination_tab_ref,
                SwarmTab::Agents => agents_tab_ref,
            };
            if let Some(element) = next_ref.get_untracked()
                && element.focus().is_err()
            {
                action_error.set(Some("Could not focus the selected swarm tab.".to_owned()));
            }
        };
        view! {
            <button
                class="swarm-board-tab"
                role="tab"
                id=tab_id(target)
                node_ref=tab_ref
                aria-controls=panel_id.get_value()
                tabindex=move || if tab.get() == target { "0" } else { "-1" }
                aria-selected=move || (tab.get() == target).to_string()
                class:active=move || tab.get() == target
                data-board=target.label()
                aria-label=target.label()
                on:click=move |_| tab.set(target)
                on:keydown=on_keydown
            >
                <span>{target.label()}</span>
                {move || (unread() > 0).then(|| view! {
                    <span class="swarm-unread-badge" aria-label=format!("{} unread", unread())>{unread()}</span>
                })}
                {move || (target == SwarmTab::Agents).then(|| {
                    let total = swarm.with(|s| s.as_ref().map(|s| s.members.iter().filter(|member| member.state != SwarmMemberState::Retired).count()).unwrap_or(0));
                    view! { <span class="swarm-tab-count" aria-label=format!("{total} agents")>{total}</span> }
                })}
            </button>
        }
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
                <div class="swarm-banners">{status_banners}{error_banners}</div>
                <nav class="swarm-board-tabs" role="tablist" aria-label="Swarm views">
                    {view_tab(SwarmTab::Board(SwarmBoard::Briefing))}
                    {view_tab(SwarmTab::Board(SwarmBoard::Coordination))}
                    {view_tab(SwarmTab::Agents)}
                </nav>
                <div class="swarm-content" role="tabpanel" id=panel_id.get_value() aria-labelledby=move || tab_id(tab.get()) tabindex="0">
                <div class="swarm-board-scroll" node_ref=scroll_ref hidden=move || board.get().is_none()>
                    <div class="swarm-board-column">
                        <Show when=move || board_empty.get()>
                            <div class="swarm-empty">
                                {move || if board_loading.get() {
                                    "Loading board…"
                                } else if board.get() == Some(SwarmBoard::Briefing) {
                                    "Your swarm is ready. Send the first message below."
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
                <div class="swarm-agents-scroll" hidden=move || tab.get() != SwarmTab::Agents>{agents}</div>
                </div>
                // Humans start threads only on Briefing; the draft stays mounted
                // so switching tabs never loses unsent text.
                <div class="swarm-root-composer" hidden=move || board.get() != Some(SwarmBoard::Briefing)>
                    <SwarmComposer
                        host=host
                        sid=sid
                        swarm=swarm
                        board=Signal::derive(|| SwarmBoard::Briefing)
                        thread_id=None
                    />
                </div>
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

#[derive(Clone, Debug, PartialEq)]
struct ThreadHeader {
    parent: Option<(SwarmThreadId, String)>,
    title: Option<String>,
    naming_error: Option<String>,
    summary: String,
    children: Vec<(SwarmThreadId, String)>,
}

/// A Briefing thread reads as the human's posts and the members' results,
/// with the member discussion between them folded into one group.
#[derive(Clone, Debug, PartialEq)]
enum ThreadItem {
    Post(Box<SwarmPost>),
    AgentReplies(Vec<SwarmPost>),
}

impl ThreadItem {
    fn key(&self) -> String {
        match self {
            Self::Post(post) => post.id.0.clone(),
            Self::AgentReplies(posts) => format!("agent-replies-{}", posts[0].id.0),
        }
    }
}

fn thread_items(board: SwarmBoard, replies: &[SwarmPost]) -> Vec<ThreadItem> {
    let mut items = Vec::new();
    for post in replies {
        let discussion = board == SwarmBoard::Briefing
            && !post.result
            && matches!(post.author, SwarmAuthor::Member { .. });
        match items.last_mut() {
            Some(ThreadItem::AgentReplies(group)) if discussion => group.push(post.clone()),
            _ if discussion => items.push(ThreadItem::AgentReplies(vec![post.clone()])),
            _ => items.push(ThreadItem::Post(Box::new(post.clone()))),
        }
    }
    items
}

#[component]
fn SwarmThread(
    host: StoredValue<String>,
    sid: StoredValue<SwarmId>,
    thread_id: SwarmThreadId,
    swarm: Memo<Option<Swarm>>,
    board: SwarmBoard,
    selected_board: Signal<Option<SwarmBoard>>,
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
    let thread_header = Memo::new(move |_| {
        swarm.with(|swarm| {
            let swarm = swarm.as_ref()?;
            let id = thread.get_value();
            let state = swarm.threads.iter().find(|state| state.thread_id == id)?;
            let title_of = |thread: &protocol::SwarmThread| {
                thread.title.clone().unwrap_or_else(|| "Naming…".to_owned())
            };
            let parent = state.parent_thread_id.as_ref().and_then(|parent_id| {
                swarm
                    .threads
                    .iter()
                    .find(|parent| &parent.thread_id == parent_id)
                    .map(|parent| (parent_id.clone(), title_of(parent)))
            });
            let mut children = swarm
                .threads
                .iter()
                .filter(|child| child.parent_thread_id.as_ref() == Some(&id))
                .collect::<Vec<_>>();
            children.sort_by_key(|child| child.creation_cursor);
            Some(ThreadHeader {
                parent,
                title: state.title.clone(),
                naming_error: state.naming_error.clone(),
                summary: state.summary.clone(),
                children: children
                    .into_iter()
                    .map(|child| (child.thread_id.clone(), title_of(child)))
                    .collect(),
            })
        })
    });
    let items = Memo::new(move |_| thread_posts.with(|(_, replies)| thread_items(board, replies)));
    let quote: RwSignal<Option<SwarmPost>> = RwSignal::new(None);
    let on_reply = Callback::new(move |()| reply_open.set(true));
    let on_quote = Callback::new(move |post: SwarmPost| {
        quote.set(Some(post));
        reply_open.set(true);
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
        <section class="swarm-thread" data-thread-id=thread.get_value().0 hidden=move || selected_board.get() != Some(board)>
            {move || thread_header.get().map(|header| view! {
                <div class="swarm-thread-head">
                    {header.parent.map(|(parent_id, parent_title)| view! {
                        <button class="swarm-link-btn swarm-thread-parent" on:click=move |_| on_link.run(SwarmPostId(parent_id.0.clone()))>{format!("Re: {parent_title}")}</button>
                    })}
                    {match header.title {
                        Some(title) => view! { <h3 class="swarm-thread-title">{title}</h3> }.into_any(),
                        None => view! { <h3 class="swarm-thread-title swarm-thread-naming" role="status">"Naming…"</h3> }.into_any(),
                    }}
                    {(!header.children.is_empty()).then(|| view! {
                        <nav class="swarm-thread-children" aria-label="Coordination threads">
                            {header.children.into_iter().map(|(child_id, child_title)| view! {
                                <button class="swarm-thread-chip" on:click=move |_| on_link.run(SwarmPostId(child_id.0.clone()))>{format!("↳ Coordination: {child_title}")}</button>
                            }).collect_view()}
                        </nav>
                    })}
                    {(!header.summary.is_empty()).then(|| view! {
                        <details class="swarm-thread-state">
                            <summary>"Agent notes"</summary>
                            <div class="swarm-thread-summary">{header.summary}</div>
                        </details>
                    })}
                    {header.naming_error.map(|message| view! { <p class="swarm-thread-error" role="status">{format!("Naming failed, retrying: {message}")}</p> })}
                </div>
            })}
            {move || match thread_posts.with(|(root, _)| root.clone()) {
                Some(root) => view! {
                    <SwarmPostCard host=host post=root swarm=swarm on_link=on_link on_reply=on_reply on_quote=on_quote />
                }.into_any(),
                None => view! {
                    <div class="swarm-thread-orphan">"Reply to an earlier post"</div>
                }.into_any(),
            }}
            <div class="swarm-replies">
                <For
                    each=move || items.get()
                    key=|item| item.key()
                    let:item
                >
                    {match item {
                        ThreadItem::Post(post) => view! {
                            <SwarmPostCard host=host post=*post swarm=swarm on_link=on_link on_reply=on_reply on_quote=on_quote />
                        }.into_any(),
                        ThreadItem::AgentReplies(posts) => {
                            let key = ThreadItem::AgentReplies(posts).key();
                            let group = Memo::new(move |_| items.with(|items| items.iter().find_map(|item| match item {
                                ThreadItem::AgentReplies(posts) if item.key() == key => Some(posts.clone()),
                                _ => None,
                            }).unwrap_or_default()));
                            view! {
                                <details class="swarm-agent-replies">
                                    <summary>
                                        <span class="swarm-faces swarm-faces-sm" aria-hidden="true">
                                            {move || swarm.with(|swarm| {
                                                let mut authors: Vec<SwarmMemberId> = Vec::new();
                                                for post in group.get() {
                                                    if let SwarmAuthor::Member { member_id } = post.author
                                                        && !authors.contains(&member_id)
                                                    {
                                                        authors.push(member_id);
                                                    }
                                                }
                                                authors.into_iter().map(|member_id| {
                                                    let name = member_name(swarm.as_ref(), &member_id);
                                                    view! { <span class="swarm-avatar swarm-face" style=avatar_style(&member_id.0)>{initials(&name)}</span> }
                                                }).collect_view()
                                            })}
                                        </span>
                                        <span>{move || {
                                            let count = group.with(Vec::len);
                                            format!("{count} agent repl{}", if count == 1 { "y" } else { "ies" })
                                        }}</span>
                                    </summary>
                                    <For each=move || group.get() key=|post| post.id.clone() let:post>
                                        <SwarmPostCard host=host post=post swarm=swarm on_link=on_link on_reply=on_reply on_quote=on_quote />
                                    </For>
                                </details>
                            }.into_any()
                        }
                    }}
                </For>
            </div>
            <Show when=show_load>
                <div class="swarm-thread-actions">
                    <button class="swarm-link-btn" on:click=load_replies>{load_label}</button>
                </div>
            </Show>
            <Show when=move || reply_open.get()>
                <div class="swarm-reply-composer">
                    <SwarmComposer
                        host=host
                        sid=sid
                        swarm=swarm
                        board=Signal::derive(move || board)
                        thread_id=Some(thread.get_value())
                        on_cancel=Callback::new(move |_| reply_open.set(false))
                        quote=quote
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
    on_reply: Callback<()>,
    on_quote: Callback<SwarmPost>,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let posts_signal = state.swarm_posts;
    let host_streams = state.host_streams;
    let state_sv = StoredValue::new_local(state.clone());
    let post = StoredValue::new(post);

    let author = move || {
        swarm.with(|swarm| post.with_value(|post| author_name(swarm.as_ref(), &post.author)))
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
            post.with_value(|post| render_body(&post.body, swarm_value.as_ref(), posts))
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

    // Delivery outcomes for this post, grouped by state. "Delivered" means the
    // backend accepted the turn — never that the member read or acted on it.
    let delivery = move || {
        let post_id = post.with_value(|post| post.id.clone());
        let Some(current) = swarm.get() else {
            return ().into_any();
        };
        let mut groups: Vec<DeliveryGroup> = Vec::new();
        // Accepted deliveries are the normal case; only outstanding or
        // problem deliveries need the human's attention.
        for notification in current.notifications.iter().filter(|notification| {
            notification.post_ids.contains(&post_id)
                && notification.state != SwarmDeliveryState::Accepted
        }) {
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

    let (post_id, created_at_ms, is_result) =
        post.with_value(|post| (post.id.0.clone(), post.created_at_ms, post.result));
    view! {
        <article
            class="swarm-post"
            class:swarm-post-human=is_human
            class:swarm-post-result=is_result
            id=format!("swarm-post-{post_id}")
            data-post-id=post_id
            tabindex="-1"
        >
            <span class="swarm-avatar" style=avatar_style(&avatar_key) aria-hidden="true">
                {move || initials(&author())}
            </span>
            <div class="swarm-post-main">
                <header class="swarm-post-meta">
                    {is_result.then(|| view! { <span class="swarm-result-label">"✓ Result"</span> })}
                    <span class="swarm-post-author">{author}</span>
                    <time class="swarm-post-time">{format_time(created_at_ms)}</time>
                    <span class="swarm-post-actions">
                        <button class="swarm-post-action" on:click=move |_| on_reply.run(())>"Reply"</button>
                        <button class="swarm-post-action" title="Link this post in a reply" on:click=move |_| on_quote.run(post.get_value())>"Link"</button>
                    </span>
                </header>
                <div class="swarm-post-body chat-card-body" on:click=on_body_click on:keydown=on_body_keydown inner_html=body_html></div>
                <SwarmAttachments host=host attachments=post.with_value(|post| post.attachments.clone()) />
                <div class="swarm-image-gallery">
                    {post.with_value(|post| post.images.clone()).into_iter().map(|image| view! {
                        <SwarmSharedImage host=host swarm_id=post.with_value(|post| post.swarm_id.clone()) image=image />
                    }).collect_view()}
                </div>
                {delivery}
            </div>
        </article>
    }
}

#[component]
fn SwarmAttachments(host: StoredValue<String>, attachments: Vec<SwarmAttachment>) -> impl IntoView {
    let state_sv = StoredValue::new_local(expect_context::<AppState>());
    if attachments.is_empty() {
        return ().into_any();
    }
    let chips = attachments
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
}

#[component]
fn SwarmSharedImage(
    host: StoredValue<String>,
    swarm_id: SwarmId,
    image: SwarmImage,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let posts = state.swarm_posts;
    let host_streams = state.host_streams;
    let swarm_id = StoredValue::new(swarm_id);
    let image = StoredValue::new(image);
    let requested: RwSignal<Option<StreamPath>> = RwSignal::new(None);
    let transport_error: RwSignal<Option<String>> = RwSignal::new(None);
    let decode_error = RwSignal::new(false);
    let current = RwSignal::new(None::<usize>);
    let outcome = Memo::new(move |_| {
        posts.with(|map| {
            map.get(&(host.get_value(), swarm_id.get_value()))
                .and_then(|posts| {
                    posts
                        .images
                        .get(&image.with_value(|image| image.id.clone()))
                        .cloned()
                })
        })
    });
    let source = Memo::new(move |_| match outcome.get() {
        Some(SwarmImageOutcome::Ready {
            data: Some(data), ..
        }) => Some(format!("data:{};base64,{}", data.media_type, data.data)),
        Some(SwarmImageOutcome::Ready { data: None, .. } | SwarmImageOutcome::Failed { .. })
        | None => None,
    });
    let load_error = Memo::new(move |_| match outcome.get() {
        Some(SwarmImageOutcome::Failed { error }) => Some(error.message),
        _ => transport_error.get(),
    });
    let read = move || {
        transport_error.set(None);
        decode_error.set(false);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::ReadImage {
                swarm_id: swarm_id.get_value(),
                image_id: image.with_value(|image| image.id.clone()),
            },
            Some(Callback::new(move |message| {
                transport_error.set(Some(message))
            })),
        );
    };
    Effect::new(move |_| {
        let Some(stream) = host_streams.with(|streams| streams.get(&host.get_value()).cloned())
        else {
            return;
        };
        if source.get().is_some()
            || load_error.get().is_some()
            || requested.get_untracked().as_ref() == Some(&stream)
        {
            return;
        }
        requested.set(Some(stream));
        read();
    });
    let name = image.with_value(|image| image.name.clone());
    let alt = name.clone();
    view! {
        <figure class="swarm-shared-image">
            {move || match source.get() {
                Some(src) => view! {
                    <button class="swarm-image-open" aria-label=format!("Open image {}", image.with_value(|image| image.name.clone())) on:click=move |_| current.set(Some(0))>
                        <img src=src.clone() alt=alt.clone() loading="lazy" on:error=move |_| decode_error.set(true) />
                    </button>
                    {move || current.get().map(|_| view! { <crate::components::image_lightbox::ImageLightbox sources=vec![src.clone()].into() current=current /> })}
                }.into_any(),
                None => view! {
                    <div class="swarm-image-placeholder">
                        {move || load_error.get().unwrap_or_else(|| "Loading shared image…".to_owned())}
                        <Show when=move || load_error.get().is_some()><button class="swarm-link-btn" on:click=move |_| read()>"Retry image"</button></Show>
                    </div>
                }.into_any(),
            }}
            <figcaption title=name.clone()>{name.clone()}</figcaption>
            <Show when=move || decode_error.get()><span class="swarm-composer-error" role="alert">"This browser could not display the shared image."</span></Show>
        </figure>
    }
}

#[derive(Clone, PartialEq)]
struct SwarmDraftImage {
    upload: SwarmImageUpload,
    transport_error: Option<String>,
}

// ── Composer ───────────────────────────────────────────────────────────────

/// Publication ids currently held by a mounted composer in this swarm view.
#[derive(Clone, Copy)]
struct ClaimedPublications(RwSignal<HashSet<SwarmPublicationId>>);

#[derive(Clone, Debug, PartialEq)]
struct PendingPublication {
    swarm_id: SwarmId,
    board: SwarmBoard,
    post: SwarmHumanPost,
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
    /// A post the human asked to link from the thread; the composer inserts
    /// the link and clears the request.
    #[prop(optional)]
    quote: Option<RwSignal<Option<SwarmPost>>>,
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
    let images: RwSignal<Vec<SwarmDraftImage>> = RwSignal::new(Vec::new());
    let reading_images = RwSignal::new(false);
    let image_error: RwSignal<Option<String>> = RwSignal::new(None);
    let drag_depth = RwSignal::new(0u32);
    let image_input_ref = NodeRef::<leptos::html::Input>::new();
    let pending: RwSignal<Option<PendingPublication>> = RwSignal::new(None);
    let send_error: RwSignal<Option<String>> = RwSignal::new(None);
    let completion: RwSignal<Option<(usize, String)>> = RwSignal::new(None);
    let selected = RwSignal::new(0usize);
    let attach_open = RwSignal::new(false);
    let link_open = RwSignal::new(false);
    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
    let upload_image = move |upload: SwarmImageUpload| {
        let id = upload.image_id.clone();
        images.update(|images| {
            if let Some(image) = images.iter_mut().find(|image| image.upload.image_id == id) {
                image.transport_error = None;
            }
        });
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::UploadImage {
                swarm_id: sid.get_value(),
                image: upload,
            },
            Some(Callback::new(move |message: String| {
                images.update(|images| {
                    if let Some(image) = images.iter_mut().find(|image| image.upload.image_id == id)
                    {
                        image.transport_error = Some(message.clone());
                    }
                });
            })),
        );
    };
    let images_ready = Memo::new(move |_| {
        if reading_images.get() {
            return false;
        }
        let ids = images.with(|images| {
            images
                .iter()
                .map(|image| image.upload.image_id.clone())
                .collect::<Vec<_>>()
        });
        posts_signal.with(|map| {
            ids.iter().all(|id| {
                matches!(
                    map.get(&(host.get_value(), sid.get_value()))
                        .and_then(|posts| posts.images.get(id)),
                    Some(SwarmImageOutcome::Ready { .. })
                )
            })
        })
    });
    let attach_images = move |files: Vec<web_sys::File>| {
        if pending.get_untracked().is_some() {
            image_error.set(Some(
                "Finish or edit the pending publication before adding images.".to_owned(),
            ));
            return;
        }
        if reading_images.get_untracked() {
            image_error.set(Some(
                "Wait for the current images to finish loading.".to_owned(),
            ));
            return;
        }
        let used = images.with_untracked(Vec::len) + attachments.with_untracked(Vec::len);
        if files.len() + used > protocol::SWARM_MAX_ATTACHMENTS {
            image_error.set(Some(format!(
                "A post can carry at most {} images and file attachments combined.",
                protocol::SWARM_MAX_ATTACHMENTS
            )));
            return;
        }
        image_error.set(None);
        reading_images.set(true);
        spawn_local(async move {
            let mut errors = Vec::new();
            for file in files {
                if !matches!(
                    file.type_().as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    errors.push("Supported images are PNG, JPEG, GIF, and WebP.".to_owned());
                    continue;
                }
                if file.size() == 0.0 || file.size() > protocol::SWARM_MAX_IMAGE_BYTES as f64 {
                    errors.push("Each image must be nonempty and at most 4 MiB.".to_owned());
                    continue;
                }
                match crate::components::chat_input::read_image_file(file).await {
                    Ok(image) => {
                        let mut random = [0u8; 16];
                        let entropy = web_sys::window()
                            .ok_or("The browser window is unavailable.")
                            .and_then(|window| {
                                window
                                    .crypto()
                                    .map_err(|_| "Browser crypto is unavailable.")
                            })
                            .and_then(|crypto| {
                                crypto
                                    .get_random_values_with_u8_array(&mut random)
                                    .map_err(|_| "Cannot generate a shared-image identity.")
                            });
                        if let Err(message) = entropy {
                            errors.push(message.to_owned());
                            continue;
                        }
                        random[6] = (random[6] & 0x0f) | 0x40;
                        random[8] = (random[8] & 0x3f) | 0x80;
                        let hex = format!("{:032x}", u128::from_be_bytes(random));
                        let image_id = format!(
                            "{}-{}-{}-{}-{}",
                            &hex[..8],
                            &hex[8..12],
                            &hex[12..16],
                            &hex[16..20],
                            &hex[20..]
                        );
                        let upload = SwarmImageUpload {
                            image_id: SwarmImageId(image_id),
                            name: image.name,
                            data: protocol::ImageData {
                                media_type: image.media_type,
                                data: image.data,
                            },
                        };
                        if images
                            .try_update(|images| {
                                images.push(SwarmDraftImage {
                                    upload: upload.clone(),
                                    transport_error: None,
                                })
                            })
                            .is_none()
                        {
                            return;
                        }
                        upload_image(upload);
                    }
                    Err(message) => errors.push(message),
                }
            }
            image_error.try_set((!errors.is_empty()).then(|| errors.join(" ")));
            reading_images.try_set(false);
        });
    };
    let on_image_paste = move |ev: web_sys::ClipboardEvent| {
        let Some(clipboard) = ev.clipboard_data() else {
            return;
        };
        let files = crate::components::chat_input::clipboard_image_files(&clipboard);
        if files.is_empty() {
            return;
        }
        ev.prevent_default();
        attach_images(files);
    };
    let on_image_dragover = move |ev: web_sys::DragEvent| {
        if !crate::app::drag_event_offers_external_files(&ev) {
            return;
        }
        ev.prevent_default();
        if let Some(transfer) = ev.data_transfer() {
            transfer.set_drop_effect("copy");
        }
        drag_depth.set(1);
    };
    let on_image_drop = move |ev: web_sys::DragEvent| {
        if !crate::app::drag_event_offers_external_files(&ev) {
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        drag_depth.set(0);
        let Some(transfer) = ev.data_transfer() else {
            return;
        };
        attach_images(crate::components::chat_input::data_transfer_files(
            &transfer,
        ));
    };
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
        let current =
            pending.with(|pending| pending.as_ref().map(|p| p.post.publication_id.clone()));
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
                set.remove(&waiting.post.publication_id);
            });
        }
    });
    let publication_error = Memo::new(move |_| {
        let id = pending.with(|pending| pending.as_ref().map(|p| p.post.publication_id.clone()))?;
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
        let id = &waiting.post.publication_id;
        let published = posts_signal.with(|map| {
            map.get(&(host.get_value(), waiting.swarm_id.clone()))
                .is_some_and(|posts| posts.posts.values().any(|post| &post.publication_id == id))
        });
        if published {
            clear_publication_errors(&waiting.post.publication_id);
            pending.set(None);
            text.set(String::new());
            references.set(Vec::new());
            attachments.set(Vec::new());
            images.set(Vec::new());
            image_error.set(None);
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
        clear_publication_errors(&waiting.post.publication_id);
        send_swarm_command(
            host_streams,
            &host.get_value(),
            SwarmCommandPayload::Post {
                swarm_id: waiting.swarm_id,
                post: waiting.post,
            },
            Some(Callback::new(move |message: String| {
                send_error.set(Some(message))
            })),
        );
    };

    let submit = move || {
        if pending.get_untracked().is_some()
            || !routing_available.get_untracked()
            || !images_ready.get_untracked()
        {
            return;
        }
        let body_text = text.get_untracked();
        if body_text.trim().is_empty() && images.with_untracked(Vec::is_empty) {
            return;
        }
        let waiting = PendingPublication {
            swarm_id: sid.get_value(),
            board: board.get_untracked(),
            post: SwarmHumanPost {
                publication_id: SwarmPublicationId(mint_id()),
                thread_id: thread.get_value(),
                body: compose_segments(&body_text, &references.get_untracked()),
                attachments: attachments.get_untracked(),
                images: images.with_untracked(|images| {
                    images
                        .iter()
                        .map(|image| image.upload.image_id.clone())
                        .collect()
                }),
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

    // Who a post will notify. The full list is the Post button's tooltip;
    // the composer only spells it out when something needs the human's eye.
    let routing = Memo::new(move |_| {
        let Some(current) = swarm.get() else {
            return (
                "Notification preview unavailable: swarm state is missing.".to_owned(),
                None,
                true,
            );
        };
        let waiting = pending.get();
        let target_board = waiting
            .as_ref()
            .map(|waiting| waiting.board)
            .unwrap_or_else(|| board.get());
        let body = waiting
            .as_ref()
            .map(|waiting| waiting.post.body.clone())
            .unwrap_or_else(|| compose_segments(&text.get(), &references.get()));
        let root_author = reply_root_author.get();
        if is_reply && root_author.is_none() {
            return (
                "Notification preview unavailable until the thread root is loaded.".to_owned(),
                None,
                true,
            );
        }
        let recipients = protocol::swarm_publication_recipients(
            &current,
            &SwarmAuthor::Human,
            target_board,
            root_author.as_ref(),
            &body,
        );
        if recipients.is_empty() {
            return (
                "Shared context only — no members notified.".to_owned(),
                None,
                true,
            );
        }
        let mut problem = false;
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
                        | SwarmMemberState::RetiringReserved => {
                            problem = true;
                            format!(
                                "{} — undeliverable ({})",
                                recipient_name(&current, member, index + 1),
                                member_status_label(member).to_lowercase()
                            )
                        }
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
                    None => {
                        problem = true;
                        "Unknown recipient — delivery preview unavailable".to_owned()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let lifecycle_note = match current.lifecycle {
            SwarmLifecycle::Pausing => Some("Pausing; posting does not resume delivery."),
            SwarmLifecycle::Paused => Some("Paused; posting does not resume delivery."),
            SwarmLifecycle::AttentionRequired => {
                Some("Needs attention; posting does not resume delivery.")
            }
            SwarmLifecycle::Running | SwarmLifecycle::Transitioning => None,
        };
        (
            format!("Notification recipients: {labels}."),
            lifecycle_note,
            problem || lifecycle_note.is_some(),
        )
    });
    let routing_preview =
        move || {
            let (summary, note, problem) = routing.get();
            problem.then(|| view! {
            <div class="swarm-routing-preview" role="status">
                <span>{summary}</span>
                {note.map(|note| view! { <span class="swarm-routing-warning">{note}</span> })}
            </div>
        })
        };

    if let Some(quote) = quote {
        Effect::new(move |_| {
            let Some(post) = quote.get() else {
                return;
            };
            if textarea_ref.get().is_none() || pending.get().is_some() {
                return;
            }
            quote.set(None);
            insert_post_link(post);
        });
    }

    let attachable = Memo::new(move |_| {
        let host_id = host.get_value();
        let mut files: Vec<SwarmAttachment> = open_files.with(|files| {
            files
                .keys()
                .filter(|key| key.host_id == host_id)
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
            "Reply… (@ to mention)"
        } else {
            "Message the swarm… (@ to mention)"
        }
    };

    view! {
        <div class="swarm-composer" class:swarm-composer-reply=is_reply class:swarm-composer-dragging={move || drag_depth.get() > 0} on:paste=on_image_paste on:dragover=on_image_dragover on:dragleave=move |_| drag_depth.set(0) on:drop=on_image_drop>
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
            <Show when=move || !images.with(Vec::is_empty)>
                <div class="swarm-image-gallery swarm-image-drafts">
                    <For each=move || images.with(|images| images.iter().map(|image| image.upload.image_id.clone()).collect::<Vec<_>>()) key=|id| id.clone() let:id>
                        {let image_id = StoredValue::new(id);
                         let draft = Memo::new(move |_| images.with(|images| images.iter().find(|image| image.upload.image_id == image_id.get_value()).cloned()));
                         let outcome = Memo::new(move |_| posts_signal.with(|map| map.get(&(host.get_value(), sid.get_value())).and_then(|posts| posts.images.get(&image_id.get_value()).cloned())));
                         view! {
                            <figure class="swarm-draft-image">
                                <img src=move || draft.get().map(|image| format!("data:{};base64,{}", image.upload.data.media_type, image.upload.data.data)) alt=move || draft.get().map(|image| image.upload.name) />
                                <button class="swarm-image-remove" aria-label=move || draft.get().map(|image| format!("Remove image {}", image.upload.name)) disabled=move || pending.get().is_some() on:click=move |_| images.update(|images| images.retain(|image| image.upload.image_id != image_id.get_value()))>"×"</button>
                                <figcaption title=move || draft.get().map(|image| image.upload.name.clone())>{move || draft.get().map(|image| image.upload.name)}</figcaption>
                                {move || match outcome.get() {
                                    Some(SwarmImageOutcome::Ready { .. }) => view! { <span class="swarm-image-status">"Ready"</span> }.into_any(),
                                    Some(SwarmImageOutcome::Failed { error }) => view! { <span class="swarm-composer-error" role="alert">{error.message}</span> }.into_any(),
                                    None => view! { <span class="swarm-image-status">{move || draft.get().and_then(|image| image.transport_error).unwrap_or_else(|| "Uploading…".to_owned())}</span> }.into_any(),
                                }}
                                <Show when=move || !matches!(outcome.get(), Some(SwarmImageOutcome::Ready { .. }))>
                                    <button class="swarm-link-btn" disabled=move || pending.get().is_some() on:click=move |_| { if let Some(image) = draft.get_untracked() { upload_image(image.upload); } }>"Retry image"</button>
                                </Show>
                            </figure>
                         }
                        }
                    </For>
                </div>
            </Show>
            <Show when=move || reading_images.get()><span class="swarm-image-status" role="status">"Reading images…"</span></Show>
            {move || image_error.get().map(|message| view! { <span class="swarm-composer-error" role="alert">{message}</span> })}

            <textarea
                class="swarm-composer-input"
                node_ref=textarea_ref
                rows="2"
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
                <div class="swarm-composer-status" hidden=move || pending.get().is_none() && send_error.get().is_none()>
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
            <div class="swarm-composer-footer">
                <div class="swarm-composer-tools">
                    <input type="file" node_ref=image_input_ref accept="image/png,image/jpeg,image/gif,image/webp" multiple=true hidden=true aria-label="Choose images" on:change=move |_| {
                        if let Some(input) = image_input_ref.get_untracked() {
                            let files = input.files().map(|files| (0..files.length()).filter_map(|index| files.get(index)).collect::<Vec<_>>()).unwrap_or_default();
                            input.set_value("");
                            attach_images(files);
                        }
                    } />
                    <Show when=move || pending.get().is_none()>
                        <button class="swarm-icon-btn" aria-label="Add images" title="Add images, paste screenshots, or drop multiple images here" disabled={move || reading_images.get() || images.with(Vec::len) + attachments.with(Vec::len) >= protocol::SWARM_MAX_ATTACHMENTS} on:click=move |_| { if let Some(input) = image_input_ref.get_untracked() { input.click(); } }><span aria-hidden="true">"🖼"</span></button>
                    </Show>
                    <Show when=move || pending.get().is_none()>
                        <div class="swarm-attach-wrap">
                            <button
                                class="swarm-icon-btn"
                                aria-label="Link a post"
                                title="Link a post"
                                aria-haspopup="true"
                                aria-expanded=move || link_open.get().to_string()
                                on:click=move |_| link_open.update(|open| *open = !*open)
                            >
                                <span aria-hidden="true">"🔗"</span>
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
                                class="swarm-icon-btn"
                                aria-label="Attach open file"
                                aria-haspopup="listbox"
                                aria-expanded=move || attach_open.get().to_string()
                                disabled={move || attachments.with(Vec::len) + images.with(Vec::len) >= protocol::SWARM_MAX_ATTACHMENTS}
                                title=move || { if attachments.with(Vec::len) + images.with(Vec::len) >= protocol::SWARM_MAX_ATTACHMENTS {
                                    format!("A post can carry at most {} attachments", protocol::SWARM_MAX_ATTACHMENTS)
                                } else {
                                    "Attach open file".to_owned()
                                } }
                                on:click=move |_| attach_open.update(|open| *open = !*open)
                            >
                                <span aria-hidden="true">"📄"</span>
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
                </div>
                <div class="swarm-composer-buttons">
                    {move || pending.get().map(|waiting| {
                        let retry = waiting.clone();
                        view! {
                            <button class="swarm-btn" on:click=move |_| dispatch_pending(retry.clone())>"Retry"</button>
                            <button
                                class="swarm-btn swarm-btn-quiet"
                                on:click=move |_| {
                                    clear_publication_errors(&waiting.post.publication_id);
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
                        title=move || routing.with(|(summary, _, _)| summary.clone())
                        disabled=move || pending.get().is_some() || !routing_available.get() || !images_ready.get() || (text.with(|t| t.trim().is_empty()) && images.with(Vec::is_empty))
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

        pub(crate) fn command_hosts(&self) -> Vec<String> {
            self.calls
                .iter()
                .filter_map(|entry| {
                    let entry = entry.dyn_into::<js_sys::Array>().expect("entry");
                    if entry.get(0).as_string().as_deref() != Some("send_host_line") {
                        return None;
                    }
                    let args: Value =
                        serde_json::from_str(&entry.get(1).as_string().expect("args"))
                            .expect("args json");
                    let envelope: Value =
                        serde_json::from_str(args["line"].as_str().expect("line"))
                            .expect("envelope");
                    (envelope["kind"] == "swarm_command")
                        .then(|| args["hostId"].as_str().expect("host").to_owned())
                })
                .collect()
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
            guidance_changed: false,
            unfinished_notification_ids: Vec::new(),
            replacement_due_at_ms: None,
            consecutive_replacements: 0,
            last_replacement: None,
            replaced_session_ids: Vec::new(),
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
            threads: Vec::new(),
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
                agent_wake_budget: Some(SWARM_DEFAULT_AGENT_WAKE_BUDGET),
            },
            lifecycle: SwarmLifecycle::Running,
            members,
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
            thread_change: None,
            thread_seq: None,
            result: false,
            images: Vec::new(),
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

    /// A button matches by its visible text or, for icon buttons, by the
    /// aria-label a screen reader announces.
    fn button_named(button: &HtmlElement, label: &str) -> bool {
        text_of(button).trim() == label
            || button.get_attribute("aria-label").as_deref() == Some(label)
    }

    pub(crate) fn button(root: &web_sys::Element, label: &str) -> HtmlElement {
        all(root, "button")
            .into_iter()
            .find(|button| button_named(button, label))
            .unwrap_or_else(|| panic!("button {label:?} not rendered; text: {}", text_of(root)))
    }

    /// The header's member faces as (announced label, working ring shown).
    pub(crate) fn header_faces(root: &web_sys::Element) -> Vec<(String, bool)> {
        all(root, ".swarm-header .swarm-face")
            .iter()
            .map(|face| {
                (
                    face.get_attribute("aria-label").unwrap_or_default(),
                    face.class_list().contains("swarm-face-working"),
                )
            })
            .collect()
    }

    fn faces(expected: &[(&str, bool)]) -> Vec<(String, bool)> {
        expected
            .iter()
            .map(|(label, working)| ((*label).to_owned(), *working))
            .collect()
    }

    pub(crate) fn has_button(root: &web_sys::Element, label: &str) -> bool {
        all(root, "button")
            .iter()
            .any(|button| button_named(button, label))
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
        let mut swarm = make_swarm(
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
        // The default read-only scope needs no badge; only a widened scope
        // (which the user must notice) renders one.
        assert!(all(&view, ".swarm-scope-label").is_empty());
        let reads = harness.commands_of("read_board");
        assert_eq!(reads.len(), 1, "first page requested once: {reads:?}");
        assert_eq!(reads[0]["query"]["board"], "briefing");
        assert_eq!(reads[0]["query"]["after_cursor"], Value::Null);

        assert_eq!(
            header_faces(&view),
            faces(&[
                ("Ada — Idle", false),
                ("Bo — Status unavailable", false),
                ("Cy — Ready to chat", false),
            ])
        );
        assert!(
            all(&view, ".swarm-member-chip")
                .iter()
                .all(|row| row.get_bounding_client_rect().height() == 0.0),
            "individual agents do not occupy the conversation header"
        );
        let reads_before_agents = harness.commands_of("read_board").len();
        button(&view, "Agents").click();
        settle().await;
        assert!(
            all(&view, ".swarm-root-composer")
                .iter()
                .all(|composer| composer.get_bounding_client_rect().height() == 0.0),
            "Agents has no board composer"
        );
        assert_eq!(
            harness.commands_of("read_board").len(),
            reads_before_agents,
            "opening Agents does not request a board"
        );
        let chips = all(&view, ".swarm-member-chip");
        assert!(
            chips
                .iter()
                .all(|row| row.get_bounding_client_rect().height() > 0.0)
        );
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
        assert_eq!(status("Cy"), "Ready to chat");
        assert!(
            chips
                .iter()
                .all(|chip| text_of(chip).contains("Cy") == (chip.tag_name() == "DIV")),
            "only members with a conversation are clickable"
        );

        let mut live_update = swarm.clone();
        live_update.revision += 1;
        live_update.members[1].runtime_status = Some(AgentControlStatus::Thinking);
        live_update.members[1].spec.focus = Some("Review layout and accessibility".to_owned());
        harness.swarm(&live_update);
        settle().await;
        assert_eq!(
            header_faces(&view),
            faces(&[
                ("Ada — Idle", false),
                ("Bo — Working", true),
                ("Cy — Ready to chat", false),
            ])
        );
        let bo = all(&view, ".swarm-member-chip")
            .into_iter()
            .find(|row| text_of(&one(row, ".swarm-member-chip-name")) == "Bo")
            .unwrap();
        assert_eq!(text_of(&one(&bo, ".swarm-member-chip-status")), "Working");
        assert_eq!(
            text_of(&one(&bo, ".swarm-agent-focus")),
            "Review layout and accessibility"
        );
        assert!(
            one(&bo, ".swarm-agent-focus")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        live_update.revision += 1;
        live_update.members[1].runtime_status = Some(AgentControlStatus::AwaitingUser);
        harness.swarm(&live_update);
        settle().await;
        let bo = all(&view, ".swarm-member-chip")
            .into_iter()
            .find(|row| text_of(&one(row, ".swarm-member-chip-name")) == "Bo")
            .unwrap();
        assert_eq!(
            text_of(&one(&bo, ".swarm-member-chip-status")),
            "Needs your answer"
        );
        assert_eq!(
            header_faces(&view),
            faces(&[
                ("Ada — Idle", false),
                ("Bo — Needs your answer", false),
                ("Cy — Ready to chat", false),
            ])
        );
        live_update.revision += 1;
        live_update.members[1].runtime_status = Some(AgentControlStatus::Failed);
        live_update.members[1].error = Some("Host could not start this turn".to_owned());
        harness.swarm(&live_update);
        settle().await;
        let bo = all(&view, ".swarm-member-chip")
            .into_iter()
            .find(|row| text_of(&one(row, ".swarm-member-chip-name")) == "Bo")
            .unwrap();
        assert_eq!(text_of(&one(&bo, ".swarm-member-chip-status")), "Failed");
        assert_eq!(
            text_of(&one(&bo, ".swarm-agent-error")),
            "Host could not start this turn"
        );
        let bo_row = |view: &web_sys::Element| {
            all(view, ".swarm-member-chip")
                .into_iter()
                .find(|row| text_of(&one(row, ".swarm-member-chip-name")) == "Bo")
                .unwrap()
        };
        let bo_agent = live_update.members[1].agent_id.clone();
        live_update.revision += 1;
        live_update.members[1].state = SwarmMemberState::Failed;
        live_update.members[1].agent_id = None;
        live_update.members[1].runtime_status = None;
        live_update.members[1].error = Some("Member agent terminated".to_owned());
        live_update.members[1].replacement_due_at_ms = Some(1);
        harness.swarm(&live_update);
        settle().await;
        let bo = bo_row(&view);
        assert_eq!(text_of(&one(&bo, ".swarm-member-chip-status")), "Replacing");
        assert_eq!(
            text_of(&one(&bo, ".swarm-agent-error")),
            "Member agent terminated"
        );
        assert!(
            !text_of(&view).contains("A member needs attention"),
            "a scheduled automatic replacement is not presented as needing attention"
        );
        live_update.revision += 1;
        live_update.members[1].state = SwarmMemberState::Live;
        live_update.members[1].agent_id = bo_agent.clone();
        live_update.members[1].runtime_status = Some(AgentControlStatus::Thinking);
        live_update.members[1].error = None;
        live_update.members[1].replacement_due_at_ms = None;
        live_update.members[1].last_replacement = Some(protocol::SwarmMemberReplacement {
            reason: "Member agent terminated".to_owned(),
            replaced_at_ms: 1,
            total: 1,
        });
        harness.swarm(&live_update);
        settle().await;
        let bo = bo_row(&view);
        assert_eq!(text_of(&one(&bo, ".swarm-member-chip-status")), "Working");
        let replaced = one(&bo, ".swarm-agent-replaced");
        assert_eq!(text_of(&replaced), "Replaced after an error");
        assert!(replaced.get_bounding_client_rect().height() > 0.0);
        assert_eq!(
            replaced.get_attribute("title").as_deref(),
            Some("Last error: Member agent terminated")
        );
        live_update.revision += 1;
        live_update.members[1].state = SwarmMemberState::Failed;
        live_update.members[1].agent_id = None;
        live_update.members[1].runtime_status = None;
        live_update.members[1].error = Some(
            "Bo failed after 3 automatic replacements; Retry restarts it manually. Last error: Member agent terminated".to_owned(),
        );
        live_update.members[1]
            .last_replacement
            .as_mut()
            .unwrap()
            .total = 3;
        harness.swarm(&live_update);
        settle().await;
        let bo = bo_row(&view);
        assert_eq!(text_of(&one(&bo, ".swarm-member-chip-status")), "Failed");
        assert_eq!(
            text_of(&one(&bo, ".swarm-agent-replaced")),
            "Replaced after errors (3×)"
        );
        assert!(
            text_of(&view).contains("A member needs attention"),
            "an exhausted member asks for a manual retry"
        );
        container.style().set_property("width", "420px").unwrap();
        settle().await;
        let pane = one(&view, ".swarm-agents-scroll");
        assert!(
            pane.scroll_width() <= pane.client_width() + 1,
            "agent details wrap in a narrow workspace pane"
        );
        for row in all(&view, ".swarm-member-chip") {
            assert!(
                one(&row, ".swarm-member-chip-status")
                    .get_bounding_client_rect()
                    .height()
                    > 0.0
            );
            assert!(
                row.get_bounding_client_rect().right()
                    <= view.get_bounding_client_rect().right() + 1.0
            );
        }
        container.style().set_property("width", "1100px").unwrap();
        swarm.revision = live_update.revision + 1;
        harness.swarm(&swarm);
        settle().await;

        all(&view, ".swarm-member-chip")
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
        assert_eq!(
            header_faces(&container),
            faces(&[
                ("Ada — Idle", false),
                ("Bo — Status unavailable", false),
                ("Cy — Ready to chat", false),
            ])
        );
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
        assert_eq!(header_faces(&container), faces(&[("Ada — Idle", false)]));
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
        let mut current = harness
            .state
            .swarms
            .get_untracked()
            .get(&harness.host)
            .and_then(|swarms| swarms.get(&SwarmId(sid.into())))
            .cloned()
            .expect("current swarm");
        current.threads.push(protocol::SwarmThread {
            swarm_id: current.id.clone(),
            thread_id: SwarmThreadId("r1".into()),
            board: SwarmBoard::Briefing,
            parent_thread_id: None,
            title: Some("Review documentation".into()),
            description: Some("Human review request".into()),
            naming_error: None,
            summary: "Documentation review in progress".into(),
            seq: 2,
            child_seq: 0,
            creation_cursor: 1,
        });
        harness.swarm(&current);
        let editing_thread = one(&container, "[data-thread-id='r1']");
        container.style().set_property("width", "1600px").unwrap();
        settle().await;
        let view_width = one(&container, ".swarm-view")
            .get_bounding_client_rect()
            .width();
        let thread_rect = editing_thread.get_bounding_client_rect();
        let root_composer_rect = one(
            &container,
            ".swarm-root-composer:not([hidden]) .swarm-composer",
        )
        .get_bounding_client_rect();
        assert!(
            thread_rect.width() >= view_width - 40.0,
            "threads use the workspace width, not a centered reading column"
        );
        assert!(
            root_composer_rect.width() >= view_width - 40.0,
            "the composer uses the same full workspace width"
        );
        assert!(
            (thread_rect.left() - root_composer_rect.left()).abs() <= 2.0,
            "thread and composer gutters align"
        );
        let header = one(&container, ".swarm-header").get_bounding_client_rect();
        let tabs = one(&container, "[role='tablist']").get_bounding_client_rect();
        assert!(
            tabs.bottom() - header.top() <= 105.0,
            "a summary header and slim tabs leave space for content"
        );
        wasm_bindgen_test::console_log!(
            "Swarm empty composer geometry: height={} children={:?}",
            root_composer_rect.height(),
            all(
                &one(
                    &container,
                    ".swarm-root-composer:not([hidden]) .swarm-composer"
                ),
                ":scope > *"
            )
            .iter()
            .map(|element| (
                element.tag_name(),
                element.class_name(),
                element.get_bounding_client_rect().height()
            ))
            .collect::<Vec<_>>()
        );
        assert!(
            root_composer_rect.height() <= 135.0,
            "an empty composer does not reserve a large block of screen space"
        );
        container.style().set_property("width", "420px").unwrap();
        settle().await;
        let view = one(&container, ".swarm-view");
        assert!(
            view.scroll_width() <= view.client_width() + 1,
            "dense controls stay inside a narrow pane"
        );
        for element in [
            editing_thread.clone(),
            one(
                &container,
                ".swarm-root-composer:not([hidden]) .swarm-composer",
            ),
            button(&container, "Post"),
        ] {
            let bounds = element.get_bounding_client_rect();
            assert!(
                bounds.right() <= view.get_bounding_client_rect().right() + 1.0,
                "narrow content does not run off screen"
            );
        }
        let root_composer = one(&container, ".swarm-root-composer:not([hidden])");
        button(&root_composer, "Attach open file").click();
        settle().await;
        let menu = one(&root_composer, ".swarm-attach-menu").get_bounding_client_rect();
        assert!(
            menu.left() >= view.get_bounding_client_rect().left()
                && menu.right() <= view.get_bounding_client_rect().right(),
            "attachment choices remain on screen in a narrow pane"
        );
        button(&root_composer, "Attach open file").click();
        container.style().set_property("width", "1100px").unwrap();
        settle().await;
        button(&editing_thread, "Reply").click();
        settle().await;
        let composer = one(&editing_thread, ".swarm-reply-composer");
        let draft_input = one(&composer, "textarea");
        // protocol::swarm_publication_recipients wakes every active member
        // for an unmentioned human reply on a human Briefing request.
        assert!(
            one(&composer, ".swarm-composer-send")
                .get_attribute("title")
                .unwrap_or_default()
                .starts_with("Notification recipients: Ada (idle), Bo (idle)."),
            "a reply to a human request previews the whole-swarm wake the host will send"
        );
        assert!(
            all(&composer, ".swarm-routing-preview").is_empty(),
            "deliverable routing stays out of the reply composer body"
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
            coordination_tab.is_same_node(
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
        let publication = publications[0]["post"].clone();
        assert!(
            publication["body"]
                == json!([{"kind":"text","text":"Draft "}, {"kind":"member_mention","member_id":"bo"}, {"kind":"text","text":" please review"}]),
            "live reply preserves the selected member occurrence"
        );
        assert!(
            publication["attachments"]
                == json!([{ "project_id": PROJECT, "path": attachment_path }]),
            "live reply preserves attachments"
        );
        assert_eq!(publication["thread_id"], "r1");

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
            harness.commands_of("post")[1]["post"] == publication,
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
        let agents_tab = button(&container, "Agents");
        assert!(agents_tab.id() != briefing_tab.id() && agents_tab.id() != coordination_tab.id());
        assert!(agents_tab.get_attribute("aria-controls").as_deref() == Some(panel.id().as_str()));
        for (key, target, expected) in [
            ("ArrowLeft", &coordination_tab, &briefing_tab),
            ("End", &briefing_tab, &agents_tab),
            ("ArrowRight", &agents_tab, &briefing_tab),
            ("ArrowLeft", &briefing_tab, &agents_tab),
            ("ArrowLeft", &agents_tab, &coordination_tab),
            ("ArrowRight", &coordination_tab, &agents_tab),
            ("Home", &agents_tab, &briefing_tab),
        ] {
            press(target, key);
            settle().await;
            assert_eq!(
                expected.get_attribute("aria-selected").as_deref(),
                Some("true")
            );
            assert_eq!(expected.tab_index(), 0);
            for other in [&briefing_tab, &coordination_tab, &agents_tab] {
                if !other.is_same_node(Some(expected)) {
                    assert_eq!(other.tab_index(), -1);
                }
            }
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
            harness.commands_of("post").last().unwrap()["post"] == publication,
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
            one(&orphan_composer, ".swarm-composer-send")
                .get_attribute("title")
                .unwrap_or_default()
                .starts_with("Notification recipients: Ada (idle), Bo (idle)."),
            "reply preview combines the real root author with the selected member"
        );
        assert!(
            all(&orphan_composer, ".swarm-routing-preview").is_empty(),
            "the unavailable-preview warning clears once the root arrives"
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
        // Who a post notifies is always one hover away on Post; the composer
        // only spells it out when routing needs attention.
        let notification_preview = || {
            one(&root_composer, ".swarm-composer-send")
                .get_attribute("title")
                .unwrap_or_default()
        };
        let routing_line = || {
            all(&root_composer, ".swarm-routing-preview")
                .first()
                .map(|line| text_of(line))
                .unwrap_or_default()
        };
        assert!(
            notification_preview()
                .starts_with("Notification recipients: Ada (idle), Alan (idle), Bo (idle)."),
            "Briefing without selected mentions previews the canonical broadcast recipients"
        );
        assert_eq!(
            routing_line(),
            "",
            "healthy routing does not add a permanent line under the composer"
        );
        assert_eq!(
            input.get_attribute("placeholder").as_deref(),
            Some("Message the swarm… (@ to mention)")
        );
        input.focus().unwrap();
        settle().await;
        let input_style = web_sys::window()
            .unwrap()
            .get_computed_style(&input)
            .unwrap()
            .unwrap();
        assert_eq!(
            input_style.get_property_value("outline-style").unwrap(),
            "none",
            "the input does not draw a second focus ring inside the composer"
        );
        let composer_style = web_sys::window()
            .unwrap()
            .get_computed_style(&one(&root_composer, ".swarm-composer"))
            .unwrap()
            .unwrap();
        let post_style = web_sys::window()
            .unwrap()
            .get_computed_style(&button(&root_composer, "Post"))
            .unwrap()
            .unwrap();
        assert_eq!(
            composer_style
                .get_property_value("border-top-color")
                .unwrap(),
            post_style.get_property_value("background-color").unwrap(),
            "the unified composer focus boundary uses the visible primary accent"
        );
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
        let publication = &posts[0]["post"];
        assert_eq!(publication["thread_id"], Value::Null);
        assert_eq!(
            publication["body"],
            json!([
                {"kind": "text", "text": "cc @Ada and "},
                {"kind": "member_mention", "member_id": "alan"},
                {"kind": "text", "text": " please review"},
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
            posts[1]["post"]["publication_id"], publication["publication_id"],
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
            routing_line().contains("Needs attention; posting does not resume delivery."),
            "a stalled swarm surfaces its routing consequence in the composer"
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
        let duplicate_publication = duplicate_commands.last().unwrap()["post"].clone();
        assert!(
            duplicate_publication["body"]
                == json!([
                    {"kind":"text","text":"🧭 literal @Nova; selected "},
                    {"kind":"member_mention","member_id":"nova-a"},
                    {"kind":"text","text":" and "},
                    {"kind":"member_mention","member_id":"nova-b"},
                    {"kind":"text","text":" literal again @Nova"}
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
            routing_line().starts_with("Notification recipients: Nova — undeliverable (retiring)."),
            "selected retiring reference remains an undeliverable recipient, not a replacement"
        );
        same_names.members[1].state = SwarmMemberState::Retired;
        same_names.members[1].agent_id = None;
        same_names.members[1].runtime_status = None;
        same_names.lifecycle = SwarmLifecycle::Paused;
        harness.swarm(&same_names);
        settle().await;
        assert!(
            routing_line().starts_with("Notification recipients: Nova — undeliverable (retired).")
        );
        assert!(routing_line().contains("Paused; posting does not resume delivery."));
        assert!(
            one(&root_composer, ".swarm-routing-warning")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        press(&input, "Escape");
        press(&input, "Enter");
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["post"]["body"]
                == json!([
                    {"kind":"text","text":"🧭 literal @Nova; selected @Nova and "},
                    {"kind":"member_mention","member_id":"nova-b"},
                    {"kind":"text","text":" literal again @Nova"}
                ]),
            "only the untouched occurrence stays a mention after same-text replacement"
        );

        let briefing_publication = harness.commands_of("post").last().unwrap()["post"].clone();
        assert_eq!(
            briefing_publication["thread_id"],
            Value::Null,
            "the board composer opens a new request"
        );
        button(&root_composer, "Edit").click();
        settle().await;
        let briefing_draft = textarea.value();
        let briefing_tab = one(&container, "[role='tab'][data-board='Briefing']");
        let coordination_tab = one(&container, "[role='tab'][data-board='Coordination']");
        press(&briefing_tab, "ArrowRight");
        settle().await;
        assert!(
            all(&container, ".swarm-root-composer")
                .iter()
                .all(|composer| composer.get_bounding_client_rect().height() == 0.0),
            "humans open requests on Briefing only; Coordination has no new-thread composer"
        );
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
        let resent = harness.commands_of("post").last().unwrap()["post"].clone();
        assert!(
            resent["body"] == briefing_publication["body"],
            "the unsent draft keeps the exact selected member occurrence across tab switches"
        );
        assert!(textarea.read_only());
        button(&root_composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["post"] == resent,
            "retry resends the entire original publication identity and typed body"
        );
        let mut accepted = make_post(
            sid,
            "board-briefing-accepted",
            "board-briefing-accepted",
            SwarmBoard::Briefing,
            3,
            SwarmAuthor::Human,
            serde_json::from_value(resent["body"].clone()).unwrap(),
        );
        accepted.publication_id =
            SwarmPublicationId(resent["publication_id"].as_str().unwrap().to_owned());
        harness.post(&accepted);
        settle().await;
        assert!(
            textarea.value().is_empty() && !textarea.read_only(),
            "canonical acceptance clears the draft"
        );
        assert!(all(&root_composer, ".swarm-mention-chip").is_empty());
    }

    #[wasm_bindgen_test]
    async fn shared_images_can_be_added_previewed_and_published_on_boards_and_replies() {
        use protocol::{
            ImageData, SwarmFailure, SwarmImage, SwarmImageId, SwarmImageNotifyPayload,
            SwarmImageOutcome,
        };
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAMCAIAAADkharWAAAAFUlEQVR4nGPg3fKVJMQwqmFUA3YEAK1USJCnamHcAAAAAElFTkSuQmCC";
        fn add_files(target: &HtmlElement, method: &str, names: &[&str], png: &str) {
            js_sys::Reflect::set(
                &js_sys::global(),
                &"__test_image_target".into(),
                target.as_ref(),
            )
            .unwrap();
            js_sys::eval(&format!(r#"(() => {{
                const target = window.__test_image_target;
                const transfer = new DataTransfer();
                const bytes = Uint8Array.from(atob({png}), c => c.charCodeAt(0));
                for (const name of {names}) transfer.items.add(new File([bytes], name, {{type:'image/png'}}));
                if ({method} === 'picker') {{
                    const input = target.querySelector('input[type=file]');
                    input.files = transfer.files;
                    input.dispatchEvent(new Event('change', {{bubbles:true}}));
                }} else if ({method} === 'paste') {{
                    target.querySelector('textarea').dispatchEvent(new ClipboardEvent('paste', {{clipboardData:transfer,bubbles:true,cancelable:true}}));
                }} else {{
                    target.dispatchEvent(new DragEvent('dragover', {{dataTransfer:transfer,bubbles:true,cancelable:true}}));
                    target.dispatchEvent(new DragEvent('drop', {{dataTransfer:transfer,bubbles:true,cancelable:true}}));
                }}
            }})()"#, png=serde_json::to_string(png).unwrap(), names=serde_json::to_string(names).unwrap(), method=serde_json::to_string(method).unwrap())).expect("real File, FileReader and input events");
        }
        async fn uploads(harness: &Harness, count: usize) -> Vec<protocol::SwarmImageUpload> {
            for _ in 0..40 {
                settle().await;
                if harness.commands_of("upload_image").len() >= count {
                    break;
                }
            }
            let uploads = harness.commands_of("upload_image");
            assert_eq!(
                uploads.len(),
                count,
                "every selected file crosses the host boundary exactly once"
            );
            uploads
                .into_iter()
                .map(|command| {
                    let upload: protocol::SwarmImageUpload =
                        serde_json::from_value(command["image"].clone()).unwrap();
                    assert!(
                        js_sys::RegExp::new(
                            "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
                            ""
                        )
                        .test(&upload.image_id.0),
                        "the real composer sends a UUID accepted by the host image store"
                    );
                    upload
                })
                .collect()
        }
        fn metadata(upload: &protocol::SwarmImageUpload) -> SwarmImage {
            SwarmImage {
                id: upload.image_id.clone(),
                name: upload.name.clone(),
                media_type: upload.data.media_type.clone(),
                width: 16,
                height: 12,
                byte_len: 78,
            }
        }
        fn emit(harness: &Harness, sid: &str, image_id: SwarmImageId, outcome: SwarmImageOutcome) {
            harness.emit(
                FrameKind::SwarmImageNotify,
                &SwarmImageNotifyPayload {
                    swarm_id: SwarmId(sid.into()),
                    image_id,
                    outcome,
                },
            );
        }
        fn ready(harness: &Harness, sid: &str, upload: &protocol::SwarmImageUpload, pixels: bool) {
            emit(
                harness,
                sid,
                upload.image_id.clone(),
                SwarmImageOutcome::Ready {
                    image: metadata(upload),
                    data: pixels.then(|| upload.data.clone()),
                },
            );
        }
        let harness = Harness::new("host-swarm-images");
        let sid = "sw-images";
        harness.swarm(&make_swarm(sid, "Images", vec![idle("ada", "Ada")]));
        let (container, _handle) = mount_view(&harness, sid);
        harness.board_page(page(sid, SwarmBoard::Briefing, Vec::new(), 0, false));
        settle().await;
        let composer = one(&container, ".swarm-root-composer:not([hidden])");
        assert!(
            button(&composer, "Add images")
                .get_bounding_client_rect()
                .height()
                > 0.0
        );
        add_files(
            &composer,
            "picker",
            &["one.png", "remove.png", "two.png"],
            PNG,
        );
        let selected = uploads(&harness, 3).await;
        assert!(selected.iter().all(|image| image.data
            == ImageData {
                media_type: "image/png".into(),
                data: PNG.into()
            }));
        assert_eq!(all(&composer, "figure img").len(), 3);
        one(&composer, "[aria-label='Remove image remove.png']").click();
        settle().await;
        assert_eq!(all(&composer, "figure img").len(), 2);
        assert!(
            button(&composer, "Post").has_attribute("disabled"),
            "posting waits for host durability"
        );
        ready(&harness, sid, &selected[0], false);
        emit(
            &harness,
            sid,
            selected[2].image_id.clone(),
            SwarmImageOutcome::Failed {
                error: SwarmFailure {
                    code: SwarmErrorCode::Storage,
                    message: "Upload storage unavailable".into(),
                },
            },
        );
        settle().await;
        assert!(text_of(&composer).contains("Upload storage unavailable"));
        button(&composer, "Retry image").click();
        let retried = uploads(&harness, 4).await;
        assert!(
            retried[3] == selected[2],
            "upload retries preserve identity and exact bytes"
        );
        ready(&harness, sid, &selected[2], false);
        settle().await;
        assert!(
            !button(&composer, "Post").has_attribute("disabled"),
            "image-only posts are supported"
        );
        one(&container, "[data-board='Coordination']").click();
        harness.board_page(page(sid, SwarmBoard::Coordination, Vec::new(), 0, false));
        settle().await;
        button(&container, "Agents").click();
        settle().await;
        one(&container, "[data-board='Briefing']").click();
        settle().await;
        assert_eq!(
            all(&composer, "figure img").len(),
            2,
            "board and Agents switches preserve image drafts"
        );
        button(&composer, "Post").click();
        settle().await;
        let publication: protocol::SwarmHumanPost =
            serde_json::from_value(harness.commands_of("post").last().unwrap()["post"].clone())
                .unwrap();
        assert!(publication.body.is_empty() && publication.thread_id.is_none());
        assert_eq!(
            publication.images,
            vec![selected[0].image_id.clone(), selected[2].image_id.clone()]
        );
        harness.error(
            sid,
            Some(&publication.publication_id),
            SwarmErrorCode::Storage,
            "Post not persisted",
        );
        settle().await;
        assert_eq!(
            all(&composer, "figure img").len(),
            2,
            "failed posts retain all images"
        );
        button(&composer, "Retry").click();
        settle().await;
        assert!(
            harness.commands_of("post").last().unwrap()["post"]
                == serde_json::to_value(&publication).unwrap()
        );
        let mut posted = make_post(
            sid,
            "pictures",
            "pictures",
            SwarmBoard::Briefing,
            1,
            SwarmAuthor::Human,
            Vec::new(),
        );
        posted.publication_id = publication.publication_id.clone();
        posted.images = vec![metadata(&selected[0]), metadata(&selected[2])];
        harness.post(&posted);
        let mut current = harness
            .state
            .swarms
            .get_untracked()
            .get(&harness.host)
            .and_then(|swarms| swarms.get(&SwarmId(sid.into())))
            .cloned()
            .expect("current swarm");
        current.threads.push(protocol::SwarmThread {
            swarm_id: current.id.clone(),
            thread_id: posted.thread_id.clone(),
            board: posted.board,
            parent_thread_id: None,
            title: Some("Image request".to_owned()),
            description: Some(String::new()),
            naming_error: None,
            summary: String::new(),
            seq: 1,
            child_seq: 0,
            creation_cursor: posted.cursor,
        });
        harness.swarm(&current);
        harness.board_page(page(
            sid,
            SwarmBoard::Briefing,
            vec![posted.clone()],
            1,
            false,
        ));
        settle().await;
        assert!(
            all(&composer, "figure img").is_empty(),
            "only canonical publication clears the draft"
        );
        assert_eq!(harness.commands_of("read_image").len(), 2);
        emit(
            &harness,
            sid,
            selected[0].image_id.clone(),
            SwarmImageOutcome::Failed {
                error: SwarmFailure {
                    code: SwarmErrorCode::Storage,
                    message: "Shared pixels unavailable".into(),
                },
            },
        );
        settle().await;
        let failed_image = one(&container, ".swarm-shared-image");
        assert!(text_of(&failed_image).contains("Shared pixels unavailable"));
        button(&failed_image, "Retry image").click();
        settle().await;
        assert_eq!(harness.commands_of("read_image").len(), 3);
        ready(&harness, sid, &selected[0], true);
        ready(&harness, sid, &selected[2], true);
        settle().await;
        let thread = one(&container, ".swarm-thread");
        assert_eq!(all(&thread, ".swarm-shared-image img").len(), 2);
        for thumb in all(&thread, ".swarm-shared-image img") {
            let img = thumb.dyn_into::<web_sys::HtmlImageElement>().unwrap();
            assert!(
                img.complete() && img.natural_width() == 16,
                "posted pixels decode in the real browser"
            );
        }
        ready(&harness, sid, &selected[0], false);
        settle().await;
        assert_eq!(
            all(&thread, ".swarm-shared-image img").len(),
            2,
            "a duplicate upload acknowledgement cannot erase loaded pixels"
        );
        one(&thread, "[aria-label='Open image one.png']").click();
        settle().await;
        let document = web_sys::window().unwrap().document().unwrap();
        assert!(
            document
                .query_selector("[aria-label='Image viewer']")
                .unwrap()
                .is_some()
        );
        document
            .query_selector("[aria-label='Close image viewer']")
            .unwrap()
            .unwrap()
            .dyn_into::<HtmlElement>()
            .unwrap()
            .click();
        button(&thread, "Reply").click();
        settle().await;
        let reply = one(&thread, ".swarm-composer-reply");
        add_files(&reply, "paste", &["clipboard.png"], PNG);
        let pasted = uploads(&harness, 5).await;
        add_files(&reply, "drop", &["drop-a.png", "drop-b.png"], PNG);
        let dropped = uploads(&harness, 7).await;
        ready(&harness, sid, &pasted[4], false);
        ready(&harness, sid, &dropped[5], false);
        ready(&harness, sid, &dropped[6], false);
        settle().await;
        button(&reply, "Reply").click();
        settle().await;
        let replied: protocol::SwarmHumanPost =
            serde_json::from_value(harness.commands_of("post").last().unwrap()["post"].clone())
                .unwrap();
        assert!(
            replied.thread_id == Some(posted.thread_id)
                && replied.images
                    == vec![
                        pasted[4].image_id.clone(),
                        dropped[5].image_id.clone(),
                        dropped[6].image_id.clone()
                    ]
        );
        assert!(
            harness
                .command_hosts()
                .iter()
                .all(|host| host == &harness.host),
            "all media stays on the swarm's selected host"
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

        button(&container, "Agents").click();
        settle().await;
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
        assert!(
            harness.commands_of("mark_read").is_empty(),
            "loading a hidden board while viewing Agents does not mark it read"
        );
        assert_eq!(
            text_of(&one(
                &container,
                "[data-board='Briefing'] .swarm-unread-badge"
            )),
            "2"
        );
        one(&container, "[data-board='Briefing']").click();
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
        let starting_chip = one(&container, ".swarm-member-chip");
        assert_eq!(
            text_of(&one(&starting_chip, ".swarm-member-chip-status")),
            "Starting",
            "a member activated by the first request shows its server state"
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

        let linking_thread = one(&container, "[data-thread-id='dl-new']");
        button(&linking_thread, "Reply").click();
        settle().await;
        let composer = one(&linking_thread, ".swarm-reply-composer");
        let input = one(&composer, "textarea");
        input.focus().unwrap();
        type_into(&input, "See  for context");
        settle().await;
        let routing = one(&composer, ".swarm-composer-send")
            .get_attribute("title")
            .unwrap_or_default();
        assert!(
            routing.starts_with("Notification recipients: Ada"),
            "a human reply notifies the member who opened the thread: {routing}"
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
        assert_eq!(
            one(&composer, ".swarm-composer-send")
                .get_attribute("title")
                .unwrap_or_default(),
            routing,
            "post links do not become mentions or notify linked authors"
        );
        button(&composer, "Reply").click();
        settle().await;
        let commands = harness.commands_of("post");
        let publication = &commands.last().unwrap()["post"];
        assert_eq!(publication["thread_id"], "dl-new");
        assert!(
            publication["body"]
                == json!([
                    {"kind":"text","text":"See "},
                    {"kind":"post_link","post_id":"dl-old-reply"},
                    {"kind":"text","text":"  for context"}
                ]),
            "human picker inserts the exact cross-board post ID at the caret without converting surrounding text"
        );
        let mut human_link = make_post(
            sid,
            "dl-human",
            "dl-new",
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
    #[wasm_bindgen_test]
    async fn human_threads_show_naming_links_results_and_collapsed_agent_replies() {
        let harness = Harness::new("host-stateful-thread");
        let sid = "stateful-thread";
        let mut swarm = make_swarm(sid, "Stateful", vec![idle("ada", "Ada")]);
        let thread =
            |id: &str, board, parent: Option<&str>, title: Option<&str>| protocol::SwarmThread {
                swarm_id: SwarmId(sid.into()),
                thread_id: SwarmThreadId(id.into()),
                board,
                parent_thread_id: parent.map(|parent| SwarmThreadId(parent.into())),
                title: title.map(str::to_owned),
                description: title.map(|_| "A human request".to_owned()),
                naming_error: None,
                summary: String::new(),
                seq: 1,
                child_seq: 0,
                creation_cursor: 1,
            };
        swarm
            .threads
            .push(thread("human-root", SwarmBoard::Briefing, None, None));
        swarm.threads[0].naming_error = Some("Helper offline".into());
        harness.swarm(&swarm);
        let root = make_post(
            sid,
            "human-root",
            "human-root",
            SwarmBoard::Briefing,
            1,
            SwarmAuthor::Human,
            vec![text("Check the rendering")],
        );
        harness.board_page(page(
            sid,
            SwarmBoard::Briefing,
            vec![root.clone()],
            1,
            false,
        ));
        settle().await;
        let (container, _handle) = mount_view(&harness, sid);
        settle().await;
        let head = one(&container, ".swarm-thread-head");
        assert!(
            text_of(&head).contains("Naming…")
                && text_of(&head).contains("Naming failed, retrying: Helper offline"),
            "an unnamed request shows the server's naming state: {}",
            text_of(&head)
        );
        assert!(
            container
                .query_selector(".swarm-thread-summary")
                .unwrap()
                .is_none(),
            "an empty summary renders nothing"
        );

        swarm.threads[0].title = Some("Investigate rendering".into());
        swarm.threads[0].description = Some("A human request".into());
        swarm.threads[0].naming_error = None;
        swarm.threads[0].summary = "Audit in progress".into();
        swarm.threads[0].seq = 2;
        swarm.threads[0].child_seq = 1;
        let mut child = thread(
            "child-1",
            SwarmBoard::Coordination,
            Some("human-root"),
            Some("Build logs"),
        );
        child.creation_cursor = 2;
        swarm.threads.push(child);
        harness.swarm(&swarm);
        settle().await;
        let head = one(&container, ".swarm-thread-head");
        assert_eq!(text_of(&one(&head, "h3")), "Investigate rendering");
        assert!(!text_of(&head).contains("Naming"));
        assert_eq!(
            text_of(&one(&head, ".swarm-thread-summary")),
            "Audit in progress"
        );
        let notes = one(&head, "details");
        assert!(
            text_of(&one(&notes, "summary")) == "Agent notes" && !notes.has_attribute("open"),
            "the agents' working notes stay collapsed under the title"
        );
        assert!(
            !text_of(&container).contains("A human request"),
            "the generated thread description is not shown"
        );
        assert!(
            has_button(&head, "↳ Coordination: Build logs"),
            "the coordination thread is a chip beside the title: {}",
            text_of(&head)
        );
        assert!(
            !text_of(&container).contains("sequence"),
            "internal sequence numbers are not shown"
        );
        swarm.threads[0].summary = "Correction deployed".into();
        swarm.threads[0].seq = 3;
        harness.swarm(&swarm);
        settle().await;
        assert!(
            text_of(&container).contains("Correction deployed")
                && !text_of(&container).contains("Audit in progress"),
            "the current state changes from server events without reconstructing deltas"
        );

        let thread_card = one(&container, ".swarm-thread");
        button(&thread_card, "Reply").click();
        settle().await;
        let composer = one(&thread_card, ".swarm-composer-reply");
        let input = one(&composer, ".swarm-composer-input");
        type_into(&input, "Verified in the running instance");
        settle().await;
        button(&composer, "Reply").click();
        settle().await;
        let command = harness.commands_of("post").last().unwrap()["post"].clone();
        assert!(
            command["thread_id"] == "human-root"
                && command.get("thread_change").is_none()
                && command["body"]
                    == json!([{"kind": "text", "text": "Verified in the running instance"}]),
            "the human sends only the reply; the server owns the summary update: {command}"
        );
        let publication_id =
            SwarmPublicationId(command["publication_id"].as_str().unwrap().to_owned());
        let mut committed = make_post(
            sid,
            "reply-1",
            "human-root",
            SwarmBoard::Briefing,
            3,
            SwarmAuthor::Human,
            vec![text("Verified in the running instance")],
        );
        committed.publication_id = publication_id;
        harness.post(&committed);
        settle().await;
        assert!(
            all(&thread_card, ".swarm-composer-reply").is_empty(),
            "the host recorded the reply as posted, so the reply composer closes"
        );

        // Agent discussion after a human post collapses until its result; each
        // result is shown inline where it happened, so a follow-up gets its own.
        let agent_reply = |id: &str, cursor: u64, words: &str, result: bool| {
            let mut post = make_post(
                sid,
                id,
                "human-root",
                SwarmBoard::Briefing,
                cursor,
                by("ada"),
                vec![text(words)],
            );
            post.result = result;
            post
        };
        for post in [
            agent_reply("chat-1", 4, "Looking at the renderer", false),
            agent_reply("chat-2", 5, "Found the double newline", false),
            agent_reply("result-1", 6, "Fixed: rows render once", true),
            make_post(
                sid,
                "follow-up",
                "human-root",
                SwarmBoard::Briefing,
                7,
                SwarmAuthor::Human,
                vec![text("Also check wrapping")],
            ),
            agent_reply("chat-3", 8, "Checking wrapping", false),
            agent_reply("result-2", 9, "Wrapping is correct", true),
        ] {
            harness.post(&post);
        }
        settle().await;
        let replies = one(&container, ".swarm-replies");
        let children = replies.children();
        let shown: Vec<String> = (0..children.length())
            .filter_map(|index| children.item(index))
            .map(|item| match item.query_selector("summary").unwrap() {
                Some(summary) => {
                    format!("[{}]", text_of(&summary.last_element_child().unwrap()))
                }
                None => item.id(),
            })
            .collect();
        assert_eq!(
            shown,
            [
                "swarm-post-reply-1",
                "[2 agent replies]",
                "swarm-post-result-1",
                "swarm-post-follow-up",
                "[1 agent reply]",
                "swarm-post-result-2",
            ],
            "results stay in time order and only the discussion before each collapses"
        );
        for group in all(&replies, "details") {
            assert!(!group.has_attribute("open"));
            let summary = group.query_selector("summary").unwrap().unwrap();
            assert_eq!(
                group.get_bounding_client_rect().height(),
                summary.get_bounding_client_rect().height(),
                "a collapsed group shows only its summary line"
            );
        }
        for result in ["#swarm-post-result-1", "#swarm-post-result-2"] {
            let card = one(&container, result);
            assert!(
                text_of(&card).contains("✓ Result")
                    && card.get_bounding_client_rect().height() > 0.0,
                "a result is labeled and visible without expanding anything"
            );
        }
        assert!(
            !text_of(&one(&container, "#swarm-post-follow-up")).contains("✓ Result"),
            "a human follow-up is never labeled a result"
        );
        let first_group = all(&replies, "details").remove(0);
        one(&first_group, "summary").click();
        settle().await;
        assert!(
            one(&first_group, "#swarm-post-chat-2")
                .get_bounding_client_rect()
                .height()
                > 0.0,
            "expanding the discussion reveals the agents' replies"
        );
        harness.post(&agent_reply("chat-4", 10, "One more note", false));
        settle().await;
        assert!(
            text_of(&one(
                &container,
                ".swarm-replies > details:last-child summary"
            ))
            .contains("1 agent reply"),
            "new discussion after the latest result starts a fresh collapsed group"
        );

        button(&one(&container, "#swarm-post-chat-1"), "Link").click();
        settle().await;
        let composer = one(&thread_card, ".swarm-composer-reply");
        assert_eq!(
            one(&composer, "textarea")
                .dyn_ref::<web_sys::HtmlTextAreaElement>()
                .unwrap()
                .value(),
            "↪ Ada: Looking at the renderer ",
            "Link on a post inserts a link to it in the thread reply"
        );
        assert!(
            container
                .query_selector("#swarm-post-reply-1")
                .unwrap()
                .is_some()
        );

        one(&container, "[data-board='Coordination']").click();
        let child_root = make_post(
            sid,
            "child-1",
            "child-1",
            SwarmBoard::Coordination,
            2,
            by("ada"),
            vec![text("Coordinating build logs")],
        );
        harness.board_page(page(
            sid,
            SwarmBoard::Coordination,
            vec![child_root],
            2,
            false,
        ));
        settle().await;
        let coordination = one(&container, ".swarm-thread");
        assert_eq!(text_of(&one(&coordination, "h3")), "Build logs");
        let parent_link = button(&coordination, "Re: Investigate rendering");
        parent_link.click();
        settle().await;
        assert_eq!(
            one(&container, "[role='tab'][data-board='Briefing']")
                .get_attribute("aria-selected")
                .as_deref(),
            Some("true"),
            "Re: opens the parent request"
        );
    }
}
