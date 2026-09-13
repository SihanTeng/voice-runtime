//! Provider timing is transport behavior: soft cancellation need not end a stream.
use crate::clock::Clock;
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, rc::Rc};
use tokio_util::sync::CancellationToken;

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Timing {
    pub first_ms: u64,
    pub interval_ms: u64,
    pub jitter_ms: u64,
    pub seed: u64,
    pub stall_at: Option<usize>,
    pub panic_at: Option<usize>,
    pub late_chunks: usize,
    pub first_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub total_timeout_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_ms: 40,
            interval_ms: 20,
            jitter_ms: 0,
            seed: 7,
            stall_at: None,
            panic_at: None,
            late_chunks: 0,
            first_timeout_ms: 1000,
            idle_timeout_ms: 500,
            total_timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProviderError {
    #[error("provider timeout")]
    Timeout,
    #[error("provider cancelled")]
    Cancelled,
    #[error("downstream closed or stalled")]
    Downstream,
    #[error("provider protocol violation: {0}")]
    Protocol(String),
}

pub struct Pacer {
    timing: Timing,
    clock: Rc<dyn Clock>,
    index: usize,
    remaining_late: Option<usize>,
    active_ms: u64,
}

impl Pacer {
    pub fn new(timing: Timing, clock: Rc<dyn Clock>) -> Self {
        Self {
            timing,
            clock,
            index: 0,
            remaining_late: None,
            active_ms: 0,
        }
    }

    pub async fn next(
        &mut self,
        soft: &CancellationToken,
        hard: &CancellationToken,
    ) -> Result<(), ProviderError> {
        if hard.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if soft.is_cancelled() && self.remaining_late.is_none() {
            self.remaining_late = Some(self.timing.late_chunks);
        }
        if self.remaining_late == Some(0) {
            return Err(ProviderError::Cancelled);
        }
        assert_ne!(
            self.timing.panic_at,
            Some(self.index),
            "injected provider panic"
        );
        let first = self.index == 0;
        let jitter = if self.timing.jitter_ms == 0 {
            0
        } else {
            // Stateless, specified integer mixing: reproducible across platforms.
            let x = self
                .timing
                .seed
                .wrapping_add(self.index as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1);
            x % (self.timing.jitter_ms + 1)
        };
        let delay = if self.timing.stall_at == Some(self.index) {
            u64::MAX / 2
        } else {
            (if first {
                self.timing.first_ms
            } else {
                self.timing.interval_ms
            })
            .saturating_add(jitter)
        };
        let timeout = (if first {
            self.timing.first_timeout_ms
        } else {
            self.timing.idle_timeout_ms
        })
        .min(self.timing.total_timeout_ms.saturating_sub(self.active_ms));
        let start = self.clock.now_ms();
        tokio::select! {
            biased;
            _ = hard.cancelled() => return Err(ProviderError::Cancelled),
            _ = soft.cancelled(), if self.remaining_late.is_none() => {
                self.remaining_late = Some(self.timing.late_chunks);
                if self.remaining_late == Some(0) { return Err(ProviderError::Cancelled); }
                // A cancelled transport can still deliver its next packet.
            }
            _ = self.clock.sleep_until(start.saturating_add(timeout)), if delay > timeout => return Err(ProviderError::Timeout),
            _ = self.clock.sleep_until(start.saturating_add(delay)) => {}
        }
        self.active_ms += self.clock.now_ms() - start;
        self.index += 1;
        if let Some(left) = &mut self.remaining_late {
            *left -= 1;
        }
        Ok(())
    }
}
