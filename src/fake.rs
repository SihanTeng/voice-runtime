//! Model interfaces are incremental; async transport timing and cancellation are separate.
use crate::{audio::AudioFrame, provider::ProviderError, transcript::AsrUpdate};
use serde::{Deserialize, Serialize};

pub trait VadProvider {
    fn classify(&mut self, frame: &AudioFrame) -> Result<bool, ProviderError>;
}
pub trait AsrProvider {
    fn accept(&mut self, frame: &AudioFrame, voiced: bool) -> Result<AsrUpdate, ProviderError>;
    fn finish(&mut self) -> Result<AsrUpdate, ProviderError>;
}
pub trait LlmProvider {
    fn next_text(&mut self) -> Result<Option<String>, ProviderError>;
}
pub trait TtsProvider {
    fn samples_per_word(&self) -> usize;
    fn render(
        &mut self,
        text: &str,
        offset: usize,
        samples: usize,
    ) -> Result<Vec<i16>, ProviderError>;
}
pub struct ResponseProviders {
    pub llm: Box<dyn LlmProvider>,
    pub tts: Box<dyn TtsProvider>,
}
pub trait ProviderFactory {
    fn recovery(&self, _text: &str) -> Option<ResponseProviders> {
        None
    }
    fn vad(&self) -> Box<dyn VadProvider>;
    fn asr(&self, ordinal: usize) -> Box<dyn AsrProvider>;
    fn llm(
        &self,
        ordinal: usize,
        transcript: &str,
        heard_history: &[String],
    ) -> Box<dyn LlmProvider>;
    fn tts(&self) -> Box<dyn TtsProvider>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TranscriptPoint {
    pub voiced_ms: u64,
    pub text: String,
    #[serde(default)]
    pub segment_id: u64,
    #[serde(default)]
    pub stability: Option<f32>,
    #[serde(default)]
    pub stable_prefix_bytes: usize,
    #[serde(default)]
    pub is_final: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnScript {
    pub partials: Vec<TranscriptPoint>,
    pub response: String,
    #[serde(default)]
    pub final_text: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FakeProviders {
    pub turns: Vec<TurnScript>,
    pub word_ms: u64,
    pub real_vad: bool,
}
impl Default for FakeProviders {
    fn default() -> Self {
        Self { turns: vec![TurnScript {
            partials: vec![TranscriptPoint { voiced_ms: 0, text: "Please book it for".into(), ..Default::default() },
                TranscriptPoint { voiced_ms: 800, text: "Please book it for Wednesday afternoon.".into(), ..Default::default() }],
            final_text: None, response: "Certainly I can arrange your booking for Wednesday afternoon and send you all the details once everything has been confirmed.".into(),
        }, TurnScript {
            partials: vec![TranscriptPoint { voiced_ms: 0, text: "Stop. Make it".into(), ..Default::default() },
                TranscriptPoint { voiced_ms: 600, text: "Stop. Make it Friday instead.".into(), ..Default::default() }],
            final_text: None, response: "Understood. I will make it Friday instead.".into(),
        }], word_ms: 200, real_vad: false }
    }
}

pub struct EnergyVad;
impl FakeProviders {
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.turns.len() > 32
            || !(20..=10_000).contains(&self.word_ms)
            || !self.word_ms.is_multiple_of(20)
            || self.turns.iter().any(|t| {
                t.response.len() > 16_384
                    || t.partials.len() > 256
                    || t.final_text.as_ref().is_some_and(|t| t.len() > 16_384)
                    || t.partials.iter().any(|p| {
                        p.text.len() > 16_384
                            || !p.text.is_char_boundary(p.stable_prefix_bytes)
                            || p.stability
                                .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
                    })
                    || t.partials
                        .windows(2)
                        .any(|p| p[0].voiced_ms >= p[1].voiced_ms)
            })
        {
            return Err(ProviderError::Protocol(
                "invalid or oversized fake provider script".into(),
            ));
        }
        Ok(())
    }
}
#[cfg(not(feature = "real-vad"))]
struct MissingVad;
#[cfg(not(feature = "real-vad"))]
impl VadProvider for MissingVad {
    fn classify(&mut self, _: &AudioFrame) -> Result<bool, ProviderError> {
        Err(ProviderError::Protocol(
            "real-vad feature is required".into(),
        ))
    }
}
impl VadProvider for EnergyVad {
    fn classify(&mut self, frame: &AudioFrame) -> Result<bool, ProviderError> {
        Ok(frame.samples.iter().any(|s| s.unsigned_abs() >= 1000))
    }
}

#[cfg(feature = "real-vad")]
pub struct WebRtcVad(webrtc_vad::Vad);
#[cfg(feature = "real-vad")]
impl Default for WebRtcVad {
    fn default() -> Self {
        Self(webrtc_vad::Vad::new_with_rate_and_mode(
            webrtc_vad::SampleRate::Rate16kHz,
            webrtc_vad::VadMode::Aggressive,
        ))
    }
}
#[cfg(feature = "real-vad")]
impl VadProvider for WebRtcVad {
    fn classify(&mut self, frame: &AudioFrame) -> Result<bool, ProviderError> {
        self.0
            .is_voice_segment(&frame.samples)
            .map_err(|_| ProviderError::Protocol("invalid VAD frame".into()))
    }
}

struct ScriptAsr {
    points: Vec<TranscriptPoint>,
    voiced_samples: u64,
    update: AsrUpdate,
    final_text: Option<String>,
}
impl AsrProvider for ScriptAsr {
    fn accept(&mut self, frame: &AudioFrame, voiced: bool) -> Result<AsrUpdate, ProviderError> {
        if voiced {
            self.voiced_samples += frame.valid_samples as u64;
        }
        if let Some(point) = self
            .points
            .iter()
            .rev()
            .find(|p| p.voiced_ms <= self.voiced_samples / 16)
        {
            self.update.text.clone_from(&point.text);
            self.update.segment_id = point.segment_id;
            self.update.stability = point.stability;
            self.update.stable_prefix_bytes = point.stable_prefix_bytes;
            self.update.is_final = point.is_final;
        }
        self.update.revision += 1;
        self.update.through_sequence = frame.sequence;
        Ok(self.update.clone())
    }
    fn finish(&mut self) -> Result<AsrUpdate, ProviderError> {
        self.update.revision += 1;
        if let Some(text) = &self.final_text {
            self.update.text.clone_from(text);
        }
        self.update.is_final = true;
        self.update.stable_prefix_bytes = self.update.text.len();
        Ok(self.update.clone())
    }
}
struct ScriptLlm {
    words: std::vec::IntoIter<String>,
}
impl LlmProvider for ScriptLlm {
    fn next_text(&mut self) -> Result<Option<String>, ProviderError> {
        Ok(self.words.next())
    }
}
struct ToneTts {
    samples: usize,
}
impl TtsProvider for ToneTts {
    fn samples_per_word(&self) -> usize {
        self.samples
    }
    fn render(
        &mut self,
        _: &str,
        offset: usize,
        samples: usize,
    ) -> Result<Vec<i16>, ProviderError> {
        // Audible deterministic square wave, not speech. Text alignment is synthetic.
        Ok((offset..offset + samples)
            .map(|i| if i % 40 < 20 { 400 } else { -400 })
            .collect())
    }
}
impl ProviderFactory for FakeProviders {
    fn recovery(&self, text: &str) -> Option<ResponseProviders> {
        Some(ResponseProviders {
            llm: Box::new(ScriptLlm {
                words: text
                    .split_inclusive(char::is_whitespace)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
                    .into_iter(),
            }),
            tts: Box::new(ToneTts {
                samples: self.word_ms as usize * 16,
            }),
        })
    }
    fn vad(&self) -> Box<dyn VadProvider> {
        #[cfg(not(feature = "real-vad"))]
        if self.real_vad {
            return Box::new(MissingVad);
        }
        #[cfg(feature = "real-vad")]
        if self.real_vad {
            return Box::<WebRtcVad>::default();
        }
        Box::new(EnergyVad)
    }
    fn asr(&self, ordinal: usize) -> Box<dyn AsrProvider> {
        Box::new(ScriptAsr {
            points: self
                .turns
                .get(ordinal)
                .map(|t| t.partials.clone())
                .unwrap_or_default(),
            voiced_samples: 0,
            update: AsrUpdate::default(),
            final_text: self.turns.get(ordinal).and_then(|t| t.final_text.clone()),
        })
    }
    fn llm(&self, ordinal: usize, _: &str, _: &[String]) -> Box<dyn LlmProvider> {
        let words = self
            .turns
            .get(ordinal)
            .map(|t| {
                t.response
                    .split_inclusive(char::is_whitespace)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Box::new(ScriptLlm {
            words: words.into_iter(),
        })
    }
    fn tts(&self) -> Box<dyn TtsProvider> {
        Box::new(ToneTts {
            samples: self.word_ms as usize * 16,
        })
    }
}
