//! Task supervision, priority event loop, and joined shutdown.
use super::{Session, SessionReport};
use crate::event::EventData;
use crate::{
    audio::FRAME_MS,
    event::Identity,
    provider::ProviderError,
    queue::{self, QueueMeter, QueueSnapshot},
    transport::{self, Output},
};
use std::{cell::Cell, rc::Rc};
use tokio_util::sync::CancellationToken;

pub(super) struct TaskResult {
    stage: &'static str,
    turn: Option<u64>,
    generation: Option<Identity>,
    result: Result<(), ProviderError>,
}

impl Session {
    pub(super) fn spawn(
        &mut self,
        stage: &'static str,
        turn: Option<u64>,
        generation: Option<Identity>,
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
                generation,
                result: future.await,
            }
        });
    }
    fn task_result(&mut self, result: Result<TaskResult, tokio::task::JoinError>) {
        match result {
            Ok(task) => {
                self.emit(
                    task.generation.or_else(|| {
                        task.turn.map(|turn_id| Identity {
                            turn_id,
                            generation_id: 0,
                        })
                    }),
                    EventData::TaskExited {
                        stage: task.stage.to_string(),
                        error: task.result.as_ref().err().map(ToString::to_string),
                    },
                );
                if let Err(error) = task.result
                    && error != ProviderError::Cancelled
                {
                    self.emit(
                        task.generation.or_else(|| {
                            task.turn.map(|turn_id| Identity {
                                turn_id,
                                generation_id: 0,
                            })
                        }),
                        EventData::ProviderFailed {
                            stage: task.stage.to_string(),
                            reason: error.to_string(),
                        },
                    );
                    if task.stage == "vad" {
                        self.failed = Some(error.to_string());
                    }
                    if self.turn.as_ref().is_some_and(|t| Some(t.id) == task.turn) {
                        self.reject_turn(&error.to_string());
                    }
                    if self.generation.as_ref().is_some_and(|g| {
                        task.generation
                            .map_or(Some(g.id.turn_id) == task.turn, |id| g.id == id)
                    }) {
                        self.cancel_current(&error.to_string());
                    }
                }
            }
            Err(error) if error.is_panic() => {
                self.failed = Some("background_task_panicked".into());
                self.emit(
                    None,
                    EventData::TaskPanicked {
                        error: error.to_string(),
                    },
                );
            }
            Err(_) => self.emit(None, EventData::TaskAborted {}),
        }
    }
    pub async fn run(mut self) -> SessionReport {
        let (log_tx, mut log_rx, lm) =
            queue::channel_with_clock("journal", self.config.log_capacity, self.clock.clone());
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
        self.emit(
            None,
            EventData::SessionStarted {
                config: Box::new(self.config.clone()),
                clock: "monotonic_ms".to_string(),
                playback: "simulated_consumption".to_string(),
            },
        );
        let raw = self.raw.take().expect("raw receiver owned once");
        self.spawn(
            "vad",
            None,
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
        let reason = self.failed.clone().unwrap_or_else(|| {
            if journal_failed.get() {
                "journal_failure".into()
            } else {
                self.close_reason.get().into()
            }
        });
        let reason = self.shutdown(reason).await;
        let mut queues: Vec<_> = self.meters.iter().map(QueueMeter::snapshot).collect();
        queues.push(QueueSnapshot {
            name: "playback_audio_samples".into(),
            capacity: self.config.playback_samples,
            peak: self.playback.peak_samples,
            max_wait_ms: None,
        });
        self.emit(
            None,
            EventData::SessionClosed {
                reason: reason.to_string(),
                active_tasks: 0,
                queues: queues.clone(),
            },
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
    /// Seal input and playback before the first await; abort overdue tasks and join them.
    async fn shutdown(&mut self, mut reason: String) -> String {
        self.close.cancel();
        self.cancel_current(&reason);
        self.reject_turn("session_closing");
        match self.playback.close(self.clock.now_ms()) {
            Ok(p) => self.progress(p),
            Err(e) => {
                reason = e.to_string();
                self.failed = Some(reason.clone());
                self.emit(
                    None,
                    EventData::SinkFailed {
                        reason: reason.to_string(),
                    },
                );
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
                        Output::Audio(packet) => { self.emit(Some(packet.chunk.identity), EventData::TtsChunk(packet.chunk.clone())); self.record_rejected_audio(&packet, "stale_generation"); self.stale(packet.chunk.identity, "tts_chunk"); }
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
                    Some(packet.chunk.identity),
                    EventData::TtsChunk(packet.chunk.clone()),
                );
                self.stale(packet.chunk.identity, "tts_chunk");
                self.record_rejected_audio(&packet, "stale_generation");
            }
        }
        self.preroll.clear();
        reason
    }
}
