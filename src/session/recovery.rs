//! At most one local clarification after a model timeout; never replay a heard prefix.
use super::{Session, generation::Generation};
use crate::{event::EventData, provider::Timing};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryPolicy {
    pub before_audio: String,
    pub after_audio: String,
}
impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            before_audio: "Sorry, I could not complete that. Could you repeat your request?".into(),
            after_audio: "Sorry, my reply was cut off. Could you confirm what you need next?"
                .into(),
        }
    }
}
impl Session {
    pub(super) fn recover_current(&mut self, stage: &str) -> bool {
        let Some(policy) = self.config.recovery.clone() else {
            return false;
        };
        let Some(old) = self.generation.as_ref() else {
            return false;
        };
        if old.recovery || !["llm", "tts"].contains(&stage) {
            return false;
        }
        let previous = old.id;
        let ordinal = old.ordinal;
        let speech_end_ms = old.speech_end_ms;
        // Settle the real consumed prefix before choosing a recovery response/context.
        self.cancel_current("provider_timeout");
        if self.failed.is_some() {
            return false;
        }
        let reply = self
            .playback
            .replies
            .last()
            .expect("cancelled reply exists");
        let played = reply.played_samples();
        let heard = reply.heard_text();
        let text = if played == 0 {
            policy.before_audio
        } else {
            policy.after_audio
        };
        let Some(models) = self.factory.recovery(&text) else {
            return false;
        };
        self.generation_counter += 1;
        let id = crate::event::Identity {
            turn_id: previous.turn_id,
            generation_id: self.generation_counter,
        };
        if let Err(error) = self.playback.activate(id) {
            self.failed = Some(error.to_string());
            return false;
        }
        let soft = tokio_util::sync::CancellationToken::new();
        let ctx = self.ctx(soft.clone());
        self.generation = Some(Generation {
            id,
            soft,
            ordinal,
            tts_done: false,
            started: false,
            asr_transcript: Default::default(),
            asr_final_received: true,
            recovery: true,
            speech_end_ms,
        });
        self.emit(
            Some(id),
            EventData::RecoveryStarted {
                failed_generation_id: previous.generation_id,
                stage: stage.into(),
                speech_end_ms,
                played_before_failure: played,
                heard_context: heard.clone(),
            },
        );
        self.emit(
            Some(id),
            EventData::LlmRequested {
                transcript: "[local timeout clarification]".into(),
                heard_history: vec![format!("{heard} [interrupted]")],
            },
        );
        // Separate, bounded fallback timing. These are local fake adapters, not an SDK retry.
        self.launch_response(id, models, (Timing::default(), Timing::default()), ctx);
        true
    }
}
