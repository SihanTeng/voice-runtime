use std::rc::Rc;
use voice_runtime::{
    audit,
    clock::TokioClock,
    scenario,
    session::{RecoveryPolicy, SessionConfig},
};

struct FailedClarification(voice_runtime::fake::FakeProviders);
struct TimedOutLlm;
impl voice_runtime::fake::LlmProvider for TimedOutLlm {
    fn next_text(&mut self) -> Result<Option<String>, voice_runtime::provider::ProviderError> {
        Err(voice_runtime::provider::ProviderError::Timeout)
    }
}
impl voice_runtime::fake::ProviderFactory for FailedClarification {
    fn recovery(&self, _: &str) -> Option<voice_runtime::fake::ResponseProviders> {
        Some(voice_runtime::fake::ResponseProviders {
            llm: Box::new(TimedOutLlm),
            tts: self.0.tts(),
        })
    }
    fn vad(&self) -> Box<dyn voice_runtime::fake::VadProvider> {
        self.0.vad()
    }
    fn asr(&self, ordinal: usize) -> Box<dyn voice_runtime::fake::AsrProvider> {
        self.0.asr(ordinal)
    }
    fn llm(
        &self,
        ordinal: usize,
        transcript: &str,
        history: &[String],
    ) -> Box<dyn voice_runtime::fake::LlmProvider> {
        self.0.llm(ordinal, transcript, history)
    }
    fn tts(&self) -> Box<dyn voice_runtime::fake::TtsProvider> {
        self.0.tts()
    }
}

#[tokio::test(start_paused = true)]
async fn failed_clarification_is_not_retried_and_cancellation_closes_its_tasks() {
    use voice_runtime::{
        clock::Clock,
        fake::{FakeProviders, ProviderFactory},
        playback::CountingSink,
        session::Session,
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            // Exercise a failed fallback, explicit cancellation, and close while it is speaking.
            for mode in ["fail", "cancel", "close"] {
                let mut config = SessionConfig {
                    recovery: Some(Default::default()),
                    ..Default::default()
                };
                config.tts.stall_at = Some(0);
                config.tts.first_timeout_ms = 100;
                let factory: Rc<dyn ProviderFactory> = if mode == "fail" {
                    Rc::new(FailedClarification(FakeProviders::default()))
                } else {
                    Rc::new(FakeProviders::default())
                };
                let clock: Rc<dyn Clock> = Rc::new(TokioClock::default());
                let (session, mut handle) = Session::new(
                    config,
                    factory,
                    clock.clone(),
                    Box::<CountingSink>::default(),
                )
                .unwrap();
                let owner = tokio_util::task::AbortOnDropHandle::new(tokio::task::spawn_local(
                    session.run(),
                ));
                let mut seq = 0;
                scenario::feed(&handle, clock.as_ref(), &mut seq, 800, 2000, Some(true))
                    .await
                    .unwrap();
                scenario::feed(&handle, clock.as_ref(), &mut seq, 600, 0, Some(false))
                    .await
                    .unwrap();
                if mode != "fail" {
                    scenario::wait_until(&mut handle, |s| s.started_replies == 1)
                        .await
                        .unwrap();
                }
                if mode == "cancel" {
                    for _ in 0..10 {
                        handle.cancel_generation().unwrap();
                    }
                }
                if mode != "close" {
                    clock.sleep_until(clock.now_ms() + 500).await;
                }
                handle.close();
                let r = owner.await.unwrap();
                assert_eq!(r.active_tasks, 0);
                assert_eq!(r.replies.len(), 2);
                assert!(r.has_unrecovered_failure());
                let a = audit::analyze(&r.events);
                assert!(a.violations.is_empty(), "{:?}", a.violations);
                assert_eq!(a.metrics.recovery_count, 1);
                assert_eq!(a.metrics.stale_chunk_played_count, 0);
                assert!(
                    r.replies[1].played_samples()
                        < r.replies[1].chunks.iter().map(|c| c.samples).sum::<usize>()
                        || mode == "fail"
                );
                assert!(
                    r.events
                        .iter()
                        .any(|e| e.event_type == "generation_cancelled"
                            && e.generation_id == Some(2))
                );
            }
        })
        .await;
}
#[tokio::test(start_paused = true)]
async fn timeout_recovery_before_and_during_playback_uses_a_fresh_generation() {
    for during in [false, true] {
        let mut config = SessionConfig {
            recovery: Some(RecoveryPolicy::default()),
            ..Default::default()
        };
        if during {
            config.tts.stall_at = Some(70);
            config.tts.idle_timeout_ms = 80;
        } else {
            config.llm.stall_at = Some(0);
            config.llm.first_timeout_ms = 100;
        }
        let report = tokio::task::LocalSet::new()
            .run_until(scenario::run("A", config, Rc::new(TokioClock::default())))
            .await;
        let a = audit::analyze(&report.events);
        assert!(a.violations.is_empty(), "{:?}", a.violations);
        assert_eq!(report.close_reason, "active_close");
        assert_eq!(report.active_tasks, 0);
        assert_eq!(a.metrics.recovery_count, 1);
        assert_eq!(a.metrics.turns[1].endpoint_to_first_audio_ms, None);
        assert_eq!(
            a.metrics.turns[1].recovery_start_to_first_audio_ms,
            Some(80)
        );
        assert_eq!(a.metrics.stale_chunk_played_count, 0);
        assert_eq!(report.replies.len(), 2);
        assert_eq!(
            report.replies[0].identity.turn_id,
            report.replies[1].identity.turn_id
        );
        assert_ne!(
            report.replies[0].identity.generation_id,
            report.replies[1].identity.generation_id
        );
        assert_eq!(report.replies[0].played_samples() > 0, during);
        assert_eq!(
            report.replies[1].heard_text(),
            report.replies[1].generated_text
        );
        let recovery = report
            .events
            .iter()
            .find(|e| e.event_type == "recovery_started")
            .unwrap();
        assert_eq!(
            recovery.payload["heard_context"],
            report.replies[0].heard_text()
        );
        let expected = if during {
            RecoveryPolicy::default().after_audio
        } else {
            RecoveryPolicy::default().before_audio
        };
        assert_eq!(report.replies[1].generated_text, expected);
        assert!(!report.has_unrecovered_failure());
    }
}
#[tokio::test(start_paused = true)]
async fn delayed_error_from_cancelled_worker_does_not_cancel_recovery() {
    let mut config = SessionConfig {
        recovery: Some(Default::default()),
        ..Default::default()
    };
    config.tts.stall_at = Some(0);
    config.tts.first_timeout_ms = 100;
    config.llm.interval_ms = 500;
    config.llm.idle_timeout_ms = 200;
    config.llm.late_chunks = 2;
    config.llm.late_delay_ms = 300;
    let r = tokio::task::LocalSet::new()
        .run_until(scenario::run("A", config, Rc::new(TokioClock::default())))
        .await;
    assert_eq!(r.replies.len(), 2);
    assert_eq!(r.replies[1].heard_text(), r.replies[1].generated_text);
    let recovered_at = r
        .events
        .iter()
        .find(|e| e.event_type == "recovery_started")
        .unwrap()
        .timestamp;
    assert!(r.events.iter().any(|e| e.event_type == "provider_failed"
        && e.timestamp > recovered_at
        && e.generation_id == Some(1)));
    assert!(!r.has_unrecovered_failure());
    assert_eq!(r.active_tasks, 0);
    assert!(audit::analyze(&r.events).violations.is_empty());
}
