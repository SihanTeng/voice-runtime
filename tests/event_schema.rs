use std::{io::Cursor, rc::Rc};
use voice_runtime::{audit, clock::TokioClock, scenario};
#[test]
fn committed_trace_uses_the_current_schema() {
    for trace in [
        &include_bytes!("../sample-output/trace.jsonl")[..],
        &include_bytes!("../sample-output/slow-vad/trace.jsonl")[..],
        &include_bytes!("../sample-output/input-faults/trace.jsonl")[..],
    ] {
        let events = audit::read_jsonl(Cursor::new(trace)).unwrap();
        assert!(events.iter().all(|event| event.schema_version == 3));
        assert!(audit::analyze(&events).violations.is_empty());
    }
}
#[tokio::test(start_paused = true)]
async fn missing_wrong_typed_and_unknown_fields_cannot_fabricate_valid_metrics() {
    let r = tokio::task::LocalSet::new()
        .run_until(scenario::run(
            "C",
            Default::default(),
            Rc::new(TokioClock::default()),
        ))
        .await;
    for (name, field) in [
        ("endpoint_committed", "speech_end_ms"),
        ("audio_frame", "capture_ms"),
        ("interruption_decision", "onset_ms"),
        ("session_closed", "active_tasks"),
    ] {
        for wrong_type in [false, true] {
            let mut events = r.events.clone();
            let event = events.iter_mut().find(|e| e.event_type == name).unwrap();
            if wrong_type {
                event.payload[field] = serde_json::json!("0");
            } else {
                event.payload.as_object_mut().unwrap().remove(field);
            }
            assert!(event.decode().is_err());
            let a = audit::analyze(&events);
            assert!(!a.violations.is_empty());
            assert!(a.metrics.turns.is_empty());
            let mut jsonl = Vec::new();
            voice_runtime::event::write_jsonl(&events, &mut jsonl).unwrap();
            assert!(audit::read_jsonl(Cursor::new(jsonl)).is_err());
        }
    }
    let mut event = r.events[0].clone();
    event.schema_version = 999;
    assert!(event.decode().is_err());
    event = r.events[0].clone();
    event.event_type = "unknown_event".into();
    assert!(event.decode().is_err());
    event = r.events[0].clone();
    event.payload["unexpected"] = serde_json::json!(true);
    assert!(event.decode().is_err());
    let mut event = r
        .events
        .iter()
        .find(|e| e.event_type == "asr_partial")
        .unwrap()
        .clone();
    event.turn_id = None;
    assert!(event.decode().is_err());
    for version in [1, 2] {
        let mut old = r.events[0].clone();
        old.schema_version = version;
        assert!(old.decode().is_err());
    }
    let mut missing_version = serde_json::to_value(&r.events[0]).unwrap();
    missing_version
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<voice_runtime::event::Event>(missing_version).is_err());
    let mut wire = serde_json::to_value(&r.events[0]).unwrap();
    wire["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<voice_runtime::event::Event>(wire).is_err());
}
