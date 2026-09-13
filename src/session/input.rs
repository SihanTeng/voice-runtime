//! Input candidates, ASR admission/freshness, and endpoint decisions.
use super::Session;
use crate::{
    event::Identity,
    queue,
    transport::{self, AsrInput, CapturedFrame},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub(super) struct InputTurn {
    pub(super) id: u64,
    onset: u64,
    detected: u64,
    continuous_samples: usize,
    pub(super) silence_samples: u64,
    pub(super) last_end: u64,
    last_sequence: u64,
    speaking: bool,
    confirmed: bool,
    pub(super) partial: String,
    partial_changed: u64,
    through: Option<u64>,
    pub(super) tx: Option<queue::Sender<AsrInput>>,
    pub(super) soft: CancellationToken,
    pub(super) ordinal: usize,
}

impl Session {
    pub(super) fn reject_turn(&mut self, reason: &str) {
        if let Some(turn) = self.turn.take() {
            turn.soft.cancel();
            self.emit(
                "turn_failed",
                Some(Identity {
                    turn_id: turn.id,
                    generation_id: 0,
                }),
                json!({"reason": reason}),
            );
        }
    }
    pub(super) fn on_vad(&mut self, frame: CapturedFrame, voiced: bool) {
        self.processed_frames += 1;
        let now = self.clock.now_ms();
        let audio = frame.audio;
        let missing = match self.frame_validator.accept(&audio) {
            Ok(missing) => missing,
            Err(error) => {
                self.emit(
                    "audio_frame_rejected",
                    None,
                    json!({"reason": error.to_string(), "source_sequence": audio.sequence}),
                );
                return;
            }
        };
        self.emit("audio_frame", None, json!({"source_sequence": audio.sequence, "capture_ms": audio.timestamp,
            "valid_samples": audio.valid_samples, "vad": voiced, "speech_truth": frame.speech_truth, "missing_frames": missing}));
        if missing > 0
            && let Some(turn) = &mut self.turn
        {
            turn.continuous_samples = 0;
        }
        if self.preroll.len() == 15 {
            self.preroll.pop_front();
        }
        self.preroll.push_back((audio.clone(), voiced));
        self.preroll_meter.observe(self.preroll.len());
        if self.turn.is_none() && voiced {
            self.turn_counter += 1;
            let id = self.turn_counter;
            self.turn = Some(InputTurn {
                id,
                onset: audio.timestamp,
                detected: now,
                continuous_samples: 0,
                silence_samples: 0,
                last_end: audio.timestamp,
                last_sequence: audio.sequence,
                speaking: false,
                confirmed: false,
                partial: String::new(),
                partial_changed: now,
                through: None,
                tx: None,
                soft: CancellationToken::new(),
                ordinal: self.accepted_turns,
            });
            self.emit(
                "speech_start",
                Some(Identity {
                    turn_id: id,
                    generation_id: 0,
                }),
                json!({"onset_ms": audio.timestamp, "detected_ms": now}),
            );
        }
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        if voiced {
            turn.silence_samples = 0;
            if !turn.speaking && turn.confirmed {
                self.emit(
                    "endpoint_candidate_revoked",
                    Some(Identity {
                        turn_id: turn.id,
                        generation_id: 0,
                    }),
                    json!({}),
                );
            }
            turn.continuous_samples += audio.valid_samples;
            turn.last_end = audio.timestamp + audio.valid_samples as u64 / 16;
            turn.last_sequence = audio.sequence;
            turn.speaking = true;
        } else {
            turn.silence_samples += audio.valid_samples as u64;
            if turn.speaking {
                self.emit(
                    "speech_end",
                    Some(Identity {
                        turn_id: turn.id,
                        generation_id: 0,
                    }),
                    json!({"speech_end_ms": turn.last_end}),
                );
            }
            turn.speaking = false;
            turn.continuous_samples = 0;
            if !turn.confirmed {
                self.emit(
                    "speech_candidate_rejected",
                    Some(Identity {
                        turn_id: turn.id,
                        generation_id: 0,
                    }),
                    json!({"duration_ms": turn.last_end - turn.onset}),
                );
                return;
            }
        }
        if !turn.confirmed
            && turn.continuous_samples as u64 >= self.config.endpoint.barge_in_ms * 16
        {
            if self.accepted_turns >= self.config.max_turns {
                self.failed = Some("turn_capacity".into());
                return;
            }
            turn.confirmed = true;
            self.accepted_turns += 1;
            if let Some(generation) = &self.generation {
                self.emit(
                    "interruption_decision",
                    Some(generation.id),
                    json!({"onset_ms": turn.onset,
                    "detected_ms": turn.detected, "decision_ms": now, "new_turn_id": turn.id}),
                );
                self.cancel_current("barge_in");
            }
            let (tx, rx, meter) =
                queue::channel(format!("asr_input_{}", turn.id), self.config.asr_capacity);
            self.meters.push(meter);
            for (frame, voice) in &self.preroll {
                if tx.try_send(AsrInput::Frame(frame.clone(), *voice)).is_err() {
                    self.emit(
                        "turn_failed",
                        Some(Identity {
                            turn_id: turn.id,
                            generation_id: 0,
                        }),
                        json!({"reason": "asr_overload"}),
                    );
                    return;
                }
            }
            self.spawn(
                "asr",
                Some(turn.id),
                transport::asr_worker(
                    self.factory.asr(turn.ordinal),
                    turn.id,
                    rx,
                    self.config.asr.clone(),
                    self.ctx(turn.soft.clone()),
                ),
            );
            turn.tx = Some(tx);
        } else if let Some(tx) = &turn.tx
            && tx.try_send(AsrInput::Frame(audio, voiced)).is_err()
        {
            turn.soft.cancel();
            self.emit(
                "turn_failed",
                Some(Identity {
                    turn_id: turn.id,
                    generation_id: 0,
                }),
                json!({"reason": "asr_overload"}),
            );
            return;
        }
        self.turn = Some(turn);
        self.maybe_endpoint();
    }
    pub(super) fn maybe_endpoint(&mut self) {
        let now = self.clock.now_ms();
        let Some(turn) = &self.turn else {
            return;
        };
        if !turn.confirmed || turn.speaking || self.processed_frames != self.admitted_frames.get() {
            return;
        }
        let lag = turn.through.is_none_or(|s| s < turn.last_sequence);
        if lag && now.saturating_sub(turn.last_end) >= self.config.endpoint.asr_lag_timeout_ms {
            self.reject_turn("asr_lag_timeout");
            return;
        }
        if lag
            || turn.silence_samples
                < self
                    .config
                    .endpoint
                    .silence_threshold(&turn.partial)
                    .saturating_mul(16)
            || now.saturating_sub(turn.last_end)
                < self.config.endpoint.silence_threshold(&turn.partial)
            || now.saturating_sub(turn.partial_changed) < self.config.endpoint.partial_stability_ms
        {
            return;
        }
        let turn = self.turn.take().expect("turn checked above");
        self.commit_endpoint(turn);
    }
    pub(super) fn on_partial(&mut self, turn: u64, text: String, through: u64) {
        if let Some(input) = &mut self.turn
            && input.id == turn
        {
            if input.partial != text {
                input.partial_changed = self.clock.now_ms();
            }
            input.partial.clone_from(&text);
            input.through = Some(through);
            self.emit(
                "asr_partial",
                Some(Identity {
                    turn_id: turn,
                    generation_id: 0,
                }),
                json!({"text": text, "through_sequence": through}),
            );
        } else {
            self.stale(
                Identity {
                    turn_id: turn,
                    generation_id: 0,
                },
                "asr_partial",
            );
        }
    }
}
