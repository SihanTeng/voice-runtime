use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointConfig {
    pub complete_silence_ms: u64,
    pub incomplete_silence_ms: u64,
    pub unknown_silence_ms: u64,
    pub partial_stability_ms: u64,
    pub barge_in_ms: u64,
    pub asr_lag_timeout_ms: u64,
    /// Opt-in ASR-assisted short-backchannel guard; acoustic fallback remains bounded.
    pub backchannel_max_ms: Option<u64>,
    /// Missing provider stability falls back to temporal stability, never fake confidence.
    pub min_partial_stability: Option<f32>,
}
impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            complete_silence_ms: 240,
            incomplete_silence_ms: 1000,
            unknown_silence_ms: 600,
            partial_stability_ms: 100,
            barge_in_ms: 120,
            asr_lag_timeout_ms: 1500,
            backchannel_max_ms: None,
            min_partial_stability: None,
        }
    }
}

impl EndpointConfig {
    pub fn silence_threshold(&self, partial: &str) -> u64 {
        let text = partial.trim();
        let last_word = text
            .trim_end_matches(['.', ',', '…'])
            .split_whitespace()
            .last()
            .unwrap_or("")
            .to_ascii_lowercase();
        let incomplete = text.ends_with("...")
            || text.ends_with('…')
            || [
                "for", "and", "or", "to", "with", "the", "a", "at", "on", "but", "because",
            ]
            .contains(&last_word.as_str());
        if incomplete {
            self.incomplete_silence_ms
        } else if text.ends_with(['.', '?', '!', '。', '？', '！']) {
            self.complete_silence_ms
        } else {
            self.unknown_silence_ms
        }
    }
}

/// Deliberately narrow acknowledgements. "yes"/"no" may carry business intent.
pub fn is_backchannel(text: &str) -> bool {
    matches!(
        text.trim()
            .trim_end_matches(['.', '!', '。', '！'])
            .to_lowercase()
            .as_str(),
        "mm-hmm" | "uh-huh" | "嗯" | "哦"
    )
}
