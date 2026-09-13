//! Single-session voice runtime. PCM input uses 16 kHz, mono, 20 ms frames.
pub mod audio;
pub mod clock;
pub mod event;
pub mod playback;
pub mod provider;
pub mod queue;
