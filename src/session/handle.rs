//! Client requests and status; dropping the last client requests shutdown.
use super::SessionError;
use crate::{audio::AudioFrame, queue, transport::CapturedFrame};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, rc::Rc};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

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
    admitted_frames: Rc<Cell<u64>>,
}
impl SessionHandle {
    pub(super) fn new(
        input: queue::Sender<CapturedFrame>,
        cancel: queue::Sender<()>,
        snapshot: watch::Receiver<Snapshot>,
        close: CancellationToken,
        reason: Rc<Cell<&'static str>>,
        admitted_frames: Rc<Cell<u64>>,
    ) -> Self {
        Self {
            client: Rc::new(Client { close, reason }),
            input,
            cancel,
            snapshot,
            admitted_frames,
        }
    }
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
            result = self.input.send(CapturedFrame { audio, speech_truth }) => {
                result.map_err(|_| SessionError::Closed)?;
                // No await between queue admission and this owner-visible watermark.
                self.admitted_frames.set(self.admitted_frames.get() + 1);
                Ok(())
            },
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
