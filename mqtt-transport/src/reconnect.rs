use std::time::Duration;

use thiserror::Error;

pub const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

const JITTER_NUMERATOR: u64 = 1;
const JITTER_DENOMINATOR: u64 = 4;

#[derive(Debug, Error)]
pub enum ReconnectBackoffError {
    #[error("MQTT reconnect backoff initial delay must be greater than zero")]
    InitialDelayZero,

    #[error("MQTT reconnect backoff max delay {max:?} is smaller than initial delay {initial:?}")]
    MaxBeforeInitial { initial: Duration, max: Duration },

    #[error("failed to read random bytes for MQTT reconnect jitter: {0}")]
    Random(#[from] getrandom::Error),
}

#[derive(Debug, Clone)]
pub struct MqttReconnectBackoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl MqttReconnectBackoff {
    pub fn new(initial: Duration, max: Duration) -> Result<Self, ReconnectBackoffError> {
        if initial.is_zero() {
            return Err(ReconnectBackoffError::InitialDelayZero);
        }
        if max < initial {
            return Err(ReconnectBackoffError::MaxBeforeInitial { initial, max });
        }
        Ok(Self {
            initial,
            max,
            current: initial,
        })
    }

    pub fn next_delay(&mut self) -> Result<Duration, ReconnectBackoffError> {
        let delay = jittered_delay(self.current, self.max)?;
        self.current = self.current.saturating_mul(2).min(self.max);
        Ok(delay)
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }

    pub fn current_base_delay(&self) -> Duration {
        self.current
    }
}

impl Default for MqttReconnectBackoff {
    fn default() -> Self {
        Self {
            initial: RECONNECT_INITIAL,
            max: RECONNECT_MAX,
            current: RECONNECT_INITIAL,
        }
    }
}

fn jittered_delay(base: Duration, max: Duration) -> Result<Duration, ReconnectBackoffError> {
    let base_ms = duration_millis_u64(base).max(1);
    let jitter_span = ((base_ms.saturating_mul(JITTER_NUMERATOR)) / JITTER_DENOMINATOR).max(1);
    let range = jitter_span.saturating_mul(2).saturating_add(1);
    let offset = random_u64()? % range;
    let min_ms = base_ms.saturating_sub(jitter_span);
    let delay_ms = min_ms.saturating_add(offset).max(1);
    Ok(Duration::from_millis(delay_ms).min(max))
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn random_u64() -> Result<u64, ReconnectBackoffError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_backoff_configuration() {
        assert!(matches!(
            MqttReconnectBackoff::new(Duration::ZERO, Duration::from_secs(1)),
            Err(ReconnectBackoffError::InitialDelayZero)
        ));
        assert!(matches!(
            MqttReconnectBackoff::new(Duration::from_secs(2), Duration::from_secs(1)),
            Err(ReconnectBackoffError::MaxBeforeInitial { .. })
        ));
    }

    #[test]
    fn base_delay_caps_at_max() -> Result<(), ReconnectBackoffError> {
        let mut backoff =
            MqttReconnectBackoff::new(Duration::from_millis(10), Duration::from_millis(30))?;
        assert_eq!(backoff.current_base_delay(), Duration::from_millis(10));
        let _ = backoff.next_delay()?;
        assert_eq!(backoff.current_base_delay(), Duration::from_millis(20));
        let _ = backoff.next_delay()?;
        assert_eq!(backoff.current_base_delay(), Duration::from_millis(30));
        let _ = backoff.next_delay()?;
        assert_eq!(backoff.current_base_delay(), Duration::from_millis(30));
        Ok(())
    }
}
