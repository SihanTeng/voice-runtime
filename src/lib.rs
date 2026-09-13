//! Single-session voice runtime. PCM input uses 16 kHz, mono, 20 ms frames.
pub mod audio;
pub mod audit;
pub mod clock;
pub mod endpoint;
pub mod event;
pub mod fake;
pub mod playback;
pub mod provider;
pub mod queue;
pub mod scenario;
pub mod session;
pub mod transport;
pub mod wav;
