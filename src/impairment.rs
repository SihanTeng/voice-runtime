//! Deterministic input-link faults; at most one frame is held for adjacent reordering.
use crate::{
    audio::AudioFrame,
    clock::Clock,
    session::{SessionError, SessionHandle},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InputFaults {
    pub seed: u64,
    pub jitter_ms: u64,
    pub drop_every: Option<u64>,
    pub reorder_every: Option<u64>,
    pub drop_burst: Option<DropBurst>,
}
impl InputFaults {
    pub fn validate(&self) -> bool {
        self.drop_burst
            .as_ref()
            .is_none_or(|b| b.every > 0 && b.length > 0 && b.length < b.every)
            && self.jitter_ms < 20
            && [self.drop_every, self.reorder_every]
                .iter()
                .all(|n| n.is_none_or(|n| n >= 2))
    }
    pub fn enabled(&self) -> bool {
        self.drop_burst.is_some()
            || self.jitter_ms > 0
            || self.drop_every.is_some()
            || self.reorder_every.is_some()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DropBurst {
    pub every: u64,
    pub length: u64,
}

pub struct InputLink {
    config: InputFaults,
    sequence: u64,
    pending: Option<(AudioFrame, Option<bool>)>,
    pub peak_held_frames: usize,
}
impl InputLink {
    pub fn new(config: InputFaults) -> Result<Self, SessionError> {
        if !config.validate() {
            return Err(SessionError::Configuration);
        }
        Ok(Self {
            config,
            sequence: 0,
            pending: None,
            peak_held_frames: 0,
        })
    }
    pub async fn feed(
        &mut self,
        handle: &SessionHandle,
        clock: &dyn Clock,
        duration_ms: u64,
        amplitude: i16,
        truth: Option<bool>,
    ) -> Result<(), SessionError> {
        assert_eq!(duration_ms % 20, 0);
        let base = clock.now_ms();
        for index in 0..duration_ms / 20 {
            let seq = self.sequence;
            self.sequence += 1; // Lost packets still consume source sequence numbers.
            let capture = base + index * 20;
            let jitter = self
                .config
                .seed
                .wrapping_add(seq)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1)
                % (self.config.jitter_ms + 1);
            clock.sleep_until(capture + 20 + jitter).await;
            if self
                .config
                .drop_burst
                .as_ref()
                .is_some_and(|b| seq.wrapping_add(self.config.seed % b.every) % b.every < b.length)
                || self
                    .config
                    .drop_every
                    .is_some_and(|n| (seq + 1).is_multiple_of(n))
            {
                continue;
            }
            let frame = AudioFrame::new(seq, capture, vec![amplitude; 320])?;
            if let Some((older, older_truth)) = self.pending.take() {
                handle.send_measured_audio(frame, truth).await?;
                handle.send_measured_audio(older, older_truth).await?;
            } else if self
                .config
                .reorder_every
                .is_some_and(|n| (seq + 1).is_multiple_of(n))
            {
                self.pending = Some((frame, truth));
                self.peak_held_frames = 1;
            } else {
                handle.send_measured_audio(frame, truth).await?;
            }
        }
        // Segment/EOF boundary: do not strand a held frame while the driver waits.
        if let Some((frame, truth)) = self.pending.take() {
            handle.send_measured_audio(frame, truth).await?;
        }
        Ok(())
    }
}
