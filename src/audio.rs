use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SAMPLE_RATE: u32 = 16_000;
pub const FRAME_SAMPLES: usize = 320;
pub const FRAME_MS: u64 = 20;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AudioError {
    #[error("expected 320 samples and 1..=320 valid samples")]
    InvalidFrame,
    #[error("sequence and timestamp must increase monotonically")]
    NonMonotonic,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioFrame {
    pub sequence: u64,
    /// Capture start, in milliseconds relative to the session epoch.
    pub timestamp: u64,
    pub samples: Vec<i16>,
    pub valid_samples: usize,
}

impl AudioFrame {
    pub fn new(sequence: u64, timestamp: u64, samples: Vec<i16>) -> Result<Self, AudioError> {
        let frame = Self {
            sequence,
            timestamp,
            valid_samples: samples.len(),
            samples,
        };
        frame.validate()?;
        Ok(frame)
    }

    pub fn validate(&self) -> Result<(), AudioError> {
        if self.samples.len() != FRAME_SAMPLES || !(1..=FRAME_SAMPLES).contains(&self.valid_samples)
        {
            return Err(AudioError::InvalidFrame);
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct FrameValidator(Option<(u64, u64)>);

impl FrameValidator {
    /// Returns the number of missing sequence numbers; does not synthesize speech.
    pub fn accept(&mut self, frame: &AudioFrame) -> Result<u64, AudioError> {
        frame.validate()?;
        let mut missing = 0;
        if let Some((sequence, timestamp)) = self.0 {
            if frame.sequence <= sequence || frame.timestamp <= timestamp {
                return Err(AudioError::NonMonotonic);
            }
            missing = frame.sequence - sequence - 1;
        }
        self.0 = Some((frame.sequence, frame.timestamp));
        Ok(missing)
    }
}
