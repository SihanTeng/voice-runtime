use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub turn_id: u64,
    pub generation_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// Monotonic milliseconds relative to session start, not wall-clock time.
    pub timestamp: u64,
    #[serde(default = "legacy_schema")]
    pub schema_version: u32,
    pub session_id: String,
    pub turn_id: Option<u64>,
    pub generation_id: Option<u64>,
    pub event_type: String,
    pub sequence_number: u64,
    pub payload: Value,
}

impl Event {
    pub fn identity(&self) -> Option<Identity> {
        Some(Identity {
            turn_id: self.turn_id?,
            generation_id: self.generation_id?,
        })
    }
}

pub fn write_jsonl(events: &[Event], mut writer: impl std::io::Write) -> std::io::Result<()> {
    for event in events {
        serde_json::to_writer(&mut writer, event)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

fn legacy_schema() -> u32 {
    1
}

/// All owner emissions use this typed payload; the JSONL envelope remains stable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "event_type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EventData {
    SessionStarted {
        config: Box<crate::session::SessionConfig>,
        clock: String,
        playback: String,
    },
    AudioFrame {
        source_sequence: u64,
        capture_ms: u64,
        valid_samples: usize,
        vad: bool,
        speech_truth: Option<bool>,
        missing_frames: u64,
    },
    AudioFrameRejected {
        reason: String,
        source_sequence: u64,
    },
    SpeechStart {
        onset_ms: u64,
        detected_ms: u64,
    },
    SpeechEnd {
        speech_end_ms: u64,
    },
    SpeechCandidateRejected {
        duration_ms: u64,
    },
    EndpointCandidateRevoked {},
    EndpointCommitted {
        speech_end_ms: u64,
        partial: String,
        observed_silence_ms: u64,
        vad_processed_frames: u64,
        input_admitted_frames: u64,
        threshold_ms: u64,
    },
    AsrRequested {},
    TtsRequested {
        request_ms: u64,
    },
    AsrPartial {
        text: String,
        through_sequence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<crate::transcript::AsrUpdate>,
    },
    AsrFinal {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<crate::transcript::AsrUpdate>,
    },
    RecoveryStarted {
        failed_generation_id: u64,
        stage: String,
        speech_end_ms: u64,
        played_before_failure: usize,
        heard_context: String,
    },
    BackchannelRejected {
        text: String,
        duration_ms: u64,
    },
    AsrResultRejected {
        revision: u64,
        reason: String,
    },
    LlmRequested {
        transcript: String,
        heard_history: Vec<String>,
    },
    LlmChunk {
        text: String,
    },
    AudioEnqueued {
        chunk_sequence: u64,
        queue_samples: usize,
    },
    PlaybackStarted {
        chunk_sequence: u64,
    },
    PlaybackStopped {
        reason: String,
    },
    GenerationCancelled {
        reason: String,
    },
    InterruptionDecision {
        onset_ms: u64,
        detected_ms: u64,
        decision_ms: u64,
        new_turn_id: u64,
    },
    StaleEventDropped {
        source_type: String,
    },
    TurnFailed {
        reason: String,
    },
    TaskExited {
        stage: String,
        error: Option<String>,
    },
    ProviderFailed {
        stage: String,
        reason: String,
    },
    TaskPanicked {
        error: String,
    },
    TaskAborted {},
    SinkFailed {
        reason: String,
    },
    SessionClosed {
        reason: String,
        active_tasks: usize,
        queues: Vec<crate::queue::QueueSnapshot>,
    },
    TtsChunk(crate::playback::AudioChunk),
    AudioRejected(crate::playback::ChunkRecord),
    PlaybackProgress(crate::playback::Consumption),
}

impl Event {
    pub fn new(
        timestamp: u64,
        session_id: String,
        id: Option<Identity>,
        sequence_number: u64,
        data: EventData,
    ) -> Self {
        let mut wire = serde_json::to_value(data).expect("typed event serialization");
        Self {
            timestamp,
            schema_version: 2,
            session_id,
            turn_id: id.map(|i| i.turn_id),
            generation_id: id.and_then(|i| (i.generation_id != 0).then_some(i.generation_id)),
            event_type: wire["event_type"]
                .as_str()
                .expect("tagged event")
                .to_owned(),
            sequence_number,
            payload: wire["payload"].take(),
        }
    }
    pub fn decode(&self) -> Result<EventData, String> {
        if ![1, 2].contains(&self.schema_version)
            || self.session_id.is_empty()
            || self.session_id.len() > 128
        {
            return Err("unsupported schema or invalid session identity".into());
        }
        let data: EventData = serde_json::from_value(
            serde_json::json!({"event_type": self.event_type, "payload": self.payload}),
        )
        .map_err(|e| format!("{}: {e}", self.event_type))?;
        let needs_generation = matches!(
            data,
            EventData::RecoveryStarted { .. }
                | EventData::EndpointCommitted { .. }
                | EventData::AsrFinal { .. }
                | EventData::LlmRequested { .. }
                | EventData::LlmChunk { .. }
                | EventData::TtsChunk(_)
                | EventData::TtsRequested { .. }
                | EventData::AudioEnqueued { .. }
                | EventData::AudioRejected(_)
                | EventData::PlaybackStarted { .. }
                | EventData::PlaybackProgress(_)
                | EventData::PlaybackStopped { .. }
                | EventData::GenerationCancelled { .. }
                | EventData::InterruptionDecision { .. }
        );
        if needs_generation
            && (self.turn_id.is_none_or(|i| i == 0) || self.generation_id.is_none_or(|i| i == 0))
        {
            return Err("missing generation identity".into());
        }
        if matches!(
            data,
            EventData::AsrRequested {}
                | EventData::AsrPartial { .. }
                | EventData::SpeechStart { .. }
                | EventData::SpeechEnd { .. }
                | EventData::SpeechCandidateRejected { .. }
                | EventData::EndpointCandidateRevoked {}
                | EventData::BackchannelRejected { .. }
                | EventData::AsrResultRejected { .. }
                | EventData::TurnFailed { .. }
        ) && self.turn_id.is_none_or(|id| id == 0)
        {
            return Err("missing turn identity".into());
        }
        if self.schema_version == 2
            && matches!(
                &data,
                EventData::AsrPartial { update: None, .. }
                    | EventData::AsrFinal { update: None, .. }
            )
        {
            return Err("ASR revision metadata missing".into());
        }
        match &data {
            EventData::AsrPartial {
                update: Some(update),
                through_sequence,
                ..
            } => {
                update.validate(1_048_576).map_err(|e| e.to_string())?;
                if *through_sequence != update.through_sequence {
                    return Err("ASR freshness metadata mismatch".into());
                }
            }
            EventData::AsrFinal {
                update: Some(update),
                ..
            } => {
                update.validate(1_048_576).map_err(|e| e.to_string())?;
                if !update.is_final {
                    return Err("non-final ASR final event".into());
                }
            }
            EventData::TtsRequested { request_ms } if *request_ms > self.timestamp => {
                return Err("TTS request in the future".into());
            }
            EventData::InterruptionDecision { decision_ms, .. }
                if *decision_ms != self.timestamp =>
            {
                return Err("interruption decision time mismatch".into());
            }
            EventData::SessionStarted { config, .. } => {
                config.validate().map_err(|e| e.to_string())?
            }
            EventData::AudioFrame {
                valid_samples,
                capture_ms,
                ..
            } => {
                if !(1..=320).contains(valid_samples) || *capture_ms > self.timestamp {
                    return Err("invalid captured frame shape/time".into());
                }
            }
            EventData::EndpointCommitted { speech_end_ms, .. }
            | EventData::SpeechEnd { speech_end_ms } => {
                if *speech_end_ms > self.timestamp {
                    return Err("speech end in the future".into());
                }
            }
            EventData::SpeechStart {
                onset_ms,
                detected_ms,
            }
            | EventData::InterruptionDecision {
                onset_ms,
                detected_ms,
                ..
            } => {
                if onset_ms > detected_ms || *detected_ms > self.timestamp {
                    return Err("invalid speech detection times".into());
                }
            }
            EventData::TtsChunk(chunk) => {
                if chunk.samples.is_empty()
                    || chunk.samples.len() > 320
                    || Some(chunk.identity) != self.identity()
                {
                    return Err("invalid TTS chunk shape/identity".into());
                }
            }
            EventData::PlaybackProgress(p)
                if p.samples > 320
                    || p.samples == 0
                    || p.start_ms > p.end_ms
                    || p.end_ms > self.timestamp
                    || p.end_ms
                        .checked_sub(p.start_ms)
                        .and_then(|d| d.checked_mul(16))
                        != Some(p.samples as u64)
                    || Some(p.identity) != self.identity() =>
            {
                return Err("invalid consumption shape/time/identity".into());
            }
            _ => {}
        }
        Ok(data)
    }
}
