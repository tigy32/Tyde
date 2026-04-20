use js_sys::{Array, Promise};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::actions::spawn_new_chat;
use crate::components::session_settings::SessionSettingsBar;
use crate::send::send_frame;
use crate::state::{AppState, ConnectionStatus};

use protocol::{
    AgentOrigin, BackendKind, CancelQueuedMessagePayload, FrameKind, ImageData, InterruptPayload,
    QueuedMessageId, SendMessagePayload, SendQueuedMessageNowPayload, StreamPath,
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

    state
        .selected_host_settings_untracked()
        .and_then(|settings| {
            settings
                .default_backend
                .or_else(|| settings.enabled_backends.first().copied())
        })
}

fn selected_backend_kind_tracked(state: &AppState) -> Option<BackendKind> {
    if let Some(active_agent) = state.active_agent.get() {
        let agents = state.agents.get();
        if let Some(agent) = agents
            .iter()
            .find(|a| a.host_id == active_agent.host_id && a.agent_id == active_agent.agent_id)
        {
            return Some(agent.backend_kind);
        }
    }

    state.selected_host_settings().and_then(|settings| {
        settings
            .default_backend
            .or_else(|| settings.enabled_backends.first().copied())
    })
}

fn active_agent_is_initializing_tracked(state: &AppState) -> bool {
    let active_agent = match state.active_agent.get() {
        Some(active_agent) => active_agent,
        None => return false,
    };
    state.agents.get().iter().any(|agent| {
        agent.host_id == active_agent.host_id
            && agent.agent_id == active_agent.agent_id
            && !agent.started
            && agent.fatal_error.is_none()
    })
}

fn active_agent_is_backend_native(state: &AppState) -> bool {
    let active_agent = match state.active_agent.get() {
        Some(a) => a,
        None => return false,
    };
    state.agents.get().iter().any(|agent| {
        agent.host_id == active_agent.host_id
            && agent.agent_id == active_agent.agent_id
            && matches!(agent.origin, AgentOrigin::BackendNative)
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

fn submit_chat_input(state: &AppState, pending_images: RwSignal<Vec<PendingImage>>) {
    let text = state.chat_input.get_untracked();
    let text = text.trim().to_owned();
    let images = pending_images.get_untracked();
    let payload_images = pending_images_to_payload(&images);
    if text.is_empty() && payload_images.is_none() {
        return;
    }

    state.chat_input.set(String::new());
    pending_images.set(Vec::new());

    if state.active_agent.get_untracked().is_none() {
        spawn_new_chat(state, text, payload_images);
        return;
    }

    let host_id = match state.active_agent.get_untracked() {
        Some(active_agent) => active_agent.host_id,
        None => return,
    };

    let instance_stream = match active_instance_stream(state) {
        Some(stream) => stream,
        None => return,
    };

    spawn_local(async move {
        let payload = SendMessagePayload {
            message: text,
            images: payload_images,
        };
        if let Err(e) =
            send_frame(&host_id, instance_stream, FrameKind::SendMessage, &payload).await
        {
            log::error!("failed to send message: {e}");
        }
    });
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
    let text = state.chat_input.get_untracked();
    let text = text.trim().to_owned();
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
        None => return,
    };

    state.chat_input.set(String::new());
    pending_images.set(Vec::new());

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
            return;
        }

        let payload = SendMessagePayload {
            message: text,
            images: payload_images,
        };
        if let Err(e) =
            send_frame(&host_id, instance_stream, FrameKind::SendMessage, &payload).await
        {
            log::error!("failed to send steer message: {e}");
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

        let reader_for_load = reader.clone();
        let resolve_fn = resolve.clone();
        let reject_for_load = reject.clone();
        let onload =
            Closure::wrap(Box::new(move |_ev: web_sys::ProgressEvent| {
                match reader_for_load.result() {
                    Ok(result) => {
                        let _ = resolve_fn.call1(&JsValue::UNDEFINED, &result);
                    }
                    Err(err) => {
                        let _ = reject_for_load.call1(&JsValue::UNDEFINED, &err);
                    }
                }
            }) as Box<dyn FnMut(_)>);

        let reader_for_error = reader.clone();
        let reject_for_error = reject.clone();
        let reject_for_start = reject.clone();
        let onerror = Closure::wrap(Box::new(move |_ev: web_sys::ProgressEvent| {
            let _ = &reader_for_error;
            let err = JsValue::from_str("Failed to read dropped image");
            let _ = reject_for_error.call1(&JsValue::UNDEFINED, &err);
        }) as Box<dyn FnMut(_)>);

        reader.set_onload(Some(onload.as_ref().unchecked_ref()));
        reader.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        if let Err(err) = reader.read_as_data_url(&file) {
            let _ = reject_for_start.call1(&JsValue::UNDEFINED, &err);
            return;
        }

        onload.forget();
        onerror.forget();
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
            ui_state.selected_host_connection_status(),
            ConnectionStatus::Connected
        );
        let has_text = !ui_state.chat_input.get().trim().is_empty();
        let has_images = !ui_images.get().is_empty();
        let has_input = has_text || has_images;
        let is_thinking = active_agent_is_initializing_tracked(&ui_state)
            || ui_state
                .active_agent
                .get()
                .map(|agent_ref| {
                    ui_state
                        .agent_turn_active
                        .get()
                        .get(&agent_ref.agent_id)
                        .copied()
                        .unwrap_or(false)
                })
                .unwrap_or(false);

        let send_enabled = is_connected && has_input;
        let interrupt_enabled = is_connected && is_thinking;
        let send_label = if is_thinking && has_input {
            "Queue"
        } else {
            "Send"
        };
        let interrupt_label = if is_thinking && has_input {
            "Steer"
        } else {
            "Interrupt"
        };
        let interrupt_title = if is_thinking && has_input {
            "Interrupt current turn and steer with typed input"
        } else {
            "Interrupt current turn"
        };

        (
            send_enabled,
            interrupt_enabled,
            send_label,
            interrupt_label,
            interrupt_title,
            is_thinking && has_input,
        )
    });

    let readonly_state = state.clone();
    let is_readonly = Memo::new(move |_| active_agent_is_backend_native(&readonly_state));

    let can_send = move || ui_mode.get().0 && !is_readonly.get();
    let can_interrupt = move || ui_mode.get().1 && !is_readonly.get();
    let send_label = move || ui_mode.get().2;
    let interrupt_label = move || ui_mode.get().3;
    let interrupt_title = move || ui_mode.get().4;
    let is_steer = Memo::new(move |_| ui_mode.get().5);

    let thinking_state = state.clone();
    let is_thinking = move || {
        if active_agent_is_initializing_tracked(&thinking_state) {
            return true;
        }
        thinking_state
            .active_agent
            .get()
            .map(|agent_ref| {
                thinking_state
                    .agent_turn_active
                    .get()
                    .get(&agent_ref.agent_id)
                    .copied()
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    };

    let submit_on_enter_state = state.clone();
    let submit_on_enter_images = pending_images;
    let submit_on_enter_mode = move || {
        matches!(
            submit_on_enter_state.selected_host_connection_status(),
            ConnectionStatus::Connected
        ) && (!submit_on_enter_state.chat_input.get().trim().is_empty()
            || !submit_on_enter_images.get().is_empty())
    };

    let on_keydown_state = state.clone();
    let on_keydown_images = pending_images;
    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            if submit_on_enter_mode() {
                submit_chat_input(&on_keydown_state, on_keydown_images);
            }
        }
    };

    let on_click_state = state.clone();
    let on_click_images = pending_images;
    let on_click_send = move |_| {
        submit_chat_input(&on_click_state, on_click_images);
    };

    let on_click_interrupt_state = state.clone();
    let on_click_interrupt_images = pending_images;
    let is_steer_for_click = is_steer;
    let on_click_interrupt = move |_| {
        if is_steer_for_click.get() {
            steer_chat_input(&on_click_interrupt_state, on_click_interrupt_images);
        } else {
            interrupt_active_turn(&on_click_interrupt_state);
        }
    };

    let on_input_state = state.clone();
    let on_input = move |ev: leptos::ev::Event| {
        let target = event_target_value(&ev);
        on_input_state.chat_input.set(target);
        if let Some(el) = ev.target() {
            let el: web_sys::HtmlTextAreaElement = el.unchecked_into();
            let style = web_sys::HtmlElement::from(el.clone()).style();
            let _ = style.set_property("height", "auto");
            let scroll_h = el.scroll_height();
            let _ = style.set_property("height", &format!("{scroll_h}px"));
        }
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
    let reset_state = state.clone();
    Effect::new(move |_| {
        let val = reset_state.chat_input.get();
        if val.is_empty()
            && let Some(el) = textarea_ref.get()
        {
            let html_el: web_sys::HtmlElement = el.into();
            let _ = html_el.style().set_property("height", "auto");
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

    let queue_state = state.clone();
    let queue_ids = Memo::new(move |_| -> Vec<QueuedMessageId> {
        let Some(active) = queue_state.active_agent.get() else {
            return Vec::new();
        };
        let queue = queue_state.agent_message_queue.get();
        queue
            .get(&active.agent_id)
            .map(|entries| entries.iter().map(|e| e.id.clone()).collect())
            .unwrap_or_default()
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

            <div class="chat-input-row">
                <textarea
                    class="chat-textarea"
                    placeholder="Type a message or drop images..."
                    prop:value=move || state.chat_input.get()
                    prop:disabled=move || is_readonly.get()
                    on:input=on_input
                    on:keydown=on_keydown
                    rows="1"
                    node_ref=textarea_ref
                />
                <button
                    class="chat-send-btn chat-send-btn-text"
                    disabled=move || !can_send()
                    on:click=on_click_send
                    title=move || {
                        if is_steer.get() {
                            "Queue message"
                        } else {
                            "Send message (Enter)"
                        }
                    }
                >
                    <span>{send_label}</span>
                </button>
                <button
                    class="chat-send-btn chat-interrupt-btn chat-send-btn-text"
                    disabled=move || !can_interrupt()
                    on:click=on_click_interrupt
                    title=interrupt_title
                >
                    <span>{interrupt_label}</span>
                </button>
            </div>
            <SessionSettingsBar />
        </div>
    }
}
