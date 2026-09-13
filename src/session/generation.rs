//! Generation fencing, heard-only context, and playback transitions.
use super::{Session, input::InputTurn};
use crate::{
    event::Identity,
    playback::{ChunkRecord, Consumption, Packet},
    queue::{self, QueueMeter},
    transport::{self, AsrInput},
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub(super) struct Generation {
    pub(super) id: Identity,
    pub(super) soft: CancellationToken,
    ordinal: usize,
    tts_done: bool,
    started: bool,
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
            "endpoint_committed",
            Some(id),
            json!({"speech_end_ms": turn.last_end, "partial": turn.partial,
            "observed_silence_ms": turn.silence_samples / 16,
            "vad_processed_frames": self.processed_frames, "input_admitted_frames": self.admitted_frames.get(),
            "threshold_ms": self.config.endpoint.silence_threshold(&turn.partial)}),
        );
        self.generation = Some(Generation {
            id,
            soft: turn.soft.clone(),
            ordinal: turn.ordinal,
            tts_done: false,
            started: false,
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
                "playback_progress",
                Some(progress.identity),
                json!(progress),
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
            "playback_stopped",
            Some(generation.id),
            json!({"reason": reason}),
        );
        self.emit(
            "generation_cancelled",
            Some(generation.id),
            json!({"reason": reason}),
        );
        generation.soft.cancel();
        self.status.cancelled_generations += 1;
    }
    pub(super) fn on_final(&mut self, turn: u64, text: String) {
        let Some(generation) = &self.generation else {
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
        let id = generation.id;
        let ordinal = generation.ordinal;
        let ctx = self.ctx(generation.soft.clone());
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
        self.emit("asr_final", Some(id), json!({"text": text}));
        self.emit(
            "llm_requested",
            Some(id),
            json!({"transcript": text, "heard_history": history}),
        );
        let (tx, rx, meter) = queue::channel(
            format!("llm_tts_{}", id.generation_id),
            self.config.text_capacity,
        );
        self.meters.push(meter);
        let budget_meter = QueueMeter::new(
            format!("audio_in_flight_{}", id.generation_id),
            self.config.playback_samples,
        );
        self.meters.push(budget_meter.clone());
        self.spawn(
            "llm",
            Some(turn),
            transport::llm_worker(
                self.factory.llm(ordinal, &text, &history),
                id,
                tx,
                self.config.llm.clone(),
                ctx.clone(),
            ),
        );
        self.spawn(
            "tts",
            Some(turn),
            transport::tts_worker(
                self.factory.tts(),
                id,
                rx,
                Arc::new(Semaphore::new(self.config.playback_samples)),
                self.config.tts.clone(),
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
        self.emit("llm_chunk", Some(id), json!({"text": text}));
    }
    pub(super) fn on_audio(&mut self, packet: Packet) {
        let id = packet.chunk.identity;
        self.emit("tts_chunk", Some(id), json!(packet.chunk));
        if self.generation.as_ref().is_none_or(|g| g.id != id) {
            self.record_rejected_audio(&packet, "stale_generation");
            self.stale(id, "tts_chunk");
            return;
        }
        let sequence = packet.chunk.sequence;
        let mut rejected = ChunkRecord::rejected(&packet.chunk, "");
        if let Err(error) = self.playback.enqueue(packet) {
            rejected.rejection_reason = Some(error.to_string());
            self.emit("audio_rejected", Some(id), json!(rejected));
            let _ = self.playback.record_rejected(id, rejected);
            self.failed = Some(error.to_string());
            return;
        }
        self.emit(
            "audio_enqueued",
            Some(id),
            json!({"chunk_sequence": sequence, "queue_samples": self.playback.depth_samples()}),
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
                "playback_started",
                Some(id),
                json!({"chunk_sequence": sequence}),
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
                "playback_stopped",
                Some(generation.id),
                json!({"reason": "completed"}),
            );
            self.status.completed_replies += 1;
        }
    }
}
