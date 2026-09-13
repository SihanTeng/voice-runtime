use crate::{
    audio::{AudioFrame, FRAME_MS, FrameValidator},
    clock::Clock,
    endpoint::EndpointConfig,
    event::{Event, Identity},
    fake::ProviderFactory,
    playback::{Consumption, Playback, PlaybackSink, ReplyRecord},
    provider::{ProviderError, Timing},
    queue::{self, QueueMeter, QueueSnapshot},
    transport::{self, AsrInput, CapturedFrame, Output, WorkerContext},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{cell::Cell, collections::VecDeque, rc::Rc, sync::Arc};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

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
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("scenario did not reach the expected state within 15 seconds")]
    ScenarioDeadline,
    #[error("invalid session configuration")]
    Configuration,
    #[error("session is closed")]
    Closed,
    #[error("input frame rejected: {0}")]
    Audio(#[from] crate::audio::AudioError),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub idle: bool,
    pub first_audio_ms: Option<u64>,
    pub started_replies: usize,
    pub completed_replies: usize,
    pub cancelled_generations: usize,
    pub closed: bool,
}
struct Client {
    close: CancellationToken,
    reason: Rc<Cell<&'static str>>,
}
impl Drop for Client {
    fn drop(&mut self) {
        if !self.close.is_cancelled() {
            self.reason.set("client_disconnected");
            self.close.cancel();
        }
    }
}
#[derive(Clone)]
pub struct SessionHandle {
    client: Rc<Client>,
    input: queue::Sender<CapturedFrame>,
    cancel: queue::Sender<()>,
    snapshot: watch::Receiver<Snapshot>,
}
impl SessionHandle {
    pub async fn send_audio(&self, audio: AudioFrame) -> Result<(), SessionError> {
        self.send_measured_audio(audio, None).await
    }
    pub async fn send_measured_audio(
        &self,
        audio: AudioFrame,
        speech_truth: Option<bool>,
    ) -> Result<(), SessionError> {
        audio.validate()?;
        tokio::select! { biased;
            _ = self.client.close.cancelled() => Err(SessionError::Closed),
            result = self.input.send(CapturedFrame { audio, speech_truth }) => result.map_err(|_| SessionError::Closed),
        }
    }
    pub fn cancel_generation(&self) -> Result<(), SessionError> {
        if self.client.close.is_cancelled() {
            return Err(SessionError::Closed);
        }
        match self.cancel.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()), // Coalesced idempotent request.
            Err(_) => Err(SessionError::Closed),
        }
    }
    pub fn close(&self) {
        self.close_with_reason("active_close");
    }
    pub fn close_with_reason(&self, reason: &'static str) {
        if !self.client.close.is_cancelled() {
            self.client.reason.set(reason);
            self.client.close.cancel();
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }
    pub async fn changed(&mut self) -> Result<Snapshot, SessionError> {
        self.snapshot
            .changed()
            .await
            .map_err(|_| SessionError::Closed)?;
        Ok(self.snapshot())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionReport {
    pub events: Vec<Event>,
    pub replies: Vec<ReplyRecord>,
    pub queues: Vec<QueueSnapshot>,
    pub close_reason: String,
    pub active_tasks: usize,
    pub trace_complete: bool,
}

struct InputTurn {
    id: u64,
    onset: u64,
    detected: u64,
    continuous_samples: usize,
    last_end: u64,
    last_sequence: u64,
    speaking: bool,
    confirmed: bool,
    partial: String,
    partial_changed: u64,
    through: Option<u64>,
    tx: Option<queue::Sender<AsrInput>>,
    soft: CancellationToken,
    ordinal: usize,
}
struct Generation {
    id: Identity,
    soft: CancellationToken,
    ordinal: usize,
    tts_done: bool,
    started: bool,
}
struct TaskResult {
    stage: &'static str,
    turn: Option<u64>,
    result: Result<(), ProviderError>,
}

pub struct Session {
    config: SessionConfig,
    factory: Rc<dyn ProviderFactory>,
    clock: Rc<dyn Clock>,
    playback: Playback,
    close: CancellationToken,
    close_reason: Rc<Cell<&'static str>>,
    hard: CancellationToken,
    raw: Option<mpsc::Receiver<CapturedFrame>>,
    cancel: mpsc::Receiver<()>,
    out_tx: queue::Sender<Output>,
    output: mpsc::Receiver<Output>,
    status_tx: watch::Sender<Snapshot>,
    status: Snapshot,
    tasks: JoinSet<TaskResult>,
    meters: Vec<QueueMeter>,
    turn: Option<InputTurn>,
    generation: Option<Generation>,
    preroll: VecDeque<(AudioFrame, bool)>,
    preroll_meter: QueueMeter,
    frame_validator: FrameValidator,
    turn_counter: u64,
    generation_counter: u64,
    accepted_turns: usize,
    log_tx: Option<queue::Sender<Event>>,
    log_sequence: u64,
    failed: Option<String>,
    trace_complete: bool,
}

impl Session {
    /// Run the returned Session on a LocalSet and await `run()` to join all resources.
    pub fn new(
        config: SessionConfig,
        factory: Rc<dyn ProviderFactory>,
        clock: Rc<dyn Clock>,
        sink: Box<dyn PlaybackSink>,
    ) -> Result<(Self, SessionHandle), SessionError> {
        config.validate()?;
        let (input, raw, im) = queue::channel("input_audio", config.input_capacity);
        let (out_tx, output, om) = queue::channel("provider_events", config.event_capacity);
        let (cancel_tx, cancel, cm) = queue::channel("cancel_control", 1);
        let (status_tx, snapshot) = watch::channel(Snapshot::default());
        let close = CancellationToken::new();
        let reason = Rc::new(Cell::new("active_close"));
        let handle = SessionHandle {
            client: Rc::new(Client {
                close: close.clone(),
                reason: reason.clone(),
            }),
            input,
            cancel: cancel_tx,
            snapshot,
        };
        let playback = Playback::new(
            sink,
            config.playback_samples,
            config.max_chunks_per_reply,
            config.max_turns,
        );
        let preroll_meter = QueueMeter::new("preroll_frames", 15);
        let status_meter = QueueMeter::new("status_watch", 1);
        status_meter.observe(1);
        Ok((
            Self {
                config,
                factory,
                clock,
                playback,
                close,
                close_reason: reason,
                hard: CancellationToken::new(),
                raw: Some(raw),
                cancel,
                out_tx,
                output,
                status_tx,
                status: Snapshot::default(),
                tasks: JoinSet::new(),
                meters: vec![im, om, cm, preroll_meter.clone(), status_meter],
                turn: None,
                generation: None,
                preroll: VecDeque::new(),
                preroll_meter,
                frame_validator: FrameValidator::default(),
                turn_counter: 0,
                generation_counter: 0,
                accepted_turns: 0,
                log_tx: None,
                log_sequence: 0,
                failed: None,
                trace_complete: true,
            },
            handle,
        ))
    }
    fn emit(&mut self, kind: &str, id: Option<Identity>, payload: serde_json::Value) {
        let event = Event {
            timestamp: self.clock.now_ms(),
            session_id: self.config.session_id.clone(),
            turn_id: id.map(|i| i.turn_id),
            generation_id: id.and_then(|i| (i.generation_id != 0).then_some(i.generation_id)),
            event_type: kind.into(),
            sequence_number: self.log_sequence,
            payload,
        };
        self.log_sequence += 1;
        if self
            .log_tx
            .as_ref()
            .is_none_or(|tx| tx.try_send(event).is_err())
        {
            self.trace_complete = false;
            self.failed.get_or_insert("journal_backpressure".into());
        }
    }
    fn ctx(&self, soft: CancellationToken) -> WorkerContext {
        WorkerContext {
            clock: self.clock.clone(),
            hard: self.hard.clone(),
            soft,
            output: self.out_tx.clone(),
            backpressure_ms: self.config.backpressure_ms,
            max_text_bytes: self.config.max_text_bytes,
        }
    }
    fn spawn(
        &mut self,
        stage: &'static str,
        turn: Option<u64>,
        future: impl Future<Output = Result<(), ProviderError>> + 'static,
    ) {
        if self.tasks.len() >= self.config.max_tasks {
            self.failed = Some("task_capacity".into());
            return;
        }
        self.tasks.spawn_local(async move {
            TaskResult {
                stage,
                turn,
                result: future.await,
            }
        });
    }
    fn progress(&mut self, progress: Option<Consumption>) {
        if let Some(progress) = progress {
            self.emit(
                "playback_progress",
                Some(progress.identity),
                json!(progress),
            );
        }
    }
    fn cancel_current(&mut self, reason: &str) {
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
    fn reject_turn(&mut self, reason: &str) {
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
    fn stale(&mut self, id: Identity, kind: &str) {
        self.emit(
            "stale_event_dropped",
            Some(id),
            json!({"source_type": kind}),
        );
    }

    fn on_vad(&mut self, frame: CapturedFrame, voiced: bool) {
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
    }

    fn maybe_endpoint(&mut self) {
        let now = self.clock.now_ms();
        let Some(turn) = &self.turn else {
            return;
        };
        if !turn.confirmed || turn.speaking {
            return;
        }
        let lag = turn.through.is_none_or(|s| s < turn.last_sequence);
        if lag && now.saturating_sub(turn.last_end) >= self.config.endpoint.asr_lag_timeout_ms {
            self.reject_turn("asr_lag_timeout");
            return;
        }
        if lag
            || now.saturating_sub(turn.last_end)
                < self.config.endpoint.silence_threshold(&turn.partial)
            || now.saturating_sub(turn.partial_changed) < self.config.endpoint.partial_stability_ms
        {
            return;
        }
        let turn = self.turn.take().expect("turn checked above");
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

    fn on_output(&mut self, output: Output) {
        match output {
            Output::Vad(frame, voiced) => self.on_vad(frame, voiced),
            Output::Partial {
                turn,
                text,
                through,
            } => {
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
            Output::Final { turn, text } => {
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
            Output::Text(id, text) => {
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
            Output::Audio(packet) => {
                let id = packet.chunk.identity;
                self.emit("tts_chunk", Some(id), json!(packet.chunk));
                if self.generation.as_ref().is_none_or(|g| g.id != id) {
                    self.stale(id, "tts_chunk");
                    return;
                }
                let sequence = packet.chunk.sequence;
                if let Err(error) = self.playback.enqueue(packet) {
                    self.failed = Some(error.to_string());
                    return;
                }
                self.emit("audio_enqueued", Some(id), json!({"chunk_sequence": sequence, "queue_samples": self.playback.depth_samples()}));
                self.start_playback();
            }
            Output::TtsDone(id) => {
                if let Some(generation) = &mut self.generation
                    && generation.id == id
                {
                    generation.tts_done = true;
                } else {
                    self.stale(id, "tts_done");
                }
            }
        }
    }
    fn start_playback(&mut self) {
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
    fn tick(&mut self) {
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
        self.maybe_endpoint();
    }
    fn task_result(&mut self, result: Result<TaskResult, tokio::task::JoinError>) {
        match result {
            Ok(task) => {
                self.emit("task_exited", task.turn.map(|turn_id| Identity { turn_id, generation_id: 0 }), json!({"stage": task.stage, "error": task.result.as_ref().err().map(ToString::to_string)}));
                if let Err(error) = task.result
                    && error != ProviderError::Cancelled
                {
                    self.emit(
                        "provider_failed",
                        task.turn.map(|turn_id| Identity {
                            turn_id,
                            generation_id: 0,
                        }),
                        json!({"stage": task.stage, "reason": error.to_string()}),
                    );
                    if task.stage == "vad" {
                        self.failed = Some(error.to_string());
                    }
                    if self.turn.as_ref().is_some_and(|t| Some(t.id) == task.turn) {
                        self.reject_turn(&error.to_string());
                    }
                    if self
                        .generation
                        .as_ref()
                        .is_some_and(|g| Some(g.id.turn_id) == task.turn)
                    {
                        self.cancel_current(&error.to_string());
                    }
                }
            }
            Err(error) if error.is_panic() => {
                self.failed = Some("background_task_panicked".into());
                self.emit("task_panicked", None, json!({"error": error.to_string()}));
            }
            Err(_) => self.emit("task_aborted", None, json!({})),
        }
    }

    pub async fn run(mut self) -> SessionReport {
        let (log_tx, mut log_rx, lm) = queue::channel("journal", self.config.log_capacity);
        self.meters.push(lm);
        self.log_tx = Some(log_tx);
        let limit = self.config.max_events;
        let fail_after = self.config.journal_fail_after;
        let journal_failed = Rc::new(Cell::new(false));
        let journal_flag = journal_failed.clone();
        let journal =
            tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(async move {
                let mut events = Vec::new();
                while let Some(event) = log_rx.recv().await {
                    if events.len() >= limit || fail_after.is_some_and(|n| events.len() >= n) {
                        journal_flag.set(true);
                        break;
                    }
                    events.push(event);
                }
                events
            }));
        self.emit("session_started", None, json!({"config": self.config, "clock": "monotonic_ms", "playback": "simulated_consumption"}));
        let raw = self.raw.take().expect("raw receiver owned once");
        self.spawn(
            "vad",
            None,
            transport::vad_worker(
                self.factory.vad(),
                raw,
                self.config.vad.clone(),
                self.ctx(CancellationToken::new()),
            ),
        );
        let mut next_tick = self.clock.now_ms() + FRAME_MS;
        loop {
            if self.failed.is_some() || journal_failed.get() {
                break;
            }
            self.status.idle = self.turn.is_none() && self.generation.is_none();
            self.status_tx.send_replace(self.status.clone());
            let clock = self.clock.clone();
            let wake = self
                .playback
                .deadline()
                .map_or(next_tick, |d| d.min(next_tick));
            tokio::select! { biased;
                _ = self.close.cancelled() => break,
                command = self.cancel.recv(), if !self.cancel.is_closed() => { if command.is_some() { self.cancel_current("explicit_cancel"); } }
                _ = clock.sleep_until(wake) => {
                    self.tick();
                    if clock.now_ms() >= next_tick { next_tick = clock.now_ms() + FRAME_MS; }
                    if clock.now_ms() >= self.config.max_session_ms { self.failed = Some("session_deadline".into()); }
                }
                Some(result) = self.tasks.join_next(), if !self.tasks.is_empty() => self.task_result(result),
                Some(output) = self.output.recv() => self.on_output(output),
            }
        }
        let mut reason = self.failed.clone().unwrap_or_else(|| {
            if journal_failed.get() {
                "journal_failure".into()
            } else {
                self.close_reason.get().into()
            }
        });
        self.close.cancel();
        self.cancel_current(&reason);
        self.reject_turn("session_closing");
        match self.playback.close(self.clock.now_ms()) {
            Ok(p) => self.progress(p),
            Err(e) => {
                reason = e.to_string();
                self.failed = Some(reason.clone());
                self.emit("sink_failed", None, json!({"reason": reason}));
            }
        }
        self.hard.cancel();
        self.cancel.close();
        let deadline = self.clock.now_ms() + self.config.shutdown_grace_ms;
        while !self.tasks.is_empty() {
            let clock = self.clock.clone();
            tokio::select! { biased;
                Some(output) = self.output.recv() => {
                    match output {
                        Output::Audio(packet) => { self.emit("tts_chunk", Some(packet.chunk.identity), json!(packet.chunk)); self.stale(packet.chunk.identity, "tts_chunk"); }
                        Output::Text(id, _) | Output::TtsDone(id) => self.stale(id, "provider_output"),
                        _ => {}
                    }
                }
                Some(result) = self.tasks.join_next() => self.task_result(result),
                _ = clock.sleep_until(deadline) => {
                    self.tasks.abort_all();
                    while let Some(result) = self.tasks.join_next().await { self.task_result(result); }
                }
            }
        }
        self.output.close();
        while let Ok(output) = self.output.try_recv() {
            if let Output::Audio(packet) = output {
                self.emit(
                    "tts_chunk",
                    Some(packet.chunk.identity),
                    json!(packet.chunk),
                );
                self.stale(packet.chunk.identity, "tts_chunk");
            }
        }
        self.preroll.clear();
        let mut queues: Vec<_> = self.meters.iter().map(QueueMeter::snapshot).collect();
        queues.push(QueueSnapshot {
            name: "playback_audio_samples".into(),
            capacity: self.config.playback_samples,
            peak: self.playback.peak_samples,
        });
        self.emit(
            "session_closed",
            None,
            json!({"reason": reason, "active_tasks": 0, "queues": queues}),
        );
        drop(self.log_tx.take());
        let events = match journal.await {
            Ok(events) => events,
            Err(_) => {
                self.trace_complete = false;
                Vec::new()
            }
        };
        self.trace_complete &= !journal_failed.get();
        self.status.closed = true;
        self.status_tx.send_replace(self.status);
        SessionReport {
            events,
            replies: self.playback.replies,
            queues,
            close_reason: reason,
            active_tasks: self.tasks.len(),
            trace_complete: self.trace_complete,
        }
    }
}
