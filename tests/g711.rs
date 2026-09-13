use std::io::Cursor;
use voice_runtime::{
    audio::AudioFrame,
    g711::{G711Source, Law},
};

#[test]
fn codecs_match_independent_exhaustive_pcm_and_codeword_vectors() {
    for (law, encoded, decoded) in [
        (
            Law::Mulaw,
            include_bytes!("fixtures/g711-ulaw-encode.bin"),
            include_bytes!("fixtures/g711-ulaw-decode.pcm"),
        ),
        (
            Law::Alaw,
            include_bytes!("fixtures/g711-alaw-encode.bin"),
            include_bytes!("fixtures/g711-alaw-decode.pcm"),
        ),
    ] {
        for (index, expected) in encoded.iter().enumerate() {
            let pcm = (index as i32 - 32768) as i16;
            assert_eq!(law.encode(pcm), *expected, "{law:?}: {pcm}");
        }
        for byte in 0..=255u8 {
            let i = byte as usize * 2;
            assert_eq!(
                law.decode(byte),
                i16::from_le_bytes([decoded[i], decoded[i + 1]])
            );
        }
    }
}

#[test]
fn phone_packets_preserve_frame_duration_and_reject_truncated_input() {
    for law in [Law::Mulaw, Law::Alaw] {
        let pcm = AudioFrame::new(7, 140, vec![1000; 320]).unwrap();
        let packet = law.encode_frame(&pcm).unwrap();
        let frame = law.decode_frame(7, 140, &packet).unwrap();
        assert_eq!(
            (frame.sequence, frame.timestamp, frame.valid_samples),
            (7, 140, 320)
        );
        assert!(frame.samples.iter().all(|s| (*s as i32 - 1000).abs() < 32));
        let frames: Vec<_> = G711Source::new(Cursor::new([packet, packet].concat()), law).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].as_ref().unwrap().timestamp, 20);
        assert!(law.decode_frame(0, 0, &packet[..159]).is_err());
        let mut truncated = G711Source::new(Cursor::new(&packet[..159]), law);
        assert!(truncated.next().unwrap().is_err());
        assert!(truncated.next().is_none());
    }
}

#[test]
fn phone_cli_runs_the_runtime_and_exports_only_consumed_audio() {
    use std::{fs, process::Command};
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("script.json");
    fs::write(&script, r#"{"turns":[{"partials":[{"voiced_ms":0,"text":"Book Friday."}],"response":"Confirmed."}],"word_ms":200,"real_vad":false}"#).unwrap();
    for (law, name) in [(Law::Mulaw, "mulaw"), (Law::Alaw, "alaw")] {
        let input = temp.path().join(format!("input.{name}"));
        let mut bytes = vec![law.encode(2000); 40 * 160];
        bytes.extend(vec![law.encode(0); 25 * 160]);
        fs::write(&input, &bytes).unwrap();
        let output = temp.path().join(name);
        let result = Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
            .arg("g711")
            .arg(&input)
            .args(["--law", name, "--script"])
            .arg(&script)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let events = voice_runtime::audit::read_jsonl(std::io::BufReader::new(
            fs::File::open(output.join("trace.jsonl")).unwrap(),
        ))
        .unwrap();
        let audit = voice_runtime::audit::analyze(&events);
        assert!(audit.violations.is_empty());
        assert_eq!(audit.replies.len(), 1);
        assert_eq!(audit.replies[0].heard_text(), "Confirmed.");
        assert_eq!(audit.metrics.stale_chunk_played_count, 0);
        let mut wav = hound::WavReader::open(output.join("played.wav")).unwrap();
        let mut pcm: Vec<i16> = wav.samples().map(Result::unwrap).collect();
        pcm.resize(pcm.len().div_ceil(320) * 320, 0);
        let expected: Vec<_> = pcm
            .chunks_exact(2)
            .map(|p| law.encode(((p[0] as i32 + p[1] as i32) / 2) as i16))
            .collect();
        assert_eq!(
            fs::read(output.join(format!("played.{name}"))).unwrap(),
            expected
        );
        // A bad source also closes and joins the session instead of abandoning its owner.
        bytes.push(0);
        fs::write(&input, bytes).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
            .arg("g711")
            .arg(&input)
            .args(["--law", name, "--script"])
            .arg(&script)
            .arg("--output")
            .arg(&output)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let lifecycle: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("lifecycle.json")).unwrap()).unwrap();
        assert_eq!(lifecycle["active_tasks"], 0);
        assert_eq!(lifecycle["close_reason"], "audio_source_failed");
        assert_eq!(lifecycle["trace_complete"], true);
    }
}
