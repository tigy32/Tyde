use host_config::updates::{AppUpdateStatus, UpdateChannel, UpdateDismissal, UpdatePhase};
use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{bridge, state::AppState};

fn apply_status(state: &AppState, status: AppUpdateStatus) {
    state.app_update_status.try_update(|current| {
        if current
            .as_ref()
            .is_none_or(|current| status.revision > current.revision)
        {
            *current = Some(status);
        }
    });
}

fn apply(state: &AppState, result: Result<AppUpdateStatus, String>) {
    match result {
        Ok(status) => {
            apply_status(state, status);
            state.app_update_error.try_set(None);
        }
        Err(error) => {
            state.app_update_error.try_set(Some(error));
        }
    }
}

fn command(state: AppState, name: &'static str, args: serde_json::Value) {
    if state.app_update_request.get_untracked() {
        return;
    }
    state.app_update_request.set(true);
    state.app_update_error.set(None);
    spawn_local(async move {
        apply(&state, bridge::app_update_command(name, args).await);
        state.app_update_request.try_set(false);
    });
}

pub fn observe_server_version(state: &AppState, version: Option<&host_config::TydeReleaseVersion>) {
    let Some(version) = version else { return };
    let version = version.as_str().to_owned();
    let state = state.clone();
    spawn_local(async move {
        // A host supplies only a version hint; the native updater selects and
        // verifies packages from Tyde's release repository independently.
        match bridge::app_update_command(
            "plugin:app-updates|server_version",
            serde_json::json!({"version": version}),
        )
        .await
        {
            Ok(status) => {
                apply_status(&state, status);
            }
            Err(error) => log::debug!("server update hint could not be checked: {error}"),
        }
    });
}

#[component]
pub fn AppUpdateRuntime() -> impl IntoView {
    let state = expect_context::<AppState>();
    let listener = StoredValue::new_local(None::<bridge::UnlistenHandle>);
    let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let active_cleanup = active.clone();
    on_cleanup(move || {
        active_cleanup.store(false, std::sync::atomic::Ordering::Relaxed);
        listener.update_value(|handle| {
            if let Some(handle) = handle.take() {
                handle.remove();
            }
        });
    });
    spawn_local(async move {
        let event_state = state.clone();
        match bridge::listen_app_update(move |status| {
            apply_status(&event_state, status);
        })
        .await
        {
            Ok(handle) if active.load(std::sync::atomic::Ordering::Relaxed) => {
                listener.set_value(Some(handle))
            }
            Ok(handle) => {
                handle.remove();
                return;
            }
            Err(error) => log::debug!("app updates unavailable: {error}"),
        }
        if active.load(std::sync::atomic::Ordering::Relaxed) {
            let result =
                bridge::app_update_command("plugin:app-updates|status", serde_json::json!({}))
                    .await;
            if active.load(std::sync::atomic::Ordering::Relaxed) {
                apply(&state, result);
            }
        }
    });
}

fn busy(state: &AppState) -> bool {
    state.app_update_request.get()
        || state
            .app_update_status
            .with(|value| value.as_ref().is_some_and(|status| status.phase.busy()))
}

fn status_text(status: &AppUpdateStatus) -> String {
    match status.phase {
        UpdatePhase::Idle if status.last_checked.is_some() => {
            "You’re up to date on this channel.".into()
        }
        UpdatePhase::Idle => "Ready to check for updates.".into(),
        UpdatePhase::Checking => "Checking for updates…".into(),
        UpdatePhase::Available => format!(
            "Tyde {} is available.",
            status.version.as_deref().unwrap_or("update")
        ),
        UpdatePhase::Downloading => match status.total.filter(|total| *total > 0) {
            Some(total) => format!(
                "Downloading update… {}%",
                status
                    .downloaded
                    .saturating_mul(100)
                    .checked_div(total)
                    .unwrap_or(0)
                    .min(100)
            ),
            None => format!("Downloading update… {} MB", status.downloaded / 1_000_000),
        },
        UpdatePhase::Installing => "Installing update and restarting Tyde…".into(),
        UpdatePhase::Error => "The update could not be completed. You can try again.".into(),
    }
}

#[component]
pub fn UpdatesSettings() -> impl IntoView {
    let state = expect_context::<AppState>();
    let status = state.app_update_status;
    let error = state.app_update_error;
    let disabled_state = state.clone();
    let disabled = Signal::derive(move || busy(&disabled_state));
    let channel_state = state.clone();
    let automatic_state = state.clone();
    let check_state = state.clone();
    let install_state = state.clone();
    view! {
        <h2 class="settings-panel-title">"Updates"</h2>
        {move || status.get().map(|status| format!("Installed version: {}", status.current_version))}
        <p>"Release includes stable versions. Preview also includes beta and other preview versions. Switching channels never installs an older version."</p>
        <div class="settings-field">
            <label for="app-update-channel">"Update channel"</label>
            <select id="app-update-channel" disabled=move || disabled.get() || status.get().is_none()
                prop:value=move || status.with(|value| match value.as_ref().map(|value| value.preferences.channel) {
                    Some(UpdateChannel::Preview) => "preview", _ => "release",
                })
                on:change=move |event| {
                    let channel = if event_target_value(&event) == "preview" { UpdateChannel::Preview } else { UpdateChannel::Release };
                    let automatic = status.with_untracked(|value| value.as_ref().is_none_or(|value| value.preferences.automatic));
                    command(channel_state.clone(), "plugin:app-updates|configure", serde_json::json!({"channel": channel, "automatic": automatic}));
                }>
                <option value="release">"Release"</option>
                <option value="preview">"Preview"</option>
            </select>
        </div>
        <div class="settings-field">
            <label>
                <input type="checkbox" disabled=move || disabled.get() || status.get().is_none()
                    prop:checked=move || status.with(|value| value.as_ref().is_some_and(|value| value.preferences.automatic))
                    on:change=move |event| {
                        let automatic = event_target_checked(&event);
                        let channel = status.with_untracked(|value| value.as_ref().map(|value| value.preferences.channel).unwrap_or(UpdateChannel::Release));
                        command(automatic_state.clone(), "plugin:app-updates|configure", serde_json::json!({"channel": channel, "automatic": automatic}));
                    }/>
                " Automatically check for updates"
            </label>
            <p>"Checks at startup, every six hours, and when a server reports a newer version. Downloads only start after you choose Yes."</p>
        </div>
        <p role="status">{move || status.with(|value| value.as_ref().map(status_text).unwrap_or_else(|| "In-place updates are available in the desktop app.".into()))}</p>
        <p role="alert">{move || error.get().or_else(|| status.with(|value| value.as_ref().and_then(|value| value.error.clone())))}</p>
        <button class="settings-btn" disabled=move || disabled.get() || status.get().is_none()
            on:click=move |_| command(check_state.clone(), "plugin:app-updates|check_now", serde_json::json!({}))>"Check for updates"</button>
        <Show when=move || status.with(|value| value.as_ref().is_some_and(|value| value.version.is_some()))>
            <button class="settings-btn" disabled=move || disabled.get()
                on:click={let install_state = install_state.clone(); move |_| {
                    install_state.app_update_status.update(|value| { if let Some(value) = value { value.prompt = true; } });
                }}>"Review update"</button>
        </Show>
    }
}

#[component]
pub fn UpdatePrompt() -> impl IntoView {
    let state = expect_context::<AppState>();
    let status = state.app_update_status;
    let error = state.app_update_error;
    let disabled_state = state.clone();
    let disabled = Signal::derive(move || busy(&disabled_state));
    let never_state = state.clone();
    let later_state = state.clone();
    let install_state = state.clone();
    view! {
        <Show when=move || status.with(|value| value.as_ref().is_some_and(|value| value.prompt))>
            <aside class="app-update-prompt" aria-label="App update">
                <h2>"Update Tyde?"</h2>
                <p role="status">{move || status.with(|value| value.as_ref().map(status_text))}</p>
                <p>"Tyde will download the update, install it, and restart. Local running agents will stop; saved conversations remain in History. Remote servers keep running."</p>
                <details><summary>"Release notes"</summary><pre>{move || status.with(|value| value.as_ref().and_then(|value| value.notes.clone()).unwrap_or_else(|| "No release notes provided.".into()))}</pre></details>
                <p role="alert">{move || error.get().or_else(|| status.with(|value| value.as_ref().and_then(|value| value.error.clone())))}</p>
                <div class="app-update-actions">
                    <button disabled=move || disabled.get() title="Turn off automatic update checks; you can enable them again in Settings → Updates"
                        on:click={let state = never_state.clone(); move |_| command(state.clone(), "plugin:app-updates|dismiss", serde_json::json!({"choice": UpdateDismissal::Never}))}>"Never"</button>
                    <button disabled=move || disabled.get() title="Remind me in 24 hours"
                        on:click={let state = later_state.clone(); move |_| command(state.clone(), "plugin:app-updates|dismiss", serde_json::json!({"choice": UpdateDismissal::NotNow}))}>"Not now"</button>
                    <button disabled=move || disabled.get()
                        on:click={let state = install_state.clone(); move |_| {
                            if let Some(version) = status.with_untracked(|value| value.as_ref().and_then(|value| value.version.clone())) {
                                command(state.clone(), "plugin:app-updates|install", serde_json::json!({"version": version}));
                            }
                        }}>"Yes"</button>
                </div>
            </aside>
        </Show>
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlElement, HtmlInputElement, HtmlSelectElement};

    wasm_bindgen_test_configure!(run_in_browser);

    struct BridgeGuard(wasm_bindgen::JsValue);

    impl BridgeGuard {
        fn install() -> Self {
            let window = web_sys::window().unwrap();
            let original = js_sys::Reflect::get(&window, &"__TAURI__".into()).unwrap();
            js_sys::eval(r#"
                window.__tydeUpdateTest = {
                    calls: [], listeners: new Set(),
                    status: {
                        revision: 0, current_version: '1.0.0',
                        preferences: {channel: 'release', automatic: true, remind_after: 0},
                        phase: 'idle', version: null, notes: null, downloaded: 0,
                        total: null, prompt: false, last_checked: null, error: null
                    },
                    publish(changes) {
                        Object.assign(this.status, changes);
                        this.status.revision++;
                        for (const callback of this.listeners) callback({payload: structuredClone(this.status)});
                    },
                    available() {
                        this.publish({phase: 'available', version: '1.1.0', prompt: true, notes: 'A faster Tyde.', error: null});
                    }
                };
                window.__TAURI__ = {
                    event: {listen: async (name, callback) => {
                        if (name === 'tyde://app-update') window.__tydeUpdateTest.listeners.add(callback);
                        return () => window.__tydeUpdateTest.listeners.delete(callback);
                    }},
                    core: {invoke: async (command, args) => {
                        const test = window.__tydeUpdateTest;
                        // Tauri 2.11's process-ipc-message-fn.js converts Maps
                        // to objects before Rust receives command arguments.
                        args = JSON.parse(JSON.stringify(args, (key, value) =>
                            value instanceof Map ? Object.fromEntries(value.entries()) : value));
                        test.calls.push({command, args});
                        switch (command) {
                            case 'plugin:app-updates|status': break;
                            case 'plugin:app-updates|check_now': test.available(); break;
                            case 'plugin:app-updates|server_version':
                                if (test.status.preferences.automatic) test.available();
                                break;
                            case 'plugin:app-updates|configure':
                                test.status.preferences = {...args, remind_after: 0};
                                test.publish({prompt: false, phase: 'idle', version: null});
                                break;
                            case 'plugin:app-updates|dismiss':
                                if (args.choice === 'never') test.status.preferences.automatic = false;
                                else test.status.preferences.remind_after = 86400;
                                test.publish({prompt: false});
                                break;
                            case 'plugin:app-updates|install':
                                test.publish({phase: 'downloading', downloaded: 50, total: 100});
                                await new Promise(resolve => test.finish = resolve);
                                test.publish({phase: 'error', error: 'The update signature is invalid.'});
                                break;
                            default: throw new Error('Unexpected update command: ' + command);
                        }
                        return structuredClone(test.status);
                    }}
                };
            "#).unwrap();
            Self(original)
        }
    }

    impl Drop for BridgeGuard {
        fn drop(&mut self) {
            js_sys::Reflect::set(&web_sys::window().unwrap(), &"__TAURI__".into(), &self.0)
                .unwrap();
            js_sys::eval("delete window.__tydeUpdateTest").unwrap();
        }
    }

    async fn tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
    }

    fn button(container: &HtmlElement, label: &str) -> HtmlElement {
        let buttons = container.query_selector_all("button").unwrap();
        (0..buttons.length())
            .filter_map(|index| buttons.item(index))
            .find(|button| button.text_content().as_deref() == Some(label))
            .unwrap_or_else(|| panic!("Missing visible button {label}"))
            .dyn_into()
            .unwrap()
    }

    fn mount() -> (
        HtmlElement,
        impl Sized,
        std::rc::Rc<std::cell::RefCell<Option<AppState>>>,
    ) {
        let document = web_sys::window().unwrap().document().unwrap();
        let container: HtmlElement = document.create_element("div").unwrap().dyn_into().unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured_mount = captured.clone();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            *captured_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! { <AppUpdateRuntime /><UpdatesSettings /><UpdatePrompt /> }
        });
        (container, handle, captured)
    }

    #[wasm_bindgen_test]
    async fn update_choices_channels_progress_and_failures() {
        let _bridge = BridgeGuard::install();
        let (container, handle, state) = mount();
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Installed version: 1.0.0")
        );
        assert!(container.query_selector("aside").unwrap().is_none());

        let version = host_config::TydeReleaseVersion::parse("1.1.0").unwrap();
        let welcome = protocol::Envelope::from_payload(
            protocol::StreamPath("/host/update-ui-test".into()),
            protocol::FrameKind::Welcome,
            0,
            &protocol::WelcomePayload {
                protocol_version: protocol::PROTOCOL_VERSION,
                tyde_version: protocol::TYDE_VERSION,
                release_version: Some(version),
            },
        )
        .unwrap();
        crate::dispatch::dispatch_envelope(
            state.borrow().as_ref().unwrap(),
            "update-ui-test",
            welcome,
        );
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Tyde 1.1.0 is available")
        );
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Local running agents will stop")
        );
        js_sys::eval("for (const callback of window.__tydeUpdateTest.listeners) callback({payload: {...window.__tydeUpdateTest.status, revision: 0, phase: 'idle', prompt: false}})").unwrap();
        tick().await;
        assert!(
            container.query_selector("aside").unwrap().is_some(),
            "A delayed status response must not hide a newer update notification"
        );
        for label in ["Never", "Not now", "Yes"] {
            button(&container, label);
        }
        button(&container, "Not now").click();
        tick().await;
        assert!(container.query_selector("aside").unwrap().is_none());
        button(&container, "Check for updates").click();
        tick().await;
        button(&container, "Never").click();
        tick().await;
        let automatic: HtmlInputElement = container
            .query_selector("input[type=checkbox]")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(!automatic.checked(), "Never must disable automatic checks");
        assert!(container.query_selector("aside").unwrap().is_none());
        drop(handle);
        container.remove();
        tick().await;
        assert_eq!(
            js_sys::eval("window.__tydeUpdateTest.listeners.size")
                .unwrap()
                .as_f64(),
            Some(0.0)
        );

        let (container, handle, _) = mount();
        tick().await;
        let automatic: HtmlInputElement = container
            .query_selector("input[type=checkbox]")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        assert!(
            !automatic.checked(),
            "Never survives remounting the interface"
        );
        let channel: HtmlSelectElement = container
            .query_selector("select")
            .unwrap()
            .unwrap()
            .dyn_into()
            .unwrap();
        channel.set_value("preview");
        channel
            .dispatch_event(&web_sys::Event::new("change").unwrap())
            .unwrap();
        tick().await;
        assert_eq!(channel.value(), "preview");
        assert!(
            !automatic.checked(),
            "Changing channels must preserve the automatic-check preference"
        );
        button(&container, "Check for updates").click();
        tick().await;
        button(&container, "Yes").click();
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("Downloading update… 50%")
        );
        for label in ["Never", "Not now", "Yes", "Check for updates"] {
            assert!(
                button(&container, label).has_attribute("disabled"),
                "{label} must be disabled during installation"
            );
        }
        js_sys::eval("window.__tydeUpdateTest.finish()").unwrap();
        tick().await;
        assert!(
            container
                .text_content()
                .unwrap()
                .contains("The update signature is invalid.")
        );
        assert!(!button(&container, "Yes").has_attribute("disabled"));
        assert_eq!(
            js_sys::eval(
                "window.__tydeUpdateTest.calls.filter(c => c.command.endsWith('|install')).length"
            )
            .unwrap()
            .as_f64(),
            Some(1.0)
        );
        button(&container, "Not now").click();
        tick().await;
        assert!(container.query_selector("aside").unwrap().is_none());
        drop(handle);
        container.remove();
        tick().await;
    }
}
