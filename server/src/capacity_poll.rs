//! Host-owned polling for subscription capacity.
//!
//! Capacity used to arrive only as a side effect of conversation activity, so a
//! backend nobody had talked to recently showed no report at all. Every backend
//! with a capacity source also has an out-of-band one — a short-lived process or
//! connection answering a read-only status call — so the host collects on its
//! own schedule instead of waiting for an agent to exist.
//!
//! Nothing here sends a prompt, starts a turn, or spends model tokens. See
//! `dev-docs/31-subscription-capacity.md`.

use std::time::Duration;

use protocol::{BackendCapacityState, BackendKind};

use crate::backend::read_capacity_out_of_band;
use crate::host::{HostHandle, WeakHostHandle};

/// Kept under the 60-minute freshness threshold so a healthy backend refreshes
/// before its snapshot is marked stale, rather than flickering between the two.
const CAPACITY_POLL_INTERVAL: Duration = Duration::from_secs(45 * 60);

/// First retry delay after a failed poll. Doubles up to the base interval.
const CAPACITY_POLL_RETRY_MIN: Duration = Duration::from_secs(5 * 60);

/// Spread as a fraction of the interval. Several hosts signed in to the same
/// account start at different times and must not converge on one instant.
const CAPACITY_POLL_JITTER: f64 = 0.1;

/// Why a poll ran, for logging and for the single-flight decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapacityPollTrigger {
    Startup,
    Scheduled,
    Manual,
}

impl CapacityPollTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }
}

/// Longest a backend waits before its first poll. Small on purpose: the startup
/// poll is the whole point of the feature ("show me capacity without having to
/// start a conversation"), so it must not sit behind interval-scale jitter.
const CAPACITY_POLL_STARTUP_STAGGER: Duration = Duration::from_secs(4);

/// Deterministic 0..1 spread derived from the backend name, so one host staggers
/// its own backends instead of starting five provider processes at once.
fn spread_fraction(backend_kind: BackendKind) -> f64 {
    let seed = format!("{backend_kind:?}").bytes().fold(0u64, |acc, byte| {
        acc.wrapping_mul(31).wrapping_add(byte.into())
    });
    (seed % 1000) as f64 / 1000.0
}

/// Spread applied to a scheduled wait. Several hosts signed in to the same
/// account start at different times and must not converge on one instant.
fn jitter_for(backend_kind: BackendKind, interval: Duration) -> Duration {
    Duration::from_secs_f64(
        interval.as_secs_f64() * CAPACITY_POLL_JITTER * spread_fraction(backend_kind),
    )
}

/// Retry delay after `failures` consecutive failures: 5m, 10m, 20m, 40m, then
/// held at the base interval.
fn retry_delay(failures: u32) -> Duration {
    CAPACITY_POLL_RETRY_MIN
        .saturating_mul(1u32 << (failures.clamp(1, 4) - 1))
        .min(CAPACITY_POLL_INTERVAL)
}

/// Runs one backend's poll loop until the host shuts down.
///
/// One task per backend so a slow or wedged provider delays only its own next
/// poll. `poll_once` refuses to overlap, so a manual refresh landing mid-poll
/// coalesces into the one already running instead of spawning a second process.
pub(crate) async fn run_backend_poll_loop(host: WeakHostHandle, backend_kind: BackendKind) {
    let mut failures: u32 = 0;
    let mut trigger = CapacityPollTrigger::Startup;
    tokio::time::sleep(Duration::from_secs_f64(
        CAPACITY_POLL_STARTUP_STAGGER.as_secs_f64() * spread_fraction(backend_kind),
    ))
    .await;
    loop {
        // The host is gone; stop rather than keep probing providers for it.
        let Some(host) = host.upgrade() else {
            return;
        };
        match poll_once(&host, backend_kind, trigger).await {
            PollOutcome::Answered => failures = 0,
            PollOutcome::Failed => failures = failures.saturating_add(1),
            // Someone else's poll is producing the reading; wait a normal
            // interval rather than counting this as either result.
            PollOutcome::Coalesced => {}
        }
        let delay = if failures == 0 {
            CAPACITY_POLL_INTERVAL
        } else {
            retry_delay(failures)
        };
        // Dropped before sleeping, so an idle loop never holds the host alive.
        drop(host);
        tokio::time::sleep(delay + jitter_for(backend_kind, delay)).await;
        trigger = CapacityPollTrigger::Scheduled;
    }
}

/// What a completed poll means for scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollOutcome {
    /// The provider answered. Wait a full interval.
    ///
    /// Covers `Unsupported` as well as `Known`: an account that cannot report
    /// quota gave a real answer, and backing off on it would respawn a provider
    /// process every few minutes to be told the same thing again.
    Answered,
    /// The reading failed and is worth retrying sooner.
    Failed,
    /// A poll was already in flight and this one coalesced into it.
    Coalesced,
}

/// Collects one report and records it.
pub(crate) async fn poll_once(
    host: &HostHandle,
    backend_kind: BackendKind,
    trigger: CapacityPollTrigger,
) -> PollOutcome {
    if !host.begin_capacity_poll(backend_kind).await {
        tracing::debug!(
            ?backend_kind,
            trigger = trigger.label(),
            "capacity poll skipped; one is already in flight"
        );
        return PollOutcome::Coalesced;
    }
    let context = host.capacity_probe_context().await;
    let state = match context {
        Some(context) => read_capacity_out_of_band(backend_kind, &context).await,
        // Backend probing is disabled on this host, so there is no source to
        // read rather than a source that failed.
        None => BackendCapacityState::Unsupported {
            reason: protocol::CapacityUnsupportedReason::ExternalProvider,
        },
    };
    let outcome = match state {
        BackendCapacityState::Known { .. } | BackendCapacityState::Unsupported { .. } => {
            PollOutcome::Answered
        }
        _ => PollOutcome::Failed,
    };
    tracing::debug!(
        ?backend_kind,
        trigger = trigger.label(),
        ?outcome,
        "capacity poll completed"
    );
    // A manual refresh always emits, even when the numbers are unchanged: the
    // user asked, and a silent no-op is indistinguishable from a dead button.
    host.record_backend_capacity_with_emit(
        backend_kind,
        state,
        trigger == CapacityPollTrigger::Manual,
    )
    .await;
    host.end_capacity_poll(backend_kind).await;
    outcome
}

/// Every installed backend that can be polled without a conversation.
pub(crate) fn pollable_backends(installed: &[BackendKind]) -> Vec<BackendKind> {
    installed
        .iter()
        .copied()
        .filter(|kind| crate::backend::supports_out_of_band_capacity(*kind))
        .collect()
}

/// Backends that report capacity when installed, but are not installed here.
pub(crate) fn uninstalled_capacity_backends(installed: &[BackendKind]) -> Vec<BackendKind> {
    [
        BackendKind::Claude,
        BackendKind::Codex,
        BackendKind::Kiro,
        BackendKind::Antigravity,
        BackendKind::Grok,
        BackendKind::Hermes,
        BackendKind::Opencode,
        BackendKind::Tycode,
    ]
    .into_iter()
    .filter(|kind| {
        crate::backend::supports_out_of_band_capacity(*kind) && !installed.contains(kind)
    })
    .collect()
}
