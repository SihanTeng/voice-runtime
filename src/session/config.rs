//! Validated resource limits and provider/endpoint timing defaults.
use super::SessionError;
use crate::{endpoint::EndpointConfig, provider::Timing};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub session_id: String,
    pub endpoint: EndpointConfig,
    pub vad: Timing,
    pub asr: Timing,
    pub llm: Timing,
    pub tts: Timing,
    pub input_capacity: usize,
    pub asr_capacity: usize,
    pub event_capacity: usize,
    pub text_capacity: usize,
    pub log_capacity: usize,
    pub playback_samples: usize,
    pub max_turns: usize,
    pub max_text_bytes: usize,
    pub max_chunks_per_reply: usize,
    pub max_events: usize,
    pub max_tasks: usize,
    pub max_session_ms: u64,
    pub backpressure_ms: u64,
    pub shutdown_grace_ms: u64,
    /// Fault injection for the bounded journal, not an input to runtime decisions.
    pub journal_fail_after: Option<usize>,
}
impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            session_id: "session-1".into(),
            endpoint: EndpointConfig::default(),
            vad: Timing {
                first_ms: 0,
                interval_ms: 0,
                ..Timing::default()
            },
            asr: Timing {
                first_ms: 40,
                interval_ms: 0,
                ..Timing::default()
            },
            llm: Timing {
                first_ms: 80,
                interval_ms: 20,
                ..Timing::default()
            },
            tts: Timing {
                first_ms: 60,
                interval_ms: 5,
                late_chunks: 2,
                ..Timing::default()
            },
            input_capacity: 50,
            asr_capacity: 50,
            event_capacity: 32,
            text_capacity: 32,
            log_capacity: 4096,
            playback_samples: 8000,
            max_turns: 32,
            max_text_bytes: 16_384,
            max_chunks_per_reply: 3000,
            max_events: 100_000,
            max_tasks: 32,
            max_session_ms: 300_000,
            backpressure_ms: 2000,
            shutdown_grace_ms: 500,
            journal_fail_after: None,
        }
    }
}
impl SessionConfig {
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.session_id.is_empty()
            || self.session_id.len() > 128
            || [
                self.input_capacity,
                self.asr_capacity,
                self.event_capacity,
                self.text_capacity,
                self.log_capacity,
                self.max_turns,
                self.max_text_bytes,
                self.max_chunks_per_reply,
                self.max_events,
            ]
            .contains(&0)
            || self.playback_samples < 320
            || self.playback_samples > 160_000
            || self.max_tasks < 4
            || self.max_turns > 1024
            || self.max_text_bytes > 1_048_576
            || self.max_session_ms == 0
            || self.max_session_ms > 3_600_000
            || self.max_tasks > 1024
            || self.max_events > 100_000
            || self.max_chunks_per_reply > 100_000
            || [
                self.input_capacity,
                self.asr_capacity,
                self.event_capacity,
                self.text_capacity,
                self.log_capacity,
            ]
            .iter()
            .any(|n| *n > 65_536)
            || self.backpressure_ms == 0
            || self.shutdown_grace_ms == 0
            || self.endpoint.barge_in_ms < 100
        {
            return Err(SessionError::Configuration);
        }
        for timing in [&self.vad, &self.asr, &self.llm, &self.tts] {
            if timing.jitter_ms > 60_000
                || timing.first_ms > 300_000
                || timing.interval_ms > 300_000
                || timing.late_chunks > 128
                || timing.late_delay_ms > 10_000
                || [
                    timing.first_timeout_ms,
                    timing.idle_timeout_ms,
                    timing.total_timeout_ms,
                ]
                .iter()
                .any(|ms| *ms > 3_600_000)
                || timing.first_timeout_ms == 0
                || timing.idle_timeout_ms == 0
                || timing.total_timeout_ms == 0
            {
                return Err(SessionError::Configuration);
            }
        }
        Ok(())
    }
}
