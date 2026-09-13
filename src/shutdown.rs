//! Process/driver shutdown notification; the session remains responsible for joining resources.
use std::{cell::Cell, rc::Rc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct Shutdown {
    token: CancellationToken,
    reason: Rc<Cell<Option<&'static str>>>,
}
impl Shutdown {
    pub fn request(&self, reason: &'static str) {
        if !self.token.is_cancelled() {
            self.reason.set(Some(reason));
            self.token.cancel();
        }
    }
    pub fn reason(&self) -> Option<&'static str> {
        self.reason.get()
    }
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}
