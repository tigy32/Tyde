use base64::Engine;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::bridge::Accepted;
use crate::send::SendFrameError;
use crate::state::{
    AgentRef, AppState, LocalHostId, PendingSubmission, PendingSubmissionState, SubmissionTarget,
};

const CHAT_INPUT_MIN_HEIGHT_PX: i32 = 36;
const CHAT_INPUT_MAX_HEIGHT_PX: i32 = 132;

/// Visible recovery copy shown once the composer target has died. Kept
/// character-identical to the desktop composer
/// (`frontend/src/components/chat_input.rs`) so the two clients name the same
/// three routes in the same words — a user who learns the phrasing on one
/// should recognise it on the other.
const TERMINATED_COMPOSER_HINT: &str =
    "Agent stopped — use Fork + send, Resume in Sessions, or New Chat";
/// Sentence-form variant of [`TERMINATED_COMPOSER_HINT`] for `aria-label`,
/// where the text is read aloud rather than skimmed.
const TERMINATED_COMPOSER_LABEL: &str =
    "Agent stopped. Use Fork + send, Resume in Sessions, or New Chat.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingImage {
    name: String,
    media_type: String,
    data: String,
}

/// Visually hidden, still announced. Inline because this is an accessibility
/// invariant of the composer, not a theming choice.
const VISUALLY_HIDDEN: &str = "position:absolute;width:1px;height:1px;padding:0;margin:-1px;\
     overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0;";

/// What the composer announces when a submission is admitted.
///
/// Sighted users see the composer empty itself. A screen-reader user would
/// otherwise just find their text gone, so the move is announced politely.
///
/// It says **queued**, not sent: admission means the frame entered this
/// connection's outbound queue, and the client has no basis for claiming the
/// host received it. A `polite` status, never an alert — the happy path must not
/// interrupt, and it leaves no artifact to dismiss.
const QUEUED_ANNOUNCEMENT: &str = "Message queued to send.";

#[derive(Clone, Debug, PartialEq, Eq)]
struct QueuedRowRef {
    agent_ref: AgentRef,
    id: protocol::QueuedMessageId,
}

/// One outbound user submission as the composer sees it: where it is going, and
/// what the user put in it.
///
/// Bundled rather than passed as loose parameters — the destination, the text,
/// and the attachments travel together or they are not a submission, and
/// splitting them into a seven-argument call is what invites a caller to pass
/// the host of one message with the text of another.
struct OutboundSubmission {
    local_host_id: LocalHostId,
    target: SubmissionTarget,
    text: String,
    images: Vec<protocol::ImageData>,
}

/// The composer's own handles: its text and photos, the live region that
/// announces a move, and the in-flight latch.
///
/// `Copy`, so it can be handed to a `spawn_local` future without cloning
/// ceremony at every call site.
#[derive(Clone, Copy)]
struct Composer {
    textarea: NodeRef<leptos::html::Textarea>,
    images: RwSignal<Vec<PendingImage>>,
    attachment_error: RwSignal<Option<String>>,
    announcement: RwSignal<String>,
    /// Set while a submission is unsettled.
    ///
    /// This is a real guard, not a side effect. Before the fix, the composer was
    /// cleared *before* the send was awaited, so a second tap read an empty box
    /// and fell out of `if text.is_empty()`. Preserving the user's text across
    /// the in-flight window — the whole point — removed that accident. Nothing
    /// then stopped a double-tap from emitting two `SpawnAgent` frames, which is
    /// two agents, two backend sessions, and two paid turns.
    ///
    /// It does not rely on the send resolving in the same microtask. It holds
    /// even when the send genuinely yields.
    submitting: RwSignal<bool>,
}

impl Composer {
    fn new() -> Self {
        Self {
            textarea: NodeRef::new(),
            images: RwSignal::new(Vec::new()),
            attachment_error: RwSignal::new(None),
            announcement: RwSignal::new(String::new()),
            submitting: RwSignal::new(false),
        }
    }

    fn is_busy(&self) -> bool {
        self.submitting.get_untracked()
    }

    fn begin(&self) {
        self.submitting.set(true);
    }

    fn finish(&self) {
        self.submitting.set(false);
    }

    /// Empty the composer. Called only from [`settle_submission`], and only once
    /// the complete submission has a holder.
    fn clear(&self, state: &AppState) {
        state.chat_input.set(String::new());
        self.images.set(Vec::new());
        self.attachment_error.set(None);
        if let Some(textarea) = self.textarea.get_untracked() {
            textarea.set_value("");
            resize_chat_input(&textarea);
        }
    }

    fn image_payload(&self) -> Vec<protocol::ImageData> {
        self.images.with_untracked(|images| {
            images
                .iter()
                .map(|image| protocol::ImageData {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect()
        })
    }
}

fn selected_backend_kind(state: &AppState) -> Option<protocol::BackendKind> {
    if let Some(active) = state.active_agent.get_untracked()
        && let Some(kind) = state.agents.with_untracked(|agents| {
            agents
                .iter()
                .find(|agent| {
                    agent.local_host_id == active.local_host_id && agent.agent_id == active.agent_id
                })
                .map(|agent| agent.backend_kind)
        })
    {
        return Some(kind);
    }

    state
        .draft_backend_override
        .get_untracked()
        .or_else(|| {
            state
                .active_host_settings_untracked()
                .and_then(|settings| settings.default_backend)
        })
        .or_else(|| {
            state
                .active_host_settings_untracked()
                .and_then(|settings| settings.enabled_backends.first().copied())
        })
}

async fn read_image_file(file: web_sys::File) -> Result<PendingImage, String> {
    let name = file.name();
    let media_type = file.type_();
    if !media_type.starts_with("image/") {
        return Err(format!("{name} is not an image file"));
    }

    let buffer = JsFuture::from(file.array_buffer()).await.map_err(|error| {
        error
            .as_string()
            .unwrap_or_else(|| format!("Failed to read {name}"))
    })?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    Ok(PendingImage {
        name,
        media_type,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn queued_message_preview(entry: &protocol::QueuedMessageEntry) -> String {
    let mut preview = entry.message.trim().to_string();
    if preview.is_empty() {
        preview = match entry.images.len() {
            0 => "Queued message".to_owned(),
            1 => "Image attachment".to_owned(),
            count => format!("{count} image attachments"),
        };
    } else if !entry.images.is_empty() {
        let suffix = if entry.images.len() == 1 {
            "image"
        } else {
            "images"
        };
        preview.push_str(&format!(" (+{} {suffix})", entry.images.len()));
    }

    let chars: Vec<char> = preview.chars().collect();
    if chars.len() > 80 {
        chars[..80].iter().collect::<String>() + "…"
    } else {
        preview
    }
}

/// The instance stream of the active agent, **only while it can still be
/// addressed**.
///
/// After `AgentError { fatal: true }` the stream is dead and no frame sent to
/// it will ever be answered. This is the single capability choke point for the
/// composer's Send, Steer, and Interrupt: returning `None` here rejects a stale
/// click or keyboard shortcut that a reactive `disabled` attribute had not yet
/// caught up with. Rendering is guarded separately; a disabled control is not a
/// transport guard.
fn active_agent_stream(
    state: &AppState,
    active: &crate::state::ActiveAgentRef,
) -> Option<protocol::StreamPath> {
    state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|a| a.local_host_id == active.local_host_id && a.agent_id == active.agent_id)
            .filter(|a| a.fatal_error.is_none())
            .map(|a| a.instance_stream.clone())
    })
}

/// True when the exact `AgentRef` is terminated by a fatal error. Host-scoped,
/// so a same-named agent on another host is never implicated.
pub(crate) fn agent_ref_is_fatal(state: &AppState, agent_ref: &AgentRef) -> bool {
    state.agents.with_untracked(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == agent_ref.local_host_id
                && agent.agent_id == agent_ref.agent_id
                && agent.fatal_error.is_some()
        })
    })
}

/// True when the exact active agent is terminated by a fatal error.
fn active_agent_is_terminated_tracked(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get() else {
        return false;
    };
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && agent.fatal_error.is_some()
        })
    })
}

/// Untracked twin of [`active_agent_is_terminated_tracked`] for action guards,
/// which must not subscribe the caller to the agent list.
fn active_agent_is_terminated(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get_untracked() else {
        return false;
    };
    state.agents.with_untracked(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && agent.fatal_error.is_some()
        })
    })
}

fn active_agent_is_running_tracked(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get() else {
        return false;
    };
    // Fatal wins before `agent_turn_active` is consulted. The reducer normally
    // clears the turn, but a stale or late-arriving turn flag must never render
    // Cancel/Queue on an agent that cannot receive either.
    if active_agent_is_terminated_tracked(state) {
        return false;
    }
    let agent_ref = active.as_agent_ref();
    if state
        .agent_turn_active
        .with(|turns| turns.get(&agent_ref).copied().unwrap_or(false))
    {
        return true;
    }
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && !agent.started
                && agent.fatal_error.is_none()
        })
    })
}

/// True when the active agent has reported a backend session id, which is
/// required to fork via "Fork + send".
fn active_agent_has_session_id_tracked(state: &AppState) -> bool {
    let Some(active) = state.active_agent.get() else {
        return false;
    };
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.local_host_id == active.local_host_id
                && agent.agent_id == active.agent_id
                && agent.session_id.is_some()
        })
    })
}

#[component]
fn QueuedMessageControlRow(row: QueuedRowRef) -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let preview_agent = row.agent_ref.clone();
    let preview_id = row.id.clone();
    let preview_state = state.clone();
    let preview = move || {
        preview_state.agent_message_queue.with(|queues| {
            queues
                .get(&preview_agent)
                .and_then(|entries| entries.iter().find(|entry| entry.id == preview_id))
                .map(queued_message_preview)
                .unwrap_or_default()
        })
    };

    let send_now_agent = row.agent_ref.clone();
    let send_now_id = row.id.clone();
    let send_now_state = state.clone();
    let on_send_now = move |_| {
        let state = send_now_state.clone();
        let agent_ref = send_now_agent.clone();
        let id = send_now_id.clone();
        // "Send Now" is a same-actor send. The row is already hidden for a dead
        // owner, so this only catches a click that was already in flight when
        // the fatal error landed — but a hidden control is not a guard.
        if agent_ref_is_fatal(&state, &agent_ref) {
            return;
        }
        spawn_local(async move {
            if let Err(error) =
                crate::actions::send_queued_message_now(&state, &agent_ref, id).await
            {
                report_send_error(
                    &state,
                    format!("Failed to send queued message now: {error}"),
                );
            }
        });
    };

    let delete_agent = row.agent_ref;
    let delete_id = row.id;
    let delete_state = state.clone();
    let on_delete = move |_| {
        let state = delete_state.clone();
        let agent_ref = delete_agent.clone();
        let id = delete_id.clone();
        // Delete is gated on the same exact owner as Send Now.
        //
        // It is tempting to leave it live "so a stranded entry can still be
        // cleared", but that is not what happens: the whole row unmounts when
        // the owner dies, so no user can reach Delete afterwards. The only
        // behavior the ungated callback actually had was the stale-click race —
        // firing `CancelQueuedMessage` at a terminal instance stream through
        // `agent_instance_stream`, which is deliberately fatal-unfiltered
        // because Rename/Close/Load still need it. So the component callback is
        // the required guard, and this also restores desktop parity.
        if agent_ref_is_fatal(&state, &agent_ref) {
            return;
        }
        spawn_local(async move {
            if let Err(error) = crate::actions::cancel_queued_message(&state, &agent_ref, id).await
            {
                report_send_error(&state, format!("Failed to delete queued message: {error}"));
            }
        });
    };

    view! {
        <div class="chat-input-queued-row" data-mobile-test="chat-input-queued-row">
            <span class="chat-input-queued-preview">{preview}</span>
            <button
                type="button"
                class="chat-input-queued-action chat-input-queued-send-now"
                aria-label="Send queued message now"
                data-mobile-test="chat-input-queued-send-now"
                on:click=on_send_now
            >
                "Send Now"
            </button>
            <button
                type="button"
                class="chat-input-queued-action chat-input-queued-delete"
                aria-label="Delete queued message"
                data-mobile-test="chat-input-queued-delete"
                on:click=on_delete
            >
                "Delete"
            </button>
        </div>
    }
}

fn backend_value(backend: protocol::BackendKind) -> &'static str {
    match backend {
        protocol::BackendKind::Tycode => "tycode",
        protocol::BackendKind::Kiro => "acp",
        protocol::BackendKind::Claude => "claude",
        protocol::BackendKind::Codex => "codex",
        protocol::BackendKind::Antigravity => "antigravity",
        protocol::BackendKind::Hermes => "hermes",
    }
}

fn backend_label(backend: protocol::BackendKind) -> &'static str {
    match backend {
        protocol::BackendKind::Tycode => "Tycode",
        protocol::BackendKind::Kiro => "Kiro",
        protocol::BackendKind::Claude => "Claude",
        protocol::BackendKind::Codex => "Codex",
        protocol::BackendKind::Antigravity => "Antigravity",
        protocol::BackendKind::Hermes => "Hermes",
    }
}

fn parse_backend(value: &str) -> Option<protocol::BackendKind> {
    match value {
        "tycode" => Some(protocol::BackendKind::Tycode),
        "acp" => Some(protocol::BackendKind::Kiro),
        "claude" => Some(protocol::BackendKind::Claude),
        "codex" => Some(protocol::BackendKind::Codex),
        "antigravity" => Some(protocol::BackendKind::Antigravity),
        "hermes" => Some(protocol::BackendKind::Hermes),
        _ => None,
    }
}

#[component]
fn NewChatOptions() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();

    let backend_state = state.clone();
    let enabled_backends = Memo::new(move |_| {
        backend_state
            .active_host_settings()
            .map(|settings| settings.enabled_backends)
            .unwrap_or_default()
    });
    let default_state = state.clone();
    let default_backend_label = move || {
        default_state
            .active_host_settings()
            .and_then(|settings| settings.default_backend)
            .map(|backend| format!("Host default ({})", backend_label(backend)))
            .unwrap_or_else(|| "Host default".to_owned())
    };
    let selected_backend_state = state.clone();
    let selected_backend = move || {
        selected_backend_state
            .draft_backend_override
            .get()
            .map(backend_value)
            .unwrap_or_default()
    };
    let change_backend_state = state.clone();
    let on_backend_change = move |event| {
        let value = event_target_value(&event);
        change_backend_state
            .draft_backend_override
            .set(parse_backend(&value));
    };

    let custom_agents_state = state.clone();
    let custom_agents = Memo::new(move |_| {
        let mut agents = custom_agents_state
            .active_host_custom_agents()
            .into_values()
            .filter(|agent| agent.id.0 != "tyde-default")
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.name.cmp(&right.name));
        agents
    });
    let selected_agent_state = state.clone();
    let selected_agent = move || {
        selected_agent_state
            .draft_custom_agent_id
            .get()
            .map(|id| id.0)
            .unwrap_or_default()
    };
    let change_agent_state = state.clone();
    let on_agent_change = move |event| {
        let value = event_target_value(&event);
        change_agent_state
            .draft_custom_agent_id
            .set((!value.is_empty()).then_some(protocol::CustomAgentId(value)));
    };
    let agent_hint_state = state.clone();
    let agent_hint = move || {
        let Some(selected) = agent_hint_state.draft_custom_agent_id.get() else {
            return "Use the host's default agent instructions.".to_owned();
        };
        agent_hint_state
            .active_host_custom_agents()
            .get(&selected)
            .map(|agent| agent.description.clone())
            .filter(|description| !description.trim().is_empty())
            .unwrap_or_else(|| "Use this custom agent for the new chat.".to_owned())
    };

    view! {
        <section class="new-chat-options" data-mobile-test="new-chat-options" aria-label="New chat options">
            <label class="new-chat-option">
                <span class="new-chat-option-label">"Backend"</span>
                <select
                    class="new-chat-option-select"
                    data-mobile-test="new-chat-backend"
                    aria-label="Backend"
                    prop:value=selected_backend
                    on:change=on_backend_change
                >
                    <option value="">{default_backend_label}</option>
                    {move || enabled_backends.get().into_iter().map(|backend| view! {
                        <option value=backend_value(backend)>{backend_label(backend)}</option>
                    }).collect::<Vec<_>>()}
                </select>
            </label>
            <label class="new-chat-option">
                <span class="new-chat-option-label">"Agent"</span>
                <select
                    class="new-chat-option-select"
                    data-mobile-test="new-chat-agent"
                    aria-label="Agent"
                    aria-describedby="new-chat-agent-hint"
                    prop:value=selected_agent
                    on:change=on_agent_change
                >
                    <option value="">"Default agent"</option>
                    {move || custom_agents.get().into_iter().map(|agent| {
                        let value = agent.id.0;
                        view! { <option value=value>{agent.name}</option> }
                    }).collect::<Vec<_>>()}
                </select>
            </label>
            <p id="new-chat-agent-hint" class="new-chat-option-hint">{agent_hint}</p>
        </section>
    }
}

/// Mobile chat composer.
///
/// Primary button label follows the state matrix: "Send" when idle, "Queue"
/// when a turn is running and there is draft text, "Cancel" when running with
/// an empty composer. The caret is always rendered but disabled when the
/// dropdown would be empty. The dropdown carries secondary actions only:
/// "Steer" and "Cancel" when running+input; "Fork + send" when a forkable
/// session exists and there is draft text.
#[component]
pub fn ChatInput() -> impl IntoView {
    let state = use_context::<AppState>().unwrap();
    let composer = Composer::new();
    let textarea_ref = composer.textarea;
    let photo_input_ref = NodeRef::<leptos::html::Input>::new();
    let attachment_error = composer.attachment_error;
    let loading_photos = RwSignal::new(false);
    let submitting = composer.submitting;

    let do_send = {
        let state = state.clone();
        move || {
            // The composer now *keeps* the user's text for the whole in-flight
            // window — that is the entire point of the fix. It also means the old
            // double-send guard is gone: clearing the box early used to make a
            // second tap read empty text and short-circuit. That was never a
            // guard, it was a side effect, and a second `SpawnAgent` costs a
            // second agent and a second paid turn. Guard it explicitly.
            if composer.is_busy() || loading_photos.get_untracked() {
                return;
            }
            // A terminated agent is a deliberate block, not a lookup failure.
            // Falling through to the stream resolution below would surface
            // "agent stream not found", which reads as a transport bug and says
            // nothing about what to do instead. The draft survives either way —
            // it is the payload for Fork + send.
            if active_agent_is_terminated(&state) {
                return;
            }
            let text = state.chat_input.get_untracked().trim().to_string();
            let images = composer.image_payload();
            if text.is_empty() && images.is_empty() {
                return;
            }

            let state = state.clone();
            // Resolve the destination *before* anything moves, so a target we
            // cannot address fails with the composer still full.
            let active_target = match state.active_agent.get_untracked() {
                Some(active) => {
                    let Some(stream) = active_agent_stream(&state, &active) else {
                        report_send_error(
                            &state,
                            "Failed to send message: agent stream not found".into(),
                        );
                        return;
                    };
                    Some((active, stream))
                }
                None => None,
            };

            let host = match &active_target {
                Some((active, _)) => active.local_host_id.clone(),
                None => match state.active_local_host_id.get_untracked() {
                    Some(host) => host,
                    None => {
                        report_send_error(&state, "Failed to send message: no active host".into());
                        return;
                    }
                },
            };
            if refuse_unholdable(&state, &host) {
                return;
            }

            composer.begin();
            spawn_local(async move {
                // The composer still holds the text through this await. It is
                // cleared only inside `settle_submission`, and only once the
                // record has taken custody.
                let (target, outcome) = match active_target {
                    Some((active, stream)) => {
                        let payload = protocol::SendMessagePayload {
                            message: text.clone(),
                            images: (!images.is_empty()).then(|| images.clone()),
                            origin: None,
                            tool_response: None,
                        };
                        let outcome = crate::send::send_frame(
                            &active.local_host_id,
                            stream,
                            protocol::FrameKind::SendMessage,
                            &payload,
                        )
                        .await;
                        (SubmissionTarget::Agent(active.as_agent_ref()), outcome)
                    }
                    None => {
                        // A new chat has no agent yet, and the client must not
                        // guess which `NewAgent` is its own — so this record is
                        // host-scoped, not attached to any agent.
                        let outcome =
                            crate::actions::spawn_new_chat(&state, text.clone(), images.clone())
                                .await;
                        (SubmissionTarget::NewChat, outcome)
                    }
                };
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target,
                        text,
                        images,
                    },
                    outcome,
                );
            });
        }
    };

    let send_for_key = do_send.clone();
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) {
            ev.prevent_default();
            send_for_key();
        }
    };

    let do_steer = {
        let state = state.clone();
        move || {
            if composer.is_busy() || loading_photos.get_untracked() {
                return;
            }
            // Same deliberate-block rule as `do_send`: a dead agent has no turn
            // to steer, and the stream-not-found copy would misdescribe it.
            if active_agent_is_terminated(&state) {
                return;
            }
            let Some(active) = state.active_agent.get_untracked() else {
                return;
            };
            let Some(stream) = active_agent_stream(&state, &active) else {
                report_send_error(&state, "Failed to steer: agent stream not found".into());
                return;
            };

            let text = state.chat_input.get_untracked().trim().to_string();
            let images = composer.image_payload();
            let host = active.local_host_id.clone();
            if (!text.is_empty() || !images.is_empty()) && refuse_unholdable(&state, &host) {
                return;
            }

            let state = state.clone();
            composer.begin();
            spawn_local(async move {
                // The composer keeps the draft across both sends. The interrupt
                // carries no user text, so only the message that follows it
                // becomes a recovery record.
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream.clone(),
                    protocol::FrameKind::Interrupt,
                    &protocol::InterruptPayload {},
                )
                .await
                {
                    composer.finish();
                    report_send_error(&state, format!("Failed to interrupt current turn: {error}"));
                    return;
                }
                if text.is_empty() && images.is_empty() {
                    composer.finish();
                    return;
                }
                let payload = protocol::SendMessagePayload {
                    message: text.clone(),
                    images: (!images.is_empty()).then(|| images.clone()),
                    origin: None,
                    tool_response: None,
                };
                let outcome = crate::send::send_frame(
                    &active.local_host_id,
                    stream,
                    protocol::FrameKind::SendMessage,
                    &payload,
                )
                .await;
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target: SubmissionTarget::Agent(active.as_agent_ref()),
                        text,
                        images,
                    },
                    outcome,
                );
            });
        }
    };

    let steer_for_menu = do_steer.clone();

    // Plain interrupt: stop the current turn without sending the draft. The
    // menu's "Interrupt" item can appear while a draft exists, so it needs a
    // handler distinct from steer (which interrupts *and* sends the draft).
    let do_interrupt = {
        let state = state.clone();
        move || {
            // Nothing to interrupt on a dead agent, and no honest error to
            // report about it — the transcript's fatal Error row already says
            // what happened.
            if active_agent_is_terminated(&state) {
                return;
            }
            let Some(active) = state.active_agent.get_untracked() else {
                return;
            };
            let Some(stream) = active_agent_stream(&state, &active) else {
                report_send_error(&state, "Failed to interrupt: agent stream not found".into());
                return;
            };
            let state = state.clone();
            spawn_local(async move {
                if let Err(error) = crate::send::send_frame(
                    &active.local_host_id,
                    stream,
                    protocol::FrameKind::Interrupt,
                    &protocol::InterruptPayload {},
                )
                .await
                {
                    report_send_error(&state, format!("Failed to interrupt current turn: {error}"));
                }
            });
        }
    };
    let interrupt_for_menu = do_interrupt;

    // "Fork + send": fork the active agent's session and send the draft to the
    // fork. Enabled only when there is draft text and the active agent has a
    // forkable backend session. The fork is a *new* agent, so — like new chat —
    // its recovery record is host-scoped, never attributed to the agent we
    // forked from.
    let do_btw = {
        let state = state.clone();
        move || {
            if composer.is_busy() || loading_photos.get_untracked() {
                return;
            }
            let text = state.chat_input.get_untracked().trim().to_string();
            let images = composer.image_payload();
            if text.is_empty() && images.is_empty() {
                return;
            }
            let Some(host) = state
                .active_agent
                .get_untracked()
                .map(|active| active.local_host_id)
            else {
                report_send_error(
                    &state,
                    "Failed to start side question: no active agent".into(),
                );
                return;
            };
            if refuse_unholdable(&state, &host) {
                return;
            }
            let state = state.clone();
            composer.begin();
            spawn_local(async move {
                let outcome =
                    crate::actions::spawn_side_question(&state, text.clone(), images.clone()).await;
                settle_submission(
                    &state,
                    composer,
                    OutboundSubmission {
                        local_host_id: host,
                        target: SubmissionTarget::NewChat,
                        text,
                        images,
                    },
                    outcome,
                );
            });
        }
    };
    let btw_for_menu = do_btw.clone();
    let send_for_menu = do_send.clone();

    let s_input = state.clone();
    let textarea_ref_for_effect = textarea_ref;
    Effect::new(move |_| {
        let _ = s_input.chat_input.get();
        if let Some(textarea) = textarea_ref_for_effect.get() {
            resize_chat_input(&textarea);
        }
    });

    let s_input = state.clone();
    let running_state = state.clone();
    let is_running = Memo::new(move |_| active_agent_is_running_tracked(&running_state));
    let terminated_state = state.clone();
    // The composer target died. Same-actor Send/Steer/Interrupt are off, but the
    // draft, the photo controls, and Fork + send all stay live: the draft is the
    // payload the user forks with, and taking it away would remove the only
    // in-context recovery route.
    let is_terminated = Memo::new(move |_| active_agent_is_terminated_tracked(&terminated_state));
    let has_input_state = state.clone();
    let has_input = Memo::new(move |_| {
        has_input_state
            .chat_input
            .with(|text| !text.trim().is_empty())
            || composer.images.with(|images| !images.is_empty())
    });
    let btw_state = state.clone();
    let can_btw =
        Memo::new(move |_| has_input.get() && active_agent_has_session_id_tracked(&btw_state));
    // Steer = thinking + draft text or photos.
    let is_steer = Memo::new(move |_| is_running.get() && has_input.get());
    // Menu holds items only for: Fork + send (input+session) or Steer+Cancel (thinking+input).
    // Steer and Fork + send are submissions too, and both spend money (Fork + send
    // creates an agent). The in-flight latch closes the whole surface, not just
    // the primary button — otherwise the dropdown is a way around the guard.
    let menu_has_items = Memo::new(move |_| (can_btw.get() || is_steer.get()) && !submitting.get());
    let menu_open = RwSignal::new(false);
    // Auto-dismiss a stale-open menu when its items disappear.
    Effect::new(move |_| {
        if !menu_has_items.get() {
            menu_open.set(false);
        }
    });
    let on_split_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" && menu_open.get() {
            ev.prevent_default();
            menu_open.set(false);
        }
    };

    let add_photo_state = state.clone();
    let on_add_photo = move |_| {
        if !selected_backend_kind(&add_photo_state)
            .map(protocol::BackendKind::supports_image_input)
            .unwrap_or(false)
        {
            attachment_error.set(Some(
                "The selected agent backend does not support photo input.".to_owned(),
            ));
            return;
        }
        attachment_error.set(None);
        if let Some(input) = photo_input_ref.get_untracked() {
            let input: web_sys::HtmlElement = input.unchecked_into();
            input.click();
        }
    };

    let choose_photo_state = state.clone();
    let on_photos_chosen = move |ev| {
        let input = event_target::<web_sys::HtmlInputElement>(&ev);
        let files = input.files();

        if !selected_backend_kind(&choose_photo_state)
            .map(protocol::BackendKind::supports_image_input)
            .unwrap_or(false)
        {
            input.set_value("");
            attachment_error.set(Some(
                "The selected agent backend does not support photo input.".to_owned(),
            ));
            return;
        }

        let Some(files) = files else {
            return;
        };
        let files = (0..files.length())
            .filter_map(|index| files.get(index))
            .collect::<Vec<_>>();
        input.set_value("");
        if files.is_empty() {
            return;
        }

        loading_photos.set(true);
        attachment_error.set(None);
        spawn_local(async move {
            let mut added = Vec::new();
            let mut errors = Vec::new();
            for file in files {
                match read_image_file(file).await {
                    Ok(image) => added.push(image),
                    Err(error) => errors.push(error),
                }
            }
            if !added.is_empty() {
                composer.images.update(|images| images.extend(added));
            }
            if !errors.is_empty() {
                attachment_error.set(Some(errors.join(" ")));
            }
            loading_photos.set(false);
        });
    };

    let queue_state = state.clone();
    let queued_rows = Memo::new(move |_| {
        let Some(active) = queue_state.active_agent.get() else {
            return Vec::new();
        };
        // A dead agent will never dequeue anything, so "Send Now" and the
        // queued list are dead controls. The queue itself is server-owned and
        // stays in state untouched — this hides the actions, it does not
        // discard the messages.
        if active_agent_is_terminated_tracked(&queue_state) {
            return Vec::new();
        }
        let agent_ref = active.as_agent_ref();
        queue_state.agent_message_queue.with(|queues| {
            queues
                .get(&agent_ref)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| QueuedRowRef {
                            agent_ref: agent_ref.clone(),
                            id: entry.id.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    });

    view! {
        <div class="chat-input-container" data-mobile-test="chat-input-container">
            // The composer emptying itself is visible feedback for a sighted
            // user and silence for everyone else. This announces the move — and
            // says "queued", never "sent", because that is all the client knows.
            <div
                role="status"
                aria-live="polite"
                style=VISUALLY_HIDDEN
                data-mobile-test="chat-input-announcement"
            >
                {move || composer.announcement.get()}
            </div>
            <Show when=move || state.active_agent.get().is_none()>
                <NewChatOptions />
            </Show>
            {move || {
                let rows = queued_rows.get();
                if rows.is_empty() {
                    return view! { <div></div> }.into_any();
                }
                let n = rows.len();
                view! {
                    <div class="chat-input-queued-list" data-mobile-test="chat-input-queued-list" aria-live="polite">
                        <div class="chat-input-queued-title">
                            {format!("{n} message{} queued", if n == 1 { "" } else { "s" })}
                        </div>
                        <For
                            each=move || queued_rows.get()
                            key=|row| format!("{}:{}:{}", row.agent_ref.local_host_id, row.agent_ref.agent_id, row.id)
                            let:row
                        >
                            <QueuedMessageControlRow row=row />
                        </For>
                    </div>
                }.into_any()
            }}
            <input
                class="chat-photo-input"
                type="file"
                accept="image/*"
                multiple=true
                aria-label="Choose photos"
                data-mobile-test="chat-photo-input"
                node_ref=photo_input_ref
                on:change=on_photos_chosen
            />
            <Show when=move || !composer.images.get().is_empty()>
                <div
                    class="chat-photo-tray"
                    aria-label="Attached photos"
                    data-mobile-test="chat-photo-tray"
                >
                    {move || {
                        composer
                            .images
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(index, image)| {
                                let src = format!(
                                    "data:{};base64,{}",
                                    image.media_type, image.data
                                );
                                let alt = image.name.clone();
                                view! {
                                    <div class="chat-photo-preview">
                                        <img src=src alt=alt />
                                        <button
                                            type="button"
                                            class="chat-photo-remove"
                                            aria-label=format!("Remove {}", image.name)
                                            on:click=move |_| {
                                                composer.images.update(|images| {
                                                    if index < images.len() {
                                                        images.remove(index);
                                                    }
                                                });
                                                attachment_error.set(None);
                                            }
                                        >
                                            "×"
                                        </button>
                                    </div>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </div>
            </Show>
            <Show when=move || attachment_error.get().is_some()>
                <div
                    class="chat-photo-error"
                    role="alert"
                    data-mobile-test="chat-photo-error"
                >
                    {move || attachment_error.get().unwrap_or_default()}
                </div>
            </Show>
            <div class="chat-input-row">
                <button
                    type="button"
                    class="chat-photo-button"
                    aria-label="Add photos"
                    data-mobile-test="chat-add-photo"
                    disabled=move || loading_photos.get() || submitting.get()
                    on:click=on_add_photo
                >
                    <span aria-hidden="true">
                        {move || if loading_photos.get() { "…" } else { "+" }}
                    </span>
                </button>
                <textarea
                    class="chat-input-field"
                    // Visible, not just a tooltip. On a touch UI `title` never
                    // appears, so the placeholder is the only place the Resume
                    // and New Chat routes can actually be read. The field stays
                    // editable — the draft is the payload for Fork + send.
                    placeholder=move || {
                        if is_terminated.get() { TERMINATED_COMPOSER_HINT }
                        else { "Message..." }
                    }
                    aria-label="Message composer"
                    enterkeyhint="enter"
                    rows=1
                    data-mobile-test="chat-input"
                    node_ref=textarea_ref
                    prop:value=move || s_input.chat_input.get()
                    on:input=move |ev| {
                        let textarea = event_target::<web_sys::HtmlTextAreaElement>(&ev);
                        let val = textarea.value();
                        s_input.chat_input.set(val);
                        resize_chat_input(&textarea);
                    }
                    on:keydown=on_keydown
                />
                <div
                    class="chat-send-split"
                    role="group"
                    aria-label="Send actions"
                    data-mobile-test="chat-send-split"
                    on:keydown=on_split_keydown
                >
                    <button
                        type="button"
                        class="send-button chat-send-split-primary"
                        aria-label={move || {
                            if is_terminated.get() { TERMINATED_COMPOSER_LABEL }
                            else if is_running.get() && !has_input.get() { "Cancel current turn" }
                            else if is_steer.get() { "Queue message" }
                            else { "Send message" }
                        }}
                        title=move || {
                            if is_terminated.get() { TERMINATED_COMPOSER_HINT } else { "" }
                        }
                        data-mobile-test="chat-send"
                        on:click={
                            let do_interrupt = interrupt_for_menu.clone();
                            let do_send = send_for_menu.clone();
                            move |_| {
                                if is_terminated.get_untracked() {
                                    return;
                                }
                                if is_running.get_untracked() && !has_input.get_untracked() {
                                    do_interrupt();
                                } else {
                                    do_send();
                                }
                            }
                        }
                        disabled=move || {
                            // A dead actor has no send, no steer, and no turn to
                            // cancel. This outranks the Cancel branch below,
                            // which is otherwise unconditionally enabled.
                            if is_terminated.get() { true }
                            // Cancel (thinking+empty): always enabled — stopping a
                            // turn must never be blocked by an unsettled send.
                            else if is_running.get() && !has_input.get() { false }
                            // The composer keeps its draft across the in-flight
                            // window, so having input no longer implies this is
                            // a fresh send. Say so explicitly.
                            else { !has_input.get() || submitting.get() || loading_photos.get() }
                        }
                    >
                        {move || {
                            if is_terminated.get() { "Terminated" }
                            else if is_running.get() && !has_input.get() { "Cancel" }
                            else if is_steer.get() { "Queue" }
                            else { "Send" }
                        }}
                    </button>
                    <button
                        type="button"
                        class="send-menu-toggle"
                        data-mobile-test="chat-send-menu-toggle"
                        aria-haspopup="menu"
                        aria-expanded=move || {
                            if menu_open.get() { "true" } else { "false" }
                        }
                        aria-label="More send actions"
                        disabled=move || !menu_has_items.get()
                        on:click=move |_| menu_open.update(|open| *open = !*open)
                    >
                        <span aria-hidden="true">"\u{2304}"</span>
                    </button>
                    {move || {
                        if !(menu_open.get() && menu_has_items.get()) {
                            return view! { <div></div> }.into_any();
                        }
                        let on_btw = btw_for_menu.clone();
                        let on_steer = steer_for_menu.clone();
                        let on_cancel = interrupt_for_menu.clone();
                        let show_steer = is_steer.get();
                        let show_btw = can_btw.get();
                        let show_cancel = is_steer.get();
                        view! {
                            <div
                                class="chat-send-menu-backdrop"
                                data-mobile-test="chat-send-menu-backdrop"
                                on:click=move |_| menu_open.set(false)
                            ></div>
                            <div
                                class="chat-send-menu"
                                role="menu"
                                aria-label="Send actions"
                                data-mobile-test="chat-send-menu"
                            >
                                {show_steer.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-steer"
                                        on:click=move |_| { menu_open.set(false); on_steer(); }
                                    >
                                        "Steer"
                                    </button>
                                })}
                                {show_btw.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-ask-aside"
                                        on:click=move |_| { menu_open.set(false); on_btw(); }
                                    >
                                        "Fork + send"
                                    </button>
                                })}
                                {show_cancel.then(|| view! {
                                    <button
                                        type="button"
                                        class="chat-send-menu-item"
                                        role="menuitem"
                                        data-mobile-test="chat-send-menu-cancel"
                                        on:click=move |_| { menu_open.set(false); on_cancel(); }
                                    >
                                        "Cancel"
                                    </button>
                                })}
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
}

fn resize_chat_input(textarea: &web_sys::HtmlTextAreaElement) {
    let html_el: web_sys::HtmlElement = textarea.clone().unchecked_into();
    let _ = textarea.set_attribute("style", "height: auto; overflow-y: hidden;");
    let scroll_height = html_el.scroll_height();
    let target_height = scroll_height.clamp(CHAT_INPUT_MIN_HEIGHT_PX, CHAT_INPUT_MAX_HEIGHT_PX);
    let overflow = if scroll_height > CHAT_INPUT_MAX_HEIGHT_PX {
        "auto"
    } else {
        "hidden"
    };
    let _ = textarea.set_attribute(
        "style",
        &format!("height: {target_height}px; overflow-y: {overflow};"),
    );
}

fn report_send_error(state: &AppState, message: String) {
    log::error!("{message}");
    state
        .mobile_shell_error
        .set(Some(crate::state::MobileShellError {
            code: protocol::MobileAccessErrorCode::TransportFailed,
            message,
        }));
}

/// Settle one outbound user submission.
///
/// **On admission the text and images move into a recovery record and the
/// composer is cleared in the same synchronous step.** There is never a state in
/// which the user's input exists in neither place. That window was the bug: the
/// composer used to be cleared *before* the send was awaited, so a send that
/// never settled destroyed the text silently — no holder, no error, nothing.
///
/// **On rejection nothing moves.** The text never left the composer, so there is
/// nothing to restore; the user just sees why it did not go.
///
/// Admission means the frame entered this connection's bounded outbound queue.
/// It is not delivery, and this function never claims otherwise: the record is
/// born silent and is only ever surfaced if the transport later reports a
/// failure for it.
fn settle_submission(
    state: &AppState,
    composer: Composer,
    submission: OutboundSubmission,
    outcome: Result<Accepted, SendFrameError>,
) {
    let OutboundSubmission {
        local_host_id,
        target,
        text,
        images,
    } = submission;
    match outcome {
        Ok(accepted) => {
            state.hold_submission(PendingSubmission {
                local_submission_id: accepted.local_submission_id,
                // A fresh logical submission: the user just made this one.
                origin: state.mint_submission_origin(),
                local_host_id,
                connection_instance_id: accepted.connection_instance_id,
                target,
                text,
                images,
                // The composer only ever produces plain chat messages. Typed tool
                // responses (a plan approval, a rejection) are submitted by their
                // card, never typed here.
                tool_response: None,
                state: PendingSubmissionState::QueuedLocally,
            });
            composer.clear(state);
            composer.announcement.set(QUEUED_ANNOUNCEMENT.to_owned());
        }
        Err(error) => {
            // The composer still holds the text — nothing moved, nothing to
            // restore. Say exactly why it did not go.
            composer.announcement.set(String::new());
            report_send_error(state, error.to_string());
        }
    }
    composer.finish();
}

/// Refuse a send whose text we could not take custody of, **before** the frame
/// reaches the transport.
///
/// This is the only honest place to enforce the cap. Once a frame is admitted it
/// cannot be un-sent, so a cap enforced afterwards can only make room by
/// destroying a record — and every record, in-flight ones included, is the sole
/// holder of a message the user may still need back.
///
/// So the send simply does not happen, the composer keeps every character, and
/// the user is told why.
fn refuse_unholdable(state: &AppState, host: &LocalHostId) -> bool {
    if state.can_hold_submission_untracked(host) {
        return false;
    }
    report_send_error(
        state,
        "Not sent — too many messages on this host are still unresolved. Deal with the ones \
         waiting below, then send this again. Your text is still here."
            .to_owned(),
    );
    true
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::bridge::{LocalSubmissionId, SendRejected};
    use crate::state::{AgentInfo, AgentRef, AppState, LocalHostId};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, CustomAgent, CustomAgentId, QueuedMessageEntry,
        QueuedMessageId, SessionId, StreamPath, ToolPolicy,
    };
    use settings_model::HostSettings;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn accepted(id: u64) -> Accepted {
        Accepted {
            connection_instance_id: 7,
            local_submission_id: LocalSubmissionId(id),
        }
    }

    /// The regression test for the bug this whole model exists to kill.
    ///
    /// The composer used to be cleared *before* the send was awaited. A send
    /// that never settled therefore destroyed the user's text outright: it was
    /// gone from the composer, it was in no record, and no error ever ran. The
    /// message simply vanished.
    ///
    /// The contract now: on admission the text leaves the composer **and**
    /// lands in a record. Both, together. There is no observable state in which
    /// it exists in neither place.
    #[wasm_bindgen_test]
    async fn admission_moves_text_out_of_composer_and_into_a_record() {
        let state = AppState::new();
        let host = LocalHostId("host-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.chat_input.set("ship it".to_owned());

        let composer = Composer::new();

        settle_submission(
            &state,
            composer,
            OutboundSubmission {
                local_host_id: host.clone(),
                target: SubmissionTarget::NewChat,
                text: "ship it".to_owned(),
                images: Vec::new(),
            },
            Ok(accepted(1)),
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "composer must be cleared once the text has a holder"
        );
        let held = state
            .pending_submissions
            .get_untracked()
            .get(&LocalSubmissionId(1))
            .cloned()
            .expect("the admitted text must be held in a recovery record");
        assert_eq!(
            held.text, "ship it",
            "the record must hold the exact text that left the composer"
        );
        assert_eq!(
            held.state,
            PendingSubmissionState::QueuedLocally,
            "an admitted submission is queued locally — never 'sent'"
        );
        assert_eq!(
            held.target,
            SubmissionTarget::NewChat,
            "a new chat has no agent, so the record must not claim one"
        );
        assert_eq!(
            composer.announcement.get_untracked(),
            QUEUED_ANNOUNCEMENT,
            "the move must be announced, so it is not silent for a screen reader"
        );
        assert!(
            !composer.is_busy(),
            "settling must release the in-flight latch, or the composer stays wedged"
        );
    }

    #[wasm_bindgen_test]
    async fn admission_moves_photos_into_the_recovery_record() {
        let state = AppState::new();
        let host = LocalHostId("host-1".to_owned());
        let composer = Composer::new();
        composer.images.set(vec![PendingImage {
            name: "photo.jpg".to_owned(),
            media_type: "image/jpeg".to_owned(),
            data: "cGhvdG8=".to_owned(),
        }]);
        let images = composer.image_payload();

        settle_submission(
            &state,
            composer,
            OutboundSubmission {
                local_host_id: host,
                target: SubmissionTarget::NewChat,
                text: String::new(),
                images,
            },
            Ok(accepted(2)),
        );

        assert!(
            composer.images.get_untracked().is_empty(),
            "admitted photos must leave the composer"
        );
        let held = state
            .pending_submissions
            .get_untracked()
            .get(&LocalSubmissionId(2))
            .cloned()
            .expect("an admitted photo must have a recovery record");
        assert_eq!(held.images.len(), 1);
        assert_eq!(held.images[0].media_type, "image/jpeg");
        assert_eq!(held.images[0].data, "cGhvdG8=");
    }

    /// A rejected submission never left the composer, so there is nothing to
    /// restore — and nothing to hold. The user sees their text still sitting
    /// there, plus the exact reason it did not go.
    #[wasm_bindgen_test]
    async fn rejection_leaves_the_composer_untouched_and_holds_nothing() {
        let state = AppState::new();
        let host = LocalHostId("host-1".to_owned());
        state.active_local_host_id.set(Some(host.clone()));
        state.chat_input.set("ship it".to_owned());

        let composer = Composer::new();

        settle_submission(
            &state,
            composer,
            OutboundSubmission {
                local_host_id: host,
                target: SubmissionTarget::NewChat,
                text: "ship it".to_owned(),
                images: Vec::new(),
            },
            Err(SendFrameError::Rejected(SendRejected::NotConnected)),
        );

        assert_eq!(
            state.chat_input.get_untracked(),
            "ship it",
            "a rejected send must leave the user's text exactly where it was"
        );
        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "a rejected send was never admitted, so nothing may be held for it"
        );
        let surfaced = state
            .mobile_shell_error
            .get_untracked()
            .expect("the admission failure must be surfaced, not swallowed");
        assert!(
            surfaced
                .message
                .contains(&SendRejected::NotConnected.to_string()),
            "the exact admission error must be shown, got: {}",
            surfaced.message
        );
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
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

    /// Click the split-button caret to reveal the action menu.
    async fn open_menu(container: &HtmlElement) {
        let toggle: HtmlElement = container
            .query_selector("[data-mobile-test='chat-send-menu-toggle']")
            .unwrap()
            .expect("dropdown toggle must be present")
            .dyn_into()
            .unwrap();
        toggle.click();
        next_tick().await;
    }

    fn primary(container: &HtmlElement) -> web_sys::Element {
        container
            .query_selector("[data-mobile-test='chat-send']")
            .unwrap()
            .expect("primary button must be present")
    }

    fn caret(container: &HtmlElement) -> web_sys::Element {
        container
            .query_selector("[data-mobile-test='chat-send-menu-toggle']")
            .unwrap()
            .expect("caret button must always be present")
    }

    fn menu_item_texts(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all("[role='menuitem']").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|n| n.text_content().unwrap_or_default().trim().to_owned())
            .collect()
    }

    /// Dispatch a real `keydown` on `target`. The composer's Cmd/Ctrl+Enter send
    /// runs off this event and is not gated by the button's `disabled`
    /// attribute, so it is the send path a fatal guard actually has to stop.
    fn dispatch_keydown(target: &web_sys::Element, key: &str, meta: bool, ctrl: bool) {
        // Constructed through `Reflect` rather than `KeyboardEventInit`, mirroring
        // the desktop composer's helper: it does not depend on which init-dict
        // setters the pinned web-sys exposes.
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"key".into(), &key.into()).unwrap();
        js_sys::Reflect::set(
            &init,
            &"metaKey".into(),
            &wasm_bindgen::JsValue::from_bool(meta),
        )
        .unwrap();
        js_sys::Reflect::set(
            &init,
            &"ctrlKey".into(),
            &wasm_bindgen::JsValue::from_bool(ctrl),
        )
        .unwrap();
        js_sys::Reflect::set(&init, &"bubbles".into(), &wasm_bindgen::JsValue::TRUE).unwrap();
        js_sys::Reflect::set(&init, &"cancelable".into(), &wasm_bindgen::JsValue::TRUE).unwrap();
        let ctor = js_sys::Reflect::get(&js_sys::global(), &"KeyboardEvent".into()).unwrap();
        let ctor: js_sys::Function = ctor.unchecked_into();
        let args = js_sys::Array::of2(&"keydown".into(), &init);
        let event = js_sys::Reflect::construct(&ctor, &args).unwrap();
        let event: web_sys::Event = event.unchecked_into();
        target.dispatch_event(&event).unwrap();
    }

    fn type_text(container: &HtmlElement, text: &str) {
        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value(text);
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }

    /// Mount a composer in new-chat mode (no active agent) on a connected host.
    fn mount_new_chat(container: &HtmlElement) -> AppState {
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId("host-1".to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.host_streams.update(|m| {
                m.insert(host, StreamPath("/host/h1".to_owned()));
            });
            // The backend override is all `spawn_new_chat` needs to pick a
            // backend, so the test does not have to fabricate host settings.
            state.draft_backend_override.set(Some(BackendKind::Claude));
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatInput /> }
        });
        std::mem::forget(h);
        handle.borrow().as_ref().unwrap().clone()
    }

    #[wasm_bindgen_test]
    async fn new_chat_can_choose_backend_and_custom_agent() {
        let container = make_container();
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId("host-1".to_owned());
            state.active_local_host_id.set(Some(host.clone()));
            state.host_settings_by_host.update(|settings| {
                settings.insert(
                    host.clone(),
                    HostSettings {
                        enabled_backends: vec![BackendKind::Codex, BackendKind::Claude],
                        default_backend: Some(BackendKind::Codex),
                        ..HostSettings::default()
                    },
                );
            });
            state.custom_agents_by_host.update(|agents| {
                agents.entry(host).or_default().insert(
                    CustomAgentId("reviewer".to_owned()),
                    CustomAgent {
                        id: CustomAgentId("reviewer".to_owned()),
                        name: "Reviewer".to_owned(),
                        description: "Review changes before they ship.".to_owned(),
                        instructions: None,
                        skill_ids: Vec::new(),
                        mcp_server_ids: Vec::new(),
                        tool_policy: ToolPolicy::Unrestricted,
                    },
                );
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatInput /> }
        });
        std::mem::forget(h);
        next_tick().await;

        let backend: web_sys::HtmlSelectElement = container
            .query_selector("[data-mobile-test='new-chat-backend']")
            .unwrap()
            .expect("backend selector")
            .dyn_into()
            .unwrap();
        assert!(
            backend.text_content().unwrap_or_default().contains("Codex")
                && backend
                    .text_content()
                    .unwrap_or_default()
                    .contains("Claude"),
            "every enabled backend should be offered"
        );
        backend.set_value("claude");
        backend
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();

        let agent: web_sys::HtmlSelectElement = container
            .query_selector("[data-mobile-test='new-chat-agent']")
            .unwrap()
            .expect("agent selector")
            .dyn_into()
            .unwrap();
        assert!(
            agent
                .text_content()
                .unwrap_or_default()
                .contains("Reviewer"),
            "custom agents should be offered by name"
        );
        agent.set_value("reviewer");
        agent
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        next_tick().await;

        let state = handle.borrow().as_ref().unwrap().clone();
        assert_eq!(
            state.draft_backend_override.get_untracked(),
            Some(BackendKind::Claude)
        );
        assert_eq!(
            state.draft_custom_agent_id.get_untracked(),
            Some(CustomAgentId("reviewer".to_owned()))
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Review changes before they ship."),
            "the chosen agent's description should explain the selection"
        );
    }

    /// **A double-tap must never buy two agents.**
    ///
    /// The old code cleared the composer *before* awaiting the send, so a second
    /// tap read an empty box and fell out of `if text.is_empty()`. That was never
    /// a guard — it was a side effect of the very bug being fixed. Preserving the
    /// user's text across the in-flight window removed it, and nothing replaced
    /// it: `Send` stayed enabled the whole time.
    ///
    /// It did not misfire only because `send_line` happened to resolve in the
    /// same microtask drain, so the future finished before the next click could
    /// be dispatched. That is an accident of executor ordering, not a guarantee.
    /// Any future `await` on that chain that genuinely yields — backpressure, an
    /// IndexedDB read, an auth refresh — reopens it, and two `SpawnAgent` frames
    /// is two agents, two backend sessions, and two paid turns.
    ///
    /// So this test makes the send **actually yield** (the deferred seam awaits a
    /// oneshot) and then taps twice. Exactly one frame must go out.
    #[wasm_bindgen_test]
    async fn a_double_tap_during_an_unsettled_send_emits_exactly_one_frame() {
        let _guard = crate::bridge::test_defer_sends();
        let container = make_container();
        let state = mount_new_chat(&container);
        next_tick().await;

        type_text(&container, "start a new chat");
        next_tick().await;

        let send: HtmlElement = primary(&container).dyn_into().unwrap();
        assert!(!send.has_attribute("disabled"), "Send must start enabled");

        // First tap. The send is deferred, so it is genuinely unsettled: the
        // future is parked on the oneshot and the composer still holds the text.
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "the first tap must send exactly one frame"
        );
        assert_eq!(
            state.chat_input.get_untracked(),
            "start a new chat",
            "the composer must still hold the text while the send is unsettled — \
             that is the whole point, and it is what removed the accidental guard"
        );
        assert!(
            send.has_attribute("disabled"),
            "Send must be disabled while a submission is unsettled"
        );

        // The impatient second tap, while the first is still in flight.
        send.click();
        next_tick().await;
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "a double-tap during an unsettled send must not buy a second agent"
        );

        // Let the first one land; the composer empties and reopens for the next.
        crate::bridge::test_resolve_next_send();
        next_tick().await;
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "once admitted, the text moves to the record and the composer clears"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            1,
            "exactly one submission must be held"
        );
    }

    /// The same guard, on the other frame kind. `SendMessage` to a live agent does
    /// not create an agent, but a double-tap still delivers the user's message
    /// twice — and a duplicate turn is a duplicate paid turn.
    #[wasm_bindgen_test]
    async fn a_double_tap_on_an_agent_message_emits_exactly_one_frame() {
        let _guard = crate::bridge::test_defer_sends();
        let container = make_container();
        let host = LocalHostId("host-1".to_owned());
        let host_for_mount = host.clone();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.active_local_host_id.set(Some(host_for_mount.clone()));
            state.agents.set(vec![AgentInfo {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_for_mount.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "run the tests");
        next_tick().await;

        let send: HtmlElement = primary(&container).dyn_into().unwrap();
        send.click();
        next_tick().await;
        assert_eq!(crate::bridge::test_send_attempts(), 1);
        assert!(
            send.has_attribute("disabled"),
            "Send must be disabled while the message is unsettled"
        );

        send.click();
        next_tick().await;
        send.click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "an impatient double-tap must not send the message twice"
        );
    }

    // ── State matrix row 1: Idle + empty ─────────────────────────────────────
    // Primary "Send" disabled; caret visible but disabled.
    #[wasm_bindgen_test]
    async fn idle_empty_send_disabled_caret_disabled() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            p.has_attribute("disabled"),
            "Send must be disabled when empty"
        );

        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled with no menu items"
        );

        let picker: web_sys::HtmlInputElement = container
            .query_selector("[data-mobile-test='chat-photo-input']")
            .unwrap()
            .expect("the composer must include a phone photo picker")
            .dyn_into()
            .unwrap();
        assert_eq!(picker.accept(), "image/*");
        assert!(picker.multiple(), "the picker should allow multiple photos");
        assert!(
            container
                .query_selector("[data-mobile-test='chat-add-photo']")
                .unwrap()
                .is_some(),
            "the photo picker needs a visible touch target"
        );
    }

    /// When there are queued messages, the composer surfaces per-row
    /// controls so a phone can do the same send-now/delete operations as
    /// desktop — without disabling the input.
    #[wasm_bindgen_test]
    async fn queued_controls_appear_when_messages_are_queued() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_message_queue.update(|m| {
                m.insert(
                    agent_ref,
                    vec![QueuedMessageEntry {
                        id: QueuedMessageId("q-1".to_owned()),
                        message: "later".to_owned(),
                        images: Vec::new(),
                        origin: None,
                    }],
                );
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;
        let list = container
            .query_selector("[data-mobile-test='chat-input-queued-list']")
            .unwrap()
            .expect("queued controls must render when at least one message is queued");
        let text = list.text_content().unwrap_or_default();
        assert!(
            text.contains("1 message"),
            "queued controls must mention count: {text}"
        );
        assert!(
            list.query_selector("[data-mobile-test='chat-input-queued-send-now']")
                .unwrap()
                .is_some(),
            "queued row must expose Send Now"
        );
        assert!(
            list.query_selector("[data-mobile-test='chat-input-queued-delete']")
                .unwrap()
                .is_some(),
            "queued row must expose Delete"
        );
        // Composer must remain enabled for queueing more messages.
        let input = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap();
        assert!(
            !input.has_attribute("disabled"),
            "composer must stay enabled so users can queue more"
        );
    }

    // ── State matrix row 4: Thinking + empty ─────────────────────────────────
    // Primary "Cancel" enabled; caret disabled; no menu items.
    #[wasm_bindgen_test]
    async fn thinking_empty_primary_cancel_caret_disabled() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Cancel",
            "primary must be Cancel when thinking with empty composer"
        );
        assert!(
            !p.has_attribute("disabled"),
            "Cancel must be enabled while thinking"
        );

        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled when thinking+empty (no menu items)"
        );
    }

    // ── State matrix row 5: Thinking + input, no session ─────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_no_session_queue_primary_steer_cancel_menu() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "redirect this");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Queue",
            "primary must be Queue when thinking with draft"
        );
        assert!(!p.has_attribute("disabled"), "Queue must be enabled");

        assert!(
            container
                .query_selector("[data-mobile-test='chat-steer']")
                .unwrap()
                .is_none(),
            "no standalone Steer button — it lives in the dropdown"
        );

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec!["Steer".to_owned(), "Cancel".to_owned()],
            "thinking+input menu must be Steer then Cancel"
        );
    }

    // ── State matrix row 3: Idle + input + session ───────────────────────────
    // Primary "Send" enabled; caret enabled; dropdown has "Fork + send" only.
    #[wasm_bindgen_test]
    async fn idle_input_with_session_menu_fork_send_only() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        // No draft → caret present but disabled.
        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled while no menu items (idle, no draft)"
        );

        type_text(&container, "why is this slow?");
        next_tick().await;

        // Now has draft → caret enabled, menu has "Fork + send" only.
        let c = caret(&container);
        assert!(
            !c.has_attribute("disabled"),
            "caret must be enabled once draft + session"
        );

        open_menu(&container).await;
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-ask-aside']")
                .unwrap()
                .is_some(),
            "Fork + send must appear once there is draft text and a forkable session"
        );
        assert_eq!(
            menu_item_texts(&container),
            vec!["Fork + send".to_owned()],
            "idle+session menu must be exactly 'Fork + send'"
        );
        // Fork + send must only exist inside the dropdown, not as a standalone button.
        assert!(
            container
                .query_selector("[data-mobile-test='chat-btw']")
                .unwrap()
                .is_none(),
            "Fork + send must only exist inside the dropdown menu"
        );
    }

    // ── State matrix row 2: Idle + input, no session ─────────────────────────
    // Primary "Send" enabled; caret disabled (no menu items).
    #[wasm_bindgen_test]
    async fn idle_input_no_session_send_enabled_caret_disabled() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "anything");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            !p.has_attribute("disabled"),
            "Send must be enabled with draft"
        );

        // No session → Fork + send absent → caret disabled.
        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled with no session (idle+input)"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-send-menu-ask-aside']")
                .unwrap()
                .is_none(),
            "Fork + send must stay hidden when the active agent has no session id"
        );
    }

    // ── State matrix row 6: Thinking + input + session ───────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Fork + send", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_with_session_queue_primary_full_menu() {
        let host = LocalHostId("host-1".to_owned());
        let host_clone = host.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let agent_ref = AgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            };
            state.agents.set(vec![AgentInfo {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: Some(SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: None,
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host_clone.clone(),
                agent_id: AgentId("agent-1".to_owned()),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref, true);
            });
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        type_text(&container, "redirect this");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Queue",
            "primary must be Queue when thinking with draft"
        );
        assert!(!p.has_attribute("disabled"), "Queue must be enabled");

        let c = caret(&container);
        assert!(!c.has_attribute("disabled"), "caret must be enabled");

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec![
                "Steer".to_owned(),
                "Fork + send".to_owned(),
                "Cancel".to_owned(),
            ],
            "thinking+session+input menu must be Steer, Fork + send, Cancel"
        );
    }

    /// Multiline input should grow vertically instead of hiding all but
    /// one or two lines. The resize helper caps growth and then scrolls
    /// internally for very long drafts.
    #[wasm_bindgen_test]
    async fn composer_resizes_for_multiline_input() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let input: web_sys::HtmlTextAreaElement = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        input.set_value("one\ntwo\nthree\nfour\nfive\nsix");
        input
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        let style = input.get_attribute("style").unwrap_or_default();
        assert!(
            style.contains("height:") && style.contains("overflow-y:"),
            "composer should get an inline autosize style, got: {style}"
        );
    }

    // ── Fatal agent lifecycle ───────────────────────────────────────────────

    /// Mount a composer whose active agent died with `AgentError { fatal: true }`
    /// while a turn was still marked active, and while a message sat queued.
    ///
    /// The stale `agent_turn_active` is deliberate: the reducer clears it, but
    /// the composer must not *depend* on that having happened. A late frame, a
    /// replay, or an ordering change would otherwise put Cancel back on a dead
    /// agent.
    fn mount_terminated_agent(container: &HtmlElement, with_session: bool) -> AppState {
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let host = LocalHostId("host-1".to_owned());
            let agent_id = AgentId("agent-1".to_owned());
            let agent_ref = AgentRef {
                local_host_id: host.clone(),
                agent_id: agent_id.clone(),
            };
            state.active_local_host_id.set(Some(host.clone()));
            state.host_streams.update(|m| {
                m.insert(host.clone(), StreamPath("/host/h1".to_owned()));
            });
            state.agents.set(vec![AgentInfo {
                local_host_id: host.clone(),
                agent_id: agent_id.clone(),
                name: "Agent".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: with_session.then(|| SessionId("sess-1".to_owned())),
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
                started: true,
                fatal_error: Some("backend crashed".to_owned()),
            }]);
            state.active_agent.set(Some(crate::state::ActiveAgentRef {
                local_host_id: host.clone(),
                agent_id: agent_id.clone(),
            }));
            state.agent_turn_active.update(|m| {
                m.insert(agent_ref.clone(), true);
            });
            state.agent_message_queue.update(|m| {
                m.insert(
                    agent_ref,
                    vec![QueuedMessageEntry {
                        id: QueuedMessageId("q-1".to_owned()),
                        message: "also fix the flaky test".to_owned(),
                        images: Vec::new(),
                        origin: None,
                    }],
                );
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <ChatInput /> }
        });
        std::mem::forget(h);
        handle.borrow().as_ref().unwrap().clone()
    }

    /// **A dead agent must not present a working Send.**
    ///
    /// Everything on this composer used to stay live: the primary said Cancel
    /// (stale turn) or Send (with a draft), both enabled, and either one
    /// addressed a stream nobody was reading. The user's message would be
    /// admitted, held as "Queued locally", and never delivered.
    ///
    /// The draft itself must survive — it is the payload for Fork + send, which
    /// is the whole recovery route.
    #[wasm_bindgen_test]
    async fn terminated_agent_disables_send_but_keeps_the_draft_and_fork() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let state = mount_terminated_agent(&container, true);
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Terminated",
            "a stale active turn must not render Cancel on a dead agent"
        );
        assert!(
            p.has_attribute("disabled"),
            "Terminated is a state, not an action — the control must be disabled"
        );
        // The recovery routes must be *visible*, not only in a tooltip: `title`
        // never renders on a touch UI, so the placeholder is the only place
        // Resume and New Chat can actually be read. Exact copy, so mobile and
        // desktop cannot drift into naming the same routes differently.
        assert_eq!(
            p.get_attribute("aria-label").as_deref(),
            Some(TERMINATED_COMPOSER_LABEL)
        );
        assert_eq!(
            p.get_attribute("title").as_deref(),
            Some(TERMINATED_COMPOSER_HINT)
        );
        let field = container
            .query_selector("[data-mobile-test='chat-input']")
            .unwrap()
            .expect("composer field");
        assert_eq!(
            field.get_attribute("placeholder").as_deref(),
            Some(TERMINATED_COMPOSER_HINT),
            "the terminal guidance must be visible in the composer itself"
        );
        assert!(
            !field.has_attribute("disabled"),
            "the field stays editable — the draft is the payload for Fork + send"
        );

        // A draft arrives. Send stays shut; the draft is untouched.
        type_text(&container, "please continue");
        next_tick().await;
        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Terminated",
            "a draft must not turn a dead agent's primary back into Send"
        );
        assert!(p.has_attribute("disabled"));

        // Clicking a disabled button is not a real test — the browser never
        // dispatches it. The keyboard path is not gated by `disabled` at all,
        // so Cmd/Ctrl+Enter is the send route that actually has to be guarded.
        assert_eq!(crate::bridge::test_send_attempts(), 0);
        dispatch_keydown(&field, "Enter", true, false);
        next_tick().await;
        dispatch_keydown(&field, "Enter", false, true);
        next_tick().await;
        assert_eq!(
            crate::bridge::test_send_attempts(),
            0,
            "no keyboard send may reach a dead agent's stream"
        );
        assert_eq!(
            state.chat_input.get_untracked(),
            "please continue",
            "and a refused send must leave the draft intact"
        );

        // Fork + send is the escape hatch and stays reachable: it spawns a new
        // agent from the retained session on the *host* stream, never on the
        // dead instance stream. Invoke it for real rather than only reading the
        // menu label — a Fork that was itself broken would pass a label check.
        let c = caret(&container);
        assert!(
            !c.has_attribute("disabled"),
            "Fork + send must remain available on a dead agent with a session"
        );
        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec!["Fork + send".to_owned()],
            "a dead agent's menu offers recovery only — never Steer or Cancel"
        );

        let fork: HtmlElement = container
            .query_selector("[data-mobile-test='chat-send-menu-ask-aside']")
            .unwrap()
            .expect("Fork + send item")
            .dyn_into()
            .unwrap();
        fork.click();
        next_tick().await;
        next_tick().await;
        assert_eq!(
            crate::bridge::test_send_attempts(),
            1,
            "Fork + send must emit exactly one frame on a dead agent"
        );
        let line = crate::bridge::test_sent_lines()
            .last()
            .cloned()
            .expect("the fork frame must be captured");
        let frame: serde_json::Value = serde_json::from_str(&line).expect("fork frame json");
        let stream = frame
            .get("stream")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            stream, "/host/h1",
            "the fork must go to the host stream, never the dead instance stream"
        );
        assert!(
            !stream.contains("/agent/agent-1/inst"),
            "a fork addressed to the dead instance stream would be the very bug \
             this recovery route exists to avoid: {stream}"
        );
    }

    /// The queue is server-owned and stays in state, but "Send Now" on a dead
    /// agent is a send that can never land. The rows go, the queue does not.
    #[wasm_bindgen_test]
    async fn terminated_agent_hides_the_queued_rows_without_dropping_the_queue() {
        let container = make_container();
        let state = mount_terminated_agent(&container, true);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-list']")
                .unwrap()
                .is_none(),
            "a dead agent must not offer queued-message actions"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-send-now']")
                .unwrap()
                .is_none(),
            "Send Now on a dead agent is a send that can never be delivered"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='chat-input-queued-delete']")
                .unwrap()
                .is_none(),
            "and no Delete either — the whole row goes"
        );
        assert_eq!(
            state.agent_message_queue.get_untracked().values().len(),
            1,
            "hiding the rows must not discard the server-owned queue"
        );
    }

    /// **The queued-row tap that was already in flight when the owner died.**
    ///
    /// Hiding the row is a *visibility* guard; it says nothing about the
    /// callbacks a dispatched tap still holds. So mount the row component
    /// directly — bypassing the memo that hides it — and click both controls for
    /// real with the exact owner fatal.
    ///
    /// The fixture also puts a healthy agent with the *same textual `AgentId`*
    /// on a second host. If the guard ever degrades to an id-only comparison,
    /// the healthy agent starts being treated as dead, and this test is where
    /// that shows up.
    #[wasm_bindgen_test]
    async fn queued_row_callbacks_refuse_a_fatal_owner_without_touching_other_hosts() {
        let _guard = crate::bridge::test_capture_sends();
        let container = make_container();
        let dead_host = LocalHostId("host-1".to_owned());
        let live_host = LocalHostId("host-2".to_owned());
        let shared_id = AgentId("agent-1".to_owned());
        let dead_ref = AgentRef {
            local_host_id: dead_host.clone(),
            agent_id: shared_id.clone(),
        };
        let live_ref = AgentRef {
            local_host_id: live_host.clone(),
            agent_id: shared_id.clone(),
        };

        let row_for_mount = QueuedRowRef {
            agent_ref: dead_ref.clone(),
            id: QueuedMessageId("q-1".to_owned()),
        };
        let handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let handle_for_mount = handle.clone();
        let (dead_host_m, live_host_m, id_m) =
            (dead_host.clone(), live_host.clone(), shared_id.clone());
        let h = mount_to(container.clone(), move || {
            let state = AppState::new();
            let dead = AgentInfo {
                local_host_id: dead_host_m.clone(),
                agent_id: id_m.clone(),
                name: "Dead".to_owned(),
                origin: AgentOrigin::User,
                backend_kind: BackendKind::Claude,
                workspace_roots: Vec::new(),
                project_id: None,
                parent_agent_id: None,
                session_id: None,
                custom_agent_id: None,
                created_at_ms: 0,
                instance_stream: StreamPath("/agent/dead/inst".to_owned()),
                started: true,
                fatal_error: Some("backend crashed".to_owned()),
            };
            // Same textual AgentId, different host, still alive.
            let live = AgentInfo {
                local_host_id: live_host_m.clone(),
                name: "Live twin".to_owned(),
                instance_stream: StreamPath("/agent/live/inst".to_owned()),
                fatal_error: None,
                ..dead.clone()
            };
            state.agents.set(vec![dead, live]);
            state.host_streams.update(|m| {
                m.insert(dead_host_m.clone(), StreamPath("/host/h1".to_owned()));
                m.insert(live_host_m.clone(), StreamPath("/host/h2".to_owned()));
            });
            state.agent_message_queue.update(|m| {
                m.insert(
                    AgentRef {
                        local_host_id: dead_host_m.clone(),
                        agent_id: id_m.clone(),
                    },
                    vec![QueuedMessageEntry {
                        id: QueuedMessageId("q-1".to_owned()),
                        message: "still queued behind the dead agent".to_owned(),
                        images: Vec::new(),
                        origin: None,
                    }],
                );
            });
            *handle_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <QueuedMessageControlRow row=row_for_mount.clone() /> }
        });
        std::mem::forget(h);
        next_tick().await;

        let send_now: HtmlElement = container
            .query_selector("[data-mobile-test='chat-input-queued-send-now']")
            .unwrap()
            .expect("row is mounted directly, so Send Now is present")
            .dyn_into()
            .unwrap();
        let delete: HtmlElement = container
            .query_selector("[data-mobile-test='chat-input-queued-delete']")
            .unwrap()
            .expect("row is mounted directly, so Delete is present")
            .dyn_into()
            .unwrap();

        send_now.click();
        next_tick().await;
        delete.click();
        next_tick().await;
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            0,
            "neither SendQueuedMessageNow nor CancelQueuedMessage may reach a \
             terminal instance stream"
        );
        let state = handle.borrow().as_ref().unwrap().clone();
        assert!(
            state.mobile_shell_error.get_untracked().is_none(),
            "a refused queued action must not surface a transport error either"
        );
        assert_eq!(
            state
                .agent_message_queue
                .with_untracked(|m| m.get(&dead_ref).map(|entries| entries.len())),
            Some(1),
            "refusing the actions must not mutate the server-owned queue"
        );

        // The same-id agent on the other host is untouched and still live, so
        // the guard cannot have matched on AgentId alone.
        assert!(
            state.agents.with_untracked(|agents| agents
                .iter()
                .any(|a| a.local_host_id == live_ref.local_host_id
                    && a.agent_id == live_ref.agent_id
                    && a.fatal_error.is_none())),
            "the identically-named agent on host-2 must remain live"
        );
        assert!(agent_ref_is_fatal(&state, &dead_ref));
        assert!(
            !agent_ref_is_fatal(&state, &live_ref),
            "fatality is host-scoped; an id-only check would fail here"
        );
    }

    /// Without a forkable session there is no in-context recovery, so the caret
    /// closes too — but that must not quietly re-open same-actor Send.
    #[wasm_bindgen_test]
    async fn terminated_agent_without_a_session_offers_no_send_and_no_fork() {
        let container = make_container();
        let _state = mount_terminated_agent(&container, false);
        next_tick().await;
        type_text(&container, "please continue");
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Terminated");
        assert!(p.has_attribute("disabled"));
        assert!(
            caret(&container).has_attribute("disabled"),
            "no session means no Fork + send, so the menu has nothing to offer"
        );
    }
}
