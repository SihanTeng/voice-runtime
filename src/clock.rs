use std::{future::Future, pin::Pin, time::Duration};
use tokio::time::Instant;

pub type Sleep<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Every scheduling decision uses this clock, including provider fault injection.
pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_until(&self, deadline_ms: u64) -> Sleep<'_>;
}

pub struct TokioClock {
    epoch: Instant,
}

impl Default for TokioClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep<'_> {
        Box::pin(tokio::time::sleep_until(
            self.epoch + Duration::from_millis(deadline_ms),
        ))
    }
}
