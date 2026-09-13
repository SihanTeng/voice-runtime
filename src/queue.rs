use serde::{Deserialize, Serialize};
use std::{cell::Cell, rc::Rc};
use tokio::sync::mpsc;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub name: String,
    pub capacity: usize,
    pub peak: usize,
}

#[derive(Clone)]
pub struct QueueMeter {
    name: String,
    capacity: usize,
    peak: Rc<Cell<usize>>,
}

impl QueueMeter {
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            name: name.into(),
            capacity,
            peak: Rc::new(Cell::new(0)),
        }
    }
    pub fn observe(&self, depth: usize) {
        assert!(depth <= self.capacity);
        self.peak.set(self.peak.get().max(depth));
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            name: self.name.clone(),
            capacity: self.capacity,
            peak: self.peak.get(),
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
            Ok(permit) => permit,
            Err(_) => return Err(mpsc::error::SendError(value)),
        };
        // The LocalSet cannot switch tasks between send and observation.
        permit.send(value);
        self.meter
            .observe(self.inner.max_capacity() - self.inner.capacity());
        Ok(())
    }
    pub fn try_send(&self, value: T) -> Result<(), mpsc::error::TrySendError<T>> {
        self.inner.try_send(value)?;
        self.meter
            .observe(self.inner.max_capacity() - self.inner.capacity());
        Ok(())
    }
    pub fn meter(&self) -> QueueMeter {
        self.meter.clone()
    }
}

pub fn channel<T>(
    name: impl Into<String>,
    capacity: usize,
) -> (Sender<T>, mpsc::Receiver<T>, QueueMeter) {
    let (tx, rx) = mpsc::channel(capacity);
    let meter = QueueMeter::new(name, capacity);
    (
        Sender {
            inner: tx,
            meter: meter.clone(),
        },
        rx,
        meter,
    )
}
