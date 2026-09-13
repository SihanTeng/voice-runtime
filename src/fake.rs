//! Model interfaces are incremental; async transport timing and cancellation are separate.
use crate::{audio::AudioFrame, provider::ProviderError};
use serde::{Deserialize, Serialize};

pub trait VadProvider {
    fn classify(&mut self, frame: &AudioFrame) -> Result<bool, ProviderError>;
}
pub trait AsrProvider {
    fn accept(&mut self, frame: &AudioFrame, voiced: bool) -> Result<String, ProviderError>;
    fn finish(&mut self) -> Result<String, ProviderError>;
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
pub trait ProviderFactory {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptPoint {
    pub voiced_ms: u64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnScript {
    pub partials: Vec<TranscriptPoint>,
    pub response: String,
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
            partials: vec![TranscriptPoint { voiced_ms: 0, text: "Please book it for".into() },
                TranscriptPoint { voiced_ms: 800, text: "Please book it for Wednesday afternoon.".into() }],
            response: "Certainly I can arrange your booking for Wednesday afternoon and send you all the details once everything has been confirmed.".into(),
        }, TurnScript {
            partials: vec![TranscriptPoint { voiced_ms: 0, text: "Stop. Make it".into() },
                TranscriptPoint { voiced_ms: 600, text: "Stop. Make it Friday instead.".into() }],
            response: "Understood. I will make it Friday instead.".into(),
        }], word_ms: 200, real_vad: false }
    }
}

pub struct EnergyVad;
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
    text: String,
}
impl AsrProvider for ScriptAsr {
    fn accept(&mut self, frame: &AudioFrame, voiced: bool) -> Result<String, ProviderError> {
        if voiced {
            self.voiced_samples += frame.valid_samples as u64;
        }
        if let Some(point) = self
            .points
            .iter()
            .rev()
            .find(|p| p.voiced_ms <= self.voiced_samples / 16)
        {
            self.text.clone_from(&point.text);
        }
        Ok(self.text.clone())
    }
    fn finish(&mut self) -> Result<String, ProviderError> {
        Ok(self.text.clone())
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
    fn vad(&self) -> Box<dyn VadProvider> {
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
            text: String::new(),
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
