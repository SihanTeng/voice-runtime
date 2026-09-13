//! Independent trace reducer: never reads Session's mutable playback ledger.
use crate::{
    event::Event,
    playback::{AudioChunk, ChunkRecord, Consumption, ReplyRecord},
    queue::QueueSnapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, Read},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TurnMetrics {
    pub asr_first_partial_ms: Option<u64>,
    pub asr_final_wait_ms: Option<u64>,
    pub llm_ttft_ms: Option<u64>,
    pub tts_first_audio_ms: Option<u64>,
    pub endpoint_to_first_audio_ms: Option<u64>,
    pub playback_underrun_ms: u64,
    pub turn_id: u64,
    pub generation_id: u64,
    pub endpoint_latency_ms: Option<u64>,
    pub turn_end_to_first_audio_ms: Option<u64>,
    pub played_samples: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptionMetrics {
    pub generation_id: u64,
    pub onset_ms: u64,
    pub speech_start_detection_ms: u64,
    pub interruption_decision_ms: u64,
    pub playback_stop_ms: Option<u64>,
    pub interruption_to_playback_stop_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Metrics {
    pub maximum_input_age_ms: Option<u64>,
    pub maximum_cancel_to_task_exit_ms: Option<u64>,
    pub turns: Vec<TurnMetrics>,
    pub interruptions: Vec<InterruptionMetrics>,
    pub overlap_duration_ms: Option<u64>,
    pub false_interruption_count: Option<u64>,
    pub stale_chunk_received_count: u64,
    pub stale_chunk_played_count: u64,
    pub maximum_queue_lengths: Vec<QueueSnapshot>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Audit {
    pub metrics: Metrics,
    pub replies: Vec<ReplyRecord>,
    pub violations: Vec<String>,
}

fn number(event: &Event, key: &str) -> u64 {
    event.payload[key]
        .as_u64()
        .expect("schema-validated numeric field")
}

pub fn analyze(events: &[Event]) -> Audit {
    let mut audit = Audit {
        metrics: Metrics::default(),
        replies: Vec::new(),
        violations: Vec::new(),
    };
    // Do not compute plausible metrics from structurally corrupt evidence.
    for event in events {
        if let Err(error) = event.decode() {
            audit.violations.push(format!(
                "invalid event schema at {}: {error}",
                event.sequence_number
            ));
        }
    }
    if !audit.violations.is_empty() {
        return audit;
    }
    let mut reply_indices = BTreeMap::new();
    let mut chunks = BTreeMap::<(u64, u64), AudioChunk>::new();
    let mut cancelled = BTreeSet::new();
    let mut active = None;
    let mut ends = BTreeMap::new();
    let mut next_samples = BTreeMap::<u64, u64>::new();
    let mut progress = Vec::new();
    let mut closed = false;
    let mut previous_time = 0;
    let mut asr_requested = BTreeMap::new();
    let mut asr_first = BTreeMap::new();
    let mut endpoint_at = BTreeMap::new();
    let mut llm_at = BTreeMap::new();
    let mut tts_at = BTreeMap::new();
    let mut last_progress_end = BTreeMap::<u64, u64>::new();
    let mut cancel_at = BTreeMap::new();
    for (sequence, event) in events.iter().enumerate() {
        if closed
            || event.sequence_number != sequence as u64
            || event.timestamp < previous_time
            || events
                .first()
                .is_some_and(|first| first.session_id != event.session_id)
        {
            audit
                .violations
                .push(format!("event ordering/session mismatch at {sequence}"));
        }
        previous_time = event.timestamp;
        let generation = event.generation_id.unwrap_or(0);
        match event.event_type.as_str() {
            "audio_frame" => {
                let end =
                    number(event, "capture_ms").saturating_add(number(event, "valid_samples") / 16);
                audit.metrics.maximum_input_age_ms = Some(
                    audit
                        .metrics
                        .maximum_input_age_ms
                        .unwrap_or(0)
                        .max(event.timestamp.saturating_sub(end)),
                );
            }
            "asr_requested" => {
                asr_requested.insert(event.turn_id.unwrap_or(0), event.timestamp);
            }
            "asr_partial" => {
                let turn = event.turn_id.unwrap_or(0);
                if let Some(start) = asr_requested.get(&turn) {
                    asr_first
                        .entry(turn)
                        .or_insert(event.timestamp.saturating_sub(*start));
                }
            }
            "asr_final" => {
                if let Some(turn) = audit
                    .metrics
                    .turns
                    .iter_mut()
                    .find(|t| t.generation_id == generation)
                {
                    turn.asr_final_wait_ms = endpoint_at
                        .get(&generation)
                        .and_then(|at| event.timestamp.checked_sub(*at));
                }
            }
            "llm_requested" => {
                llm_at.insert(generation, event.timestamp);
            }
            "tts_requested" => {
                tts_at.insert(generation, number(event, "request_ms"));
            }
            "task_exited" => {
                if let Some(at) = cancel_at.get(&generation) {
                    let elapsed = event.timestamp.saturating_sub(*at);
                    audit.metrics.maximum_cancel_to_task_exit_ms = Some(
                        audit
                            .metrics
                            .maximum_cancel_to_task_exit_ms
                            .unwrap_or(0)
                            .max(elapsed),
                    );
                }
            }
            "endpoint_committed" => {
                let Some(id) = event.identity() else {
                    audit.violations.push("endpoint missing identity".into());
                    continue;
                };
                if reply_indices.contains_key(&generation) || active.is_some() {
                    audit
                        .violations
                        .push("overlapping or reused generation".into());
                }
                reply_indices.insert(generation, audit.replies.len());
                audit.replies.push(ReplyRecord::new(id));
                active = Some(generation);
                let end = number(event, "speech_end_ms");
                ends.insert(generation, end);
                endpoint_at.insert(generation, event.timestamp);
                audit.metrics.turns.push(TurnMetrics {
                    turn_id: id.turn_id,
                    generation_id: generation,
                    asr_first_partial_ms: asr_first.get(&id.turn_id).copied(),
                    endpoint_latency_ms: event.timestamp.checked_sub(end),
                    ..Default::default()
                });
            }
            "llm_chunk" => {
                if let Some(turn) = audit
                    .metrics
                    .turns
                    .iter_mut()
                    .find(|t| t.generation_id == generation)
                    && turn.llm_ttft_ms.is_none()
                {
                    turn.llm_ttft_ms = llm_at
                        .get(&generation)
                        .and_then(|at| event.timestamp.checked_sub(*at));
                }
                if active != Some(generation) {
                    audit.violations.push("accepted stale LLM text".into());
                }
                if let Some(index) = reply_indices.get(&generation) {
                    audit.replies[*index]
                        .generated_text
                        .push_str(event.payload["text"].as_str().unwrap_or(""));
                }
            }
            "tts_chunk" => match serde_json::from_value::<AudioChunk>(event.payload.clone()) {
                Ok(chunk) => {
                    if let Some(turn) = audit
                        .metrics
                        .turns
                        .iter_mut()
                        .find(|t| t.generation_id == generation)
                        && turn.tts_first_audio_ms.is_none()
                        && active == Some(generation)
                    {
                        turn.tts_first_audio_ms = tts_at
                            .get(&generation)
                            .and_then(|at| event.timestamp.checked_sub(*at));
                    }
                    if active != Some(generation) || cancelled.contains(&generation) {
                        audit.metrics.stale_chunk_received_count += 1;
                        if let Some(index) = reply_indices.get(&generation) {
                            audit.replies[*index]
                                .chunks
                                .push(ChunkRecord::rejected(&chunk, "stale_generation"));
                        }
                    }
                    if chunk.identity != event.identity().unwrap_or_default()
                        || chunk.samples.is_empty()
                        || chunk.samples.len() > 320
                    {
                        audit
                            .violations
                            .push("invalid audio chunk identity/size".into());
                    }
                    if chunks.insert((generation, chunk.sequence), chunk).is_some() {
                        audit.violations.push("duplicate TTS chunk".into());
                    }
                }
                Err(_) => audit.violations.push("invalid TTS chunk payload".into()),
            },
            "audio_enqueued" => {
                if active != Some(generation) || cancelled.contains(&generation) {
                    audit.violations.push("stale audio enqueued".into());
                }
                let key = (generation, number(event, "chunk_sequence"));
                if let (Some(c), Some(index)) = (chunks.get(&key), reply_indices.get(&generation)) {
                    let reply = &mut audit.replies[*index];
                    if reply.generated_text.get(c.text_range.clone()).is_none()
                        || c.sequence != reply.chunks.len() as u64
                    {
                        audit
                            .violations
                            .push("invalid text range or enqueue order".into());
                    }
                    reply.chunks.push(ChunkRecord {
                        sequence: c.sequence,
                        text_range: c.text_range.clone(),
                        sample_start: c.sample_start,
                        samples: c.samples.len(),
                        word_samples: c.word_samples,
                        word_offset: c.word_offset,
                        enqueued: true,
                        played_samples: 0,
                        truncated: false,
                        rejection_reason: None,
                    });
                } else {
                    audit.violations.push("enqueue without synthesis".into());
                }
            }
            "audio_rejected" => {
                match serde_json::from_value::<ChunkRecord>(event.payload.clone()) {
                    Ok(record) => {
                        if let Some(index) = reply_indices.get(&generation) {
                            audit.replies[*index].chunks.push(record);
                        }
                    }
                    Err(_) => audit
                        .violations
                        .push("invalid rejected chunk record".into()),
                }
            }
            "playback_started" => {
                if active != Some(generation) {
                    audit.violations.push("stale playback started".into());
                }
                if let Some(turn) = audit
                    .metrics
                    .turns
                    .iter_mut()
                    .find(|t| t.generation_id == generation)
                {
                    turn.endpoint_to_first_audio_ms = endpoint_at
                        .get(&generation)
                        .and_then(|at| event.timestamp.checked_sub(*at));
                    turn.turn_end_to_first_audio_ms = ends
                        .get(&generation)
                        .and_then(|end| event.timestamp.checked_sub(*end));
                }
            }
            "playback_progress" => {
                match serde_json::from_value::<Consumption>(event.payload.clone()) {
                    Ok(p) => {
                        if active != Some(generation) || cancelled.contains(&generation) {
                            audit.metrics.stale_chunk_played_count += 1;
                            audit.violations.push("stale audio played".into());
                        }
                        let expected = next_samples.entry(generation).or_default();
                        if p.sample_start != *expected
                            || p.end_ms > event.timestamp
                            || p.end_ms < p.start_ms
                            || p.samples as u64 != (p.end_ms - p.start_ms) * 16
                            || Some(p.identity) != event.identity()
                        {
                            audit
                                .violations
                                .push("non-contiguous or invalid consumed samples".into());
                        }
                        *expected += p.samples as u64;
                        if let Some(index) = reply_indices.get(&generation) {
                            if let Some(chunk) = audit.replies[*index]
                                .chunks
                                .get_mut(p.chunk_sequence as usize)
                            {
                                if !chunk.enqueued {
                                    audit.violations.push("rejected audio consumed".into());
                                }
                                chunk.played_samples += p.samples;
                                if chunk.played_samples > chunk.samples {
                                    audit.violations.push("chunk over-consumed".into());
                                }
                            } else {
                                audit.violations.push("playback without enqueue".into());
                            }
                        } else {
                            audit.violations.push("playback without generation".into());
                        }
                        if let Some(turn) = audit
                            .metrics
                            .turns
                            .iter_mut()
                            .find(|t| t.generation_id == generation)
                        {
                            turn.played_samples += p.samples as u64;
                            if let Some(end) = last_progress_end.insert(generation, p.end_ms) {
                                turn.playback_underrun_ms += p.start_ms.saturating_sub(end);
                            }
                        }
                        progress.push(p);
                    }
                    Err(_) => audit.violations.push("invalid consumption payload".into()),
                }
            }
            "interruption_decision" => {
                let onset = number(event, "onset_ms");
                let detected = number(event, "detected_ms");
                audit.metrics.interruptions.push(InterruptionMetrics {
                    generation_id: generation,
                    onset_ms: onset,
                    speech_start_detection_ms: detected.saturating_sub(onset),
                    interruption_decision_ms: event.timestamp.saturating_sub(detected),
                    playback_stop_ms: None,
                    interruption_to_playback_stop_ms: None,
                });
            }
            "generation_cancelled" => {
                cancelled.insert(generation);
                cancel_at.insert(generation, event.timestamp);
                if let Some(index) = reply_indices.get(&generation) {
                    audit.replies[*index].interrupted = true;
                }
            }
            "playback_stopped" => {
                active = None;
                if let Some(index) = reply_indices.get(&generation) {
                    for c in &mut audit.replies[*index].chunks {
                        c.truncated = c.played_samples < c.samples;
                    }
                }
                if let Some(interruption) = audit
                    .metrics
                    .interruptions
                    .iter_mut()
                    .rev()
                    .find(|i| i.generation_id == generation)
                {
                    let decision = interruption.onset_ms
                        + interruption.speech_start_detection_ms
                        + interruption.interruption_decision_ms;
                    interruption.playback_stop_ms = event.timestamp.checked_sub(decision);
                    interruption.interruption_to_playback_stop_ms =
                        event.timestamp.checked_sub(interruption.onset_ms);
                }
            }
            "session_closed" => {
                closed = true;
                if number(event, "active_tasks") != 0 {
                    audit.violations.push("tasks still running at close".into());
                }
                match serde_json::from_value::<Vec<QueueSnapshot>>(event.payload["queues"].clone())
                {
                    Ok(queues) => {
                        for q in &queues {
                            if q.capacity == 0 || q.peak > q.capacity {
                                audit.violations.push(format!("queue overflow: {}", q.name));
                            }
                        }
                        audit.metrics.maximum_queue_lengths = queues;
                    }
                    Err(_) => audit.violations.push("missing queue statistics".into()),
                }
            }
            _ => {}
        }
    }
    if !closed
        || events
            .first()
            .is_none_or(|e| e.event_type != "session_started")
    {
        audit.violations.push("incomplete session trace".into());
    }
    let known_truth = events
        .iter()
        .any(|e| e.event_type == "audio_frame" && e.payload["speech_truth"].is_boolean());
    // A dropped/rejected packet can contain unobserved speech. Do not label a
    // partial oracle as the actual acoustic overlap or false-interruption count.
    let incomplete_truth = events.first().is_some_and(|e| {
        let faults = &e.payload["config"]["input_faults"];
        faults["drop_every"].is_number()
            || faults["reorder_every"].is_number()
            || faults["drop_burst"].is_object()
    });
    if known_truth && !incomplete_truth {
        let speech: Vec<(u64, u64)> = events
            .iter()
            .filter(|e| e.event_type == "audio_frame" && e.payload["speech_truth"] == true)
            .map(|e| {
                let start = number(e, "capture_ms");
                (start, start + number(e, "valid_samples") / 16)
            })
            .collect();
        audit.metrics.overlap_duration_ms = Some(
            progress
                .iter()
                .map(|p| {
                    speech
                        .iter()
                        .map(|&(start, end)| {
                            p.end_ms.min(end).saturating_sub(p.start_ms.max(start))
                        })
                        .sum::<u64>()
                })
                .sum(),
        );
        audit.metrics.false_interruption_count = Some(
            audit
                .metrics
                .interruptions
                .iter()
                .filter(|i| {
                    !speech
                        .iter()
                        .any(|&(start, end)| start <= i.onset_ms && i.onset_ms < end)
                })
                .count() as u64,
        );
    }
    audit
}

pub fn read_jsonl(mut reader: impl BufRead) -> std::io::Result<Vec<Event>> {
    let mut events = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = reader.by_ref().take(65_537).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > 65_536 || events.len() >= 100_000 {
            return Err(std::io::Error::other("trace input limit exceeded"));
        }
        let event: Event = serde_json::from_slice(&line)?;
        event.decode().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("trace line {}: {error}", events.len() + 1),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

pub fn mermaid(events: &[Event]) -> String {
    let mut out = String::from(
        "sequenceDiagram\n    participant U as Audio input\n    participant R as Session\n    participant P as Providers\n    participant S as Playback sink\n",
    );
    for e in events {
        let route = match e.event_type.as_str() {
            "speech_start" => "U->>R",
            "endpoint_committed" | "llm_requested" => "R->>P",
            "playback_started" | "playback_stopped" => "R->>S",
            "generation_cancelled" => "R-->>P",
            "stale_event_dropped" => "P-->>R",
            "session_closed" => "R->>R",
            _ => continue,
        };
        // Only fixed event names and numeric identifiers enter Mermaid, never provider text.
        out.push_str(&format!(
            "    {route}: {}ms {} turn={} gen={}\n",
            e.timestamp,
            e.event_type,
            e.turn_id.unwrap_or(0),
            e.generation_id.unwrap_or(0)
        ));
    }
    out
}
