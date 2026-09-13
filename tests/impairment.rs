use std::rc::Rc;
use voice_runtime::{
    audit, clock::TokioClock, impairment::InputFaults, scenario, session::SessionConfig,
};

#[tokio::test(start_paused = true)]
async fn lost_and_reordered_input_is_reproducible_and_cannot_replay_cancelled_audio() {
    for faults in [
        InputFaults {
            drop_every: Some(23),
            ..Default::default()
        },
        InputFaults {
            reorder_every: Some(31),
            ..Default::default()
        },
        InputFaults {
            seed: 71,
            jitter_ms: 9,
            drop_every: Some(23),
            reorder_every: Some(31),
        },
    ] {
        let config = SessionConfig {
            input_faults: faults.clone(),
            ..Default::default()
        };
        let mut previous = None;
        for _ in 0..2 {
            let report = tokio::task::LocalSet::new()
                .run_until(scenario::run(
                    "C",
                    config.clone(),
                    Rc::new(TokioClock::default()),
                ))
                .await;
            let audit = audit::analyze(&report.events);
            assert!(audit.violations.is_empty(), "{:?}", audit.violations);
            assert_eq!(report.close_reason, "active_close");
            assert_eq!(report.active_tasks, 0);
            assert!(report.trace_complete);
            assert!(report.queues.iter().all(|q| q.peak <= q.capacity));
            assert_eq!(report.replies.len(), 2);
            assert!(report.replies[0].interrupted);
            assert_eq!(
                report.replies[1].heard_text(),
                report.replies[1].generated_text
            );
            assert_eq!(audit.metrics.stale_chunk_played_count, 0);
            assert_eq!(audit.metrics.stale_chunk_received_count, 2);
            assert_eq!(audit.metrics.interruptions.len(), 1);
            assert!(
                audit.metrics.interruptions[0]
                    .interruption_to_playback_stop_ms
                    .unwrap()
                    <= 250
            );
            assert_eq!(audit.metrics.overlap_duration_ms, None);
            assert_eq!(audit.metrics.false_interruption_count, None);
            assert!(report.events.iter().any(|e| e.event_type == "audio_frame"
                && e.payload["missing_frames"].as_u64().unwrap() > 0));
            if faults.reorder_every.is_some() {
                assert!(
                    report
                        .events
                        .iter()
                        .any(|e| e.event_type == "audio_frame_rejected")
                );
            }
            let accepted: Vec<_> = report
                .events
                .iter()
                .filter(|e| e.event_type == "audio_frame")
                .map(|e| e.payload["source_sequence"].as_u64().unwrap())
                .collect();
            assert!(accepted.windows(2).all(|s| s[0] < s[1]));
            if let Some(previous) = previous {
                assert_eq!(report.events, previous);
            }
            previous = Some(report.events);
        }
    }
}
