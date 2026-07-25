use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use protocol::{DEFAULT_MAX_FRAME_BYTES, HARD_MAX_FRAME_BYTES, MAX_CLIPBOARD_FRAME_BYTES};
use quinn::{IdleTimeout, TransportConfig, VarInt};
use thiserror::Error;

const DEFAULT_MAX_BIDI_STREAMS: u32 = 9;
const STREAM_RECEIVE_WINDOW: u32 = 2 * 1024 * 1024;
const CONNECTION_RECEIVE_WINDOW: u32 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SessionLimits {
    handshake_timeout: Duration,
    operation_timeout: Duration,
    idle_timeout: Duration,
    keep_alive_interval: Duration,
    maximum_control_frame_bytes: NonZeroUsize,
    maximum_clipboard_frame_bytes: NonZeroUsize,
    maximum_bidirectional_streams: NonZeroU32,
}

impl SessionLimits {
    #[must_use]
    pub fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }

    #[must_use]
    pub fn operation_timeout(&self) -> Duration {
        self.operation_timeout
    }

    #[must_use]
    pub fn maximum_control_frame_bytes(&self) -> usize {
        self.maximum_control_frame_bytes.get()
    }

    #[must_use]
    pub fn maximum_clipboard_frame_bytes(&self) -> usize {
        self.maximum_clipboard_frame_bytes.get()
    }

    pub(crate) fn transport_config(&self) -> Result<Arc<TransportConfig>, LimitsError> {
        let idle_timeout = IdleTimeout::try_from(self.idle_timeout)
            .map_err(|_| LimitsError::IdleTimeoutTooLarge)?;
        let mut config = TransportConfig::default();
        config
            .max_concurrent_bidi_streams(VarInt::from_u32(self.maximum_bidirectional_streams.get()))
            .max_concurrent_uni_streams(VarInt::from_u32(0))
            .max_idle_timeout(Some(idle_timeout))
            .keep_alive_interval(Some(self.keep_alive_interval))
            .stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW))
            .receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW))
            .send_window(u64::from(CONNECTION_RECEIVE_WINDOW))
            .datagram_receive_buffer_size(None)
            .datagram_send_buffer_size(0);
        Ok(Arc::new(config))
    }
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            operation_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            keep_alive_interval: Duration::from_secs(10),
            maximum_control_frame_bytes: NonZeroUsize::new(DEFAULT_MAX_FRAME_BYTES)
                .unwrap_or(NonZeroUsize::MIN),
            maximum_clipboard_frame_bytes: NonZeroUsize::new(MAX_CLIPBOARD_FRAME_BYTES)
                .unwrap_or(NonZeroUsize::MIN),
            maximum_bidirectional_streams: NonZeroU32::new(DEFAULT_MAX_BIDI_STREAMS)
                .unwrap_or(NonZeroU32::MIN),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReconnectPolicy {
    maximum_attempts: NonZeroU32,
    initial_delay: Duration,
    maximum_delay: Duration,
}

impl ReconnectPolicy {
    pub fn new(
        maximum_attempts: NonZeroU32,
        initial_delay: Duration,
        maximum_delay: Duration,
    ) -> Result<Self, LimitsError> {
        if initial_delay.is_zero() || maximum_delay < initial_delay {
            return Err(LimitsError::InvalidReconnectDelay);
        }
        Ok(Self {
            maximum_attempts,
            initial_delay,
            maximum_delay,
        })
    }

    #[must_use]
    pub fn delay_before(&self, attempt: u32) -> Option<Duration> {
        if attempt >= self.maximum_attempts.get() {
            return None;
        }
        let multiplier = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        Some(
            self.initial_delay
                .saturating_mul(multiplier)
                .min(self.maximum_delay),
        )
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            maximum_attempts: NonZeroU32::new(8).unwrap_or(NonZeroU32::MIN),
            initial_delay: Duration::from_millis(250),
            maximum_delay: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Error)]
pub enum LimitsError {
    #[error("the QUIC idle timeout is too large")]
    IdleTimeoutTooLarge,
    #[error("reconnect delays must be non-zero and ordered from initial to maximum")]
    InvalidReconnectDelay,
    #[error("control frame size exceeds the protocol hard limit of {HARD_MAX_FRAME_BYTES} bytes")]
    ControlFrameTooLarge,
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, time::Duration};

    use super::ReconnectPolicy;

    #[test]
    fn reconnect_delay_is_bounded_and_attempts_stop() {
        let policy = ReconnectPolicy::new(
            NonZeroU32::new(4).unwrap_or(NonZeroU32::MIN),
            Duration::from_millis(100),
            Duration::from_millis(250),
        )
        .unwrap_or_else(|error| panic!("policy should be valid: {error}"));

        assert_eq!(policy.delay_before(0), Some(Duration::from_millis(100)));
        assert_eq!(policy.delay_before(1), Some(Duration::from_millis(200)));
        assert_eq!(policy.delay_before(2), Some(Duration::from_millis(250)));
        assert_eq!(policy.delay_before(3), Some(Duration::from_millis(250)));
        assert_eq!(policy.delay_before(4), None);
    }
}
