use std::rc::Rc;
use voice_runtime::{
    audit, clock::TokioClock, evaluation::Distribution, scenario, session::SessionConfig,
};
#[test]
fn quantiles_preserve_missing_values_and_do_not_add_stage_percentiles() {
    let mut values: Vec<_> = (1..=100).map(Some).collect();
    values.push(None);
    let d = Distribution::from_values(&values);
    assert_eq!(
        (d.count, d.missing, d.p50_ms, d.p95_ms, d.p99_ms),
        (100, 1, Some(50), Some(95), Some(99))
    );
    assert_eq!(Distribution::from_values(&[None]).p99_ms, None);
}
#[tokio::test(start_paused = true)]
async fn phase_latency_and_queue_age_are_measured_from_events() {
    let report = tokio::task::LocalSet::new()
        .run_until(scenario::run(
            "A",
            Default::default(),
            Rc::new(TokioClock::default()),
        ))
        .await;
    let a = audit::analyze(&report.events);
    assert!(a.violations.is_empty());
    let t = &a.metrics.turns[0];
    assert_eq!(
        (
            t.asr_first_partial_ms,
            t.asr_final_wait_ms,
            t.llm_ttft_ms,
            t.tts_first_audio_ms
        ),
        (Some(40), Some(0), Some(80), Some(60))
    );
    assert_eq!(t.endpoint_to_first_audio_ms, Some(140));
    assert_eq!(t.playback_underrun_ms, 0);
    assert!(
        a.metrics
            .maximum_queue_lengths
            .iter()
            .any(|q| q.name.starts_with("llm_tts") && q.max_wait_ms.is_some_and(|ms| ms > 0))
    );
}
#[tokio::test(start_paused = true)]
async fn burst_latency_and_packet_loss_are_seeded_and_reproducible() {
    for seed in [1, 3, 7] {
        let mut config: SessionConfig =
            serde_json::from_str(include_str!("../examples/tail-latency.json")).unwrap();
        config.llm.seed = seed;
        config.tts.seed = seed;
        config.input_faults.drop_burst = Some(voice_runtime::impairment::DropBurst {
            every: 97,
            length: 3,
        });
        let mut previous = None;
        for _ in 0..2 {
            let r = tokio::task::LocalSet::new()
                .run_until(scenario::run(
                    "A",
                    config.clone(),
                    Rc::new(TokioClock::default()),
                ))
                .await;
            let a = audit::analyze(&r.events);
            assert!(a.violations.is_empty(), "{:?}", a.violations);
            assert_eq!(a.metrics.stale_chunk_played_count, 0);
            assert_eq!(r.active_tasks, 0);
            assert_eq!(r.close_reason, "active_close");
            assert_eq!(r.replies.len(), 1);
            assert_eq!(r.replies[0].heard_text(), r.replies[0].generated_text);
            assert!(r.events.iter().any(|e| e.event_type == "audio_frame"
                && e.payload["missing_frames"].as_u64().is_some_and(|n| n > 0)));
            if let Some(old) = previous {
                assert_eq!(r.events, old);
            }
            previous = Some(r.events);
        }
    }
}
#[test]
fn evaluation_cli_retains_trace_seeds_and_missing_failure_observations() {
    let temp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
        .args(["evaluate", "--runs", "2", "--output"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["failures"], 0);
    assert_eq!(summary["trials"][0]["seed"], 7);
    assert_eq!(summary["trials"][1]["seed"], 8);
    assert!(summary["distributions"]["llm_ttft"].is_object());
    assert!(temp.path().join("trial-0001.jsonl").exists());
    let failures = temp.path().join("failures");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
        .args([
            "evaluate",
            "--runs",
            "2",
            "--config",
            "examples/timeout.json",
            "--output",
        ])
        .arg(&failures)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let failed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(failures.join("summary.json")).unwrap()).unwrap();
    assert_eq!(failed["failures"], 2);
    assert_eq!(failed["distributions"]["llm_ttft"]["count"], 0);
    assert_eq!(failed["distributions"]["llm_ttft"]["missing"], 2);
    assert_eq!(
        failed["distributions"]["llm_ttft"]["p99_ms"],
        serde_json::Value::Null
    );
}
