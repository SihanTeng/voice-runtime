use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub turn_id: u64,
    pub generation_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// Monotonic milliseconds relative to session start, not wall-clock time.
    pub timestamp: u64,
    pub session_id: String,
    pub turn_id: Option<u64>,
    pub generation_id: Option<u64>,
    pub event_type: String,
    pub sequence_number: u64,
    pub payload: Value,
}

impl Event {
    pub fn identity(&self) -> Option<Identity> {
        Some(Identity {
            turn_id: self.turn_id?,
            generation_id: self.generation_id?,
        })
    }
}

pub fn write_jsonl(events: &[Event], mut writer: impl std::io::Write) -> std::io::Result<()> {
    for event in events {
        serde_json::to_writer(&mut writer, event)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}
