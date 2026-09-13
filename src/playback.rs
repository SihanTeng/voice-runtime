//! Playback is an owner-thread state machine. No provider can write to its sink.
use crate::{
    audio::{FRAME_SAMPLES, SAMPLE_RATE},
    event::Identity,
};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, ops::Range};
use tokio::sync::OwnedSemaphorePermit;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioChunk {
    pub identity: Identity,
    pub sequence: u64,
    pub text_range: Range<usize>,
    pub word_samples: usize,
    pub word_offset: usize,
    pub sample_start: u64,
    pub samples: Vec<i16>,
}

pub struct Packet {
    pub chunk: AudioChunk,
    /// Held across the transport, queue and current playback frame.
    pub budget: Option<OwnedSemaphorePermit>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChunkRecord {
    pub sequence: u64,
    pub text_range: Range<usize>,
    pub sample_start: u64,
    pub samples: usize,
    pub word_samples: usize,
    pub word_offset: usize,
    pub enqueued: bool,
    pub played_samples: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyRecord {
    pub identity: Identity,
    pub generated_text: String,
    pub chunks: Vec<ChunkRecord>,
    pub interrupted: bool,
}

impl ReplyRecord {
    pub fn partial_words(&self) -> Vec<(Range<usize>, usize, usize)> {
        let mut words = std::collections::BTreeMap::new();
        for chunk in &self.chunks {
            let entry = words
                .entry((chunk.text_range.start, chunk.text_range.end))
                .or_insert((0, chunk.word_samples));
            entry.0 += chunk.played_samples;
        }
        words
            .into_iter()
            .filter_map(|((start, end), (played, total))| {
                (played > 0 && played < total).then_some((start..end, played, total))
            })
            .collect()
    }
    pub fn new(identity: Identity) -> Self {
        Self {
            identity,
            generated_text: String::new(),
            chunks: Vec::new(),
            interrupted: false,
        }
    }
    pub fn played_samples(&self) -> usize {
        self.chunks.iter().map(|c| c.played_samples).sum()
    }
    /// Only completely consumed words are admitted to conversational history.
    pub fn heard_ranges(&self) -> Vec<Range<usize>> {
        let mut heard = Vec::new();
        let mut range: Option<Range<usize>> = None;
        let mut consumed = 0;
        for chunk in &self.chunks {
            if range.as_ref() != Some(&chunk.text_range) {
                range = Some(chunk.text_range.clone());
                consumed = 0;
            }
            consumed += chunk.played_samples;
            if consumed == chunk.word_samples {
                heard.push(chunk.text_range.clone());
            }
        }
        heard
    }
    pub fn heard_text(&self) -> String {
        self.heard_ranges()
            .into_iter()
            .filter_map(|r| self.generated_text.get(r))
            .collect()
    }
    pub fn enqueued_text(&self) -> String {
        let mut last = None;
        self.chunks
            .iter()
            .filter(|c| c.enqueued)
            .filter_map(|c| {
                if last.as_ref() == Some(&c.text_range) {
                    return None;
                }
                last = Some(c.text_range.clone());
                self.generated_text.get(c.text_range.clone())
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("invalid or stale playback packet")]
    InvalidPacket,
    #[error("playback queue or ledger capacity exceeded")]
    Capacity,
    #[error("sink failure: {0}")]
    Sink(String),
}

pub trait PlaybackSink {
    /// The slice is consumed during [start_ms, end_ms). A call after close is invalid.
    fn consume(&mut self, samples: &[i16], start_ms: u64, end_ms: u64)
    -> Result<(), PlaybackError>;
    fn close(&mut self) -> Result<(), PlaybackError>;
}

#[derive(Default)]
pub struct CountingSink {
    pub samples: u64,
    closed: bool,
}
impl PlaybackSink for CountingSink {
    fn consume(&mut self, samples: &[i16], _: u64, _: u64) -> Result<(), PlaybackError> {
        if self.closed {
            return Err(PlaybackError::Sink("write after close".into()));
        }
        self.samples += samples.len() as u64;
        Ok(())
    }
    fn close(&mut self) -> Result<(), PlaybackError> {
        self.closed = true;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Consumption {
    pub identity: Identity,
    pub chunk_sequence: u64,
    pub sample_start: u64,
    pub samples: usize,
    pub start_ms: u64,
    pub end_ms: u64,
}

struct Current {
    packet: Packet,
    started: u64,
    played: usize,
}

pub struct Playback {
    sink: Box<dyn PlaybackSink>,
    active: Option<Identity>,
    queue: VecDeque<Packet>,
    current: Option<Current>,
    closed: bool,
    capacity_samples: usize,
    max_chunks: usize,
    pub peak_samples: usize,
    pub replies: Vec<ReplyRecord>,
    max_replies: usize,
    expected_sample: u64,
}

impl Playback {
    pub fn new(
        sink: Box<dyn PlaybackSink>,
        capacity_samples: usize,
        max_chunks: usize,
        max_replies: usize,
    ) -> Self {
        Self {
            sink,
            active: None,
            queue: VecDeque::new(),
            current: None,
            closed: false,
            capacity_samples,
            max_chunks,
            peak_samples: 0,
            replies: Vec::new(),
            max_replies,
            expected_sample: 0,
        }
    }
    pub fn activate(&mut self, identity: Identity) -> Result<(), PlaybackError> {
        if self.closed || self.active.is_some() || self.replies.len() >= self.max_replies {
            return Err(PlaybackError::Capacity);
        }
        self.active = Some(identity);
        self.expected_sample = 0;
        self.replies.push(ReplyRecord::new(identity));
        Ok(())
    }
    pub fn active(&self) -> Option<Identity> {
        self.active
    }
    pub fn is_idle(&self) -> bool {
        self.current.is_none() && self.queue.is_empty()
    }
    pub fn depth_samples(&self) -> usize {
        self.queue
            .iter()
            .map(|p| p.chunk.samples.len())
            .sum::<usize>()
            + self
                .current
                .as_ref()
                .map_or(0, |c| c.packet.chunk.samples.len() - c.played)
    }
    pub fn append_text(
        &mut self,
        identity: Identity,
        text: &str,
        max_bytes: usize,
    ) -> Result<(), PlaybackError> {
        if self.active != Some(identity) {
            return Err(PlaybackError::InvalidPacket);
        }
        let reply = self
            .replies
            .last_mut()
            .ok_or(PlaybackError::InvalidPacket)?;
        if reply.generated_text.len() + text.len() > max_bytes {
            return Err(PlaybackError::Capacity);
        }
        reply.generated_text.push_str(text);
        Ok(())
    }
    pub fn enqueue(&mut self, packet: Packet) -> Result<(), PlaybackError> {
        let c = &packet.chunk;
        if self.closed
            || self.active != Some(c.identity)
            || c.samples.is_empty()
            || c.samples.len() > FRAME_SAMPLES
            || c.sample_start != self.expected_sample
            || c.word_offset
                .checked_add(c.samples.len())
                .is_none_or(|n| n > c.word_samples)
            || c.text_range.is_empty()
        {
            return Err(PlaybackError::InvalidPacket);
        }
        if self.depth_samples() + c.samples.len() > self.capacity_samples {
            return Err(PlaybackError::Capacity);
        }
        let reply = self
            .replies
            .last_mut()
            .ok_or(PlaybackError::InvalidPacket)?;
        if c.sequence != reply.chunks.len() as u64
            || reply.generated_text.get(c.text_range.clone()).is_none()
        {
            return Err(PlaybackError::InvalidPacket);
        }
        if reply.chunks.len() >= self.max_chunks {
            return Err(PlaybackError::Capacity);
        }
        let valid_alignment = match reply.chunks.last() {
            Some(previous) if previous.text_range == c.text_range => {
                c.word_samples == previous.word_samples
                    && c.word_offset == previous.word_offset + previous.samples
            }
            Some(previous) => {
                c.word_offset == 0
                    && c.text_range.start == previous.text_range.end
                    && previous.word_offset + previous.samples == previous.word_samples
            }
            None => c.word_offset == 0 && c.text_range.start == 0,
        };
        if !valid_alignment {
            return Err(PlaybackError::InvalidPacket);
        }
        reply.chunks.push(ChunkRecord {
            sequence: c.sequence,
            text_range: c.text_range.clone(),
            sample_start: c.sample_start,
            samples: c.samples.len(),
            word_samples: c.word_samples,
            word_offset: c.word_offset,
            enqueued: true,
            played_samples: 0,
            truncated: false,
        });
        self.expected_sample += c.samples.len() as u64;
        self.queue.push_back(packet);
        self.peak_samples = self.peak_samples.max(self.depth_samples());
        Ok(())
    }
    /// Returns a newly started frame. Starting has no claim of completed consumption.
    pub fn start_ready(&mut self, now: u64) -> Option<(Identity, u64)> {
        if self.closed || self.current.is_some() {
            return None;
        }
        let packet = self.queue.pop_front()?;
        if Some(packet.chunk.identity) != self.active {
            return None;
        }
        let result = (packet.chunk.identity, packet.chunk.sequence);
        self.current = Some(Current {
            packet,
            started: now,
            played: 0,
        });
        Some(result)
    }
    pub fn deadline(&self) -> Option<u64> {
        self.current.as_ref().map(|c| {
            c.started + (c.packet.chunk.samples.len() as u64 * 1000).div_ceil(SAMPLE_RATE as u64)
        })
    }
    pub fn settle(&mut self, now: u64) -> Result<Option<Consumption>, PlaybackError> {
        let Some(current) = &mut self.current else {
            return Ok(None);
        };
        let chunk = &current.packet.chunk;
        let consumed = ((now.saturating_sub(current.started) * SAMPLE_RATE as u64 / 1000) as usize)
            .min(chunk.samples.len());
        if consumed <= current.played {
            return Ok(None);
        }
        let start_ms = current.started + current.played as u64 * 1000 / SAMPLE_RATE as u64;
        let end_ms = current.started + consumed as u64 * 1000 / SAMPLE_RATE as u64;
        self.sink
            .consume(&chunk.samples[current.played..consumed], start_ms, end_ms)?;
        let progress = Consumption {
            identity: chunk.identity,
            chunk_sequence: chunk.sequence,
            sample_start: chunk.sample_start + current.played as u64,
            samples: consumed - current.played,
            start_ms,
            end_ms,
        };
        self.replies
            .last_mut()
            .ok_or(PlaybackError::InvalidPacket)?
            .chunks[chunk.sequence as usize]
            .played_samples += progress.samples;
        current.played = consumed;
        if consumed == chunk.samples.len() {
            self.current = None;
        }
        Ok(Some(progress))
    }
    /// Linearized by the owner: settle only the preceding audible interval, revoke, discard.
    pub fn stop(
        &mut self,
        now: u64,
        interrupted: bool,
    ) -> Result<Option<Consumption>, PlaybackError> {
        self.active = None;
        let result = self.settle(now);
        if let Some(reply) = self.replies.last_mut() {
            reply.interrupted |= interrupted;
            for chunk in &mut reply.chunks {
                chunk.truncated = chunk.played_samples < chunk.samples;
            }
        }
        self.current = None;
        self.queue.clear();
        result
    }
    pub fn close(&mut self, now: u64) -> Result<Option<Consumption>, PlaybackError> {
        let progress = self.stop(now, false);
        self.closed = true;
        self.sink.close()?;
        progress
    }
}
