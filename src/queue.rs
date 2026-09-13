use crate::clock::{Clock, TokioClock};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::VecDeque, rc::Rc};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub name: String,
    pub capacity: usize,
    pub peak: usize,
    /// Maximum age of the oldest resident item, including cleared residuals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wait_ms: Option<u64>,
}
#[derive(Clone)]
pub struct QueueMeter {
    name: String,
    capacity: usize,
    peak: Rc<Cell<usize>>,
    waits: Option<Rc<std::cell::RefCell<Waits>>>,
}
struct Waits {
    clock: Rc<dyn Clock>,
    admitted: VecDeque<u64>,
    maximum: u64,
}
impl Waits {
    fn sample(&mut self) {
        if let Some(at) = self.admitted.front() {
            self.maximum = self.maximum.max(self.clock.now_ms().saturating_sub(*at));
        }
    }
}
impl QueueMeter {
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            name: name.into(),
            capacity,
            peak: Rc::new(Cell::new(0)),
            waits: None,
        }
    }
    pub fn observe(&self, depth: usize) {
        assert!(depth <= self.capacity);
        self.peak.set(self.peak.get().max(depth));
    }
    fn admitted(&self) {
        if let Some(waits) = &self.waits {
            let mut w = waits.borrow_mut();
            w.sample();
            let now = w.clock.now_ms();
            w.admitted.push_back(now);
            assert!(w.admitted.len() <= self.capacity);
        }
    }
    fn removed(&self) {
        if let Some(waits) = &self.waits {
            let mut w = waits.borrow_mut();
            w.sample();
            w.admitted.pop_front();
        }
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        let max_wait_ms = self.waits.as_ref().map(|w| {
            let mut w = w.borrow_mut();
            w.sample();
            w.maximum
        });
        QueueSnapshot {
            name: self.name.clone(),
            capacity: self.capacity,
            peak: self.peak.get(),
            max_wait_ms,
        }
    }
}
pub struct Sender<T> {
    inner: mpsc::Sender<T>,
    meter: QueueMeter,
}
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            meter: self.meter.clone(),
        }
    }
}
impl<T> Sender<T> {
    pub async fn send(&self, value: T) -> Result<(), mpsc::error::SendError<T>> {
        let permit = match self.inner.reserve().await {
            Ok(p) => p,
            Err(_) => return Err(mpsc::error::SendError(value)),
        };
        // Admission, bookkeeping and observation cannot be interleaved on LocalSet.
        permit.send(value);
        self.meter.admitted();
        self.meter
            .observe(self.inner.max_capacity() - self.inner.capacity());
        Ok(())
    }
    pub fn try_send(&self, value: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.inner.try_send(value)?;
        self.meter.admitted();
        self.meter
            .observe(self.inner.max_capacity() - self.inner.capacity());
        Ok(())
    }
    pub fn meter(&self) -> QueueMeter {
        self.meter.clone()
    }
}
pub struct Receiver<T> {
    inner: mpsc::Receiver<T>,
    meter: QueueMeter,
}
impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        let value = self.inner.recv().await?;
        self.meter.removed();
        Some(value)
    }
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        let value = self.inner.try_recv()?;
        self.meter.removed();
        Ok(value)
    }
    pub fn close(&mut self) {
        self.inner.close();
    }
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        while self.inner.try_recv().is_ok() {
            self.meter.removed();
        }
    }
}
pub fn channel<T>(
    name: impl Into<String>,
    capacity: usize,
) -> (Sender<T>, Receiver<T>, QueueMeter) {
    channel_with_clock(name, capacity, Rc::new(TokioClock::default()))
}
pub fn channel_with_clock<T>(
    name: impl Into<String>,
    capacity: usize,
    clock: Rc<dyn Clock>,
) -> (Sender<T>, Receiver<T>, QueueMeter) {
    let (tx, rx) = mpsc::channel(capacity);
    let mut meter = QueueMeter::new(name, capacity);
    meter.waits = Some(Rc::new(std::cell::RefCell::new(Waits {
        clock,
        admitted: VecDeque::new(),
        maximum: 0,
    })));
    (
        Sender {
            inner: tx,
            meter: meter.clone(),
        },
        Receiver {
            inner: rx,
            meter: meter.clone(),
        },
        meter,
    )
}
