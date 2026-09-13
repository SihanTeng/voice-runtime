use std::rc::Rc;
use voice_runtime::{
    audit,
    clock::{Clock, TokioClock},
    fake::{FakeProviders, TranscriptPoint, TurnScript},
    playback::CountingSink,
    scenario::{feed, wait_until},
    session::Session,
};
fn point(voiced_ms: u64, text: &str) -> TranscriptPoint {
    TranscriptPoint {
        voiced_ms,
        text: text.into(),
    }
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
