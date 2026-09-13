use std::{cell::RefCell, rc::Rc};
use voice_runtime::{
    audio::AudioFrame,
    clock::{Clock, TokioClock},
    endpoint::EndpointConfig,
    fake::FakeProviders,
    playback::{PlaybackError, PlaybackSink},
    scenario,
    session::{Session, SessionConfig, SessionReport},
};

#[derive(Default)]
struct SinkState {
    calls: usize,
    closed: bool,
    after_close: usize,
    fail_at: Option<usize>,
}
struct ObservedSink(Rc<RefCell<SinkState>>);
impl PlaybackSink for ObservedSink {
    fn consume(&mut self, _: &[i16], _: u64, _: u64) -> Result<(), PlaybackError> {
        let mut s = self.0.borrow_mut();
        if s.closed {
            s.after_close += 1;
        }
        if s.fail_at == Some(s.calls) {
            return Err(PlaybackError::Sink("injected device failure".into()));
        }
        s.calls += 1;
        Ok(())
    }
    fn close(&mut self) -> Result<(), PlaybackError> {
        self.0.borrow_mut().closed = true;
        Ok(())
    }
}

async fn fault_run(config: SessionConfig, disconnect: bool, fail_sink: bool) -> SessionReport {
    let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
    let state = Rc::new(RefCell::new(SinkState {
        fail_at: fail_sink.then_some(3),
        ..Default::default()
    }));
    let (session, handle) = Session::new(
        config,
        Rc::new(FakeProviders::default()),
        clock.clone(),
        Box::new(ObservedSink(state.clone())),
    )
    .unwrap();
    let owner = tokio::task::spawn_local(session.run());
    for seq in 0..100 {
        let timestamp = clock.now_ms();
        clock.sleep_until(timestamp + 20).await;
        let frame =
            AudioFrame::new(seq, timestamp, vec![if seq < 40 { 2000 } else { 0 }; 320]).unwrap();
        if handle.send_audio(frame).await.is_err() {
            break;
        }
    }
    if !disconnect {
        clock.sleep_until(clock.now_ms() + 2000).await;
        handle.close();
    }
    drop(handle);
    let report = owner.await.unwrap();
    let calls = state.borrow().calls;
    clock.sleep_until(clock.now_ms() + 500).await;
    assert_eq!(state.borrow().calls, calls);
    assert_eq!(state.borrow().after_close, 0);
    assert!(state.borrow().closed);
    assert_eq!(report.active_tasks, 0);
    assert!(report.queues.iter().all(|q| q.peak <= q.capacity));
    report
}

#[tokio::test(start_paused = true)]
async fn every_provider_timeout_is_supervised() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for stage in ["vad", "asr", "llm", "tts"] {
                let mut config = SessionConfig::default();
                let timing = match stage {
                    "vad" => &mut config.vad,
                    "asr" => &mut config.asr,
                    "llm" => &mut config.llm,
                    _ => &mut config.tts,
                };
                timing.stall_at = Some(0);
                timing.first_timeout_ms = 200;
                let report = fault_run(config, false, false).await;
                assert!(
                    report
                        .events
                        .iter()
                        .any(|e| e.event_type == "provider_failed" && e.payload["stage"] == stage),
                    "missing timeout for {stage}"
                );
            }
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn panic_disconnect_sink_failure_and_journal_failure_release_resources() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut config = SessionConfig::default();
            config.llm.panic_at = Some(0);
            assert_eq!(
                fault_run(config, false, false).await.close_reason,
                "background_task_panicked"
            );
            assert_eq!(
                fault_run(SessionConfig::default(), true, false)
                    .await
                    .close_reason,
                "client_disconnected"
            );
            assert!(
                fault_run(SessionConfig::default(), false, true)
                    .await
                    .close_reason
                    .contains("sink failure")
            );
            let config = SessionConfig {
                journal_fail_after: Some(20),
                ..Default::default()
            };
            assert!(!fault_run(config, false, false).await.trace_complete);
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn asr_overload_is_explicit_and_does_not_expand_queues() {
    tokio::task::LocalSet::new().run_until(async {
        let mut config = SessionConfig { asr_capacity: 16, ..Default::default() };
        config.asr.interval_ms = 200;
        let report = fault_run(config, false, false).await;
        assert!(report.events.iter().any(|e| e.event_type == "turn_failed" && e.payload["reason"] == "asr_overload"));
    }).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_cancel_at_frame_boundaries_and_close_are_idempotent() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for offset in [0, 1, 19, 20, 21] {
                let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
                let state = Rc::new(RefCell::new(SinkState::default()));
                let (session, mut handle) = Session::new(
                    SessionConfig::default(),
                    Rc::new(FakeProviders::default()),
                    clock.clone(),
                    Box::new(ObservedSink(state.clone())),
                )
                .unwrap();
                let owner = tokio::task::spawn_local(session.run());
                let mut seq = 0;
                scenario::feed(&handle, clock.as_ref(), &mut seq, 800, 2000, Some(true))
                    .await
                    .unwrap();
                scenario::feed(&handle, clock.as_ref(), &mut seq, 500, 0, Some(false))
                    .await
                    .unwrap();
                scenario::wait_until(&mut handle, |s| s.started_replies == 1)
                    .await
                    .unwrap();
                clock.sleep_until(clock.now_ms() + offset).await;
                for _ in 0..100 {
                    handle.cancel_generation().unwrap();
                }
                clock.sleep_until(clock.now_ms() + 60).await;
                handle.close();
                handle.close();
                let report = owner.await.unwrap();
                assert_eq!(
                    report
                        .events
                        .iter()
                        .filter(|e| e.event_type == "generation_cancelled")
                        .count(),
                    1
                );
                assert_eq!(report.active_tasks, 0);
                assert!(
                    handle
                        .send_audio(AudioFrame::new(seq, clock.now_ms(), vec![0; 320]).unwrap())
                        .await
                        .is_err()
                );
                let calls = state.borrow().calls;
                clock.sleep_until(clock.now_ms() + 100).await;
                assert_eq!(state.borrow().calls, calls);
            }
        })
        .await;
}

#[test]
fn endpoint_policy_handles_unseen_phrasing_and_unicode_without_whole_sentence_matching() {
    let p = EndpointConfig::default();
    for text in [
        "Could we meet at",
        "I want to",
        "It depends because",
        "Let me think…",
    ] {
        assert_eq!(p.silence_threshold(text), 1000);
    }
    for text in ["Next Thursday works.", "安排好了。", "Are you available?"] {
        assert_eq!(p.silence_threshold(text), 240);
    }
    assert_eq!(p.silence_threshold("Maybe tomorrow"), 600);
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 32, rng_seed: proptest::test_runner::RngSeed::Fixed(20260913), ..Default::default()
    })]
    #[test]
    fn jitter_backpressure_and_cancellation_preserve_playback(seed in 0u64..10_000, jitter in 0u64..12, frames in 1usize..26) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
        let report = rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let mut config = SessionConfig { playback_samples: frames * 320, ..Default::default() };
            config.tts.seed = seed; config.tts.jitter_ms = jitter;
            config.llm.seed = seed; config.llm.jitter_ms = jitter;
            scenario::run("C", config, Rc::new(TokioClock::default())).await
        }));
        proptest::prop_assert_eq!(report.active_tasks, 0);
        proptest::prop_assert!(report.queues.iter().all(|q| q.peak <= q.capacity));
        for cancel in report.events.iter().filter(|e| e.event_type == "generation_cancelled") {
            proptest::prop_assert!(!report.events.iter().any(|e| e.event_type == "playback_progress" && e.identity() == cancel.identity() && e.payload["end_ms"].as_u64().unwrap() > cancel.timestamp));
        }
    }
}

#[tokio::test]
async fn real_clock_concurrent_sessions_release_all_tasks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut owners = tokio::task::JoinSet::new();
            for index in 0..8 {
                owners.spawn_local(async move {
                    let config = SessionConfig {
                        session_id: format!("stress-{index}"),
                        ..Default::default()
                    };
                    fault_run(config, true, false).await
                });
            }
            while let Some(result) = owners.join_next().await {
                assert_eq!(result.unwrap().active_tasks, 0);
            }
        })
        .await;
}
