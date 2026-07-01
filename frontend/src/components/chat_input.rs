use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Array, Promise};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::actions::spawn_new_chat;
use crate::components::session_settings::SessionSettingsBar;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{
    AgentOrigin, BackendKind, BackendSetupStatus, CancelQueuedMessagePayload, FrameKind, ImageData,
    InterruptPayload, QueuedMessageId, SendMessagePayload, SendQueuedMessageNowPayload, StreamPath,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingImage {
    name: String,
    media_type: String,
    data: String,
}

#[component]
fn QueuedMessageRow(id: QueuedMessageId) -> impl IntoView {
    let state = expect_context::<AppState>();

    let id_for_lookup = id.clone();
    let id_for_send = id.clone();
    let id_for_cancel = id.clone();
    let state_preview = state.clone();
    let state_send = state.clone();
    let state_cancel = state.clone();

    let preview = move || {
        let Some(active) = state_preview.active_agent.get() else {
            return String::new();
        };
        let queue = state_preview.agent_message_queue.get();
        let Some(entries) = queue.get(&active.agent_id) else {
            return String::new();
        };
        let Some(entry) = entries.iter().find(|entry| entry.id == id_for_lookup) else {
            return String::new();
        };
        let chars: Vec<char> = entry.message.chars().collect();
        if chars.len() > 80 {
            chars[..80].iter().collect::<String>() + "…"
        } else {
            entry.message.clone()
        }
    };

    let on_send_now = move |_| {
        let Some(active) = state_send.active_agent.get_untracked() else {
            return;
        };
        let agents = state_send.agents.get_untracked();
        let Some(agent) = agents
            .iter()
            .find(|agent| agent.host_id == active.host_id && agent.agent_id == active.agent_id)
        else {
            return;
        };
        let host_id = agent.host_id.clone();
        let stream = agent.instance_stream.clone();
        let id = id_for_send.clone();
        spawn_local(async move {
            if let Err(error) = send_frame(
                &host_id,
                stream,
                FrameKind::SendQueuedMessageNow,
                &SendQueuedMessageNowPayload { id },
            )
            .await
            {
                log::error!("failed to send send_queued_message_now: {error}");
            }
        });
    };

    let on_cancel = move |_| {
        let Some(active) = state_cancel.active_agent.get_untracked() else {
            return;
        };
        let agents = state_cancel.agents.get_untracked();
        let Some(agent) = agents
            .iter()
            .find(|agent| agent.host_id == active.host_id && agent.agent_id == active.agent_id)
        else {
            return;
        };
        let host_id = agent.host_id.clone();
        let stream = agent.instance_stream.clone();
        let id = id_for_cancel.clone();
        spawn_local(async move {
            if let Err(error) = send_frame(
                &host_id,
                stream,
                FrameKind::CancelQueuedMessage,
                &CancelQueuedMessagePayload { id },
            )
            .await
            {
                log::error!("failed to send cancel_queued_message: {error}");
            }
        });
    };

    view! {
        <div class="queued-message-item">
            <span class="queued-message-preview">{preview}</span>
            <button
                class="queued-message-btn queued-message-send-now"
                title="Send this message now"
                on:click=on_send_now
            >
                "↑ Send Now"
            </button>
            <button
                class="queued-message-btn queued-message-cancel"
                title="Cancel this queued message"
                on:click=on_cancel
            >
                "× Cancel"
            </button>
        </div>
    }
}

fn active_instance_stream(state: &AppState) -> Option<StreamPath> {
    let active_agent = state.active_agent.get_untracked()?;
    let agents = state.agents.get_untracked();
    agents
        .iter()
        .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
        .map(|a| a.instance_stream.clone())
}

fn active_instance_stream_tracked(state: &AppState) -> Option<StreamPath> {
    let active_agent = state.active_agent.get()?;
    state.agents.with(|agents| {
        agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
            .map(|a| a.instance_stream.clone())
    })
}

fn active_chat_target_ready_tracked(state: &AppState) -> bool {
    if state.active_agent.get().is_some() {
        active_instance_stream_tracked(state).is_some()
    } else {
        true
    }
}

fn selected_backend_kind(state: &AppState) -> Option<BackendKind> {
    if let Some(active_agent) = state.active_agent.get_untracked() {
        let agents = state.agents.get_untracked();
        if let Some(agent) = agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
        {
            return Some(agent.backend_kind);
        }
    }

    let draft = state.draft_backend_override.get_untracked();
    draft.or_else(|| {
        state
            .chat_context_host_settings_untracked()
            .and_then(|settings| {
                settings
                    .default_backend
                    .or_else(|| settings.enabled_backends.first().copied())
            })
    })
}

fn selected_backend_kind_tracked(state: &AppState) -> Option<BackendKind> {
    if let Some(active_agent) = state.active_agent.get() {
        let backend = state.agents.with(|agents| {
            agents
                .iter()
                .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
                .map(|a| a.backend_kind)
        });
        if let Some(backend) = backend {
            return Some(backend);
        }
    }

    let draft = state.draft_backend_override.get();
    draft.or_else(|| {
        state.chat_context_host_settings().and_then(|settings| {
            settings
                .default_backend
                .or_else(|| settings.enabled_backends.first().copied())
        })
    })
}

fn active_agent_is_initializing_tracked(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    // `with` reads through the signal without cloning the inner
    // `Vec<AgentInfo>`. The previous `state.agents.get()` cloned the
    // whole list every time this helper ran — and ui_mode Memo
    // re-runs on every chat_input keystroke, so a dozen agents +
    // a fast typer = a Vec clone per character.
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.host_id == active_agent.host_id
                && agent.agent_id == active_agent.agent_id
                && !agent.started
                && agent.fatal_error.is_none()
        })
    })
}

fn active_agent_is_backend_native(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.host_id == active_agent.host_id
                && agent.agent_id == active_agent.agent_id
                && matches!(agent.origin, AgentOrigin::BackendNative)
        })
    })
}

/// True when the active agent has reported a backend session id, which is
/// required to fork via "Fork + send". Tracked so the Fork + send menu
/// item appears the moment the `AgentStart`/bootstrap event lands.
fn active_agent_has_session_id_tracked(state: &AppState) -> bool {
    let Some(active_agent) = state.active_agent.get() else {
        return false;
    };
    state.agents.with(|agents| {
        agents.iter().any(|agent| {
            agent.host_id == active_agent.host_id
                && agent.agent_id == active_agent.agent_id
                && agent.session_id.is_some()
        })
    })
}

fn pending_images_to_payload(images: &[PendingImage]) -> Option<Vec<ImageData>> {
    if images.is_empty() {
        None
    } else {
        Some(
            images
                .iter()
                .map(|image| ImageData {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                })
                .collect(),
        )
    }
}

/// Why a fresh "New Chat" draft can't be started yet, limited to things the
/// user can fix from Settings → Backends. Drives the inline notice above the
/// composer so a misconfigured first run guides the user instead of silently
/// eating their message.
#[derive(Clone, Copy, PartialEq)]
enum DraftBackendNotice {
    NoBackend,
    NotInstalled(BackendKind),
}

fn draft_backend_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Tycode => "Tycode",
        BackendKind::Kiro => "Kiro",
        BackendKind::Claude => "Claude",
        BackendKind::Codex => "Codex",
        BackendKind::Antigravity => "Antigravity",
        BackendKind::Hermes => "Hermes",
    }
}

impl DraftBackendNotice {
    fn message(self) -> String {
        match self {
            DraftBackendNotice::NoBackend => {
                "No agent backend is set up yet — connect one to start chatting.".to_string()
            }
            DraftBackendNotice::NotInstalled(kind) => format!(
                "{} isn't installed on this host yet — finish setup to start chatting.",
                draft_backend_label(kind)
            ),
        }
    }

    fn cta(self) -> &'static str {
        match self {
            DraftBackendNotice::NoBackend => "Connect an agent backend →",
            DraftBackendNotice::NotInstalled(_) => "Finish setup →",
        }
    }
}

/// Readiness of the backend a fresh draft would spawn. `None` means "go ahead":
/// either a live agent is active (not a draft), we're mid team-member
/// activation, we're not connected (handled elsewhere), or the resolved backend
/// looks usable. We can detect a missing or not-installed backend up front;
/// "installed but not signed in" only surfaces as a runtime spawn error, so it
/// is intentionally not covered here.
fn draft_backend_notice(state: &AppState) -> Option<DraftBackendNotice> {
    if state.active_agent.get().is_some() {
        return None;
    }
    if state.active_pending_team_member_untracked().is_some() {
        return None;
    }
    if !matches!(
        state.chat_context_connection_status(),
        ConnectionStatus::Connected
    ) {
        return None;
    }
    let host_id = state.chat_context_host_id()?;
    let settings = state.chat_context_host_settings()?;
    let backend = state
        .draft_backend_override
        .get()
        .or(settings.default_backend)
        .or_else(|| settings.enabled_backends.first().copied());
    let Some(backend) = backend else {
        return Some(DraftBackendNotice::NoBackend);
    };
    let not_installed = state.backend_setup_by_host.with(|map| {
        map.get(&host_id)
            .and_then(|infos| infos.iter().find(|info| info.backend_kind == backend))
            .map(|info| info.status == BackendSetupStatus::NotInstalled)
            .unwrap_or(false)
    });
    not_installed.then_some(DraftBackendNotice::NotInstalled(backend))
}

fn restore_submitted_input(
    state: &AppState,
    pending_images: RwSignal<Vec<PendingImage>>,
    draft: String,
    images: Vec<PendingImage>,
) {
    if state.chat_input.get_untracked().is_empty() && pending_images.get_untracked().is_empty() {
        state.chat_input.set(draft);
        pending_images.set(images);
    }
}

fn submit_chat_input(state: &AppState, pending_images: RwSignal<Vec<PendingImage>>) {
    let draft = state.chat_input.get_untracked();
    let text = draft.trim().to_owned();
    let images = pending_images.get_untracked();
    let payload_images = pending_images_to_payload(&images);
    if text.is_empty() && payload_images.is_none() {
        return;
    }

    // A draft with no usable backend: keep the text and let the inline notice
    // above the composer guide the user to setup, instead of clearing the input
    // and silently failing to spawn.
    if draft_backend_notice(state).is_some() {
        return;
    }

    if state.active_agent.get_untracked().is_none() {
        // Active tab has no live agent. If it's a draft team-member tab,
        // route through `TeamMemberActivate` so the server spawns the agent
        // under the right `AgentOrigin::TeamMember` (see
        // `dev-docs/19-agent-teams.md` §5 and the backend
        // `activate_team_member` flow). The server's `NewAgent` echo will
        // upgrade this tab's `agent_ref` in dispatch.rs. Otherwise it's an
        // ordinary "New Chat" draft and we fall through to `spawn_new_chat`.
        if let Some(pending) = state.active_pending_team_member_untracked() {
            let Some(stream) = state.host_stream_untracked(&pending.host_id) else {
                log::error!(
                    "submit_chat_input: host stream missing for {host}",
                    host = pending.host_id
                );
                return;
            };
            state.chat_input.set(String::new());
            pending_images.set(Vec::new());
            let restore_state = state.clone();
            let restore_draft = draft.clone();
            let restore_images = images.clone();
            spawn_local(async move {
                if let Err(error) = crate::send::team_member_activate(
                    &pending.host_id,
                    stream,
                    pending.member_id,
                    Some(text),
                    payload_images,
                )
                .await
                {
                    log::error!("team_member_activate (with prompt) failed: {error}");
                    restore_submitted_input(
                        &restore_state,
                        pending_images,
                        restore_draft,
                        restore_images,
                    );
                }
            });
            return;
        }
        let restore_state = state.clone();
        let restore_draft = draft.clone();
        let restore_images = images.clone();
        if spawn_new_chat(state, text, payload_images, move |_| {
            restore_submitted_input(
                &restore_state,
                pending_images,
                restore_draft,
                restore_images,
            );
        }) {
            state.chat_input.set(String::new());
            pending_images.set(Vec::new());
        }
        return;
    }

    let active_agent = match state.active_agent.get_untracked() {
        Some(active_agent) => active_agent,
        None => return,
    };
    let host_id = active_agent.host_id.clone();

    let instance_stream = match active_instance_stream(state) {
        Some(stream) => stream,
        None => {
            log::error!("submit_chat_input: active agent stream missing");
            return;
        }
    };

    state.chat_input.set(String::new());
    pending_images.set(Vec::new());
    let restore_state = state.clone();
    let restore_draft = draft.clone();
    let restore_images = images.clone();
    spawn_local(async move {
        let payload = SendMessagePayload {
            message: text,
            images: payload_images,
            origin: None,
            tool_response: None,
        };
        if let Err(e) =
            send_frame(&host_id, instance_stream, FrameKind::SendMessage, &payload).await
        {
            log::error!("failed to send message: {e}");
            restore_submitted_input(
                &restore_state,
                pending_images,
                restore_draft,
                restore_images,
            );
        }
    });
}

/// Fork the session and send the draft as a new side question, then clear the
/// draft optimistically — mirroring `submit_chat_input`'s clear-on-submit.
/// The Fork + send menu item only shows when there is input and the active agent
/// has a session id, so the guard here just protects against an empty draft.
fn submit_side_question(state: &AppState, pending_images: RwSignal<Vec<PendingImage>>) {
    let text = state.chat_input.get_untracked();
    let text = text.trim().to_owned();
    let images = pending_images.get_untracked();
    let payload_images = pending_images_to_payload(&images);
    if text.is_empty() && payload_images.is_none() {
        return;
    }

    state.chat_input.set(String::new());
    pending_images.set(Vec::new());

    crate::actions::spawn_side_question(state, text, payload_images);
}

fn interrupt_active_turn(state: &AppState) {
    let host_id = match state.active_agent.get_untracked() {
        Some(active_agent) => active_agent.host_id,
        None => return,
    };

    let instance_stream = match active_instance_stream(state) {
        Some(stream) => stream,
        None => return,
    };

    spawn_local(async move {
        if let Err(e) = send_frame(
            &host_id,
            instance_stream,
            FrameKind::Interrupt,
            &InterruptPayload::default(),
        )
        .await
        {
            log::error!("failed to interrupt conversation: {e}");
        }
    });
}

fn steer_chat_input(state: &AppState, pending_images: RwSignal<Vec<PendingImage>>) {
    let draft = state.chat_input.get_untracked();
    let text = draft.trim().to_owned();
    let images = pending_images.get_untracked();
    let payload_images = pending_images_to_payload(&images);
    if text.is_empty() && payload_images.is_none() {
        interrupt_active_turn(state);
        return;
    }

    let host_id = match state.active_agent.get_untracked() {
        Some(active_agent) => active_agent.host_id,
        None => return,
    };

    let instance_stream = match active_instance_stream(state) {
        Some(stream) => stream,
        None => {
            log::error!("steer_chat_input: active agent stream missing");
            return;
        }
    };

    state.chat_input.set(String::new());
    pending_images.set(Vec::new());
    let restore_state = state.clone();
    let restore_draft = draft.clone();
    let restore_images = images.clone();

    spawn_local(async move {
        if let Err(e) = send_frame(
            &host_id,
            instance_stream.clone(),
            FrameKind::Interrupt,
            &InterruptPayload::default(),
        )
        .await
        {
            log::error!("failed to interrupt conversation for steer: {e}");
            restore_submitted_input(
                &restore_state,
                pending_images,
                restore_draft,
                restore_images,
            );
            return;
        }

        let payload = SendMessagePayload {
            message: text,
            images: payload_images,
            origin: None,
            tool_response: None,
        };
        if let Err(e) =
            send_frame(&host_id, instance_stream, FrameKind::SendMessage, &payload).await
        {
            log::error!("failed to send steer message: {e}");
            restore_submitted_input(
                &restore_state,
                pending_images,
                restore_draft,
                restore_images,
            );
        }
    });
}

fn data_transfer_files(ev: &web_sys::DragEvent) -> Vec<web_sys::File> {
    let Some(data_transfer) = ev.data_transfer() else {
        return Vec::new();
    };
    let Some(files) = data_transfer.files() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for idx in 0..files.length() {
        if let Some(file) = files.get(idx) {
            out.push(file);
        }
    }
    out
}

fn data_transfer_has_files(ev: &web_sys::DragEvent) -> bool {
    let Some(data_transfer) = ev.data_transfer() else {
        return false;
    };

    let types: Array = data_transfer.types();
    types
        .iter()
        .any(|value| value.as_string().as_deref() == Some("Files"))
}

fn parse_data_url(data_url: &str) -> Result<(String, String), String> {
    let rest = data_url
        .strip_prefix("data:")
        .ok_or_else(|| "Dropped file did not produce a valid data URL".to_string())?;
    let (meta, data) = rest
        .split_once(',')
        .ok_or_else(|| "Dropped file produced a malformed data URL".to_string())?;
    let media_type = meta
        .strip_suffix(";base64")
        .unwrap_or(meta)
        .trim()
        .to_string();
    Ok((media_type, data.to_string()))
}

fn js_error_to_string(err: JsValue) -> String {
    err.as_string().unwrap_or_else(|| format!("{err:?}"))
}

async fn read_image_file(file: web_sys::File) -> Result<PendingImage, String> {
    let name = file.name();
    let media_type = file.type_();
    if !media_type.starts_with("image/") {
        return Err(format!("{name} is not an image file"));
    }

    let promise = Promise::new(&mut move |resolve, reject| {
        let reader = match web_sys::FileReader::new() {
            Ok(reader) => reader,
            Err(err) => {
                let _ = reject.call1(&JsValue::UNDEFINED, &err);
                return;
            }
        };

        type FileReaderCallback = Closure<dyn FnMut(web_sys::ProgressEvent)>;

        let onload_slot: Rc<RefCell<Option<FileReaderCallback>>> = Rc::new(RefCell::new(None));
        let onerror_slot: Rc<RefCell<Option<FileReaderCallback>>> = Rc::new(RefCell::new(None));

        let reader_for_load = reader.clone();
        let resolve_fn = resolve.clone();
        let reject_for_load = reject.clone();
        let onload_slot_for_load = onload_slot.clone();
        let onerror_slot_for_load = onerror_slot.clone();
        let onload = Closure::wrap(Box::new(move |_ev: web_sys::ProgressEvent| {
            reader_for_load.set_onload(None);
            reader_for_load.set_onerror(None);
            match reader_for_load.result() {
                Ok(result) => {
                    let _ = resolve_fn.call1(&JsValue::UNDEFINED, &result);
                }
                Err(err) => {
                    let _ = reject_for_load.call1(&JsValue::UNDEFINED, &err);
                }
            }
            onload_slot_for_load.borrow_mut().take();
            onerror_slot_for_load.borrow_mut().take();
        }) as Box<dyn FnMut(_)>);

        let reader_for_error = reader.clone();
        let reject_for_error = reject.clone();
        let reject_for_start = reject.clone();
        let onload_slot_for_error = onload_slot.clone();
        let onerror_slot_for_error = onerror_slot.clone();
        let onerror = Closure::wrap(Box::new(move |_ev: web_sys::ProgressEvent| {
            reader_for_error.set_onload(None);
            reader_for_error.set_onerror(None);
            let err = JsValue::from_str("Failed to read dropped image");
            let _ = reject_for_error.call1(&JsValue::UNDEFINED, &err);
            onload_slot_for_error.borrow_mut().take();
            onerror_slot_for_error.borrow_mut().take();
        }) as Box<dyn FnMut(_)>);

        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        reader.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onload_slot.borrow_mut().replace(onload);
        onerror_slot.borrow_mut().replace(onerror);

        if let Err(err) = reader.read_as_data_url(&file) {
            reader.set_onload(None);
            reader.set_onerror(None);
            onload_slot.borrow_mut().take();
            onerror_slot.borrow_mut().take();
            let _ = reject_for_start.call1(&JsValue::UNDEFINED, &err);
        }
    });

    let data_url = JsFuture::from(promise)
        .await
        .map_err(js_error_to_string)?
        .as_string()
        .ok_or_else(|| format!("Failed to decode dropped image {name}"))?;
    let (parsed_media_type, data) = parse_data_url(&data_url)?;

    Ok(PendingImage {
        name,
        media_type: if parsed_media_type.is_empty() {
            media_type
        } else {
            parsed_media_type
        },
        data,
    })
}

#[component]
pub fn ChatInput() -> impl IntoView {
    let state = expect_context::<AppState>();
    let pending_images = RwSignal::new(Vec::<PendingImage>::new());
    let attachment_error = RwSignal::new(None::<String>);
    let drag_depth = RwSignal::new(0u32);

    let ui_state = state.clone();
    let ui_images = pending_images;
    let ui_mode = Memo::new(move |_| {
        let is_connected = matches!(
            ui_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        );
        // `with` reads through the signal to compute `has_text` without
        // cloning the input string per keystroke — `chat_input.get()`
        // would clone the entire String into a temporary just to check
        // `trim().is_empty()` and drop it.
        let has_text = ui_state.chat_input.with(|s| !s.trim().is_empty());
        let has_images = ui_images.with(|images| !images.is_empty());
        let has_input = has_text || has_images;
        let target_ready = active_chat_target_ready_tracked(&ui_state);
        let is_thinking = active_agent_is_initializing_tracked(&ui_state)
            || ui_state
                .active_agent
                .get()
                .map(|agent_ref| {
                    ui_state
                        .agent_turn_active
                        .with(|map| map.get(&agent_ref.agent_id).copied().unwrap_or(false))
                })
                .unwrap_or(false);

        let send_enabled = is_connected && has_input && target_ready;
        let interrupt_enabled = is_connected && is_thinking && target_ready;
        // (send_enabled, interrupt_enabled, is_steer). `is_steer` (running with
        // input) gates the secondary Steer and Cancel items in the dropdown.
        (
            send_enabled,
            interrupt_enabled,
            is_thinking && has_input && target_ready,
        )
    });

    let readonly_state = state.clone();
    let is_readonly = Memo::new(move |_| active_agent_is_backend_native(&readonly_state));

    let btw_state = state.clone();
    let active_has_session = Memo::new(move |_| active_agent_has_session_id_tracked(&btw_state));
    // A "Fork + send" needs draft input and a forkable backend session.
    let can_btw = move || ui_mode.get().0 && active_has_session.get();

    let can_send = move || ui_mode.get().0 && !is_readonly.get();
    let can_interrupt = move || ui_mode.get().1 && !is_readonly.get();
    let is_steer = Memo::new(move |_| ui_mode.get().2);

    // The dropdown holds items only in specific states (see state matrix):
    // - Fork + send: idle or thinking + input + session
    // - Steer + Cancel: thinking + input (with or without session)
    let menu_has_items = Memo::new(move |_| can_btw() || (can_interrupt() && is_steer.get()));
    let menu_open = RwSignal::new(false);
    // Auto-dismiss a stale-open menu when its items disappear.
    Effect::new(move |_| {
        if !menu_has_items.get() {
            menu_open.set(false);
        }
    });

    let thinking_state = state.clone();
    let is_thinking = move || {
        if active_agent_is_initializing_tracked(&thinking_state) {
            return true;
        }
        thinking_state
            .active_agent
            .get()
            .map(|agent_ref| {
                // `with` reads through the HashMap signal without cloning it.
                thinking_state
                    .agent_turn_active
                    .with(|map| map.get(&agent_ref.agent_id).copied().unwrap_or(false))
            })
            .unwrap_or(false)
    };

    let submit_on_enter_state = state.clone();
    let submit_on_enter_images = pending_images;
    let submit_on_enter_mode = move || {
        matches!(
            submit_on_enter_state.chat_context_connection_status(),
            ConnectionStatus::Connected
        ) && active_chat_target_ready_tracked(&submit_on_enter_state)
            && (submit_on_enter_state
                .chat_input
                .with(|s| !s.trim().is_empty())
                || submit_on_enter_images.with(|images| !images.is_empty()))
    };

    let on_keydown_state = state.clone();
    let on_keydown_images = pending_images;
    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() != "Enter" {
            return;
        }
        // Cmd (macOS) or Ctrl (other platforms) is the "command" modifier.
        let command = ev.meta_key() || ev.ctrl_key();
        if command {
            // Explicit chord — always acts, overriding the submit-on-Enter
            // toggle. ui_mode.0 is `send_enabled` (connected + has input).
            ev.prevent_default();
            if ev.shift_key() {
                // Cmd/Ctrl+Shift+Enter → Fork + send, when available.
                if ui_mode.get_untracked().0 && active_has_session.get_untracked() {
                    submit_side_question(&on_keydown_state, on_keydown_images);
                }
            } else if is_steer.get_untracked() {
                // Cmd/Ctrl+Enter while thinking with input → steer, mirroring
                // the dropdown Steer item's exact gate (`can_interrupt() &&
                // is_steer`), which excludes read-only backend-native agents.
                // If steer isn't actionable, no-op — do NOT fall through to
                // send (the dropdown offers nothing actionable here either).
                if ui_mode.get_untracked().1 && !is_readonly.get_untracked() {
                    steer_chat_input(&on_keydown_state, on_keydown_images);
                }
            } else if ui_mode.get_untracked().0 {
                // Cmd/Ctrl+Enter otherwise → normal send.
                submit_chat_input(&on_keydown_state, on_keydown_images);
            }
            return;
        }
        // Plain Enter → existing submit-on-Enter behavior. Shift+Enter (no
        // command modifier) falls through to the browser as a newline.
        if !ev.shift_key() {
            ev.prevent_default();
            if submit_on_enter_mode() {
                submit_chat_input(&on_keydown_state, on_keydown_images);
            }
        }
    };

    // Primary button handler: Cancel (interrupt) when thinking+empty, else submit.
    // Menu handlers park non-`Copy` AppState in a StoredValue so they're `Copy`.
    let primary_interrupt_stored = StoredValue::new_local(state.clone());
    let primary_submit_stored = StoredValue::new_local(state.clone());
    let primary_submit_images = pending_images;
    let on_click_primary = move |_| {
        let mode = ui_mode.get_untracked();
        let is_steer_now = is_steer.get_untracked();
        let readonly = is_readonly.get_untracked();
        if mode.1 && !is_steer_now && !readonly {
            primary_interrupt_stored.with_value(interrupt_active_turn);
        } else {
            primary_submit_stored.with_value(|s| submit_chat_input(s, primary_submit_images));
        }
    };

    let menu_state = StoredValue::new_local(state.clone());
    let menu_images = pending_images;
    let on_menu_btw = move |_| {
        menu_open.set(false);
        menu_state.with_value(|s| submit_side_question(s, menu_images));
    };
    let on_menu_steer = move |_| {
        menu_open.set(false);
        menu_state.with_value(|s| steer_chat_input(s, menu_images));
    };
    let on_menu_cancel = move |_| {
        menu_open.set(false);
        menu_state.with_value(interrupt_active_turn);
    };

    let on_split_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Escape" && menu_open.get() {
            ev.prevent_default();
            menu_open.set(false);
        }
    };

    let on_input_state = state.clone();
    // Throttle textarea autosize to one update per animation frame.
    // The previous code ran height="auto" → read scrollHeight →
    // write height inline on every keypress, which forces a synchronous
    // layout each character. Coalescing into rAF caps this to 60Hz
    // and aligns the layout pass with the browser's paint cycle.
    let autosize_pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let on_input = move |ev: leptos::ev::Event| {
        let target = event_target_value(&ev);
        on_input_state.chat_input.set(target);
        if autosize_pending.get() {
            return;
        }
        let Some(node) = ev.target() else {
            return;
        };
        let Ok(textarea) = node.dyn_into::<web_sys::HtmlTextAreaElement>() else {
            return;
        };
        autosize_pending.set(true);
        let pending = autosize_pending.clone();
        leptos::prelude::request_animation_frame(move || {
            pending.set(false);
            let style = web_sys::HtmlElement::from(textarea.clone()).style();
            // Hide overflow before measuring so a scrollbar doesn't inflate scrollHeight.
            let _ = style.set_property("overflow-y", "hidden");
            let _ = style.set_property("height", "auto");
            let scroll_h = textarea.scroll_height();
            // Account for border-box: borders aren't in scrollHeight but are in offsetHeight.
            let border_h = textarea.offset_height() - textarea.client_height();
            let _ = style.set_property("height", &format!("{}px", scroll_h + border_h));
            // Re-enable scrollbar only if CSS max-height is now capping the element.
            let overflow = if textarea.scroll_height() > textarea.client_height() {
                "auto"
            } else {
                "hidden"
            };
            let _ = style.set_property("overflow-y", overflow);
        });
    };

    let on_dragenter_error = attachment_error;
    let on_dragenter_depth = drag_depth;
    let on_dragenter = move |ev: web_sys::DragEvent| {
        if !data_transfer_has_files(&ev) {
            return;
        }
        ev.prevent_default();
        on_dragenter_error.set(None);
        on_dragenter_depth.update(|depth| *depth += 1);
    };

    let on_dragover_depth = drag_depth;
    let on_dragover = move |ev: web_sys::DragEvent| {
        if !data_transfer_has_files(&ev) {
            return;
        }
        ev.prevent_default();
        if let Some(data_transfer) = ev.data_transfer() {
            data_transfer.set_drop_effect("copy");
        }
        on_dragover_depth.set(1);
    };

    let on_dragleave_depth = drag_depth;
    let on_dragleave = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        on_dragleave_depth.update(|depth| *depth = depth.saturating_sub(1));
    };

    let on_drop_state = state.clone();
    let on_drop_images = pending_images;
    let on_drop_error = attachment_error;
    let on_drop_depth = drag_depth;
    let on_drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        on_drop_depth.set(0);

        let files = data_transfer_files(&ev);
        if files.is_empty() {
            return;
        }

        if !selected_backend_kind(&on_drop_state)
            .map(BackendKind::supports_image_input)
            .unwrap_or(false)
        {
            let backend_name = selected_backend_kind(&on_drop_state)
                .map(|backend| format!("{backend:?}"))
                .unwrap_or_else(|| "selected backend".to_string());
            on_drop_error.set(Some(format!("{backend_name} does not support image input")));
            return;
        }

        on_drop_error.set(None);
        spawn_local(async move {
            let mut added_images = Vec::new();
            let mut errors = Vec::new();
            for file in files {
                match read_image_file(file).await {
                    Ok(image) => added_images.push(image),
                    Err(err) => errors.push(err),
                }
            }

            if !added_images.is_empty() {
                on_drop_images.update(|images| images.extend(added_images));
            }

            if errors.is_empty() {
                on_drop_error.set(None);
            } else {
                on_drop_error.set(Some(errors.join(" ")));
            }
        });
    };

    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();

    // Synchronise the textarea's `value` property *only* when the
    // signal diverges from what the DOM already has — the common case
    // is the user typing, where `on_input` already mutated the
    // textarea before the signal updated, so we'd otherwise re-set
    // the property to the same string per keystroke. The comparison
    // turns those into no-ops while still letting external resets
    // (send, attach, prefill) push their value into the DOM.
    //
    // This replaces the previous `prop:value=move || …` reactive
    // binding on the textarea, which subscribed unconditionally and
    // ran a property write per keystroke.
    let reset_state = state.clone();
    Effect::new(move |_| {
        let Some(el) = textarea_ref.get() else {
            return;
        };
        let textarea: web_sys::HtmlTextAreaElement = (*el).clone().unchecked_into();
        // `with` reads the signal in place to skip cloning the input
        // string per keystroke; the previous `chat_input.get()`
        // allocated a fresh `String` just to compare against the DOM
        // value and (in the no-op case) drop it.
        let needs_set = reset_state.chat_input.with(|val| textarea.value() != *val);
        if needs_set {
            let val = reset_state.chat_input.get_untracked();
            textarea.set_value(&val);
        }
        let is_empty = reset_state.chat_input.with(|val| val.is_empty());
        if is_empty {
            let html_el: web_sys::HtmlElement = el.into();
            let style = html_el.style();
            let _ = style.set_property("height", "auto");
            let _ = style.set_property("overflow-y", "hidden");
        }
    });

    let overlay_support_state = state.clone();
    let overlay_is_unsupported = Memo::new(move |_| {
        !selected_backend_kind_tracked(&overlay_support_state)
            .map(BackendKind::supports_image_input)
            .unwrap_or(false)
    });
    let overlay_support_copy_state = state.clone();
    let overlay_drop_copy = Memo::new(move |_| {
        if selected_backend_kind_tracked(&overlay_support_copy_state)
            .map(BackendKind::supports_image_input)
            .unwrap_or(false)
        {
            "Drop images to attach".to_string()
        } else {
            "This backend does not support image input".to_string()
        }
    });

    // Draft "New Chat" with no usable backend → inline guidance toward setup.
    let notice_compute_state = state.clone();
    let backend_notice = Memo::new(move |_| draft_backend_notice(&notice_compute_state));
    let notice_state = state.clone();

    let queue_state = state.clone();
    let queue_ids = Memo::new(move |_| -> Vec<QueuedMessageId> {
        let Some(active) = queue_state.active_agent.get() else {
            return Vec::new();
        };
        queue_state.agent_message_queue.with(|queue| {
            queue
                .get(&active.agent_id)
                .map(|entries| entries.iter().map(|e| e.id.clone()).collect())
                .unwrap_or_default()
        })
    });

    view! {
        <div
            class="chat-input-area"
            class:thinking=is_thinking
            on:dragenter=on_dragenter
            on:dragover=on_dragover
            on:dragleave=on_dragleave
            on:drop=on_drop
        >
            <Show when=move || { drag_depth.get() > 0 }>
                <div
                    class="chat-input-drop-overlay"
                    class:unsupported=move || overlay_is_unsupported.get()
                >
                    <div class="chat-input-drop-copy">
                        {move || overlay_drop_copy.get()}
                    </div>
                </div>
            </Show>

            <Show when=move || !pending_images.get().is_empty()>
                <div class="chat-attachment-bar">
                    {move || {
                        pending_images
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(index, image)| {
                                let src = format!("data:{};base64,{}", image.media_type, image.data);
                                let remove_images = pending_images;
                                let remove_error = attachment_error;
                                let name = image.name.clone();
                                view! {
                                    <div class="chat-attachment-card">
                                        <button
                                            class="chat-attachment-remove"
                                            title="Remove image"
                                            on:click=move |_| {
                                                remove_images.update(|images| {
                                                    if index < images.len() {
                                                        images.remove(index);
                                                    }
                                                });
                                                remove_error.set(None);
                                            }
                                        >
                                            "×"
                                        </button>
                                        <img
                                            class="chat-attachment-thumb"
                                            src=src
                                            alt=name.clone()
                                        />
                                        <div class="chat-attachment-name">{name}</div>
                                    </div>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </div>
            </Show>

            <Show when=move || attachment_error.get().is_some()>
                <div class="chat-input-error">
                    {move || attachment_error.get().unwrap_or_default()}
                </div>
            </Show>

            <Show when=move || !queue_ids.get().is_empty()>
                <div class="queued-messages">
                    <For
                        each=move || queue_ids.get()
                        key=|id| id.0.clone()
                        let:id
                    >
                        <QueuedMessageRow id=id />
                    </For>
                </div>
            </Show>

            <Show when=move || is_readonly.get()>
                <div class="chat-readonly-notice">"Read-only: native sub-agent"</div>
            </Show>

            <Show when=move || backend_notice.get().is_some()>
                <div class="chat-backend-notice">
                    <span class="chat-backend-notice-text">
                        {move || backend_notice.get().map(|n| n.message()).unwrap_or_default()}
                    </span>
                    <button
                        class="chat-backend-notice-cta"
                        on:click={
                            let notice_state = notice_state.clone();
                            move |_| {
                                notice_state.settings_tab_request.set(Some("Backends"));
                                notice_state.settings_open.set(true);
                            }
                        }
                    >
                        {move || backend_notice.get().map(|n| n.cta()).unwrap_or_default()}
                    </button>
                </div>
            </Show>

            <div class="chat-input-row">
                <textarea
                    class="chat-textarea"
                    placeholder="Type a message or drop images..."
                    prop:disabled=move || is_readonly.get()
                    on:input=on_input
                    on:keydown=on_keydown
                    rows="1"
                    node_ref=textarea_ref
                    spellcheck="false"
                    {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                    autocapitalize="none"
                    autocomplete="off"
                />
                <div
                    class="chat-send-split"
                    role="group"
                    aria-label="Send actions"
                    on:keydown=on_split_keydown
                >
                    <button
                        class="chat-send-btn chat-send-btn-text chat-send-split-primary"
                        data-test="chat-send-primary"
                        disabled=move || {
                            let mode = ui_mode.get();
                            let is_steer_now = is_steer.get();
                            let readonly = is_readonly.get();
                            // thinking+empty → Cancel, always enabled
                            if mode.1 && !is_steer_now && !readonly { false } else { !can_send() }
                        }
                        on:click=on_click_primary
                        title=move || {
                            let mode = ui_mode.get();
                            let is_steer_now = is_steer.get();
                            let readonly = is_readonly.get();
                            if mode.1 && !is_steer_now && !readonly { "Cancel current turn" }
                            else if is_steer_now && !readonly { "Queue message (Enter)" }
                            else { "Send message (Enter)" }
                        }
                    >
                        <span>{move || {
                            let mode = ui_mode.get();
                            let is_steer_now = is_steer.get();
                            let readonly = is_readonly.get();
                            if mode.1 && !is_steer_now && !readonly { "Cancel" }
                            else if is_steer_now && !readonly { "Queue" }
                            else { "Send" }
                        }}</span>
                    </button>
                    <button
                        type="button"
                        class="chat-send-btn chat-send-split-toggle"
                        data-test="chat-send-menu-toggle"
                        aria-haspopup="menu"
                        aria-expanded=move || {
                            if menu_open.get() { "true" } else { "false" }
                        }
                        aria-label="More send actions"
                        title="More send actions"
                        disabled=move || !menu_has_items.get()
                        on:click=move |_| menu_open.update(|open| *open = !*open)
                    >
                        <span aria-hidden="true">"⌄"</span>
                    </button>
                    <Show when=move || menu_open.get() && menu_has_items.get()>
                        <div
                            class="chat-send-menu-backdrop"
                            on:click=move |_| menu_open.set(false)
                        ></div>
                        <div
                            class="chat-send-menu"
                            role="menu"
                            aria-label="Send actions"
                            data-test="chat-send-menu"
                        >
                            <Show when=move || can_interrupt() && is_steer.get()>
                                <button
                                    type="button"
                                    class="chat-send-menu-item"
                                    role="menuitem"
                                    data-test="chat-send-menu-steer"
                                    title="Interrupt current turn and redirect with your message"
                                    on:click=on_menu_steer
                                >
                                    <span class="chat-send-menu-label">"Steer"</span>
                                    <span class="chat-send-menu-shortcut" aria-hidden="true">
                                        "⌘↵"
                                    </span>
                                </button>
                            </Show>
                            <Show when=move || can_btw()>
                                <button
                                    type="button"
                                    class="chat-send-menu-item"
                                    role="menuitem"
                                    data-test="chat-send-menu-ask-aside"
                                    title="Fork + send — forks this session and sends the draft to the fork"
                                    on:click=on_menu_btw
                                >
                                    <span class="chat-send-menu-label">"Fork + send"</span>
                                    <span class="chat-send-menu-shortcut" aria-hidden="true">
                                        "⌘⇧↵"
                                    </span>
                                </button>
                            </Show>
                            <Show when=move || can_interrupt() && is_steer.get()>
                                <button
                                    type="button"
                                    class="chat-send-menu-item"
                                    role="menuitem"
                                    data-test="chat-send-menu-cancel"
                                    title="Cancel the current turn"
                                    on:click=on_menu_cancel
                                >
                                    <span class="chat-send-menu-label">"Cancel"</span>
                                </button>
                            </Show>
                        </div>
                    </Show>
                </div>
            </div>
            <SessionSettingsBar />
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, ConnectionStatus, Tab, TabContent};
    use leptos::mount::mount_to;
    use protocol::{AgentId, AgentOrigin, BackendKind, SessionId, StreamPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "host-1";
    const AGENT: &str = "agent-1";

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

    /// Install a minimal `window.__TAURI__.core.invoke` stub so the bridge's
    /// `send_host_line` resolves in the headless test environment, which has no
    /// Tauri host. Without it, `submit_chat_input` performs its synchronous
    /// optimistic clear and then asynchronously *reverts* it via
    /// `restore_submitted_input` when the real send fails — and whether that
    /// async revert lands before or after a test's `next_tick().await` depends
    /// on total event-loop load, so the "draft cleared" assertion passes in
    /// isolation but races under the full suite. A succeeding send keeps the
    /// draft cleared, matching production. Only `send_host_line` is resolved;
    /// every other command keeps the default "no host" rejection so unrelated
    /// behaviour is unchanged.
    fn stub_send_host_line() {
        use wasm_bindgen::closure::Closure;
        let window = web_sys::window().unwrap();
        let invoke = Closure::<dyn Fn(JsValue, JsValue) -> js_sys::Promise>::new(
            |cmd: JsValue, _args: JsValue| {
                if cmd.as_string().as_deref() == Some("send_host_line") {
                    js_sys::Promise::resolve(&JsValue::NULL)
                } else {
                    js_sys::Promise::reject(&JsValue::from_str("no tauri host in test"))
                }
            },
        );
        let core = js_sys::Object::new();
        js_sys::Reflect::set(&core, &"invoke".into(), invoke.as_ref()).unwrap();
        let tauri = js_sys::Object::new();
        js_sys::Reflect::set(&tauri, &"core".into(), &core).unwrap();
        js_sys::Reflect::set(&window, &"__TAURI__".into(), &tauri).unwrap();
        invoke.forget();
    }

    /// Connect the active chat tab to a single live agent and optionally seed a
    /// draft / running turn / forkable session, mirroring how the dispatcher
    /// populates state when a chat is open.
    fn configure(state: &AppState, session: bool, running: bool, input: &str) {
        stub_send_host_line();
        let agent_id = AgentId(AGENT.to_owned());
        state.agents.set(vec![AgentInfo {
            host_id: HOST.to_owned(),
            agent_id: agent_id.clone(),
            name: "Agent".to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            session_id: session.then(|| SessionId("sess-1".to_owned())),
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath("/agent/agent-1/inst".to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }]);
        state.connection_statuses.update(|m| {
            m.insert(HOST.to_owned(), ConnectionStatus::Connected);
        });
        if running {
            state.agent_turn_active.update(|m| {
                m.insert(agent_id.clone(), true);
            });
        }
        if !input.is_empty() {
            state.chat_input.set(input.to_owned());
        }
        state.center_zone.update(|cz| {
            let id = crate::state::next_tab_id();
            cz.tabs.push(Tab {
                id,
                content: TabContent::Chat {
                    agent_ref: Some(ActiveAgentRef {
                        host_id: HOST.to_owned(),
                        agent_id: agent_id.clone(),
                    }),
                    pending_team_member: None,
                },
                label: "Chat".to_owned(),
                closeable: true,
            });
            cz.active_tab_id = Some(id);
        });
    }

    fn query(container: &HtmlElement, sel: &str) -> Option<web_sys::Element> {
        container.query_selector(sel).unwrap()
    }

    fn primary(container: &HtmlElement) -> web_sys::Element {
        query(container, "[data-test='chat-send-primary']").expect("primary button must be present")
    }

    fn caret(container: &HtmlElement) -> web_sys::Element {
        query(container, "[data-test='chat-send-menu-toggle']")
            .expect("caret button must always be present")
    }

    async fn open_menu(container: &HtmlElement) {
        let toggle: HtmlElement = caret(container).dyn_into().unwrap();
        toggle.click();
        next_tick().await;
    }

    /// Label text of every menu item in DOM order. Reads the dedicated label
    /// span so the appended keyboard-shortcut hint (e.g. "⌘↵") does not bleed
    /// into the compared label — the assertions still verify which items render,
    /// in what order, with what label.
    fn menu_item_texts(container: &HtmlElement) -> Vec<String> {
        let nodes = container.query_selector_all("[role='menuitem']").unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .map(|n| {
                let el: web_sys::Element = n.dyn_into().unwrap();
                let label = el
                    .query_selector(".chat-send-menu-label")
                    .unwrap()
                    .expect("each menu item must have a label span");
                label.text_content().unwrap_or_default().trim().to_owned()
            })
            .collect()
    }

    /// Build and dispatch a real `keydown` on `target` with the given key and
    /// modifiers, using the global `KeyboardEvent` constructor (the typed
    /// `web_sys::KeyboardEvent` binding isn't an enabled feature). Returns
    /// whether the event's default action was *not* prevented.
    fn dispatch_keydown(target: &web_sys::Element, key: &str, meta: bool, shift: bool) {
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"key".into(), &key.into()).unwrap();
        js_sys::Reflect::set(&init, &"metaKey".into(), &JsValue::from_bool(meta)).unwrap();
        js_sys::Reflect::set(&init, &"shiftKey".into(), &JsValue::from_bool(shift)).unwrap();
        js_sys::Reflect::set(&init, &"bubbles".into(), &JsValue::TRUE).unwrap();
        js_sys::Reflect::set(&init, &"cancelable".into(), &JsValue::TRUE).unwrap();
        let ctor = js_sys::Reflect::get(&js_sys::global(), &"KeyboardEvent".into()).unwrap();
        let ctor: js_sys::Function = ctor.unchecked_into();
        let args = js_sys::Array::of2(&"keydown".into(), &init);
        let event = js_sys::Reflect::construct(&ctor, &args).unwrap();
        let event: web_sys::Event = event.unchecked_into();
        target.dispatch_event(&event).unwrap();
    }

    // ── State matrix row 1: Idle + empty ──────────────────────────────────────
    // Primary "Send" disabled; caret visible but disabled; no dropdown items.
    #[wasm_bindgen_test]
    async fn idle_empty_send_disabled_caret_disabled_no_menu() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, false, false, "");
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
            "caret must be disabled when no menu items"
        );

        assert!(
            query(&container, "[data-test='chat-send-menu']").is_none(),
            "no menu should be open"
        );
    }

    // ── State matrix row 2: Idle + input, no session ──────────────────────────
    // Primary "Send" enabled; caret visible but disabled; no dropdown.
    #[wasm_bindgen_test]
    async fn idle_input_no_session_send_enabled_caret_disabled() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, false, false, "hello");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            !p.has_attribute("disabled"),
            "Send must be enabled with draft"
        );

        let c = caret(&container);
        assert!(
            c.has_attribute("disabled"),
            "caret must be disabled with no menu items (no session)"
        );
    }

    #[wasm_bindgen_test]
    async fn stale_active_agent_disables_send_and_keeps_draft() {
        let state = AppState::new();
        configure(&state, false, false, "hello");
        state.agents.set(Vec::new());
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert!(
            p.has_attribute("disabled"),
            "Send must be disabled when the active chat tab has no live agent stream"
        );

        dispatch_keydown(&textarea(&container), "Enter", true, false);
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "hello",
            "keyboard submit must not clear a draft that has no live agent stream"
        );
    }

    // ── State matrix row 3: Idle + input + session ────────────────────────────
    // Primary "Send" enabled; caret enabled; dropdown has "Fork + send" only.
    #[wasm_bindgen_test]
    async fn idle_input_with_session_send_enabled_menu_fork_send_only() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, true, false, "hello");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(p.text_content().unwrap_or_default().trim(), "Send");
        assert!(
            !p.has_attribute("disabled"),
            "Send must be enabled with draft"
        );

        let c = caret(&container);
        assert!(
            !c.has_attribute("disabled"),
            "caret must be enabled with a menu item"
        );

        open_menu(&container).await;
        assert_eq!(
            menu_item_texts(&container),
            vec!["Fork + send".to_owned()],
            "idle+session menu must be exactly 'Fork + send'"
        );
    }

    // ── State matrix row 4: Thinking + empty ─────────────────────────────────
    // Primary "Cancel" enabled; caret visible but disabled; no dropdown.
    #[wasm_bindgen_test]
    async fn thinking_empty_primary_cancel_caret_disabled() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, false, true, "");
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
            "caret must be disabled when no menu items (thinking+empty)"
        );
    }

    // ── State matrix row 5: Thinking + input, no session ─────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_no_session_queue_primary_steer_cancel_menu() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, false, true, "redirect this");
            provide_context(state);
            view! { <ChatInput /> }
        });
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
            vec!["Steer".to_owned(), "Cancel".to_owned()],
            "thinking+input menu must be exactly Steer then Cancel"
        );
    }

    // ── State matrix row 6: Thinking + input + session ───────────────────────
    // Primary "Queue" enabled; caret enabled; dropdown has "Steer", "Fork + send", "Cancel".
    #[wasm_bindgen_test]
    async fn thinking_input_with_session_queue_primary_full_menu() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, true, true, "redirect this");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let p = primary(&container);
        assert_eq!(
            p.text_content().unwrap_or_default().trim(),
            "Queue",
            "primary must be Queue when thinking with draft"
        );
        assert!(!p.has_attribute("disabled"), "Queue must be enabled");

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

    /// Caret is always rendered in the DOM regardless of state — it just becomes
    /// disabled when the dropdown would be empty.
    #[wasm_bindgen_test]
    async fn caret_always_in_dom() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            // No agent, no connection, empty input — the most minimal state.
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        let c = query(&container, "[data-test='chat-send-menu-toggle']");
        assert!(
            c.is_some(),
            "caret toggle must always be present in the DOM, even when disabled"
        );
    }

    // ── Image-only input coverage note ────────────────────────────────────────
    // The matrix says desktop input = "text/images". Full image-only tests
    // (e.g. idle image+session → Send enabled/caret enabled/Fork + send) are not
    // feasible from this wasm test module because:
    //   1. `pending_images` is a component-local RwSignal<Vec<PendingImage>>
    //      not exposed via AppState or any injectable context — there is no
    //      external setter.
    //   2. Simulating a file-drop via DragEvent requires a real FileList, which
    //      web-sys provides no constructor for in test scope.
    // The `has_input = has_text || has_images` gate in ui_mode uses a single
    // branch: both text and images set the same boolean, so behaviour is
    // identical — the text-only matrix rows above already exercise every
    // reachable code path gated on `has_input`. The attachment UI (thumbnails,
    // drop overlay, error banner) is rendered independently of the split-button
    // state machine and is not covered here.
    //
    // To add image-only tests in the future: move `pending_images` into
    // AppState or provide it via a Leptos context, then seed it in `configure`.

    /// Attachment error banner is absent in the baseline state so it does not
    /// pollute the split-button matrix rows above.
    #[wasm_bindgen_test]
    async fn no_attachment_error_in_baseline() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            configure(&state, false, false, "");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        assert!(
            query(&container, ".chat-input-error").is_none(),
            "attachment error banner must not appear in the baseline state"
        );
        assert!(
            query(&container, ".chat-attachment-bar").is_none(),
            "attachment thumbnail bar must not appear when no images are pending"
        );
    }

    /// Primary button never appears as a duplicate item inside the dropdown.
    #[wasm_bindgen_test]
    async fn primary_label_not_duplicated_in_dropdown() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            // thinking+input+session → primary=Queue, menu=Steer/Fork + send/Cancel
            configure(&state, true, true, "redirect");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        open_menu(&container).await;
        let items = menu_item_texts(&container);
        assert!(
            !items.iter().any(|t| t == "Queue"),
            "Queue (primary label) must not appear as a menu item: {items:?}"
        );
        assert!(
            !items.iter().any(|t| t == "Send"),
            "Send must not appear as a menu item: {items:?}"
        );
    }

    fn textarea(container: &HtmlElement) -> web_sys::Element {
        query(container, "textarea").expect("textarea must be present")
    }

    /// Cmd+Enter while idle with a draft submits the message: the observable
    /// effect is the draft being cleared, exactly as plain-Enter send does.
    #[wasm_bindgen_test]
    async fn cmd_enter_idle_submits_and_clears_draft() {
        let state = AppState::new();
        configure(&state, false, false, "hello");
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ChatInput /> }
        });
        next_tick().await;

        assert_eq!(state.chat_input.get_untracked(), "hello");
        dispatch_keydown(&textarea(&container), "Enter", true, false);
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "Cmd+Enter must submit and clear the draft"
        );
    }

    /// Cmd+Shift+Enter when Fork + send is available triggers the fork path,
    /// whose observable effect (like clicking "Fork + send") is the draft being
    /// cleared. We can't observe the dispatched protocol frame from this
    /// harness, so the cleared draft is the proxy for the fork being initiated.
    #[wasm_bindgen_test]
    async fn cmd_shift_enter_forks_and_clears_draft() {
        let state = AppState::new();
        // idle + session + input → Fork + send available (can_btw).
        configure(&state, true, false, "fork this");
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ChatInput /> }
        });
        next_tick().await;

        assert_eq!(state.chat_input.get_untracked(), "fork this");
        dispatch_keydown(&textarea(&container), "Enter", true, true);
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "",
            "Cmd+Shift+Enter must initiate Fork + send and clear the draft"
        );
    }

    /// Cmd+Shift+Enter when Fork + send is NOT available is a no-op: it must not
    /// submit (clear) the draft, and must not insert a newline.
    #[wasm_bindgen_test]
    async fn cmd_shift_enter_no_fork_is_noop() {
        let state = AppState::new();
        // idle + NO session + input → no fork available.
        configure(&state, false, false, "keep this");
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ChatInput /> }
        });
        next_tick().await;

        dispatch_keydown(&textarea(&container), "Enter", true, true);
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "keep this",
            "Cmd+Shift+Enter with no fork available must not submit or alter the draft"
        );
    }

    /// Cmd+Enter while thinking with input must NOT steer a read-only
    /// backend-native agent — the shortcut mirrors the dropdown Steer item,
    /// which is hidden in this state. Observable proxy: steer would clear the
    /// draft, so an unchanged draft proves the shortcut no-op'd.
    #[wasm_bindgen_test]
    async fn cmd_enter_readonly_thinking_does_not_steer() {
        let state = AppState::new();
        // thinking + session + input, then mark the agent backend-native so the
        // composer is read-only (matches `active_agent_is_backend_native`).
        configure(&state, true, true, "redirect this");
        state.agents.update(|agents| {
            agents[0].origin = AgentOrigin::BackendNative;
        });
        let mount_state = state.clone();
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            provide_context(mount_state.clone());
            view! { <ChatInput /> }
        });
        next_tick().await;

        dispatch_keydown(&textarea(&container), "Enter", true, false);
        next_tick().await;

        assert_eq!(
            state.chat_input.get_untracked(),
            "redirect this",
            "Cmd+Enter must not steer a read-only backend-native agent"
        );
    }

    /// The Steer and Fork + send items render their keyboard-shortcut hints,
    /// while Cancel renders none.
    #[wasm_bindgen_test]
    async fn menu_items_render_shortcut_hints() {
        let container = make_container();
        let _h = mount_to(container.clone(), move || {
            let state = AppState::new();
            // thinking + session + input → Steer, Fork + send, Cancel.
            configure(&state, true, true, "redirect");
            provide_context(state);
            view! { <ChatInput /> }
        });
        next_tick().await;

        open_menu(&container).await;

        let steer = query(&container, "[data-test='chat-send-menu-steer']")
            .expect("steer item must be present");
        assert!(
            steer.text_content().unwrap_or_default().contains("⌘↵"),
            "Steer item must show the ⌘↵ shortcut hint"
        );

        let fork = query(&container, "[data-test='chat-send-menu-ask-aside']")
            .expect("fork item must be present");
        assert!(
            fork.text_content().unwrap_or_default().contains("⌘⇧↵"),
            "Fork + send item must show the ⌘⇧↵ shortcut hint"
        );

        let cancel = query(&container, "[data-test='chat-send-menu-cancel']")
            .expect("cancel item must be present");
        assert!(
            !cancel.text_content().unwrap_or_default().contains('⌘'),
            "Cancel item must not show a shortcut hint"
        );
    }
}
