use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::components::ui::{Button, ButtonSize, ButtonVariant};
use crate::state::{
    ActiveAgentRef, AgentRef, AppState, PendingSubmissionState, SubmissionLifecycle,
    SubmissionOriginId, SubmissionWithdrawal, ToolOutputMode, ToolRequestEntry,
};

use protocol::{
    AgentControlStatus, AgentId, AskUserQuestion, ExitPlanModeDecision, FrameKind,
    SendMessagePayload, SendMessageToolResponse, StreamPath, ToolExecutionNormalizationFailure,
    ToolExecutionResult, ToolRequestType, TydeAgentWaitStatus,
};

/// Renders a single tool request inside an assistant message.
///
/// Carries semantic state through the `data-mobile-test` selector
/// (`tool-card-running`, `tool-card-success`, `tool-card-failed`) so
/// tests don't need to guess at color or icon. Failed and running
/// cards always reveal their output detail; successful cards honor
/// the global `ToolOutputMode`.
///
/// Claude's typed `AskUserQuestion` tool is special-cased: instead of the raw
/// result it renders an interactive question card whose answer is sent back
/// through the normal `SendMessage` path (see [`AskUserQuestionCard`]).
///
/// `owner_agent_ref` is the chat-map key of the stream that produced this card:
/// the calling agent, on the host that owns it. The Tyde orchestration cards
/// need that host to resolve the *child* agents they refer to — a child
/// `AgentId` is only meaningful relative to its host, and the same id can exist
/// on two hosts.
///
/// It is plumbed explicitly rather than read from `state.active_agent`, and that
/// distinction is load-bearing rather than stylistic. `active_agent` is mutable
/// navigation state: it changes the instant the user opens a different chat,
/// while this card still belongs to the stream it came from. Resolving ownership
/// from it means a card silently re-points at a different host's agent — showing
/// another agent's name and status, and navigating to the wrong agent — purely
/// because the user looked elsewhere. Stream ownership travels with the row.
#[component]
pub fn ToolCardView(owner_agent_ref: AgentRef, entry: ToolRequestEntry) -> impl IntoView {
    let state = expect_context::<AppState>();
    let tool_output_mode = state.tool_output_mode;

    let tool_name = entry.request.tool_name.clone();
    let normalization_failure = entry
        .result
        .as_ref()
        .and_then(|result| result.normalization_failure);
    let normalization_failed = normalization_failure.is_some();

    if let ToolRequestType::AskUserQuestion { questions } = &entry.request.tool_type
        && entry.result.as_ref().is_none_or(|result| result.success)
    {
        let questions = questions.clone();
        return view! {
            <div class="tool-card ask-question" data-mobile-test="tool-card-ask-question">
                <div class="tool-card-header">
                    <span class="tool-name">{tool_name}</span>
                </div>
                <AskUserQuestionCard
                    owner_agent_ref=owner_agent_ref.clone()
                    questions=questions
                />
            </div>
        }
        .into_any();
    }

    // A pending ExitPlanMode awaits the user's approval: render the plan plus
    // Approve/Reject controls. Once the request succeeds the card renders the
    // plan read-only (no active controls), matching the desktop card. A failed
    // completion falls through to the generic completion body below.
    if let ToolRequestType::ExitPlanMode { plan, plan_path } = &entry.request.tool_type {
        let succeeded = entry.result.as_ref().map(|r| r.success);
        match succeeded {
            // Pending: interactive Approve/Reject card.
            None => {
                let plan = plan.clone();
                let plan_path = plan_path.clone();
                let tool_call_id = entry.request.tool_call_id.clone();
                return view! {
                    <div class="tool-card exit-plan" data-mobile-test="tool-card-exit-plan">
                        <div class="tool-card-header">
                            <span class="tool-name">{tool_name}</span>
                        </div>
                        <ExitPlanModeCard
                            owner_agent_ref=owner_agent_ref.clone()
                            tool_call_id=tool_call_id
                            plan=plan
                            plan_path=plan_path
                        />
                    </div>
                }
                .into_any();
            }
            // Approved/decided: show the plan read-only, no controls.
            Some(true) => {
                let plan = plan.clone();
                let plan_path = plan_path.clone();
                return view! {
                    <div
                        class="tool-card completed success exit-plan"
                        data-mobile-test="tool-card-exit-plan-done"
                        aria-label="Plan reviewed"
                    >
                        <div class="tool-card-header">
                            <span class="tool-status-icon" aria-hidden="true">"\u{2713}"</span>
                            <span class="tool-name">{tool_name}</span>
                        </div>
                        <div class="exit-plan-body exit-plan-readonly" data-mobile-test="exit-plan-readonly">
                            {plan_path.map(|path| view! {
                                <div class="exit-plan-path">{format!("Plan: {path}")}</div>
                            })}
                            {plan
                                .filter(|p| !p.trim().is_empty())
                                .map(|plan| view! {
                                    <div class="exit-plan-text" data-mobile-test="exit-plan-text">{plan}</div>
                                })}
                        </div>
                    </div>
                }
                .into_any();
            }
            // Failed: fall through to the generic failed-completion body.
            Some(false) => {}
        }
    }

    // Tyde orchestration tools carry their meaning in typed data, so they render
    // semantically here rather than falling through to the `Debug` dump below —
    // a new typed variant that degraded to `format!("{:?}", …)` on the PWA would
    // be a silent fallback, which is exactly what the architecture forbids. A
    // *failed* call still falls through on purpose: the error must stay visible.
    let succeeded = entry.result.as_ref().map(|result| result.success);
    if succeeded != Some(false) {
        let status_class = if normalization_failed {
            "completed failed"
        } else if entry.result.is_some() {
            "completed success"
        } else {
            "running"
        };

        if let ToolRequestType::TydeSendAgentMessage { agent_id, message } =
            &entry.request.tool_type
        {
            let agent_id = agent_id.clone();
            let message = message.clone();
            // The completion is an ack with no body. A *successful* completion of
            // any other shape means the request and result normalizers disagree —
            // protocol drift, which must be loud rather than silently rendered as
            // if everything were fine.
            let mismatch = match entry.result.as_ref().map(|result| &result.tool_result) {
                None | Some(ToolExecutionResult::TydeSendAgentMessage) => false,
                Some(other) => {
                    log::error!(
                        "tyde_send_agent_message completed with a non-ack result: {other:?}"
                    );
                    true
                }
            };
            let typed_request = typed_request_json(&entry.request.tool_type);
            return view! {
                <div
                    class=format!("tool-card {status_class} send-agent-message")
                    data-mobile-test="tool-card-send-message"
                    aria-label=normalization_failed.then_some("Tool failed: canonical data could not be normalized")
                >
                    <div class="tool-card-header">
                        <span class="tool-name">{tool_name}</span>
                    </div>
                    <SendAgentMessageCard
                        owner_agent_ref=owner_agent_ref
                        agent_id=agent_id
                        message=message
                        mismatch=mismatch
                        normalization_failed=normalization_failed
                        typed_request=typed_request
                    />
                </div>
            }
            .into_any();
        }

        if let ToolRequestType::TydeAwaitAgents { agent_ids } = &entry.request.tool_type {
            let agent_ids = agent_ids.clone();
            let verdict = if normalization_failed {
                AwaitVerdict::NormalizationFailure
            } else {
                match entry.result.as_ref().map(|result| &result.tool_result) {
                    None => AwaitVerdict::Pending,
                    Some(ToolExecutionResult::TydeAwaitAgents {
                        ready,
                        still_thinking,
                    }) => AwaitVerdict::Completed {
                        ready: ready.clone(),
                        still_thinking: still_thinking.clone(),
                    },
                    // A typed request whose completion is untyped means the request
                    // and result normalizers disagree. Surface it; never paper over.
                    Some(other) => {
                        log::error!(
                            "tyde_await_agents completed with an untyped result: {other:?}"
                        );
                        AwaitVerdict::Mismatch
                    }
                }
            };
            return view! {
                <div
                    class=format!("tool-card {status_class} await-agents")
                    data-mobile-test="tool-card-await-agents"
                    aria-label=normalization_failed.then_some("Tool failed: canonical data could not be normalized")
                >
                    <div class="tool-card-header">
                        <span class="tool-name">{tool_name}</span>
                    </div>
                    <AwaitAgentsCard
                        owner_agent_ref=owner_agent_ref
                        agent_ids=agent_ids
                        verdict=verdict
                    />
                </div>
            }
            .into_any();
        }

        if let ToolRequestType::GenerateImage { prompt } = &entry.request.tool_type {
            let prompt = prompt
                .clone()
                .unwrap_or_else(|| "Generating image".to_owned());
            let detail = match entry.result.as_ref().map(|result| &result.tool_result) {
                None => "Generating image".to_owned(),
                Some(ToolExecutionResult::GenerateImage { image_count, .. }) => format!(
                    "{image_count} image{} generated",
                    if *image_count == 1 { "" } else { "s" }
                ),
                Some(other) => {
                    log::error!("generate_image completed with an untyped result: {other:?}");
                    "Image result could not be read".to_owned()
                }
            };
            return view! {
                <div
                    class=format!("tool-card {status_class} generate-image")
                    data-mobile-test="tool-card-generate-image"
                >
                    <div class="tool-card-header">
                        <span class="tool-name">{tool_name}</span>
                    </div>
                    <div class="tool-image-generation-prompt">{prompt}</div>
                    <div class="tool-image-generation-status">{detail}</div>
                </div>
            }
            .into_any();
        }

        let native_detail = match &entry.request.tool_type {
            ToolRequestType::WebSearch { query } => Some(("Search", query.clone())),
            ToolRequestType::ViewImage { path } => Some(("View image", path.clone())),
            ToolRequestType::Sleep { duration_ms } => Some(("Wait", format!("{} ms", duration_ms))),
            _ => None,
        };
        if let Some((label, detail)) = native_detail {
            return view! {
                <div
                    class=format!("tool-card {status_class} native-tool")
                    data-mobile-test="tool-card-native"
                >
                    <div class="tool-card-header">
                        <span class="tool-name">{label}</span>
                    </div>
                    <div class="tool-native-detail">{detail}</div>
                </div>
            }
            .into_any();
        }
    }

    let is_completed = entry.result.is_some();
    let success = entry.result.as_ref().map(|r| r.success).unwrap_or(false);
    let result_summary = entry
        .result
        .as_ref()
        .map(|r| format!("{:?}", r.tool_result))
        .unwrap_or_default();

    // A canonical-request normalization marker pairs with the preserved `Other`
    // request. Surface a sanitized copy in every mode; never infer this state
    // from a tool name, result shape, or error prose.
    let malformed_payload = malformed_request_payload(&entry);

    let (status_class, status_icon, status_test, aria_label) = if normalization_failed {
        (
            "completed failed malformed",
            "\u{2717}",
            "tool-card-failed",
            "Tool failed: canonical data could not be normalized",
        )
    } else if is_completed {
        if success {
            (
                "completed success",
                "\u{2713}",
                "tool-card-success",
                "Tool completed successfully",
            )
        } else {
            (
                "completed failed",
                "\u{2717}",
                "tool-card-failed",
                "Tool failed",
            )
        }
    } else {
        (
            "running",
            "\u{25D4}",
            "tool-card-running",
            "Tool is running",
        )
    };

    // Failed/running cards always show their detail; otherwise honor the mode.
    let force_show = !is_completed || !success;

    view! {
        <div class=format!("tool-card {status_class}") data-mobile-test=status_test aria-label=aria_label>
            <div class="tool-card-header">
                <span class="tool-status-icon" aria-hidden="true">{status_icon}</span>
                <span class="tool-name">{tool_name}</span>
            </div>
            {malformed_payload.map(|payload| view! {
                <div
                    class="tool-typed-mismatch"
                    data-mobile-test="tool-card-malformed-note"
                    role="alert"
                >
                    "This tool call could not be normalized. A sanitized copy of the \
                     request payload is available below."
                </div>
                <details
                    class="tool-result tool-malformed-payload"
                    data-mobile-test="tool-card-malformed-payload"
                >
                    <summary>"Sanitized raw request"</summary>
                    <pre class="tool-output">{payload}</pre>
                </details>
            })}
            {
                let rs = result_summary.clone();
                let rs2 = result_summary.clone();
                let show = move || {
                    !rs.is_empty()
                        && (force_show || tool_output_mode.get() != ToolOutputMode::Summary)
                };
                view! {
                    <Show when=show>
                        // A failed or still-running tool opens its detail outright.
                        // Leaving a failure behind a closed disclosure in
                        // Summary/Compact hid the only explanation of what went
                        // wrong behind an extra tap.
                        <details
                            class="tool-result"
                            data-mobile-test="tool-card-result"
                            prop:open=move || {
                                force_show || tool_output_mode.get() == ToolOutputMode::Full
                            }
                        >
                            <summary>"Result"</summary>
                            <pre class="tool-output">{rs2.clone()}</pre>
                        </details>
                    </Show>
                }
            }
        </div>
    }
    .into_any()
}

/// The sanitized request payload paired with a typed canonical-request
/// normalization marker. Returns `None` for every unmarked completion.
///
/// Mirrors `frontend/src/components/tool_card/other.rs`.
fn malformed_request_payload(entry: &ToolRequestEntry) -> Option<String> {
    let ToolRequestType::Other { args } = &entry.request.tool_type else {
        return None;
    };
    let completion = entry.result.as_ref()?;
    if !matches!(
        completion.normalization_failure,
        Some(ToolExecutionNormalizationFailure::CanonicalRequest)
            | Some(ToolExecutionNormalizationFailure::CanonicalRequestAndResult)
    ) {
        return None;
    }
    log::error!(
        "tool card: request normalization drift reached an untyped request. A \
         sanitized payload is surfaced in the card."
    );
    Some(sanitized_request_payload_json(args))
}

const SANITIZE_MAX_DEPTH: usize = 8;
const EMBEDDED_JSON_WRAPPER_KEYS: &[&str] = &[
    "arguments",
    "args",
    "input",
    "input_data",
    "inputData",
    "tool_input",
    "toolInput",
    "parameters",
    "params",
];

fn sanitized_request_payload_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(&sanitize_request_payload(value, 0))
        .unwrap_or_else(|_| "\"[SANITIZATION FAILED]\"".to_owned())
}

fn sanitize_request_payload(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    if depth > SANITIZE_MAX_DEPTH {
        return serde_json::Value::String("[REDACTED: MAX DEPTH]".to_owned());
    }
    match value {
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let sanitized = if is_secret_key(key) {
                        serde_json::Value::String("[REDACTED]".to_owned())
                    } else if EMBEDDED_JSON_WRAPPER_KEYS.contains(&key.as_str()) {
                        sanitize_embedded_json(value, depth + 1)
                    } else {
                        sanitize_request_payload(value, depth + 1)
                    };
                    (key.clone(), sanitized)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| sanitize_request_payload(value, depth + 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sanitize_embedded_json(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    let serde_json::Value::String(text) = value else {
        return sanitize_request_payload(value, depth);
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return value.clone();
    };
    serde_json::Value::String(
        serde_json::to_string(&sanitize_request_payload(&parsed, depth))
            .unwrap_or_else(|_| "[SANITIZATION FAILED]".to_owned()),
    )
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized == "auth"
        || normalized.contains("authorization")
        || normalized.contains("bearer")
        || normalized.contains("cookie")
        || normalized.contains("apikey")
        || normalized.contains("token")
        || normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("secret")
        || normalized.contains("privatekey")
        || normalized.contains("credential")
        || normalized.contains("psk")
}

// ── Tyde orchestration ──────────────────────────────────────────────────
//
// Mirrors the desktop `tool_card::tyde_send_agent_message` and
// `tool_card::tyde_await_agents` renderers. The two frontends are separate
// crates with separate component trees, so the small view-models are duplicated
// rather than shared.
//
// Mobile has no `ToolProgress` wiring, and needs none: the typed data rides the
// `ToolRequest` event, which mobile already handles.

/// An agent's live human name, resolved from server-owned state on **the host
/// that owns the calling stream**. Falls back to the raw id — never to an
/// invented label.
///
/// The host comes from the card's `owner_agent_ref`, not from
/// `state.active_agent`. A child `AgentId` is only meaningful relative to a
/// host, and the same id can exist on two hosts; resolving against whatever
/// chat the user is currently looking at would let a card re-point at a
/// different host's agent the moment they navigate away.
fn agent_display_name(state: &AppState, owner: &AgentRef, agent_id: &AgentId) -> String {
    state
        .agents
        .with(|agents| {
            agents
                .iter()
                .find(|agent| {
                    agent.local_host_id == owner.local_host_id && agent.agent_id == *agent_id
                })
                .map(|agent| agent.name.clone())
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| agent_id.0.clone())
}

/// Live status of a watched agent, derived from server-owned state. Mirrors the
/// desktop card's derivation, including `Unknown` for an agent this client has
/// no record of — the status is never fabricated.
#[derive(Clone, Copy, PartialEq)]
enum LiveAgentStatus {
    Starting,
    Running,
    Idle,
    Failed,
    Unknown,
}

impl LiveAgentStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Idle => "Idle",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }
}

/// Live status of a watched agent on the **owning** host (see
/// [`agent_display_name`]). Keyed by `(owner host, target agent)` throughout, so
/// a same-id agent on another host can never leak its status into this card.
fn live_agent_status(state: &AppState, owner: &AgentRef, agent_id: &AgentId) -> LiveAgentStatus {
    let agent_ref = AgentRef {
        local_host_id: owner.local_host_id.clone(),
        agent_id: agent_id.clone(),
    };
    let agent = state.agents.with(|agents| {
        agents
            .iter()
            .find(|agent| agent.local_host_id == owner.local_host_id && agent.agent_id == *agent_id)
            .cloned()
    });
    match agent {
        None => LiveAgentStatus::Unknown,
        Some(agent) if agent.fatal_error.is_some() => LiveAgentStatus::Failed,
        Some(agent) if !agent.started => LiveAgentStatus::Starting,
        Some(_) => {
            let typing = state
                .agent_turn_active
                .with(|map| map.get(&agent_ref).copied().unwrap_or(false));
            let streaming = state
                .streaming_text
                .with(|map| map.contains_key(&agent_ref));
            if typing || streaming {
                LiveAgentStatus::Running
            } else {
                LiveAgentStatus::Idle
            }
        }
    }
}

fn wait_status_label(status: AgentControlStatus) -> &'static str {
    match status {
        AgentControlStatus::Thinking => "Thinking",
        AgentControlStatus::Idle => "Idle",
        AgentControlStatus::Failed => "Failed",
    }
}

/// Serialize a typed request for the `Full`-mode diagnostics disclosure. This is
/// the *typed* request — the canonical thing the server produced and the card
/// rendered — not the MCP envelope, so it is labeled accordingly.
fn typed_request_json(tool_type: &ToolRequestType) -> String {
    match serde_json::to_string_pretty(tool_type) {
        Ok(pretty) => pretty,
        Err(error) => {
            log::error!("failed to serialize typed tool request: {error}");
            format!("failed to serialize typed request: {error}")
        }
    }
}

/// The message a `tyde_send_agent_message` call delivered, rendered as Markdown
/// (the same hardened renderer chat uses) under the recipient's live name — not
/// as the escaped-JSON envelope that used to carry it twice.
///
/// **Long-content parity with desktop, stated exactly.** Desktop clamps the
/// rendered body with `overflow: hidden` and a measured `Show more` toggle, using
/// a `ResizeObserver` to re-measure on width changes. Mobile deliberately does
/// *not* copy that: it bounds the body's height and makes it **scrollable**
/// instead. That reaches the same goal — the card cannot swallow the viewport —
/// while keeping every word reachable with no measurement, no `ResizeObserver`,
/// and no focusable child ever clipped out of sight (a scroll container brings
/// focused children into view automatically). The behaviors differ; the
/// guarantees are the same: full content, always reachable, never dominating.
///
/// `ToolOutputMode` is honored the same way it is on desktop:
/// - `Summary` — the message sits behind a **closed** disclosure, so a user who
///   set Summary to quiet the conversation actually gets quiet.
/// - `Compact` — the message is shown, bounded and scrollable. No JSON.
/// - `Full` — as Compact, plus a **closed** `Typed request` disclosure.
#[component]
fn SendAgentMessageCard(
    owner_agent_ref: AgentRef,
    agent_id: AgentId,
    message: String,
    mismatch: bool,
    normalization_failed: bool,
    typed_request: String,
) -> impl IntoView {
    let state = expect_context::<AppState>();
    let mode = state.tool_output_mode;

    let recipient = Signal::derive({
        let state = state.clone();
        let owner = owner_agent_ref.clone();
        let agent_id = agent_id.clone();
        move || agent_display_name(&state, &owner, &agent_id)
    });

    // Same navigation the agents list uses: point the app at the child agent and
    // show the chat. The host is the one that owns *this card's* stream, so the
    // action lands on the agent the card is actually about — not on a same-id
    // agent belonging to whichever host the user last visited.
    let on_open = Callback::new({
        let state = state.clone();
        let owner = owner_agent_ref.clone();
        let agent_id = agent_id.clone();
        move |_: ()| {
            state.active_agent.set(Some(ActiveAgentRef {
                local_host_id: owner.local_host_id.clone(),
                agent_id: agent_id.clone(),
            }));
            state.viewing_chat.set(true);
        }
    });

    let message_html = crate::markdown::render_markdown(&message);
    let summary_html = message_html.clone();

    view! {
        <div class="send-agent-message-body" data-mobile-test="send-message-body">
            <div class="send-agent-message-to">
                <span class="send-agent-message-label">"To"</span>
                <span
                    class="send-agent-message-recipient"
                    data-mobile-test="send-message-recipient"
                >
                    {move || recipient.get()}
                </span>
                <Button
                    label="Open agent"
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Compact
                    data_mobile_test="send-message-open-agent"
                    on_click=on_open
                />
            </div>

            {move || {
                (mode.get() == ToolOutputMode::Summary).then(|| {
                    view! {
                        <details
                            class="send-agent-message-disclosure"
                            data-mobile-test="send-message-disclosure"
                        >
                            <summary>"Message"</summary>
                            <div
                                class="send-agent-message-text tool-md"
                                data-mobile-test="send-message-text"
                                inner_html=summary_html.clone()
                            ></div>
                        </details>
                    }
                })
            }}

            {move || {
                (mode.get() != ToolOutputMode::Summary).then(|| {
                    view! {
                        <div
                            class="send-agent-message-text tool-md"
                            data-mobile-test="send-message-text"
                            inner_html=message_html.clone()
                        ></div>
                    }
                })
            }}

            {normalization_failed.then(|| view! {
                <div
                    class="tool-typed-mismatch"
                    data-mobile-test="send-message-normalization-failure"
                    role="alert"
                >
                    "The canonical tool data could not be normalized."
                </div>
            })}
            {(mismatch && !normalization_failed).then(|| view! {
                <div
                    class="tool-typed-mismatch"
                    data-mobile-test="send-message-mismatch"
                    role="alert"
                >
                    "Unexpected result shape for tyde_send_agent_message. \
                     The message above is the request that was sent."
                </div>
            })}

            {move || {
                (mode.get() == ToolOutputMode::Full).then(|| {
                    view! {
                        <details
                            class="tool-result"
                            data-mobile-test="send-message-typed-request"
                        >
                            <summary>"Typed request"</summary>
                            <pre class="tool-output">{typed_request.clone()}</pre>
                        </details>
                    }
                })
            }}
        </div>
    }
}

/// What a `tyde_await_agents` call has to show right now.
#[derive(Clone)]
enum AwaitVerdict {
    /// Still waiting — the watched agents render with their live status.
    Pending,
    /// The wait returned. This is the tool's own verdict at that moment, so the
    /// statuses render verbatim rather than being re-derived from current state,
    /// which would silently rewrite history.
    Completed {
        ready: Vec<TydeAgentWaitStatus>,
        still_thinking: Vec<TydeAgentWaitStatus>,
    },
    NormalizationFailure,
    /// The request was typed but the completion was not — protocol drift.
    Mismatch,
}

#[component]
fn AwaitAgentsCard(
    owner_agent_ref: AgentRef,
    agent_ids: Vec<AgentId>,
    verdict: AwaitVerdict,
) -> impl IntoView {
    let live_rows = || {
        agent_ids
            .iter()
            .map(|agent_id| {
                view! {
                    <AwaitLiveAgentRow
                        owner_agent_ref=owner_agent_ref.clone()
                        agent_id=agent_id.clone()
                    />
                }
            })
            .collect::<Vec<_>>()
    };

    match verdict {
        AwaitVerdict::Pending => view! {
            <div class="await-agents-body" data-mobile-test="await-agents-body">
                {live_rows()}
            </div>
        }
        .into_any(),
        AwaitVerdict::Completed {
            ready,
            still_thinking,
        } => view! {
            <div class="await-agents-body" data-mobile-test="await-agents-body">
                {wait_status_group(&owner_agent_ref, "Ready", ready)}
                {wait_status_group(&owner_agent_ref, "Still thinking", still_thinking)}
            </div>
        }
        .into_any(),
        AwaitVerdict::NormalizationFailure => view! {
            <div class="await-agents-body" data-mobile-test="await-agents-body">
                <div
                    class="tool-typed-mismatch"
                    data-mobile-test="await-agents-normalization-failure"
                    role="alert"
                >
                    "The canonical tool data could not be normalized."
                </div>
                {live_rows()}
            </div>
        }
        .into_any(),
        AwaitVerdict::Mismatch => view! {
            <div class="await-agents-body" data-mobile-test="await-agents-body">
                <div
                    class="tool-typed-mismatch"
                    data-mobile-test="await-agents-mismatch"
                    role="alert"
                >
                    "Unexpected result shape for tyde_await_agents."
                </div>
                {live_rows()}
            </div>
        }
        .into_any(),
    }
}

fn wait_status_group(
    owner: &AgentRef,
    title: &'static str,
    agents: Vec<TydeAgentWaitStatus>,
) -> Option<AnyView> {
    if agents.is_empty() {
        return None;
    }
    let rows = agents
        .into_iter()
        .map(|agent| {
            view! { <AwaitResultAgentRow owner_agent_ref=owner.clone() agent=agent /> }
        })
        .collect::<Vec<_>>();
    Some(
        view! {
            <div class="await-agents-group">
                <div class="await-agents-group-title">{title}</div>
                {rows}
            </div>
        }
        .into_any(),
    )
}

#[component]
fn AwaitLiveAgentRow(owner_agent_ref: AgentRef, agent_id: AgentId) -> impl IntoView {
    let state = expect_context::<AppState>();
    let name = Signal::derive({
        let state = state.clone();
        let owner = owner_agent_ref.clone();
        let agent_id = agent_id.clone();
        move || agent_display_name(&state, &owner, &agent_id)
    });
    let status = Signal::derive({
        let state = state.clone();
        let owner = owner_agent_ref.clone();
        let agent_id = agent_id.clone();
        move || live_agent_status(&state, &owner, &agent_id)
    });

    view! {
        <div class="await-agents-row" data-mobile-test="await-agents-row">
            <span class="await-agents-name">{move || name.get()}</span>
            <span class="await-agents-status">{move || status.get().label()}</span>
        </div>
    }
}

#[component]
fn AwaitResultAgentRow(owner_agent_ref: AgentRef, agent: TydeAgentWaitStatus) -> impl IntoView {
    let state = expect_context::<AppState>();
    let status = agent.status;
    let agent_id = agent.agent_id;
    let name = Signal::derive({
        let state = state.clone();
        let owner = owner_agent_ref.clone();
        let agent_id = agent_id.clone();
        move || agent_display_name(&state, &owner, &agent_id)
    });

    view! {
        <div class="await-agents-row" data-mobile-test="await-agents-row">
            <span class="await-agents-name">{move || name.get()}</span>
            <span class="await-agents-status">{wait_status_label(status)}</span>
        </div>
    }
}

// ── AskUserQuestion ─────────────────────────────────────────────────────
//
// Mirrors the desktop `tool_card::ask_user_question` renderer. The two
// frontends are separate crates with separate component trees, so the small
// view-model / format helpers are duplicated rather than shared.

fn format_answer(questions: &[AskUserQuestion], responses: &[(Vec<usize>, String)]) -> String {
    let mut lines = Vec::new();
    for (q, (selected, custom)) in questions.iter().zip(responses) {
        let mut parts: Vec<String> = selected
            .iter()
            .filter_map(|&idx| q.options.get(idx).map(|o| o.label.clone()))
            .collect();
        let custom = custom.trim();
        if !custom.is_empty() {
            parts.push(custom.to_owned());
        }
        if parts.is_empty() {
            continue;
        }
        let label = q
            .header
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| q.question.trim());
        lines.push(format!("{label}: {}", parts.join(", ")));
    }
    lines.join("\n")
}

#[derive(Clone, Copy)]
struct QuestionState {
    selected: RwSignal<Vec<usize>>,
    custom: RwSignal<String>,
}

#[component]
fn AskUserQuestionCard(
    owner_agent_ref: AgentRef,
    questions: Vec<AskUserQuestion>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let questions = std::sync::Arc::new(questions);
    let states: std::sync::Arc<Vec<QuestionState>> = std::sync::Arc::new(
        questions
            .iter()
            .map(|_| QuestionState {
                selected: RwSignal::new(Vec::new()),
                custom: RwSignal::new(String::new()),
            })
            .collect(),
    );
    // A logical identity, not a `bool` and not a transport-attempt id: this is how
    // the card finds its own reply afterwards — including after a deliberate resend
    // has replaced the underlying record — and therefore how a later transport
    // failure ever gets back to the card that caused it.
    let submitted = RwSignal::new(None::<SubmissionOriginId>);
    let sending = RwSignal::new(false);
    let send_error = RwSignal::new(None::<String>);
    let status = ReplyStatus {
        submitted,
        sending,
        send_error,
    };

    let all_answered = {
        let states = states.clone();
        move || {
            states
                .iter()
                .all(|qs| !qs.selected.get().is_empty() || !qs.custom.get().trim().is_empty())
        }
    };

    let question_views = questions
        .iter()
        .enumerate()
        .map(|(idx, question)| render_question(question.clone(), states[idx], submitted, sending))
        .collect::<Vec<_>>();

    let on_submit = {
        let questions = questions.clone();
        let states = states.clone();
        let owner = owner_agent_ref.clone();
        let state = state.clone();
        move |_| {
            if submitted.get_untracked().is_some() || sending.get_untracked() {
                return;
            }
            send_error.set(None);
            let responses: Vec<(Vec<usize>, String)> = states
                .iter()
                .map(|qs| (qs.selected.get_untracked(), qs.custom.get_untracked()))
                .collect();
            let message = format_answer(&questions, &responses);
            if message.is_empty() {
                return;
            }
            // The answer goes back to the agent that *asked*, on its own host —
            // not to whatever chat is on screen when the user finally taps.
            let (host_id, stream) = match answer_target(&state, &owner) {
                Ok(target) => target,
                Err(message) => {
                    send_error.set(Some(message.to_owned()));
                    return;
                }
            };
            // Preflight the hard cap *before* the frame reaches the transport. Once
            // admitted it cannot be un-sent, so a cap enforced afterwards can only
            // make room by destroying a record — and the composer already refuses at
            // this gate. The answer stays on screen and the controls stay live.
            if refuse_uncapturable_reply(&state, &host_id, send_error) {
                return;
            }
            sending.set(true);
            send_answer(
                ReplyChannel {
                    state: state.clone(),
                    owner: owner.clone(),
                    host_id,
                    stream,
                },
                status,
                message,
            );
        }
    };

    let submit_disabled = {
        let all_answered = all_answered.clone();
        move || submitted.get().is_some() || sending.get() || !all_answered()
    };

    // The card's own view of what happened to its reply. A read of the record it
    // created — not a second source of truth. A `Memo` so it is `Copy` and can be
    // read from every closure in the view without cloning ceremony.
    let reply_state = {
        let state = state.clone();
        Memo::new(move |_| reply_lifecycle(&state, submitted.get()))
    };

    view! {
        <div class="ask-question-body" data-mobile-test="ask-question-body">
            {question_views}
            <div class="ask-question-actions">
                <button
                    type="button"
                    class="ui-button ui-button-primary ui-button-compact"
                    data-mobile-test="ask-question-submit"
                    disabled=submit_disabled
                    on:click=on_submit
                >
                    <span class="ui-button-label">
                        {move || {
                            if submitted.get().is_some() {
                                "Answer queued"
                            } else if sending.get() {
                                "Queueing..."
                            } else {
                                "Submit answer"
                            }
                        }}
                    </span>
                </button>
                <Show when=move || submitted.get().is_some()>
                    <span
                        class="ask-question-sent-note"
                        data-mobile-test="ask-question-sent"
                        role=move || reply_note(reply_state.get()).0
                        aria-live=move || {
                            if reply_note(reply_state.get()).0 == "alert" { "assertive" } else { "polite" }
                        }
                    >
                        {move || reply_note(reply_state.get()).1}
                    </span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="ask-question-error-note" data-mobile-test="ask-question-error" role="alert">
                        {move || send_error.get().unwrap_or_default()}
                    </span>
                </Show>
            </div>
        </div>
    }
}

fn render_question(
    question: AskUserQuestion,
    qstate: QuestionState,
    submitted: RwSignal<Option<SubmissionOriginId>>,
    sending: RwSignal<bool>,
) -> AnyView {
    let multi_select = question.multi_select;
    let header = question
        .header
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let prompt = question.question.clone();

    let option_views = question
        .options
        .iter()
        .enumerate()
        .map(|(idx, option)| {
            let selected = qstate.selected;
            let is_selected = move || selected.get().contains(&idx);
            let on_click = move |_| {
                if submitted.get_untracked().is_some() || sending.get_untracked() {
                    return;
                }
                selected.update(|current| {
                    if multi_select {
                        if let Some(pos) = current.iter().position(|&i| i == idx) {
                            current.remove(pos);
                        } else {
                            current.push(idx);
                        }
                    } else if current.first() == Some(&idx) {
                        current.clear();
                    } else {
                        current.clear();
                        current.push(idx);
                    }
                });
            };
            let label = option.label.clone();
            let description = option
                .description
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            view! {
                <button
                    class="ask-question-option"
                    data-mobile-test="ask-question-option"
                    class:selected=is_selected
                    prop:disabled=move || submitted.get().is_some() || sending.get()
                    aria-pressed=move || if is_selected() { "true" } else { "false" }
                    on:click=on_click
                >
                    <span class="ask-question-option-label">{label}</span>
                    {(!description.is_empty()).then(|| view! {
                        <span class="ask-question-option-desc">{description}</span>
                    })}
                </button>
            }
        })
        .collect::<Vec<_>>();

    let custom = qstate.custom;
    let on_custom_input = move |ev: web_sys::Event| {
        if submitted.get_untracked().is_some() || sending.get_untracked() {
            return;
        }
        custom.set(event_target_value(&ev));
    };

    view! {
        <div class="ask-question" data-mobile-test="ask-question">
            {(!header.is_empty()).then(|| view! {
                <div class="ask-question-header">{header}</div>
            })}
            <div class="ask-question-text">{prompt}</div>
            <div class="ask-question-options" class:multi=multi_select>
                {option_views}
            </div>
            <input
                class="ask-question-custom"
                data-mobile-test="ask-question-custom"
                r#type="text"
                placeholder="Or type your own answer"
                prop:disabled=move || submitted.get().is_some() || sending.get()
                on:input=on_custom_input
            />
        </div>
    }
    .into_any()
}

/// Where an interactive card's reply must be sent: the stream of the agent that
/// **asked**, on the host that owns it.
///
/// This resolves against the card's `owner_agent_ref`, never `state.active_agent`.
/// The two are the same only while the user stays put. `active_agent` moves the
/// instant they open another chat — and a question can sit unanswered for a long
/// time. Routing the reply by the *active* agent would deliver an answer, plan
/// approval, or rejection to whichever conversation happened to be on screen when
/// the user finally tapped, on whichever host that conversation lives on. The card
/// knows who asked; that is who gets the answer.
fn answer_target(
    state: &AppState,
    owner: &AgentRef,
) -> Result<(crate::state::LocalHostId, StreamPath), &'static str> {
    let stream = state.agents.with_untracked(|agents| {
        agents.iter().find_map(|agent| {
            (agent.local_host_id == owner.local_host_id && agent.agent_id == owner.agent_id)
                .then(|| agent.instance_stream.clone())
        })
    });
    let Some(stream) = stream else {
        log::error!(
            "tool card reply: no agent record for the asking agent {:?} on {:?}",
            owner.agent_id,
            owner.local_host_id
        );
        return Err("No active agent is available. Reopen the chat and try again.");
    };
    Ok((owner.local_host_id.clone(), stream))
}

/// What became of this card's reply, tracked by **logical** identity.
///
/// Keyed by `SubmissionOriginId`, not by the transport attempt's
/// `LocalSubmissionId`: a deliberate resend from the recovery list retires the old
/// attempt and mints a new one, and a card watching the *attempt* would lose sight
/// of its own reply the instant the user pressed Send again — falling back to
/// "Queued locally." while the replacement was, say, `DeliveryUnknown`.
///
/// This is a lookup by client-local UI identity. It never correlates a server
/// event with a submission, and it never claims the host received anything.
fn reply_lifecycle(state: &AppState, origin: Option<SubmissionOriginId>) -> SubmissionLifecycle {
    match origin {
        Some(origin) => state.submission_lifecycle(origin),
        None => SubmissionLifecycle::QueuedLocally,
    }
}

/// The note shown once a reply has been submitted.
///
/// It never says "sent". Admission means the frame entered this connection's
/// outbound queue — nothing more.
fn reply_note(lifecycle: SubmissionLifecycle) -> (&'static str, &'static str) {
    match lifecycle {
        SubmissionLifecycle::NotSent => (
            "alert",
            "Not sent — the connection dropped before this went out.",
        ),
        SubmissionLifecycle::DeliveryUnknown => (
            "alert",
            "May not have reached the agent — the connection dropped while it was going out.",
        ),
        // The user took it back from the recovery list. Reverting to "queued
        // locally" here — which is what happened while the card tracked the
        // transport attempt rather than the logical submission — would be a flat
        // lie about a message they had just thrown away.
        SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::Discarded) => {
            ("alert", "Discarded — this reply was never sent.")
        }
        SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::ReturnedToComposer) => (
            "alert",
            "Moved back to the message box — this reply was not sent.",
        ),
        // Queued, or retired by a broker ack. Both leave "queued locally" as the
        // last true statement the client can make.
        SubmissionLifecycle::QueuedLocally => ("status", "Queued locally."),
    }
}

/// Refuse a card reply the client could not take custody of — **before** it
/// reaches the transport.
///
/// The composer preflights the hard pending cap; the tool cards did not, so at cap
/// they would admit a frame and then hold a record over the bound, defeating the
/// gate entirely. Refusing here keeps the answer on screen, keeps the controls
/// live, and tells the user why nothing happened.
fn refuse_uncapturable_reply(
    state: &AppState,
    host_id: &crate::state::LocalHostId,
    send_error: RwSignal<Option<String>>,
) -> bool {
    if state.can_hold_submission_untracked(host_id) {
        return false;
    }
    send_error.set(Some(
        "Not sent — too many messages on this host are still unresolved. Deal with the ones \
         waiting below, then try again. Your answer is still here."
            .to_owned(),
    ));
    true
}

/// Take custody of a card's admitted reply, exactly like a composer message.
///
/// The `LocalSubmissionId` used to be thrown away (`Ok(_) =>`). That left the card
/// saying "Queued locally." forever: if the connection then died and the answer
/// never went out, nothing anywhere said so, and the user was left believing the
/// agent had their reply.
///
/// Holding it means a later `NotSent` / `DeliveryUnknown` surfaces through the
/// *existing* recovery mechanisms — the card's own note, and the agent-scoped
/// recovery list in this very chat — with Copy / Send again / Discard.
fn hold_reply(
    state: &AppState,
    owner: &AgentRef,
    accepted: crate::bridge::Accepted,
    text: String,
    tool_response: Option<SendMessageToolResponse>,
) -> SubmissionOriginId {
    let origin = state.mint_submission_origin();
    state.hold_submission(crate::state::PendingSubmission {
        local_submission_id: accepted.local_submission_id,
        // The card keeps *this*, not the attempt id, so it still recognises its own
        // reply after a deliberate resend replaces the record underneath it.
        origin,
        local_host_id: owner.local_host_id.clone(),
        connection_instance_id: accepted.connection_instance_id,
        // Ownership is known: this is the agent that asked.
        target: crate::state::SubmissionTarget::Agent(owner.clone()),
        text,
        images: Vec::new(),
        tool_response,
        state: PendingSubmissionState::QueuedLocally,
    });
    origin
}

/// Where a card's reply is going: the agent that asked, on its own host.
///
/// Bundled so the send helpers stay at a sane arity — the app state, the asking
/// agent, and the stream travel together or the reply cannot be routed at all.
#[derive(Clone)]
struct ReplyChannel {
    state: AppState,
    owner: AgentRef,
    host_id: crate::state::LocalHostId,
    stream: StreamPath,
}

/// The signals an `AskUserQuestion` card shares with its async send task.
#[derive(Clone, Copy)]
struct ReplyStatus {
    /// `Some(origin)` once the reply has been admitted. The *logical* identity —
    /// stable across a deliberate resend, which mints a new transport attempt.
    submitted: RwSignal<Option<SubmissionOriginId>>,
    sending: RwSignal<bool>,
    send_error: RwSignal<Option<String>>,
}

fn send_answer(channel: ReplyChannel, status: ReplyStatus, message: String) {
    spawn_local(async move {
        let payload = SendMessagePayload {
            message: message.clone(),
            images: None,
            origin: None,
            tool_response: None,
        };
        match crate::send::send_frame(
            &channel.host_id,
            channel.stream,
            FrameKind::SendMessage,
            &payload,
        )
        .await
        {
            Ok(accepted) => {
                let origin = hold_reply(&channel.state, &channel.owner, accepted, message, None);
                status.submitted.set(Some(origin));
                status.send_error.set(None);
            }
            Err(error) => {
                // Plain language on the failure path, and no false claim: nothing
                // was sent. (The *success* path is where "sent" would have been a
                // lie, and that is what changed.)
                log::error!("ask_user_question: failed to send answer: {error}");
                status
                    .send_error
                    .set(Some(format!("Could not send answer: {error}. Try again.")));
            }
        }
        status.sending.set(false);
    });
}

// ── ExitPlanMode ────────────────────────────────────────────────────────
//
// Mirrors the desktop `tool_card::exit_plan_mode` renderer. The two frontends
// are separate crates, so the small view-model is duplicated rather than
// shared. The decision rides the normal `SendMessage` path as a
// `SendMessageToolResponse::ExitPlanMode` tool response — no dedicated frame.

/// The reactive status signals an `ExitPlanModeCard` shares with its async send
/// task. Bundled into one `Copy` value so the decision/result of a submit can be
/// reported back without threading a long argument list through `send_decision`.
#[derive(Clone, Copy)]
struct DecisionStatus {
    decision_sent: RwSignal<Option<ExitPlanModeDecision>>,
    /// The admitted decision's *logical* identity. Kept, not discarded, so a later
    /// `NotSent` / `DeliveryUnknown` — including one on a replacement record after
    /// a deliberate resend — finds its way back to this card, instead of leaving it
    /// claiming "queued locally" forever over a decision that never reached the agent.
    submitted: RwSignal<Option<SubmissionOriginId>>,
    sending: RwSignal<bool>,
    send_error: RwSignal<Option<String>>,
}

#[component]
fn ExitPlanModeCard(
    owner_agent_ref: AgentRef,
    tool_call_id: String,
    plan: Option<String>,
    plan_path: Option<String>,
) -> impl IntoView {
    let state = expect_context::<AppState>();

    let decision_sent = RwSignal::new(None::<ExitPlanModeDecision>);
    let submitted = RwSignal::new(None::<SubmissionOriginId>);
    let sending = RwSignal::new(false);
    let send_error = RwSignal::new(None::<String>);
    let feedback = RwSignal::new(String::new());
    let status = DecisionStatus {
        decision_sent,
        submitted,
        sending,
        send_error,
    };

    // The card's own view of what happened to its decision — a reactive read of
    // the record it created, not a second source of truth.
    let reply_state = {
        let state = state.clone();
        Memo::new(move |_| reply_lifecycle(&state, submitted.get()))
    };

    let tool_call_id = std::sync::Arc::new(tool_call_id);

    let submit = {
        let state = state.clone();
        let tool_call_id = tool_call_id.clone();
        let owner = owner_agent_ref.clone();
        move |decision: ExitPlanModeDecision| {
            if decision_sent.get_untracked().is_some() || sending.get_untracked() {
                return;
            }
            send_error.set(None);
            let trimmed = feedback.get_untracked().trim().to_owned();
            let feedback = match decision {
                ExitPlanModeDecision::Reject if !trimmed.is_empty() => Some(trimmed),
                _ => None,
            };
            // The approval/rejection goes back to the agent that proposed the
            // plan, on its own host — a plan can sit unanswered while the user
            // wanders off to another chat, and the decision must still land on the
            // stream that is waiting for it.
            let (host_id, stream) = match answer_target(&state, &owner) {
                Ok(target) => target,
                Err(message) => {
                    send_error.set(Some(message.to_owned()));
                    return;
                }
            };
            // Same hard gate as the composer and the answer card: refuse before
            // admission, keep the decision on screen, keep the buttons live.
            if refuse_uncapturable_reply(&state, &host_id, send_error) {
                return;
            }
            sending.set(true);
            send_decision(
                ReplyChannel {
                    state: state.clone(),
                    owner: owner.clone(),
                    host_id,
                    stream,
                },
                status,
                (*tool_call_id).clone(),
                decision,
                feedback,
            );
        }
    };

    let on_approve = Callback::new({
        let submit = submit.clone();
        move |_: ()| submit(ExitPlanModeDecision::Approve)
    });
    let on_reject = Callback::new(move |_: ()| submit(ExitPlanModeDecision::Reject));

    let controls_disabled = move || decision_sent.get().is_some() || sending.get();

    view! {
        <div class="exit-plan-body" data-mobile-test="exit-plan-body">
            {plan_path.map(|path| view! {
                <div class="exit-plan-path">{format!("Plan: {path}")}</div>
            })}
            {plan
                .filter(|p| !p.trim().is_empty())
                .map(|plan| view! {
                    <div class="exit-plan-text" data-mobile-test="exit-plan-text">{plan}</div>
                })}
            <textarea
                class="exit-plan-feedback"
                data-mobile-test="exit-plan-feedback"
                rows="2"
                // "included if you reject", not "sent if you reject". The card
                // cannot promise anything was sent — it can only say what the
                // feedback will be attached to.
                placeholder="Optional feedback (included if you reject)"
                prop:disabled=controls_disabled
                on:input=move |ev: web_sys::Event| {
                    if decision_sent.get_untracked().is_some() || sending.get_untracked() {
                        return;
                    }
                    feedback.set(event_target_value(&ev));
                }
            />
            <div class="exit-plan-actions">
                <Button
                    label="Approve"
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Compact
                    data_mobile_test="exit-plan-approve"
                    disabled=Signal::derive(controls_disabled)
                    on_click=on_approve
                />
                <Button
                    label="Reject"
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Compact
                    data_mobile_test="exit-plan-reject"
                    disabled=Signal::derive(controls_disabled)
                    on_click=on_reject
                />
                <Show when=move || decision_sent.get().is_some()>
                    // "Queued locally", never "sent" — and once the transport
                    // reports a failure this stops saying the safe thing and says
                    // the true one instead. Discarding the submission id is what
                    // used to make that impossible.
                    <span
                        class="exit-plan-sent-note"
                        data-mobile-test="exit-plan-sent"
                        role=move || reply_note(reply_state.get()).0
                        aria-live=move || {
                            if reply_note(reply_state.get()).0 == "alert" { "assertive" } else { "polite" }
                        }
                    >
                        {move || match reply_state.get() {
                            // Anything other than "still on its way" is the
                            // transport's or the user's word, and it overrides the
                            // card's own optimistic phrasing.
                            SubmissionLifecycle::QueuedLocally => match decision_sent.get() {
                                Some(ExitPlanModeDecision::Approve) => {
                                    "Approval queued locally.".to_owned()
                                }
                                Some(ExitPlanModeDecision::Reject) => {
                                    "Rejection queued locally.".to_owned()
                                }
                                None => String::new(),
                            },
                            other => reply_note(other).1.to_owned(),
                        }}
                    </span>
                </Show>
                <Show when=move || send_error.get().is_some()>
                    <span class="exit-plan-error-note" data-mobile-test="exit-plan-error" role="alert">
                        {move || send_error.get().unwrap_or_default()}
                    </span>
                </Show>
            </div>
        </div>
    }
}

fn send_decision(
    channel: ReplyChannel,
    status: DecisionStatus,
    tool_call_id: String,
    decision: ExitPlanModeDecision,
    feedback: Option<String>,
) {
    spawn_local(async move {
        let tool_response = SendMessageToolResponse::ExitPlanMode {
            tool_call_id,
            decision,
            feedback,
        };
        let payload = SendMessagePayload {
            message: String::new(),
            images: None,
            origin: None,
            tool_response: Some(tool_response.clone()),
        };
        match crate::send::send_frame(
            &channel.host_id,
            channel.stream,
            FrameKind::SendMessage,
            &payload,
        )
        .await
        {
            Ok(accepted) => {
                // The decision *is* the payload — `message` is empty — so the
                // typed tool response has to be held with the record. A resend
                // that dropped it would put an empty chat message on the wire and
                // leave the agent still waiting for an answer.
                let origin = hold_reply(
                    &channel.state,
                    &channel.owner,
                    accepted,
                    String::new(),
                    Some(tool_response),
                );
                status.decision_sent.set(Some(decision));
                status.submitted.set(Some(origin));
                status.send_error.set(None);
            }
            Err(error) => {
                log::error!("exit_plan_mode: failed to send decision: {error}");
                status.send_error.set(Some(format!(
                    "Could not send decision: {error}. Try again."
                )));
            }
        }
        status.sending.set(false);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AskUserQuestionOption;

    fn question() -> AskUserQuestion {
        AskUserQuestion {
            id: None,
            question: "Which language?".to_owned(),
            header: Some("Language".to_owned()),
            options: vec![
                AskUserQuestionOption {
                    label: "Rust".to_owned(),
                    description: None,
                },
                AskUserQuestionOption {
                    label: "Python".to_owned(),
                    description: None,
                },
            ],
            multi_select: true,
        }
    }

    #[test]
    fn format_answer_joins_selected_and_custom() {
        let qs = vec![question()];
        let responses = vec![(vec![0usize, 1usize], "Go".to_owned())];
        assert_eq!(format_answer(&qs, &responses), "Language: Rust, Python, Go");
    }

    #[test]
    fn format_answer_falls_back_to_question_without_header() {
        let mut q = question();
        q.header = None;
        let responses = vec![(vec![0usize], String::new())];
        assert_eq!(format_answer(&[q], &responses), "Which language?: Rust");
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::{ActiveAgentRef, AgentInfo, AppState, LocalHostId, ToolRequestEntry};
    use leptos::mount::mount_to;
    use protocol::{
        AgentId, AgentOrigin, AskUserQuestion, AskUserQuestionOption, BackendKind, StreamPath,
        ToolExecutionCompletedData, ToolExecutionResult, ToolRequest, ToolRequestType,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{HtmlButtonElement, HtmlElement, HtmlInputElement};

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

    fn opt(label: &str, desc: &str) -> AskUserQuestionOption {
        AskUserQuestionOption {
            label: label.to_owned(),
            description: (!desc.is_empty()).then(|| desc.to_owned()),
        }
    }

    fn ask_entry(multi_select: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_ask".to_owned(),
                tool_name: "AskUserQuestion".to_owned(),
                tool_type: ToolRequestType::AskUserQuestion {
                    questions: vec![AskUserQuestion {
                        id: None,
                        question: "Which language?".to_owned(),
                        header: Some("Language".to_owned()),
                        options: vec![
                            opt("Rust", "Systems lang"),
                            opt("Python", "Scripting lang"),
                            opt("Go", ""),
                        ],
                        multi_select,
                    }],
                },
            },
            result: None,
        }
    }

    /// The stream these cards belong to by default: agent-1 on host-1, matching
    /// [`configure_active_agent`].
    fn owner_on(host: &str) -> AgentRef {
        AgentRef {
            local_host_id: LocalHostId(host.to_owned()),
            agent_id: AgentId("agent-1".to_owned()),
        }
    }

    fn owner_ref() -> AgentRef {
        owner_on("host-1")
    }

    fn mount_card(entry: ToolRequestEntry) -> HtmlElement {
        mount_card_with_setup(entry, |_| {})
    }

    fn mount_card_with_setup<S>(entry: ToolRequestEntry, setup: S) -> HtmlElement
    where
        S: FnOnce(&AppState) + 'static,
    {
        mount_card_owned_by(owner_ref(), entry, setup)
    }

    /// Mount a card that belongs to a specific stream. The owner is what the
    /// orchestration cards resolve child agents against, so a test can pin a card
    /// to host A and then move `active_agent` to host B.
    fn mount_card_owned_by<S>(owner: AgentRef, entry: ToolRequestEntry, setup: S) -> HtmlElement
    where
        S: FnOnce(&AppState) + 'static,
    {
        let container = make_container();
        let handle = mount_to(container.clone(), move || {
            let state = AppState::new();
            setup(&state);
            provide_context(state);
            view! { <ToolCardView owner_agent_ref=owner.clone() entry=entry.clone() /> }
        });
        handle.forget();
        container
    }

    /// Same as [`mount_card_with_setup`], but hands the state back so a test can
    /// push a later server update and assert the card re-renders.
    fn mount_card_with_state<S>(entry: ToolRequestEntry, setup: S) -> (HtmlElement, AppState)
    where
        S: FnOnce(&AppState) + 'static,
    {
        mount_card_with_state_owned_by(owner_ref(), entry, setup)
    }

    fn mount_card_with_state_owned_by<S>(
        owner: AgentRef,
        entry: ToolRequestEntry,
        setup: S,
    ) -> (HtmlElement, AppState)
    where
        S: FnOnce(&AppState) + 'static,
    {
        let state = AppState::new();
        setup(&state);
        let container = make_container();
        let mount_state = state.clone();
        let handle = mount_to(container.clone(), move || {
            provide_context(mount_state);
            view! { <ToolCardView owner_agent_ref=owner.clone() entry=entry.clone() /> }
        });
        handle.forget();
        (container, state)
    }

    fn configure_active_agent(state: &AppState) {
        let local_host_id = LocalHostId("host-1".to_owned());
        let agent_id = AgentId("agent-1".to_owned());
        state.active_agent.set(Some(ActiveAgentRef {
            local_host_id: local_host_id.clone(),
            agent_id: agent_id.clone(),
        }));
        state.agents.update(|agents| {
            agents.push(AgentInfo {
                local_host_id,
                agent_id,
                name: "Claude".to_owned(),
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
            });
        });
    }

    struct TestSendCalls {
        _guard: crate::bridge::TestSendGuard,
    }

    impl TestSendCalls {
        fn length(&self) -> u32 {
            crate::bridge::test_send_attempts() as u32
        }
    }

    fn configure_deferred_web_sends() -> TestSendCalls {
        TestSendCalls {
            _guard: crate::bridge::test_defer_sends(),
        }
    }

    fn configure_failing_web_sends() -> crate::bridge::TestSendGuard {
        crate::bridge::test_reject_sends()
    }

    /// Captures sends and, crucially, counts admission *attempts* — so a test can
    /// assert that a refused reply produced **zero** frames.
    fn configure_capturing_web_sends() -> crate::bridge::TestSendGuard {
        crate::bridge::test_capture_sends()
    }

    fn resolve_deferred_send() {
        crate::bridge::test_resolve_next_send();
    }

    fn option_buttons(container: &HtmlElement) -> Vec<HtmlButtonElement> {
        let nodes = container
            .query_selector_all("[data-mobile-test='ask-question-option']")
            .unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.get(i))
            .filter_map(|n| n.dyn_into::<HtmlButtonElement>().ok())
            .collect()
    }

    fn submit_button(container: &HtmlElement) -> HtmlButtonElement {
        container
            .query_selector("[data-mobile-test='ask-question-submit']")
            .unwrap()
            .expect("submit button")
            .dyn_into::<HtmlButtonElement>()
            .unwrap()
    }

    fn custom_input(container: &HtmlElement) -> HtmlInputElement {
        container
            .query_selector("[data-mobile-test='ask-question-custom']")
            .unwrap()
            .expect("custom input")
            .dyn_into::<HtmlInputElement>()
            .unwrap()
    }

    fn assert_answer_controls_disabled(container: &HtmlElement) {
        assert!(
            option_buttons(container)
                .iter()
                .all(HtmlButtonElement::disabled),
            "option controls should be disabled"
        );
        assert!(
            custom_input(container).disabled(),
            "custom answer input should be disabled"
        );
    }

    fn is_pressed(btn: &HtmlButtonElement) -> bool {
        btn.get_attribute("aria-pressed").as_deref() == Some("true")
    }

    fn has_sent_note(container: &HtmlElement) -> bool {
        container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .is_some()
    }

    fn error_note_text(container: &HtmlElement) -> String {
        container
            .query_selector("[data-mobile-test='ask-question-error']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn renders_interactive_card_instead_of_raw_result() {
        let container = mount_card(ask_entry(false));
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Which language?"), "question: {body}");
        assert!(body.contains("Rust") && body.contains("Python") && body.contains("Go"));
        assert_eq!(option_buttons(&container).len(), 3);
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "should not render the generic result block"
        );
    }

    #[wasm_bindgen_test]
    async fn successful_ask_user_question_still_renders_interactive_card() {
        let mut entry = ask_entry(false);
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_ask".to_owned(),
            tool_name: "AskUserQuestion".to_owned(),
            tool_result: ToolExecutionResult::Other {
                result: serde_json::json!({}),
            },
            success: true,
            error: None,
            normalization_failure: None,
        });
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-ask-question']")
                .unwrap()
                .is_some(),
            "successful AskUserQuestion should remain interactive"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='ask-question-body']")
                .unwrap()
                .is_some(),
            "interactive body should be visible"
        );
    }

    #[wasm_bindgen_test]
    async fn failed_ask_user_question_renders_error_completion() {
        let mut entry = ask_entry(false);
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_ask".to_owned(),
            tool_name: "AskUserQuestion".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "ask failed".to_owned(),
                detailed_message: "AskUserQuestion failed".to_owned(),
            },
            success: false,
            error: Some("ask failed".to_owned()),
            normalization_failure: None,
        });
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-failed']")
                .unwrap()
                .is_some(),
            "failed AskUserQuestion should render the failed completion card"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='ask-question-body']")
                .unwrap()
                .is_none(),
            "failed AskUserQuestion should not render the interactive body"
        );
    }

    #[wasm_bindgen_test]
    async fn submit_enables_after_selection() {
        let container = mount_card(ask_entry(false));
        next_tick().await;
        assert!(submit_button(&container).disabled());

        option_buttons(&container)[0].click();
        next_tick().await;
        assert!(!submit_button(&container).disabled());
    }

    #[wasm_bindgen_test]
    async fn multi_select_allows_multiple() {
        let container = mount_card(ask_entry(true));
        next_tick().await;

        let buttons = option_buttons(&container);
        buttons[0].click();
        buttons[2].click();
        next_tick().await;
        assert!(is_pressed(&buttons[0]));
        assert!(!is_pressed(&buttons[1]));
        assert!(is_pressed(&buttons[2]));
    }

    #[wasm_bindgen_test]
    async fn submit_waits_for_successful_send_before_showing_sent() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_with_setup(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "send_frame should be invoked once");
        assert!(
            submit_button(&container).disabled(),
            "disabled while sending"
        );
        assert_answer_controls_disabled(&container);
        assert!(
            !has_sent_note(&container),
            "not marked sent before send resolves"
        );

        resolve_deferred_send();
        next_tick().await;

        assert!(
            has_sent_note(&container),
            "sent note appears after send resolves"
        );
        assert_answer_controls_disabled(&container);
    }

    /// Admission is not delivery, and the card must not claim it is.
    ///
    /// Every word on the success path is about the local queue. "Sent" would be a
    /// claim the client has no basis for: the frame entered this connection's
    /// outbound queue and nothing more.
    #[wasm_bindgen_test]
    async fn an_admitted_answer_is_never_described_as_sent() {
        let _guard = configure_deferred_web_sends();
        let (container, _state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            note.contains("queued locally"),
            "an admitted answer is queued locally, and that is all it is: {note}"
        );
        assert!(
            !note.contains("sent")
                && !note.contains("delivered")
                && !note.contains("received")
                && !note.contains("answered"),
            "the client cannot know the agent got this and must never say it did: {note}"
        );
    }

    /// **The reply must not go on claiming "queued locally" after the transport
    /// says it never arrived.**
    ///
    /// The card used to throw away the `LocalSubmissionId` (`Ok(_) =>`). With no id
    /// there was nothing to match a later transport outcome against, so a
    /// connection that died before the answer went out left the card cheerfully
    /// saying "Queued locally." forever — and the user believing the agent had
    /// their reply.
    #[wasm_bindgen_test]
    async fn a_later_transport_failure_reaches_the_card_that_caused_it() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        // The reply is held like any other submission — that is what makes it
        // recoverable through the existing mechanisms.
        let held = state.pending_submissions.get_untracked();
        assert_eq!(
            held.len(),
            1,
            "an admitted reply must be held, or a later failure has nothing to attach to"
        );
        let record = held.values().next().unwrap().clone();
        assert_eq!(
            record.target,
            crate::state::SubmissionTarget::Agent(owner_ref()),
            "the reply belongs to the agent that asked — ownership is known here"
        );

        // The connection dies while it was going out.
        state.apply_submission_outcome(
            record.local_submission_id,
            record.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::DeliveryUnknown,
        );
        next_tick().await;

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .expect("the note must still be on screen");
        let text = note.text_content().unwrap_or_default().to_lowercase();
        assert!(
            !text.contains("queued locally"),
            "the card must stop saying the safe thing once the transport says otherwise: {text}"
        );
        assert!(
            text.contains("may not have reached"),
            "the user must be told the agent may never have got their answer: {text}"
        );
        assert_eq!(
            note.get_attribute("role").as_deref(),
            Some("alert"),
            "an action that may or may not have applied must interrupt, not sit politely"
        );
    }

    /// A `NotSent` reply is provably never transmitted, and says so — distinct
    /// from "cannot tell", because only one of those is safe to redo freely.
    #[wasm_bindgen_test]
    async fn a_not_sent_reply_is_named_as_a_definite_failure() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let record = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .unwrap()
            .clone();
        state.apply_submission_outcome(
            record.local_submission_id,
            record.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::NotSent,
        );
        next_tick().await;

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .unwrap();
        let text = note.text_content().unwrap_or_default().to_lowercase();
        assert!(
            text.contains("not sent"),
            "a provably-unsent reply must be named as such: {text}"
        );
        assert!(
            !text.contains("may not have reached"),
            "hedging a fact the client *does* know is its own false claim: {text}"
        );
    }

    /// A broker ack retires the record. That is a transport fact and nothing more,
    /// so the card must not upgrade it into "the agent has your answer".
    #[wasm_bindgen_test]
    async fn a_broker_ack_retires_the_record_without_claiming_delivery() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let record = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .unwrap()
            .clone();
        state.apply_submission_outcome(
            record.local_submission_id,
            record.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::BrokerAcknowledged,
        );
        next_tick().await;

        assert!(
            state.pending_submissions.get_untracked().is_empty(),
            "a broker-acknowledged reply is retired, not kept as an artifact"
        );
        let text = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .unwrap()
            .text_content()
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            text.contains("queued locally"),
            "a broker ack is not delivery — the last true statement stands: {text}"
        );
        assert!(
            !text.contains("delivered") && !text.contains("received"),
            "a PUBACK must never be promoted into a delivery claim: {text}"
        );
    }

    /// Fill a host to the hard pending cap with unresolved records.
    fn fill_to_cap(state: &AppState) {
        let host = LocalHostId("host-1".to_owned());
        for id in 0..crate::state::MAX_PENDING_SUBMISSIONS_PER_HOST as u64 {
            state.hold_submission(crate::state::PendingSubmission {
                local_submission_id: crate::bridge::LocalSubmissionId(10_000 + id),
                origin: crate::state::SubmissionOriginId(10_000 + id),
                local_host_id: host.clone(),
                connection_instance_id: 7,
                target: crate::state::SubmissionTarget::NewChat,
                text: format!("stuck {id}"),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::DeliveryUnknown,
            });
        }
    }

    /// **At the cap, a tool-card reply must not reach the transport at all.**
    ///
    /// The composer preflights the hard cap; the cards did not. So at cap a card
    /// would admit a frame and *then* hold a record over the bound — defeating the
    /// gate whose entire purpose is that the client can always take custody of what
    /// it sends. Zero frames, controls still live, answer still on screen.
    #[wasm_bindgen_test]
    async fn an_answer_at_the_pending_cap_is_refused_before_the_transport() {
        let _guard = configure_capturing_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), |state| {
            configure_active_agent(state);
            fill_to_cap(state);
        });
        next_tick().await;

        let held_before = state.pending_submissions.get_untracked().len();
        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            0,
            "at the cap the frame must never reach the transport — once admitted it \
             cannot be un-sent, and the client could not have held it"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            held_before,
            "and nothing may be held over the bound"
        );
        assert!(
            !has_sent_note(&container),
            "nothing was queued, so nothing may say it was"
        );
        assert!(
            error_note_text(&container).contains("still unresolved"),
            "the user must be told why nothing happened: {}",
            error_note_text(&container)
        );
        assert!(
            !submit_button(&container).disabled(),
            "the answer stays on screen and the controls stay live — the user has to \
             be able to submit it once they have cleared the backlog"
        );
    }

    /// Same hard gate on the plan decision, which is the more expensive one to lose.
    #[wasm_bindgen_test]
    async fn a_plan_decision_at_the_pending_cap_is_refused_before_the_transport() {
        let _guard = configure_capturing_web_sends();
        let (container, state) = mount_card_with_state(exit_plan_entry(false), |state| {
            configure_active_agent(state);
            fill_to_cap(state);
        });
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        assert_eq!(
            crate::bridge::test_send_attempts(),
            0,
            "the decision must not reach the transport at the cap"
        );
        assert_eq!(
            state.pending_submissions.get_untracked().len(),
            crate::state::MAX_PENDING_SUBMISSIONS_PER_HOST,
            "nothing held over the bound"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-sent']")
                .unwrap()
                .is_none(),
            "nothing was queued, so nothing may claim it was"
        );
        assert!(
            !exit_approve_button(&container).unwrap().disabled(),
            "the decision must remain makeable"
        );
    }

    /// **A discarded reply must never revert to "Queued locally."**
    ///
    /// The card used to watch the *transport attempt*. A discard removes that
    /// record, so the lookup found nothing and the card fell back to its
    /// happy-path phrasing — cheerfully telling the user a message they had just
    /// thrown away was on its way to the agent.
    #[wasm_bindgen_test]
    async fn a_discarded_reply_never_reverts_to_queued_locally() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let record = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .unwrap();
        state.apply_submission_outcome(
            record.local_submission_id,
            record.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::NotSent,
        );
        next_tick().await;

        // The user discards it from the recovery list.
        state.withdraw_submission(record.local_submission_id, SubmissionWithdrawal::Discarded);
        next_tick().await;

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .expect("the note must still render");
        let text = note.text_content().unwrap_or_default().to_lowercase();
        assert!(
            !text.contains("queued locally"),
            "a message the user threw away is not queued: {text}"
        );
        assert!(
            text.contains("discarded"),
            "the card must say what actually became of the reply: {text}"
        );
        assert_eq!(
            note.get_attribute("role").as_deref(),
            Some("alert"),
            "a reply that will never be sent is not a polite status"
        );
    }

    /// **A tool card that is still on screen must keep the truth about its own
    /// reply, no matter how many other messages the user has discarded since.**
    ///
    /// The tombstones were LRU-capped at 128. The justification was that an evicted
    /// entry only cost "a stale label on a very old card that has long since
    /// scrolled away" — which is simply wrong about how the transcript works. A
    /// tool card is not unmounted by scrolling past it. It stays in the DOM for the
    /// whole conversation and re-reads its lifecycle reactively, forever. So the
    /// cap did not drop unreachable facts; it dropped facts belonging to UI that
    /// was still rendered, and the card went straight back to telling the user that
    /// a message they had **discarded** was on its way to the agent.
    ///
    /// This drives past the old cap with the originating card still mounted.
    #[wasm_bindgen_test]
    async fn a_discarded_reply_stays_discarded_past_any_number_of_later_withdrawals() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let record = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .unwrap();
        let origin = record.origin;
        state.apply_submission_outcome(
            record.local_submission_id,
            record.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::NotSent,
        );
        next_tick().await;
        state.withdraw_submission(record.local_submission_id, SubmissionWithdrawal::Discarded);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='ask-question-sent']")
                .unwrap()
                .unwrap()
                .text_content()
                .unwrap_or_default()
                .to_lowercase()
                .contains("discarded"),
            "precondition: the card knows its reply was discarded"
        );

        // Now the user gets on with their life and discards a great many other
        // messages — well past the 128 the old cap allowed. This card is still
        // sitting in the transcript the whole time.
        let host = LocalHostId("host-1".to_owned());
        for id in 0..300u64 {
            let attempt = crate::bridge::LocalSubmissionId(50_000 + id);
            state.hold_submission(crate::state::PendingSubmission {
                local_submission_id: attempt,
                origin: crate::state::SubmissionOriginId(50_000 + id),
                local_host_id: host.clone(),
                connection_instance_id: 7,
                target: crate::state::SubmissionTarget::NewChat,
                text: format!("junk {id}"),
                images: Vec::new(),
                tool_response: None,
                state: PendingSubmissionState::NotSent,
            });
            state.withdraw_submission(attempt, SubmissionWithdrawal::Discarded);
        }
        next_tick().await;

        assert_eq!(
            state.submission_lifecycle(origin),
            SubmissionLifecycle::Withdrawn(SubmissionWithdrawal::Discarded),
            "the truth about a discarded message must survive 300 later discards — \
             evicting it is not 'forgetting something old', it is making a live card lie"
        );

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .expect("the card is still rendered");
        let text = note.text_content().unwrap_or_default().to_lowercase();
        assert!(
            !text.contains("queued locally"),
            "a card still on screen must never revert to claiming a discarded message \
             is on its way to the agent: {text}"
        );
        assert!(
            text.contains("discarded"),
            "it must still say what actually became of the reply: {text}"
        );
        assert_eq!(
            note.get_attribute("role").as_deref(),
            Some("alert"),
            "and it is still an alert, not a polite status"
        );
    }

    /// **After a deliberate resend, a replacement `DeliveryUnknown` must still be an
    /// alert on the originating card.**
    ///
    /// The resend retires the old attempt and mints a new one. A card watching the
    /// *attempt* id loses its own reply at that moment and reverts to "Queued
    /// locally." — and then never surfaces the replacement's failure at all. Tracking
    /// the logical submission, which the replacement inherits, is what keeps the
    /// lifecycle intact. No server event is correlated to do it.
    #[wasm_bindgen_test]
    async fn a_replacement_after_resend_still_alerts_on_the_originating_card() {
        let _guard = configure_deferred_web_sends();
        let (container, state) = mount_card_with_state(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let original = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .unwrap();
        state.apply_submission_outcome(
            original.local_submission_id,
            original.connection_instance_id,
            crate::bridge::SubmissionTransportOutcome::NotSent,
        );
        next_tick().await;

        // The user hits Send again. A new transport attempt supersedes the old one,
        // inheriting the same logical identity.
        state.hold_submission(crate::state::PendingSubmission {
            local_submission_id: crate::bridge::LocalSubmissionId(777),
            origin: original.origin,
            local_host_id: original.local_host_id.clone(),
            connection_instance_id: 9,
            target: original.target.clone(),
            text: original.text.clone(),
            images: original.images.clone(),
            tool_response: original.tool_response.clone(),
            state: PendingSubmissionState::QueuedLocally,
        });
        state.retire_submission_attempt(original.local_submission_id);
        next_tick().await;

        // …and the replacement is the one that goes ambiguous.
        state.apply_submission_outcome(
            crate::bridge::LocalSubmissionId(777),
            9,
            crate::bridge::SubmissionTransportOutcome::DeliveryUnknown,
        );
        next_tick().await;

        let note = container
            .query_selector("[data-mobile-test='ask-question-sent']")
            .unwrap()
            .expect("the note must still render");
        let text = note.text_content().unwrap_or_default().to_lowercase();
        assert!(
            text.contains("may not have reached"),
            "the replacement's failure must reach the card that originated the reply: {text}"
        );
        assert!(
            !text.contains("queued locally"),
            "the card must not lose its own reply when a resend replaces the record: {text}"
        );
        assert_eq!(
            note.get_attribute("role").as_deref(),
            Some("alert"),
            "a replacement that may or may not have applied is still an alert"
        );
    }

    #[wasm_bindgen_test]
    async fn submit_without_active_agent_shows_retryable_error() {
        let container = mount_card(ask_entry(false));
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert!(!has_sent_note(&container));
        assert!(
            error_note_text(&container).contains("No active agent"),
            "missing active-agent note should be visible"
        );
        assert!(
            !submit_button(&container).disabled(),
            "answer should remain retryable"
        );
    }

    #[wasm_bindgen_test]
    async fn send_failure_leaves_answer_retryable() {
        let _send_guard = configure_failing_web_sends();
        let container = mount_card_with_setup(ask_entry(false), configure_active_agent);
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert!(!has_sent_note(&container));
        assert!(
            error_note_text(&container).contains("Could not send answer"),
            "send failure note should be visible"
        );
        assert!(
            !submit_button(&container).disabled(),
            "answer should remain retryable after send failure"
        );
    }

    // ── ExitPlanMode ────────────────────────────────────────────────────

    fn exit_plan_entry(completed: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_type: ToolRequestType::ExitPlanMode {
                    plan: Some("Step 1: do the thing\nStep 2: verify it".to_owned()),
                    plan_path: Some("docs/plan.md".to_owned()),
                },
            },
            result: completed.then(|| ToolExecutionCompletedData {
                tool_call_id: "toolu_plan".to_owned(),
                tool_name: "ExitPlanMode".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({ "decision": "approve" }),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
        }
    }

    fn exit_approve_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector("[data-mobile-test='exit-plan-approve']")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn exit_reject_button(container: &HtmlElement) -> Option<HtmlButtonElement> {
        container
            .query_selector("[data-mobile-test='exit-plan-reject']")
            .unwrap()
            .and_then(|n| n.dyn_into::<HtmlButtonElement>().ok())
    }

    fn last_send_payload(calls: &TestSendCalls) -> String {
        let len = calls.length();
        assert!(len > 0, "expected at least one send call");
        crate::bridge::test_sent_lines()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn pending_exit_plan_renders_plan_and_controls() {
        let container = mount_card(exit_plan_entry(false));
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Step 1: do the thing"), "plan text: {body}");
        assert!(body.contains("docs/plan.md"), "plan path: {body}");
        assert!(exit_approve_button(&container).is_some(), "approve present");
        assert!(exit_reject_button(&container).is_some(), "reject present");
    }

    #[wasm_bindgen_test]
    async fn completed_exit_plan_drops_controls() {
        let container = mount_card(exit_plan_entry(true));
        next_tick().await;

        assert!(
            exit_approve_button(&container).is_none(),
            "completed plan must not keep active approve control"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-body']")
                .unwrap()
                .is_none(),
            "completed plan must not render the interactive body"
        );
    }

    #[wasm_bindgen_test]
    async fn completed_exit_plan_shows_plan_readonly() {
        let container = mount_card(exit_plan_entry(true));
        next_tick().await;

        // The plan and path remain visible, read-only, after completion.
        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Step 1: do the thing"), "plan text: {body}");
        assert!(body.contains("docs/plan.md"), "plan path: {body}");
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-readonly']")
                .unwrap()
                .is_some(),
            "completed plan should render the read-only summary"
        );
        // No interactive controls and no raw-JSON result dump.
        assert!(
            exit_approve_button(&container).is_none(),
            "read-only plan must not keep an approve control"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-feedback']")
                .unwrap()
                .is_none(),
            "read-only plan must not keep the feedback textarea"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "read-only plan must not dump the generic raw result"
        );
    }

    #[wasm_bindgen_test]
    async fn approve_sends_decision_and_disables() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_with_setup(exit_plan_entry(false), configure_active_agent);
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "one send for one click");
        let payload = last_send_payload(&calls);
        assert!(payload.contains("ExitPlanMode"), "payload: {payload}");
        assert!(
            payload.contains("approve"),
            "payload carries approve: {payload}"
        );
        assert!(
            payload.contains("toolu_plan"),
            "payload carries id: {payload}"
        );
        assert!(
            exit_approve_button(&container).unwrap().disabled(),
            "controls disabled while sending"
        );

        resolve_deferred_send();
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='exit-plan-sent']")
                .unwrap()
                .is_some(),
            "sent note appears after send resolves"
        );
    }

    #[wasm_bindgen_test]
    async fn reject_includes_feedback() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_with_setup(exit_plan_entry(false), configure_active_agent);
        next_tick().await;

        let area = container
            .query_selector("[data-mobile-test='exit-plan-feedback']")
            .unwrap()
            .expect("feedback area")
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .unwrap();
        area.set_value("use a different approach");
        area.dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
        next_tick().await;

        exit_reject_button(&container).unwrap().click();
        next_tick().await;

        let payload = last_send_payload(&calls);
        assert!(
            payload.contains("reject"),
            "payload carries reject: {payload}"
        );
        assert!(
            payload.contains("use a different approach"),
            "reject payload carries feedback: {payload}"
        );
    }

    /// **A plan decision's payload *is* the decision — `message` is empty.**
    ///
    /// So the typed tool response has to be held with the record. If it were
    /// dropped, "Send again" would put an empty chat message on the wire and the
    /// agent would still be sitting there waiting for an answer it never gets —
    /// a silent, worse failure than the one being recovered from.
    #[wasm_bindgen_test]
    async fn a_held_plan_decision_keeps_the_typed_tool_response() {
        let _guard = configure_deferred_web_sends();
        let (container, state) =
            mount_card_with_state(exit_plan_entry(false), configure_active_agent);
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;
        resolve_deferred_send();
        next_tick().await;

        let record = state
            .pending_submissions
            .get_untracked()
            .values()
            .next()
            .cloned()
            .expect("an admitted decision must be held so a later failure can surface");

        assert!(
            record.text.is_empty(),
            "precondition: a plan decision carries no message text — the payload is the decision"
        );
        match record.tool_response.as_ref() {
            Some(SendMessageToolResponse::ExitPlanMode { decision, .. }) => {
                assert_eq!(
                    *decision,
                    ExitPlanModeDecision::Approve,
                    "the held record must carry the actual decision"
                );
            }
            None => panic!(
                "the tool response must be held, or a resend degrades into an empty chat message"
            ),
        }

        // And it must not render as a blank box: the user has to see what they are
        // being asked to recover.
        assert!(
            record.display_text().to_lowercase().contains("approved"),
            "an empty-bodied decision must still describe itself: {}",
            record.display_text()
        );
        // The composer cannot hold a typed decision, so Edit must refuse it rather
        // than dropping an empty string in and losing the decision.
        assert!(
            !record.is_editable_in_composer(),
            "a plan decision is not chat text and must not be handed to the composer"
        );
    }

    /// The feedback box must not promise a send it cannot promise.
    #[wasm_bindgen_test]
    async fn the_feedback_box_makes_no_transport_claim() {
        let container = mount_card(exit_plan_entry(false));
        next_tick().await;

        let placeholder = container
            .query_selector("[data-mobile-test='exit-plan-feedback']")
            .unwrap()
            .expect("feedback box must render")
            .get_attribute("placeholder")
            .unwrap_or_default()
            .to_lowercase();
        assert!(
            !placeholder.contains("sent"),
            "the card cannot promise anything was sent — it can only say what the \
             feedback is attached to: {placeholder}"
        );
        assert!(
            placeholder.contains("included"),
            "the feedback box must still say when the feedback is used: {placeholder}"
        );
    }

    #[wasm_bindgen_test]
    async fn decision_without_active_agent_shows_retryable_error() {
        let container = mount_card(exit_plan_entry(false));
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        let err = container
            .query_selector("[data-mobile-test='exit-plan-error']")
            .unwrap()
            .and_then(|el| el.text_content())
            .unwrap_or_default();
        assert!(err.contains("No active agent"), "error note: {err}");
        assert!(
            !exit_approve_button(&container).unwrap().disabled(),
            "decision should remain retryable"
        );
    }

    // ── Malformed canonical payload ──────────────────────────────────────

    /// The exact shape a malformed canonical `tyde_send_agent_message` produces:
    /// the normalizer rejects the arguments and falls back to `Other`, while the
    /// completion (`{"ok": true}`) still normalizes to the typed ack.
    fn malformed_entry() -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_send_3".to_owned(),
                tool_name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
                tool_type: ToolRequestType::Other {
                    args: serde_json::json!({
                        "tool": "mcp__tyde-agent-control__tyde_send_agent_message",
                        "arguments": { "agent_id": "", "message": "" },
                    }),
                },
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_send_3".to_owned(),
                tool_name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
                tool_result: ToolExecutionResult::TydeSendAgentMessage,
                success: true,
                error: None,
                normalization_failure: Some(ToolExecutionNormalizationFailure::CanonicalRequest),
            }),
        }
    }

    /// Regression lock for QA D1, mobile half. The payload that failed to
    /// normalize must be inspectable in **every** mode — the whole point of the
    /// server's fallback to `Other` is that a safe request stays inspectable, and
    /// mobile's generic body only ever dumped the *result*.
    #[wasm_bindgen_test]
    async fn malformed_payload_is_inspectable_in_every_mode() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let container = mount_card_with_setup(malformed_entry(), move |state| {
                state.tool_output_mode.set(mode);
            });
            next_tick().await;

            let note = container
                .query_selector("[data-mobile-test='tool-card-malformed-note']")
                .unwrap()
                .unwrap_or_else(|| panic!("the drift is announced in {mode:?}"));
            assert_eq!(
                note.get_attribute("role").as_deref(),
                Some("alert"),
                "announced to assistive tech, not merely drawn"
            );
            let shell = container
                .query_selector("[data-mobile-test='tool-card-failed']")
                .unwrap()
                .expect("malformed shell is failed");
            assert_eq!(
                shell.get_attribute("aria-label").as_deref(),
                Some("Tool failed: canonical data could not be normalized")
            );

            let payload = container
                .query_selector("[data-mobile-test='tool-card-malformed-payload']")
                .unwrap()
                .unwrap_or_else(|| panic!("raw payload is reachable in {mode:?}"))
                .dyn_into::<web_sys::HtmlDetailsElement>()
                .expect("details element");
            assert!(
                !payload.open(),
                "it stays closed by default in {mode:?} — inspectable, not a JSON blob"
            );

            let raw = payload.text_content().unwrap_or_default();
            assert!(
                raw.contains("agent_id") && raw.contains("tyde_send_agent_message"),
                "the disclosure carries the useful payload fields in {mode:?}: {raw}"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn result_only_marker_fails_semantic_card_without_request_disclosure() {
        let mut entry = send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true);
        entry.result.as_mut().unwrap().normalization_failure =
            Some(ToolExecutionNormalizationFailure::CanonicalResult);
        let container = mount_card(entry);
        next_tick().await;

        let shell = container
            .query_selector("[data-mobile-test='tool-card-send-message']")
            .unwrap()
            .expect("semantic send card");
        assert!(shell.class_list().contains("failed"));
        assert_eq!(
            shell.get_attribute("aria-label").as_deref(),
            Some("Tool failed: canonical data could not be normalized")
        );
        assert!(
            container
                .query_selector(
                    "[data-mobile-test='send-message-normalization-failure'][role='alert']"
                )
                .unwrap()
                .is_some()
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-malformed-payload']")
                .unwrap()
                .is_none()
        );
    }

    #[wasm_bindgen_test]
    async fn combined_marker_keeps_sanitized_request_diagnostic() {
        let mut entry = malformed_entry();
        entry.result.as_mut().unwrap().normalization_failure =
            Some(ToolExecutionNormalizationFailure::CanonicalRequestAndResult);
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-malformed-note']")
                .unwrap()
                .is_some()
        );
    }

    #[wasm_bindgen_test]
    async fn malformed_payload_redacts_compound_keys_arrays_and_embedded_json() {
        let mut entry = malformed_entry();
        let ToolRequestType::Other { args } = &mut entry.request.tool_type else {
            unreachable!();
        };
        *args = serde_json::json!({
            "tool": "mcp__tyde-agent-control__tyde_send_agent_message",
            "arguments": serde_json::json!({
                "agent_id": "",
                "message": "<img src=x onerror=alert(1)>",
                "x-api-key": "secret-x",
                "OPENAI_API_KEY": "secret-openai",
                "authorization_header": "secret-auth",
                "bearer": "secret-bearer",
                "nested": [{ "github_token": "secret-github" }],
            }).to_string(),
            "array": [{ "client_secret": "secret-client" }],
        });

        let container = mount_card(entry);
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        for secret in [
            "secret-x",
            "secret-openai",
            "secret-auth",
            "secret-bearer",
            "secret-github",
            "secret-client",
        ] {
            assert!(
                !text.contains(secret),
                "secret reached mobile DOM: {secret}"
            );
        }
        assert!(text.contains("[REDACTED]"));
        assert!(container.query_selector("img").unwrap().is_none());
    }

    #[wasm_bindgen_test]
    async fn normalization_error_completion_retains_sanitized_request() {
        let mut entry = malformed_entry();
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_send_3".to_owned(),
            tool_name: "mcp__tyde-agent-control__tyde_send_agent_message".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "worker launch failed".to_owned(),
                detailed_message: "request rejected before execution".to_owned(),
            },
            success: false,
            error: None,
            normalization_failure: Some(ToolExecutionNormalizationFailure::CanonicalRequest),
        });
        let ToolRequestType::Other { args } = &mut entry.request.tool_type else {
            unreachable!();
        };
        args["arguments"]["github_token"] = serde_json::json!("never-mobile");

        let container = mount_card(entry);
        next_tick().await;
        let text = container.text_content().unwrap_or_default();
        assert!(text.contains("Sanitized raw request"));
        assert!(text.contains("github_token") && text.contains("[REDACTED]"));
        assert!(!text.contains("never-mobile"));
    }

    #[wasm_bindgen_test]
    async fn matching_error_prose_without_marker_does_not_trigger_diagnostic() {
        let mut entry = malformed_entry();
        entry.request.tool_name = "tyde_spawn_agent".to_owned();
        entry.result = Some(ToolExecutionCompletedData {
            tool_call_id: "toolu_send_3".to_owned(),
            tool_name: "tyde_spawn_agent".to_owned(),
            tool_result: ToolExecutionResult::Error {
                short_message: "worker launch failed".to_owned(),
                detailed_message: "process exited before ready".to_owned(),
            },
            success: false,
            error: Some("Failed to normalize canonical tool request".to_owned()),
            normalization_failure: None,
        });

        let container = mount_card(entry);
        next_tick().await;
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-malformed-note']")
                .unwrap()
                .is_none()
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("worker launch failed")
        );
    }

    /// A well-formed generic tool is untouched — no drift note, no raw request
    /// payload. This guards against the fix leaking JSON blobs back into normal
    /// cards.
    #[wasm_bindgen_test]
    async fn well_formed_tool_shows_no_malformed_payload() {
        let entry = ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_spawn".to_owned(),
                tool_name: "tyde_spawn_agent".to_owned(),
                tool_type: ToolRequestType::Other {
                    args: serde_json::json!({ "prompt": "do the thing" }),
                },
            },
            result: Some(ToolExecutionCompletedData {
                tool_call_id: "toolu_spawn".to_owned(),
                tool_name: "tyde_spawn_agent".to_owned(),
                tool_result: ToolExecutionResult::Other {
                    result: serde_json::json!({ "agent_id": "agent-sub" }),
                },
                success: true,
                error: None,
                normalization_failure: None,
            }),
        };
        let container = mount_card(entry);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-malformed-note']")
                .unwrap()
                .is_none(),
            "no drift note on a well-formed tool"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-malformed-payload']")
                .unwrap()
                .is_none(),
            "no raw request payload on a well-formed tool"
        );
    }

    // ── Interactive replies target the asking stream ─────────────────────
    //
    // An `AskUserQuestion` / `ExitPlanMode` card asks a question and then waits —
    // possibly for a long time, while the user wanders off to another chat, on
    // another host. Routing the reply by `state.active_agent` would deliver the
    // answer (or a plan approval) to whichever conversation happened to be on
    // screen when they finally tapped. The card knows who asked; that is who must
    // get the answer.
    //
    // The fixture puts the *asking* agent (`agent-1`) on both hosts with distinct
    // instance streams, and leaves the user looking at host-B while the card under
    // test is owned by host-A.

    const HOST_A_STREAM: &str = "/agent/host-a/agent-1/inst";
    const HOST_B_STREAM: &str = "/agent/host-b/agent-1/inst";

    fn asker_on_both_hosts_active_on_b(state: &AppState) {
        state.agents.update(|agents| {
            agents.push(child_agent_on("host-a", "agent-1", "Asker on A", true));
            agents.push(child_agent_on("host-b", "agent-1", "Asker on B", true));
        });
        state.active_agent.set(Some(ActiveAgentRef {
            local_host_id: LocalHostId("host-b".to_owned()),
            agent_id: AgentId("agent-1".to_owned()),
        }));
    }

    /// An answer goes back to the stream that asked — host-A's agent — even though
    /// the user has since opened a chat on host-B.
    #[wasm_bindgen_test]
    async fn answer_targets_the_asking_stream_not_the_active_one() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_owned_by(
            owner_on("host-a"),
            ask_entry(false),
            asker_on_both_hosts_active_on_b,
        );
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "one submit sends exactly once");
        let payload = last_send_payload(&calls);
        assert!(
            payload.contains(HOST_A_STREAM),
            "the answer is addressed to host-A's asking agent: {payload}"
        );
        assert!(
            !payload.contains(HOST_B_STREAM),
            "and never to the same-id agent on the host the user drifted to: {payload}"
        );
    }

    /// The host used for the lookup is the *owner's*, not the active one. Only
    /// host-A has a record of the asking agent; host-B has none. Under the old
    /// `state.active_agent` routing this errored with "No active agent" and sent
    /// nothing. It must now send, on host-A.
    #[wasm_bindgen_test]
    async fn answer_resolves_the_owning_host_even_when_the_active_host_has_no_record() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_owned_by(owner_on("host-a"), ask_entry(false), |state| {
            state
                .agents
                .update(|agents| agents.push(child_agent_on("host-a", "agent-1", "Asker", true)));
            // The user is on a host that knows nothing about this agent.
            state.active_agent.set(Some(ActiveAgentRef {
                local_host_id: LocalHostId("host-b".to_owned()),
                agent_id: AgentId("agent-1".to_owned()),
            }));
        });
        next_tick().await;

        option_buttons(&container)[0].click();
        next_tick().await;
        submit_button(&container).click();
        next_tick().await;

        assert_eq!(
            calls.length(),
            1,
            "the reply is sent — the owning host resolves it, so the active host \
             knowing nothing about the agent is irrelevant"
        );
        assert!(
            error_note_text(&container).is_empty(),
            "no error: the asking agent is perfectly reachable on its own host"
        );
        assert!(
            last_send_payload(&calls).contains(HOST_A_STREAM),
            "and it lands on host-A's stream"
        );
    }

    /// The same rule for a plan decision: approval goes to the agent that proposed
    /// the plan, on its host — a plan can sit unanswered for a long time.
    #[wasm_bindgen_test]
    async fn plan_decision_targets_the_proposing_stream_not_the_active_one() {
        let calls = configure_deferred_web_sends();
        let container = mount_card_owned_by(
            owner_on("host-a"),
            exit_plan_entry(false),
            asker_on_both_hosts_active_on_b,
        );
        next_tick().await;

        exit_approve_button(&container).unwrap().click();
        next_tick().await;

        assert_eq!(calls.length(), 1, "one click sends once");
        let payload = last_send_payload(&calls);
        assert!(
            payload.contains("ExitPlanMode") && payload.contains("approve"),
            "the decision is carried: {payload}"
        );
        assert!(
            payload.contains(HOST_A_STREAM),
            "addressed to the agent that proposed the plan, on host-A: {payload}"
        );
        assert!(
            !payload.contains(HOST_B_STREAM),
            "never to the same-id agent on host-B: {payload}"
        );
    }

    // ── Tyde orchestration ──────────────────────────────────────────────

    const SEND_MESSAGE: &str = "## Fixing exact rerun behavior\n\n\
        - check `mock.rs`\n\
        - then rerun\n";

    fn child_agent(agent_id: &str, name: &str) -> AgentInfo {
        child_agent_on("host-1", agent_id, name, true)
    }

    fn child_agent_on(host: &str, agent_id: &str, name: &str, started: bool) -> AgentInfo {
        AgentInfo {
            local_host_id: LocalHostId(host.to_owned()),
            agent_id: AgentId(agent_id.to_owned()),
            name: name.to_owned(),
            origin: AgentOrigin::AgentControl,
            backend_kind: BackendKind::Codex,
            workspace_roots: Vec::new(),
            project_id: None,
            parent_agent_id: Some(AgentId("agent-1".to_owned())),
            session_id: None,
            custom_agent_id: None,
            created_at_ms: 0,
            instance_stream: StreamPath(format!("/agent/{host}/{agent_id}/inst")),
            started,
            fatal_error: None,
        }
    }

    fn send_message_entry(result: Option<ToolExecutionResult>, success: bool) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_send".to_owned(),
                tool_name: "tyde_send_agent_message".to_owned(),
                tool_type: ToolRequestType::TydeSendAgentMessage {
                    agent_id: AgentId("agent-sub".to_owned()),
                    message: SEND_MESSAGE.to_owned(),
                },
            },
            result: result.map(|tool_result| ToolExecutionCompletedData {
                tool_call_id: "toolu_send".to_owned(),
                tool_name: "tyde_send_agent_message".to_owned(),
                tool_result,
                success,
                error: (!success).then(|| "send failed".to_owned()),
                normalization_failure: None,
            }),
        }
    }

    fn await_entry(result: Option<ToolExecutionResult>) -> ToolRequestEntry {
        ToolRequestEntry {
            request: ToolRequest {
                tool_call_id: "toolu_await".to_owned(),
                tool_name: "tyde_await_agents".to_owned(),
                tool_type: ToolRequestType::TydeAwaitAgents {
                    agent_ids: vec![AgentId("agent-sub".to_owned())],
                },
            },
            result: result.map(|tool_result| ToolExecutionCompletedData {
                tool_call_id: "toolu_await".to_owned(),
                tool_name: "tyde_await_agents".to_owned(),
                tool_result,
                success: true,
                error: None,
                normalization_failure: None,
            }),
        }
    }

    fn with_child_agent(state: &AppState) {
        configure_active_agent(state);
        state
            .agents
            .update(|agents| agents.push(child_agent("agent-sub", "Agent state bugs")));
    }

    /// The PWA must render the sent message semantically, not fall through to
    /// `format!("{:?}", tool_result)` — a new typed variant that silently
    /// degraded on another surface would be exactly the fallback the
    /// architecture forbids, and the compiler cannot catch it here because
    /// mobile dispatches with `if let`, not an exhaustive `match`.
    #[wasm_bindgen_test]
    async fn send_message_card_renders_markdown_not_debug_dump() {
        let container = mount_card_with_setup(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            with_child_agent,
        );
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-send-message']")
                .unwrap()
                .is_some(),
            "send card renders its own presentation"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "no generic Debug-dump result block"
        );

        // Real Markdown, not escaped JSON.
        let text = container
            .query_selector("[data-mobile-test='send-message-text']")
            .unwrap()
            .expect("message body")
            .inner_html();
        assert!(
            text.contains("<h2>"),
            "heading renders as a heading: {text}"
        );
        assert!(
            text.contains("<li>"),
            "bullets render as list items: {text}"
        );

        let body = container.text_content().unwrap_or_default();
        assert!(
            body.contains("Agent state bugs"),
            "recipient named by their live human name: {body}"
        );
        assert!(
            !body.contains("TydeSendAgentMessage"),
            "no Rust Debug dump: {body}"
        );
        assert!(!body.contains("agent_id"), "no raw JSON keys: {body}");
    }

    /// The recipient is a pure projection of server-owned state: renaming the
    /// agent re-renders the card, with no local mirror to go stale.
    #[wasm_bindgen_test]
    async fn send_message_recipient_tracks_live_state() {
        fn recipient(container: &HtmlElement) -> String {
            container
                .query_selector("[data-mobile-test='send-message-recipient']")
                .unwrap()
                .expect("recipient")
                .text_content()
                .unwrap_or_default()
        }

        let (container, state) = mount_card_with_state(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            with_child_agent,
        );
        next_tick().await;
        assert_eq!(recipient(&container), "Agent state bugs");

        state.agents.update(|agents| {
            if let Some(agent) = agents
                .iter_mut()
                .find(|agent| agent.agent_id == AgentId("agent-sub".to_owned()))
            {
                agent.name = "Renamed worker".to_owned();
            }
        });
        next_tick().await;
        assert_eq!(
            recipient(&container),
            "Renamed worker",
            "a rename re-renders the card"
        );
    }

    /// A failed send still shows its error rather than a pretty card.
    #[wasm_bindgen_test]
    async fn failed_send_message_renders_error_completion() {
        let container = mount_card_with_setup(
            send_message_entry(
                Some(ToolExecutionResult::Error {
                    short_message: "unknown agent_id".to_owned(),
                    detailed_message: "agent-sub is not a direct child".to_owned(),
                }),
                false,
            ),
            with_child_agent,
        );
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-failed']")
                .unwrap()
                .is_some(),
            "a failed send renders the failed completion card"
        );
        let body = container.text_content().unwrap_or_default();
        assert!(
            body.contains("unknown agent_id"),
            "the error stays visible: {body}"
        );
    }

    /// The await card renders the typed verdict — agent names and statuses — not
    /// a Debug dump, and no raw JSON.
    #[wasm_bindgen_test]
    async fn await_card_renders_typed_status_not_debug_dump() {
        let result = ToolExecutionResult::TydeAwaitAgents {
            ready: vec![TydeAgentWaitStatus {
                agent_id: AgentId("agent-sub".to_owned()),
                status: AgentControlStatus::Idle,
            }],
            still_thinking: Vec::new(),
        };
        let container = mount_card_with_setup(await_entry(Some(result)), with_child_agent);
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-await-agents']")
                .unwrap()
                .is_some(),
            "await card renders its own presentation"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='tool-card-result']")
                .unwrap()
                .is_none(),
            "no generic Debug-dump result block"
        );

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Ready"), "verdict group titled: {body}");
        assert!(body.contains("Agent state bugs"), "agent named: {body}");
        assert!(body.contains("Idle"), "typed status rendered: {body}");
        assert!(
            !body.contains("TydeAwaitAgents"),
            "no Rust Debug dump: {body}"
        );
        assert!(!body.contains("agent_ids"), "no raw JSON keys: {body}");
    }

    /// While the wait is pending, the watched agents show with their live status
    /// derived from server-owned state — never a fabricated one.
    #[wasm_bindgen_test]
    async fn pending_await_card_shows_live_agent_status() {
        let container = mount_card_with_setup(await_entry(None), with_child_agent);
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(body.contains("Agent state bugs"), "agent named: {body}");
        // Started, not typing, not streaming → Idle.
        assert!(body.contains("Idle"), "live status derived: {body}");
    }

    /// An agent this client has no record of renders an explicit `Unknown`
    /// rather than an invented status.
    #[wasm_bindgen_test]
    async fn pending_await_card_marks_unknown_agent() {
        let container = mount_card_with_setup(await_entry(None), configure_active_agent);
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(
            body.contains("Unknown"),
            "an agent with no record is explicitly Unknown: {body}"
        );
    }

    // ── Markdown safety (the message lands in an `inner_html` sink) ──────

    fn send_message_html(container: &HtmlElement) -> String {
        container
            .query_selector("[data-mobile-test='send-message-text']")
            .unwrap()
            .expect("message body")
            .inner_html()
    }

    fn mount_send_with_message(message: &str) -> HtmlElement {
        let mut entry = send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true);
        entry.request.tool_type = ToolRequestType::TydeSendAgentMessage {
            agent_id: AgentId("agent-sub".to_owned()),
            message: message.to_owned(),
        };
        mount_card_with_setup(entry, with_child_agent)
    }

    /// Every element inside the rendered message body.
    fn message_body_elements(container: &HtmlElement) -> Vec<web_sys::Element> {
        let body = container
            .query_selector("[data-mobile-test='send-message-text']")
            .unwrap()
            .expect("message body");
        let nodes = body.query_selector_all("*").expect("query subtree");
        (0..nodes.length())
            .filter_map(|i| nodes.get(i))
            .filter_map(|node| node.dyn_into::<web_sys::Element>().ok())
            .collect()
    }

    /// Lowercased tag names of every element in the message body.
    fn element_tag_names(container: &HtmlElement) -> Vec<String> {
        message_body_elements(container)
            .iter()
            .map(|element| element.tag_name().to_ascii_lowercase())
            .collect()
    }

    /// Every event-handler (`on*`) attribute attached to a real element anywhere in
    /// the message body.
    ///
    /// This — not a substring of a serialized string — is the security property. A
    /// handler can only run if it is an **attribute on an element**; text that
    /// merely spells `onerror=` is inert.
    fn event_handler_attributes(container: &HtmlElement) -> Vec<String> {
        message_body_elements(container)
            .iter()
            .flat_map(|element| {
                element
                    .get_attribute_names()
                    .to_vec()
                    .into_iter()
                    .filter_map(|name| name.as_string())
                    .filter(|name| name.to_ascii_lowercase().starts_with("on"))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The message is agent-authored content fed straight into `inner_html`, and
    /// agents routinely relay text they did not write. Raw HTML must land in the
    /// DOM as **inert text** — never as a live element carrying a live handler.
    ///
    /// **Asserted against DOM nodes and attributes, not against a serialized
    /// `innerHTML` string.** `innerHTML` serialization re-escapes only `&`, `<` and
    /// `>` inside a text node; it leaves `=` and `"` alone. So a correctly escaped
    /// `<img src=x onerror="alert(1)">` round-trips through `innerHTML` as the
    /// literal substring `onerror="alert(1)"` even though the DOM holds nothing but
    /// a text node — no element, no attribute, nothing executable. An earlier
    /// version of this test asserted on that substring and therefore failed on
    /// perfectly safe output: it could not tell inert text from executable markup.
    /// Element and attribute checks can, and are strictly stronger — they would
    /// also catch a handler on a tag this test never thought to name.
    #[wasm_bindgen_test]
    async fn send_message_escapes_raw_html() {
        let container = mount_send_with_message("<img src=x onerror=\"alert(1)\">\n");
        next_tick().await;

        let tags = element_tag_names(&container);
        assert!(
            !tags.contains(&"img".to_owned()),
            "no <img> element may exist in the DOM: {tags:?}"
        );
        let handlers = event_handler_attributes(&container);
        assert!(
            handlers.is_empty(),
            "no element may carry an event-handler attribute: {handlers:?}"
        );

        // A `<` survives serialization only when it opens a real tag — inside a text
        // node it comes back as `&lt;`. So this still catches genuine markup.
        let html = send_message_html(&container);
        assert!(!html.contains("<img"), "no live <img> markup: {html}");

        // And the payload is still shown to the reader, as text.
        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("<img src=x onerror="),
            "the raw HTML remains visible as escaped text: {text}"
        );
    }

    /// Same contract for raw HTML appearing *inline*, mid-sentence: escaped to
    /// text, with the surrounding prose intact.
    #[wasm_bindgen_test]
    async fn send_message_escapes_inline_html() {
        let container = mount_send_with_message("hi <svg onload=\"alert(1)\"></svg> there");
        next_tick().await;

        let tags = element_tag_names(&container);
        assert!(
            !tags.contains(&"svg".to_owned()),
            "no <svg> element may exist in the DOM: {tags:?}"
        );
        let handlers = event_handler_attributes(&container);
        assert!(
            handlers.is_empty(),
            "no element may carry an event-handler attribute: {handlers:?}"
        );

        let html = send_message_html(&container);
        assert!(!html.contains("<svg"), "no live <svg> markup: {html}");

        let text = container.text_content().unwrap_or_default();
        assert!(
            text.contains("hi") && text.contains("<svg onload=") && text.contains("there"),
            "the prose and the escaped markup both stay visible: {text}"
        );
    }

    /// A `<script>` payload is escaped to text as well: no script element enters the
    /// DOM. (`innerHTML` would not execute one anyway, but it must not be built.)
    #[wasm_bindgen_test]
    async fn send_message_escapes_script_tag() {
        let container = mount_send_with_message("<script>alert(1)</script>\n");
        next_tick().await;

        let tags = element_tag_names(&container);
        assert!(
            !tags.contains(&"script".to_owned()),
            "no <script> element may exist in the DOM: {tags:?}"
        );
        assert!(
            event_handler_attributes(&container).is_empty(),
            "no event-handler attribute may exist"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("<script>"),
            "the payload is shown to the reader as escaped text"
        );
    }

    #[wasm_bindgen_test]
    async fn send_message_rejects_javascript_link_scheme() {
        let container = mount_send_with_message("[click me](javascript:alert(1))");
        next_tick().await;

        let html = send_message_html(&container);
        assert!(
            !html.contains("javascript:"),
            "javascript: scheme must not survive: {html}"
        );
        assert!(
            container
                .query_selector("[data-mobile-test='send-message-text'] a")
                .unwrap()
                .is_none(),
            "the link wrapper is dropped entirely"
        );
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("click me"),
            "the link text is preserved as plain text"
        );
    }

    #[wasm_bindgen_test]
    async fn send_message_rejects_data_image_scheme() {
        let container =
            mount_send_with_message("![beacon](data:image/gif;base64,R0lGODlhAQABAAA=)");
        next_tick().await;

        let html = send_message_html(&container);
        assert!(!html.contains("data:image"), "data: URL dropped: {html}");
        assert!(
            container
                .query_selector("[data-mobile-test='send-message-text'] img")
                .unwrap()
                .is_none(),
            "no data: image beacon is created"
        );
    }

    #[wasm_bindgen_test]
    async fn send_message_preserves_safe_links() {
        let container = mount_send_with_message("[example](https://example.com/path)");
        next_tick().await;

        let html = send_message_html(&container);
        assert!(
            html.contains("href=\"https://example.com/path\""),
            "an https link still works: {html}"
        );
    }

    // ── Result shape, disclosure state, output modes ─────────────────────

    fn details(container: &HtmlElement, test_id: &str) -> Option<web_sys::HtmlDetailsElement> {
        container
            .query_selector(&format!("[data-mobile-test='{test_id}']"))
            .unwrap()
            .and_then(|node| node.dyn_into::<web_sys::HtmlDetailsElement>().ok())
    }

    /// A successful completion whose shape is not the ack means the request and
    /// result normalizers disagree. That is protocol drift and must be visible,
    /// not silently rendered as though everything were fine.
    #[wasm_bindgen_test]
    async fn send_message_mismatched_success_result_alerts() {
        let container = mount_card_with_setup(
            send_message_entry(
                Some(ToolExecutionResult::Other {
                    result: serde_json::json!({"ok": true}),
                }),
                true,
            ),
            with_child_agent,
        );
        next_tick().await;

        let alert = container
            .query_selector("[data-mobile-test='send-message-mismatch']")
            .unwrap()
            .expect("a mismatched successful result must alert");
        assert_eq!(
            alert.get_attribute("role").as_deref(),
            Some("alert"),
            "the mismatch is announced, not just drawn"
        );
        // The message itself still renders — it is the request that was sent.
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Fixing exact rerun behavior")
        );
    }

    /// A matching ack renders no alert.
    #[wasm_bindgen_test]
    async fn send_message_matching_ack_does_not_alert() {
        let container = mount_card_with_setup(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            with_child_agent,
        );
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='send-message-mismatch']")
                .unwrap()
                .is_none(),
            "the expected ack must not raise a false alarm"
        );
    }

    /// A failure must be *visibly* expanded, not merely present in the DOM behind
    /// a closed disclosure. Asserting on `text_content` would pass either way —
    /// closed `<details>` still has text — so this asserts the disclosure state.
    #[wasm_bindgen_test]
    async fn failed_tool_result_is_open_in_summary_and_compact() {
        for mode in [ToolOutputMode::Summary, ToolOutputMode::Compact] {
            let container = mount_card_with_setup(
                send_message_entry(
                    Some(ToolExecutionResult::Error {
                        short_message: "unknown agent_id".to_owned(),
                        detailed_message: "agent-sub is not a direct child".to_owned(),
                    }),
                    false,
                ),
                move |state| {
                    with_child_agent(state);
                    state.tool_output_mode.set(mode);
                },
            );
            next_tick().await;

            let result = details(&container, "tool-card-result")
                .expect("a failed tool shows its result disclosure");
            assert!(
                result.open(),
                "a failure must be expanded on sight in {mode:?}, not hidden behind a tap"
            );
        }
    }

    /// Summary quiets the card: the message goes behind a **closed** disclosure
    /// instead of rendering a wall of text on the smallest screen.
    #[wasm_bindgen_test]
    async fn summary_mode_puts_the_message_behind_a_closed_disclosure() {
        let container = mount_card_with_setup(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            |state| {
                with_child_agent(state);
                state.tool_output_mode.set(ToolOutputMode::Summary);
            },
        );
        next_tick().await;

        let disclosure = details(&container, "send-message-disclosure")
            .expect("Summary hides the message behind a disclosure");
        assert!(!disclosure.open(), "and it starts closed");
        // The recipient stays visible — Summary still answers "who was messaged".
        assert!(
            container
                .text_content()
                .unwrap_or_default()
                .contains("Agent state bugs")
        );
    }

    /// Compact shows the message with no JSON of any kind.
    #[wasm_bindgen_test]
    async fn compact_mode_shows_the_message_and_no_diagnostics() {
        let container = mount_card_with_setup(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            |state| {
                with_child_agent(state);
                state.tool_output_mode.set(ToolOutputMode::Compact);
            },
        );
        next_tick().await;

        assert!(
            container
                .query_selector("[data-mobile-test='send-message-text']")
                .unwrap()
                .is_some(),
            "the message renders inline in Compact"
        );
        assert!(
            details(&container, "send-message-typed-request").is_none(),
            "no diagnostics outside Full"
        );
        assert!(
            details(&container, "send-message-disclosure").is_none(),
            "no Summary disclosure in Compact"
        );
    }

    /// Full adds the documented diagnostics — closed by default, so they never
    /// dominate the conversation.
    #[wasm_bindgen_test]
    async fn full_mode_adds_closed_typed_request_diagnostics() {
        let container = mount_card_with_setup(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            |state| {
                with_child_agent(state);
                state.tool_output_mode.set(ToolOutputMode::Full);
            },
        );
        next_tick().await;

        let diagnostics = details(&container, "send-message-typed-request")
            .expect("Full exposes the typed request");
        assert!(!diagnostics.open(), "diagnostics start closed");
        let body = diagnostics.text_content().unwrap_or_default();
        assert!(
            body.contains("agent_id") && body.contains("message"),
            "the disclosure carries the typed request: {body}"
        );
    }

    /// The await card carries no raw JSON in any mode — Full included.
    #[wasm_bindgen_test]
    async fn await_card_has_no_raw_diagnostics_in_any_mode() {
        for mode in [
            ToolOutputMode::Summary,
            ToolOutputMode::Compact,
            ToolOutputMode::Full,
        ] {
            let result = ToolExecutionResult::TydeAwaitAgents {
                ready: vec![TydeAgentWaitStatus {
                    agent_id: AgentId("agent-sub".to_owned()),
                    status: AgentControlStatus::Idle,
                }],
                still_thinking: Vec::new(),
            };
            let container = mount_card_with_setup(await_entry(Some(result)), move |state| {
                with_child_agent(state);
                state.tool_output_mode.set(mode);
            });
            next_tick().await;

            assert!(
                container
                    .query_selector("[data-mobile-test='tool-card-result']")
                    .unwrap()
                    .is_none(),
                "no generic result dump in {mode:?}"
            );
            assert!(
                container.query_selector("pre").unwrap().is_none(),
                "no raw JSON block in {mode:?}, Full included"
            );
            let body = container.text_content().unwrap_or_default();
            assert!(
                !body.contains("agent_ids"),
                "no raw JSON keys in {mode:?}: {body}"
            );
        }
    }

    // ── Stream ownership across hosts ────────────────────────────────────
    //
    // The same child `AgentId` can exist on two hosts, under different names and
    // in different states. A card belongs to the stream that produced it, so it
    // must resolve its child against *that stream's* host — forever. Resolving
    // against `state.active_agent` instead would re-point the card at another
    // host's agent the moment the user opens a different chat: wrong name, wrong
    // status, and an Open-agent action that navigates to the wrong agent.
    //
    // These fixtures put `agent-sub` on host-A (idle, "Host A worker") and on
    // host-B (still starting, "Host B worker"), then move `active_agent` to
    // host-B while the card under test is owned by host-A.

    /// `agent-sub` exists on both hosts with different names and states, and the
    /// user is currently looking at host-B.
    fn two_hosts_active_on_b(state: &AppState) {
        state.agents.update(|agents| {
            agents.push(child_agent_on("host-a", "agent-sub", "Host A worker", true));
            agents.push(child_agent_on(
                "host-b",
                "agent-sub",
                "Host B worker",
                false,
            ));
        });
        state.active_agent.set(Some(ActiveAgentRef {
            local_host_id: LocalHostId("host-b".to_owned()),
            agent_id: AgentId("agent-1".to_owned()),
        }));
    }

    /// A **completed** send card owned by host-A keeps naming host-A's agent even
    /// though the user has navigated to host-B.
    #[wasm_bindgen_test]
    async fn completed_send_card_resolves_against_its_owning_host() {
        let container = mount_card_owned_by(
            owner_on("host-a"),
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            two_hosts_active_on_b,
        );
        next_tick().await;

        let recipient = container
            .query_selector("[data-mobile-test='send-message-recipient']")
            .unwrap()
            .expect("recipient")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            recipient, "Host A worker",
            "the card belongs to host-A's stream, so it names host-A's agent — not \
             the same-id agent on the host the user happens to be viewing"
        );
    }

    /// Same for a **streaming** (still-pending) card: ownership is fixed at the
    /// stream that is producing it, not at wherever the user drifts to while it
    /// is still in flight.
    #[wasm_bindgen_test]
    async fn streaming_send_card_resolves_against_its_owning_host() {
        let container = mount_card_owned_by(
            owner_on("host-a"),
            send_message_entry(None, false),
            two_hosts_active_on_b,
        );
        next_tick().await;

        let recipient = container
            .query_selector("[data-mobile-test='send-message-recipient']")
            .unwrap()
            .expect("recipient")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            recipient, "Host A worker",
            "an in-flight card is owned by the stream producing it"
        );
    }

    /// A card owned by host-A must not adopt host-B's agent when `active_agent`
    /// moves *after* mount. This is the regression the old
    /// `state.active_agent`-based lookup would fail: it would re-render with
    /// host-B's name and status.
    #[wasm_bindgen_test]
    async fn card_keeps_its_owner_when_active_agent_changes() {
        let (container, state) = mount_card_with_state_owned_by(
            owner_on("host-a"),
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            |state| {
                state.agents.update(|agents| {
                    agents.push(child_agent_on("host-a", "agent-sub", "Host A worker", true));
                    agents.push(child_agent_on(
                        "host-b",
                        "agent-sub",
                        "Host B worker",
                        false,
                    ));
                });
                state.active_agent.set(Some(ActiveAgentRef {
                    local_host_id: LocalHostId("host-a".to_owned()),
                    agent_id: AgentId("agent-1".to_owned()),
                }));
            },
        );
        next_tick().await;

        let recipient = |container: &HtmlElement| {
            container
                .query_selector("[data-mobile-test='send-message-recipient']")
                .unwrap()
                .expect("recipient")
                .text_content()
                .unwrap_or_default()
        };
        assert_eq!(recipient(&container), "Host A worker");

        // The user opens a chat on host-B. The card is untouched by that.
        state.active_agent.set(Some(ActiveAgentRef {
            local_host_id: LocalHostId("host-b".to_owned()),
            agent_id: AgentId("agent-1".to_owned()),
        }));
        next_tick().await;

        assert_eq!(
            recipient(&container),
            "Host A worker",
            "navigating to another host must not re-point an existing card at that \
             host's same-id agent"
        );
    }

    /// The await card's live status is keyed by (owning host, target agent) too:
    /// host-A's agent is started/idle, host-B's is still starting. A card owned by
    /// host-A must report host-A's status regardless of where the user is.
    #[wasm_bindgen_test]
    async fn await_card_status_resolves_against_its_owning_host() {
        let container =
            mount_card_owned_by(owner_on("host-a"), await_entry(None), two_hosts_active_on_b);
        next_tick().await;

        let body = container.text_content().unwrap_or_default();
        assert!(
            body.contains("Host A worker"),
            "await row names the owning host's agent: {body}"
        );
        assert!(
            body.contains("Idle"),
            "and reports that agent's status (host-A is started/idle): {body}"
        );
        assert!(
            !body.contains("Host B worker") && !body.contains("Starting"),
            "the same-id agent on host-B must not leak in: {body}"
        );
    }

    /// Open agent must navigate to the target id **on the owning host**, not on
    /// whichever host the user last viewed. Asserts both halves of the ref.
    #[wasm_bindgen_test]
    async fn open_agent_navigates_to_the_target_on_the_owning_host() {
        let (container, state) = mount_card_with_state_owned_by(
            owner_on("host-a"),
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            two_hosts_active_on_b,
        );
        next_tick().await;

        container
            .query_selector("[data-mobile-test='send-message-open-agent']")
            .unwrap()
            .expect("open-agent action")
            .dyn_into::<HtmlButtonElement>()
            .unwrap()
            .click();
        next_tick().await;

        let active = state.active_agent.get_untracked().expect("active agent");
        assert_eq!(
            active.local_host_id,
            LocalHostId("host-a".to_owned()),
            "Open agent lands on the host that owns the card's stream, even though \
             the user was viewing host-B"
        );
        assert_eq!(
            active.agent_id,
            AgentId("agent-sub".to_owned()),
            "and on the agent the card is actually about"
        );
    }

    /// The send card offers the same navigation the agents list uses, so the
    /// recipient is one tap away on mobile too.
    #[wasm_bindgen_test]
    async fn send_message_open_agent_navigates_to_the_recipient() {
        let (container, state) = mount_card_with_state(
            send_message_entry(Some(ToolExecutionResult::TydeSendAgentMessage), true),
            with_child_agent,
        );
        next_tick().await;

        let open = container
            .query_selector("[data-mobile-test='send-message-open-agent']")
            .unwrap()
            .expect("open-agent action")
            .dyn_into::<HtmlButtonElement>()
            .unwrap();
        open.click();
        next_tick().await;

        let active = state.active_agent.get_untracked().expect("active agent");
        assert_eq!(
            active.agent_id,
            AgentId("agent-sub".to_owned()),
            "tapping Open agent points the app at the recipient"
        );
        assert!(
            state.viewing_chat.get_untracked(),
            "and shows the chat view"
        );
    }
}
