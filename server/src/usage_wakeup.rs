use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{
    BackendKind, CapacityBucket, CapacityBucketId, CapacityBucketStatus, CapacityMeasure,
    CapacityReport, CapacityReset, CapacityWindow, ClaudeLimitType, CodexLimitSlot,
    SessionSettingValue, SessionSettingsValues,
};
use serde::{Deserialize, Serialize};
use settings_model::UsageLimitSettings;

pub(crate) const HOURLY_INTERVAL: Duration = Duration::from_secs(60 * 60);
const HOUR_MS: u64 = 60 * 60 * 1000;
const WEEK_MINUTES: u32 = 7 * 24 * 60;
const MAX_OBSERVATION_AGE_MS: u64 = 2 * 60 * 1000;

#[derive(Clone, Debug)]
pub(crate) struct WakeupTarget {
    pub backend_kind: BackendKind,
    pub model: Option<String>,
    pub bucket: CapacityBucketId,
    pub reset_at_ms: u64,
    duration_minutes: u32,
}

impl WakeupTarget {
    pub fn session_settings(&self) -> SessionSettingsValues {
        let mut settings = SessionSettingsValues::default();
        if let Some(model) = &self.model {
            settings.0.insert(
                "model".to_owned(),
                SessionSettingValue::String(model.clone()),
            );
        }
        settings
    }
}

#[derive(Default, Serialize, Deserialize)]
struct WakeupLedger {
    last_tick_at_ms: Option<u64>,
    last_attempt_at_ms: Option<u64>,
    windows: BTreeMap<String, WindowRecord>,
}

#[derive(Default, Serialize, Deserialize)]
struct WindowRecord {
    observed_at_ms: u64,
    armed_reset_at_ms: Option<u64>,
    attempted_reset_at_ms: Option<u64>,
    cooldown_until_ms: u64,
}

pub(crate) struct WakeupStore {
    path: PathBuf,
}

impl WakeupStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn transaction<T>(
        &self,
        update: impl FnOnce(&mut WakeupLedger) -> Result<T, String>,
    ) -> Result<T, String> {
        let parent = self
            .path
            .parent()
            .ok_or("usage wake-up store has no parent")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path.with_extension("lock"))
            .map_err(|error| format!("Cannot open usage wake-up lock: {error}"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|error| format!("Cannot claim usage wake-up lock: {error}"))?;
        // Read under the file lock, not from an in-memory cache: two hosts using
        // the same data directory must share the same spending guard.
        let mut ledger = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<WakeupLedger>(&bytes)
                .map_err(|error| format!("Invalid usage wake-up ledger: {error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => WakeupLedger::default(),
            Err(error) => return Err(format!("Cannot read usage wake-up ledger: {error}")),
        };
        let result = update(&mut ledger)?;
        let bytes = serde_json::to_vec(&ledger).map_err(|error| error.to_string())?;
        crate::store::permissions::atomic_write_owner_only(&self.path, &bytes)?;
        sync_directory(parent)?;
        Ok(result)
    }

    pub fn claim_hour(&self, now_ms: u64) -> Result<bool, String> {
        self.transaction(|ledger| {
            if ledger
                .last_tick_at_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < HOUR_MS)
            {
                return Ok(false);
            }
            ledger.last_tick_at_ms = Some(now_ms);
            Ok(true)
        })
    }

    pub fn reserve_attempt(
        &self,
        now_ms: u64,
        settings: UsageLimitSettings,
        reports: &[(BackendKind, CapacityReport)],
        blocked_backends: &[BackendKind],
    ) -> Result<Option<WakeupTarget>, String> {
        self.transaction(|ledger| {
            let mut candidates = Vec::new();
            for (backend, report) in reports {
                if report.source.backend_kind() != *backend {
                    continue;
                }
                let observed_at_ms = report.observed_at_ms.unwrap_or(now_ms);
                if observed_at_ms > now_ms
                    || now_ms.saturating_sub(observed_at_ms) > MAX_OBSERVATION_AGE_MS
                {
                    continue;
                }
                let blocked = blocked_backends.contains(backend)
                    || has_blocking_limit(*backend, report, settings.stop_used_percent);
                for bucket in &report.buckets {
                    let Some(duration_minutes) = window_minutes(*backend, bucket) else {
                        continue;
                    };
                    let key = window_key(*backend, &bucket.id)?;
                    if !ledger.windows.contains_key(&key) && ledger.windows.len() >= 128 {
                        return Err("Too many usage window identities; wake-ups stopped".to_owned());
                    }
                    let record = ledger.windows.entry(key).or_default();
                    if observed_at_ms < record.observed_at_ms {
                        continue;
                    }
                    record.observed_at_ms = observed_at_ms;
                    let reset_at_ms = match bucket.reset {
                        CapacityReset::At { at_ms } => Some(at_ms),
                        CapacityReset::NotReported => None,
                    };
                    if let Some(reset) = reset_at_ms.filter(|reset| *reset > now_ms) {
                        // A provider already reporting a future deadline has a
                        // running window. Even 0% may be rounded-down real work.
                        record.armed_reset_at_ms = Some(reset);
                        continue;
                    }
                    let Some(reset) = reset_at_ms.or(record.armed_reset_at_ms) else {
                        continue;
                    };
                    if reset > now_ms
                        || record
                            .attempted_reset_at_ms
                            .is_some_and(|last| reset <= last)
                        || now_ms < record.cooldown_until_ms
                        || blocked
                        || !matches!(
                            bucket.measure,
                            CapacityMeasure::UsedPercent {
                                used_percent: 0,
                                ..
                            }
                        )
                    {
                        continue;
                    }
                    let Some(model) = wakeup_model(*backend, &bucket.id) else {
                        continue;
                    };
                    candidates.push(WakeupTarget {
                        backend_kind: *backend,
                        model,
                        bucket: bucket.id.clone(),
                        reset_at_ms: reset,
                        duration_minutes,
                    });
                }
            }
            if ledger
                .last_attempt_at_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < HOUR_MS)
            {
                return Ok(None);
            }
            candidates.sort_by_key(|candidate| {
                (
                    candidate.duration_minutes != WEEK_MINUTES,
                    candidate.reset_at_ms,
                )
            });
            let Some(target) = candidates
                .iter()
                .find(|candidate| {
                    if candidate.duration_minutes == WEEK_MINUTES {
                        settings.auto_start_weekly_windows
                    } else {
                        settings.auto_start_short_windows
                    }
                })
                .cloned()
            else {
                return Ok(None);
            };
            ledger.last_attempt_at_ms = Some(now_ms);
            for candidate in &candidates {
                if covered_by(&target, candidate) {
                    let record = ledger
                        .windows
                        .get_mut(&window_key(candidate.backend_kind, &candidate.bucket)?)
                        .ok_or("Missing usage window reservation")?;
                    record.attempted_reset_at_ms = Some(candidate.reset_at_ms);
                    record.cooldown_until_ms =
                        now_ms.saturating_add(u64::from(candidate.duration_minutes) * 60_000);
                }
            }
            // A timeout, failed spawn, or process crash consumes this attempt.
            // No caller may send until this transaction is durably committed.
            Ok(Some(target))
        })
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Cannot sync usage wake-up store directory: {error}"))
}

fn window_key(backend: BackendKind, bucket: &CapacityBucketId) -> Result<String, String> {
    serde_json::to_string(&(backend, bucket)).map_err(|error| error.to_string())
}

pub(crate) fn has_blocking_limit(
    backend: BackendKind,
    report: &CapacityReport,
    stop_at: u8,
) -> bool {
    report.buckets.iter().any(|bucket| {
        bucket.status == Some(CapacityBucketStatus::Rejected)
            || (window_minutes(backend, bucket).is_some()
                && !matches!(bucket.measure, CapacityMeasure::UsedPercent { used_percent, .. } if used_percent < stop_at))
    })
}

pub(crate) fn supports_wakeup(backend: BackendKind) -> bool {
    matches!(
        backend,
        BackendKind::Claude | BackendKind::Codex | BackendKind::Antigravity | BackendKind::Grok
    )
}

fn window_minutes(backend: BackendKind, bucket: &CapacityBucket) -> Option<u32> {
    match (&bucket.id, backend) {
        (
            CapacityBucketId::Claude {
                limit: ClaudeLimitType::FiveHour,
            },
            BackendKind::Claude,
        ) => Some(5 * 60),
        (
            CapacityBucketId::Claude {
                limit:
                    ClaudeLimitType::SevenDay
                    | ClaudeLimitType::SevenDayOpus
                    | ClaudeLimitType::SevenDaySonnet,
            },
            BackendKind::Claude,
        )
        | (CapacityBucketId::ClaudeModel { .. }, BackendKind::Claude) => Some(WEEK_MINUTES),
        (CapacityBucketId::Grok { bucket }, BackendKind::Grok) if bucket == "weekly" => {
            Some(WEEK_MINUTES)
        }
        (
            CapacityBucketId::Codex {
                slot: CodexLimitSlot::Primary | CodexLimitSlot::Secondary,
            },
            BackendKind::Codex,
        )
        | (CapacityBucketId::Antigravity { .. }, BackendKind::Antigravity) => match bucket.window {
            CapacityWindow::Rolling { duration_minutes }
                if (1..24 * 60).contains(&duration_minutes) || duration_minutes == WEEK_MINUTES =>
            {
                Some(duration_minutes)
            }
            _ => None,
        },
        _ => None,
    }
}

fn wakeup_model(backend: BackendKind, bucket: &CapacityBucketId) -> Option<Option<String>> {
    match (backend, bucket) {
        (BackendKind::Claude, CapacityBucketId::Claude { limit }) => match limit {
            ClaudeLimitType::FiveHour | ClaudeLimitType::SevenDay => Some(Some("haiku".to_owned())),
            ClaudeLimitType::SevenDaySonnet => Some(Some("sonnet".to_owned())),
            ClaudeLimitType::SevenDayOpus => Some(Some("opus".to_owned())),
            _ => None,
        },
        (BackendKind::Claude, CapacityBucketId::ClaudeModel { name }) => {
            // Preserve the existing wake-up policy independently of discovery.
            ["haiku", "sonnet", "opus", "fable"]
                .into_iter()
                .find(|model| model.eq_ignore_ascii_case(name))
                .map(|model| Some(model.to_owned()))
        }
        (BackendKind::Antigravity, CapacityBucketId::Antigravity { bucket })
            if bucket.starts_with("gemini-") =>
        {
            Some(Some("Gemini 3.7 Flash (Low)".to_owned()))
        }
        (BackendKind::Antigravity, CapacityBucketId::Antigravity { bucket })
            if bucket.starts_with("3p-") =>
        {
            Some(Some("Claude Sonnet 4.6 (Thinking)".to_owned()))
        }
        (BackendKind::Codex, CapacityBucketId::Codex { .. })
        | (BackendKind::Grok, CapacityBucketId::Grok { .. }) => Some(None),
        _ => None,
    }
}

fn covered_by(target: &WakeupTarget, other: &WakeupTarget) -> bool {
    target.backend_kind == other.backend_kind
        && (target.model == other.model
            || matches!(
                other.bucket,
                CapacityBucketId::Claude {
                    limit: ClaudeLimitType::FiveHour | ClaudeLimitType::SevenDay
                }
            ))
}
