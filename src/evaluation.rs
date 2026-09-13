//! Bounded experiment aggregation. Quantiles use measured end-to-end observations.
use crate::{
    audit,
    session::{SessionConfig, SessionReport},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Distribution {
    pub count: usize,
    pub missing: usize,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub p99_ms: Option<u64>,
    pub max_ms: Option<u64>,
}
impl Distribution {
    /// Nearest-rank quantiles. Missing/failed observations are never converted to zero.
    pub fn from_values(values: &[Option<u64>]) -> Self {
        let mut sorted: Vec<_> = values.iter().filter_map(|v| *v).collect();
        sorted.sort_unstable();
        let quantile = |p: usize| {
            if sorted.is_empty() {
                None
            } else {
                Some(sorted[(p * sorted.len()).div_ceil(100) - 1])
            }
        };
        Self {
            count: sorted.len(),
            missing: values.len() - sorted.len(),
            p50_ms: quantile(50),
            p95_ms: quantile(95),
            p99_ms: quantile(99),
            max_ms: sorted.last().copied(),
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Trial {
    pub seed: u64,
    pub close_reason: String,
    pub failed: bool,
    pub metrics: audit::Metrics,
    pub violations: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Evaluation {
    pub scenario: String,
    pub clock: String,
    pub base_config: SessionConfig,
    pub trials: Vec<Trial>,
    pub distributions: BTreeMap<String, Distribution>,
    pub failures: usize,
    pub interpretation: String,
}
impl Evaluation {
    pub fn new(scenario: String, virtual_time: bool, base_config: SessionConfig) -> Self {
        Self {
            scenario,
            clock: if virtual_time { "virtual" } else { "real" }.into(),
            base_config,
            trials: Vec::new(),
            distributions: BTreeMap::new(),
            failures: 0,
            interpretation: concat!(
                "Deterministic fake-provider workload; quantiles describe this configuration, not production SLOs. ",
                "Inspect counts/missing values and failed trials; ",
                "trials without an endpoint have no turn observation. ",
                "p99 from small samples is not a reliable population estimate."
            ).into(),
        }
    }
    pub fn record(&mut self, seed: u64, report: &SessionReport) -> Result<(), String> {
        if self.trials.len() >= 1000 {
            return Err("evaluation trial limit exceeded".into());
        }
        let mut audit = audit::analyze(&report.events);
        if report.active_tasks != 0 || !report.trace_complete || audit.replies != report.replies {
            audit
                .violations
                .push("incomplete lifecycle or ledger mismatch".into());
        }
        let failed = report.close_reason != "active_close"
            || report.has_provider_failure()
            || !audit.violations.is_empty();
        self.failures += usize::from(failed);
        self.trials.push(Trial {
            seed,
            close_reason: report.close_reason.clone(),
            failed,
            metrics: audit.metrics,
            violations: audit.violations,
        });
        Ok(())
    }
    pub fn summarize(&mut self) {
        let mut values: BTreeMap<String, Vec<Option<u64>>> = BTreeMap::new();
        for trial in &self.trials {
            for turn in &trial.metrics.turns {
                for (metric, value) in [
                    ("endpoint_latency", turn.endpoint_latency_ms),
                    ("turn_end_to_first_audio", turn.turn_end_to_first_audio_ms),
                    ("endpoint_to_first_audio", turn.endpoint_to_first_audio_ms),
                    ("asr_first_partial", turn.asr_first_partial_ms),
                    ("asr_final_wait", turn.asr_final_wait_ms),
                    ("llm_ttft", turn.llm_ttft_ms),
                    ("tts_first_audio", turn.tts_first_audio_ms),
                    ("playback_underrun", Some(turn.playback_underrun_ms)),
                ] {
                    values.entry(metric.into()).or_default().push(value);
                }
            }
            for interruption in &trial.metrics.interruptions {
                values
                    .entry("interruption_to_playback_stop".into())
                    .or_default()
                    .push(interruption.interruption_to_playback_stop_ms);
            }
            values
                .entry("maximum_input_age".into())
                .or_default()
                .push(trial.metrics.maximum_input_age_ms);
            values
                .entry("cancel_to_task_exit".into())
                .or_default()
                .push(trial.metrics.maximum_cancel_to_task_exit_ms);
            for q in &trial.metrics.maximum_queue_lengths {
                values
                    .entry(format!("queue.{}.max_wait", q.name))
                    .or_default()
                    .push(q.max_wait_ms);
            }
        }
        self.distributions = values
            .into_iter()
            .map(|(name, values)| (name, Distribution::from_values(&values)))
            .collect();
    }
}
