use crate::{
    audio::{AudioFrame, FRAME_SAMPLES},
    audit,
    event::Event,
    playback::{AudioChunk, Consumption},
};
use std::io::{Read, Seek};

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("WAV must be mono PCM16, 16 kHz, no more than 300 seconds")]
    Format,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Hound(#[from] hound::Error),
    #[error("cannot export an invalid playback trace")]
    InvalidTrace,
}

pub struct WavSource<R> {
    reader: hound::WavReader<R>,
    sequence: u64,
    finished: bool,
}
impl<R: Read + Seek> WavSource<R> {
    pub fn new(reader: R) -> Result<Self, WavError> {
        let reader = hound::WavReader::new(reader)?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.sample_rate != 16_000
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
            || reader.duration() > 16_000 * 300
        {
            return Err(WavError::Format);
        }
        Ok(Self {
            reader,
            sequence: 0,
            finished: false,
        })
    }
}
impl<R: Read + Seek> Iterator for WavSource<R> {
    type Item = Result<AudioFrame, WavError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut pcm = Vec::with_capacity(FRAME_SAMPLES);
        for sample in self.reader.samples::<i16>().take(FRAME_SAMPLES) {
            match sample {
                Ok(sample) => pcm.push(sample),
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error.into()));
                }
            }
        }
        if pcm.is_empty() {
            self.finished = true;
            return None;
        }
        let valid_samples = pcm.len();
        pcm.resize(FRAME_SAMPLES, 0);
        let audio = AudioFrame {
            sequence: self.sequence,
            timestamp: self.sequence * 20,
            samples: pcm,
            valid_samples,
        };
        self.sequence += 1;
        Some(Ok(audio))
    }
}

/// Reconstruct only consumed PCM and its silence gaps; queued suffixes never enter the file.
pub fn export_played(events: &[Event], path: &std::path::Path) -> Result<(), WavError> {
    if !audit::analyze(events).violations.is_empty() {
        return Err(WavError::InvalidTrace);
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    let mut chunks = std::collections::BTreeMap::new();
    let mut written = 0;
    for event in events {
        if event.event_type == "tts_chunk" {
            let chunk: AudioChunk = serde_json::from_value(event.payload.clone())
                .map_err(|_| WavError::InvalidTrace)?;
            chunks.insert((chunk.identity.generation_id, chunk.sequence), chunk);
        } else if event.event_type == "playback_progress" {
            let p: Consumption = serde_json::from_value(event.payload.clone())
                .map_err(|_| WavError::InvalidTrace)?;
            let position = p.start_ms * 16;
            if position < written || position > 16_000 * 300 {
                return Err(WavError::InvalidTrace);
            }
            while written < position {
                writer.write_sample(0i16)?;
                written += 1;
            }
            let c = chunks
                .get(&(p.identity.generation_id, p.chunk_sequence))
                .ok_or(WavError::InvalidTrace)?;
            let offset = p
                .sample_start
                .checked_sub(c.sample_start)
                .ok_or(WavError::InvalidTrace)? as usize;
            let samples = c
                .samples
                .get(offset..offset + p.samples)
                .ok_or(WavError::InvalidTrace)?;
            for sample in samples {
                writer.write_sample(*sample)?;
                written += 1;
            }
        }
    }
    writer.finalize()?;
    Ok(())
}
