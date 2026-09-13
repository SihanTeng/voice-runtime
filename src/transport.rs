//! Supervised provider workers. All output routes pass through the session owner.
use crate::{
    audio::{AudioFrame, FRAME_SAMPLES},
    clock::Clock,
    event::Identity,
    fake::{AsrProvider, LlmProvider, TtsProvider, VadProvider},
    playback::{AudioChunk, Packet},
    provider::{Pacer, ProviderError, Timing},
    queue::{QueueMeter, Sender},
};
use std::{rc::Rc, sync::Arc};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub struct CapturedFrame {
    pub audio: AudioFrame,
    /// Measurement oracle only. Never passed to VAD, ASR, or endpointing.
    pub speech_truth: Option<bool>,
}
pub enum AsrInput {
    Frame(AudioFrame, bool),
    Finish,
}
pub enum Output {
    Vad(CapturedFrame, bool),
    Partial {
        turn: u64,
        text: String,
        through: u64,
    },
    Final {
        turn: u64,
        text: String,
    },
    TtsRequested(Identity, u64),
    Text(Identity, String),
    Audio(Packet),
    TtsDone(Identity),
}
#[derive(Clone)]
pub struct WorkerContext {
    pub clock: Rc<dyn Clock>,
    pub hard: CancellationToken,
    pub soft: CancellationToken,
    pub output: Sender<Output>,
    pub backpressure_ms: u64,
    pub max_text_bytes: usize,
}
impl WorkerContext {
    pub async fn send<T>(&self, sender: &Sender<T>, value: T) -> Result<(), ProviderError> {
        let deadline = self.clock.now_ms() + self.backpressure_ms;
        tokio::select! {
            biased;
            _ = self.hard.cancelled() => Err(ProviderError::Cancelled),
            result = sender.send(value) => result.map_err(|_| ProviderError::Downstream),
            _ = self.clock.sleep_until(deadline) => Err(ProviderError::Downstream),
        }
    }
    pub async fn emit(&self, value: Output) -> Result<(), ProviderError> {
        self.send(&self.output, value).await
    }
}

pub async fn vad_worker(
    mut provider: Box<dyn VadProvider>,
    mut input: crate::queue::Receiver<CapturedFrame>,
    timing: Timing,
    ctx: WorkerContext,
) -> Result<(), ProviderError> {
    let mut pacer = Pacer::new(timing, ctx.clock.clone());
    loop {
        let frame = tokio::select! { biased;
            _ = ctx.hard.cancelled() => return Ok(()),
            frame = input.recv() => match frame { Some(f) => f, None => return Ok(()) }
        };
        pacer.next(&ctx.soft, &ctx.hard).await?;
        let voiced = provider.classify(&frame.audio)?;
        ctx.emit(Output::Vad(frame, voiced)).await?;
    }
}

pub async fn asr_worker(
    mut provider: Box<dyn AsrProvider>,
    turn: u64,
    mut input: crate::queue::Receiver<AsrInput>,
    timing: Timing,
    ctx: WorkerContext,
) -> Result<(), ProviderError> {
    let mut pacer = Pacer::new(timing, ctx.clock.clone());
    let mut last_frame: Option<(AudioFrame, bool)> = None;
    loop {
        let message = tokio::select! { biased;
            _ = ctx.hard.cancelled() => return Ok(()),
            _ = ctx.soft.cancelled() => match &last_frame {
                Some((frame, voiced)) => AsrInput::Frame(frame.clone(), *voiced),
                None => return Ok(()),
            },
            message = input.recv() => match message { Some(m) => m, None => return Ok(()) }
        };
        if let AsrInput::Frame(frame, voiced) = &message {
            last_frame = Some((frame.clone(), *voiced));
        }
        pacer.next(&ctx.soft, &ctx.hard).await?;
        let output = match message {
            AsrInput::Frame(frame, voiced) => {
                let text = provider.accept(&frame, voiced)?;
                if text.len() > ctx.max_text_bytes {
                    return Err(ProviderError::Protocol("ASR text limit".into()));
                }
                Output::Partial {
                    turn,
                    text,
                    through: frame.sequence,
                }
            }
            AsrInput::Finish => {
                let text = provider.finish()?;
                if text.len() > ctx.max_text_bytes {
                    return Err(ProviderError::Protocol("ASR final text limit".into()));
                }
                ctx.emit(Output::Final { turn, text }).await?;
                return Ok(());
            }
        };
        ctx.emit(output).await?;
    }
}

pub async fn llm_worker(
    mut provider: Box<dyn LlmProvider>,
    id: Identity,
    text: Sender<String>,
    timing: Timing,
    ctx: WorkerContext,
) -> Result<(), ProviderError> {
    let mut pacer = Pacer::new(timing, ctx.clock.clone());
    let mut bytes = 0;
    while let Some(chunk) = provider.next_text()? {
        if chunk.is_empty() || chunk.len() > 4096 {
            return Err(ProviderError::Protocol("LLM chunk size".into()));
        }
        bytes += chunk.len();
        if bytes > ctx.max_text_bytes {
            return Err(ProviderError::Protocol("LLM text limit".into()));
        }
        pacer.next(&ctx.soft, &ctx.hard).await?;
        ctx.emit(Output::Text(id, chunk.clone())).await?;
        ctx.send(&text, chunk).await?;
    }
    Ok(())
}

pub async fn tts_worker(
    mut provider: Box<dyn TtsProvider>,
    id: Identity,
    mut text: crate::queue::Receiver<String>,
    budget: Arc<Semaphore>,
    timing: Timing,
    ctx: WorkerContext,
    budget_meter: QueueMeter,
) -> Result<(), ProviderError> {
    let mut pacer = Pacer::new(timing, ctx.clock.clone());
    let word_samples = provider.samples_per_word();
    if word_samples == 0 || word_samples > 160_000 {
        return Err(ProviderError::Protocol("TTS word duration".into()));
    }
    let mut text_offset = 0;
    let mut sample_start = 0;
    let mut sequence = 0;
    let mut last_word = String::new();
    loop {
        let word = tokio::select! { biased;
            _ = ctx.hard.cancelled() => return Ok(()),
            value = text.recv() => match value {
                Some(value) => value,
                None if ctx.soft.is_cancelled() && !last_word.is_empty() => last_word.clone(),
                None => { ctx.emit(Output::TtsDone(id)).await?; return Ok(()); }
            }
        };
        if sequence == 0 {
            ctx.emit(Output::TtsRequested(id, ctx.clock.now_ms()))
                .await?;
        }
        last_word.clone_from(&word);
        for offset in (0..word_samples).step_by(FRAME_SAMPLES) {
            let samples = FRAME_SAMPLES.min(word_samples - offset);
            // Reserve BEFORE asking for the next packet. A late packet is still bounded.
            let deadline = ctx.clock.now_ms() + ctx.backpressure_ms;
            let permit = tokio::select! { biased;
                _ = ctx.hard.cancelled() => return Ok(()),
                permit = budget.clone().acquire_many_owned(samples as u32) => permit.map_err(|_| ProviderError::Downstream)?,
                _ = ctx.clock.sleep_until(deadline) => return Err(ProviderError::Downstream),
            };
            budget_meter.observe(budget_meter.snapshot().capacity - budget.available_permits());
            pacer.next(&ctx.soft, &ctx.hard).await?;
            let pcm = provider.render(&word, offset, samples)?;
            if pcm.len() != samples {
                return Err(ProviderError::Protocol("TTS PCM length".into()));
            }
            ctx.emit(Output::Audio(Packet {
                chunk: AudioChunk {
                    identity: id,
                    sequence,
                    text_range: text_offset..text_offset + word.len(),
                    word_samples,
                    word_offset: offset,
                    sample_start,
                    samples: pcm,
                },
                budget: Some(permit),
            }))
            .await?;
            sample_start += samples as u64;
            sequence += 1;
        }
        text_offset += word.len();
    }
}
