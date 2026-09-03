use leptos::prelude::*;
use protocol::{
    AgentId, BackendCapacitySnapshot, BackendKind, ChatEvent, MessageSender, SessionSettingValue,
    TokenUsage,
};
use serde::{Deserialize, Serialize};

use crate::components::backend_capacity::SubscriptionCapacitySection;
use crate::state::AppState;

const STORAGE_KEY: &str = "tyde.backend-statistics.v1";
const SCHEMA_VERSION: u32 = 1;
const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_REQUESTS: usize = 2_000;
const MAX_CAPACITY: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestSample {
    pub host_id: String,
    pub backend: BackendKind,
    pub model: Option<String>,
    pub reasoning_tier: Option<String>,
    pub started_at_ms: u64,
    pub status: RequestStatus,
    pub time_to_first_output_ms: Option<u64>,
    pub total_duration_ms: u64,
    pub post_first_output_tokens_per_second: Option<f64>,
    pub tokens: Option<TokenUsage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapacitySample {
    pub host_id: String,
    #[serde(flatten)]
    pub snapshot: BackendCapacitySnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryStore {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub requests: Vec<RequestSample>,
    #[serde(default)]
    pub capacity: Vec<CapacitySample>,
}

impl Default for TelemetryStore {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            requests: Vec::new(),
            capacity: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveRequest {
    backend: BackendKind,
    model: Option<String>,
    reasoning_tier: Option<String>,
    started_at_ms: u64,
    dispatch_observed: bool,
    first_output_at_ms: Option<u64>,
}

const fn schema_version() -> u32 {
    SCHEMA_VERSION
}

pub fn load_store() -> TelemetryStore {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return TelemetryStore::default();
    };
    let Ok(Some(raw)) = storage.get_item(STORAGE_KEY) else {
        return TelemetryStore::default();
    };
    let Ok(mut store) = serde_json::from_str::<TelemetryStore>(&raw) else {
        log::warn!("discarding unreadable backend statistics store");
        return TelemetryStore::default();
    };
    if store.schema_version != SCHEMA_VERSION {
        log::warn!(
            "discarding unsupported backend statistics schema {}",
            store.schema_version
        );
        return TelemetryStore::default();
    }
    prune(&mut store, now_ms());
    store
}

fn persist(store: &TelemetryStore) {
    let Some(storage) = web_sys::window().and_then(|window| window.local_storage().ok().flatten())
    else {
        return;
    };
    match serde_json::to_string(store) {
        Ok(raw) => {
            if let Err(error) = storage.set_item(STORAGE_KEY, &raw) {
                log::warn!("failed to persist backend statistics: {error:?}");
            }
        }
        Err(error) => log::warn!("failed to encode backend statistics: {error}"),
    }
}

fn prune(store: &mut TelemetryStore, now: u64) {
    let cutoff = now.saturating_sub(RETENTION_MS);
    store
        .requests
        .retain(|sample| sample.started_at_ms >= cutoff);
    store
        .capacity
        .retain(|sample| sample.snapshot.retrieved_at_ms >= cutoff);
    if store.requests.len() > MAX_REQUESTS {
        store.requests.drain(..store.requests.len() - MAX_REQUESTS);
    }
    if store.capacity.len() > MAX_CAPACITY {
        store.capacity.drain(..store.capacity.len() - MAX_CAPACITY);
    }
}

fn now_ms() -> u64 {
    js_sys::Date::now().max(0.0) as u64
}

fn request_identity(
    state: &AppState,
    host_id: &str,
    agent_id: &AgentId,
) -> Option<(BackendKind, Option<String>)> {
    let backend = state.agents.with_untracked(|agents| {
        agents
            .iter()
            .find(|agent| agent.host_id == host_id && agent.agent_id == *agent_id)
            .map(|agent| agent.backend_kind)
    })?;
    let tier = state.agent_session_settings.with_untracked(|settings| {
        let values = settings.get(agent_id)?;
        [
            "reasoning_effort",
            "reasoning",
            "complexity",
            "thinking_level",
        ]
        .iter()
        .find_map(|key| match values.0.get(*key)? {
            SessionSettingValue::String(value) if !value.trim().is_empty() => Some(value.clone()),
            SessionSettingValue::Integer(value) => Some(value.to_string()),
            _ => None,
        })
    });
    Some((backend, tier))
}

fn request_usage(event: &ChatEvent) -> Option<TokenUsage> {
    let ChatEvent::StreamEnd(end) = event else {
        return None;
    };
    end.message
        .token_usage
        .as_ref()?
        .request
        .known_usage()
        .cloned()
}

/// Observe only live dispatch, never bootstrap/history replay. No event text is
/// copied into either the active request or the persisted sample.
pub fn observe_chat_event(state: &AppState, host_id: &str, agent_id: &AgentId, event: &ChatEvent) {
    let now = now_ms();
    let key = (host_id.to_owned(), agent_id.clone());
    match event {
        ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::User) => {
            let Some((backend, reasoning_tier)) = request_identity(state, host_id, agent_id) else {
                return;
            };
            state.active_backend_requests.update(|active| {
                active.insert(
                    key,
                    ActiveRequest {
                        backend,
                        model: None,
                        reasoning_tier,
                        started_at_ms: now,
                        dispatch_observed: true,
                        first_output_at_ms: None,
                    },
                );
            });
        }
        ChatEvent::StreamStart(start) => {
            let Some((backend, reasoning_tier)) = request_identity(state, host_id, agent_id) else {
                return;
            };
            state.active_backend_requests.update(|active| {
                let request = active.entry(key).or_insert_with(|| ActiveRequest {
                    backend,
                    model: start.model.clone(),
                    reasoning_tier,
                    started_at_ms: now,
                    dispatch_observed: false,
                    first_output_at_ms: None,
                });
                request.model = start.model.clone().or_else(|| request.model.clone());
            });
        }
        ChatEvent::StreamDelta(_) => {
            state.active_backend_requests.update(|active| {
                if let Some(request) = active.get_mut(&key) {
                    request.first_output_at_ms.get_or_insert(now);
                }
            });
        }
        ChatEvent::StreamEnd(end) => finish_request(
            state,
            host_id,
            agent_id,
            RequestStatus::Completed,
            request_usage(event),
            end.message
                .model_info
                .as_ref()
                .map(|info| info.model.clone()),
            now,
        ),
        ChatEvent::OperationCancelled(_) => finish_request(
            state,
            host_id,
            agent_id,
            RequestStatus::Interrupted,
            None,
            None,
            now,
        ),
        ChatEvent::TypingStatusChanged(false) => finish_request(
            state,
            host_id,
            agent_id,
            RequestStatus::Failed,
            None,
            None,
            now,
        ),
        ChatEvent::MessageAdded(message) if matches!(message.sender, MessageSender::Error) => {
            finish_request(
                state,
                host_id,
                agent_id,
                RequestStatus::Failed,
                None,
                None,
                now,
            )
        }
        _ => {}
    }
}

pub fn observe_agent_failure(state: &AppState, host_id: &str, agent_id: &AgentId) {
    finish_request(
        state,
        host_id,
        agent_id,
        RequestStatus::Failed,
        None,
        None,
        now_ms(),
    );
}

fn finish_request(
    state: &AppState,
    host_id: &str,
    agent_id: &AgentId,
    status: RequestStatus,
    tokens: Option<TokenUsage>,
    final_model: Option<String>,
    now: u64,
) {
    let key = (host_id.to_owned(), agent_id.clone());
    let active = state
        .active_backend_requests
        .try_update(|active| active.remove(&key))
        .flatten();
    let Some(active) = active else { return };
    let first = active.first_output_at_ms;
    let post_seconds = first.map(|first| now.saturating_sub(first) as f64 / 1_000.0);
    let throughput = match (tokens.as_ref(), post_seconds) {
        (Some(tokens), Some(seconds)) if seconds > 0.0 => {
            Some(tokens.output_tokens as f64 / seconds)
        }
        _ => None,
    };
    let sample = RequestSample {
        host_id: host_id.to_owned(),
        backend: active.backend,
        model: final_model.or(active.model),
        reasoning_tier: active.reasoning_tier,
        started_at_ms: active.started_at_ms,
        status,
        time_to_first_output_ms: active
            .dispatch_observed
            .then(|| first.map(|first| first.saturating_sub(active.started_at_ms)))
            .flatten(),
        total_duration_ms: now.saturating_sub(active.started_at_ms),
        post_first_output_tokens_per_second: throughput,
        tokens,
    };
    state.backend_statistics.update(|store| {
        store.requests.push(sample);
        prune(store, now);
        persist(store);
    });
}

pub fn observe_capacity(state: &AppState, host_id: &str, snapshots: &[BackendCapacitySnapshot]) {
    let now = now_ms();
    state.backend_statistics.update(|store| {
        for snapshot in snapshots {
            let duplicate = store
                .capacity
                .last()
                .is_some_and(|last| last.host_id == host_id && last.snapshot == *snapshot);
            if !duplicate {
                store.capacity.push(CapacitySample {
                    host_id: host_id.to_owned(),
                    snapshot: snapshot.clone(),
                });
            }
        }
        prune(store, now);
        persist(store);
    });
}

fn backend_label(kind: BackendKind) -> String {
    format!("{kind:?}")
}

fn percentile(mut values: Vec<f64>, percentile: f64) -> Option<f64> {
    values.retain(|value| value.is_finite());
    values.sort_by(f64::total_cmp);
    let index = ((values.len().saturating_sub(1)) as f64 * percentile).round() as usize;
    values.get(index).copied()
}

fn line_points(values: &[f64]) -> String {
    let max = values.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let width = 600.0;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = if values.len() < 2 {
                width / 2.0
            } else {
                index as f64 * width / (values.len() - 1) as f64
            };
            let y = 120.0 - (value / max * 110.0);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[component]
fn MetricChart(title: &'static str, unit: &'static str, values: Vec<f64>) -> impl IntoView {
    let points = line_points(&values);
    let accessible = if values.is_empty() {
        format!("{title}: no supported samples")
    } else {
        format!(
            "{title}: {} samples, latest {:.1} {unit}",
            values.len(),
            values.last().copied().unwrap_or_default()
        )
    };
    view! {
        <section class="statistics-chart">
            <h3>{title}</h3>
            {if values.is_empty() { view! { <p class="settings-description">"No supported samples in this range."</p> }.into_any() } else { view! {
                <svg viewBox="0 0 600 130" role="img" aria-label=accessible>
                    <line x1="0" y1="120" x2="600" y2="120" />
                    <polyline points=points fill="none" />
                </svg>
            }.into_any() }}
        </section>
    }
}

#[component]
pub fn BackendStatisticsPage() -> impl IntoView {
    let state = expect_context::<AppState>();
    let backend_filter = RwSignal::new(String::new());
    let model_filter = RwSignal::new(String::new());
    let tier_filter = RwSignal::new(String::new());
    let range_days = RwSignal::new(7_u64);
    let samples_state = state.clone();
    let samples = Memo::new(move |_| {
        let Some(host_id) = samples_state.selected_host_id.get() else {
            return Vec::new();
        };
        let cutoff = now_ms().saturating_sub(range_days.get() * 24 * 60 * 60 * 1_000);
        samples_state
            .backend_statistics
            .get()
            .requests
            .into_iter()
            .filter(|sample| {
                sample.host_id == host_id
                    && sample.started_at_ms >= cutoff
                    && (backend_filter.get().is_empty()
                        || backend_label(sample.backend).to_lowercase() == backend_filter.get())
                    && (model_filter.get().is_empty()
                        || sample.model.as_deref() == Some(model_filter.get().as_str()))
                    && (tier_filter.get().is_empty()
                        || sample.reasoning_tier.as_deref() == Some(tier_filter.get().as_str()))
            })
            .collect::<Vec<_>>()
    });
    let option_state = state.clone();
    let options = Memo::new(move |_| {
        let host = option_state.selected_host_id.get();
        let requests = option_state.backend_statistics.get().requests;
        let mut backends = requests
            .iter()
            .filter(|s| Some(&s.host_id) == host.as_ref())
            .map(|s| backend_label(s.backend).to_lowercase())
            .collect::<Vec<_>>();
        let mut models = requests
            .iter()
            .filter(|s| Some(&s.host_id) == host.as_ref())
            .filter_map(|s| s.model.clone())
            .collect::<Vec<_>>();
        let mut tiers = requests
            .iter()
            .filter(|s| Some(&s.host_id) == host.as_ref())
            .filter_map(|s| s.reasoning_tier.clone())
            .collect::<Vec<_>>();
        backends.sort();
        backends.dedup();
        models.sort();
        models.dedup();
        tiers.sort();
        tiers.dedup();
        (backends, models, tiers)
    });
    view! {
        <div class="settings-panel-header"><h2 class="settings-panel-title">"Statistics"</h2></div>
        <p class="settings-description settings-panel-intro">"Local, content-free backend measurements retained on this device for 30 days (up to 2,000 requests). They are observational only and never affect routing."</p>
        <div class="statistics-filters" aria-label="Statistics filters">
            <select aria-label="Backend" on:change=move |ev| backend_filter.set(event_target_value(&ev))><option value="">"All backends"</option>{move || options.get().0.into_iter().map(|value| view! { <option value=value.clone()>{value.clone()}</option> }).collect_view()}</select>
            <select aria-label="Model" on:change=move |ev| model_filter.set(event_target_value(&ev))><option value="">"All models"</option>{move || options.get().1.into_iter().map(|value| view! { <option value=value.clone()>{value.clone()}</option> }).collect_view()}</select>
            <select aria-label="Reasoning tier" on:change=move |ev| tier_filter.set(event_target_value(&ev))><option value="">"All reasoning tiers"</option>{move || options.get().2.into_iter().map(|value| view! { <option value=value.clone()>{value.clone()}</option> }).collect_view()}</select>
            <select aria-label="Time range" on:change=move |ev| range_days.set(event_target_value(&ev).parse().unwrap_or(7))><option value="1">"24 hours"</option><option value="7" selected>"7 days"</option><option value="30">"30 days"</option></select>
        </div>
        {move || {
            let samples = samples.get();
            if samples.is_empty() { return view! { <div class="statistics-empty"><h3>"No request samples yet"</h3><p>"Use an enabled backend on this host. Timing-only requests appear as partial samples when token counts are unavailable."</p></div> }.into_any(); }
            let completed = samples.iter().filter(|s| s.status == RequestStatus::Completed).count();
            let interrupted = samples.iter().filter(|s| s.status == RequestStatus::Interrupted).count();
            let failed = samples.iter().filter(|s| s.status == RequestStatus::Failed).count();
            let partial = samples.iter().filter(|s| s.tokens.is_none() || s.time_to_first_output_ms.is_none()).count();
            let latency = samples.iter().filter(|s| s.status == RequestStatus::Completed).filter_map(|s| s.time_to_first_output_ms.map(|v| v as f64)).collect::<Vec<_>>();
            let throughput = samples.iter().filter(|s| s.status == RequestStatus::Completed).filter_map(|s| s.post_first_output_tokens_per_second).collect::<Vec<_>>();
            let input_tokens = samples.iter().filter_map(|s| s.tokens.as_ref().map(|t| t.input_tokens as f64)).collect::<Vec<_>>();
            let cached_tokens = samples.iter().filter_map(|s| s.tokens.as_ref().and_then(|t| t.cached_prompt_tokens.map(|v| v as f64))).collect::<Vec<_>>();
            let reasoning_tokens = samples.iter().filter_map(|s| s.tokens.as_ref().and_then(|t| t.reasoning_tokens.map(|v| v as f64))).collect::<Vec<_>>();
            let output_tokens = samples.iter().filter_map(|s| s.tokens.as_ref().map(|t| t.output_tokens as f64)).collect::<Vec<_>>();
            let med = percentile(latency.clone(), 0.5).map(|v| format!("{v:.0} ms")).unwrap_or_else(|| "Unavailable".into());
            let p95 = percentile(latency.clone(), 0.95).map(|v| format!("{v:.0} ms")).unwrap_or_else(|| "Unavailable".into());
            let tp_med = percentile(throughput.clone(), 0.5).map(|v| format!("{v:.1} tok/s")).unwrap_or_else(|| "Unavailable".into());
            view! { <div class="statistics-results">
                <div class="statistics-summary" aria-label="Request summary"><div><strong>{samples.len()}</strong><span>"Samples"</span></div><div><strong>{completed}</strong><span>"Completed"</span></div><div><strong>{interrupted}</strong><span>"Interrupted"</span></div><div><strong>{failed}</strong><span>"Failed"</span></div><div><strong>{partial}</strong><span>"Partial"</span></div><div><strong>{med}</strong><span>"Median first output"</span></div><div><strong>{p95}</strong><span>"P95 first output"</span></div><div><strong>{tp_med}</strong><span>"Median throughput"</span></div></div>
                <MetricChart title="Time to first output" unit="ms" values=latency />
                <MetricChart title="Post-first-output throughput" unit="tokens/second" values=throughput />
                <MetricChart title="Reported input tokens" unit="tokens" values=input_tokens />
                <MetricChart title="Reported cached-input tokens" unit="tokens" values=cached_tokens />
                <MetricChart title="Reported reasoning tokens" unit="tokens" values=reasoning_tokens />
                <MetricChart title="Reported output tokens" unit="tokens" values=output_tokens />
                <p class="settings-description">{format!("{} provider-reported capacity snapshots retained. Current reset times and scopes follow below; unsupported states are never estimated.", state.backend_statistics.get().capacity.iter().filter(|s| Some(&s.host_id) == state.selected_host_id.get().as_ref()).count())}</p>
                <SubscriptionCapacitySection />
            </div> }.into_any()
        }}
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use leptos::mount::mount_to;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    fn container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let element: HtmlElement = document.create_element("div").unwrap().dyn_into().unwrap();
        document.body().unwrap().append_child(&element).unwrap();
        element
    }

    async fn next_tick() {
        JsFuture::from(js_sys::Promise::resolve(&wasm_bindgen::JsValue::NULL))
            .await
            .unwrap();
    }

    /// A timing-only request is still a visible partial sample, while charts
    /// whose metric is unsupported say so instead of plotting a fabricated
    /// zero. This exercises the real Leptos component in Chrome.
    #[wasm_bindgen_test]
    async fn statistics_render_partial_sparse_samples() {
        let state = AppState::new();
        state.selected_host_id.set(Some("local".into()));
        state.backend_statistics.set(TelemetryStore {
            requests: vec![RequestSample {
                host_id: "local".into(),
                backend: BackendKind::Claude,
                model: Some("sonnet".into()),
                reasoning_tier: Some("high".into()),
                started_at_ms: now_ms(),
                status: RequestStatus::Completed,
                time_to_first_output_ms: Some(420),
                total_duration_ms: 900,
                post_first_output_tokens_per_second: None,
                tokens: None,
            }],
            ..TelemetryStore::default()
        });
        let mounted_state = state.clone();
        let root = container();
        let _handle = mount_to(root.clone(), move || {
            provide_context(mounted_state.clone());
            view! { <BackendStatisticsPage /> }
        });
        next_tick().await;
        let text = root.text_content().unwrap_or_default();
        assert!(
            text.contains("1Samples"),
            "sample summary must be visible: {text}"
        );
        assert!(
            text.contains("1Partial"),
            "missing usage must be labelled partial: {text}"
        );
        assert!(
            text.contains("No supported samples in this range."),
            "unsupported charts need an honest empty state: {text}"
        );
        assert_eq!(
            root.query_selector_all("svg[role='img']").unwrap().length(),
            1,
            "only the supported latency series is charted"
        );
        root.remove();
    }
}
