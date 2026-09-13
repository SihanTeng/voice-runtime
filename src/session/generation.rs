//! Generation fencing, heard-only context, and playback transitions.
use super::{Session, input::InputTurn};
use crate::event::EventData;
use crate::{
    event::Identity,
    playback::{ChunkRecord, Consumption, Packet},
    queue::{self, QueueMeter},
    transport::{self, AsrInput},
};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub(super) struct Generation {
    pub(super) id: Identity,
    pub(super) soft: CancellationToken,
    pub(super) ordinal: usize,
    pub(super) tts_done: bool,
    pub(super) started: bool,
    pub(super) asr_transcript: crate::transcript::Transcript,
    pub(super) asr_final_received: bool,
    pub(super) recovery: bool,
    pub(super) speech_end_ms: u64,
}

impl Session {
    pub(super) fn commit_endpoint(&mut self, turn: InputTurn) {
        self.cancel_current("new_endpoint");
        self.generation_counter += 1;
        let id = Identity {
            turn_id: turn.id,
            generation_id: self.generation_counter,
        };
        if let Err(error) = self.playback.activate(id) {
            self.failed = Some(error.to_string());
            return;
        }
        self.emit(
            Some(id),
            EventData::EndpointCommitted {
                speech_end_ms: turn.last_end,
                partial: turn.partial.to_string(),
                observed_silence_ms: turn.silence_samples / 16,
                vad_processed_frames: self.processed_frames,
                input_admitted_frames: self.admitted_frames.get(),
                threshold_ms: self.config.endpoint.silence_threshold(&turn.partial),
            },
        );
        self.generation = Some(Generation {
            id,
            soft: turn.soft.clone(),
            ordinal: turn.ordinal,
            tts_done: false,
            started: false,
            asr_transcript: turn.transcript,
            asr_final_received: false,
            recovery: false,
            speech_end_ms: turn.last_end,
        });
        if turn
            .tx
            .as_ref()
            .is_none_or(|tx| tx.try_send(AsrInput::Finish).is_err())
        {
            self.cancel_current("asr_overload");
        }
    }
    pub(super) fn progress(&mut self, progress: Option<Consumption>) {
        if let Some(progress) = progress {
            self.emit(
                Some(progress.identity),
                EventData::PlaybackProgress(progress),
            );
        }
    }
    pub(super) fn cancel_current(&mut self, reason: &str) {
        let Some(generation) = self.generation.take() else {
            return;
        };
        // Taking the generation revokes admission before any subsequent event can be handled.
        match self.playback.stop(self.clock.now_ms(), true) {
            Ok(progress) => self.progress(progress),
            Err(error) => self.failed = Some(error.to_string()),
        }
        self.emit(
            Some(generation.id),
            EventData::PlaybackStopped {
                reason: reason.to_string(),
            },
        );
        self.emit(
            Some(generation.id),
            EventData::GenerationCancelled {
                reason: reason.to_string(),
            },
        );
        generation.soft.cancel();
        self.status.cancelled_generations += 1;
    }
    pub(super) fn on_final(&mut self, turn: u64, update: crate::transcript::AsrUpdate) {
        let Some(generation) = &mut self.generation else {
            self.stale(
                Identity {
                    turn_id: turn,
                    generation_id: 0,
                },
                "asr_final",
            );
            return;
        };
        if generation.id.turn_id != turn {
            self.stale(
                Identity {
                    turn_id: turn,
                    generation_id: 0,
                },
                "asr_final",
            );
            return;
        }
        if generation.asr_final_received {
            let id = generation.id;
            self.stale(id, "asr_final");
            return;
        }
        match generation
            .asr_transcript
            .apply(&update, self.config.max_text_bytes)
        {
            Ok(true) if update.is_final => {}
            _ => {
                self.cancel_current("invalid_asr_final");
                return;
            }
        }
        generation.asr_final_received = true;
        let text = generation.asr_transcript.text().to_owned();
        let id = generation.id;
        let ordinal = generation.ordinal;
        let soft = generation.soft.clone();
        let ctx = self.ctx(soft);
        let history: Vec<String> = self
            .playback
            .replies
            .iter()
            .rev()
            .skip(1)
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|r| {
                format!(
                    "{}{}",
                    r.heard_text(),
                    if r.interrupted { " [interrupted]" } else { "" }
                )
            })
            .collect();
        self.emit(
            Some(id),
            EventData::AsrFinal {
                text: text.to_string(),
                update: Some(update),
            },
        );
        self.emit(
            Some(id),
            EventData::LlmRequested {
                transcript: text.to_string(),
                heard_history: history.clone(),
            },
        );
        let models = crate::fake::ResponseProviders {
            llm: self.factory.llm(ordinal, &text, &history),
            tts: self.factory.tts(),
        };
        self.launch_response(
            id,
            models,
            (self.config.llm.clone(), self.config.tts.clone()),
            ctx,
        );
    }
    pub(super) fn launch_response(
        &mut self,
        id: Identity,
        models: crate::fake::ResponseProviders,
        timing: (crate::provider::Timing, crate::provider::Timing),
        ctx: crate::transport::WorkerContext,
    ) {
        let (tx, rx, meter) = queue::channel_with_clock(
            format!("llm_tts_{}", id.generation_id),
            self.config.text_capacity,
            self.clock.clone(),
        );
        self.meters.push(meter);
        let budget_meter = QueueMeter::new(
            format!("audio_in_flight_{}", id.generation_id),
            self.config.playback_samples,
        );
        self.meters.push(budget_meter.clone());
        self.spawn(
            "llm",
            Some(id.turn_id),
            Some(id),
            transport::llm_worker(models.llm, id, tx, timing.0, ctx.clone()),
        );
        self.spawn(
            "tts",
            Some(id.turn_id),
            Some(id),
            transport::tts_worker(
                models.tts,
                id,
                rx,
                Arc::new(Semaphore::new(self.config.playback_samples)),
                timing.1,
                ctx,
                budget_meter,
            ),
        );
    }
    pub(super) fn on_text(&mut self, id: Identity, text: String) {
        if self.generation.as_ref().is_none_or(|g| g.id != id) {
            self.stale(id, "llm_chunk");
            return;
        }
        if let Err(error) = self
            .playback
            .append_text(id, &text, self.config.max_text_bytes)
        {
            self.failed = Some(error.to_string());
            return;
        }
        self.emit(
            Some(id),
            EventData::LlmChunk {
                text: text.to_string(),
            },
        );
    }
    pub(super) fn on_audio(&mut self, packet: Packet) {
        let id = packet.chunk.identity;
        self.emit(Some(id), EventData::TtsChunk(packet.chunk.clone()));
        if self.generation.as_ref().is_none_or(|g| g.id != id) {
            self.record_rejected_audio(&packet, "stale_generation");
            self.stale(id, "tts_chunk");
            return;
        }
        let sequence = packet.chunk.sequence;
        let mut rejected = ChunkRecord::rejected(&packet.chunk, "");
        if let Err(error) = self.playback.enqueue(packet) {
            rejected.rejection_reason = Some(error.to_string());
            self.emit(Some(id), EventData::AudioRejected(rejected.clone()));
            let _ = self.playback.record_rejected(id, rejected);
            self.failed = Some(error.to_string());
            return;
        }
        self.emit(
            Some(id),
            EventData::AudioEnqueued {
                chunk_sequence: sequence,
                queue_samples: self.playback.depth_samples(),
            },
        );
        self.start_playback();
    }
    pub(super) fn record_rejected_audio(&mut self, packet: &Packet, reason: &str) {
        if let Err(error) = self.playback.record_rejected(
            packet.chunk.identity,
            ChunkRecord::rejected(&packet.chunk, reason),
        ) {
            self.failed = Some(error.to_string());
            self.trace_complete = false;
        }
    }
    pub(super) fn on_tts_done(&mut self, id: Identity) {
        if let Some(generation) = &mut self.generation
            && generation.id == id
        {
            generation.tts_done = true;
        } else {
            self.stale(id, "tts_done");
        }
    }
    pub(super) fn start_playback(&mut self) {
        if let Some((id, sequence)) = self.playback.start_ready(self.clock.now_ms())
            && let Some(generation) = &mut self.generation
            && !generation.started
        {
            generation.started = true;
            self.status.started_replies += 1;
            self.status
                .first_audio_ms
                .get_or_insert(self.clock.now_ms());
            self.emit(
                Some(id),
                EventData::PlaybackStarted {
                    chunk_sequence: sequence,
                },
            );
        }
    }
    pub(super) fn tick_playback(&mut self) {
        match self.playback.settle(self.clock.now_ms()) {
            Ok(p) => self.progress(p),
            Err(e) => self.failed = Some(e.to_string()),
        }
        self.start_playback();
        if self.generation.as_ref().is_some_and(|g| g.tts_done) && self.playback.is_idle() {
            let generation = self.generation.take().expect("generation checked above");
            if let Err(error) = self.playback.stop(self.clock.now_ms(), false) {
                self.failed = Some(error.to_string());
            }
            self.emit(
                Some(generation.id),
                EventData::PlaybackStopped {
                    reason: "completed".to_string(),
                },
            );
            self.status.completed_replies += 1;
        }
    }
}
