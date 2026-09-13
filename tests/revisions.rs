use std::rc::Rc;
use voice_runtime::{
    audit,
    clock::{Clock, TokioClock},
    fake::{FakeProviders, TranscriptPoint, TurnScript},
    playback::CountingSink,
    scenario::{feed, wait_until},
    session::{Session, SessionConfig},
};
fn point(voiced_ms: u64, text: &str) -> TranscriptPoint {
    TranscriptPoint {
        voiced_ms,
        text: text.into(),
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn segment_finality_and_low_stability_do_not_commit_an_endpoint() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for segmented in [false, true] {
                let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
                let mut config = SessionConfig::default();
                config.endpoint.min_partial_stability = Some(0.8);
                let mut early = point(0, "Wednesday. ");
                early.is_final = segmented;
                early.stability = Some(0.2);
                let mut final_point = point(600, "Friday instead.");
                final_point.stability = Some(0.95);
                final_point.segment_id = u64::from(segmented);
                let factory = FakeProviders {
                    turns: vec![TurnScript {
                        partials: vec![early, final_point],
                        final_text: None,
                        response: "Confirmed.".into(),
                    }],
                    ..Default::default()
                };
                let (session, mut handle) = Session::new(
                    config,
                    Rc::new(factory),
                    clock.clone(),
                    Box::<CountingSink>::default(),
                )
                .unwrap();
                let owner = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(
                    session.run(),
                ));
                let mut seq = 0;
                feed(&handle, clock.as_ref(), &mut seq, 300, 2000, Some(true))
                    .await
                    .unwrap();
                feed(&handle, clock.as_ref(), &mut seq, 700, 0, Some(false))
                    .await
                    .unwrap();
                assert_eq!(handle.snapshot().started_replies, 0);
                feed(&handle, clock.as_ref(), &mut seq, 400, 2000, Some(true))
                    .await
                    .unwrap();
                feed(&handle, clock.as_ref(), &mut seq, 500, 0, Some(false))
                    .await
                    .unwrap();
                wait_until(&mut handle, |s| s.completed_replies == 1)
                    .await
                    .unwrap();
                handle.close();
                let r = owner.await.unwrap();
                let a = audit::analyze(&r.events);
                assert!(a.violations.is_empty(), "{:?}", a.violations);
                assert_eq!(a.metrics.turns.len(), 1);
                assert_eq!(a.metrics.turns[0].endpoint_latency_ms, Some(240));
                let request = r
                    .events
                    .iter()
                    .find(|e| e.event_type == "llm_requested")
                    .unwrap();
                assert_eq!(
                    request.payload["transcript"],
                    if segmented {
                        "Wednesday. Friday instead."
                    } else {
                        "Friday instead."
                    }
                );
                assert_eq!(r.active_tasks, 0);
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn revised_partial_and_different_final_drive_the_llm_request() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
            let factory = FakeProviders {
                turns: vec![TurnScript {
                    partials: vec![
                        point(0, "Wednesday."),
                        point(240, "Wednesday…"),
                        point(600, "Friday afternoon."),
                    ],
                    final_text: Some("Friday evening.".into()),
                    response: "Confirmed.".into(),
                }],
                ..Default::default()
            };
            let (session, mut handle) = Session::new(
                Default::default(),
                Rc::new(factory),
                clock.clone(),
                Box::<CountingSink>::default(),
            )
            .unwrap();
            let owner =
                tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(session.run()));
            let mut seq = 0;
            feed(&handle, clock.as_ref(), &mut seq, 300, 2000, Some(true))
                .await
                .unwrap();
            feed(&handle, clock.as_ref(), &mut seq, 700, 0, Some(false))
                .await
                .unwrap();
            assert_eq!(handle.snapshot().started_replies, 0);
            feed(&handle, clock.as_ref(), &mut seq, 300, 2000, Some(true))
                .await
                .unwrap();
            feed(&handle, clock.as_ref(), &mut seq, 500, 0, Some(false))
                .await
                .unwrap();
            wait_until(&mut handle, |s| s.completed_replies == 1)
                .await
                .unwrap();
            handle.close();
            let report = owner.await.unwrap();
            assert_eq!(
                report
                    .events
                    .iter()
                    .filter(|e| e.event_type == "endpoint_committed")
                    .count(),
                1
            );
            let request = report
                .events
                .iter()
                .find(|e| e.event_type == "llm_requested")
                .unwrap();
            assert_eq!(request.payload["transcript"], "Friday evening.");
            assert!(audit::analyze(&report.events).violations.is_empty());
            assert_eq!(report.active_tasks, 0);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn short_backchannel_preserves_playback_but_commands_interrupt_within_budget() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (utterance, duration, asr_delay) in [
                ("mm-hmm", 160, 40),
                ("嗯", 160, 40),
                ("Stop. Make it Friday instead.", 300, 40),
                ("", 300, 40),
                ("", 160, 500),
            ] {
                let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
                let mut config = SessionConfig::default();
                config.endpoint.backchannel_max_ms = Some(220);
                config.asr.first_ms = asr_delay;
                let mut factory = FakeProviders::default();
                factory.turns[1].partials = vec![point(0, utterance)];
                let (session, mut handle) = Session::new(
                    config,
                    Rc::new(factory),
                    clock.clone(),
                    Box::<CountingSink>::default(),
                )
                .unwrap();
                let owner = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(
                    session.run(),
                ));
                let mut seq = 0;
                feed(&handle, clock.as_ref(), &mut seq, 800, 2000, Some(true))
                    .await
                    .unwrap();
                feed(&handle, clock.as_ref(), &mut seq, 500, 0, Some(false))
                    .await
                    .unwrap();
                wait_until(&mut handle, |s| s.started_replies == 1)
                    .await
                    .unwrap();
                let onset = clock.now_ms();
                let short = voice_runtime::endpoint::is_backchannel(utterance);
                feed(
                    &handle,
                    clock.as_ref(),
                    &mut seq,
                    duration,
                    2000,
                    Some(true),
                )
                .await
                .unwrap();
                feed(&handle, clock.as_ref(), &mut seq, 1500, 0, Some(false))
                    .await
                    .unwrap();
                wait_until(&mut handle, |s| s.completed_replies >= 1)
                    .await
                    .unwrap();
                handle.close();
                let report = owner.await.unwrap();
                let a = audit::analyze(&report.events);
                assert!(a.violations.is_empty(), "{:?}", a.violations);
                if short {
                    assert!(a.metrics.interruptions.is_empty());
                    assert!(
                        report
                            .events
                            .iter()
                            .any(|e| e.event_type == "backchannel_rejected")
                    );
                    assert_eq!(
                        report.replies[0].heard_text(),
                        report.replies[0].generated_text
                    );
                } else {
                    assert_eq!(a.metrics.interruptions.len(), 1);
                    let stopped = report
                        .events
                        .iter()
                        .find(|e| e.event_type == "playback_stopped")
                        .unwrap();
                    assert!(stopped.timestamp - onset <= 250);
                }
                assert_eq!(report.active_tasks, 0);
            }
        })
        .await;
}
