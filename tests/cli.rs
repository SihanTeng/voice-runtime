use std::{fs::File, io::BufReader, process::Command};

#[test]
fn injected_failure_exits_nonzero_but_keeps_an_inspectable_trace() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("fault.json");
    std::fs::write(&config, r#"{"llm":{"stall_at":0,"first_timeout_ms":200}}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
        .args(["run", "--scenario", "A", "--output"])
        .arg(temp.path())
        .arg("--config")
        .arg(config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    let events = voice_runtime::audit::read_jsonl(BufReader::new(
        File::open(temp.path().join("A/trace.jsonl")).unwrap(),
    ))
    .unwrap();
    assert_eq!(events.last().unwrap().event_type, "session_closed");
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "provider_failed" && e.payload["stage"] == "llm")
    );
}

#[test]
fn cli_runs_scenario_and_replay_with_identical_metrics() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
        .args(["run", "--scenario", "E", "--output"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = temp.path().join("E/trace.jsonl");
    let replay = temp.path().join("replay");
    let output = Command::new(env!("CARGO_BIN_EXE_voice-runtime"))
        .arg("replay")
        .arg(&trace)
        .arg("--output")
        .arg(&replay)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let online: serde_json::Value =
        serde_json::from_reader(File::open(temp.path().join("E/metrics.json")).unwrap()).unwrap();
    let replay: serde_json::Value =
        serde_json::from_reader(File::open(replay.join("audit.json")).unwrap()).unwrap();
    assert_eq!(online, replay["metrics"]);
    let events =
        voice_runtime::audit::read_jsonl(BufReader::new(File::open(trace).unwrap())).unwrap();
    assert_eq!(
        voice_runtime::audit::analyze(&events)
            .metrics
            .stale_chunk_played_count,
        0
    );
}

#[tokio::test]
async fn real_clock_barge_in_meets_stop_budget() {
    let report = tokio::task::LocalSet::new()
        .run_until(voice_runtime::scenario::run(
            "C",
            Default::default(),
            std::rc::Rc::new(voice_runtime::clock::TokioClock::default()),
        ))
        .await;
    let audit = voice_runtime::audit::analyze(&report.events);
    assert!(audit.violations.is_empty(), "{:?}", audit.violations);
    assert_eq!(audit.metrics.interruptions.len(), 1);
    assert!(
        audit.metrics.interruptions[0]
            .interruption_to_playback_stop_ms
            .unwrap()
            <= 250
    );
    assert_eq!(audit.metrics.stale_chunk_played_count, 0);
    assert_eq!(report.active_tasks, 0);
}
