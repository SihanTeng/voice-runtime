use std::rc::Rc;
use voice_runtime::{
    audio::{FRAME_MS, SAMPLE_RATE},
    clock::TokioClock,
    scenario,
    session::{SessionConfig, SessionReport},
};

async fn run(name: &str) -> SessionReport {
    tokio::task::LocalSet::new()
        .run_until(scenario::run(
            name,
            SessionConfig::default(),
            Rc::new(TokioClock::default()),
        ))
        .await
}
fn invariants(report: &SessionReport) {
    let audit = voice_runtime::audit::analyze(&report.events);
    assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    assert_eq!(audit.replies, report.replies);
    assert_eq!(audit.metrics.stale_chunk_played_count, 0);
    assert!(report.trace_complete);
    assert_eq!(report.active_tasks, 0);
    assert!(
        report
            .queues
            .iter()
            .all(|q| q.capacity > 0 && q.peak <= q.capacity)
    );
    assert_eq!(report.events.last().unwrap().event_type, "session_closed");
    for cancel in report
        .events
        .iter()
        .filter(|e| e.event_type == "generation_cancelled")
    {
        assert!(
            !report
                .events
                .iter()
                .any(|e| e.event_type == "playback_progress"
                    && e.identity() == cancel.identity()
                    && e.payload["end_ms"].as_u64().unwrap() > cancel.timestamp)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_complete_utterance_streams_without_added_queue_delay() {
    // Include non-tick-aligned latencies and zero latency: the allowance is for
    // scheduling, not a fixed total that only fits the default provider settings.
    for (llm_ms, tts_ms) in [(0, 0), (13, 47), (80, 60), (150, 110)] {
        let mut config = SessionConfig::default();
        config.llm.first_ms = llm_ms;
        config.tts.first_ms = tts_ms;
        let r = tokio::task::LocalSet::new()
            .run_until(scenario::run("A", config, Rc::new(TokioClock::default())))
            .await;
        invariants(&r);
        let recorded: SessionConfig =
            serde_json::from_value(r.events[0].payload["config"].clone()).unwrap();
        let endpoint = r
            .events
            .iter()
            .find(|e| e.event_type == "endpoint_committed")
            .unwrap();
        let consumed = r
            .events
            .iter()
            .find(|e| e.event_type == "playback_progress")
            .unwrap();
        let first_audio = consumed.payload["start_ms"].as_u64().unwrap();
        // ASR has already streamed its first packet; finalization uses interval_ms.
        let provider_budget =
            recorded.asr.interval_ms + recorded.llm.first_ms + recorded.tts.first_ms;
        let elapsed = first_audio - endpoint.timestamp;
        assert!(elapsed >= provider_budget);
        assert!(
            elapsed - provider_budget <= 2 * FRAME_MS,
            "provider budget {provider_budget}ms, actual {elapsed}ms"
        );
        assert_eq!(r.replies[0].heard_text(), r.replies[0].generated_text);
    }
}
#[tokio::test(start_paused = true)]
async fn b_seven_hundred_ms_hesitation_never_commits_or_plays() {
    let r = run("B").await;
    invariants(&r);
    // Use captured input truth, independently of endpoint/speech-end decisions.
    let speech_end = r
        .events
        .iter()
        .filter(|e| e.event_type == "audio_frame" && e.payload["speech_truth"] == true)
        .map(|e| {
            e.payload["capture_ms"].as_u64().unwrap()
                + e.payload["valid_samples"].as_u64().unwrap() * 1000 / SAMPLE_RATE as u64
        })
        .max()
        .unwrap();
    assert!(!r.events.iter().any(|e| {
        ["endpoint_committed", "playback_started"].contains(&e.event_type.as_str())
            && e.timestamp < speech_end
    }));
    let endpoint = r
        .events
        .iter()
        .find(|e| e.event_type == "endpoint_committed")
        .unwrap();
    let config: SessionConfig =
        serde_json::from_value(r.events[0].payload["config"].clone()).unwrap();
    assert!(endpoint.timestamp - speech_end <= config.endpoint.complete_silence_ms + 2 * FRAME_MS);
    assert!(endpoint.timestamp - speech_end <= 400); // External acceptance budget.
}
#[tokio::test(start_paused = true)]
async fn c_barge_in_stops_within_250ms_and_history_excludes_unheard_suffix() {
    let r = run("C").await;
    invariants(&r);
    let decision = r
        .events
        .iter()
        .find(|e| e.event_type == "interruption_decision")
        .unwrap();
    let stop = r
        .events
        .iter()
        .find(|e| e.event_type == "playback_stopped" && e.identity() == decision.identity())
        .unwrap();
    assert!(stop.timestamp - decision.payload["onset_ms"].as_u64().unwrap() <= 250);
    assert!(r.replies[0].interrupted);
    assert!(r.replies[0].heard_text().len() < r.replies[0].generated_text.len());
    let request = r
        .events
        .iter()
        .filter(|e| e.event_type == "llm_requested")
        .nth(1)
        .unwrap();
    assert_eq!(
        request.payload["heard_history"][0].as_str().unwrap(),
        format!("{} [interrupted]", r.replies[0].heard_text())
    );
    assert_eq!(r.replies.len(), 2);
    assert_eq!(r.replies[1].heard_text(), r.replies[1].generated_text);
}
#[tokio::test(start_paused = true)]
async fn d_eighty_ms_noise_does_not_cancel_reply() {
    let r = run("D").await;
    invariants(&r);
    assert!(
        !r.events
            .iter()
            .any(|e| e.event_type == "generation_cancelled")
    );
    assert!(
        r.events
            .iter()
            .any(|e| e.event_type == "speech_candidate_rejected")
    );
    assert_eq!(r.replies[0].heard_text(), r.replies[0].generated_text);
}
#[tokio::test(start_paused = true)]
async fn e_two_late_chunks_are_received_but_never_enqueued_or_played() {
    let r = run("E").await;
    invariants(&r);
    let stale: Vec<_> = r
        .events
        .iter()
        .filter(|e| {
            e.event_type == "stale_event_dropped" && e.payload["source_type"] == "tts_chunk"
        })
        .collect();
    assert_eq!(stale.len(), 2);
    let cancelled = r
        .events
        .iter()
        .find(|e| e.event_type == "generation_cancelled")
        .unwrap();
    assert!(!r.events.iter().any(|e| e.event_type == "audio_enqueued"
        && e.identity() == cancelled.identity()
        && e.sequence_number > cancelled.sequence_number));
}

#[tokio::test(start_paused = true)]
async fn old_audio_arriving_after_new_playback_starts_is_still_rejected() {
    let mut config = SessionConfig::default();
    config.tts.late_delay_ms = 1000;
    let report = tokio::task::LocalSet::new()
        .run_until(scenario::run("E", config, Rc::new(TokioClock::default())))
        .await;
    invariants(&report);
    let second = report
        .events
        .iter()
        .filter(|e| e.event_type == "playback_started")
        .nth(1)
        .unwrap()
        .timestamp;
    let stale: Vec<_> = report
        .events
        .iter()
        .filter(|e| {
            e.event_type == "stale_event_dropped" && e.payload["source_type"] == "tts_chunk"
        })
        .collect();
    assert_eq!(stale.len(), 2);
    assert!(stale.iter().all(|e| e.timestamp > second));
}
