use std::rc::Rc;
use voice_runtime::{
    audio::FRAME_MS,
    clock::{Clock, TokioClock},
    fake::{FakeProviders, TranscriptPoint, TurnScript},
    playback::CountingSink,
    scenario::{feed, wait_until},
    session::{Session, SessionConfig},
};

#[tokio::test(start_paused = true)]
async fn vad_backlog_never_turns_unprocessed_continuation_into_silence() {
    for interval in [25, 30, 40] {
        let mut config = SessionConfig::default();
        config.vad.interval_ms = interval;
        config.llm.first_ms = 0;
        config.tts.first_ms = 0;
        let report = tokio::task::LocalSet::new()
            .run_until(voice_runtime::scenario::run(
                "B",
                config,
                Rc::new(TokioClock::default()),
            ))
            .await;
        let endpoints: Vec<_> = report
            .events
            .iter()
            .filter(|e| e.event_type == "endpoint_committed")
            .collect();
        assert_eq!(endpoints.len(), 1, "VAD interval {interval}");
        assert_eq!(
            endpoints[0].payload["partial"],
            "Please book it for Wednesday afternoon."
        );
        assert!(!report.events.iter().any(|e| {
            ["endpoint_committed", "playback_started"].contains(&e.event_type.as_str())
                && e.timestamp < 1500
        }));
        assert!(
            !report
                .events
                .iter()
                .any(|e| e.event_type == "interruption_decision")
        );
        assert_eq!(
            report.replies[0].heard_text(),
            report.replies[0].generated_text
        );
        assert_eq!(report.active_tasks, 0);
        assert!(report.queues.iter().all(|q| q.peak <= q.capacity));
        assert!(
            voice_runtime::audit::analyze(&report.events)
                .violations
                .is_empty()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn hesitation_policy_generalizes_beyond_function_words_and_fixture_timestamps() {
    tokio::task::LocalSet::new()
        .run_until(async {
            // Varied utterance lengths ensure the assertions do not know B's 1500ms end.
            // Only the first case uses a lexical hint; the others require ellipsis.
            for (partial, final_text, before_ms, after_ms) in [
                ("Could we meet at", "Could we meet at noon?", 280, 420),
                (
                    "I was thinking…",
                    "I was thinking tomorrow would work.",
                    560,
                    240,
                ),
                (
                    "Perhaps tomorrow...",
                    "Perhaps tomorrow afternoon!",
                    360,
                    620,
                ),
                ("我想想…", "我想想，周五可以。", 460, 340),
            ] {
                let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
                let config = SessionConfig::default();
                let endpoint_budget = config.endpoint.complete_silence_ms + 2 * FRAME_MS;
                let providers = FakeProviders {
                    turns: vec![TurnScript {
                        partials: vec![
                            TranscriptPoint {
                                voiced_ms: 0,
                                text: partial.into(),
                            },
                            TranscriptPoint {
                                voiced_ms: before_ms + after_ms,
                                text: final_text.into(),
                            },
                        ],
                        response: "Confirmed.".into(),
                        final_text: None,
                    }],
                    ..FakeProviders::default()
                };
                let (session, mut handle) = Session::new(
                    config,
                    Rc::new(providers),
                    clock.clone(),
                    Box::<CountingSink>::default(),
                )
                .unwrap();
                let owner = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(
                    session.run(),
                ));
                let mut sequence = 0;
                feed(
                    &handle,
                    clock.as_ref(),
                    &mut sequence,
                    before_ms,
                    2000,
                    Some(true),
                )
                .await
                .unwrap();
                let pause_start = clock.now_ms();
                feed(&handle, clock.as_ref(), &mut sequence, 700, 0, Some(false))
                    .await
                    .unwrap();
                let pause_end = clock.now_ms();
                assert_eq!(pause_end - pause_start, 700);
                assert_eq!(handle.snapshot().started_replies, 0, "{partial}");
                feed(
                    &handle,
                    clock.as_ref(),
                    &mut sequence,
                    after_ms,
                    2000,
                    Some(true),
                )
                .await
                .unwrap();
                let speech_end = clock.now_ms();
                feed(&handle, clock.as_ref(), &mut sequence, 500, 0, Some(false))
                    .await
                    .unwrap();
                wait_until(&mut handle, |s| s.completed_replies == 1)
                    .await
                    .unwrap();
                handle.close();
                let report = owner.await.unwrap();
                assert!(
                    !report
                        .events
                        .iter()
                        .any(|e| ["endpoint_committed", "playback_started"]
                            .contains(&e.event_type.as_str())
                            && e.timestamp < speech_end),
                    "premature endpoint for {partial}"
                );
                let endpoints: Vec<_> = report
                    .events
                    .iter()
                    .filter(|e| e.event_type == "endpoint_committed")
                    .collect();
                assert_eq!(endpoints.len(), 1, "{partial}");
                let endpoint = endpoints[0];
                assert_eq!(endpoint.payload["partial"], final_text);
                assert!(
                    endpoint.timestamp - speech_end <= endpoint_budget,
                    "{partial}"
                );
                assert!(endpoint.timestamp - speech_end <= 400);
                assert!(
                    report
                        .events
                        .iter()
                        .any(|e| e.event_type == "endpoint_candidate_revoked"
                            && e.timestamp >= pause_end
                            && e.timestamp <= pause_end + FRAME_MS)
                );
                assert_eq!(report.active_tasks, 0);
                assert!(report.trace_complete);
                assert!(report.queues.iter().all(|q| q.peak <= q.capacity));
                let audit = voice_runtime::audit::analyze(&report.events);
                assert!(audit.violations.is_empty(), "{:?}", audit.violations);
                assert_eq!(audit.replies, report.replies);
                assert_eq!(audit.metrics.stale_chunk_played_count, 0);
            }
        })
        .await;
}
