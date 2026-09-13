//! Single-owner runtime. Private modules group transitions without introducing tasks or locks.
use crate::event::EventData;
mod config;
mod generation;
mod handle;
mod input;
mod lifecycle;
mod recovery;
pub use recovery::RecoveryPolicy;

use crate::{
    audio::{AudioFrame, FrameValidator},
    clock::Clock,
    event::{Event, Identity},
    fake::ProviderFactory,
    playback::{Playback, PlaybackSink, ReplyRecord},
    queue::{self, QueueMeter, QueueSnapshot},
    transport::{CapturedFrame, Output, WorkerContext},
};
pub use config::SessionConfig;
use generation::Generation;
pub use handle::{SessionHandle, Snapshot};
use input::InputTurn;
use lifecycle::TaskResult;
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::VecDeque, rc::Rc};
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

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

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionReport {
    pub events: Vec<Event>,
    pub replies: Vec<ReplyRecord>,
    pub queues: Vec<QueueSnapshot>,
    pub close_reason: String,
    pub active_tasks: usize,
    pub trace_complete: bool,
}

impl SessionReport {
    /// A failure is handled only when its replacement clarification actually completed.
    pub fn has_unrecovered_failure(&self) -> bool {
        let completed: std::collections::BTreeSet<_> = self
            .events
            .iter()
            .filter(|e| e.event_type == "playback_stopped" && e.payload["reason"] == "completed")
            .filter_map(|e| e.generation_id)
            .collect();
        let recovered: std::collections::BTreeSet<_> = self
            .events
            .iter()
            .filter(|e| {
                e.event_type == "recovery_started"
                    && e.generation_id.is_some_and(|id| completed.contains(&id))
            })
            .filter_map(|e| e.payload["failed_generation_id"].as_u64())
            .collect();
        self.events.iter().any(|event| {
            if event.event_type == "turn_failed" {
                return event.payload["reason"] != "session_closing";
            }
            if event.event_type != "provider_failed" {
                return false;
            }
            event
                .generation_id
                .is_none_or(|id| !recovered.contains(&id))
        })
    }
}

pub struct Session {
    config: SessionConfig,
    factory: Rc<dyn ProviderFactory>,
    clock: Rc<dyn Clock>,
    playback: Playback,
    close: CancellationToken,
    close_reason: Rc<Cell<&'static str>>,
    hard: CancellationToken,
    raw: Option<queue::Receiver<CapturedFrame>>,
    cancel: queue::Receiver<()>,
    out_tx: queue::Sender<Output>,
    output: queue::Receiver<Output>,
    status_tx: watch::Sender<Snapshot>,
    status: Snapshot,
    tasks: JoinSet<TaskResult>,
    meters: Vec<QueueMeter>,
    turn: Option<InputTurn>,
    generation: Option<Generation>,
    preroll: VecDeque<(AudioFrame, bool)>,
    preroll_meter: QueueMeter,
    frame_validator: FrameValidator,
    admitted_frames: Rc<Cell<u64>>,
    processed_frames: u64,
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
        let (input, raw, im) =
            queue::channel_with_clock("input_audio", config.input_capacity, clock.clone());
        let (out_tx, output, om) =
            queue::channel_with_clock("provider_events", config.event_capacity, clock.clone());
        let (cancel_tx, cancel, cm) = queue::channel_with_clock("cancel_control", 1, clock.clone());
        let (status_tx, snapshot) = watch::channel(Snapshot::default());
        let close = CancellationToken::new();
        let reason = Rc::new(Cell::new("active_close"));
        let admitted_frames = Rc::new(Cell::new(0));
        let handle = SessionHandle::new(
            input,
            cancel_tx,
            snapshot,
            close.clone(),
            reason.clone(),
            admitted_frames.clone(),
        );
        let playback = Playback::new(
            sink,
            config.playback_samples,
            config.max_chunks_per_reply,
            config.max_turns * if config.recovery.is_some() { 2 } else { 1 },
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
                admitted_frames,
                processed_frames: 0,
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
    fn emit(&mut self, id: Option<Identity>, data: EventData) {
        let event = Event::new(
            self.clock.now_ms(),
            self.config.session_id.clone(),
            id,
            self.log_sequence,
            data,
        );
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
    fn stale(&mut self, id: Identity, kind: &str) {
        self.emit(
            Some(id),
            EventData::StaleEventDropped {
                source_type: kind.to_string(),
            },
        );
    }
    fn on_output(&mut self, output: Output) {
        match output {
            Output::Vad(frame, voiced) => self.on_vad(frame, voiced),
            Output::Partial { turn, update } => self.on_partial(turn, update),
            Output::Final { turn, update } => self.on_final(turn, update),
            Output::TtsRequested(id, request_ms) => {
                if self.generation.as_ref().is_some_and(|g| g.id == id) {
                    self.emit(Some(id), EventData::TtsRequested { request_ms });
                } else {
                    self.stale(id, "tts_requested");
                }
            }
            Output::Text(id, text) => self.on_text(id, text),
            Output::Audio(packet) => self.on_audio(packet),
            Output::TtsDone(id) => self.on_tts_done(id),
        }
    }
    fn tick(&mut self) {
        self.tick_playback();
        self.maybe_interrupt();
        self.maybe_endpoint();
    }
}
