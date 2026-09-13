use std::{io::Cursor, rc::Rc};
use voice_runtime::{audit, clock::TokioClock, event, scenario, session::SessionConfig, wav};

#[tokio::test(start_paused = true)]
async fn replay_reconstructs_actual_ledger_and_metrics_and_exports_only_consumed_pcm() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let report = scenario::run(
                "C",
                SessionConfig::default(),
                Rc::new(TokioClock::default()),
            )
            .await;
            let mut jsonl = Vec::new();
            event::write_jsonl(&report.events, &mut jsonl).unwrap();
            let events = audit::read_jsonl(Cursor::new(jsonl)).unwrap();
            let audit = audit::analyze(&events);
            assert!(audit.violations.is_empty(), "{:?}", audit.violations);
            assert_eq!(audit.replies, report.replies);
            assert_eq!(audit.metrics.stale_chunk_received_count, 2);
            assert_eq!(audit.metrics.stale_chunk_played_count, 0);
            assert_eq!(audit.metrics.overlap_duration_ms, Some(120));
            assert_eq!(audit.metrics.interruptions[0].speech_start_detection_ms, 20);
            assert_eq!(audit.metrics.interruptions[0].interruption_decision_ms, 100);
            assert_eq!(audit.metrics.interruptions[0].playback_stop_ms, Some(0));
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("heard.wav");
            wav::export_played(&events, &path).unwrap();
            let mut wav = hound::WavReader::open(path).unwrap();
            let nonzero = wav
                .samples::<i16>()
                .map(Result::unwrap)
                .filter(|s| *s != 0)
                .count();
            assert_eq!(
                nonzero,
                report
                    .replies
                    .iter()
                    .map(|r| r.played_samples())
                    .sum::<usize>()
            );
            let diagram = audit::mermaid(&events);
            assert!(diagram.contains("generation_cancelled"));
            assert!(diagram.contains("stale_event_dropped"));
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn tampered_trace_detects_stale_playback_missing_events_and_overconsumption() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let report = scenario::run(
                "E",
                SessionConfig::default(),
                Rc::new(TokioClock::default()),
            )
            .await;
            let mut events = report.events.clone();
            let mut stale = events
                .iter()
                .find(|e| e.event_type == "playback_progress")
                .unwrap()
                .clone();
            let insertion = events
                .iter()
                .position(|e| e.event_type == "generation_cancelled")
                .unwrap()
                + 1;
            stale.timestamp = events[insertion].timestamp;
            events.insert(insertion, stale);
            for (n, event) in events.iter_mut().enumerate() {
                event.sequence_number = n as u64;
            }
            let audit = audit::analyze(&events);
            assert_eq!(audit.metrics.stale_chunk_played_count, 1);
            assert!(!audit.violations.is_empty());
            let mut events = report.events.clone();
            events.remove(5);
            assert!(!audit::analyze(&events).violations.is_empty());
            let mut events = report.events.clone();
            events.pop();
            assert!(!audit::analyze(&events).violations.is_empty());
        })
        .await;
}

#[test]
fn oversized_jsonl_line_is_rejected_with_a_bounded_read() {
    assert!(audit::read_jsonl(Cursor::new(vec![b' '; 65_537])).is_err());
}
