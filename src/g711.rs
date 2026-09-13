//! Raw 8 kHz mono PCMU/PCMA packets, 160 octets per 20 ms.
//! Resampling is deliberately minimal: sample hold on input, pair averaging on output.
use crate::audio::{AudioError, AudioFrame};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Law {
    Mulaw,
    Alaw,
}

impl Law {
    pub fn decode(self, byte: u8) -> i16 {
        match self {
            Self::Mulaw => {
                let bits = !byte;
                let magnitude = (((bits & 15) as i32 * 8 + 132) << ((bits >> 4) & 7)) - 132;
                (if bits & 128 != 0 {
                    -magnitude
                } else {
                    magnitude
                }) as i16
            }
            Self::Alaw => {
                let bits = byte ^ 0x55;
                let segment = (bits >> 4) & 7;
                let magnitude = if segment == 0 {
                    (bits & 15) as i32 * 16 + 8
                } else {
                    ((bits & 15) as i32 * 16 + 264) << (segment - 1)
                };
                (if bits & 128 == 0 {
                    -magnitude
                } else {
                    magnitude
                }) as i16
            }
        }
    }
    /// Quantization follows the conventional 14-bit μ-law / 13-bit A-law PCM mapping.
    pub fn encode(self, sample: i16) -> u8 {
        match self {
            Self::Mulaw => {
                let linear = (sample as i32) >> 2;
                let biased = linear.abs().min(8159) as u32 + 33;
                let segment = (31 - biased.leading_zeros()).saturating_sub(5).min(7);
                let code = if biased >= 8192 {
                    127
                } else {
                    ((segment << 4) | ((biased >> (segment + 1)) & 15)) as u8
                };
                code ^ if linear < 0 { 0x7f } else { 0xff }
            }
            Self::Alaw => {
                let linear = (sample as i32) >> 3;
                let magnitude = if linear < 0 { -linear - 1 } else { linear } as u32;
                let segment = (31 - magnitude.max(1).leading_zeros())
                    .saturating_sub(4)
                    .min(7);
                let code = ((segment << 4) | ((magnitude >> segment.max(1)) & 15)) as u8;
                code ^ if linear < 0 { 0x55 } else { 0xd5 }
            }
        }
    }
    pub fn decode_frame(
        self,
        sequence: u64,
        timestamp: u64,
        packet: &[u8],
    ) -> Result<AudioFrame, G711Error> {
        if packet.len() != 160 {
            return Err(G711Error::PacketLength);
        }
        let pcm = packet
            .iter()
            .flat_map(|byte| [self.decode(*byte); 2])
            .collect();
        Ok(AudioFrame::new(sequence, timestamp, pcm)?)
    }
    pub fn encode_frame(self, frame: &AudioFrame) -> Result<[u8; 160], G711Error> {
        frame.validate()?;
        let mut bytes = [0; 160];
        for (out, pair) in bytes.iter_mut().zip(frame.samples.chunks_exact(2)) {
            *out = self.encode(((pair[0] as i32 + pair[1] as i32) / 2) as i16);
        }
        Ok(bytes)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum G711Error {
    #[error("G.711 input requires complete 160-byte packets (20 ms at 8 kHz)")]
    PacketLength,
    #[error("G.711 input exceeds 300 seconds")]
    Duration,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Audio(#[from] AudioError),
}

pub struct G711Source<R> {
    reader: R,
    law: Law,
    sequence: u64,
    finished: bool,
}
impl<R: Read> G711Source<R> {
    pub fn new(reader: R, law: Law) -> Self {
        Self {
            reader,
            law,
            sequence: 0,
            finished: false,
        }
    }
}
impl<R: Read> Iterator for G711Source<R> {
    type Item = Result<AudioFrame, G711Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let mut packet = [0; 160];
        let mut length = 0;
        while length < packet.len() {
            match self.reader.read(&mut packet[length..]) {
                Ok(0) => {
                    self.finished = true;
                    return if length == 0 {
                        None
                    } else {
                        Some(Err(G711Error::PacketLength))
                    };
                }
                Ok(n) => length += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e.into()));
                }
            }
        }
        if self.sequence >= 15_000 {
            self.finished = true;
            return Some(Err(G711Error::Duration));
        }
        let frame = self
            .law
            .decode_frame(self.sequence, self.sequence * 20, &packet);
        self.sequence += 1;
        Some(frame)
    }
}

/// Convert the already-audited consumption WAV, including gaps, never queued audio.
pub fn encode_wav(
    input: &std::path::Path,
    output: &std::path::Path,
    law: Law,
) -> Result<(), Box<dyn std::error::Error>> {
    if output.exists() && input.canonicalize()? == output.canonicalize()? {
        return Err("G.711 output must differ from input WAV".into());
    }
    let source = crate::wav::WavSource::new(std::io::BufReader::new(std::fs::File::open(input)?))?;
    let mut writer = std::io::BufWriter::new(std::fs::File::create(output)?);
    for frame in source {
        writer.write_all(&law.encode_frame(&frame?)?)?;
    }
    writer.flush()?;
    Ok(())
}
