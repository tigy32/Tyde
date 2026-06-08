use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::components::ui::{Button, ButtonVariant};
use crate::state::{AppMode, AppState, PairingScreen};

/// Top-level dispatcher for the pairing experience. The current step lives in
/// `state.app_mode` as `AppMode::Pairing(PairingScreen)`. Each sub-screen
/// transitions by mutating that signal.
#[component]
pub fn PairingFlow(screen: PairingScreen) -> impl IntoView {
    match screen {
        PairingScreen::Scanner => view! { <ScannerScreen /> }.into_any(),
        PairingScreen::ManualPaste => view! { <ManualPasteScreen /> }.into_any(),
        PairingScreen::Confirm { qr_uri, preview } => view! {
            <ConfirmScreen qr_uri=qr_uri preview=preview />
        }
        .into_any(),
        PairingScreen::InProgress { qr_uri, preview } => view! {
            <InProgressScreen qr_uri=qr_uri preview=preview />
        }
        .into_any(),
        PairingScreen::Failed { message } => view! {
            <FailedScreen message=message />
        }
        .into_any(),
    }
}

/// Live scanner. Calls the Tauri barcode-scanner plugin via `bridge::scan_qr`;
/// on iOS the plugin opens the native camera viewfinder and returns the scanned
/// QR contents.
#[component]
fn ScannerScreen() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let state_for_paste = state.clone();
    let state_for_cancel = state.clone();
    let scan_nonce = RwSignal::new(0_u64);
    let error = RwSignal::new(None::<String>);

    {
        let state = state.clone();
        Effect::new(move |_| {
            let _nonce = scan_nonce.get();
            error.set(None);
            let state = state.clone();
            spawn_local(async move {
                if let Err(e) = bridge::ensure_camera_permission().await {
                    error.set(Some(e));
                    return;
                }

                match bridge::scan_qr().await {
                    Ok(result) => {
                        let trimmed = result.content.trim().to_owned();
                        match bridge::preview_pairing_uri(&trimmed).await {
                            Ok(preview) => {
                                state.app_mode.set(AppMode::Pairing(PairingScreen::Confirm {
                                    qr_uri: trimmed,
                                    preview,
                                }));
                            }
                            Err(e) => {
                                error.set(Some(format!(
                                    "Scanned code isn't a Tyde pairing QR: {e}"
                                )));
                            }
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        });
    }

    let on_rescan = Callback::new(move |_: ()| {
        error.set(None);
        scan_nonce.update(|nonce| *nonce += 1);
    });
    let on_paste = Callback::new(move |_: ()| {
        state_for_paste
            .app_mode
            .set(AppMode::Pairing(PairingScreen::ManualPaste))
    });

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Scan QR"</h1>
                <button
                    class="header-action"
                    data-mobile-test="pairing-scanner-cancel"
                    on:click=move |_| state_for_cancel.app_mode.set(AppMode::Workspace)
                >"Cancel"</button>
            </div>
            <div class="view-body">
                <p class="pairing-instruction" data-mobile-test="pairing-scanner-instruction">
                    "Point your phone at the QR code shown in Tyde on your computer (Settings → Hosts)."
                </p>
                <div class="pairing-scanner-frame" data-mobile-test="pairing-scanner-frame">
                    <div class="pairing-scanner-reticle" aria-hidden="true"></div>
                    {move || {
                        if let Some(err) = error.get() {
                            view! {
                                <div class="pairing-scanner-overlay" role="alert" data-mobile-test="pairing-scanner-error">
                                    <p class="pairing-error">{err}</p>
                                    <Button
                                        label="Try again"
                                        variant=ButtonVariant::Primary
                                        full_width=true
                                        data_mobile_test="pairing-scanner-rescan"
                                        on_click=on_rescan
                                    />
                                </div>
                            }.into_any()
                        } else {
                            view! {
                                <span class="pairing-scanner-placeholder">"Opening camera…"</span>
                            }.into_any()
                        }
                    }}
                </div>
                <Button
                    label="Paste pairing URI instead"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    data_mobile_test="pairing-scanner-paste"
                    on_click=on_paste
                />
            </div>
        </div>
    }
}

/// Simulator/dev fallback: paste a `tyde-pair://v1?...` URI in plain text.
#[component]
fn ManualPasteScreen() -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let pasted = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let pending = RwSignal::new(false);

    let state_for_cancel = state.clone();
    let state_for_continue = state.clone();
    let on_continue = move |_| {
        let raw = pasted.get_untracked();
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("Paste a tyde-pair:// URI to continue.".to_string()));
            return;
        }
        pending.set(true);
        error.set(None);
        let state = state_for_continue.clone();
        spawn_local(async move {
            match bridge::preview_pairing_uri(&trimmed).await {
                Ok(preview) => {
                    state.app_mode.set(AppMode::Pairing(PairingScreen::Confirm {
                        qr_uri: trimmed,
                        preview,
                    }));
                }
                Err(e) => error.set(Some(format!("Invalid pairing URI: {e}"))),
            }
            pending.set(false);
        });
    };

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Paste pairing URI"</h1>
                <button
                    class="header-action"
                    on:click=move |_| state_for_cancel.app_mode.set(AppMode::Workspace)
                >"Cancel"</button>
            </div>
            <div class="view-body">
                <textarea
                    class="pairing-paste-input"
                    rows=4
                    placeholder="tyde-pair://v1?..."
                    prop:value=move || pasted.get()
                    on:input=move |ev| pasted.set(event_target_value(&ev))
                />
                {move || error.get().map(|msg| view! {
                    <p class="pairing-error">{msg}</p>
                })}
                <button
                    type="button"
                    class="ui-button ui-button-primary ui-button-full"
                    disabled=move || pending.get()
                    on:click=on_continue
                >
                    <span class="ui-button-label">
                        {move || if pending.get() { "Checking…" } else { "Continue" }}
                    </span>
                </button>
            </div>
        </div>
    }
}

/// "Pair with: <host_label>?" confirmation. The QR already contains the MQTT
/// room and PSK credential; tapping Pair stores the PSK in Keychain and starts
/// the encrypted MQTT connection.
#[component]
fn ConfirmScreen(qr_uri: String, preview: crate::state::MobilePairingPreview) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let host_label = preview.host_label.clone();

    let state_for_cancel = state.clone();
    let state_for_pair = state.clone();
    let qr_uri_for_pair = qr_uri.clone();
    let preview_for_pair = preview.clone();

    let on_pair = Callback::new(move |_: ()| {
        state_for_pair
            .app_mode
            .set(AppMode::Pairing(PairingScreen::InProgress {
                qr_uri: qr_uri_for_pair.clone(),
                preview: preview_for_pair.clone(),
            }));
    });

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">{format!("Pair with {host_label}?")}</h1>
                <button
                    class="header-action"
                    on:click=move |_| state_for_cancel.app_mode.set(AppMode::Pairing(PairingScreen::Scanner))
                >"Back"</button>
            </div>
            <div class="view-body">
                <p class="pairing-instruction">
                    {format!("Pairing stores an encrypted MQTT credential for \"{host_label}\" in this device's Keychain.")}
                </p>
                <Button
                    label="Pair"
                    variant=ButtonVariant::Primary
                    full_width=true
                    on_click=on_pair
                />
            </div>
        </div>
    }
}

#[component]
fn InProgressScreen(qr_uri: String, preview: crate::state::MobilePairingPreview) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let started = RwSignal::new(false);
    let host_label = preview.host_label.clone();

    // Kick off the actual pairing on first render.
    {
        let state = state.clone();
        let qr_uri = qr_uri.clone();
        Effect::new(move |_| {
            if started.get_untracked() {
                return;
            }
            started.set(true);
            let state = state.clone();
            let qr_uri = qr_uri.clone();
            spawn_local(async move {
                match bridge::start_pairing(&qr_uri).await {
                    Ok(()) => {
                        // The `tyde://paired-hosts-changed` event is the
                        // source of truth for the paired list. Returning to the
                        // workspace lets the picker render that event.
                        state.app_mode.set(AppMode::Workspace);
                    }
                    Err(error) => {
                        state
                            .app_mode
                            .set(AppMode::Pairing(PairingScreen::Failed { message: error }));
                    }
                }
            });
        });
    }

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Pairing…"</h1>
            </div>
            <div class="view-body">
                <div class="pairing-progress">
                    <span class="pairing-spinner">"…"</span>
                    <p class="pairing-instruction">
                        {format!("Connecting to {host_label} over encrypted MQTT.")}
                    </p>
                </div>
            </div>
        </div>
    }
}

#[component]
fn FailedScreen(message: String) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let state_for_retry = state.clone();
    let state_for_back = state.clone();
    let on_retry = Callback::new(move |_: ()| {
        state_for_retry
            .app_mode
            .set(AppMode::Pairing(PairingScreen::Scanner))
    });
    let on_cancel = Callback::new(move |_: ()| state_for_back.app_mode.set(AppMode::Workspace));
    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Pairing failed"</h1>
            </div>
            <div class="view-body">
                <p class="pairing-error">{message}</p>
                <Button
                    label="Try again"
                    variant=ButtonVariant::Primary
                    full_width=true
                    on_click=on_retry
                />
                <Button
                    label="Cancel"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    on_click=on_cancel
                />
            </div>
        </div>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use mobile_shell_types::MobilePairingPreview;
    use protocol::BrokerUrl;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

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

    fn fixture_preview(host_label: &str) -> MobilePairingPreview {
        MobilePairingPreview {
            host_label: host_label.to_owned(),
            broker_url: BrokerUrl::new("mqtts://broker.emqx.io:8883").unwrap(),
        }
    }

    #[wasm_bindgen_test]
    async fn confirm_screen_shows_host_label() {
        let preview = fixture_preview("Living Room");
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <PairingFlow screen=PairingScreen::Confirm {
                    qr_uri: "tyde-pair://v1?test".to_owned(),
                    preview: preview.clone(),
                } />
            }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Pair with Living Room"),
            "expected confirmation prompt: {text}"
        );
        assert!(text.contains("Pair"), "expected Pair button: {text}");
    }
}
