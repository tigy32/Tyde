//! The desktop voice surface: a microphone control in the composer and a
//! compact strip above it while a session is live.
//!
//! The strip is deliberately a sibling of the transcript and composer, in the
//! same slot as the in-flight tray. Voice never takes the screen over: the
//! transcript stays scrollable, the textarea stays typable, and every existing
//! send action keeps working while a session runs.
//!
//! Rendering discipline: this file keeps a small number of `view!` sites and
//! erases branches with `.into_any()`. The frontend's wasm test binary runs the
//! whole suite in one browser instance and sits close to its memory ceiling, so
//! per-branch view monomorphisation is a real cost here, not a style question.

use leptos::prelude::*;

use crate::state::{ActiveAgentRef, AppState, TabId};
use crate::voice::{
    self,
    session::{VoicePhase, VoiceUiState},
};

/// Microphone control for one chat's composer, plus the visible reason when it
/// is unavailable.
///
/// Takes the chat's own `agent_ref` rather than reading the global focused
/// agent: in a split, each composer must target its own chat. Before starting,
/// the chat is revealed so it becomes the composer owner — otherwise the focus
/// guard would end the session in the same tick it began.
///
/// The unavailable reason is rendered as **visible text**, not only as `title`
/// and `aria-label`. A disabled button is not keyboard-focusable in normal
/// browser behaviour, so a tooltip is unreachable for keyboard and touch users
/// and the "visible unavailable state" the design calls for would not exist.
#[component]
pub fn VoiceMicButton(
    agent_ref: Signal<Option<ActiveAgentRef>>,
    /// The tab this composer belongs to, so starting voice can focus it.
    tab_id: TabId,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let voice_state = voice::ui_state();

    // Availability is recomputed from authoritative state on every render:
    // agent list, connection status, and the host's own declared availability.
    let readiness = {
        let state = state.clone();
        Memo::new(move |_| {
            state.agents.with(|_| ());
            state.connection_statuses.with(|_| ());
            state.host_settings_by_host.with(|_| ());
            let agent = agent_ref.get();
            match voice::availability(&state, agent.as_ref()) {
                Ok(_) => Ok(()),
                Err(reason) => Err(reason.reason()),
            }
        })
    };

    let is_live = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| {
            let agent = agent_ref.get();
            voice_state.with(|voice| voice.is_engaged() && voice.bound_agent() == agent.as_ref())
        })
    };

    // Only shown when the chat itself is otherwise usable. "Open a chat to use
    // voice" next to every draft composer would be noise, not information.
    let visible_reason = Memo::new(move |_| match readiness.get() {
        Err(reason) if !reason.is_empty() => Some(reason),
        _ => None,
    });

    let on_click = {
        let state = state.clone();
        move |_| {
            if is_live.get_untracked() {
                voice::end(voice::session::VoiceEndReason::UserRequested);
                return;
            }
            // Make this chat the composer owner first. `reveal_tab` moves pane
            // focus as well as activating the tab, which is what makes the
            // started session's target the derived focused agent.
            state.center_zone.update(|center_zone| {
                center_zone.reveal_tab(tab_id);
            });
            let agent = agent_ref.get_untracked();
            match voice::availability(&state, agent.as_ref()) {
                Ok(target) => voice::start(target),
                Err(reason) => {
                    crate::components::header::report_user_error(format!(
                        "Voice unavailable: {}",
                        reason.reason()
                    ));
                }
            }
        }
    };

    view! {
        <div class="chat-voice-control">
            <button
                type="button"
                class="chat-voice-btn"
                class:chat-voice-btn-live=move || is_live.get()
                data-test="chat-voice-toggle"
                disabled=move || !is_live.get() && readiness.get().is_err()
                on:click=on_click
                aria-pressed=move || if is_live.get() { "true" } else { "false" }
                aria-label=move || mic_label(is_live.get(), &readiness.get())
                title=move || mic_label(is_live.get(), &readiness.get())
            >
                <span class="chat-voice-btn-glyph" aria-hidden="true">
                    {move || if is_live.get() { "◉" } else { "🎙" }}
                </span>
            </button>
            <span class="chat-voice-unavailable" data-test="chat-voice-unavailable">
                {move || visible_reason.get()}
            </span>
        </div>
    }
}

/// Rows below the strip's header: the tool banner, server-projected progress,
/// and any warning or terminal explanation.
///
/// Built as one erased `Vec<AnyView>` rather than several conditional blocks,
/// which keeps this file to a small number of monomorphised view types.
/// One row of the strip body, as data.
///
/// The composition — which rows, in what order, with what text — is the part
/// worth pinning, and it is pure. Keeping it separate from rendering lets it be
/// asserted in a native test instead of a browser one, which matters: the whole
/// frontend suite runs in a single wasm instance and mounted tests are the
/// expensive kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StripRow {
    pub class: &'static str,
    pub test_id: &'static str,
    pub text: String,
}

impl StripRow {
    fn new(class: &'static str, test_id: &'static str, text: String) -> Self {
        Self {
            class,
            test_id,
            text,
        }
    }
}

/// What the strip body says, in order.
///
/// Ordering is the contract: settled transcript oldest-first, then the live
/// caption, then what the agent is doing, then anything that went wrong. A
/// terminal failure replaces the "why it ended" line rather than stacking with
/// it — one explanation, not two.
pub(crate) fn strip_rows(voice: &VoiceUiState) -> Vec<StripRow> {
    let mut rows = Vec::new();

    // Finalised lines first, oldest to newest, each attributed to the speaker
    // the host named. Attribution is never inferred here.
    for line in &voice.transcript {
        rows.push(StripRow::new(
            "voice-strip-transcript",
            "voice-transcript",
            format!("{}: {}", line.speaker.label(), line.text),
        ));
    }
    // Then whatever is being said right now, which may still be partial.
    if let Some(caption) = voice.caption.as_ref() {
        rows.push(StripRow::new(
            "voice-strip-caption",
            "voice-caption",
            caption.clone(),
        ));
    }
    if let Some(notice) = voice.tool_notice.as_ref() {
        rows.push(StripRow::new(
            "voice-strip-tool",
            "voice-tool",
            notice.clone(),
        ));
    }
    for line in &voice.progress {
        rows.push(StripRow::new(
            "voice-strip-progress",
            "voice-progress",
            line.text.clone(),
        ));
    }
    // A warning describes a session that is still running, so it renders
    // distinctly from the terminal explanation below it.
    if let Some(warning) = voice.warning.as_ref() {
        rows.push(StripRow::new(
            "voice-strip-warning",
            "voice-warning",
            format!("{}: {}", warning.stage.label(), warning.message),
        ));
    }
    if let Some(failure) = voice.failure.as_ref() {
        rows.push(StripRow::new(
            "voice-strip-error",
            "voice-error",
            format!("{}: {}", failure.stage.label(), failure.message),
        ));
    } else if let Some(reason) = voice.ended_reason.filter(|reason| reason.is_involuntary()) {
        rows.push(StripRow::new(
            "voice-strip-ended",
            "voice-ended",
            reason.label().to_owned(),
        ));
    }
    rows
}

/// Render [`strip_rows`] through a single `view!` site, so the strip
/// contributes one monomorphised row type rather than one per kind.
fn body_rows(voice: &VoiceUiState) -> Vec<AnyView> {
    strip_rows(voice)
        .into_iter()
        .map(|row| view! { <div class=row.class data-test=row.test_id>{row.text}</div> }.into_any())
        .collect()
}

/// What the strip's status line says.
///
/// Pure, so it is pinned in a native test rather than by reading text out of a
/// mounted tree. The rendered assertion then only has to prove the component
/// uses it — which is a stronger check than re-stating the string in the DOM
/// test, and costs nothing in the browser suite.
pub(crate) fn strip_status(voice: &VoiceUiState, who: &str) -> String {
    match voice.phase {
        VoicePhase::RequestingMic => format!("Voice with {who} · Waiting for the microphone"),
        VoicePhase::Negotiating => format!("Voice with {who} · Connecting"),
        VoicePhase::Live => format!("Voice with {who} · {}", voice.activity.label()),
        VoicePhase::Ending => format!("Voice with {who} · Ending"),
        VoicePhase::Failed => format!("Voice with {who} · Stopped"),
        VoicePhase::Idle => String::new(),
    }
}

/// The observed-audio diagnostics line. Reports what the track said, plus the
/// host's reachability constraint when it declared one.
pub(crate) fn strip_diagnostics(voice: &VoiceUiState) -> String {
    let mut text = voice.effective_audio.summary();
    if voice.direct_connections_only {
        text.push_str(" · Direct connections only");
    }
    text
}

/// The microphone control's accessible name and tooltip.
///
/// A live session offers to end it; an unavailable host has to say *why*, in
/// words, because a disabled button cannot be focused to reveal a tooltip.
pub(crate) fn mic_label(is_live: bool, readiness: &Result<(), String>) -> String {
    match (is_live, readiness) {
        (true, _) => "End voice".to_owned(),
        (false, Ok(())) => "Start voice".to_owned(),
        (false, Err(reason)) => format!("Voice unavailable: {reason}"),
    }
}

/// Resolve the bound agent's display name.
///
/// Matched on the **host-qualified** identity. `AgentId`s are only unique per
/// host, so matching on the id alone can label the strip with an agent from a
/// different machine.
fn agent_name(state: &AppState, agent: &ActiveAgentRef) -> String {
    state
        .agents
        .with(|agents| {
            agents
                .iter()
                .find(|info| info.host_id == agent.host_id && info.agent_id == agent.agent_id)
                .map(|info| info.name.clone())
        })
        .unwrap_or_else(|| "this agent".to_owned())
}

/// The compact voice layer for one chat.
///
/// Renders nothing unless a session is bound to this chat's agent, so in a
/// split only the targeted chat shows it and there is never more than one
/// strip on screen.
#[component]
pub fn VoiceStrip(agent_ref: Signal<Option<ActiveAgentRef>>) -> impl IntoView {
    let state = expect_context::<AppState>();
    let voice_state = voice::ui_state();

    let visible = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| {
            let agent = agent_ref.get();
            voice_state.with(|voice| {
                voice.bound_agent() == agent.as_ref() && !matches!(voice.phase, VoicePhase::Idle)
            })
        })
    };

    let status = {
        let voice_state = voice_state.clone();
        let state = state.clone();
        Memo::new(move |_| {
            voice_state.with(|voice| {
                let who = voice
                    .target
                    .as_ref()
                    .map(|target| agent_name(&state, &target.agent))
                    .unwrap_or_else(|| "this agent".to_owned());
                strip_status(voice, &who)
            })
        })
    };

    let diagnostics = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(strip_diagnostics))
    };

    let rows = {
        let voice_state = voice_state.clone();
        move || voice_state.with(body_rows)
    };

    let aec_label = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(|voice| voice.aec.label()))
    };
    let aec_warns = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(|voice| voice.aec.is_warning()))
    };
    let muted = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(|voice| voice.muted))
    };
    let engaged = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(VoiceUiState::is_engaged))
    };
    let playback_blocked = {
        let voice_state = voice_state.clone();
        Memo::new(move |_| voice_state.with(|voice| voice.playback_blocked))
    };

    view! {
        <div
            class="voice-strip"
            class:voice-strip-hidden=move || !visible.get()
            data-test="voice-strip"
            role="group"
            aria-label="Voice session"
        >
            <div class="voice-strip-head">
                // Only the status text is a live region. Wrapping the control
                // group would make every state change re-announce all four
                // button labels.
                <span
                    class="voice-strip-status"
                    data-test="voice-status"
                    role="status"
                    aria-live="polite"
                >
                    {move || status.get()}
                </span>
                <span
                    class="voice-strip-aec"
                    class:voice-strip-aec-warn=move || aec_warns.get()
                    data-test="voice-aec"
                    title=move || diagnostics.get()
                >
                    {move || aec_label.get()}
                </span>
                <span class="voice-strip-spacer"></span>
                // Named for what it actually does. It pauses the local audio
                // element and sends nothing to the host, so it cannot be
                // claimed to interrupt the agent's response — a live
                // microphone may cause provider-side barge-in, but that is
                // unverified live behaviour and must not be asserted by a
                // control label.
                <button
                    type="button"
                    class="voice-strip-btn"
                    data-test="voice-silence-output"
                    hidden=move || !engaged.get()
                    on:click=move |_| voice::silence_output()
                    aria-label="Silence voice output and keep the microphone open"
                >
                    "Silence output"
                </button>
                <button
                    type="button"
                    class="voice-strip-btn"
                    data-test="voice-mute"
                    hidden=move || !engaged.get()
                    on:click=move |_| voice::toggle_mute()
                    aria-pressed=move || if muted.get() { "true" } else { "false" }
                    aria-label=move || {
                        if muted.get() { "Unmute microphone" } else { "Mute microphone" }
                    }
                >
                    {move || if muted.get() { "Unmute" } else { "Mute" }}
                </button>
                <button
                    type="button"
                    class="voice-strip-btn voice-strip-btn-warn"
                    data-test="voice-resume-playback"
                    hidden=move || !playback_blocked.get()
                    on:click=move |_| voice::resume_playback()
                    aria-label="Play voice output"
                >
                    "Tap to hear"
                </button>
                <button
                    type="button"
                    class="voice-strip-btn"
                    data-test="voice-end"
                    hidden=move || !engaged.get()
                    on:click=move |_| voice::end(voice::session::VoiceEndReason::UserRequested)
                    aria-label="End voice"
                >
                    "End"
                </button>
                <button
                    type="button"
                    class="voice-strip-btn"
                    data-test="voice-dismiss"
                    hidden=move || engaged.get()
                    on:click=move |_| voice::dismiss()
                    aria-label="Dismiss voice status"
                >
                    "Dismiss"
                </button>
            </div>
            <div class="voice-strip-body">{rows}</div>
            <div class="voice-strip-diagnostics" data-test="voice-diagnostics">
                {move || diagnostics.get()}
            </div>
        </div>
    }
}

/// Row composition is pure, so it is pinned here rather than through the DOM.
///
/// These assertions used to live in mounted browser tests. Moving them costs
/// nothing in coverage — the ordering, attribution, and
/// warning-versus-failure contracts are all expressible on the data — and it
/// keeps the wasm suite to the assertions that genuinely need a rendered tree.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod row_tests {
    use super::*;
    use crate::voice::session::{
        EffectiveAudio, TranscriptLine, TranscriptSpeaker, VoiceActivity, VoiceEndReason,
        VoiceFailure, VoiceProgressLine, VoiceStage,
    };

    fn live_state() -> VoiceUiState {
        VoiceUiState {
            transcript: vec![
                TranscriptLine::bounded(
                    TranscriptSpeaker::User,
                    "why is it failing".to_owned(),
                    true,
                ),
                TranscriptLine::bounded(
                    TranscriptSpeaker::Agent,
                    "the linker step".to_owned(),
                    true,
                ),
            ],
            caption: Some("checking the build".to_owned()),
            tool_notice: Some("Agent is running a tool".to_owned()),
            progress: vec![VoiceProgressLine {
                sequence: 3,
                text: "Agent started a tool".to_owned(),
            }],
            ..VoiceUiState::default()
        }
    }

    fn ids(voice: &VoiceUiState) -> Vec<&'static str> {
        strip_rows(voice)
            .into_iter()
            .map(|row| row.test_id)
            .collect()
    }

    #[test]
    fn the_status_line_names_the_bound_agent_in_every_phase_that_has_one() {
        let mut voice = VoiceUiState::default();
        assert_eq!(
            strip_status(&voice, "Nova"),
            "",
            "an idle strip claims no session"
        );

        for (phase, expected) in [
            (
                VoicePhase::RequestingMic,
                "Voice with Nova · Waiting for the microphone",
            ),
            (VoicePhase::Negotiating, "Voice with Nova · Connecting"),
            (VoicePhase::Ending, "Voice with Nova · Ending"),
            (VoicePhase::Failed, "Voice with Nova · Stopped"),
        ] {
            voice.phase = phase;
            assert_eq!(strip_status(&voice, "Nova"), expected, "{phase:?}");
        }

        // Live defers to the host-reported activity rather than inventing one.
        voice.phase = VoicePhase::Live;
        for activity in [
            VoiceActivity::Connecting,
            VoiceActivity::Listening,
            VoiceActivity::AgentSpeaking,
            VoiceActivity::AgentWorking,
            VoiceActivity::Ending,
        ] {
            voice.activity = activity;
            assert_eq!(
                strip_status(&voice, "Nova"),
                format!("Voice with Nova · {}", activity.label()),
                "{activity:?}"
            );
        }
    }

    #[test]
    fn diagnostics_report_what_was_observed_and_the_hosts_reachability() {
        let mut voice = VoiceUiState {
            effective_audio: EffectiveAudio {
                echo_cancellation: Some(true),
                noise_suppression: Some(false),
                auto_gain_control: None,
                sample_rate: Some(48_000),
            },
            ..VoiceUiState::default()
        };
        let text = strip_diagnostics(&voice);
        assert!(text.contains("Echo cancellation: on"), "{text}");
        assert!(text.contains("Noise suppression: off"), "{text}");
        assert!(
            text.contains("Auto gain: not reported"),
            "an absent setting is not a disabled one: {text}"
        );
        assert!(text.contains("48000 Hz"), "{text}");
        assert!(
            !text.contains("Direct connections only"),
            "the constraint is only claimed when the host declared it: {text}"
        );

        voice.direct_connections_only = true;
        assert!(
            strip_diagnostics(&voice).ends_with("· Direct connections only"),
            "the host's reachability constraint belongs in diagnostics"
        );
    }

    #[test]
    fn the_microphone_control_always_says_what_it_will_do_or_why_it_cannot() {
        assert_eq!(mic_label(true, &Ok(())), "End voice");
        assert_eq!(
            mic_label(true, &Err("stale".to_owned())),
            "End voice",
            "a live session is endable regardless of what availability now says"
        );
        assert_eq!(mic_label(false, &Ok(())), "Start voice");
        assert_eq!(
            mic_label(false, &Err("no direct network route".to_owned())),
            "Voice unavailable: no direct network route",
            "an unavailable control must carry the host's reason, not a generic label"
        );
    }

    #[test]
    fn what_was_said_comes_first_oldest_to_newest_then_the_live_caption() {
        let rows = strip_rows(&live_state());
        assert_eq!(
            rows.iter().map(|row| row.test_id).collect::<Vec<_>>(),
            vec![
                "voice-transcript",
                "voice-transcript",
                "voice-caption",
                "voice-tool",
                "voice-progress",
            ]
        );
        assert_eq!(rows[0].text, "You: why is it failing");
        assert_eq!(
            rows[1].text, "Agent: the linker step",
            "each line is attributed to the speaker the host named"
        );
        assert_eq!(
            rows[2].text, "checking the build",
            "the live caption is its own row, not folded into the transcript"
        );
    }

    #[test]
    fn a_warning_renders_distinctly_from_a_terminal_failure() {
        let mut voice = live_state();
        voice.warning = Some(VoiceFailure {
            stage: VoiceStage::Media,
            message: "the connection is unstable".to_owned(),
            retryable: true,
        });
        let rows = strip_rows(&voice);
        let warning = rows
            .iter()
            .find(|row| row.test_id == "voice-warning")
            .expect("a surviving problem is shown");
        assert_eq!(warning.text, "Audio: the connection is unstable");
        assert_ne!(
            warning.class, "voice-strip-error",
            "a session that is still running must not look like one that stopped"
        );
        assert!(
            !ids(&voice).contains(&"voice-error"),
            "a warning is not a failure"
        );
    }

    #[test]
    fn a_failure_replaces_the_ended_line_rather_than_stacking_with_it() {
        let mut voice = live_state();
        voice.ended_reason = Some(VoiceEndReason::MediaFailed);
        assert_eq!(
            ids(&voice).last(),
            Some(&"voice-ended"),
            "an involuntary end explains itself"
        );

        voice.failure = Some(VoiceFailure {
            stage: VoiceStage::Microphone,
            message: "the microphone stopped".to_owned(),
            retryable: true,
        });
        let ids = ids(&voice);
        assert!(ids.contains(&"voice-error"));
        assert!(
            !ids.contains(&"voice-ended"),
            "one explanation, not two: the failure says what the end reason would have"
        );
    }

    #[test]
    fn a_user_requested_end_explains_nothing_because_the_user_already_knows() {
        let mut voice = VoiceUiState {
            ended_reason: Some(VoiceEndReason::UserRequested),
            ..VoiceUiState::default()
        };
        assert!(strip_rows(&voice).is_empty());

        voice.ended_reason = Some(VoiceEndReason::HostDisconnected);
        assert_eq!(
            ids(&voice),
            vec!["voice-ended"],
            "an end the user did not ask for does need explaining"
        );
    }

    #[test]
    fn an_idle_strip_says_nothing() {
        assert!(strip_rows(&VoiceUiState::default()).is_empty());
    }
}

/// The desktop voice layer's one rendered scenario.
///
/// Deliberately a single mount. The whole frontend suite runs in one wasm
/// instance, and each `mount_to` call site monomorphises its own render
/// pipeline, so extra mounted tests cost far more than extra assertions. Every
/// contract that does not need a real DOM has been moved to the native
/// `row_tests` module above; what remains here is what only a rendered tree
/// and a real reactive runtime can prove:
///
/// - the control and strip are wired to the pure text those tests pin;
/// - clicking the control starts a session bound to *this* chat;
/// - exactly one strip exists, and it names the agent it is bound to;
/// - the surrounding chat stays mounted, usable, and undisturbed throughout;
/// - the reactive guard observes the derived composer owner and an agent
///   going fatal, and tears the session down through the real `Effect`;
/// - an unavailable host disables the control and says why in visible text.
#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{AgentInfo, ConnectionStatus, TabContent, TabId};
    use crate::voice::{
        self,
        session::{
            AecStatus, EffectiveAudio, TranscriptLine, TranscriptSpeaker, VoiceActivity,
            VoicePhase, VoiceProgressLine, VoiceSessionKey, VoiceTarget,
        },
    };
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, BackendKind, HostSettings, StreamPath, VoiceAvailability,
        VoiceSettings, VoiceUnavailableReason,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    const HOST: &str = "host-1";
    const AGENT: &str = "agent-1";
    const OTHER_AGENT: &str = "agent-2";
    const STREAM: &str = "/agent/agent-1/inst";

    fn container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let element = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&element).unwrap();
        element.dyn_into::<HtmlElement>().unwrap()
    }

    async fn next_tick() {
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                .unwrap();
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    fn agent_ref(id: &str) -> ActiveAgentRef {
        ActiveAgentRef {
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
        }
    }

    fn agent_info(id: &str, name: &str, stream: &str) -> AgentInfo {
        AgentInfo {
            host_id: HOST.to_owned(),
            agent_id: AgentId(id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::User,
            backend_kind: BackendKind::Claude,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: None,
            team_member_id: None,
            session_id: None,
            custom_agent_id: None,
            workflow: None,
            created_at_ms: 0,
            instance_stream: StreamPath(stream.to_owned()),
            started: true,
            fatal_error: None,
            activity_summary: Default::default(),
        }
    }

    fn set_availability(state: &AppState, availability: VoiceAvailability) {
        state.host_settings_by_host.update(|settings| {
            settings.insert(
                HOST.to_owned(),
                HostSettings {
                    voice: VoiceSettings {
                        enabled: true,
                        availability,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        });
    }

    fn seed(state: &AppState) {
        state.agents.set(vec![
            agent_info(AGENT, "Nova Test Agent", STREAM),
            agent_info(OTHER_AGENT, "Other Agent", "/agent/agent-2/inst"),
        ]);
        state.connection_statuses.update(|statuses| {
            statuses.insert(HOST.to_owned(), ConnectionStatus::Connected);
        });
        set_availability(
            state,
            VoiceAvailability::Available {
                direct_connections_only: true,
            },
        );
    }

    /// Drive the reducer into a live session carrying everything the strip can
    /// show, without a browser media stack.
    fn go_live(activity: VoiceActivity, echo_cancellation: Option<bool>) {
        voice::ui_state().update(|voice| {
            let generation = voice.begin(
                VoiceTarget::new(agent_ref(AGENT), StreamPath(STREAM.to_owned())),
                VoiceSessionKey {
                    session_id: "sess-1".to_owned(),
                    stream: StreamPath("/voice/sess-1".to_owned()),
                },
            );
            voice.mic_granted(
                generation,
                EffectiveAudio {
                    echo_cancellation,
                    noise_suppression: Some(true),
                    auto_gain_control: Some(true),
                    sample_rate: Some(48_000),
                },
            );
            voice.ready(generation, true);
            voice.connected(generation);
            voice.set_activity(generation, activity);
            voice.push_progress(
                generation,
                VoiceProgressLine {
                    sequence: 12,
                    text: "Agent started a tool".to_owned(),
                },
            );
            voice.push_transcript(
                generation,
                TranscriptLine::bounded(
                    TranscriptSpeaker::User,
                    "what is failing in the build".to_owned(),
                    true,
                ),
            );
            voice.set_caption(generation, Some("the linker step".to_owned()));
        });
    }

    fn text_of(root: &HtmlElement, selector: &str) -> String {
        root.query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("missing {selector}"))
            .text_content()
            .unwrap_or_default()
    }

    fn find(root: &HtmlElement, selector: &str) -> HtmlElement {
        root.query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("missing {selector}"))
            .dyn_into::<HtmlElement>()
            .unwrap()
    }

    fn visible_strips(root: &HtmlElement) -> u32 {
        root.query_selector_all("[data-test='voice-strip']:not(.voice-strip-hidden)")
            .unwrap()
            .length()
    }

    fn open_chat(state: &AppState, id: &str) -> TabId {
        state
            .open_tab(
                TabContent::chat_with_agent(agent_ref(id)),
                id.to_owned(),
                true,
            )
            .expect("chat tab opens")
    }

    #[wasm_bindgen_test]
    async fn the_voice_layer_renders_binds_guards_and_leaves_the_chat_alone() {
        let owner = leptos::reactive::owner::Owner::new();
        owner.set();
        let root = container();
        let state = AppState::new();
        voice::reset_for_tests();
        seed(&state);
        voice::set_platform_factory(std::rc::Rc::new(|| {
            std::rc::Rc::new(crate::voice::media::FakeMediaPlatform::new())
                as std::rc::Rc<dyn crate::voice::media::MediaPlatform>
        }));

        let tab = open_chat(&state, AGENT);
        let composer = state.composer_for(tab);
        composer.text.set("a draft the user typed".to_owned());

        let handle = mount_to(root.clone().unchecked_into(), {
            let state = state.clone();
            move || {
                provide_context(state.clone());
                let bound = Signal::derive(move || Some(agent_ref(AGENT)));
                view! {
                    <div>
                        // Stands in for the surrounding chat: if voice ever
                        // unmounts or disables the composer, this goes with it.
                        <textarea class="chat-textarea"></textarea>
                        <VoiceStrip agent_ref=bound />
                        <VoiceMicButton agent_ref=bound tab_id=tab />
                    </div>
                }
            }
        });
        // Installed after the mount: `mount_to` is what initialises Leptos's
        // async executor, and an `Effect` created before that never runs. The
        // guard therefore runs through a real `Effect` here, which is what
        // makes this scenario cover the production route by which switching
        // chats ends voice.
        voice::install_guard(&state);
        next_tick().await;

        // ── idle ────────────────────────────────────────────────────────────
        assert_eq!(visible_strips(&root), 0, "no strip before voice starts");
        let button = find(&root, "[data-test='chat-voice-toggle']");
        assert!(
            !button
                .clone()
                .dyn_into::<web_sys::HtmlButtonElement>()
                .unwrap()
                .disabled(),
            "an available host offers the control"
        );
        assert_eq!(
            button.get_attribute("aria-label").unwrap_or_default(),
            mic_label(false, &Ok(())),
            "the control is wired to the label the native tests pin"
        );

        // ── clicking starts a session bound to this chat ────────────────────
        button.click();
        next_tick().await;
        assert!(voice::is_engaged(), "clicking starts a session");
        assert_eq!(
            visible_strips(&root),
            1,
            "exactly one strip, bound to this chat"
        );

        // ── a live session renders what the host reported ───────────────────
        go_live(VoiceActivity::AgentSpeaking, Some(true));
        next_tick().await;

        let expected_status =
            voice::ui_state().with_untracked(|voice| strip_status(voice, "Nova Test Agent"));
        assert_eq!(
            text_of(&root, "[data-test='voice-status']"),
            expected_status,
            "the strip renders the status the native tests pin, for the agent it is bound to"
        );
        assert!(
            expected_status.contains("Nova Test Agent") && expected_status.contains("Speaking"),
            "and that status names the agent and the host-reported state, got {expected_status:?}"
        );

        assert_eq!(
            text_of(&root, "[data-test='voice-aec']"),
            AecStatus::Confirmed.label(),
            "an observed echoCancellation:true is the only thing allowed to read as AEC on"
        );
        assert_eq!(
            text_of(&root, "[data-test='voice-diagnostics']"),
            voice::ui_state().with_untracked(strip_diagnostics),
            "diagnostics render the observed settings, not the requested ones"
        );

        // Protocol 45 carries what was actually said; showing only state
        // labels while the host has real words is a regression. Row
        // composition and attribution are pinned in `row_tests`; what needs a
        // rendered tree is that those rows reach the DOM.
        let body = text_of(&root, ".voice-strip-body");
        assert!(
            body.contains("Agent started a tool"),
            "server-projected progress must be visible, got {body:?}"
        );
        assert!(
            body.contains("You: what is failing in the build"),
            "the settled, attributed transcript line must be visible, got {body:?}"
        );
        assert_eq!(
            text_of(&root, "[data-test='voice-caption']"),
            "the linker step",
            "and the live caption is shown as its own line"
        );

        // ── a browser that reports no echoCancellation at all ───────────────
        go_live(VoiceActivity::Listening, None);
        next_tick().await;
        let label = text_of(&root, "[data-test='voice-aec']");
        assert_eq!(label, AecStatus::RequestedUnreported.label());
        assert_ne!(
            label,
            AecStatus::Confirmed.label(),
            "requesting echo cancellation is not evidence it is happening"
        );
        assert!(
            text_of(&root, "[data-test='voice-diagnostics']")
                .contains("Echo cancellation: not reported"),
            "diagnostics must say the browser did not report, not that it is off"
        );

        // ── the surrounding chat is untouched ───────────────────────────────
        let textarea = find(&root, ".chat-textarea")
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        assert!(
            !textarea.disabled(),
            "typing must remain possible during voice"
        );
        assert_eq!(
            composer.text.get_untracked(),
            "a draft the user typed",
            "a live session must not disturb the draft"
        );

        // ── the reactive guard ends voice when the composer owner moves ─────
        open_chat(&state, OTHER_AGENT);
        next_tick().await;
        assert!(
            !voice::is_engaged(),
            "the guard must observe the derived composer owner through its Effect"
        );
        assert!(
            text_of(&root, ".voice-strip-body").contains("switched to another agent"),
            "an involuntary end explains itself where the user was already looking"
        );
        assert_eq!(
            composer.text.get_untracked(),
            "a draft the user typed",
            "a guard-initiated teardown must not disturb the draft either"
        );

        // ── and when the bound agent dies under it ──────────────────────────
        // The control focuses its own chat before starting, so clicking it is
        // enough to bring the composer owner back.
        find(&root, "[data-test='chat-voice-toggle']").click();
        next_tick().await;
        assert!(voice::is_engaged(), "voice restarts on the original chat");
        assert_eq!(visible_strips(&root), 1);

        state
            .agents
            .update(|agents| agents[0].fatal_error = Some("backend crashed".to_owned()));
        next_tick().await;
        assert!(
            !voice::is_engaged(),
            "a fatal agent error reaches the guard through the agent list alone"
        );
        assert_eq!(
            voice::ui_state().with_untracked(|voice| voice.ended_reason),
            Some(voice::session::VoiceEndReason::AgentFatal)
        );

        // ── dismissing clears the residual explanation ──────────────────────
        find(&root, "[data-test='voice-dismiss']").click();
        next_tick().await;
        assert_eq!(
            visible_strips(&root),
            0,
            "once acknowledged, voice leaves the chat exactly as it found it"
        );
        assert_eq!(voice::phase(), VoicePhase::Idle);

        // ── an unavailable host disables the control and says why ───────────
        state.agents.update(|agents| agents[0].fatal_error = None);
        set_availability(
            &state,
            VoiceAvailability::Unavailable {
                reason: VoiceUnavailableReason::NoReachableCandidate,
            },
        );
        next_tick().await;

        let button = find(&root, "[data-test='chat-voice-toggle']")
            .dyn_into::<web_sys::HtmlButtonElement>()
            .unwrap();
        assert!(
            button.disabled(),
            "an unavailable host must not offer voice"
        );
        // Visible text, not only a tooltip: a disabled button is not
        // keyboard-focusable, so `title` alone is unreachable for keyboard and
        // touch users and there would be no visible unavailable state at all.
        let visible = text_of(&root, "[data-test='chat-voice-unavailable']");
        assert!(
            visible.contains("direct network route"),
            "the reason must be readable without hovering, got {visible:?}"
        );
        assert!(
            button
                .get_attribute("title")
                .unwrap_or_default()
                .contains("direct network route"),
            "a disabled control that does not say why is a silent failure"
        );
        assert!(
            button
                .get_attribute("aria-label")
                .unwrap_or_default()
                .contains("direct network route"),
            "the reason must reach assistive technology too"
        );
        assert!(!voice::is_engaged(), "a disabled control starts nothing");

        drop(handle);
        root.remove();
        voice::reset_for_tests();
    }
}
