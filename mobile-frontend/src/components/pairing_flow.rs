use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::bridge;
use crate::bridge::{AuthProvider, RedeemOutcome};
use crate::components::ui::{Button, ButtonVariant};
use crate::state::{AppMode, AppState, MobileServiceAuthState, PairingOffer, PairingScreen};

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
        PairingScreen::ServiceAuth {
            qr_uri,
            host_label,
            auth,
        } => view! {
            <ServiceAuthScreen qr_uri=qr_uri host_label=host_label initial_auth=auth />
        }
        .into_any(),
        PairingScreen::ServiceAuthStatus { auth } => view! {
            <ServiceAuthStatusScreen initial_auth=auth />
        }
        .into_any(),
        PairingScreen::RepairRequired { message } => view! {
            <RepairRequiredScreen message=message />
        }
        .into_any(),
        PairingScreen::Failed { message } => view! {
            <FailedScreen message=message />
        }
        .into_any(),
    }
}

/// Routes a classified [`PairingOffer`] onto the right pairing screen. Shared by
/// the scanner and the manual-paste path so both branch identically.
fn route_offer(state: &AppState, qr_uri: String, offer: PairingOffer) {
    let screen = match offer {
        PairingOffer::ManagedService { host_label } => PairingScreen::ServiceAuth {
            qr_uri,
            host_label,
            auth: MobileServiceAuthState::Idle,
        },
        PairingOffer::RepairRequired { message } => PairingScreen::RepairRequired { message },
        PairingOffer::DirectPairing { preview } => PairingScreen::Confirm { qr_uri, preview },
    };
    state.app_mode.set(AppMode::Pairing(screen));
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
                        match bridge::classify_pairing_offer(&trimmed).await {
                            Ok(offer) => route_offer(&state, trimmed, offer),
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
            match bridge::classify_pairing_offer(&trimmed).await {
                Ok(offer) => route_offer(&state, trimmed, offer),
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
                    placeholder="tyde-pair://..."
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

/// Drives the managed (`tyde-pair://v2`) auth + redeem sequence against
/// `tycode.dev`. The pre-transport Tyggs sign-in runs first; only once
/// `tycode.dev` confirms a valid Tyggs Pass does the offer redeem + managed
/// broker connect proceed. Every terminal state renders an explicit card — a
/// paywall link, a retry, or a re-pair prompt — so the user is never left on an
/// indefinite spinner.
#[component]
fn ServiceAuthScreen(
    qr_uri: String,
    host_label: String,
    initial_auth: MobileServiceAuthState,
) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let auth = RwSignal::new(initial_auth);
    // True while an auth or redeem call is in flight, so the card shows a bounded
    // "working" spinner and retry buttons stay disabled.
    let busy = RwSignal::new(false);
    let started = RwSignal::new(false);

    let qr_for_run = qr_uri.clone();
    let state_for_run = state.clone();
    // Runs sign-in, then (on success) redeem + connect. Callable from the initial
    // effect and from every retry button.
    let run = Callback::new(move |_: ()| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let qr_uri = qr_for_run.clone();
        let state = state_for_run.clone();
        spawn_local(async move {
            let authenticated = match auth.get_untracked() {
                authenticated @ MobileServiceAuthState::Authenticated { .. } => authenticated,
                _ => {
                    let authenticated = bridge::authenticate_managed(&qr_uri).await;
                    auth.set(authenticated.clone());
                    authenticated
                }
            };
            if !matches!(authenticated, MobileServiceAuthState::Authenticated { .. }) {
                busy.set(false);
                return;
            }
            match bridge::redeem_managed_and_connect(&qr_uri).await {
                Ok(()) => {
                    // The paired-hosts-changed event drives the picker; drop back
                    // to the workspace so it renders the freshly paired host.
                    state.app_mode.set(AppMode::Workspace);
                }
                Err(RedeemOutcome::Auth(next)) => {
                    auth.set(next);
                    busy.set(false);
                }
                Err(RedeemOutcome::Repair { message }) => {
                    state
                        .app_mode
                        .set(AppMode::Pairing(PairingScreen::RepairRequired { message }));
                }
                Err(RedeemOutcome::Terminal { message }) => {
                    state
                        .app_mode
                        .set(AppMode::Pairing(PairingScreen::Failed { message }));
                }
            }
        });
    });

    // Kick off sign-in on first render.
    Effect::new(move |_| {
        if started.get_untracked() {
            return;
        }
        started.set(true);
        if matches!(
            auth.get_untracked(),
            MobileServiceAuthState::Idle
                | MobileServiceAuthState::Authenticating
                | MobileServiceAuthState::Authenticated { .. }
        ) {
            run.run(());
        }
    });

    // Not-signed-in state: navigate to the tycode.dev-hosted Tyggs OAuth start,
    // stashing the pairing URI so the flow resumes when the redirect returns.
    let qr_for_sign_in = qr_uri.clone();
    let on_sign_in = Callback::new(move |provider: AuthProvider| {
        if let Err(error) = bridge::begin_tyggs_sign_in(provider, Some(&qr_for_sign_in)) {
            log::error!("failed to start Tyggs sign-in: {error}");
        }
    });

    let state_for_cancel = state.clone();
    let host_label_for_title = host_label.clone();

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">{format!("Connect {host_label_for_title}")}</h1>
                <button
                    class="header-action"
                    data-mobile-test="pairing-service-cancel"
                    on:click=move |_| state_for_cancel.app_mode.set(AppMode::Workspace)
                >"Cancel"</button>
            </div>
            <div class="view-body">
                {move || view! {
                    <ServiceAuthCard
                        auth=auth.get()
                        busy=busy.get()
                        on_retry=run
                        on_sign_in=on_sign_in
                    />
                }}
            </div>
        </div>
    }
}

#[component]
fn ServiceAuthStatusScreen(initial_auth: MobileServiceAuthState) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let auth = RwSignal::new(initial_auth);
    let busy = RwSignal::new(false);

    let state_for_retry = state.clone();
    let on_retry = Callback::new(move |_: ()| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let state = state_for_retry.clone();
        spawn_local(async move {
            let next = bridge::probe_managed_auth().await;
            if matches!(next, MobileServiceAuthState::Authenticated { .. }) {
                state.app_mode.set(AppMode::Workspace);
            } else {
                auth.set(next);
                busy.set(false);
            }
        });
    });

    let on_sign_in = Callback::new(move |provider: AuthProvider| {
        if let Err(error) = bridge::begin_tyggs_sign_in(provider, None) {
            log::error!("failed to restart Tyggs sign-in: {error}");
        }
    });
    let state_for_cancel = state.clone();

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Tyggs mobile access"</h1>
                <button
                    class="header-action"
                    data-mobile-test="boot-service-auth-cancel"
                    on:click=move |_| state_for_cancel.app_mode.set(AppMode::Workspace)
                >"Cancel"</button>
            </div>
            <div class="view-body">
                {move || view! {
                    <ServiceAuthCard
                        auth=auth.get()
                        busy=busy.get()
                        on_retry=on_retry
                        on_sign_in=on_sign_in
                    />
                }}
            </div>
        </div>
    }
}

/// Pure projection of a [`MobileServiceAuthState`] onto the managed-auth card.
/// Split out from [`ServiceAuthScreen`] so the render for each state — spinner,
/// paywall, sign-in, retry — is deterministic and testable without the async
/// authenticate/redeem orchestration. `on_retry` re-runs the auth sequence;
/// `on_sign_in` starts the Tyggs OAuth redirect for the signed-out state.
#[component]
fn ServiceAuthCard(
    auth: MobileServiceAuthState,
    busy: bool,
    on_retry: Callback<()>,
    on_sign_in: Callback<AuthProvider>,
) -> impl IntoView {
    match auth {
        MobileServiceAuthState::Idle | MobileServiceAuthState::Authenticating => view! {
            <ServiceWorking message="Signing in with Tyggs…".to_owned() />
        }
        .into_any(),
        MobileServiceAuthState::Authenticated { .. } => view! {
            <ServiceWorking message="Setting up secure access…".to_owned() />
        }
        .into_any(),
        MobileServiceAuthState::PassRequired {
            message,
            paywall_url,
        } => view! {
            <div class="pairing-card pairing-paywall" data-mobile-test="pairing-paywall">
                <h2 class="pairing-card-title">"Tyggs Pass required"</h2>
                <p class="pairing-card-body">{message}</p>
                <a
                    class="ui-button ui-button-primary ui-button-full"
                    href=paywall_url
                    target="_blank"
                    rel="noopener noreferrer"
                    data-mobile-test="pairing-paywall-link"
                >
                    <span class="ui-button-label">"Get a Tyggs Pass"</span>
                </a>
                <Button
                    label="I have a Pass — try again"
                    variant=ButtonVariant::Secondary
                    full_width=true
                    disabled=busy
                    data_mobile_test="pairing-paywall-retry"
                    on_click=on_retry
                />
            </div>
        }
        .into_any(),
        MobileServiceAuthState::AuthFailed { message } => view! {
            <div class="pairing-card" data-mobile-test="pairing-auth-failed">
                <h2 class="pairing-card-title">"Sign in with Tyggs"</h2>
                <p class="pairing-card-body">{message}</p>
                {auth_provider_buttons(
                    "pairing-auth-sign-in",
                    busy,
                    on_sign_in,
                    pairing_auth_provider_test_id,
                )}
            </div>
        }
        .into_any(),
        MobileServiceAuthState::ServiceUnavailable { message, retryable } => view! {
            <div class="pairing-card" data-mobile-test="pairing-service-unavailable">
                <h2 class="pairing-card-title">"Service unavailable"</h2>
                <p class="pairing-card-body">{message}</p>
                {retryable.then(|| view! {
                    <Button
                        label="Try again"
                        variant=ButtonVariant::Primary
                        full_width=true
                        disabled=busy
                        data_mobile_test="pairing-service-retry"
                        on_click=on_retry
                    />
                })}
            </div>
        }
        .into_any(),
    }
}

fn auth_provider_buttons(
    legacy_test_id: &'static str,
    busy: bool,
    on_sign_in: Callback<AuthProvider>,
    provider_test_id: fn(AuthProvider) -> &'static str,
) -> AnyView {
    match bridge::tyggs_auth_providers() {
        Ok(providers) => view! {
            <div class="auth-provider-actions">
                {providers
                    .into_iter()
                    .enumerate()
                    .map(|(index, provider)| {
                        let test_id = if index == 0 {
                            legacy_test_id
                        } else {
                            provider_test_id(provider)
                        };
                        view! {
                            <button
                                type="button"
                                class="ui-button ui-button-primary ui-button-full"
                                data-mobile-test=test_id
                                data-mobile-auth-provider=provider.as_str()
                                disabled=busy
                                aria-disabled=if busy { "true" } else { "false" }
                                aria-label=provider.sign_in_label()
                                on:click=move |_| {
                                    if !busy {
                                        on_sign_in.run(provider);
                                    }
                                }
                            >
                                <span class="ui-button-label">{provider.sign_in_label()}</span>
                            </button>
                        }
                    })
                    .collect::<Vec<_>>()}
            </div>
        }
        .into_any(),
        Err(message) => view! {
            <p class="pairing-error" role="alert" data-mobile-test="pairing-auth-provider-error">
                {message}
            </p>
        }
        .into_any(),
    }
}

fn pairing_auth_provider_test_id(provider: AuthProvider) -> &'static str {
    match provider {
        AuthProvider::Apple => "pairing-auth-sign-in-apple",
        AuthProvider::Google => "pairing-auth-sign-in-google",
    }
}

/// Bounded working state shown while a `tycode.dev` call is in flight. Distinct
/// from a stuck spinner: it only renders between a request and its typed reply.
#[component]
fn ServiceWorking(message: String) -> impl IntoView {
    view! {
        <div class="pairing-progress" data-mobile-test="pairing-service-working">
            <span class="pairing-spinner">"…"</span>
            <p class="pairing-instruction">{message}</p>
        </div>
    }
}

/// Terminal screen for a legacy public-broker QR or stored record. Explains why
/// it can't connect and points the user at a fresh re-pair — never a spinner or
/// a silent public-broker connect.
#[component]
fn RepairRequiredScreen(message: String) -> impl IntoView {
    let state = use_context::<AppState>().expect("AppState context");
    let state_for_rescan = state.clone();
    let state_for_cancel = state.clone();
    let on_rescan = Callback::new(move |_: ()| {
        state_for_rescan
            .app_mode
            .set(AppMode::Pairing(PairingScreen::Scanner))
    });
    let on_cancel = Callback::new(move |_: ()| state_for_cancel.app_mode.set(AppMode::Workspace));

    view! {
        <div class="view pairing-view">
            <div class="view-header">
                <h1 class="view-title">"Re-pair required"</h1>
            </div>
            <div class="view-body">
                <div class="pairing-card" data-mobile-test="pairing-repair-required">
                    <p class="pairing-card-body">{message}</p>
                </div>
                <Button
                    label="Scan the new QR code"
                    variant=ButtonVariant::Primary
                    full_width=true
                    data_mobile_test="pairing-repair-rescan"
                    on_click=on_rescan
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

    fn force_web_backend() {
        let window = web_sys::window().expect("window");
        let _ = js_sys::Reflect::delete_property(
            &window,
            &wasm_bindgen::JsValue::from_str("__TAURI__"),
        );
    }

    fn set_service_config(json: &str) {
        force_web_backend();
        let window = web_sys::window().expect("window");
        let key = wasm_bindgen::JsValue::from_str("__TYDE_MOBILE_SERVICE__");
        if json.is_empty() {
            let _ = js_sys::Reflect::delete_property(&window, &key);
            return;
        }
        let value = js_sys::JSON::parse(json).expect("parse service config");
        js_sys::Reflect::set(&window, &key, &value).expect("install service config");
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

    /// A `pass_required` auth state renders the paywall card with a working
    /// purchase link pointing at the `tycode.dev`-provided URL — never a spinner,
    /// never a redeem attempt. Mounts the pure card so the assertion is
    /// deterministic and independent of the async authenticate orchestration.
    #[wasm_bindgen_test]
    async fn service_auth_card_pass_required_renders_paywall_link() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <ServiceAuthCard
                    auth=MobileServiceAuthState::PassRequired {
                        message: "A Tyggs Pass is required.".to_owned(),
                        paywall_url: "https://tyggs.com/go".to_owned(),
                    }
                    busy=false
                    on_retry=Callback::new(|_: ()| {})
                    on_sign_in=Callback::new(|_: AuthProvider| {})
                />
            }
        });
        next_tick().await;

        let link: HtmlElement = container
            .query_selector("[data-mobile-test='pairing-paywall-link']")
            .unwrap()
            .expect("pass_required must render a paywall link")
            .dyn_into()
            .unwrap();
        assert_eq!(
            link.get_attribute("href").as_deref(),
            Some("https://tyggs.com/go"),
            "paywall link must point at the tycode.dev-provided paywall URL",
        );
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Tyggs Pass required"),
            "paywall card must be explicit: {text}"
        );
    }

    /// Signed-out managed auth renders one button per configured provider in
    /// config order, so Apple/Google account handoff stays an explicit choice.
    #[wasm_bindgen_test]
    async fn service_auth_card_auth_failed_renders_configured_provider_buttons() {
        set_service_config(r#"{"providers":["apple","google"]}"#);
        let container = make_container();
        let clicked: std::sync::Arc<std::sync::Mutex<Vec<AuthProvider>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let clicked_for_mount = clicked.clone();
        let _handle = mount_to(container.clone(), move || {
            let clicked = clicked_for_mount.clone();
            view! {
                <ServiceAuthCard
                    auth=MobileServiceAuthState::AuthFailed {
                        message: "Sign in to continue.".to_owned(),
                    }
                    busy=false
                    on_retry=Callback::new(|_: ()| {})
                    on_sign_in=Callback::new(move |provider: AuthProvider| {
                        clicked.lock().unwrap().push(provider);
                    })
                />
            }
        });
        next_tick().await;

        let legacy: HtmlElement = container
            .query_selector("[data-mobile-test='pairing-auth-sign-in']")
            .unwrap()
            .expect("legacy selector must point at the first provider button")
            .dyn_into()
            .unwrap();
        assert_eq!(
            legacy.tag_name(),
            "BUTTON",
            "legacy selector must remain a clickable button target"
        );
        assert_eq!(
            legacy.get_attribute("data-mobile-auth-provider").as_deref(),
            Some("apple"),
            "first configured provider should keep the legacy click target"
        );
        let apple_button: HtmlElement = container
            .query_selector("[data-mobile-auth-provider='apple']")
            .unwrap()
            .expect("Apple sign-in button must render")
            .dyn_into()
            .unwrap();
        let google_button: HtmlElement = container
            .query_selector("[data-mobile-auth-provider='google']")
            .unwrap()
            .expect("Google sign-in button must render")
            .dyn_into()
            .unwrap();
        let text = container.text_content().unwrap_or_default();
        let apple_label = text.find("Continue with Apple").expect("Apple label");
        let google_label = text.find("Continue with Google").expect("Google label");
        assert!(
            apple_label < google_label,
            "buttons must preserve provider config order: {text}"
        );
        assert_eq!(
            google_button.get_attribute("data-mobile-test").as_deref(),
            Some("pairing-auth-sign-in-google"),
            "non-default provider keeps a provider-specific test selector"
        );
        apple_button.click();
        google_button.click();
        assert_eq!(
            clicked.lock().unwrap().as_slice(),
            &[AuthProvider::Apple, AuthProvider::Google],
            "provider buttons must invoke the selected provider callback"
        );

        set_service_config("");
    }

    /// A retryable `service_unavailable` renders a retry affordance; a
    /// non-retryable one does not — the user is never stuck on a spinner nor
    /// offered a futile retry.
    #[wasm_bindgen_test]
    async fn service_auth_card_service_unavailable_retry_tracks_retryable() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <div>
                    <ServiceAuthCard
                        auth=MobileServiceAuthState::ServiceUnavailable {
                            message: "Temporarily down.".to_owned(),
                            retryable: true,
                        }
                        busy=false
                        on_retry=Callback::new(|_: ()| {})
                        on_sign_in=Callback::new(|_: AuthProvider| {})
                    />
                </div>
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='pairing-service-retry']")
                .unwrap()
                .is_some(),
            "retryable service_unavailable must offer a retry button"
        );

        // A non-retryable variant offers no retry.
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            view! {
                <div>
                    <ServiceAuthCard
                        auth=MobileServiceAuthState::ServiceUnavailable {
                            message: "Not configured.".to_owned(),
                            retryable: false,
                        }
                        busy=false
                        on_retry=Callback::new(|_: ()| {})
                        on_sign_in=Callback::new(|_: AuthProvider| {})
                    />
                </div>
            }
        });
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='pairing-service-retry']")
                .unwrap()
                .is_none(),
            "a non-retryable service_unavailable must not offer a futile retry"
        );
    }

    #[wasm_bindgen_test]
    async fn boot_auth_status_preserves_paywall_and_retry_actions_without_qr() {
        let paywall = make_container();
        let _paywall_handle = mount_to(paywall.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <PairingFlow screen=PairingScreen::ServiceAuthStatus {
                    auth: MobileServiceAuthState::PassRequired {
                        message: "A Tyggs Pass is required.".to_owned(),
                        paywall_url: "https://tyggs.com/pass/checkout".to_owned(),
                    },
                } />
            }
        });
        next_tick().await;

        let link = paywall
            .query_selector("[data-mobile-test='pairing-paywall-link']")
            .unwrap()
            .expect("boot pass_required must keep its paywall action");
        assert_eq!(
            link.get_attribute("href").as_deref(),
            Some("https://tyggs.com/pass/checkout")
        );

        set_service_config(r#"{"stubAuth":"authenticated"}"#);
        let unavailable = make_container();
        let state_handle: std::rc::Rc<std::cell::RefCell<Option<AppState>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let state_for_mount = state_handle.clone();
        let _unavailable_handle = mount_to(unavailable.clone(), move || {
            let state = AppState::new();
            *state_for_mount.borrow_mut() = Some(state.clone());
            provide_context(state);
            view! {
                <PairingFlow screen=PairingScreen::ServiceAuthStatus {
                    auth: MobileServiceAuthState::ServiceUnavailable {
                        message: "Try again shortly.".to_owned(),
                        retryable: true,
                    },
                } />
            }
        });
        next_tick().await;

        assert!(
            unavailable
                .query_selector("[data-mobile-test='pairing-service-retry']")
                .unwrap()
                .is_some(),
            "retryable boot service failure must keep its retry action"
        );
        let retry: HtmlElement = unavailable
            .query_selector("[data-mobile-test='pairing-service-retry']")
            .unwrap()
            .expect("retry action")
            .dyn_into()
            .unwrap();
        retry.click();
        next_tick().await;
        next_tick().await;
        assert_eq!(
            state_handle
                .borrow()
                .as_ref()
                .expect("captured state")
                .app_mode
                .get_untracked(),
            AppMode::Workspace,
            "successful no-QR retry should leave auth status without redeeming"
        );
        set_service_config("");
    }

    /// The repair-required screen explains the failure and offers a re-scan —
    /// never a spinner or a silent legacy connect.
    #[wasm_bindgen_test]
    async fn repair_required_screen_offers_rescan() {
        let container = make_container();
        let _handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            provide_context(state);
            view! {
                <PairingFlow screen=PairingScreen::RepairRequired {
                    message: "Re-pair from the host's current QR code.".to_owned(),
                } />
            }
        });
        next_tick().await;

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("Re-pair from the host's current QR code."),
            "repair screen must render the actionable message: {text}"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='pairing-repair-rescan']")
                .unwrap()
                .is_some(),
            "repair screen must offer a re-scan affordance"
        );
    }
}
