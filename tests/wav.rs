use std::io::Cursor;
use voice_runtime::wav::WavSource;

#[test]
fn wav_validates_format_and_zero_pads_only_the_final_frame() {
    let mut cursor = Cursor::new(Vec::new());
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for _ in 0..333 {
            writer.write_sample(42i16).unwrap();
        }
        writer.finalize().unwrap();
    }
    cursor.set_position(0);
    let frames: Vec<_> = WavSource::new(cursor)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].sequence, 1);
    assert_eq!(frames[1].timestamp, 20);
    assert_eq!(frames[1].valid_samples, 13);
    assert_eq!(&frames[1].samples[..13], &[42; 13]);
    assert!(frames[1].samples[13..].iter().all(|s| *s == 0));
    let mut cursor = Cursor::new(Vec::new());
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    hound::WavWriter::new(&mut cursor, spec)
        .unwrap()
        .finalize()
        .unwrap();
    cursor.set_position(0);
    assert!(WavSource::new(cursor).is_err());
}

#[cfg(feature = "real-vad")]
#[test]
fn real_webrtc_vad_detects_fixed_speech_fixture_and_returns_to_silence() {
    use voice_runtime::{
        audio::AudioFrame,
        fake::{VadProvider, WebRtcVad},
    };
    let mut vad = WebRtcVad::default();
    let silence = AudioFrame::new(0, 0, vec![0; 320]).unwrap();
    for _ in 0..50 {
        assert!(!vad.classify(&silence).unwrap());
    }
    let bytes = include_bytes!("fixtures/speech16.wav");
    let mut voiced = 0;
    for frame in WavSource::new(Cursor::new(bytes)).unwrap() {
        if vad.classify(&frame.unwrap()).unwrap() {
            voiced += 1;
        }
    }
    assert!(voiced > 50, "speech frames: {voiced}");
    // WebRTC has an internal hangover; do not pretend it ends on the first silent frame.
    for _ in 0..50 {
        vad.classify(&silence).unwrap();
    }
    for _ in 0..50 {
        assert!(!vad.classify(&silence).unwrap());
    }
}
