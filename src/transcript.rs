//! Revision-aware ASR transcript. Segment finality is not an endpoint decision.
use crate::provider::ProviderError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AsrUpdate {
    pub segment_id: u64,
    pub revision: u64,
    pub through_sequence: u64,
    pub text: String,
    pub stable_prefix_bytes: usize,
    pub stability: Option<f32>,
    pub is_final: bool,
}

impl AsrUpdate {
    pub fn validate(&self, max_bytes: usize) -> Result<(), ProviderError> {
        if self.revision == 0
            || self.text.len() > max_bytes
            || !self.text.is_char_boundary(self.stable_prefix_bytes)
            || self
                .stability
                .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err(ProviderError::Protocol(
                "invalid ASR revision metadata".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    prefix: String,
    current: Option<AsrUpdate>,
    text: String,
}
impl Transcript {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn stability(&self) -> Option<f32> {
        self.current.as_ref().and_then(|u| u.stability)
    }
    /// False means a duplicate/out-of-order update; it cannot advance freshness.
    pub fn apply(&mut self, update: &AsrUpdate, max_bytes: usize) -> Result<bool, ProviderError> {
        let invalid = || ProviderError::Protocol("invalid ASR revision or stable prefix".into());
        update.validate(max_bytes)?;
        let mut prefix = self.prefix.clone();
        if let Some(previous) = &self.current {
            if update.segment_id < previous.segment_id
                || update.through_sequence < previous.through_sequence
                || (update.segment_id == previous.segment_id
                    && update.revision <= previous.revision)
            {
                return Ok(false);
            }
            if update.segment_id == previous.segment_id {
                if (previous.is_final && (!update.is_final || update.text != previous.text))
                    || update.stable_prefix_bytes < previous.stable_prefix_bytes
                    || !update
                        .text
                        .starts_with(&previous.text[..previous.stable_prefix_bytes])
                {
                    return Err(invalid());
                }
            } else {
                if !previous.is_final
                    || Some(update.segment_id) != previous.segment_id.checked_add(1)
                {
                    return Err(invalid());
                }
                prefix.push_str(&previous.text);
            }
        } else if update.segment_id != 0 {
            return Err(invalid());
        }
        if prefix.len().saturating_add(update.text.len()) > max_bytes {
            return Err(invalid());
        }
        self.text = format!("{prefix}{}", update.text);
        self.prefix = prefix;
        self.current = Some(update.clone());
        Ok(true)
    }
}
